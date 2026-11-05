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
    if matches!(e, GifServiceError::ExportBusy) {
        return CommandError::new("gif_export_busy", e.to_string(), true);
    }
    CommandError::new("gif_error", e.to_string(), false)
}

fn io_err(e: std::io::Error) -> CommandError {
    CommandError::new("gif_io", e.to_string(), false)
}

/// 历史素材的只读校验不依赖当前编辑会话，也不进入活动 reader 缓存。
fn open_material(
    service: &GifService,
    root: &std::path::Path,
    session_id: &str,
    thumbnail: bool,
) -> Result<SpoolReader, CommandError> {
    ensure_not_recording(service, session_id)?;
    let reader = if thumbnail {
        SpoolReader::open_thumbnail(root, session_id)
    } else {
        SpoolReader::open(root, session_id)
    };
    reader.map_err(|e| CommandError::new("gif_session", e.to_string(), false))
}

fn ensure_not_recording(service: &GifService, session_id: &str) -> Result<(), CommandError> {
    if service
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .is_some_and(|r| r.session_id == session_id)
    {
        return Err(CommandError::new(
            "gif_session_busy",
            "素材仍在录制，请先停止后再编辑或导出",
            true,
        ));
    }
    Ok(())
}

/// 打开（或重建）会话时间轴，返回其 reader 元数据与视图。
pub(super) fn with_session(
    app: &AppHandle,
    session_id: &str,
) -> Result<(Arc<SpoolReader>, Arc<GifService>), CommandError> {
    let service = app.state::<Arc<GifService>>().inner().clone();
    ensure_not_recording(&service, session_id)?;
    {
        let current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current.as_deref() != Some(session_id) {
            return Err(CommandError::new(
                "gif_stale_session",
                "编辑会话已切换",
                true,
            ));
        }
        if let Some(reader) = service
            .readers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(session_id)
            .cloned()
        {
            return Ok((reader, service.clone()));
        }
    }
    // Validate/map material without holding the window state gate.
    let opened = Arc::new(
        SpoolReader::open(&GifService::root(), session_id)
            .map_err(|e| CommandError::new("gif_session", e.to_string(), false))?,
    );
    let current = service
        .editor_session
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    if current.as_deref() != Some(session_id) {
        return Err(CommandError::new(
            "gif_stale_session",
            "编辑会话已切换",
            true,
        ));
    }
    let reader = service
        .readers
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(session_id.to_owned())
        .or_insert(opened)
        .clone();
    drop(current);
    Ok((reader, service))
}

/// 确保内存时间轴存在（编辑器重开 / 崩溃后重建）。首建时记录素材身份并
/// 尝试恢复编辑草稿（0.28.23 §5.7.4）——所有首建路径（查看/编辑/导出）
/// 一致，不会因先到的命令不同而丢失进度；已在内存则返回 `Missing`。
pub(super) fn ensure_timeline(
    service: &Arc<GifService>,
    reader: &SpoolReader,
    session_id: &str,
) -> Result<DraftRestore, CommandError> {
    let restored = crate::app::gif_draft::ensure_timeline(service, reader, session_id)
        .map_err(|e| CommandError::new("gif_timing", e, false))?;
    if service
        .editor_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_deref()
        != Some(session_id)
    {
        crate::app::gif_draft::release_if_saved(service, session_id);
        return Err(CommandError::new(
            "gif_stale_session",
            "编辑会话已切换",
            true,
        ));
    }
    Ok(restored)
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
    let session_id = tauri::async_runtime::spawn_blocking(move || {
        gif_service::start_recording(
            &app,
            x,
            y,
            w,
            h,
            fps.unwrap_or(15),
            show_cursor.unwrap_or(true),
            audio.unwrap_or_default(),
        )
    })
    .await
    .map_err(|_| CommandError::new("gif_start", "录制准备任务失败", true))?
    .map_err(err)?;
    Ok(GifStartInfo { session_id })
}

#[tauri::command]
pub fn gif_pause_recording(app: AppHandle, session_id: Option<String>) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Pause, false, session_id.as_deref())
        .map_err(err)
}

#[tauri::command]
pub fn gif_resume_recording(
    app: AppHandle,
    session_id: Option<String>,
) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Resume, false, session_id.as_deref())
        .map_err(err)
}

/// 停止并进入编辑器（素材保留）。
#[tauri::command]
pub fn gif_stop_recording(app: AppHandle, session_id: Option<String>) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Stop, false, session_id.as_deref())
        .map_err(err)
}

/// 取消并丢弃素材。
#[tauri::command]
pub fn gif_cancel_recording(
    app: AppHandle,
    session_id: Option<String>,
) -> Result<(), CommandError> {
    gif_service::command_recording(&app, CaptureCommand::Stop, true, session_id.as_deref())
        .map_err(err)
}

/// 移动录制区域（拖动柄上报目标坐标；尺寸不变，采集位置热更新）。
#[tauri::command]
pub fn gif_move_recording_region(
    app: AppHandle,
    x: i32,
    y: i32,
    session_id: Option<String>,
) -> Result<(), CommandError> {
    gif_service::move_recording_region(&app, x, y, session_id.as_deref()).map_err(err)
}

// ── overlay 录制叠层输入（0.28.23：统一浮栏由 chord-screenshot overlay 承担）──

/// 原生窗口操作可能等待主循环；会话门锁也可能正由收尾线程持有。
/// 两者都不能在同步 IPC 的主线程执行，统一交给阻塞池，异步等待结果。
async fn run_overlay_command<T: Send + 'static>(
    operation: impl FnOnce() -> Result<T, GifServiceError> + Send + 'static,
) -> Result<T, CommandError> {
    tauri::async_runtime::spawn_blocking(operation)
        .await
        .map_err(|e| CommandError::new("gif_overlay", format!("录制浮层任务失败: {e}"), true))?
        .map_err(err)
}

#[tauri::command]
pub async fn gif_overlay_prepare(
    app: AppHandle,
    window: tauri::WebviewWindow,
    active: bool,
    preparation_id: Option<u64>,
) -> Result<Option<u64>, CommandError> {
    if window.label() != "chord-screenshot" {
        return Err(CommandError::new(
            "permission_denied",
            "此操作仅用于录制准备",
            false,
        ));
    }
    run_overlay_command(move || gif_service::prepare_overlay(&app, active, preparation_id)).await
}

/// 原生控件整窗排除；标注通过有界二进制快照单独合成进采集帧。
#[tauri::command]
pub async fn gif_overlay_annotation(
    app: AppHandle,
    window: tauri::WebviewWindow,
    request: tauri::ipc::Request<'_>,
) -> Result<(), CommandError> {
    if window.label() != "chord-screenshot" {
        return Err(CommandError::new(
            "permission_denied",
            "标注更新仅用于录制叠层",
            false,
        ));
    }
    let header = |name: &str| request.headers().get(name).and_then(|v| v.to_str().ok());
    let session_id = header("x-recording-session")
        .ok_or_else(|| CommandError::new("gif_session", "缺少录制会话", false))?
        .to_owned();
    let revision = header("x-annotation-revision")
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|n| *n > 0)
        .ok_or_else(|| CommandError::new("gif_annotation", "标注版本无效", false))?;
    let service = app.state::<Arc<GifService>>().inner().clone();
    let (annotation, permit, width, height) = {
        let recording = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        let r = recording
            .as_ref()
            .filter(|r| r.session_id == session_id)
            .ok_or_else(|| err(GifServiceError::SessionNotFound(session_id.clone())))?;
        let permit = r
            .annotation_updates
            .clone()
            .try_acquire_owned()
            .map_err(|_| CommandError::new("gif_annotation_busy", "标注更新进行中", true))?;
        (r.annotation.clone(), permit, r.region_w, r.region_h)
    };
    let png = match request.body() {
        tauri::ipc::InvokeBody::Raw(bytes) if bytes.len() <= infra_gif::annotation::MAX_BYTES => {
            bytes.clone()
        }
        _ => {
            return Err(CommandError::new(
                "gif_annotation",
                "标注必须使用有界二进制 PNG",
                false,
            ));
        }
    };
    tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        let frame = infra_gif::annotation::AnnotationFrame::decode(&png, width, height)
            .map_err(|e| CommandError::new("gif_annotation", e, true))?;
        let recording = service.recording.lock().unwrap_or_else(|e| e.into_inner());
        if !recording
            .as_ref()
            .is_some_and(|r| r.session_id == session_id && Arc::ptr_eq(&r.annotation, &annotation))
        {
            return Err(CommandError::new(
                "gif_stale_session",
                "录制会话已结束",
                false,
            ));
        }
        let mut state = annotation.lock().unwrap_or_else(|e| e.into_inner());
        if revision > state.revision {
            state.revision = revision;
            state.frame = Some(Arc::new(frame));
        }
        Ok(())
    })
    .await
    .map_err(|e| CommandError::new("gif_annotation", e.to_string(), true))?
}

#[tauri::command]
pub async fn gif_overlay_ready(app: AppHandle, session_id: String) -> Result<(), CommandError> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let result = (|| {
            let service = handle.state::<Arc<GifService>>();
            let recording = service.recording.lock().unwrap_or_else(|e| e.into_inner());
            let r = recording
                .as_ref()
                .filter(|r| r.session_id == session_id)
                .ok_or_else(|| CommandError::new("gif_stale_session", "录制会话已结束", false))?;
            if r.annotation
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .revision
                == 0
            {
                return Err(CommandError::new(
                    "gif_annotation",
                    "标注画布尚未就绪",
                    true,
                ));
            }
            #[cfg(target_os = "windows")]
            blink_infra::platform::window::scroll_capture::exclude_recording_overlay(&handle)
                .map_err(|e| CommandError::new("gif_capture_exclusion", e, true))?;
            drop(recording);
            gif_service::overlay_ready(&handle, &session_id).map_err(err)
        })();
        let _ = tx.send(result);
    })
    .map_err(|e| CommandError::new("gif_overlay", e.to_string(), true))?;
    rx.await
        .map_err(|e| CommandError::new("gif_overlay", e.to_string(), true))?
}

/// 上报 overlay 输入模式与命中矩形：`desktop=true` 操作桌面（穿透 + 命中
/// 矩形内可点），`desktop=false` 涂画（整窗接收指针）。矩形为物理屏幕坐标。
#[tauri::command]
pub async fn gif_overlay_input(
    app: AppHandle,
    desktop: bool,
    rects: Option<Vec<[i32; 4]>>,
    preparation_id: Option<u64>,
) -> Result<(), CommandError> {
    run_overlay_command(move || {
        gif_service::set_overlay_input(&app, desktop, rects.unwrap_or_default(), preparation_id)
    })
    .await
}

/// 仅刷新命中矩形（工具栏拖动/弹层开合后的周期上报）。
#[tauri::command]
pub async fn gif_overlay_hit_rects(
    app: AppHandle,
    rects: Vec<[i32; 4]>,
    preparation_id: Option<u64>,
) -> Result<(), CommandError> {
    run_overlay_command(move || gif_service::set_overlay_hit_rects(&app, rects, preparation_id))
        .await
}

/// 文字输入期间申请/归还 overlay 键盘焦点。
#[tauri::command]
pub async fn gif_overlay_focusable(
    app: AppHandle,
    focusable: bool,
    preparation_id: Option<u64>,
) -> Result<(), CommandError> {
    run_overlay_command(move || gif_service::set_overlay_focusable(&app, focusable, preparation_id))
        .await
}

/// 控制条 ready 拉取当前状态。
#[tauri::command]
pub fn gif_recorder_state(app: AppHandle, restore: Option<bool>) -> serde_json::Value {
    let service = app.state::<Arc<GifService>>();
    let payload = gif_service::snapshot_state_public(&service);
    let mut value = serde_json::to_value(payload).unwrap_or_default();
    let preparing = service
        .preparation
        .load(std::sync::atomic::Ordering::SeqCst);
    if preparing != 0 {
        value["preparationId"] = serde_json::json!(preparing);
    }
    if restore == Some(true) && value["sessionId"].is_string() {
        value["overlayMeta"] = gif_service::overlay_restore_meta();
    }
    value
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
        if let Ok(r) = infra_gif::read_summary(&root, &id) {
            out.push(GifSessionSummary {
                id,
                created_at: r.created_at,
                width: r.width,
                height: r.height,
                fps: r.fps,
                frames: r.frame_count,
                state: r.state,
            });
        }
    }
    out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
    Ok(out)
}

/// 显式删除；当前编辑会话需要单独授权，目录删除成功后才发布空会话。
#[tauri::command]
pub async fn gif_discard_session(
    app: AppHandle,
    window: tauri::WebviewWindow,
    session_id: String,
    discard_current: Option<bool>,
) -> Result<(), CommandError> {
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "此操作仅用于录制编辑器",
            false,
        ));
    }
    let service = app.state::<Arc<GifService>>().inner().clone();
    let handle = app.clone();
    let id = session_id.clone();
    let was_current = tauri::async_runtime::spawn_blocking(move || {
        let _task = blink_infra::media::acquire_task_lease()
            .map_err(|e| CommandError::new(e.code(), e.to_string(), true))?;
        if discard_current.unwrap_or(false) {
            crate::app::gif_draft::flush_draft(&handle, &id);
        }
        discard_session_at(
            &service,
            &GifService::root(),
            &id,
            discard_current.unwrap_or(false),
        )
    })
    .await
    .map_err(|e| CommandError::new("gif_discard", e.to_string(), true))??;
    if was_current {
        use tauri::Emitter;
        let _ = app.emit_to(
            "gif-editor",
            blink_infra::event_names::EventNames::GIF_EDITOR_SESSION,
            serde_json::json!({"sessionId": null, "discardedSessionId": session_id}),
        );
    }
    Ok(())
}

fn discard_session_at(
    service: &Arc<GifService>,
    root: &std::path::Path,
    session_id: &str,
    discard_current: bool,
) -> Result<bool, CommandError> {
    if !infra_gif::spool::is_valid_session_id(session_id) {
        return Err(CommandError::new("gif_session", "素材会话标识无效", false));
    }
    let recording = service.recording.lock().unwrap_or_else(|e| e.into_inner());
    if recording
        .as_ref()
        .is_some_and(|r| r.session_id == session_id)
    {
        return Err(CommandError::new(
            "gif_session_busy",
            "录制进行中，请先停止或丢弃",
            true,
        ));
    }
    drop(recording);
    let mut current = service
        .editor_session
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let was_current = current.as_deref() == Some(session_id);
    if was_current && !discard_current {
        return Err(CommandError::new(
            "gif_session_busy",
            "当前编辑会话需要明确确认后删除",
            true,
        ));
    }
    if discard_current && !was_current {
        return Err(CommandError::new(
            "gif_stale_session",
            "编辑会话已切换，请重新确认删除",
            true,
        ));
    }
    if crate::app::media_service::shared().has_active_job(session_id) {
        return Err(CommandError::new(
            "gif_export_busy",
            "请先完成或取消当前导出，再删除录制",
            true,
        ));
    }
    crate::app::media_service::shared().cancel_estimate(session_id);
    crate::app::gif_draft::release_if_saved(service, session_id);
    infra_gif::spool::discard_session(root, session_id).map_err(|e| {
        tracing::warn!(session_id, error = %e, "录制素材删除失败，保留当前会话供重试");
        CommandError::new("gif_session_busy", e.to_string(), true)
    })?;
    service
        .timelines
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .remove(session_id);
    crate::app::gif_draft::forget_session(service, session_id);
    if was_current {
        *current = None;
    }
    tracing::info!(session_id, "录制素材与编辑进度已删除");
    Ok(was_current)
}

/// 会话详情：元数据 + 时间轴视图 + 草稿恢复状态（0.28.23）。
#[tauri::command]
pub async fn gif_get_session(
    app: AppHandle,
    session_id: String,
) -> Result<serde_json::Value, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (reader, service) = with_session(&app, &session_id)?;
        let restore = ensure_timeline(&service, &reader, &session_id)?;
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
    })
    .await
    .map_err(|_| CommandError::new("gif_task", "录制任务失败", true))?
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
        let scale = if sw > max_w {
            max_w as f64 / sw as f64
        } else {
            1.0
        };
        let (dw, dh) = (
            ((sw as f64 * scale).round() as u32).max(1),
            ((sh as f64 * scale).round() as u32).max(1),
        );
        if (dw as u64)
            .saturating_mul(dh as u64)
            .saturating_mul(4)
            .saturating_mul(indices.len() as u64)
            > 64 << 20
        {
            return Err(CommandError::new(
                "gif_frame_budget",
                "单次预览解码超过 64MiB，请缩小批次",
                false,
            ));
        }
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
pub async fn gif_edit(
    app: AppHandle,
    session_id: String,
    command: serde_json::Value,
    incremental: Option<bool>,
    revision: Option<u64>,
) -> Result<serde_json::Value, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
    let (reader, service) = with_session(&app, &session_id)?;
    let _ = ensure_timeline(&service, &reader, &session_id)?;
    let mut timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
    let tl = timelines
        .get_mut(&session_id)
        .ok_or_else(|| CommandError::new("gif_session", "时间轴构建失败", false))?;
    if revision.is_some_and(|r| r != tl.revision()) {
        return Err(CommandError::new("gif_revision", "时间轴已变化，请刷新后再编辑", true));
    }
    if command.get("frames").and_then(|v|v.as_array()).is_some_and(|v|v.len()>4096) {
        return Err(CommandError::new("gif_edit", "批量编辑请使用帧范围", false));
    }
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
            "invalidatedFrom": invalidated_from, "canUndo": tl.can_undo(), "canRedo": tl.can_redo(), "effectiveCount": tl.effective_count(), "speed": tl.speed(), "loopCount": tl.loop_count(), "mix": tl.mix()})
    } else {
        gif_service::timeline_view(tl)
    };
    drop(timelines);
    crate::app::gif_draft::schedule_draft_save(&app, &session_id);
    Ok(view)
    }).await.map_err(|_|CommandError::new("gif_task","录制任务失败",true))?
}

/// 分页帧源契约：revision 对账后才返回页，不依赖前端全量帧表；增量编辑返回失效起点。
#[tauri::command]
pub async fn gif_timeline_page(
    app: AppHandle,
    session_id: String,
    offset: u32,
    limit: u32,
    revision: Option<u64>,
) -> Result<serde_json::Value, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let (reader, service) = with_session(&app, &session_id)?;
        ensure_timeline(&service, &reader, &session_id)?;
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
        let frames: Vec<_> = tl
            .frame_page(offset as usize, limit.clamp(1, 256) as usize)
            .iter()
            .map(|f| serde_json::json!({"srcIdx": f.src_idx, "durMs": f.dur_ms, "held": f.held}))
            .collect();
        Ok(
            serde_json::json!({"revision": tl.revision(), "offset": offset, "frames": frames,
        "totalMs": tl.effective_total_ms(), "mix": tl.mix()}),
        )
    })
    .await
    .map_err(|_| CommandError::new("gif_task", "录制任务失败", true))?
}

/// Bounded source/play/seek query, tied to the exact edit revision.
#[tauri::command]
pub async fn gif_timeline_window(
    app: AppHandle,
    session_id: String,
    revision: u64,
    play: Option<u32>,
    time_ms: Option<u64>,
    source: Option<u32>,
    source_offset: Option<u32>,
    selection: Option<Vec<(u32, u32)>>,
) -> Result<serde_json::Value, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let service = app.state::<Arc<GifService>>();
        let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        let tl = timelines
            .get(&session_id)
            .ok_or_else(|| CommandError::new("gif_session", "请先打开会话", true))?;
        if tl.revision() != revision {
            return Err(CommandError::new("gif_revision", "时间轴已变化", true));
        }
        Ok(if let Some(ranges) = selection {
            if ranges.len() > 256 || ranges.iter().any(|&(a, b)| a > b || b >= tl.frame_count()) {
                return Err(CommandError::new(
                    "gif_selection",
                    "选区无效或超过 256 个区间",
                    false,
                ));
            }
            tl.selection_summary(&ranges)
        } else if let Some(offset) = source_offset {
            tl.source_page(offset, 256)
        } else {
            tl.window(play.map(|v| v as usize), time_ms, source)
        })
    })
    .await
    .map_err(|_| CommandError::new("gif_timeline", "时间轴查询失败", true))?
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

#[derive(Default, Serialize)]
pub struct GifClearHistoryResult {
    deleted: u64,
    retained: u64,
}

/// 只清历史临时素材；当前会话及持使用租约的录制/导出始终保留。
#[tauri::command]
pub async fn gif_clear_history(
    app: AppHandle,
    window: tauri::WebviewWindow,
    preserve_session_id: Option<String>,
) -> Result<GifClearHistoryResult, CommandError> {
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "此操作仅用于录制编辑器",
            false,
        ));
    }
    let service = app.state::<Arc<GifService>>().inner().clone();
    tauri::async_runtime::spawn_blocking(move || {
        clear_history_at(
            &service,
            &GifService::root(),
            preserve_session_id.as_deref(),
        )
    })
    .await
    .map_err(|_| CommandError::new("gif_clear_history", "历史草稿清理任务失败", true))?
}

fn clear_history_at(
    service: &Arc<GifService>,
    root: &std::path::Path,
    preserve_session_id: Option<&str>,
) -> Result<GifClearHistoryResult, CommandError> {
    if preserve_session_id.is_some_and(|id| !infra_gif::spool::is_valid_session_id(id)) {
        return Err(CommandError::new("gif_session", "保留会话标识无效", false));
    }
    let recording = service
        .recording
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .map(|r| r.session_id.clone());
    let current = service
        .editor_session
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clone();
    let media = crate::app::media_service::shared();
    let exporting = media
        .snapshot(None, None)
        .filter(|job| media.has_active_job(&job.session_id))
        .map(|job| job.session_id);
    let protected: Vec<_> = [
        recording,
        current,
        exporting,
        preserve_session_id.map(str::to_owned),
    ]
    .into_iter()
    .flatten()
    .collect();
    let entries = match std::fs::read_dir(root.join("sessions")) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(GifClearHistoryResult::default());
        }
        Err(error) => return Err(io_err(error)),
    };
    // Freeze candidates before deleting, so newly created sessions are not added
    // to a running clear. Per-session guards recheck current ownership at deletion.
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(io_err)?;
        let Some(id) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        if infra_gif::spool::is_valid_session_id(&id) && entry.path().join("session.json").is_file()
        {
            ids.push(id);
        }
    }
    Ok(clear_history_candidates(service, root, ids, &protected))
}

fn clear_history_candidates(
    service: &Arc<GifService>,
    root: &std::path::Path,
    ids: Vec<String>,
    protected: &[String],
) -> GifClearHistoryResult {
    let mut result = GifClearHistoryResult::default();
    for id in ids {
        // Keep both the original active sessions and any session adopted since
        // the snapshot; closing/switching the editor cannot turn either into trash.
        if protected.contains(&id) {
            result.retained += 1;
            continue;
        }
        // No global media-task lease: unrelated exports continue. Their source
        // readers, active writers and unsaved edits retain their own usage lease.
        match discard_session_at(service, root, &id, false) {
            Ok(_) => result.deleted += 1,
            Err(_) => result.retained += 1,
        }
    }
    tracing::info!(
        deleted = result.deleted,
        retained = result.retained,
        "录制历史草稿清理完成"
    );
    result
}

/// 会话缩略图（首帧等比缩放 PNG base64；列表可见项按需取，不预载素材）。
#[tauri::command]
pub async fn gif_session_thumbnail(
    app: AppHandle,
    session_id: String,
) -> Result<GifFramePng, CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let service = app.state::<Arc<GifService>>();
        let reader = open_material(&service, &GifService::root(), &session_id, true)?;
        if reader.frame_count() == 0 {
            return Err(CommandError::new("gif_frame", "会话没有可显示的帧", false));
        }
        let (sw, sh) = (reader.meta.region.w, reader.meta.region.h);
        let max_w = 200u32;
        let scale = if sw > max_w {
            max_w as f64 / sw as f64
        } else {
            1.0
        };
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
pub async fn gif_open_session(app: AppHandle, session_id: String) -> Result<(), CommandError> {
    tauri::async_runtime::spawn_blocking(move || {
        let service = app.state::<Arc<GifService>>();
        {
            let current = service
                .editor_session
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            if let Some(cur) = current.as_deref().filter(|c| *c != session_id.as_str()) {
                if crate::app::media_service::shared().has_active_job(cur) {
                    return Err(CommandError::new(
                        "gif_export_busy",
                        "当前录制正在导出，请先完成或取消导出再切换",
                        true,
                    ));
                }
            }
        }
        // 保留读租约到切换完成，避免校验后被 TTL/列表删除抢先清理。
        let _reader = open_material(&service, &GifService::root(), &session_id, false)?;
        if !gif_service::open_editor(&app, &session_id) {
            return Err(CommandError::new(
                "gif_export_busy",
                "当前录制正在导出，请先完成或取消导出再切换",
                true,
            ));
        }
        Ok(())
    })
    .await
    .map_err(|_| CommandError::new("gif_session", "打开录制任务失败", true))?
}

/// 统一功能目录入口：有编辑会话原地激活，无会话进入「最近录制」首页。
#[tauri::command]
pub async fn gif_open_editor_entry(app: AppHandle) -> Result<(), CommandError> {
    // WebView2 首建需要主循环继续调度，不能在同步命令的主线程等待 build。
    tauri::async_runtime::spawn_blocking(move || gif_service::open_editor_entry(&app))
        .await
        .map_err(|e| CommandError::new("gif_editor", format!("打开录制编辑器任务失败: {e}"), true))
}

/// 录制详情面板「最近录制」入口（0.28.25）：总是进入编辑器「最近录制」首页。
///
/// 当前编辑会话有在途导出时拒绝（与 `gif_open_session` 一致，不静默取消），
/// 前端捕获 `gif_export_busy` 后提示。
#[tauri::command]
pub async fn gif_open_editor_home(app: AppHandle) -> Result<(), CommandError> {
    run_overlay_command(move || {
        tracing::info!("从录制详情面板打开最近录制首页");
        gif_service::open_editor_home(&app)
    })
    .await
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gif_clear_history_deletes_all_pages_and_preserves_current_and_leased_material() {
        let root = tempfile::tempdir().unwrap();
        let service = Arc::new(GifService::new());
        let create = |id: &str| {
            let meta = infra_gif::SessionMeta {
                spool_version: infra_gif::SPOOL_VERSION,
                id: id.into(),
                region: infra_gif::CaptureRegion {
                    x: 0,
                    y: 0,
                    w: 8,
                    h: 8,
                },
                fps: 10,
                show_cursor: false,
                created_at: 42,
                state: "recording".into(),
                ts_ms: vec![],
                last_dur_ms: 100,
                spool_bytes: 0,
                stop_reason: None,
            };
            let mut writer = infra_gif::SpoolWriter::create(root.path(), meta).unwrap();
            writer.append_frame(0, &[1; 256]).unwrap();
            writer
        };
        for i in 0..26 {
            create(&format!("history-{i}")).finalize("user").unwrap();
        }
        for id in ["current", "exporting"] {
            create(id).finalize("user").unwrap();
        }
        *service.editor_session.lock().unwrap() = Some("current".into());
        let exporting = SpoolReader::open(root.path(), "exporting").unwrap();
        let recording = create("recording");
        let exported_file = root.path().join("keep.mp4");
        std::fs::write(&exported_file, b"user export").unwrap();
        std::fs::create_dir(root.path().join("sessions/partial")).unwrap();
        let result = clear_history_at(&service, root.path(), None).unwrap();
        assert_eq!((result.deleted, result.retained), (26, 3));
        for id in ["current", "exporting", "recording", "partial"] {
            assert!(root.path().join("sessions").join(id).exists(), "{id}");
        }
        assert_eq!(std::fs::read(&exported_file).unwrap(), b"user export");
        assert_eq!(exporting.read_frame(0).unwrap(), vec![1; 256]);
        assert_eq!(
            service.editor_session.lock().unwrap().as_deref(),
            Some("current")
        );
        drop(exporting);
        recording.finalize("user").unwrap();
        let retry = clear_history_at(&service, root.path(), Some("exporting")).unwrap();
        assert_eq!((retry.deleted, retry.retained), (1, 2));
    }

    #[test]
    fn gif_clear_history_distinguishes_missing_storage_from_enumeration_failure() {
        let root = tempfile::tempdir().unwrap();
        let service = Arc::new(GifService::new());
        let empty = clear_history_at(&service, root.path(), None).unwrap();
        assert_eq!((empty.deleted, empty.retained), (0, 0));
        std::fs::write(root.path().join("sessions"), b"not a directory").unwrap();
        assert!(clear_history_at(&service, root.path(), None).is_err());
    }

    #[test]
    fn gif_clear_history_keeps_the_original_editor_after_it_switches_or_closes() {
        let root = tempfile::tempdir().unwrap();
        let service = Arc::new(GifService::new());
        for current in [None, Some("replacement".to_string())] {
            *service.editor_session.lock().unwrap() = current;
            // These names were current/recording/exporting at clear-start. No
            // filesystem lookup/deletion may happen after their live state changes.
            let protected = vec![
                "original".to_string(),
                "recording".to_string(),
                "exporting".to_string(),
            ];
            let result =
                clear_history_candidates(&service, root.path(), protected.clone(), &protected);
            assert_eq!((result.deleted, result.retained), (0, 3));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn gif_overlay_lock_wait_keeps_dispatcher_responsive() {
        use std::sync::{Mutex, mpsc};
        use std::time::Duration;

        // 后台收尾占有会话门锁；模拟主循环必须继续调度，才能放行收尾。
        // 所有等待有界，即使误改成同步执行也会失败退出，不挂死测试进程。
        let gate = Arc::new(Mutex::new(()));
        let (held_tx, held_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let finalizing_gate = gate.clone();
        let finalizer = std::thread::spawn(move || {
            let _guard = finalizing_gate.lock().unwrap();
            held_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(3))
        });
        held_rx.recv_timeout(Duration::from_secs(3)).unwrap();
        let dispatcher_thread = std::thread::current().id();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let command = run_overlay_command(move || {
            let _ = entered_tx.send(std::thread::current().id());
            let _guard = gate.lock().unwrap();
            Ok(37)
        });
        let dispatch = async move {
            let worker = tokio::time::timeout(Duration::from_secs(1), entered_rx)
                .await
                .unwrap()
                .unwrap();
            assert_ne!(worker, dispatcher_thread);
            release_tx.send(()).unwrap();
        };
        let (result, ()) = tokio::join!(command, dispatch);
        assert_eq!(result.unwrap(), 37);
        assert!(finalizer.join().unwrap().is_ok());

        // 后台业务错误仍需透传，不能当作成功或丢失 Promise 的结束状态。
        assert!(
            run_overlay_command::<()>(|| Err(GifServiceError::NoActiveRecording))
                .await
                .is_err()
        );
    }

    #[test]
    fn deleting_current_recording_requires_consent_and_preserves_current_on_lease_failure() {
        let root = tempfile::tempdir().unwrap();
        let service = Arc::new(GifService::new());
        let id = "delete-current";
        let meta = infra_gif::SessionMeta {
            spool_version: infra_gif::SPOOL_VERSION,
            id: id.into(),
            region: infra_gif::CaptureRegion {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            fps: 10,
            show_cursor: false,
            created_at: 42,
            state: "recording".into(),
            ts_ms: vec![],
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        let mut writer = infra_gif::SpoolWriter::create(root.path(), meta).unwrap();
        writer.append_frame(0, &[0; 256]).unwrap();
        writer.finalize("user").unwrap();
        *service.editor_session.lock().unwrap() = Some(id.into());
        let reader = SpoolReader::open(root.path(), id).unwrap();
        assert!(discard_session_at(&service, root.path(), id, false).is_err());
        assert!(discard_session_at(&service, root.path(), id, true).is_err());
        assert_eq!(service.editor_session.lock().unwrap().as_deref(), Some(id));
        assert!(root.path().join("sessions").join(id).exists());
        assert!(discard_session_at(&service, root.path(), "another", true).is_err());
        drop(reader);
        assert!(discard_session_at(&service, root.path(), id, true).unwrap());
        assert!(service.editor_session.lock().unwrap().is_none());
        assert!(!root.path().join("sessions").join(id).exists());
    }

    #[test]
    fn history_material_opens_without_adopting_the_current_editor_session() {
        let root = tempfile::tempdir().unwrap();
        let service = GifService::new();
        let id = "history-material";
        let meta = infra_gif::SessionMeta {
            spool_version: infra_gif::SPOOL_VERSION,
            id: id.into(),
            region: infra_gif::CaptureRegion {
                x: 0,
                y: 0,
                w: 8,
                h: 8,
            },
            fps: 10,
            show_cursor: false,
            created_at: 42,
            state: "recording".into(),
            ts_ms: Vec::new(),
            last_dur_ms: 100,
            spool_bytes: 0,
            stop_reason: None,
        };
        let mut writer = infra_gif::SpoolWriter::create(root.path(), meta).unwrap();
        writer.append_frame(0, &[0; 256]).unwrap();
        writer.append_frame(100, &[255; 256]).unwrap();
        writer.finalize("user").unwrap();

        for current in [None, Some("another-session".to_string())] {
            *service.editor_session.lock().unwrap() = current.clone();
            let reader = open_material(&service, root.path(), id, false).unwrap();
            assert_eq!(reader.frame_count(), 2);
            assert_eq!(reader.read_frame(1).unwrap(), vec![255; 256]);
            let thumbnail = open_material(&service, root.path(), id, true).unwrap();
            assert_eq!(thumbnail.frame_count(), 1);
            assert_eq!(thumbnail.read_frame(0).unwrap(), vec![0; 256]);
            assert_eq!(*service.editor_session.lock().unwrap(), current);
            assert!(service.readers.lock().unwrap().is_empty());
            assert!(infra_gif::spool::discard_session(root.path(), id).is_err());
        }
        assert!(open_material(&service, root.path(), "../escape", false).is_err());
        assert!(open_material(&service, root.path(), "missing", true).is_err());
        infra_gif::spool::discard_session(root.path(), id).unwrap();
    }
}
