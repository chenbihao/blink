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
//!   → G1: emit "blink://chord-fill-query"(文本)
//!     G2: inject_text(文本)
//! ```
//!
//! ## G1/G2 区分
//!
//! - hold 时主窗口可见(先 tap 出窗)→ G1: 文字填 #query
//! - hold 时主窗口不可见 → G2: 文字注入前台应用

use std::sync::{Arc, Mutex};

use tauri::{Emitter, Manager};

use crate::domain::stt::SttEngine;
use crate::infra::platform;
use crate::infra::platform::audio::{AudioCapture, AudioFormat};

/// 语音目标(G1 主窗口 / G2 前台应用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VoiceTarget {
    /// G1: 文字填进 blink 主窗口 #query
    MainWindow,
    /// G2: 文字注入前台应用光标处
    ForegroundApp,
}

/// 语音会话状态。
struct VoiceSession {
    /// STT 引擎
    engine: Option<Arc<dyn SttEngine>>,
    /// 音频采集器
    capture: Option<Box<dyn AudioCapture>>,
    /// 音频采集 task 的 JoinHandle（stop/cancel 时 abort，避免与 finalize 锁竞争）
    audio_task: Option<tokio::task::JoinHandle<()>>,
    /// 目标(G1/G2)
    target: VoiceTarget,
    /// 是否正在录音
    recording: bool,
    /// 最新 partial 文本
    last_partial: String,
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
            last_partial: String::new(),
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
    pub fn start_recording(&self) {
        let mut session = self.session.lock().unwrap();

        if session.recording {
            tracing::warn!("start_recording: 已在录音中,忽略");
            return;
        }

        // 判断 G1/G2:主窗口是否可见
        let main_visible = self
            .app
            .get_webview_window("main")
            .map(|w| w.is_visible().unwrap_or(false))
            .unwrap_or(false);
        session.target = if main_visible {
            VoiceTarget::MainWindow
        } else {
            VoiceTarget::ForegroundApp
        };

        // 创建 STT engine + audio capture
        let config = crate::app::stt_config::get_stt_config();

        // ── 服务就绪检查：本地模式下检查 FunASR 服务是否运行 ──
        let need_check = match config.mode {
            crate::app::stt_config::SttMode::Local => true,
            crate::app::stt_config::SttMode::Cloud => config.cloud_provider.is_none(),
        };
        if need_check {
            let (ready, msg) = match config.mode {
                crate::app::stt_config::SttMode::Local => {
                    let port = config.local_engine.server_port;
                    if crate::domain::stt::funasr::is_server_ready(port) {
                        (true, String::new())
                    } else {
                        (
                            false,
                            "FunASR 服务未启动，请在设置页「语音输入」中启动服务".to_string(),
                        )
                    }
                }
                crate::app::stt_config::SttMode::Cloud => {
                    (false, "云端 STT 未配置供应商，请在设置页中配置".to_string())
                }
            };
            if !ready {
                tracing::warn!(target = ?session.target, %msg, "语音录音中止：服务未就绪");
                let target_str = if session.target == VoiceTarget::MainWindow {
                    "g1"
                } else {
                    "g2"
                };

                // G2: 先显示 overlay，再延迟 emit 错误消息
                // （窗口刚 show 时事件可能未就绪，延迟 100ms 确保接收）
                if session.target == VoiceTarget::ForegroundApp {
                    platform::window::show_voice_overlay(&self.app);
                    let app_clone = self.app.clone();
                    let msg_clone = msg.clone();
                    tokio::spawn(async move {
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                        let _ = app_clone.emit(
                            "blink://voice-error",
                            serde_json::json!({
                                "message": msg_clone,
                                "target": "g2",
                            }),
                        );
                        // 2s 后自动隐藏
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                        platform::window::hide_voice_overlay(&app_clone);
                    });
                } else {
                    // G1: 直接 emit（主窗口已可见，事件就绪）
                    let _ = self.app.emit(
                        "blink://voice-error",
                        serde_json::json!({
                            "message": msg,
                            "target": target_str,
                        }),
                    );
                }
                // 不启动录音，直接返回
                return;
            }
        }

        let engine = crate::domain::stt::create_engine();
        let mut capture = if let Some(dev_id) = config.audio_device_id {
            platform::audio::create_capture_with_device(dev_id)
        } else {
            platform::audio::create_capture()
        };
        engine.reset();

        let format = AudioFormat::default();
        match capture.start(format) {
            Ok(mut rx) => {
                session.recording = true;
                session.last_partial.clear();

                // 设置全局录音标志（hotkey hook 读它判断 ESC + 吞 Alt+Space）
                crate::infra::platform::hotkey::set_voice_recording(true);

                tracing::info!(
                    target = ?session.target,
                    "语音录音开始"
                );

                // G2: 显示 mini overlay 窗口
                if session.target == VoiceTarget::ForegroundApp {
                    // 保存前台窗口 HWND（注入前恢复焦点，提升 Ctrl+V 成功率）
                    session.prev_fg_hwnd = platform::window::get_foreground_hwnd();
                    platform::window::show_voice_overlay(&self.app);
                }

                // spawn 采集 task: audio chunk → STT → emit partial
                let app = self.app.clone();
                let target = session.target;
                let engine: Arc<dyn SttEngine> = Arc::from(engine);
                let engine_for_task = engine.clone();

                let task_handle = tokio::spawn(async move {
                    while let Some(chunk) = rx.recv().await {
                        // 计算 RMS 音量（0.0 ~ 1.0）
                        let level = compute_rms(&chunk.samples);
                        let target_str = if target == VoiceTarget::MainWindow {
                            "g1"
                        } else {
                            "g2"
                        };
                        let _ = app.emit(
                            "blink://voice-level",
                            serde_json::json!({
                                "level": level,
                                "target": target_str,
                            }),
                        );

                        match engine_for_task.transcribe_chunk(&chunk.samples).await {
                            Ok(text) => {
                                if !text.is_empty() {
                                    // 尝试解析 JSON（伪流式引擎返回 confirmed + preview）
                                    if let Ok(v) =
                                        serde_json::from_str::<serde_json::Value>(&text)
                                    {
                                        let confirmed = v
                                            .get("confirmed")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("");
                                        let preview = v
                                            .get("preview")
                                            .and_then(|t| t.as_str())
                                            .unwrap_or("");
                                        // 只在有内容时 emit
                                        if !confirmed.is_empty() || !preview.is_empty() {
                                            let _ = app.emit(
                                                "blink://voice-partial",
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
                                            "blink://voice-partial",
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
            }
            Err(e) => {
                tracing::error!(%e, "音频采集启动失败");
            }
        }
    }

    /// HoldRelease 事件:停止录音 → STT 最终识别 → 注入/填充。
    ///
    /// async 因为 `SttEngine::finalize` 是 async（HTTP 请求）。
    /// 调用方（HotkeyService）在 async task 中 .await 此方法。
    pub async fn stop_recording(&self) {
        // 取出 engine + 停止采集 + abort 音频 task，然后立即释放锁
        let (engine, target) = {
            let mut session = self.session.lock().unwrap();

            if !session.recording {
                tracing::warn!("stop_recording: 未在录音中,忽略");
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

            // 清除全局录音标志
            crate::infra::platform::hotkey::set_voice_recording(false);

            (engine, target)
        }; // 锁在此释放，await 不持锁

        // 最终识别（async）
        // 加 10s 超时保护：即使 abort 后仍有异常情况（如 WS 半连接），不会永久卡住
        let final_text = match engine {
            Some(e) => {
                match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    e.finalize(),
                ).await {
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
        };

        tracing::info!(
            target = ?target,
            text_len = final_text.chars().count(),
            %final_text,
            "语音识别完成"
        );

        if final_text.is_empty() {
            tracing::info!("识别结果为空,跳过注入");
            // G2: 隐藏 mini overlay
            if target == VoiceTarget::ForegroundApp {
                platform::window::hide_voice_overlay(&self.app);
            }
            let _ = self.app.emit("blink://voice-recording-end", ());
            return;
        }

        match target {
            VoiceTarget::MainWindow => {
                // G1: 填进 #query(复用 chord-fill-query 链路)
                // payload 必须是 serde_json::Value::String,与 chord 模块 pattern 一致
                let _ = self.app.emit(
                    "blink://chord-fill-query",
                    serde_json::Value::String(final_text.clone()),
                );
                tracing::info!(text = %final_text, "G1: 文字已 emit chord-fill-query");
                let _ = self.app.emit("blink://voice-recording-end", ());
            }
            VoiceTarget::ForegroundApp => {
                // G2: 注入前台应用
                // 注入在 spawn_blocking 中执行(SendInput 需要同线程)
                let app = self.app.clone();
                let prev_hwnd = {
                    let session = self.session.lock().unwrap();
                    session.prev_fg_hwnd
                };
                tokio::task::spawn_blocking(move || {
                    // 注入前恢复前台窗口焦点（finalize 期间焦点可能漂移）
                    if let Some(hwnd) = prev_hwnd {
                        platform::window::restore_foreground(hwnd);
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    match platform::inject::inject_text(&final_text) {
                        Ok(()) => {
                            tracing::info!("G2: 文字已注入前台应用");
                        }
                        Err(e) => {
                            tracing::error!(%e, "G2: 文本注入失败");
                        }
                    }
                    // 注入完成后隐藏 overlay + emit end
                    platform::window::hide_voice_overlay(&app);
                    let _ = app.emit("blink://voice-recording-end", ());
                });
            }
        }
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
        session.last_partial.clear();

        // 清除全局录音标志
        crate::infra::platform::hotkey::set_voice_recording(false);

        tracing::info!("语音录音已取消");
        drop(session);

        // 隐藏 mini overlay(G2)
        platform::window::hide_voice_overlay(&self.app);
    }

    /// 是否正在录音。
    pub fn is_recording(&self) -> bool {
        self.session.lock().unwrap().recording
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
