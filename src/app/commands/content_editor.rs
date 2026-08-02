//! 通用内容编辑器 IPC 入口（0.16.3）。
//!
//! 架构归属（定案）：编辑逻辑在前端，后端只做窗口创建 + 剪贴板读写桥接。
//! 不建 domain/content_editor 模块——编辑器是应用层窗口编排，无独立业务域逻辑。
//!
//! 三个命令：
//! - `open_content_editor(payload)`：存储 payload + 创建/显示编辑器窗口
//! - `get_content_editor_payload()`：前端 init 时拉取 payload（避免事件竞态）
//! - `save_content_editor(body, origin_ref)`：保存为新剪贴板记录 + 写回系统剪贴板

use std::sync::Mutex;

use tauri::Manager;

/// 待编辑内容 payload（前端 → 后端 → 前端，经 Tauri State 中转避免事件竞态）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EditableContentPayload {
    /// 文本内容
    pub body: String,
    /// 格式："plain" | "markdown"（0.16 仅 "plain"）
    #[serde(default = "default_format")]
    pub format: String,
    /// 窗口标题（默认"编辑剪贴板内容"）
    pub title: Option<String>,
    /// 来源："clipboard"（0.16 唯一来源）
    pub origin: String,
    /// 原剪贴板记录 id（用于继承 hit_count）
    pub origin_ref: Option<String>,
    /// 保存策略："clipboard_new"（0.16 唯一策略）
    #[serde(default = "default_save_policy")]
    pub save_policy: String,
}

fn default_format() -> String {
    "plain".to_string()
}

fn default_save_policy() -> String {
    "clipboard_new".to_string()
}

/// 待编辑 payload 暂存——open_content_editor 存入，get_content_editor_payload 取出并清空。
pub struct PendingEditorPayload(pub Mutex<Option<EditableContentPayload>>);

impl Default for PendingEditorPayload {
    fn default() -> Self {
        Self(Mutex::new(None))
    }
}

/// 打开内容编辑器窗口。
///
/// payload 经 Tauri State 中转：存入 `PendingEditorPayload`，前端 init 时调
/// `get_content_editor_payload` 拉取。这比事件 emit 更可靠——新建窗口 JS
/// init 有延迟，emit 可能在 listener 注册前发出。
#[tauri::command]
pub async fn open_content_editor(
    app: tauri::AppHandle,
    payload: EditableContentPayload,
) -> Result<(), String> {
    tracing::info!(
        origin = %payload.origin,
        has_origin_ref = payload.origin_ref.is_some(),
        body_len = payload.body.len(),
        "open_content_editor"
    );

    // 存储 payload 供前端拉取
    let pending = app.state::<PendingEditorPayload>();
    *pending.0.lock().map_err(|e| format!("锁失败: {e}"))? = Some(payload);

    // 创建/显示编辑器窗口
    crate::infra::platform::window::show_content_editor_window(&app)?;

    Ok(())
}

/// 前端 init 时拉取待编辑 payload（取出后清空）。
#[tauri::command]
pub async fn get_content_editor_payload(
    app: tauri::AppHandle,
) -> Option<EditableContentPayload> {
    let pending = app.state::<PendingEditorPayload>();
    pending.0.lock().ok().and_then(|mut guard| guard.take())
}

/// 保存编辑后的内容（savePolicy=clipboard_new）。
///
/// 链路：
/// 1. 新建一条 ClipboardItem（新 id、新 created_at），hit_count 从 originRef 继承
/// 2. 写入 clipboard_history 表（save_item，INSERT OR REPLACE）
/// 3. 写回系统剪贴板
/// 4. 原项不删除、不覆盖——保留可恢复路径
/// 5. 返回新记录 id
#[tauri::command]
pub async fn save_content_editor(
    app: tauri::AppHandle,
    body: String,
    origin_ref: Option<String>,
) -> Result<String, String> {
    tracing::info!(
        has_origin_ref = origin_ref.is_some(),
        body_len = body.len(),
        "save_content_editor"
    );

    let pool = &app.state::<crate::infra::data::DbPools>().history;

    // 1. 查询原项 hit_count（如有 originRef）
    let hit_count = if let Some(ref id) = origin_ref {
        match crate::infra::data::clipboard::query_by_id(pool, id).await {
            Some(item) => item.hit_count,
            None => {
                tracing::warn!(origin_ref = %id, "原剪贴板记录不存在，hit_count 从 0 开始");
                0
            }
        }
    } else {
        0
    };

    // 2. 新建 ClipboardItem
    let new_item = crate::infra::data::clipboard::ClipboardItem {
        id: crate::infra::data::clipboard::generate_id(),
        text: body.clone(),
        preview: crate::infra::data::clipboard::make_preview(&body),
        created_at: chrono::Utc::now().timestamp(),
        source_app: None,
        hit_count,
    };

    // 3. 写入数据库
    crate::infra::data::clipboard::save_item(pool, &new_item)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "保存剪贴板记录失败");
            format!("保存失败: {e}")
        })?;

    // 4. 写回系统剪贴板（复用 copy_to_clipboard 命令的 Win32 逻辑）
    crate::app::commands::copy_to_clipboard(body)
        .await
        .map_err(|e| {
            tracing::error!(error = %e, "写回系统剪贴板失败");
            format!("写回剪贴板失败: {e}")
        })?;

    tracing::info!(new_id = %new_item.id, "save_content_editor 完成");
    Ok(new_item.id)
}
