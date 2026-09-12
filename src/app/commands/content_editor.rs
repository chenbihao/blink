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
            EditorError::AiAlreadyActive { .. } => ("ai_already_active", false),
            EditorError::Cancelled => ("cancelled", false),
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
    // 先取会话身份（end 会消费 request），end 后兜底清理悬空整理请求。
    let (session_ref, generation) = (request.session_ref.clone(), request.generation);
    // end 先等待进行中的 commit 副作用完成（mutation gate，§5.7 ④），再清槽。
    service.end(&label, request).await?;
    // 0.23.4：会话结束时清理悬空整理请求（防跨会话迟到事件）。
    // 前端正常路径已显式 cancel；此处兜底（幂等，不匹配时静默忽略）。
    if let Ok(transform) = editor_transform_service(&app) {
        transform
            .cancel_active_for_session(&session_ref, generation)
            .await;
    }
    if let Ok(voice) = voice_service(&app) {
        voice.release_editor_session(&session_ref, generation);
    }
    Ok(())
}
/// 前端应答退出确认（§3.5 主动退出一次汇总确认的应答侧）。
/// 只有与待决请求 id 匹配的应答生效；迟到的旧应答静默忽略。
#[tauri::command]
pub fn resolve_editor_exit(
    app: tauri::AppHandle,
    request: ResolveEditorExitRequest,
) -> Result<bool, CommandError> {
    let service = service(&app)?;
    Ok(service.resolve_editor_exit(&request.request_id, request.confirmed))
}

// ── 编辑器恢复草稿（与 Ctrl+S 完全分离的持久化通道）─────────────────────
//
// 只读写草稿文件：**绝不**触发剪贴板 / 便签 / 文件 / Capability 等保存目标
// 副作用（草稿与"发布"是两条独立链路）。全部命令只接受 `content-editor` 窗口，
// 并在前端传入会话身份时校验 `session_ref + generation`。

/// save_editor_draft 请求：来源键 + 冻结会话身份 + 单调 revision + 正文。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveEditorDraftRequest {
    pub key: String,
    #[serde(default)]
    pub session_ref: Option<String>,
    #[serde(default)]
    pub generation: u64,
    pub revision: u64,
    #[serde(default)]
    pub hash: String,
    pub body: String,
    #[serde(default)]
    pub base_digest: String,
    #[serde(default)]
    pub base_revision: Option<i64>,
    #[serde(default)]
    pub source_instance_id: String,
    #[serde(default)]
    pub schema_version: u32,
}

/// 落盘后的 revision 水位。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SaveEditorDraftResult {
    pub stored_revision: u64,
}

/// clear_editor_draft 请求：会话结束/放弃修改时清理草稿。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ClearEditorDraftRequest {
    pub key: String,
    #[serde(default)]
    pub session_ref: Option<String>,
    #[serde(default)]
    pub generation: u64,
    #[serde(default)]
    pub expected_revision: Option<u64>,
    #[serde(default)]
    pub expected_hash: Option<String>,
}

fn draft_store(
    app: &tauri::AppHandle,
) -> Result<std::sync::Arc<crate::app::editor_draft::EditorDraftStore>, CommandError> {
    Ok(app
        .try_state::<std::sync::Arc<crate::app::editor_draft::EditorDraftStore>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("io", "草稿存储不可用", false))?)
}

/// 保存恢复草稿（防抖 / 关键边界 flush）。同一会话身份下旧 revision 被拒绝。
#[tauri::command]
pub async fn save_editor_draft(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: SaveEditorDraftRequest,
) -> Result<SaveEditorDraftResult, CommandError> {
    ensure_editor_caller(window.label())?;
    if let Some(session_ref) = request.session_ref.as_deref() {
        if !service(&app)?.verify_active_session(session_ref, request.generation) {
            return Err(CommandError::from(EditorError::StaleSession));
        }
    }
    let store = draft_store(&app)?;
    let computed_hash = format!("{:016x}", crate::domain::editor::body_digest(&request.body));
    if !request.hash.is_empty() && request.hash != computed_hash {
        tracing::debug!("save_editor_draft: 纠正前端正文摘要");
    }
    let draft = crate::app::editor_draft::EditorDraft {
        key: request.key,
        session_ref: request.session_ref.unwrap_or_default(),
        generation: request.generation,
        revision: request.revision,
        // EditorDraftStore recomputes this again; retaining the field keeps
        // the IPC contract explicit while making the backend the authority.
        hash: computed_hash,
        body: request.body,
        base_digest: request.base_digest,
        base_revision: request.base_revision,
        source_instance_id: request.source_instance_id,
        schema_version: if request.schema_version == 0 {
            crate::app::editor_draft::CURRENT_EDITOR_DRAFT_SCHEMA
        } else {
            request.schema_version
        },
        orphaned: false,
        updated_at_ms: 0,
    };
    let stored_revision = store.save(draft).await?;
    Ok(SaveEditorDraftResult { stored_revision })
}

/// 读取恢复草稿（会话绑定时探测崩溃残留）。无草稿返回 null。
#[tauri::command]
pub async fn load_editor_draft(
    app: tauri::AppHandle,
    window: tauri::Window,
    key: String,
    session_ref: Option<String>,
    generation: Option<u64>,
) -> Result<Option<crate::app::editor_draft::EditorDraft>, CommandError> {
    ensure_editor_caller(window.label())?;
    if let Some(session_ref) = session_ref.as_deref() {
        if !service(&app)?.verify_active_session(session_ref, generation.unwrap_or(0)) {
            return Err(CommandError::from(EditorError::StaleSession));
        }
    }
    Ok(draft_store(&app)?.load(&key).await?)
}

/// 返回最近恢复候选，供无法自动关联临时来源的新会话显式选择。
#[tauri::command]
pub async fn list_editor_drafts(
    app: tauri::AppHandle,
    window: tauri::Window,
) -> Result<Vec<crate::app::editor_draft::EditorDraft>, CommandError> {
    ensure_editor_caller(window.label())?;
    Ok(draft_store(&app)?.list().await?)
}

/// 清理恢复草稿（成功提交 / 放弃修改 / 会话结束）。幂等。
/// 会话可能已经结束，故不强制校验会话身份，仍只允许编辑器窗口调用。
#[tauri::command]
pub async fn clear_editor_draft(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: ClearEditorDraftRequest,
) -> Result<bool, CommandError> {
    ensure_editor_caller(window.label())?;
    // 会话可能已结束（end 后才 discard），故不强制校验身份；身份仅作诊断。
    tracing::debug!(
        key = %request.key,
        session_ref = ?request.session_ref,
        generation = request.generation,
        "clear_editor_draft"
    );
    let cleared = draft_store(&app)
        .map_err(|e| e)?
        .clear_if_matches(
            &request.key,
            request.session_ref.as_deref(),
            Some(request.generation),
            request.expected_revision,
            request.expected_hash.as_deref(),
        )
        .await?;
    if !cleared {
        tracing::debug!(key = %request.key, "clear_editor_draft: 草稿身份不匹配，保留当前草稿");
    }
    Ok(cleared)
}

/// 将来源失效后的恢复草稿转存为 orphan 候选。幂等且带完整身份墙。
#[tauri::command]
pub async fn orphan_editor_draft(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: ClearEditorDraftRequest,
) -> Result<bool, CommandError> {
    ensure_editor_caller(window.label())?;
    let marked = draft_store(&app)?
        .mark_orphaned_if_matches(
            &request.key,
            request.session_ref.as_deref(),
            Some(request.generation),
            request.expected_revision,
            request.expected_hash.as_deref(),
        )
        .await?;
    if !marked {
        tracing::debug!(key = %request.key, "orphan_editor_draft: 草稿身份不匹配，保留原状态");
    }
    Ok(marked)
}

/// 同键冲突选择“暂不处理”时，把旧候选迁移到独立 orphan 键，
/// 避免当前会话后续自动保存覆盖它。返回新键；版本墙不匹配返回 null。
#[tauri::command]
pub async fn archive_editor_draft(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: ClearEditorDraftRequest,
) -> Result<Option<String>, CommandError> {
    ensure_editor_caller(window.label())?;
    Ok(draft_store(&app)?
        .archive_if_matches(
            &request.key,
            request.session_ref.as_deref(),
            Some(request.generation),
            request.expected_revision,
            request.expected_hash.as_deref(),
        )
        .await?)
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

// ── 编辑器 AI 整理（0.23.4 §3.7 / §3.9）─────────────────────────────────

/// start_editor_transform 请求：冻结的会话身份 + 整理范围 + 冻结三元组。
///
/// `text` 为整理输入（选区文本或本次听写拼接）；`revision` / `rangeHandle`
/// 是前端 Engine 冻结的 opaque 值，后端不解释、仅随完成事件回显，最终由
/// 前端 Engine 在确认替换前复核。
#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartEditorTransformRequest {
    pub session_ref: String,
    pub generation: u64,
    pub scope: crate::domain::editor::TransformScope,
    pub text: String,
    pub revision: u64,
    pub range_handle: String,
}

/// start_editor_transform 结果：全局单活跃协调器分配的请求 id。
#[derive(Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorTransformStartResult {
    pub request_id: u64,
}

fn editor_transform_service(
    app: &tauri::AppHandle,
) -> Result<std::sync::Arc<crate::app::editor_transform::EditorTransformService>, CommandError> {
    Ok(app
        .try_state::<std::sync::Arc<crate::app::editor_transform::EditorTransformService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("io", "整理服务不可用", false))?)
}

/// 发起编辑器 AI 整理（整理选中内容 / 整理本次听写）。
///
/// 全局单活跃：主窗口或 Chat 有运行中请求时返回 `ai_already_active`
/// （detail.activeWindow 携带当前活跃窗口标识，§3.10）。
#[tauri::command]
pub async fn start_editor_transform(
    app: tauri::AppHandle,
    window: tauri::Window,
    request: StartEditorTransformRequest,
) -> Result<EditorTransformStartResult, CommandError> {
    let label = window.label().to_string();
    ensure_editor_caller(&label)?;
    tracing::info!(
        session_ref = %request.session_ref,
        generation = request.generation,
        scope = request.scope.as_str(),
        revision = request.revision,
        text_chars = request.text.chars().count(),
        "start_editor_transform"
    );

    let editor = service(&app)?;
    let transform = editor_transform_service(&app)?;
    let request_id = transform
        .start(
            &editor,
            request.session_ref,
            request.generation,
            request.scope,
            request.text,
            request.revision,
            request.range_handle,
        )
        .await?;
    Ok(EditorTransformStartResult { request_id })
}

/// 取消编辑器整理请求（新请求/视图切换/会话结束/窗口关闭）。
/// 与运行中 request_id 不匹配的取消静默忽略（幂等）。
#[tauri::command]
pub async fn cancel_editor_transform(
    app: tauri::AppHandle,
    window: tauri::Window,
    request_id: u64,
) -> Result<(), CommandError> {
    ensure_editor_caller(window.label())?;
    editor_transform_service(&app)?.cancel(request_id).await;
    Ok(())
}
