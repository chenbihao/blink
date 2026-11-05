//! 录制音频受限 IPC：设备/试音/采集静音/异常替换及源轨二进制读取。
use crate::app::{command_error::CommandError, gif_service::GifService};
use blink_infra::{
    gif::audio::{self, AudioOptions, AudioStatus},
    platform::audio::recording,
};
use std::sync::Arc;
use tauri::{AppHandle, Manager, WebviewWindow};
fn error(message: impl AsRef<str>) -> CommandError {
    CommandError::new("recording_audio", message, true)
}
fn overlay(window: &WebviewWindow) -> Result<(), CommandError> {
    if window.label() != "chord-screenshot" {
        return Err(error("此操作仅用于录制浮栏"));
    }
    Ok(())
}
#[tauri::command]
pub async fn gif_audio_devices(
    window: WebviewWindow,
) -> Result<Vec<recording::DeviceInfo>, CommandError> {
    overlay(&window)?;
    tauri::async_runtime::spawn_blocking(recording::devices)
        .await
        .map_err(|_| error("设备枚举失败"))?
        .map_err(error)
}
#[tauri::command]
pub async fn gif_audio_monitor(
    app: AppHandle,
    window: WebviewWindow,
    options: Option<AudioOptions>,
) -> Result<(), CommandError> {
    overlay(&window)?;
    let handle = app.clone();
    let monitored = tauri::async_runtime::spawn_blocking(move || {
        let service = handle.state::<Arc<GifService>>();
        // 与正式录制启动共用所有权门，避免试音迟到占用录制的设备。
        let _session = service.lock_overlay_session();
        if let Some(options) = options.filter(|v| v.mode != audio::AudioMode::Silent) {
            if service
                .recording
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .is_some()
            {
                return Err(error("录制期间不能启动试音"));
            }
            Ok(Some(service.audio_preview.configure(options)))
        } else {
            service.audio_preview.stop();
            Ok(None)
        }
    })
    .await
    .map_err(|_| error("声音试音任务失败"))??;
    if let Some(generation) = monitored {
        tauri::async_runtime::spawn(async move {
            loop {
                tokio::time::sleep(std::time::Duration::from_millis(250)).await;
                let visible = app
                    .get_webview_window("chord-screenshot")
                    .is_some_and(|window| window.is_visible().unwrap_or(false));
                let service = app.state::<Arc<GifService>>().inner().clone();
                // 试音启动和设备释放可能占锁并 join worker，检测与释放均留在 blocking 池。
                let current = tauri::async_runtime::spawn_blocking(move || {
                    let current = service.audio_preview.generation() == generation;
                    if current && !visible && service.audio_preview.stop_if_generation(generation) {
                        tracing::info!("录制准备窗口已隐藏，释放试音设备");
                    }
                    current
                })
                .await
                .unwrap_or(false);
                if !current || !visible {
                    break;
                }
            }
        });
    }
    Ok(())
}

#[tauri::command]
pub fn gif_audio_monitor_status(
    app: AppHandle,
    window: WebviewWindow,
) -> Result<AudioStatus, CommandError> {
    overlay(&window)?;
    let service = app.state::<Arc<GifService>>();
    Ok(service.audio_preview.snapshot())
}
#[tauri::command]
pub fn gif_audio_control(
    app: AppHandle,
    window: WebviewWindow,
    session_id: String,
    source: Option<usize>,
    muted: Option<bool>,
    options: Option<AudioOptions>,
) -> Result<(), CommandError> {
    overlay(&window)?;
    let service = app.state::<Arc<GifService>>();
    let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    let r = rec
        .as_ref()
        .filter(|r| r.session_id == session_id)
        .ok_or_else(|| error("录制会话已结束"))?;
    let mut gate = r.audio.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(options) = options {
        if r.ctl.phase() != "paused" || gate.status.active {
            return Err(error("请先暂停录制，再更换同类声音设备"));
        }
        if options.mode != gate.options.mode {
            return Err(error("恢复时不能改变声音模式"));
        }
        gate.options = options;
    }
    if let (Some(source), Some(muted)) = (source, muted) {
        if source > 1 || !gate.options.enabled(source) {
            return Err(error("声音来源无效"));
        }
        let at = gate.time_ms(std::time::Instant::now());
        gate.status.muted[source] = muted;
        gate.mute_edges[source].push((at, muted));
        gate.status.rms[source] = 0.0;
        gate.status.peak[source] = 0.0;
        tracing::info!(source, muted, "录制采集静音状态已更新");
    }
    Ok(())
}
pub fn serve_recording_audio(
    app: &AppHandle,
    label: &str,
    path: &str,
    range: Option<&str>,
) -> tauri::http::Response<Vec<u8>> {
    let response = || -> Result<_, CommandError> {
        if label != "gif-editor" {
            return Err(error("此资源仅用于录制编辑器"));
        }
        let (session_id, source) = path
            .trim_start_matches('/')
            .split_once('/')
            .ok_or_else(|| error("音轨地址无效"))?;
        let source: usize = source.parse().map_err(|_| error("声音来源无效"))?;
        let service = app.state::<Arc<GifService>>();
        if service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .as_deref()
            != Some(session_id)
        {
            return Err(error("编辑会话已切换"));
        }
        let (reader, _) = super::gif::with_session(app, session_id)?;
        let manifest = audio::manifest(reader.dir())
            .map_err(|_| error("声音元数据损坏"))?
            .ok_or_else(|| error("此素材没有声音"))?;
        if source > 1 || !manifest.enabled[source] {
            return Err(error("声音来源不存在"));
        }
        let end = reader.source_end_ms();
        audio::read_wav_range(reader.dir(), source, end, range)
            .map_err(|_| error("声音素材缺失或 Range 无效"))
    };
    match response() {
        Ok((body, start, end, total)) => {
            let mut builder = tauri::http::Response::builder()
                .status(if range.is_some() { 206 } else { 200 })
                .header("Content-Type", "audio/wav")
                .header("Accept-Ranges", "bytes")
                .header("Access-Control-Allow-Origin", "*")
                .header("Cache-Control", "no-store")
                .header("Content-Length", body.len());
            if range.is_some() {
                builder = builder.header("Content-Range", format!("bytes {start}-{end}/{total}"));
            }
            builder.body(body).unwrap()
        }
        Err(_) => tauri::http::Response::builder()
            .status(404)
            .body(Vec::new())
            .unwrap(),
    }
}
