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
use crate::domain::stt::{StreamingSttPort, SttEngine, SttEvent};
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
    /// STT 引擎（旧接口，用于 GGUF 伪流式/非流式兼容）
    engine: Option<Arc<dyn SttEngine>>,
    /// 结构化 STT port（0.22.9：统一事件流）
    ///
    /// 存在时优先使用，替代旧的 transcribe_chunk/finalize 管线。
    /// 不存在时回退到旧管线（GGUF 伪流式适配器创建失败时）。
    stt_port: Option<Arc<dyn StreamingSttPort>>,
    /// 当前 session 的 generation（用于事件过滤）
    generation: Option<u64>,
    /// 事件消费 task 的 JoinHandle
    event_task: Option<tokio::task::JoinHandle<()>>,
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
            stt_port: None,
            generation: None,
            event_task: None,
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

        let target = self.session.lock().unwrap().target;

        // 0.22.7 GGUF worker：本地模式下从 EngineManager 获取连接快照，
        // 就绪检查走 worker transport 的 NDJSON hello（无 HTTP 端口）。
        // 0.22.9 Handoff 08：连接携带 start 冻结的 implementation——
        // ParaformerOnline 返回真实 streaming port（真流式），GGUF 返回
        // 现有 transport（伪流式适配）；VoiceService 按实现选择 port，
        // 不接受任何前端提交的 implementation/runtime。
        let mut streaming_port: Option<
            std::sync::Arc<
                crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter,
            >,
        > = None;
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
                            // 按冻结 implementation 分派 port（Handoff 08）
                            if conn.implementation
                                == Some(crate::domain::local_engine::ImplementationId::ParaformerOnnxWorker)
                            {
                                match conn.streaming {
                                    Some(port) if port.is_ready() => {
                                        streaming_port = Some(port);
                                        None
                                    }
                                    Some(_) => {
                                        tracing::warn!(
                                            engine = %conn.engine_id,
                                            "ONNX streaming 通道已断开（worker 可能已退出）"
                                        );
                                        None
                                    }
                                    None => {
                                        tracing::warn!(
                                            engine = %conn.engine_id,
                                            "运行中的 ONNX 实例缺少 streaming 通道"
                                        );
                                        None
                                    }
                                }
                            } else {
                                // GGUF：投影 LocalEngineConnection → SttEngineConnection
                                // 0.22.7.4：worker 传输是唯一本地实现，endpoint
                                // 不承载地址语义（host/port 为诊断占位）。
                                match conn.worker {
                                    Some(transport) => {
                                        Some(crate::domain::stt::SttEngineConnection {
                                            host: "127.0.0.1".to_string(),
                                            port: 0,
                                            engine_id: conn.engine_id,
                                            instance_id: conn.instance_id,
                                            transport: Some(transport),
                                        })
                                    }
                                    None => {
                                        tracing::warn!(
                                            engine = %conn.engine_id,
                                            "运行中的本地引擎连接缺少 worker 通道（应为 StdioWorker）"
                                        );
                                        None
                                    }
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
            // 无连接/port = 服务未运行，直接中止
            if config.mode == SttMode::Local && connection.is_none() && streaming_port.is_none() {
                let msg = "FunASR 服务未运行，请在设置页「语音输入」中启动服务";
                tracing::warn!(target = ?target, %msg, "语音录音中止：无连接");
                self.emit_voice_error(target, msg);
                return false;
            }

            let (ready, msg) = match config.mode {
                SttMode::Local => {
                    if streaming_port.is_some() {
                        // Handoff 08：ONNX ready 由 start 时的 hello/ready 握手
                        // 保证（ORT + 模型加载完成后才 Ready）；断连已在上方
                        // is_ready 检查中识别。
                        (true, String::new())
                    } else {
                        let conn = connection.as_ref().expect("connection 已在上方验证为 Some");
                        // 0.22.7 GGUF worker：就绪检查走 NDJSON 通道 hello——
                        // 通道与实例绑定，模型就绪由 start 的 ready 握手保证。
                        match conn.transport.as_ref() {
                            Some(transport) => match transport.check_ready().await {
                                Ok(()) => (true, String::new()),
                                Err(e) => {
                                    (false, format!("语音服务不可用：{e}。请在设置页重启服务。"))
                                }
                            },
                            None => (
                                false,
                                "语音服务连接缺少 worker 通道，请在设置页重启服务".to_string(),
                            ),
                        }
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
            // 0.22.7 GGUF worker：就绪已由上方通道 hello 验证（模型加载完成
            // 才有 ready），无需二次模型状态轮询。
            // 0.22.9 ONNX：ready 已由 start 握手验证（同语义）。
        };

        // ── 重新获取 session 锁，创建引擎 + 启动采集 ──
        // 注意：std::sync::MutexGuard 不是 Send，所有锁操作必须在不含 await 的 block 内完成。
        let (stt_port, engine_arc, mut rx, target, _target_str, prev_fg_hwnd) = {
            let mut session = self.session.lock().unwrap();

            // 二次检查：模型加载等待期间可能已被 cancel
            if session.recording {
                tracing::warn!("begin_recording: 模型加载期间已被其他路径占用");
                return false;
            }

            // Handoff 08：ONNX 真流式 port 直接使用（native partial）；
            // GGUF 走现有引擎创建 + 伪流式适配器包装。
            let (engine_arc, stt_port): (Option<Arc<dyn SttEngine>>, Arc<dyn StreamingSttPort>) =
                if let Some(port) = streaming_port.take() {
                    tracing::info!(
                        implementation = "paraformer_onnx_worker",
                        "STT port: ParaformerOnline 真流式（按冻结 implementation 选择）"
                    );
                    (None, port)
                } else {
                    let engine = match crate::domain::stt::create_engine(connection.clone()) {
                        Ok(e) => e,
                        Err(e) => {
                            tracing::warn!(target = ?session.target, %e, "语音录音中止：引擎创建失败");
                            let msg = e;
                            self.emit_voice_error(session.target, &msg);
                            return false;
                        }
                    };
                    engine.reset();
                    let engine_arc: Arc<dyn SttEngine> = Arc::from(engine);
                    let port: Arc<dyn StreamingSttPort> = Arc::new(
                        crate::domain::stt::streaming_port::GgufStreamingAdapter::new(
                            engine_arc.clone(),
                        ),
                    );
                    (Some(engine_arc), port)
                };

            let mut capture = if let Some(dev_id) = &config.audio_device_id {
                platform::audio::create_capture_with_device(dev_id.clone())
            } else {
                platform::audio::create_capture()
            };

            let format = AudioFormat::default();
            match capture.start(format) {
                Ok(rx) => {
                    // capture 必须存入 session——它是块内局部变量，块结束时
                    // drop 会触发 CpalCapture::drop 停掉采集线程，音频通道
                    // 随即关闭，识别全程收不到任何样本（0.22.9 回归）。
                    session.capture = Some(capture);
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
                        session.prev_fg_hwnd = platform::window::get_foreground_hwnd();
                    }

                    let target = session.target;
                    let target_str = session.target.as_str();
                    let prev_fg_hwnd = session.prev_fg_hwnd;

                    // 通知前端录音已开始
                    let _ = self.app.emit(
                        EventNames::VOICE_RECORDING_START,
                        serde_json::json!({ "target": target_str }),
                    );

                    (stt_port, engine_arc, rx, target, target_str, prev_fg_hwnd)
                }
                Err(e) => {
                    tracing::error!(%e, "音频采集启动失败");
                    return false;
                }
            }
        }; // MutexGuard 在此释放，后续 await 安全

        // 0.22.9：begin session 获取 generation（在锁外 await）
        let port = stt_port.clone();
        let session_gen = match port.begin_session().await {
            Ok(g) => g,
            Err(e) => {
                tracing::error!(%e, "begin_session 失败");
                self.emit_voice_error(target, &e.to_string());
                {
                    let mut session = self.session.lock().unwrap();
                    session.recording = false;
                    // 立即 drop capture（停采集 + 关通道），不留残留实例
                    session.capture.take();
                }
                return false;
            }
        };

        // 获取事件 receiver（在 begin_session 之后）
        let event_rx = port.events();

        // spawn 事件消费 task：按 generation 过滤，emit 到前端
        let app_for_events = self.app.clone();
        let target_for_events = target;
        let prev_hwnd_for_events = prev_fg_hwnd;
        let event_task = tokio::spawn(async move {
            consume_stt_events(
                event_rx,
                session_gen,
                target_for_events,
                prev_hwnd_for_events,
                app_for_events,
            )
            .await;
        });

        // spawn 采集 task: audio chunk → push_audio（非阻塞）
        let app = self.app.clone();
        let port_for_audio = stt_port.clone();
        let target_for_audio = target;

        let task_handle = tokio::spawn(async move {
            while let Some(chunk) = rx.recv().await {
                // 计算 RMS 音量（0.0 ~ 1.0）
                let level = compute_rms(&chunk.samples);
                let target_str = target_for_audio.as_str();
                let _ = app.emit(
                    EventNames::VOICE_LEVEL,
                    serde_json::json!({
                        "level": level,
                        "target": target_str,
                    }),
                );

                // 0.22.9：通过统一 port 推送音频
                // push_audio 不阻塞——内部通过 channel 转发
                if let Err(e) = port_for_audio.push_audio(session_gen, &chunk.samples).await {
                    tracing::warn!(%e, "push_audio 失败");
                    break;
                }
            }
            tracing::debug!("音频采集 channel 已关闭");
        });

        // 重新获取锁写入剩余字段
        {
            let mut session = self.session.lock().unwrap();
            session.generation = Some(session_gen);
            // ONNX 真流式路径 engine 为 None（finalize 回退路径不适用，
            // port 的 finish/cancel 语义完整覆盖）
            session.engine = engine_arc;
            session.stt_port = Some(stt_port);
            session.audio_task = Some(task_handle);
            session.event_task = Some(event_task);
        }

        true
    }

    /// HoldRelease 事件:停止录音 → STT 最终识别 → 注入/填充。
    ///
    /// async 因为 `SttEngine::finalize` 是 async（HTTP 请求）。
    /// 调用方（HotkeyService）在 async task 中 .await 此方法。
    ///
    /// 0.12.2 §4.3：ChatWindow 路径由 `stop_chat_recording` 调用此方法，
    /// 最终文本通过 `voice-partial(target="chat")` emit 到 chat 窗口。
    pub async fn stop_recording(&self) {
        // 取出 engine + port + generation + 停止采集 + abort 音频 task，然后立即释放锁
        let (engine, stt_port, generation, target) = {
            let mut session = self.session.lock().unwrap();

            if !session.recording {
                tracing::warn!("stop_recording: 未在录音中,忽略");
                // 服务未就绪等早退路径：overlay 已在 start_recording 中提前显示，
                // emit_voice_error 已更新了内容（不再 spawn 延迟 hide task）。
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
            let stt_port = session.stt_port.take();
            let generation = session.generation.take();
            session.recording = false;

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (engine, stt_port, generation, target)
        }; // 锁在此释放，await 不持锁

        // 0.22.9：通过 StreamingSttPort::finish_session 通知引擎音频流结束
        // Final 结果将通过事件消费 task 异步产出

        // G2: 松键立即隐藏 overlay（识别 + 注入在后台进行）
        if target == VoiceTarget::ForegroundApp {
            platform::window::hide_voice_overlay(&self.app);
            let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
        }

        if let (Some(port), Some(session_gen)) = (&stt_port, generation) {
            if let Err(e) = port.finish_session(session_gen).await {
                tracing::warn!(%e, "finish_session 失败，回退旧 finalize 路径");
                // 回退：直接调 finalize_engine
                let final_text = finalize_engine(engine).await;
                self.deliver_final_text(target, final_text).await;
            } else {
                // finish_session 已产出 Final 事件，事件消费 task 会处理
                // 但需要给事件消费 task 时间处理 Final——等待它完成
                // 0.22.9：等待 event_task 完成或超时
                let event_task = self.session.lock().unwrap().event_task.take();
                if let Some(handle) = event_task {
                    let _ = tokio::time::timeout(std::time::Duration::from_secs(12), handle).await;
                }
                // 事件消费 task 已完成（或超时），清理
            }
            return;
        }

        // 回退：旧 finalize 路径（stt_port 不存在时）
        let final_text = finalize_engine(engine).await;
        self.deliver_final_text(target, final_text).await;
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
        // 取出 stt_port + generation + 停止采集 + abort tasks，然后释放锁
        let (stt_port, generation, _target) = {
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

            // abort 事件消费 task
            if let Some(handle) = session.event_task.take() {
                handle.abort();
            }

            let target = session.target;
            let stt_port = session.stt_port.take();
            let generation = session.generation.take();
            session.recording = false;
            session.engine = None;

            // 通知输入状态机回 Idle
            crate::infra::platform::hotkey::InputController::update_voice_phase(
                crate::infra::platform::hotkey::VoicePhase::Idle,
            );

            (stt_port, generation, target)
        }; // 锁在此释放

        // 0.22.9：通过 StreamingSttPort::cancel_session 通知引擎丢弃在途结果。
        // cancel_session 是 async，但 cancel 不应阻塞 P0 主链路（ESC 后用户已离开），
        // 用 spawn 脱离调用方 effect 循环。cancel 幂等——即使 spawn 未执行也不影响正确性。
        if let (Some(port), Some(session_gen)) = (stt_port, generation) {
            tokio::spawn(async move {
                if let Err(e) = port.cancel_session(session_gen).await {
                    tracing::warn!(%e, "cancel_session 失败（可忽略——cancel 幂等）");
                }
            });
        }

        tracing::info!("语音录音已取消");

        // 隐藏 mini overlay(G2)
        platform::window::hide_voice_overlay(&self.app);

        // 通知前端录音已结束（G1 隐藏语音指示器 + 恢复 Ghost overlay / G3 chat 麦克风恢复）
        let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
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

    /// 交付最终识别文本到目标（G1/G2/G3）。
    ///
    /// 0.22.9：从旧 `stop_recording` 的内联交付逻辑提取为独立方法，
    /// 供 `stop_recording` 回退路径和 `consume_stt_events` 共用。
    ///
    /// - G1: emit `CHORD_FILL_QUERY` + `VOICE_RECORDING_END`
    /// - G2: spawn 后台 inject_text（脱离 effect 循环，恢复焦点 + 注入）
    /// - G3: emit `VOICE_PARTIAL(target="chat")` + `VOICE_RECORDING_END`
    async fn deliver_final_text(&self, target: VoiceTarget, final_text: String) {
        tracing::debug!(
            target = ?target,
            text_len = final_text.chars().count(),
            "语音识别完成"
        );

        if final_text.is_empty() {
            tracing::debug!("识别结果为空,跳过交付");
            let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
            return;
        }

        match target {
            VoiceTarget::MainWindow => {
                // G1: 文字填 #query
                let _ = self.app.emit(
                    EventNames::CHORD_FILL_QUERY,
                    serde_json::Value::String(final_text.clone()),
                );
                tracing::debug!("G1: 文字已 emit chord-fill-query");
                let _ = self.app.emit(EventNames::VOICE_RECORDING_END, ());
            }
            VoiceTarget::ForegroundApp => {
                // G2: 文字注入前台应用光标处
                let prev_hwnd = self.session.lock().unwrap().prev_fg_hwnd;
                tokio::spawn(async move {
                    // 注入在 spawn_blocking 中执行（SendInput 需要同线程）
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
                // G2 overlay 已在 stop_recording 中隐藏
            }
            VoiceTarget::ChatWindow => {
                // G3: 文字填 chat composer textarea
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

/// STT 事件消费 task：循环接收 `SttEvent`，按 generation 过滤旧事件，
/// 将有效事件 emit 到前端或调用 `deliver_final_text` 交付最终文本。
///
/// 0.22.9 Handoff 05：此 task 在 `begin_recording` 时 spawn，
/// 在 `stop_recording`（等待完成或超时）或 `cancel_recording`（abort）时终止。
///
/// **事件处理**：
/// - `Partial` → emit `VOICE_PARTIAL`（confirmed + preview）
/// - `Final` → 调用 `deliver_final_text` 交付最终文本
/// - `Busy` → 打 debug 日志（可选降频，当前不处理）
/// - `Error` → emit `VOICE_ERROR`
///
/// **generation 过滤**：generation 不匹配的事件直接丢弃，
/// 防止 cancel/reset 后迟到的旧结果污染 UI。
async fn consume_stt_events(
    mut rx: tokio::sync::mpsc::UnboundedReceiver<SttEvent>,
    expected_gen: u64,
    target: VoiceTarget,
    prev_fg_hwnd: Option<isize>,
    app: tauri::AppHandle,
) {
    // 将 AppHandle 包装为 VoiceService-like 的 deliver 闭包——
    // consume_stt_events 不能直接调 VoiceService::deliver_final_text（无 &self），
    // 但 deliver 逻辑只依赖 app.emit + inject，可在此内联。
    let target_str = target.as_str();

    while let Some(event) = rx.recv().await {
        match event {
            SttEvent::Partial {
                generation,
                confirmed,
                preview,
            } => {
                if generation != expected_gen {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        "丢弃旧 generation 的 Partial 事件"
                    );
                    continue;
                }
                // emit partial 到前端
                let _ = app.emit(
                    EventNames::VOICE_PARTIAL,
                    serde_json::json!({
                        "confirmed": confirmed,
                        "preview": preview,
                        "target": target_str,
                    }),
                );
            }
            SttEvent::Final { generation, text } => {
                if generation != expected_gen {
                    tracing::debug!(
                        gen = generation,
                        expected = expected_gen,
                        "丢弃旧 generation 的 Final 事件"
                    );
                    continue;
                }
                tracing::debug!(
                    target = ?target,
                    text_len = text.chars().count(),
                    "收到 Final 事件"
                );

                // 交付最终文本——内联 deliver 逻辑（无法访问 &self）
                if text.is_empty() {
                    tracing::debug!("识别结果为空,跳过交付");
                    let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
                } else {
                    match target {
                        VoiceTarget::MainWindow => {
                            let _ = app.emit(
                                EventNames::CHORD_FILL_QUERY,
                                serde_json::Value::String(text.clone()),
                            );
                            tracing::debug!("G1: 文字已 emit chord-fill-query");
                            let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
                        }
                        VoiceTarget::ForegroundApp => {
                            // G2: 注入前台应用光标处
                            tokio::spawn(async move {
                                tokio::task::spawn_blocking(move || {
                                    // 注入前恢复前台窗口焦点（finalize 期间焦点可能漂移）
                                    if let Some(hwnd) = prev_fg_hwnd {
                                        platform::window::restore_foreground(hwnd);
                                        std::thread::sleep(std::time::Duration::from_millis(50));
                                    }
                                    if let Err(e) = platform::inject::inject_text(&text) {
                                        tracing::error!(%e, "G2: 文本注入失败（事件路径）");
                                    }
                                });
                            });
                        }
                        VoiceTarget::ChatWindow => {
                            let _ = app.emit(
                                EventNames::VOICE_PARTIAL,
                                serde_json::json!({
                                    "text": text,
                                    "target": "chat",
                                }),
                            );
                            let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
                        }
                    }
                }
                // Final 是 session 的最后一个事件，退出循环
                break;
            }
            SttEvent::Busy { generation, reason } => {
                if generation != expected_gen {
                    continue;
                }
                tracing::debug!(%reason, "STT 引擎忙（背压）");
            }
            SttEvent::Error {
                generation,
                message,
            } => {
                if generation != expected_gen {
                    continue;
                }
                tracing::error!(%message, "STT 引擎错误事件");
                let _ = app.emit(
                    EventNames::VOICE_ERROR,
                    serde_json::json!({
                        "message": message,
                        "target": target_str,
                    }),
                );
                let _ = app.emit(EventNames::VOICE_RECORDING_END, ());
                // Error 是终止事件，退出循环
                break;
            }
        }
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
    use super::compute_rms;

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
}
