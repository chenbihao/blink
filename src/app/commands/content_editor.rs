//! 通用内容编辑器 IPC 入口（0.23.1 重构）。
//!
//! 会话编排归 `crate::app::editor::EditorSessionService`；本文件只做 IPC
//! 形态适配：注入调用窗口 label、调用服务、把 `EditorError` 投影为
//! `CommandError`（前端按 code 分类展示，不解析中文 message）。
//!
//! 四个命令（phase 文档 §3.9 最小集合的 0.23.1 子集）：
//! - `open_content_editor(request)`：绑定/激活同源会话或返回 `editor_busy`
//! - `get_content_editor_session()`：前端绑定时拉取只读快照
//! - `commit_content_editor(request)`：按来源推导的保存目标提交正文
//! - `end_content_editor(request)`：结束会话并释放便签租约
//!
//! `get/commit/end` 校验调用窗口必须是 `content-editor`；`open` 任何窗口可发起。

use tauri::Manager;

use crate::app::command_error::CommandError;
use crate::app::editor::EditorSessionService;
use crate::domain::editor::{
    CommitEditorRequest, CommitOutcome, EditorError, EditorSessionSnapshot, EndEditorRequest,
    OpenEditorRequest,
};

impl From<EditorError> for CommandError {
    fn from(e: EditorError) -> Self {
        let (code, retryable) = match &e {
            EditorError::EditorBusy { .. } => ("editor_busy", false),
            EditorError::StaleSession => ("stale_session", false),
            EditorError::StaleRevision => ("stale_revision", false),
            EditorError::SourceConflict { .. } => ("source_conflict", true),
            EditorError::TargetUnavailable { .. } => ("target_unavailable", true),
            EditorError::Unsupported { .. } => ("unsupported", false),
            EditorError::Io { .. } => ("io", true),
        };
        let detail = serde_json::to_value(&e).ok();
        match detail {
            Some(detail) => CommandError::with_detail(code, e.to_string(), retryable, detail),
            None => CommandError::new(code, e.to_string(), retryable),
        }
    }
}

fn service(app: &tauri::AppHandle) -> Result<std::sync::Arc<EditorSessionService>, CommandError> {
    Ok(app
        .try_state::<std::sync::Arc<EditorSessionService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("io", "编辑器会话服务不可用", false))?)
}

/// 打开内容编辑器：无活动会话则绑定并显示；同一持久来源激活现有窗口；
/// 不同来源返回 `editor_busy` 并激活现有任务，正文绝不覆盖。
#[tauri::command]
pub async fn open_content_editor(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: OpenEditorRequest,
) -> Result<EditorSessionSnapshot, CommandError> {
    tracing::info!(
        caller = %window.label(),
        source_kind = ?request.source,
        body_len = request.body.len(),
        "open_content_editor"
    );
    let service = service(&app)?;
    let snapshot = service.open(request).await?;
    Ok(snapshot)
}

/// 前端绑定时拉取只读会话快照；无活动会话返回 null。
/// 只接受 `content-editor` 窗口调用。
#[tauri::command]
pub async fn get_content_editor_session(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<Option<EditorSessionSnapshot>, CommandError> {
    let service = service(&app)?;
    Ok(service.current_snapshot(window.label())?)
}

/// 提交正文：按来源推导的保存目标执行副作用，返回新基线。
/// 只接受 `content-editor` 窗口调用；校验 session_ref + generation。
#[tauri::command]
pub async fn commit_content_editor(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: CommitEditorRequest,
) -> Result<CommitOutcome, CommandError> {
    tracing::info!(
        caller = %window.label(),
        session_ref = %request.session_ref,
        generation = request.generation,
        revision = request.revision,
        body_len = request.body.len(),
        "commit_content_editor"
    );
    let service = service(&app)?;
    Ok(service.commit(window.label(), request).await?)
}

/// 结束会话：释放便签租约、清空活动会话；窗口 reset 由前端执行。
/// 只接受 `content-editor` 窗口调用。
#[tauri::command]
pub async fn end_content_editor(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: EndEditorRequest,
) -> Result<(), CommandError> {
    let service = service(&app)?;
    let label = window.label().to_string();
    // end 是纯内存操作（清会话槽 + 发事件），同步完成。
    service.end(&label, request)?;
    Ok(())
}
