//! GIF 录制命令层（0.28.7）。
//!
//! 命令分组：
//! - 录制：`gif_start_recording` / `gif_pause_recording` / `gif_resume_recording`
//!   / `gif_stop_recording` / `gif_cancel_recording` / `gif_recorder_state`
//! - 会话/编辑：`gif_list_sessions` / `gif_discard_session` / `gif_get_session`
//!   / `gif_editor_current` / `gif_get_frames` / `gif_edit` / `gif_set_play_position`
//! - 最近录制（0.28.23）：`gif_recent_sessions` / `gif_session_thumbnail`
//!   / `gif_open_session` / `gif_open_editor_entry`
//! - 导出：`gif_export` / `gif_export_cancel` / `gif_open_containing_folder`
//!
//! 原始帧不出后端——预览/缩略图按需解码 + 缩放 + PNG（base64）返回。

use std::sync::Arc;

use base64::Engine;
use serde::Serialize;
use tauri::{AppHandle, Manager};

use blink_infra::gif::{self as infra_gif, CaptureCommand, SpoolReader};

use crate::app::command_error::CommandError;
use crate::app::gif_draft::DraftRestore;
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
    if service.recording.lock().unwrap_or_else(|e| e.into_inner()).as_ref()
        .is_some_and(|r| r.session_id == session_id) {
        return Err(CommandError::new("gif_session_busy", "素材仍在录制，请先停止后再编辑或导出", true));
    }
    let reader = SpoolReader::open(&GifService::root(), session_id)
        .map_err(|e| CommandError::new("gif_session", e.to_string(), false))?;
    Ok((reader, service))
}

/// 确保内存时间轴存在（编辑器重开 / 崩溃后重建）。首建时记录素材身份并
/// 尝试恢复编辑草稿（0.28.23 §5.7.4）——所有首建路径（查看/编辑/导出）
/// 一致，不会因先到的命令不同而丢失进度；已在内存则返回 `Missing`。
pub(super) fn ensure_timeline(
    service: &Arc<GifService>,
    reader: &SpoolReader,
    session_id: &str,
) -> DraftRestore {
    crate::app::gif_draft::ensure_timeline(service, reader, session_id)
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
    audio: Option<blink_infra::gif::audio::AudioOptions>,
) -> Result<GifStartInfo, CommandError> {
    let session_id = tauri::async_runtime::spawn_blocking(move || gif_service::start_recording(
        &app,
        x,
        y,
        w,
        h,
        fps.unwrap_or(15).clamp(5, 30),
        show_cursor.unwrap_or(true),
        audio.unwrap_or_default(),
    )).await.map_err(|_| CommandError::new("gif_start", "录制准备任务失败", true))?.map_err(err)?;
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

// ── overlay 录制叠层输入（0.28.23：统一浮栏由 chord-screenshot overlay 承担）──

#[tauri::command]
pub fn gif_overlay_ready(app: AppHandle, session_id: String) -> Result<(), CommandError> {
    gif_service::overlay_ready(&app, &session_id).map_err(err)
}

/// 上报 overlay 输入模式与命中矩形：`desktop=true` 操作桌面（穿透 + 命中
/// 矩形内可点），`desktop=false` 涂画（整窗接收指针）。矩形为物理屏幕坐标。
#[tauri::command]
pub fn gif_overlay_input(
    app: AppHandle,
    desktop: bool,
    rects: Option<Vec<[i32; 4]>>,
) -> Result<(), CommandError> {
    gif_service::set_overlay_input(&app, desktop, rects.unwrap_or_default()).map_err(err)
}

/// 仅刷新命中矩形（工具栏拖动/弹层开合后的周期上报）。
#[tauri::command]
pub fn gif_overlay_hit_rects(app: AppHandle, rects: Vec<[i32; 4]>) -> Result<(), CommandError> {
    gif_service::set_overlay_hit_rects(&app, rects).map_err(err)
}

/// 文字输入期间申请/归还 overlay 键盘焦点。
#[tauri::command]
pub fn gif_overlay_focusable(app: AppHandle, focusable: bool) -> Result<(), CommandError> {
    gif_service::set_overlay_focusable(&app, focusable).map_err(err)
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
            return Err(err(GifServiceError::SessionNotFound(format!(
                "{session_id}（录制进行中，请先停止/取消）"
            ))));
        }
    }
    let current = service.editor_session.lock().unwrap_or_else(|e| e.into_inner());
    if current.as_deref() == Some(session_id.as_str()) {
        return Err(CommandError::new(
            "gif_session_busy",
            "当前编辑会话不能删除，请先切换",
            true,
        ));
    }
    infra_gif::spool::discard_session(&GifService::root(), &session_id)
        .map_err(|e| CommandError::new("gif_session_busy", e.to_string(), true))?;
    service
        .timelines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(&session_id);
    // 内存草稿/身份数据一并清理（目录删除已带走草稿文件；防抖任务因
    // 状态缺失自然失效，不复活已删会话）
    crate::app::gif_draft::forget_session(&service, &session_id);
    tracing::info!(session_id, "GIF 会话素材已丢弃");
    Ok(())
}

/// 会话详情：元数据 + 时间轴视图 + 草稿恢复状态（0.28.23）。
#[tauri::command]
pub fn gif_get_session(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, CommandError> {
    let (reader, service) = with_session(&app, &session_id)?;
    let restore = ensure_timeline(&service, &reader, &session_id);
    let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines
        .get(&session_id)
        .ok_or_else(|| CommandError::new("gif_session", "时间轴构建失败", false))?;
    let mut view = gif_service::timeline_view(tl);
    drop(timelines);
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
    // 草稿状态：首开恢复结果，或活跃会话的已记录播放位置（重开窗口续播）
    let (status, play_pos) = match restore {
        DraftRestore::Restored(pos) => ("restored", pos),
        DraftRestore::Discarded => ("discarded", 0),
        DraftRestore::Missing => {
            let pos = service
                .drafts
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&session_id)
                .map(|s| s.play_pos_ms)
                .unwrap_or(0);
            ("none", pos)
        }
    };
    obj.insert("draftStatus".into(), serde_json::json!(status));
    obj.insert("draftPlayPosMs".into(), serde_json::json!(play_pos));
    obj.insert(
        "audio".into(),
        serde_json::to_value(infra_gif::audio::manifest(reader.dir()).map_err(io_err)?)
            .unwrap_or_default(),
    );
    obj.insert("audioRecovered".into(), reader.audio_recovered.into());
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
            let bgra = reader
                .read_frame(idx)
                .map_err(|e| CommandError::new("gif_frame", e.to_string(), false))?;
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
/// 其余按 `EditCommand` 反序列化。成功后调度编辑草稿防抖保存（0.28.23）。
#[tauri::command]
pub fn gif_edit(
    app: AppHandle,
    session_id: String,
    command: serde_json::Value,
    incremental: Option<bool>,
) -> Result<serde_json::Value, CommandError> {
    let (reader, service) = with_session(&app, &session_id)?;
    let _ = ensure_timeline(&service, &reader, &session_id);
    let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines
        .get_mut(&session_id)
        .ok_or_else(|| CommandError::new("gif_session", "时间轴构建失败", false))?;
    let invalidated_from = command
        .get("frame")
        .and_then(|v| v.as_u64())
        .or_else(|| {
            command
                .get("frames")
                .and_then(|v| v.as_array())
                .and_then(|v| v.iter().filter_map(|f| f.as_u64()).min())
        })
        .unwrap_or(0);
    match command.get("type").and_then(|v| v.as_str()) {
        Some("undo") => {
            tl.undo();
        }
        Some("redo") => {
            tl.redo();
        }
        _ => {
            let cmd: blink_domain::gif::EditCommand = serde_json::from_value(command)
                .map_err(|e| CommandError::new("gif_edit", format!("命令格式错误: {e}"), false))?;
            tl.apply(&cmd)
                .map_err(|e| CommandError::new("gif_edit", e.to_string(), false))?;
        }
    }
    let view = if incremental.unwrap_or(false) {
        serde_json::json!({"revision": tl.revision(), "frameCount": tl.frame_count(), "totalMs": tl.effective_total_ms(),
            "invalidatedFrom": invalidated_from, "canUndo": tl.can_undo(), "canRedo": tl.can_redo()})
    } else {
        gif_service::timeline_view(tl)
    };
    drop(timelines);
    crate::app::gif_draft::schedule_draft_save(&app, &session_id);
    Ok(view)
}

/// 分页帧源契约：revision 对账后才返回页，不依赖前端全量帧表；增量编辑返回失效起点。
#[tauri::command]
pub fn gif_timeline_page(
    app: AppHandle,
    session_id: String,
    offset: u32,
    limit: u32,
    revision: Option<u64>,
) -> Result<serde_json::Value, CommandError> {
    let (reader, service) = with_session(&app, &session_id)?;
    ensure_timeline(&service, &reader, &session_id);
    let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines
        .get(&session_id)
        .ok_or_else(|| CommandError::new("gif_session", "时间轴不存在", false))?;
    if revision.is_some_and(|v| v != tl.revision()) {
        return Err(CommandError::new(
            "gif_revision",
            "时间轴已变化，请重新请求",
            true,
        ));
    }
    let frames: Vec<_> = tl.frame_page(offset as usize, limit.clamp(1, 256) as usize).iter()
        .map(|f| serde_json::json!({"srcIdx": f.src_idx, "durMs": f.dur_ms, "held": f.held})).collect();
    Ok(
        serde_json::json!({"revision": tl.revision(), "offset": offset, "frames": frames,
        "totalMs": tl.effective_total_ms(), "mix": tl.mix(), "audioSpans": tl.audio_spans()}),
    )
}

// ── 最近录制与编辑进度（0.28.23 §5.7.4） ───────────────────────────────────

/// 播放头位置更新（编辑器定位提交；与编辑保存共用草稿防抖链）。
#[tauri::command]
pub fn gif_set_play_position(
    app: AppHandle,
    session_id: String,
    pos_ms: u64,
) -> Result<(), CommandError> {
    crate::app::gif_draft::set_play_position(&app, &session_id, pos_ms);
    Ok(())
}

/// 最近录制分页摘要（按创建时间倒序；轻量——不解析完整帧索引/素材）。
#[tauri::command]
pub async fn gif_recent_sessions(
    offset: Option<u32>,
    limit: Option<u32>,
) -> Result<serde_json::Value, CommandError> {
    let offset = offset.unwrap_or(0) as usize;
    let limit = limit.unwrap_or(20).clamp(1, 50) as usize;
    tauri::async_runtime::spawn_blocking(move || {
        let (items, total) = infra_gif::list_summaries(&GifService::root(), offset, limit);
        serde_json::json!({
            "items": items,
            "total": total,
            "offset": offset,
            "limit": limit,
        })
    })
    .await
    .map_err(|e| CommandError::new("gif_session", format!("摘要任务失败: {e}"), true))
}

/// 会话缩略图（首帧等比缩放 PNG base64；列表可见项按需取，不预载素材）。
#[tauri::command]
pub async fn gif_session_thumbnail(
    app: AppHandle,
    session_id: String,
) -> Result<GifFramePng, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (reader, _service) = with_session(&app, &session_id)?;
        if reader.frame_count() == 0 {
            return Err(CommandError::new("gif_frame", "会话没有可显示的帧", false));
        }
        let (sw, sh) = (reader.meta.region.w, reader.meta.region.h);
        let max_w = 200u32;
        let scale = if sw > max_w { max_w as f64 / sw as f64 } else { 1.0 };
        let (dw, dh) = (
            ((sw as f64 * scale).round() as u32).max(1),
            ((sh as f64 * scale).round() as u32).max(1),
        );
        let bgra = reader
            .read_frame(0)
            .map_err(|e| CommandError::new("gif_frame", e.to_string(), false))?;
        let rgba = infra_gif::encode::resize_bgra(&bgra, sw, sh, dw, dh);
        let png = infra_gif::encode::encode_png_rgba(&rgba, dw, dh)
            .map_err(|e| CommandError::new("gif_png", e, false))?;
        Ok(GifFramePng {
            index: 0,
            width: dw,
            height: dh,
            png_base64: base64::engine::general_purpose::STANDARD.encode(png),
        })
    })
    .await
    .map_err(|e| CommandError::new("gif_frame", format!("缩略图任务失败: {e}"), true))?
}

/// 从最近录制打开指定会话（编辑器复用换会话）。
///
/// 当前编辑会话有在途导出时拒绝切换（0.28.23：须先完成或显式取消，
/// 不静默取消导出）——前端捕获 `gif_export_busy` 后确认并取消再重试。
#[tauri::command]
pub fn gif_open_session(app: AppHandle, session_id: String) -> Result<(), CommandError> {
    {
        let service = app.state::<Arc<GifService>>();
        let current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if let Some(cur) = current.as_deref().filter(|c| *c != session_id.as_str()) {
            if crate::app::media_service::shared().has_active_job(cur) {
                return Err(CommandError::new(
                    "gif_export_busy",
                    "当前录制正在导出，请先完成或取消导出再切换".to_string(),
                    true,
                ));
            }
        }
    }
    let _ = with_session(&app, &session_id)?; // 会话存在性/可读性校验
    if !gif_service::open_editor(&app, &session_id) {
        return Err(CommandError::new(
            "gif_export_busy",
            "当前录制正在导出，请先完成或取消导出再切换",
            true,
        ));
    }
    Ok(())
}

/// 统一功能目录入口：有编辑会话原地激活，无会话进入「最近录制」首页。
#[tauri::command]
pub fn gif_open_editor_entry(app: AppHandle) -> Result<(), CommandError> {
    gif_service::open_editor_entry(&app);
    Ok(())
}

/// 在资源管理器中定位导出文件（explorer /select）。
#[tauri::command]
pub fn gif_open_containing_folder(path: String) -> Result<(), CommandError> {
    if !std::path::Path::new(&path)
        .extension()
        .and_then(|v| v.to_str())
        .is_some_and(|v| v.eq_ignore_ascii_case("gif") || v.eq_ignore_ascii_case("mp4"))
    {
        return Err(CommandError::new(
            "gif_path",
            "仅支持定位 GIF / MP4 成品",
            false,
        ));
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
