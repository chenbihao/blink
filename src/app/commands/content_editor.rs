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
    OpenEditorRequest, ResolveEditorExitRequest,
};

impl From<EditorError> for CommandError {
    fn from(e: EditorError) -> Self {
        let (code, retryable) = match &e {
            EditorError::EditorBusy { .. } => ("editor_busy", false),
            EditorError::VoiceBusy => ("voice_busy", false),
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

/// 前端应答退出确认（§3.5 主动退出一次汇总确认的应答侧）。
/// 只有与待决请求 id 匹配的应答生效；迟到的旧应答静默忽略。
#[tauri::command]
pub fn resolve_editor_exit(
    app: tauri::AppHandle,
    request: ResolveEditorExitRequest,
) -> Result<(), CommandError> {
    let service = service(&app)?;
    service.resolve_editor_exit(&request.request_id, request.confirmed);
    Ok(())
}

// ── 编辑器连续听写（0.23.3 §3.6 / §3.9）─────────────────────────────────

/// 听写控制命令允许的调用窗口：编辑器窗口本体 + 语音浮窗（暂停/继续/结束按钮）。
/// `start` 只允许编辑器窗口（听写由编辑器会话显式发起并冻结身份）。
const VOICE_CONTROL_CALLERS: &[&str] = &[crate::app::editor::CONTENT_EDITOR_LABEL, "voice-overlay"];

/// start_editor_voice 请求：冻结的编辑器会话身份。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartEditorVoiceRequest {
    pub session_ref: String,
    pub generation: u64,
}

/// start_editor_voice 结果：本次听写 epoch（前端按 epoch + seq 去重/补齐）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorVoiceStartResult {
    pub epoch: u64,
}

/// 快照段（补齐缺号用）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorVoiceSegmentDto {
    pub seq: u64,
    pub text: String,
}

/// 有界 confirmed 快照（§3.6：缺号、窗口恢复或重新聚焦时拉取补齐）。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorVoiceSnapshotDto {
    pub epoch: u64,
    pub segments: Vec<EditorVoiceSegmentDto>,
    /// 因缓冲满被淘汰的最旧段数（前端据此感知无法补齐的区间）。
    pub truncated: usize,
}

fn voice_service(
    app: &tauri::AppHandle,
) -> Result<std::sync::Arc<crate::app::voice::VoiceService>, CommandError> {
    Ok(app
        .try_state::<std::sync::Arc<crate::app::voice::VoiceService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("io", "语音服务不可用", false))?)
}

/// 校验调用窗口属于听写控制白名单（编辑器 + 浮窗）。
fn ensure_voice_control_caller(label: &str) -> Result<(), CommandError> {
    if VOICE_CONTROL_CALLERS.contains(&label) {
        Ok(())
    } else {
        tracing::warn!(caller = %label, "编辑器听写控制: 非白名单窗口调用被拒绝");
        Err(CommandError::new(
            "unsupported",
            "该命令只能由 content-editor 或 voice-overlay 窗口调用",
            false,
        ))
    }
}

/// 开始编辑器连续听写：校验编辑器会话身份 → 启动 Editor VoiceSession。
///
/// G1/G2/G3/Editor 互斥：已有任意录音时返回 `voice_busy`（§6.4）。
/// 成功返回本次听写 epoch，后续段/状态事件携带同一 epoch。
#[tauri::command]
pub async fn start_editor_voice(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: StartEditorVoiceRequest,
) -> Result<EditorVoiceStartResult, CommandError> {
    let label = window.label().to_string();
    ensure_editor_caller(&label)?;
    tracing::info!(
        session_ref = %request.session_ref,
        generation = request.generation,
        "start_editor_voice"
    );

    let editor = service(&app)?;
    if !editor.verify_active_session(&request.session_ref, request.generation) {
        return Err(CommandError::from(EditorError::StaleSession));
    }

    let voice = voice_service(&app)?;
    match voice
        .start_editor_recording(request.session_ref, request.generation)
        .await
    {
        crate::app::voice::EditorVoiceStart::Started(epoch) => Ok(EditorVoiceStartResult { epoch }),
        crate::app::voice::EditorVoiceStart::Disabled => Err(CommandError::new(
            "stt_disabled",
            "语音输入未启用，请在设置中开启",
            false,
        )),
        crate::app::voice::EditorVoiceStart::Busy => {
            Err(CommandError::from(EditorError::VoiceBusy))
        }
        crate::app::voice::EditorVoiceStart::Failed => Err(CommandError::new(
            "voice_failed",
            "语音服务未就绪，请检查语音设置",
            true,
        )),
    }
}

/// 暂停编辑器听写（音频丢弃不识别，confirmed 保持）。幂等。
#[tauri::command]
pub fn pause_editor_voice(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<(), CommandError> {
    ensure_voice_control_caller(window.label())?;
    voice_service(&app)?
        .pause_editor_recording()
        .then_some(())
        .ok_or_else(|| CommandError::new("invalid_state", "未在连续听写中", false))
}

/// 继续编辑器听写。幂等。
#[tauri::command]
pub fn resume_editor_voice(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<(), CommandError> {
    ensure_voice_control_caller(window.label())?;
    voice_service(&app)?
        .resume_editor_recording()
        .then_some(())
        .ok_or_else(|| CommandError::new("invalid_state", "未在连续听写中", false))
}

/// 结束编辑器听写：confirmed 保留、preview 丢弃，返回各段已交付。
#[tauri::command]
pub async fn stop_editor_voice(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<(), CommandError> {
    ensure_voice_control_caller(window.label())?;
    voice_service(&app)?.stop_editor_recording().await;
    Ok(())
}

/// 拉取听写 confirmed 快照：返回 epoch 匹配且 seq > afterSeq 的段。
/// epoch 不匹配或无听写状态返回 null（前端提示无法补齐）。
#[tauri::command]
pub fn get_editor_voice_snapshot(
    app: tauri::AppHandle,
    window: tauri::Window,
    epoch: u64,
    after_seq: u64,
) -> Result<Option<EditorVoiceSnapshotDto>, CommandError> {
    ensure_voice_control_caller(window.label())?;
    Ok(voice_service(&app)?
        .editor_voice_snapshot(epoch, after_seq)
        .map(|(epoch, segments, truncated)| EditorVoiceSnapshotDto {
            epoch,
            truncated,
            segments: segments
                .into_iter()
                .map(|(seq, text)| EditorVoiceSegmentDto { seq, text })
                .collect(),
        }))
}

/// 唤起并聚焦编辑器窗口（voice-overlay"返回编辑器"按钮）。
#[tauri::command]
pub fn focus_content_editor(app: tauri::AppHandle) -> Result<(), CommandError> {
    crate::infra::platform::window::show_content_editor_window(&app)
        .map_err(|e| CommandError::new("io", e, true))?;
    if let Some(win) = app.get_webview_window(crate::app::editor::CONTENT_EDITOR_LABEL) {
        let _ = win.set_focus();
    }
    Ok(())
}

/// start_editor_voice 只接受编辑器窗口（与 get/commit/end 同一约束）。
fn ensure_editor_caller(label: &str) -> Result<(), CommandError> {
    if label == crate::app::editor::CONTENT_EDITOR_LABEL {
        Ok(())
    } else {
        Err(CommandError::new(
            "unsupported",
            "该命令只能由 content-editor 窗口调用",
            false,
        ))
    }
}
