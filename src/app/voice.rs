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
use crate::infra::platform::audio::{AudioCapture, AudioFormat};
use crate::infra::platform;

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
    /// 目标(G1/G2)
    target: VoiceTarget,
    /// 是否正在录音
    recording: bool,
    /// 最新 partial 文本
    last_partial: String,
}

impl Default for VoiceSession {
    fn default() -> Self {
        Self {
            engine: None,
            capture: None,
            target: VoiceTarget::ForegroundApp,
            recording: false,
            last_partial: String::new(),
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
        let engine = crate::domain::stt::create_engine();
        let config = crate::app::stt_config::get_stt_config();
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
                    platform::window::show_voice_overlay(&self.app);
                }

                // spawn 采集 task: audio chunk → STT → emit partial
                let app = self.app.clone();
                let target = session.target;
                let engine: Arc<dyn SttEngine> = Arc::from(engine);
                let engine_for_task = engine.clone();

                tokio::spawn(async move {
                    while let Some(chunk) = rx.recv().await {
                        // 计算 RMS 音量（0.0 ~ 1.0）
                        let level = compute_rms(&chunk.samples);
                        let target_str = if target == VoiceTarget::MainWindow { "g1" } else { "g2" };
                        let _ = app.emit(
                            "blink://voice-level",
                            serde_json::json!({
                                "level": level,
                                "target": target_str,
                            }),
                        );

                        match engine_for_task.transcribe_chunk(&chunk.samples) {
                            Ok(text) => {
                                if !text.is_empty() {
                                    let _ = app.emit(
                                        "blink://voice-partial",
                                        serde_json::json!({
                                            "text": text,
                                            "target": target_str,
                                        }),
                                    );
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
            }
            Err(e) => {
                tracing::error!(%e, "音频采集启动失败");
            }
        }
    }

    /// HoldRelease 事件:停止录音 → STT 最终识别 → 注入/填充。
    pub fn stop_recording(&self) {
        let mut session = self.session.lock().unwrap();

        if !session.recording {
            tracing::warn!("stop_recording: 未在录音中,忽略");
            return;
        }

        // 停止采集
        if let Some(mut capture) = session.capture.take() {
            capture.stop();
        }

        // 最终识别
        let final_text = session
            .engine
            .as_ref()
            .and_then(|engine| engine.finalize().ok())
            .unwrap_or_default();

        session.recording = false;
        let target = session.target;
        session.engine = None;

        // 清除全局录音标志
        crate::infra::platform::hotkey::set_voice_recording(false);

        tracing::info!(
            target = ?target,
            text_len = final_text.chars().count(),
            %final_text,
            "语音识别完成"
        );

        // 释放锁后执行注入(避免阻塞其他操作)
        drop(session);

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
                tokio::task::spawn_blocking(move || {
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

/// 计算 PCM 样本的 RMS（均方根）音量，返回 0.0 ~ 1.0。
/// 用于前端音量波动条可视化。
fn compute_rms(samples: &[f32]) -> f64 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|s| (*s as f64) * (*s as f64)).sum();
    let rms = (sum_sq / samples.len() as f64).sqrt();
    // 归一化到 0~1：RMS 通常在 0~0.3 范围，乘以 3 放大
    (rms * 3.0).min(1.0)
}
