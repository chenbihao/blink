//! 语音服务:hold-to-talk 管线编排。
//!
//! ## 管线
//!
//! ```text
//! Hold 事件 → start_recording()
//!   → 创建 AudioCapture + SttEngine
//!   → spawn 采集 task: audio chunk → SttEngine::transcribe_chunk → emit partial
//!
//! HoldRelease 事件 → stop_recording()
//!   → stop AudioCapture
//!   → SttEngine::finalize() → 最终文本
//!   → G1: emit EventNames::CHORD_FILL_QUERY(文本)
//!     G2: inject_text(文本)
//!     G3: emit EventNames::VOICE_PARTIAL(target="chat", 文本)
//! ```
//!
//! ## G1/G2/G3 区分
//!
//! - hold 时主窗口可见(先 tap 出窗)→ G1: 文字填 #query
//! - hold 时 chat 窗口可见 → G3: 文字填 chat composer textarea
//! - hold 时主窗口 + chat 均不可见 → G2: 文字注入前台应用

use std::sync::{Arc, Mutex};

use tauri::{Emitter, Manager};

use crate::domain::event_names::EventNames;
use crate::domain::stt::SttEngine;
use crate::infra::platform;
use crate::infra::platform::audio::{AudioCapture, AudioFormat};

/// 语音目标(G1 主窗口 / G2 前台应用 / G3 chat 窗口)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTarget {
    /// G1: 文字填进 blink 主窗口 #query
    MainWindow,
    /// G2: 文字注入前台应用光标处
    ForegroundApp,
    /// G3: 文字填进 chat 窗口 composer textarea（0.12.2 §4.3）
    ///
    /// 0.12.2: 仅 IPC 驱动（start_chat_stt）。
    /// 0.12.3: 热键驱动也走此路径——chat 窗口可见时 hold Alt+Space
    /// 自动检测并走 G3 而非 G2（不唤起前台注入）。
    ChatWindow,
}

impl VoiceTarget {
    /// 序列化给前端的字符串标签。
    pub fn as_str(&self) -> &'static str {
        match self {
            VoiceTarget::MainWindow => "g1",
            VoiceTarget::ForegroundApp => "g2",
            VoiceTarget::ChatWindow => "chat",
        }
    }
}

/// 语音会话状态。
struct VoiceSession {
    /// STT 引擎
    engine: Option<Arc<dyn SttEngine>>,
    /// 音频采集器
    capture: Option<Box<dyn AudioCapture>>,
    /// 音频采集 task 的 JoinHandle（stop/cancel 时 abort，避免与 finalize 锁竞争）
    audio_task: Option<tokio::task::JoinHandle<()>>,
    /// 目标(G1/G2/G3)
    target: VoiceTarget,
    /// 是否正在录音
    recording: bool,
    /// G2: 录音开始时的前台窗口 HWND（用于注入前恢复焦点）
    prev_fg_hwnd: Option<isize>,
}

impl Default for VoiceSession {
    fn default() -> Self {
        Self {
            engine: None,
            capture: None,
            audio_task: None,
            target: VoiceTarget::ForegroundApp,
            recording: false,
            prev_fg_hwnd: None,
        }
    }
}

/// 语音服务:管理 hold-to-talk 录音 + STT + 注入管线。
pub struct VoiceService {
    session: Mutex<VoiceSession>,
    app: tauri::AppHandle,
}

impl VoiceService {
    pub fn new(app: tauri::AppHandle) -> Self {
        Self {
            session: Mutex::new(VoiceSession::default()),
            app,
        }
    }

    /// Hold 事件:开始录音。
    ///
    /// 根据 main 窗口是否可见决定 G1/G2 目标。
    ///
    /// async 因为需要检查模型加载状态（HTTP /health 请求）。
    /// 返回 `true` = 录音已真正启动（调用方据此决定是否启动托盘动画等副作用）。
    pub async fn start_recording(&self) -> bool {
        // ── 总开关检查：STT 未启用时静默忽略 hold 事件 ──
        let config = crate::app::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("语音未启用,忽略 hold 事件");
            return false;
        }

        // ── 立即通知输入状态机进入 Recording ──
        // hold_fired 期间 reducer 已吞 Space/Alt keydown（防系统菜单），此处同步 voice phase
        // 使 ESC 能产生 VoiceCancel，并延续吞键到 keyup -> stop_recording 之间。必须在任何
        // .await 之前设置--否则 await 期间 ESC 无法取消。guard 确保所有早退路径
        // （服务未就绪 / 模型加载中等）回 Idle。
        struct VoiceRecordingGuard {
            armed: bool,
        }
        impl VoiceRecordingGuard {
            fn new() -> Self {
                crate::infra::platform::hotkey::InputController::update_voice_phase(
                    crate::infra::platform::hotkey::VoicePhase::Recording { gesture_id: 0 },
                );
                Self { armed: true }
            }
            fn disarm(&mut self) {
                self.armed = false;
            }
        }
        impl Drop for VoiceRecordingGuard {
            fn drop(&mut self) {
                if self.armed {
                    crate::infra::platform::hotkey::InputController::update_voice_phase(
                        crate::infra::platform::hotkey::VoicePhase::Idle,
                    );
                }
            }
        }
        let mut _voice_guard = VoiceRecordingGuard::new();

        // ── G1/G2 判定 + 互斥检查（scoped block：MutexGuard 不跨 await） ──
        let target;
        {
            let mut session = self.session.lock().unwrap();

            if session.recording {
                tracing::warn!("start_recording: 已在录音中,忽略");
                return false;
            }

            // 判断 G1/G2/G3：主窗口可见->G1，chat 窗口可见->G3，否则->G2
            let main_visible = self
                .app
                .get_webview_window("main")
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false);
            let chat_visible = self
                .app
                .get_webview_window("chat")
                .map(|w| w.is_visible().unwrap_or(false))
                .unwrap_or(false);
            target = if main_visible {
                VoiceTarget::MainWindow
            } else if chat_visible {
                VoiceTarget::ChatWindow
            } else {
                VoiceTarget::ForegroundApp
            };
            session.target = target;
        }

        // G2: 在服务就绪检查之前立即显示 overlay，让用户瞬间看到反馈。
        // overlay 初始显示默认文案（"语音输入中…"），服务检查完成后再更新内容
        // （错误消息或录音开始）。避免服务检查阻塞导致窗口延迟出现。
        if target == VoiceTarget::ForegroundApp {
            platform::window::show_voice_overlay(&self.app);
        }

        // ── 共享录音启动逻辑 ──
        if self.begin_recording(&config, true).await {
            // 录音真正开始，解除 guard（标志由 stop_recording/cancel_recording 清除）
            _voice_guard.disarm();
            true
        } else {
            false
        }
    }

    /// Chat 窗口 IPC 驱动:开始录音（0.12.2 §4.3）。
    ///
    /// 与 `start_recording` 的区别：
    /// - 不走热键状态机，由 `start_chat_stt` IPC command 直接调用
    /// - target 固定为 `ChatWindow`，不做 G1/G2 检测
    /// - 不设置 `VOICE_RECORDING` 标志（chat 窗口无需吞 Alt+Space）
    /// - 与 G1/G2 三方互斥（`session.recording` 标志保证同一时刻只有一个 target）
    pub async fn start_chat_recording(&self) {
        // ── 总开关检查 ──
        let config = crate::app::stt_config::get_stt_config();
        if !config.enabled {
            tracing::debug!("语音未启用,忽略 chat STT 请求");
            self.emit_voice_error(VoiceTarget::ChatWindow, "语音输入未启用，请在设置中开启");
            return;
        }

        // ── 互斥检查 + 设置 target ──
        {
            let mut session = self.session.lock().unwrap();
            if session.recording {
                tracing::warn!("start_chat_recording: 已在录音中,忽略");
                return;
            }
            session.target = VoiceTarget::ChatWindow;
        }

        // ── 共享录音启动逻辑（不设置 hotkey flag） ──
        if self.begin_recording(&config, false).await {
            // 0.17.2：chat 录音开始 → 托盘呼吸动画
            crate::app::tray::start_breathing(&self.app);
        }
    }

    /// 共享录音启动逻辑：服务就绪检查 + 模型加载检查 + 引擎创建 + 音频采集 + 采集 task。
    ///
    /// **调用方职责**：
    /// - 配置检查（STT enabled）
    /// - 设置 `session.target`（G1/G2 检测或 ChatWindow）
    /// - 检查 `session.recording`（互斥）
    /// - 管理 `VoiceRecordingGuard`（仅热键路径）
    ///
    /// `set_voice_flag`：是否通知输入状态机进入 Recording（仅热键路径需要）。
    /// 返回 `true` = 录音已启动。
    ///
    /// **0.22.6 批次 3**：本地模式下先从 `EngineManager` 获取连接快照
    /// （endpoint + token + engine_id + instance_id），再做 token-aware health 检查。
    /// 不再用配置中的 preferred port 做 port-only health——动态分配的 endpoint
    /// 可能与 preferred port 不一致，且 Python /health 强制要求 token 鉴权。
    async fn begin_recording(
        &self,
        config: &crate::app::stt_config::SttConfig,
        set_voice_flag: bool,
    ) -> bool {
        use crate::app::stt_config::SttMode;
        use crate::domain::stt::funasr::ModelLoadStatus;

        let target = self.session.lock().unwrap().target;

        // 0.22.6 批次 3: 本地模式下先获取连接，再做 token-aware health 检查
        //
        // 流程：get_connection → TCP 预检 → token-aware /health → create_engine
        //
        // 不再用 config.local_engine.server_port 做 port-only health：
        // 1. preferred port 只是启动偏好，不一定是实际监听端口
        // 2. Python /health 强制要求 X-Engine-Token，无 token 的请求返回 401
        let connection = if config.mode == SttMode::Local {
            let svc = self
                .app
                .try_state::<std::sync::Arc<crate::app::local_engine::EngineManager>>();
            match svc {
                Some(s) => {
                    let engine_id = crate::infra::local_engine::runtime::EngineId::new(
                        crate::app::local_engine::funasr::FUNASR_ENGINE_ID,
                    )
                    .unwrap_or_else(|_| {
                        crate::infra::local_engine::runtime::EngineId::new("funasr").unwrap()
                    });
                    match s.get_connection(&engine_id).await {
                        Ok(Some(conn)) => {
                            // 投影：LocalEngineConnection → SttEngineConnection
                            // endpoint 格式为 "http://127.0.0.1:port"，解析出 host 和 port
                            let parsed = parse_endpoint(&conn.endpoint);
                            match parsed {
                                Some((host, port)) => {
                                    Some(crate::domain::stt::SttEngineConnection {
                                        host,
                                        port,
                                        token: conn.token,
                                        engine_id: conn.engine_id,
                                        instance_id: conn.instance_id,
                                    })
                                }
                                None => {
                                    tracing::warn!(
                                        endpoint = %conn.endpoint,
                                        "LocalEngineConnection endpoint 解析失败,跳过连接"
                                    );
                                    None
                                }
                            }
                        }
                        Ok(None) => None, // 服务未运行
                        Err(e) => {
                            tracing::warn!(%e, "get_connection 查询失败");
                            None
                        }
                    }
                }
                None => None, // EngineManager 未注册
            }
        } else {
            None
        };

        // ── 服务就绪检查 ──
        let need_check = match config.mode {
            SttMode::Local => true,
            SttMode::Cloud => config.cloud_provider.is_none(),
        };
        if need_check {
            // 0.22.6: 本地模式下必须先获取连接，再检查服务状态
            // 无连接 = 服务未运行，直接中止
            if config.mode == SttMode::Local && connection.is_none() {
                let msg = "FunASR 服务未运行，请在设置页「语音输入」中启动服务";
                tracing::warn!(target = ?target, %msg, "语音录音中止：无连接");
                self.emit_voice_error(target, msg);
                return false;
            }

            let (ready, msg) = match config.mode {
                SttMode::Local => {
                    let conn = connection.as_ref().expect("connection 已在上方验证为 Some");
                    // TCP 预检：用连接中的 host:port（非配置 preferred port）
                    if !crate::domain::stt::funasr::is_server_ready_async(conn.port).await {
                        (
                            false,
                            "FunASR 服务未启动，请在设置页「语音输入」中启动服务".to_string(),
                        )
                    } else {
                        (true, String::new())
                    }
                }
                SttMode::Cloud => (false, "云端 STT 未配置供应商，请在设置页中配置".to_string()),
            };
            if !ready {
                tracing::warn!(target = ?target, %msg, "语音录音中止：服务未就绪");
                self.emit_voice_error(target, &msg);
                return false;
            }

            // ── 模型加载状态检查（本地模式）──
            // 0.22.6: 使用 token-aware health 检查，不再用 port-only check_model_loaded
            // TCP 端口可达不代表模型已就绪——uvicorn 先绑定端口，模型加载需 30-60s。
            // 必须携带 X-Engine-Token，否则 /health 返回 401
            if config.mode == SttMode::Local {
                let conn = connection.as_ref().expect("connection 已在上方验证为 Some");
                let status = crate::domain::stt::funasr::check_model_loaded_with_token(conn).await;
                match status {
                    ModelLoadStatus::Ready => {
                        // 模型就绪，继续录音流程
                    }
                    ModelLoadStatus::Loading | ModelLoadStatus::Idle => {
                        tracing::info!(target = ?target, "模型加载中,跳过本次录音");
                        self.emit_voice_status(target, "模型加载中，请稍候再试");
                        return false;
                    }
                    ModelLoadStatus::Error => {
                        let msg = "模型加载失败，请检查设置页日志";
                        tracing::warn!(target = ?target, %msg);
                        self.emit_voice_error(target, msg);
                        return false;
                    }
                    ModelLoadStatus::Unreachable => {
                        // 0.22.6: Unreachable 包括 401（token 不匹配）和连接失败
                        let msg = "服务连接失败或鉴权失败，请检查设置页中服务状态";
                        tracing::warn!(target = ?target, %msg);
                        self.emit_voice_error(target, msg);
                        return false;
                    }
                }
            }
        };

        // ── 重新获取 session 锁，创建引擎 + 启动采集 ──
        let mut session = self.session.lock().unwrap();

        // 二次检查：模型加载等待期间可能已被 cancel
        if session.recording {
            tracing::warn!("begin_recording: 模型加载期间已被其他路径占用");
            return false;
        }

        let engine = match crate::domain::stt::create_engine(connection) {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(target = ?session.target, %e, "语音录音中止：引擎创建失败");
                self.emit_voice_error(session.target, &e);
                return false;
            }
        };
        let mut capture = if let Some(dev_id) = &config.audio_device_id {
            platform::audio::create_capture_with_device(dev_id.clone())
        } else {
            platform::audio::create_capture()
        };
        engine.reset();

        let format = AudioFormat::default();
        match capture.start(format) {
            Ok(mut rx) => {
                session.recording = true;

                // 通知输入状态机进入 Recording（仅热键路径，使 ESC 能产生 VoiceCancel）
                if set_voice_flag {
                    crate::infra::platform::hotkey::InputController::update_voice_phase(
                        crate::infra::platform::hotkey::VoicePhase::Recording { gesture_id: 0 },
                    );
                }

                tracing::info!(
                    target = ?session.target,
                    "语音录音开始"
                );

                // G2: overlay 已在 start_recording 中提前显示，此处只需保存前台窗口 HWND
                if session.target == VoiceTarget::ForegroundApp {
                    // 保存前台窗口 HWND（注入前恢复焦点，提升 Ctrl+V 成功率）
                    session.prev_fg_hwnd = platform::window::get_foreground_hwnd();
                }

                // 通知前端录音已开始（G1 隐藏 Ghost overlay / G2 overlay 已显示 / G3 chat 麦克风按钮切换态）
                let target_str = session.target.as_str();
                let _ = self.app.emit(
                    EventNames::VOICE_RECORDING_START,
                    serde_json::json!({ "target": target_str }),
                );

                // spawn 采集 task: audio chunk → STT → emit partial
                let app = self.app.clone();
                let target = session.target;
                let engine: Arc<dyn SttEngine> = Arc::from(engine);
                let engine_for_task = engine.clone();

                let task_handle = tokio::spawn(async move {
                    while let Some(chunk) = rx.recv().await {
                        // 计算 RMS 音量（0.0 ~ 1.0）
                        let level = compute_rms(&chunk.samples);
                        let target_str = target.as_str();
                        let _ = app.emit(
                            EventNames::VOICE_LEVEL,
                            serde_json::json!({
                                "level": level,
                                "target": target_str,
                            }),
                        );

                        match engine_for_task.transcribe_chunk(&chunk.samples).await {
                            Ok(text) => {
                                if !text.is_empty() {
                                    // 尝试解析 JSON（伪流式引擎返回 confirmed + preview）
                                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
                                    {
                                        let confirmed = v
                                            .get("confirmed")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("");
                                        let preview =
                                            v.get("preview").and_then(|t| t.as_str()).unwrap_or("");
                                        // 只在有内容时 emit
                                        if !confirmed.is_empty() || !preview.is_empty() {
                                            let _ = app.emit(
                                                EventNames::VOICE_PARTIAL,
                                                serde_json::json!({
                                                    "confirmed": confirmed,
                                                    "preview": preview,
                                                    "target": target_str,
                                                }),
                                            );
                                        }
                                    } else {
                                        // 纯文本（真流式 / 非流式引擎的兼容路径）
                                        let _ = app.emit(
                                            EventNames::VOICE_PARTIAL,
                                            serde_json::json!({
                                                "text": text,
                                                "target": target_str,
                                            }),
                                        );
                                    }
                                }
                            }
                            Err(e) => {
                                tracing::warn!(%e, "STT transcribe_chunk 失败");
                            }
                        }
                    }
                    tracing::debug!("音频采集 channel 已关闭");
                });

                session.engine = Some(engine);
                session.capture = Some(capture);
                session.audio_task = Some(task_handle);

                true
            }
            Err(e) => {
                tracing::error!(%e, "音频采集启动失败");
                false
            }
        }
    }

    /// HoldRelease 事件:停止录音 → STT 最终识别 → 注入/填充。
    ///
    /// async 因为 `SttEngine::finalize` 是 async（HTTP 请求）。
    /// 调用方（HotkeyService）在 async task 中 .await 此方法。
    ///
    /// 0.12.2 §4.3：ChatWindow 路径由 `stop_chat_recording` 调用此方法，
    /// 最终文本通过 `voice-partial(target="chat")` emit 到 chat 窗口。
    pub async fn stop_recording(&self) {
        // 取出 engine + 停止采集 + abort 音频 task，然后立即释放锁
        let (engine, target) = {
            let mut session = self.session.lock().unwrap();

            if !session.recording {
                tracing::warn!("stop_recording: 未在录音中,忽略");
                // 服务未就绪 / 模型加载中等早退路径：overlay 已在 start_recording 中提前显示，
                // emit_voice_error/emit_voice_status 已更新了内容（不再 spawn 延迟 hide task）。
                // 松键即隐藏 overlay + 回 Idle + 清理前端状态。
                crate::infra::platform::hotkey::InputController::update_voice_phase(
                    crate::infra::platform::hotkey::VoicePhase::Idle,
                );
                platform::window::hide_voice_overlay(&self.app);
                let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                return;
            }

            // 停止采集
            if let Some(mut capture) = session.capture.take() {
                capture.stop();
            }

            // ★ abort 音频采集 task —— 释放 streaming engine 的 inner 锁
            // transcribe_chunk 可能在 ensure_connected 中阻塞（WebSocket 握手慢），
            // 持有 tokio::sync::Mutex。如果不 abort，finalize() 会永久阻塞在锁上。
            // abort 会取消 future，drop MutexGuard，释放锁。
            if let Some(handle) = session.audio_task.take() {
                handle.abort();
                tracing::debug!("音频采集 task 已 abort");
            }

            let target = session.target;
            let engine = session.engine.take();
            session.recording = false;

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (engine, target)
        }; // 锁在此释放，await 不持锁

        // 最终识别（async）
        // 加 10s 超时保护：即使 abort 后仍有异常情况（如 WS 半连接），不会永久卡住
        //
        // **G2 路径优化**：finalize + inject 整体放到 spawn 里脱离 effect 串行循环，
        // 避免识别期间阻塞后续 effect（Tap/HoldStarted）。松键后 overlay 立即隐藏，
        // 识别完成后在 spawn_blocking 中恢复焦点 + 注入文本。
        match target {
            VoiceTarget::MainWindow => {
                // G1: finalize 在 effect 循环内（G1 无 overlay，不阻塞 UI 反馈）
                let final_text = finalize_engine(engine).await;
                tracing::debug!(
                    target = ?target,
                    text_len = final_text.chars().count(),
                    "语音识别完成"
                );
                if final_text.is_empty() {
                    let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                } else {
                    let _ = self.app.emit(
                        EventNames::CHORD_FILL_QUERY,
                        serde_json::Value::String(final_text.clone()),
                    );
                    tracing::debug!("G1: 文字已 emit chord-fill-query");
                    let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                }
            }
            VoiceTarget::ForegroundApp => {
                // G2: finalize + restore_foreground + inject 脱离 effect 循环
                let prev_hwnd = {
                    let session = self.session.lock().unwrap();
                    session.prev_fg_hwnd
                };
                // 松键立即隐藏 overlay（识别 + 注入在后台进行）
                platform::window::hide_voice_overlay(&self.app);
                let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                tokio::spawn(async move {
                    let final_text = finalize_engine(engine).await;
                    tracing::debug!(
                        target = "ForegroundApp",
                        text_len = final_text.chars().count(),
                        "语音识别完成"
                    );
                    if final_text.is_empty() {
                        tracing::debug!("识别结果为空,跳过注入");
                        return;
                    }
                    // 注入在 spawn_blocking 中执行(SendInput 需要同线程)
                    tokio::task::spawn_blocking(move || {
                        // 注入前恢复前台窗口焦点（finalize 期间焦点可能漂移）
                        if let Some(hwnd) = prev_hwnd {
                            platform::window::restore_foreground(hwnd);
                            std::thread::sleep(std::time::Duration::from_millis(50));
                        }
                        if let Err(e) = platform::inject::inject_text(&final_text) {
                            tracing::error!(%e, "G2: 文本注入失败");
                        }
                    });
                });
            }
            VoiceTarget::ChatWindow => {
                // G3: finalize 在 effect 循环内（G3 无 overlay，chat 窗口自己管理 UI）
                let final_text = finalize_engine(engine).await;
                tracing::debug!(
                    target = ?target,
                    text_len = final_text.chars().count(),
                    "语音识别完成"
                );
                if final_text.is_empty() {
                    tracing::debug!("识别结果为空,跳过注入");
                    let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                } else {
                    let _ = self.app.emit(
                        EventNames::VOICE_PARTIAL,
                        serde_json::json!({
                            "text": final_text,
                            "target": "chat",
                        }),
                    );
                    let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
                }
            }
        }
    }

    /// Chat 窗口 IPC 驱动:停止录音（0.12.2 §4.3）。
    ///
    /// 与 `stop_recording` 共用同一逻辑——target 已在 `start_chat_recording` 时
    /// 设为 `ChatWindow`，`stop_recording` 按 target 分支到 G3 路径。
    pub async fn stop_chat_recording(&self) {
        self.stop_recording().await;
        // 0.17.2：chat 录音结束 → 停止托盘呼吸动画
        crate::app::tray::stop_breathing(&self.app);
    }

    pub fn cancel_recording(&self) {
        let mut session = self.session.lock().unwrap();

        if !session.recording {
            return;
        }

        if let Some(mut capture) = session.capture.take() {
            capture.stop();
        }

        // abort 音频采集 task（与 stop_recording 一致）
        if let Some(handle) = session.audio_task.take() {
            handle.abort();
        }

        session.recording = false;
        session.engine = None;

        // 通知输入状态机回 Idle
        crate::infra::platform::hotkey::InputController::update_voice_phase(
            crate::infra::platform::hotkey::VoicePhase::Idle,
        );

        tracing::info!("语音录音已取消");
        drop(session);

        // 隐藏 mini overlay(G2)
        platform::window::hide_voice_overlay(&self.app);

        // 通知前端录音已结束（G1 隐藏语音指示器 + 恢复 Ghost overlay / G3 chat 麦克风恢复）
        let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
    }

    /// 向用户反馈语音状态（如"模型加载中"），非错误性质。
    ///
    /// G1: emit 事件让前端显示在语音指示器区域。
    /// G2: 先显示 overlay 窗口再 emit（与 emit_voice_error 类似的时序处理）。
    /// G3: 直接 emit（chat 窗口已可见）。
    fn emit_voice_status(&self, target: VoiceTarget, message: &str) {
        if target == VoiceTarget::ForegroundApp {
            // G2: overlay 已在 start_recording 中提前显示，此处只 emit 状态消息更新内容。
            let _ = self.app.emit(
                EventNames::VOICE_STATUS,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        } else {
            // G1/G3: 直接 emit
            let _ = self.app.emit(
                EventNames::VOICE_STATUS,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        }
    }

    /// 向用户反馈语音错误（G1 直接 emit，G2 先显示 overlay 再延迟 emit，G3 直接 emit）。
    ///
    /// **绝不**用 Mock 引擎的假文本上屏——错误就是错误，告知用户而非静默吞掉。
    fn emit_voice_error(&self, target: VoiceTarget, message: &str) {
        if target == VoiceTarget::ForegroundApp {
            // G2: overlay 已在 start_recording 中提前显示，此处只 emit 错误消息更新内容。
            // 不再 show_voice_overlay（已显示），也不 spawn 延迟 hide--
            // overlay 生命周期由 stop_recording/cancel_recording 统一管理（松键即隐藏）。
            let _ = self.app.emit(
                EventNames::VOICE_ERROR,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        } else {
            // G1/G3: 直接 emit（窗口已可见，事件就绪）
            let _ = self.app.emit(
                EventNames::VOICE_ERROR,
                serde_json::json!({
                    "message": message,
                    "target": target.as_str(),
                }),
            );
        }
    }

    /// 是否正在录音。
    pub fn is_recording(&self) -> bool {
        self.session.lock().unwrap().recording
    }
}

/// STT finalize + 10s 超时保护（G1/G2/G3 三路共用）。
///
/// engine 为 None 时返回空字符串；finalize 成功返回识别文本；
/// 失败或超时返回空字符串并打 warn 日志。
async fn finalize_engine(engine: Option<Arc<dyn SttEngine>>) -> String {
    match engine {
        Some(e) => {
            match tokio::time::timeout(std::time::Duration::from_secs(10), e.finalize()).await {
                Ok(Ok(text)) => text,
                Ok(Err(e)) => {
                    tracing::warn!(%e, "STT finalize 失败");
                    String::new()
                }
                Err(_) => {
                    tracing::warn!("STT finalize 超时（10s），放弃等待");
                    String::new()
                }
            }
        }
        None => String::new(),
    }
}

/// 从 endpoint URL 解析出 host 和 port。
///
/// endpoint 格式预期为 `http://127.0.0.1:port` 或 `http://host:port`。
/// 解析失败返回 None（由调用方决定降级行为）。
///
/// 0.22.6 批次 3 H4: 替代此前 domain 层的 `rsplit(':')` 字符串猜测——
/// app 层负责结构化解析，domain 层只接收纯数据。
fn parse_endpoint(endpoint: &str) -> Option<(String, u16)> {
    parse_endpoint_pub(endpoint)
}

/// `parse_endpoint` 的公共入口，供 maintenance 等诊断命令复用。
///
/// 0.22.6: diagnosis 命令需要从 `LocalEngineConnection.endpoint` 解析
/// host/port 以做 token-aware health 检查。
pub fn parse_endpoint_pub(endpoint: &str) -> Option<(String, u16)> {
    // 去掉 scheme 前缀
    let host_port = endpoint
        .strip_prefix("http://")
        .or_else(|| endpoint.strip_prefix("https://"))
        .unwrap_or(endpoint);

    // 去掉末尾可能的 path
    let host_port = host_port.split('/').next().unwrap_or(host_port);

    // host:port 分割
    let colon = host_port.rfind(':')?;
    let host = &host_port[..colon];
    let port_str = &host_port[colon + 1..];
    let port: u16 = port_str.parse().ok()?;
    Some((host.to_string(), port))
}

/// 计算 PCM 样本的音量级别（0.0 ~ 1.0），用于前端波形条可视化。
///
/// 使用 RMS（均方根）+ 噪声门限 + 平方根曲线：
/// - RMS < 0.001（噪声门限）→ 0.0（静默）
/// - RMS 0.001~0.15 映射到 0.0~1.0，用 sqrt 曲线增强小信号区域
/// - 平方根曲线让安静说话也能有 20-30% 的音量指示，不会"看起来没反应"
fn compute_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt();

    // 噪声门限：低于此值视为静默
    const NOISE_FLOOR: f64 = 0.001;
    if rms < NOISE_FLOOR {
        return 0.0;
    }

    // 线性映射到 0~1（参考电平 0.15 = 正常说话音量）
    let normalized = ((rms - NOISE_FLOOR) / (0.15 - NOISE_FLOOR)).min(1.0);

    // 平方根曲线：增强小信号区域，让安静说话也有明显指示
    normalized.sqrt()
}

#[cfg(test)]
mod tests {
    use super::{compute_rms, parse_endpoint};

    #[test]
    fn rms_empty_returns_zero() {
        assert_eq!(compute_rms(&[]), 0.0);
    }

    #[test]
    fn rms_silence_returns_zero() {
        // 全零样本 → RMS = 0 < NOISE_FLOOR → 0.0
        assert_eq!(compute_rms(&[0.0; 1000]), 0.0);
    }

    #[test]
    fn rms_noise_floor_returns_zero() {
        // 极小值（低于 NOISE_FLOOR = 0.001）→ 静默
        let samples = vec![0.0001f32; 100];
        assert_eq!(compute_rms(&samples), 0.0);
    }

    #[test]
    fn rms_normal_speech_nonzero() {
        // 模拟正常说话音量（幅度 ~0.3）
        let samples: Vec<f32> = (0..1600)
            .map(|i| {
                let t = i as f32 / 16000.0;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.3
            })
            .collect();
        let level = compute_rms(&samples);
        // RMS ≈ 0.3 / √2 ≈ 0.212，远超 0.15 上限 → normalized = 1.0 → level = 1.0
        assert!(level > 0.0, "正常音量不应返回 0");
        assert!(level <= 1.0, "音量不应超过 1.0");
    }

    #[test]
    fn rms_clamped_to_one() {
        // 最大幅度 → 不超过 1.0
        let samples = vec![1.0f32; 100];
        let level = compute_rms(&samples);
        assert!(level <= 1.0, "音量上限为 1.0，got {level}");
    }

    #[test]
    fn rms_sqrt_curve_enhances_small_signals() {
        // 小信号（RMS 略高于 NOISE_FLOOR）应因 sqrt 曲线得到增强
        // RMS ≈ 0.01 → normalized = (0.01 - 0.001) / (0.15 - 0.001) ≈ 0.0604
        // sqrt(0.0604) ≈ 0.246
        let samples = vec![0.01f32; 100];
        let level = compute_rms(&samples);
        assert!(level > 0.0, "小信号应非零");
        // 线性值 ≈ 0.06，sqrt 后 ≈ 0.25，应明显大于线性值
        assert!(level > 0.06, "sqrt 曲线应增强小信号: got {level}");
    }

    // ── parse_endpoint 单测 ──

    #[test]
    fn parse_endpoint_localhost_with_port() {
        let (host, port) = parse_endpoint("http://127.0.0.1:8100").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8100);
    }

    #[test]
    fn parse_endpoint_https_scheme() {
        let (host, port) = parse_endpoint("https://127.0.0.1:443").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 443);
    }

    #[test]
    fn parse_endpoint_with_trailing_path() {
        let (host, port) = parse_endpoint("http://127.0.0.1:8100/health").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8100);
    }

    #[test]
    fn parse_endpoint_no_scheme() {
        // 无 scheme 前缀也能解析
        let (host, port) = parse_endpoint("127.0.0.1:8100").unwrap();
        assert_eq!(host, "127.0.0.1");
        assert_eq!(port, 8100);
    }

    #[test]
    fn parse_endpoint_no_port_returns_none() {
        assert!(parse_endpoint("http://127.0.0.1").is_none());
    }

    #[test]
    fn parse_endpoint_invalid_port_returns_none() {
        assert!(parse_endpoint("http://127.0.0.1:abc").is_none());
    }
}
