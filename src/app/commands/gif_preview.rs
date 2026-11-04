use super::gif::with_session;
use crate::app::command_error::CommandError;
use tauri::{AppHandle, WebviewWindow, ipc::Response};

static DECODE_SLOTS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// 单帧二进制预览；全局并发有界，素材租约覆盖读取和缩放。
#[tauri::command]
pub async fn gif_get_preview_frame(
    app: AppHandle,
    window: WebviewWindow,
    session_id: String,
    index: u32,
    max_w: u32,
) -> Result<Response, CommandError> {
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "此操作仅用于录制编辑器",
            false,
        ));
    }
    let permit = DECODE_SLOTS
        .try_acquire()
        .map_err(|_| CommandError::new("gif_preview_busy", "预览解码繁忙", true))?;
    tauri::async_runtime::spawn_blocking(move || {
        let _permit = permit;
        let (reader, _service) = with_session(&app, &session_id)?;
        let bgra = reader
            .read_frame(index)
            .map_err(|e| CommandError::new("gif_frame", e.to_string(), false))?;
        let region = &reader.meta.region;
        let bytes = blink_infra::gif::preview::frame(&bgra, region.w, region.h, max_w)
            .map_err(|e| CommandError::new("gif_preview", e, false))?;
        Ok(Response::new(bytes))
    })
    .await
    .map_err(|e| CommandError::new("gif_preview", format!("预览解码任务失败: {e}"), true))?
}
