//! 便签 IPC 命令层（0.16.7）。
//!
//! 架构定位：app 层编排，组合 StickyService（domain）和窗口管理（infra）。
//! 命令保持轻量——只做参数映射和事件 emit，业务逻辑在 domain。
//!
//! **日志隐私**（§5.8）：事件/command 只传 sticky id 与必要字段，不在日志记录正文。

use tauri::{AppHandle, Emitter, Manager};

use crate::domain::sticky::{StickyColor, StickyFormat, StickyService, StickyError};
use crate::domain::event_names::EventNames;

/// 创建便签。
///
/// 返回创建的 StickyNote（含 id）。前端拿到 id 后打开便签窗口。
/// 事件：emit `blink://sticky-created` `{ stickyId }`。
#[tauri::command]
pub async fn create_sticky_note(
    app: AppHandle,
    content: Option<String>,
    color: Option<String>,
) -> Result<crate::domain::sticky::StickyNote, String> {
    let svc = app
        .state::<std::sync::Arc<StickyService>>()
        .inner()
        .clone();

    let c = color
        .as_deref()
        .map(StickyColor::from_str)
        .unwrap_or_default();

    let note = svc
        .create_note(content.as_deref().unwrap_or(""), c)
        .await
        .map_err(|e: StickyError| e.to_string())?;

    // emit 事件（不传正文）
    let _ = app.emit(
        EventNames::STICKY_CREATED,
        serde_json::json!({ "stickyId": note.id }),
    );

    Ok(note)
}

/// 获取单条便签。
#[tauri::command]
pub async fn get_sticky_note(
    app: AppHandle,
    id: String,
) -> Option<crate::domain::sticky::StickyNote> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.get_note(&id).await
}

/// 列出全部便签。
#[tauri::command]
pub async fn list_sticky_notes(
    app: AppHandle,
) -> Vec<crate::domain::sticky::StickyNote> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.list_notes().await
}

/// 更新便签内容（前端防抖后调用）。
///
/// emit `STICKY_CONTENT_CHANGED` 让管理界面和其他便签窗口感知内容变更。
#[tauri::command]
pub async fn update_sticky_content(
    app: AppHandle,
    id: String,
    content: String,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.update_content_debounced(&id, &content)
        .await
        .map_err(|e: StickyError| e.to_string())?;
    let _ = app.emit(
        EventNames::STICKY_CONTENT_CHANGED,
        serde_json::json!({ "stickyId": id }),
    );
    Ok(())
}

/// 更新便签外观（颜色 + 可选格式）。
#[tauri::command]
pub async fn update_sticky_appearance(
    app: AppHandle,
    id: String,
    color: String,
    format: Option<String>,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    let c = StickyColor::from_str(&color);
    let color_str = c.as_str().to_string();
    let f = format.map(|s| StickyFormat::from_str(&s));
    svc.update_appearance(&id, c, f)
        .await
        .map_err(|e: StickyError| e.to_string())?;

    let _ = app.emit(
        EventNames::STICKY_APPEARANCE_CHANGED,
        serde_json::json!({ "stickyId": id, "color": color_str }),
    );

    Ok(())
}

/// 更新便签窗口几何。
#[tauri::command]
pub async fn update_sticky_geometry(
    app: AppHandle,
    id: String,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.update_geometry(&id, x, y, width, height)
        .await
        .map_err(|e: StickyError| e.to_string())
}

/// 设置便签可见性。
#[tauri::command]
pub async fn set_sticky_visible(
    app: AppHandle,
    id: String,
    visible: bool,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.set_visible(&id, visible)
        .await
        .map_err(|e: StickyError| e.to_string())?;

    let _ = app.emit(
        EventNames::STICKY_VISIBILITY_CHANGED,
        serde_json::json!({ "stickyId": id, "visible": visible }),
    );

    Ok(())
}

/// 设置便签置顶。
#[tauri::command]
pub async fn set_sticky_always_on_top(
    app: AppHandle,
    id: String,
    always_on_top: bool,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.set_always_on_top(&id, always_on_top)
        .await
        .map_err(|e: StickyError| e.to_string())
}

/// 删除便签（永久）。
#[tauri::command]
pub async fn delete_sticky_note(
    app: AppHandle,
    id: String,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.delete_note(&id)
        .await
        .map_err(|e: StickyError| e.to_string())?;

    let _ = app.emit(
        EventNames::STICKY_DELETED,
        serde_json::json!({ "stickyId": id }),
    );

    Ok(())
}

/// 获取便签统计。
#[tauri::command]
pub async fn get_sticky_stats(app: AppHandle) -> serde_json::Value {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    svc.get_stats().await
}

/// 显示便签窗口（0.16.8）。
///
/// 前端创建便签后调用此命令打开桌面窗口。
/// 后端根据 sticky_id 从 DB 读取位置/尺寸/置顶状态，恢复窗口。
#[tauri::command]
pub async fn show_sticky_window_cmd(
    app: AppHandle,
    sticky_id: String,
) -> Result<(), String> {
    let svc = app.state::<std::sync::Arc<StickyService>>();
    let note = svc
        .get_note(&sticky_id)
        .await
        .ok_or_else(|| format!("便签不存在: {sticky_id}"))?;

    // 设置 visible=true
    svc.set_visible(&sticky_id, true)
        .await
        .map_err(|e: StickyError| e.to_string())?;
    let _ = app.emit(
        EventNames::STICKY_VISIBILITY_CHANGED,
        serde_json::json!({ "stickyId": sticky_id, "visible": true }),
    );

    // 创建/显示窗口（用户主动操作，需要聚焦）
    crate::infra::platform::window::show_sticky_window(
        &app,
        &sticky_id,
        note.x,
        note.y,
        note.width,
        note.height,
        note.always_on_top,
        true, // 0.16.11：用户操作需要聚焦
    )?;

    Ok(())
}

/// 销毁便签窗口（删除数据后调用）。
#[tauri::command]
pub async fn destroy_sticky_window_cmd(
    app: AppHandle,
    sticky_id: String,
) -> Result<(), String> {
    crate::infra::platform::window::destroy_sticky_window(&app, &sticky_id);
    Ok(())
}

/// 显示便签管理窗口（0.16.10）。
///
/// 独立窗口，列出所有便签，支持隐藏/编辑/改色/删除。
#[tauri::command]
pub async fn show_sticky_manager_cmd(app: AppHandle) -> Result<(), String> {
    crate::infra::platform::window::show_sticky_manager_window(&app)
}
