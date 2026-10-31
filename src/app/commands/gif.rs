//! GIF 录制命令层（0.28.7）。
//!
//! 命令分组：
//! - 录制：`gif_start_recording` / `gif_pause_recording` / `gif_resume_recording`
//!   / `gif_stop_recording` / `gif_cancel_recording` / `gif_recorder_state`
//! - 会话/编辑：`gif_list_sessions` / `gif_discard_session` / `gif_get_session`
//!   / `gif_editor_current` / `gif_get_frames` / `gif_edit`
//! - 导出：`gif_export` / `gif_export_cancel` / `gif_open_containing_folder`
//!
//! 原始帧不出后端——预览/缩略图按需解码 + 缩放 + PNG（base64）返回。

use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use blink_domain::gif::EditCommand;
use blink_infra::gif::{
    self as infra_gif, CaptureCommand, SpoolReader,
};

use crate::app::command_error::CommandError;
use crate::app::gif_service::{self, GifService, GifServiceError};

fn err(e: GifServiceError) -> CommandError {
    CommandError::new("gif_error", e.to_string(), false)
}

fn io_err(e: std::io::Error) -> CommandError {
    CommandError::new("gif_io", e.to_string(), false)
}

/// 打开（或重建）会话时间轴，返回其 reader 元数据与视图。
pub(super) fn with_session(
    app: &AppHandle,
    session_id: &str,
) -> Result<(SpoolReader, Arc<GifService>), CommandError> {
    let service = app.state::<Arc<GifService>>().inner().clone();
    let reader = SpoolReader::open(&GifService::root(), session_id).map_err(|e| {
        CommandError::new("gif_session", e.to_string(), false)
    })?;
    Ok((reader, service))
}

/// 确保内存时间轴存在（编辑器重开 / 崩溃后重建）。
pub(super) fn ensure_timeline(service: &Arc<GifService>, reader: &SpoolReader, session_id: &str) {
    let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    timelines
        .entry(session_id.to_string())
        .or_insert_with(|| {
            let durs = reader.durations();
            blink_domain::gif::Timeline::from_durations(durs)
        });
}

// ── 录制 ────────────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GifStartInfo {
    pub session_id: String,
}

/// async 命令：创建控制条 WebView 窗口的 `build()` 若在同步命令（主线程）
/// 里执行，会与 WebView2 初始化互相等待（实测 invoke Promise 长时间 pending）；
/// async 命令跑 tokio 线程，窗口创建由 Tauri marshal 到空闲主循环执行
/// （`plugin_surface::open_plugin_page` 同模式）。
#[tauri::command]
pub async fn gif_start_recording(
    app: AppHandle,
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    fps: Option<u32>,
    show_cursor: Option<bool>,
    // 0.28.7 体验：截图工具栏的屏幕物理坐标（overlay 传给后端）——控制条
    // 定位到这里，视觉上「工具栏原地切换成录制栏」（同长截图的模式切换）。
    control_x: Option<i32>,
    control_y: Option<i32>,
) -> Result<GifStartInfo, CommandError> {
    let session_id = gif_service::start_recording(
        &app,
        x,
        y,
        w,
        h,
        fps.unwrap_or(15).clamp(5, 30),
        show_cursor.unwrap_or(true),
        control_x,
        control_y,
    )
    .map_err(err)?;
    Ok(GifStartInfo { session_id })
}

#[tauri::command]
pub fn gif_pause_recording(app: AppHandle) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Pause, false).map_err(err)
}

#[tauri::command]
pub fn gif_resume_recording(app: AppHandle) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Resume, false).map_err(err)
}

/// 停止并进入编辑器（素材保留）。
#[tauri::command]
pub fn gif_stop_recording(app: AppHandle) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Stop, false).map_err(err)
}

/// 取消并丢弃素材。
#[tauri::command]
pub fn gif_cancel_recording(app: AppHandle) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Stop, true).map_err(err)
}

/// 移动录制区域（拖动柄上报目标坐标；尺寸不变，采集位置热更新）。
#[tauri::command]
pub fn gif_move_recording_region(app: AppHandle, x: i32, y: i32) -> Result<(), CommandError> {
    gif_service::move_recording_region(&app, x, y).map_err(err)
}

/// 预热录制辅助窗口（准备模式进入时调用）：三窗隐藏态建好，点开始走
/// 复用路径毫秒级出边框。async 同 gif_start_recording——窗口创建不能在
/// 同步命令（主线程）里执行（WebView2 初始化互等）。
#[tauri::command]
pub async fn gif_prewarm_windows(app: AppHandle) -> Result<(), CommandError> {
    gif_service::prewarm_windows(&app);
    Ok(())
}

/// 当前录制区域 + 虚拟屏边界（拖动柄窗口加载时拉取；无活跃录制返回 null）。
#[tauri::command]
pub fn gif_recording_region(app: AppHandle) -> Option<serde_json::Value> {
    let service = app.state::<Arc<GifService>>();
    gif_service::recording_region_snapshot(&service)
}

/// 控制条 ready 拉取当前状态。
#[tauri::command]
pub fn gif_recorder_state(app: AppHandle) -> serde_json::Value {
    let service = app.state::<Arc<GifService>>();
    let payload = gif_service::snapshot_state_public(&service);
    serde_json::to_value(payload).unwrap_or_default()
}

// ── 会话与编辑 ──────────────────────────────────────────────────────────────

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GifSessionSummary {
    pub id: String,
    pub created_at: u64,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub frames: u32,
    pub state: String,
}

#[tauri::command]
pub fn gif_list_sessions() -> Result<Vec<GifSessionSummary>, CommandError> {
    let root = GifService::root();
    let mut out = Vec::new();
    for id in infra_gif::list_session_ids(&root) {
        if let Ok(r) = SpoolReader::open(&root, &id) {
            out.push(GifSessionSummary {
                id,
                created_at: r.meta.created_at,
                width: r.meta.region.w,
                height: r.meta.region.h,
                fps: r.meta.fps,
                frames: r.frame_count(),
                state: r.meta.state.clone(),
            });
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// 丢弃会话素材（录制中会话拒绝——先停止/取消）。
#[tauri::command]
pub fn gif_discard_session(app: AppHandle, session_id: String) -> Result<(), CommandError> {
    let _task = blink_infra::media::acquire_task_lease().map_err(|e| CommandError::new(e.code(), e.to_string(), true))?;
    let _ = with_session(&app, &session_id)?;
    let service = app.state::<Arc<GifService>>();
    {
        let rec = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if matches!(rec.as_ref(), Some(r) if r.session_id == session_id) {
            return Err(err(GifServiceError::SessionNotFound(
                format!("{session_id}（录制进行中，请先停止/取消）"),
            )));
        }
    }
    service
        .timelines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id);
    let dir = GifService::root().join("sessions").join(&session_id);
    std::fs::remove_dir_all(&dir).map_err(io_err)?;
    tracing::info!(session_id, "GIF 会话素材已丢弃");
    Ok(())
}

/// 会话详情：元数据 + 时间轴视图。
#[tauri::command]
pub fn gif_get_session(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, CommandError> {
    let (reader, service) = with_session(&app, &session_id)?;
    ensure_timeline(&service, &reader, &session_id);
    let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines.get(&session_id).ok_or_else(|| {
        CommandError::new("gif_session", "时间轴构建失败", false)
    })?;
    let mut view = gif_service::timeline_view(tl);
    // 附加会话元数据（stopReason 供编辑器展示"为何自动停止"）
    let obj = view.as_object_mut().unwrap();
    obj.insert(
        "meta".into(),
        serde_json::json!({
            "id": session_id,
            "width": reader.meta.region.w,
            "height": reader.meta.region.h,
            "fps": reader.meta.fps,
            "showCursor": reader.meta.show_cursor,
            "createdAt": reader.meta.created_at,
            "state": reader.meta.state,
            "stopReason": reader.meta.stop_reason,
        }),
    );
    Ok(view)
}

/// 编辑器当前会话（ready/pull；None = 等待事件）。
#[tauri::command]
pub fn gif_editor_current(app: AppHandle) -> Option<String> {
    let service = app.state::<Arc<GifService>>();
    service
        .editor_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone()
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GifFramePng {
    pub index: u32,
    pub width: u32,
    pub height: u32,
    pub png_base64: String,
}

/// 按需解码帧（缩略图/预览）：BGRA → 等比缩放（max_w）→ PNG base64。
/// 单次上限 24 帧（分页加载；原始帧不整段驻前端内存）。
#[tauri::command]
pub async fn gif_get_frames(
    app: AppHandle,
    session_id: String,
    indices: Vec<u32>,
    max_w: Option<u32>,
) -> Result<Vec<GifFramePng>, CommandError> {
    if indices.len() > 24 {
        return Err(CommandError::new(
            "gif_frames",
            "单次最多 24 帧（分页加载）",
            false,
        ));
    }
    tauri::async_runtime::spawn_blocking(move || {
        let (reader, _service) = with_session(&app, &session_id)?;
        let (sw, sh) = (reader.meta.region.w, reader.meta.region.h);
        let max_w = max_w.unwrap_or(240).clamp(80, 4096);
        let scale = if sw > max_w { max_w as f64 / sw as f64 } else { 1.0 };
        let (dw, dh) = (
            ((sw as f64 * scale).round() as u32).max(1),
            ((sh as f64 * scale).round() as u32).max(1),
        );
        let mut out = Vec::with_capacity(indices.len());
        for idx in indices {
            let bgra = reader.read_frame(idx).map_err(|e| {
                CommandError::new("gif_frame", e.to_string(), false)
            })?;
            // resize_bgra 输出已是 RGBA；screenshot::encode_png 契约是 BGRA 输入
            // （内部再 R↔B swap），此处必须用无交换的 encode_png_rgba
            let rgba = infra_gif::encode::resize_bgra(&bgra, sw, sh, dw, dh);
            let png = infra_gif::encode::encode_png_rgba(&rgba, dw, dh)
                .map_err(|e| CommandError::new("gif_png", e, false))?;
            out.push(GifFramePng {
                index: idx,
                width: dw,
                height: dh,
                png_base64: base64::engine::general_purpose::STANDARD.encode(png),
            });
        }
        Ok(out)
    })
    .await
    .map_err(|e| CommandError::new("gif_frame", format!("帧解码任务失败: {e}"), true))?
}

/// 应用编辑命令（trim/delete/speed/hold/loop/reset + undo/redo 栈操作）。
///
/// `command` 为 JSON：`type: undo|redo` 特判为栈操作（不是 EditCommand 变体），
/// 其余按 `EditCommand` 反序列化。
#[tauri::command]
pub fn gif_edit(
    app: AppHandle,
    session_id: String,
    command: serde_json::Value,
) -> Result<serde_json::Value, CommandError> {
    let (reader, service) = with_session(&app, &session_id)?;
    ensure_timeline(&service, &reader, &session_id);
    let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines.get_mut(&session_id).ok_or_else(|| {
        CommandError::new("gif_session", "时间轴构建失败", false)
    })?;
    match command.get("type").and_then(|v| v.as_str()) {
        Some("undo") => {
            tl.undo();
        }
        Some("redo") => {
            tl.redo();
        }
        _ => {
            let cmd: EditCommand = serde_json::from_value(command).map_err(|e| {
                CommandError::new("gif_edit", format!("命令格式错误: {e}"), false)
            })?;
            tl.apply(&cmd)
                .map_err(|e| CommandError::new("gif_edit", e.to_string(), false))?;
        }
    }
    Ok(gif_service::timeline_view(tl))
}

/// 在资源管理器中定位导出文件（explorer /select）。
#[tauri::command]
pub fn gif_open_containing_folder(path: String) -> Result<(), CommandError> {
    if !std::path::Path::new(&path).extension().and_then(|v| v.to_str()).is_some_and(|v| v.eq_ignore_ascii_case("gif") || v.eq_ignore_ascii_case("mp4")) {
        return Err(CommandError::new("gif_path", "仅支持定位 GIF / MP4 成品", false));
    }
    let p = std::path::Path::new(&path);
    if !p.is_file() {
        return Err(CommandError::new("gif_path", "文件不存在", false));
    }
    blink_infra::platform::no_window(std::process::Command::new("explorer.exe"))
        .arg("/select,")
        .arg(p)
        .spawn()
        .map_err(io_err)?;
    Ok(())
}
