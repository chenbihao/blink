//! 内容编辑器域（0.23.1；0.23.2 起含保存目标）——单 EditorSession 的类型与纯逻辑。
//!
//! **定位**：框架无关。本模块不 `use tauri`（arch_guard 强制），不持有窗口、
//! DB 或事件副作用；会话的编排（窗口绑定、保存副作用、事件发射）在应用层
//! `crate::app::editor::EditorSessionService`。
//!
//! **核心规则**（docs/phases/0.23-editor-voice-ai-workflow.md §3.1/§3.3/§3.5）：
//! - 全进程最多一个活动 EditorSession；再次打开同源激活现有窗口，异源返回 `EditorBusy`。
//! - `SourceDescriptor` 只描述来源；`CommitTarget` 只描述主保存去向，二者分离（§3.5）。
//! - `session_ref` 是后端生成的不可猜测 opaque 引用；所有 IPC 校验 `session_ref + generation`。
//! - Source envelope：2,000,000 字符硬上限，超限拒绝载入（§3.10 冻结值）。

use serde::{Deserialize, Serialize};

/// Source envelope 硬上限（§3.10 冻结）：2,000,000 字符，超限拒绝载入。
pub const MAX_SOURCE_CHARS: usize = 2_000_000;

/// 编辑器来源描述符（§3.5）——只描述"内容从哪来"，不描述保存去向。
///
/// serde tag 供 IPC 直接传输；同源判定只对持久来源（便签）有意义。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum SourceDescriptor {
    /// 空白编辑（0.23.3 连续听写的主要入口）。
    Empty,
    /// 剪贴板历史文本项。`item_ref` 为剪贴板历史记录 id（可选）。
    ClipboardItem { item_ref: Option<String> },
    /// 便签正文（持久来源，保存目标为原位更新）。
    Sticky { sticky_id: String },
    /// 划词选区文本。
    Selection,
    /// 能力结果文本（如 Chord edit 的当前内容）。
    CapabilityResult { capability_id: String },
}

impl SourceDescriptor {
    /// 持久来源判定键：同一键的打开请求激活现有窗口而非 `EditorBusy`。
    ///
    /// 目前只有便签是持久编辑目标；剪贴板/选区/能力结果都是一次性临时任务。
    pub fn persistence_key(&self) -> Option<String> {
        match self {
            SourceDescriptor::Sticky { sticky_id } => Some(format!("sticky:{sticky_id}")),
            _ => None,
        }
    }

    /// 便签 id（若来源为便签）。用于租约事件与保存分派。
    pub fn sticky_id(&self) -> Option<&str> {
        match self {
            SourceDescriptor::Sticky { sticky_id } => Some(sticky_id),
            _ => None,
        }
    }
}

/// 主保存目标（§3.5，0.23.2 类型化）——只描述"保存到哪里"，与来源分离。
///
/// `ClipboardResult` 为会话结果历史项：第一次保存创建，后续保存更新同一项
///（§3.10，以结果项 id 为键）。`ConfirmedFile` 仅由用户完成"保存到…"后建立，
/// 后续保存校验文件 identity 后原位写入。
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum CommitTarget {
    /// 会话剪贴板结果（默认：剪贴板、空白、选区、不可回写能力结果）。
    ClipboardResult,
    /// 便签原位更新（默认：便签来源）。
    UpdateSticky { sticky_id: String },
    /// 回写具有可靠 endpoint 的 Blink 调用方（0.23.2 无此入口，协议预留）。
    #[allow(dead_code)] // 协议完备性：默认映射与"保存到…"都不会产生该目标
    ReturnToCaller,
    /// 已确认文件（用户"保存到…"后建立；后续保存原位写入）。
    ConfirmedFile { path: String },
}

/// 按来源推导默认保存目标（§3.5 冻结映射）。
pub fn default_commit_target(source: &SourceDescriptor) -> CommitTarget {
    match source {
        SourceDescriptor::Sticky { sticky_id } => CommitTarget::UpdateSticky {
            sticky_id: sticky_id.clone(),
        },
        _ => CommitTarget::ClipboardResult,
    }
}

/// 单次提交的目标覆盖（§3.5：「保存到…」切换主目标；「另存为副本…」不改）。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CommitTargetOverride {
    /// 写入指定文件并切换主目标为 `ConfirmedFile`。
    SaveToFile { path: String },
    /// 写入指定文件副本，主目标不变。
    SaveCopyToFile { path: String },
    /// 已确认文件被外部修改后，用户显式选择覆盖（跳过 identity 校验）。
    OverwriteConfirmedFile,
}

/// 单次提交的执行计划——`plan_commit` 纯函数产物，应用层照单执行副作用。
#[derive(Debug, Clone, PartialEq)]
pub enum CommitPlan {
    /// 便签原位更新（携带冲突基线 revision）。
    UpdateSticky {
        sticky_id: String,
        expected_revision: Option<i64>,
    },
    /// 创建新的会话剪贴板结果项。
    CreateClipboardResult { origin_ref: Option<String> },
    /// 更新既有的会话剪贴板结果项（同一项不堆积）。
    UpdateClipboardResult { result_item_id: String },
    /// 原位写入已确认文件（先校验 identity）。
    WriteConfirmedFile {
        path: String,
        identity: FileIdentity,
    },
    /// 用户跳过 identity 校验强制覆盖已确认文件。
    OverwriteConfirmedFile { path: String },
    /// 写入"保存到…"目标并切换主目标。
    SaveToFile { path: String },
    /// 写入副本文件，主目标不变。
    SaveCopyToFile { path: String },
    /// 回写调用方（0.23.2 无可靠 endpoint，恒不可用）。
    ReturnToCaller,
}

/// 已确认文件的写入身份（§3.5：identity/mtime 保护，不被静默覆盖）。
///
/// 仅在会话内存活：写入成功后记录 size + mtime_ms，下次原位保存前
/// 与磁盘现状比对，不一致返回 `SourceConflict`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileIdentity {
    pub size: u64,
    pub mtime_ms: i64,
}

/// 提交计划纯决策（§3.5/§3.10）。
///
/// 输入会话当前状态（主目标、结果项 id、文件 identity、便签冲突基线、来源
/// 原始引用）与单次覆盖，产出完整执行计划；应用层照单执行副作用，不再自行
/// 分支。
///
/// - 显式覆盖优先：`SaveToFile`/`SaveCopyToFile`/`OverwriteConfirmedFile`；
/// - 否则按当前主目标分派：便签带 revision；剪贴板结果首存创建、后续更新
///   同一项；已确认文件带 identity 校验（无 identity 视为首次落盘，直接写）。
pub fn plan_commit(
    state: CommitState<'_>,
    r#override: Option<&CommitTargetOverride>,
) -> CommitPlan {
    match r#override {
        Some(CommitTargetOverride::SaveToFile { path }) => {
            CommitPlan::SaveToFile { path: path.clone() }
        }
        Some(CommitTargetOverride::SaveCopyToFile { path }) => {
            CommitPlan::SaveCopyToFile { path: path.clone() }
        }
        Some(CommitTargetOverride::OverwriteConfirmedFile) => match state.target {
            CommitTarget::ConfirmedFile { path } => {
                CommitPlan::OverwriteConfirmedFile { path: path.clone() }
            }
            _ => CommitPlan::OverwriteConfirmedFile {
                path: String::new(),
            },
        },
        None => match state.target {
            CommitTarget::UpdateSticky { sticky_id } => CommitPlan::UpdateSticky {
                sticky_id: sticky_id.clone(),
                expected_revision: state.sticky_revision,
            },
            CommitTarget::ClipboardResult => match state.result_item_id {
                Some(result_item_id) => CommitPlan::UpdateClipboardResult {
                    result_item_id: result_item_id.to_string(),
                },
                None => CommitPlan::CreateClipboardResult {
                    origin_ref: state.source_item_ref.map(str::to_string),
                },
            },
            CommitTarget::ConfirmedFile { path } => match state.file_identity {
                Some(identity) => CommitPlan::WriteConfirmedFile {
                    path: path.clone(),
                    identity,
                },
                None => CommitPlan::OverwriteConfirmedFile { path: path.clone() },
            },
            CommitTarget::ReturnToCaller => CommitPlan::ReturnToCaller,
        },
    }
}

/// `plan_commit` 的会话状态输入（借用快照，避免搬运 LiveSession）。
#[derive(Debug, Clone, Copy)]
pub struct CommitState<'a> {
    /// 当前主保存目标。
    pub target: &'a CommitTarget,
    /// 会话剪贴板结果项 id（首存后存在）。
    pub result_item_id: Option<&'a str>,
    /// 已确认文件的写入身份（"保存到…"成功后存在）。
    pub file_identity: Option<FileIdentity>,
    /// 便签冲突基线（打开/上次成功提交时的 `updated_at`）。
    pub sticky_revision: Option<i64>,
    /// 剪贴板来源的原始历史项 id（首存继承命中数用）。
    pub source_item_ref: Option<&'a str>,
}

/// Markdown 视图入口策略（§3.3）——按来源声明，风险门在前端执行且优先于本策略。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MarkdownViewPolicy {
    /// 只显示原文（结构化代码/配置类来源；0.23.1 无此类来源，预留）。
    Disabled,
    /// 默认原文，可手动进入 MD。
    Available,
    /// 通过风险门后默认 MD。
    Preferred,
}

/// 按来源推导视图策略（§3.3 冻结：空白和便签为 `Preferred`；其余为 `Available`）。
pub fn markdown_view_policy(source: &SourceDescriptor) -> MarkdownViewPolicy {
    match source {
        SourceDescriptor::Empty | SourceDescriptor::Sticky { .. } => MarkdownViewPolicy::Preferred,
        _ => MarkdownViewPolicy::Available,
    }
}

/// 编辑器结构化错误（§3.9 冻结集合的 0.23.1 子集）。
///
/// 经 `CommandError` 投影后前端按 `code` 分类展示，不解析中文 message。
/// `VoiceBusy`/`AiAlreadyActive`/`Cancelled` 随 0.23.3/0.23.4 接入时补充。
#[derive(Debug, Clone, PartialEq, thiserror::Error, Serialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum EditorError {
    /// 已有不同来源的活动会话——正文绝不覆盖。
    #[error("已有编辑任务进行中")]
    EditorBusy { active_title: Option<String> },
    /// session_ref 或 generation 不匹配——请求来自旧会话，只能清理自身。
    #[error("编辑会话已失效")]
    StaleSession,
    /// 前端提交的 content_revision 已过期（§3.9 冻结错误集；0.23.4 AI 候选启用）。
    #[error("内容版本已过期")]
    #[allow(dead_code)]
    StaleRevision,
    /// 便签等持久来源在会话外被修改，原位保存被拒绝。
    #[error("内容已被外部修改")]
    SourceConflict {
        expected_updated_at: i64,
        actual_updated_at: i64,
    },
    /// 保存目标不可用（如剪贴板被占用、目标已失效）。
    #[error("保存目标不可用: {detail}")]
    TargetUnavailable { detail: String },
    /// 协议违规或来源不合法（如错误窗口调用、来源超限）。
    #[error("不支持的操作: {detail}")]
    Unsupported { detail: String },
    /// 底层 IO/DB 失败。
    #[error("存储错误: {detail}")]
    Io { detail: String },
}

/// 打开请求（§3.9 `open_content_editor(request)`）。
///
/// 便签来源的正文与 revision 由服务端在绑定时从 DB 读取，`body` 不作为真源。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct OpenEditorRequest {
    /// 初始文本（非便签来源时为真源；便签来源忽略此字段）。
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub title: Option<String>,
    pub source: SourceDescriptor,
}

/// 只读会话快照（§3.9 `get_content_editor_session(session_ref)`）——前端绑定时拉取。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EditorSessionSnapshot {
    pub session_ref: String,
    pub generation: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// 初始正文（UTF-8 真源）。
    pub body: String,
    pub source: SourceDescriptor,
    /// 便签来源的打开时 revision（`updated_at`），作为原位保存的冲突基线。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<i64>,
    pub markdown_policy: MarkdownViewPolicy,
    /// 当前主保存目标（0.23.2）——前端主保存区按此显示真实去向。
    pub commit_target: CommitTarget,
}

/// 提交请求（§3.9 `commit_content_editor(request)`）——前端提交完整 UTF-8 文本。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitEditorRequest {
    pub session_ref: String,
    pub generation: u64,
    /// 前端单调 `content_revision`（0.23.1 作簿记，0.23.4 AI 候选以此判 stale）。
    pub revision: u64,
    pub body: String,
    /// 单次目标覆盖（"保存到…"/"另存为副本…"/覆盖冲突文件）；省略按主目标。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<CommitTargetOverride>,
}

/// 提交结果——按目标保存后的新基线与新主目标。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitOutcome {
    /// 便签来源：写入后的新 `updated_at`；其余来源为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<i64>,
    /// 本次提交后的主保存目标（"保存到…"会从默认目标切换为 ConfirmedFile）。
    pub commit_target: CommitTarget,
    /// 目标文件身份（ConfirmedFile/保存到 成功后返回，供诊断与测试；冲突基线在后端）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub file_identity: Option<FileIdentityDto>,
}

/// 文件身份的 IPC 投影（ms 时间戳，跨进程序稳定）。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct FileIdentityDto {
    pub size: u64,
    pub mtime_ms: i64,
}

impl From<FileIdentity> for FileIdentityDto {
    fn from(i: FileIdentity) -> Self {
        Self {
            size: i.size,
            mtime_ms: i.mtime_ms,
        }
    }
}

impl From<FileIdentityDto> for FileIdentity {
    fn from(i: FileIdentityDto) -> Self {
        Self {
            size: i.size,
            mtime_ms: i.mtime_ms,
        }
    }
}

/// 结束请求（§3.9 `end_content_editor(request)`）——保存后结束或明确放弃。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndEditorRequest {
    pub session_ref: String,
    pub generation: u64,
    /// `"saved"`（已提交）或 `"abandoned"`（放弃修改）；仅作日志语义。
    pub reason: EndReason,
}

/// 退出确认应答（§3.5 主动退出一次汇总确认）——前端对
/// `blink://editor-exit-request` 的回应。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResolveEditorExitRequest {
    /// 必须与待决请求 id 一致（不可猜测），迟到的旧应答直接忽略。
    pub request_id: String,
    /// true = 用户确认退出；false = 取消退出。
    pub confirmed: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EndReason {
    Saved,
    Abandoned,
}

/// 打开决策——纯函数产物，由应用层执行窗口副作用。
#[derive(Debug, Clone, PartialEq)]
pub enum OpenDecision {
    /// 无活动会话：绑定新会话。
    Create,
    /// 与当前持久来源相同：激活已有窗口（不重置正文）。
    ActivateExisting,
    /// 来源不同：返回 `EditorBusy` 并激活已有窗口。
    Busy,
}

/// 打开决策纯逻辑（§3.1 冻结规则）。
///
/// - 无活动会话 → `Create`
/// - 同一持久来源（当前仅便签）→ `ActivateExisting`
/// - 其他一切（含临时来源重复打开）→ `Busy`：0.23 不排队、不覆盖、不自动合并
pub fn decide_open(
    active_persistence_key: Option<&str>,
    request: &SourceDescriptor,
) -> OpenDecision {
    match active_persistence_key {
        None => OpenDecision::Create,
        Some(active) => {
            if Some(active) == request.persistence_key().as_deref() {
                OpenDecision::ActivateExisting
            } else {
                OpenDecision::Busy
            }
        }
    }
}

/// Source envelope 校验（§3.10）：超限拒绝载入，按字符数而非字节数。
pub fn validate_source_body(body: &str) -> Result<(), EditorError> {
    if body.chars().count() > MAX_SOURCE_CHARS {
        return Err(EditorError::Unsupported {
            detail: format!("文本超过编辑器上限（{MAX_SOURCE_CHARS} 字符），无法载入"),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sticky(id: &str) -> SourceDescriptor {
        SourceDescriptor::Sticky {
            sticky_id: id.into(),
        }
    }

    #[test]
    fn persistence_key_only_for_sticky() {
        assert_eq!(
            sticky("s1").persistence_key(),
            Some("sticky:s1".to_string())
        );
        assert_eq!(SourceDescriptor::Empty.persistence_key(), None);
        assert_eq!(SourceDescriptor::Selection.persistence_key(), None);
        assert_eq!(
            SourceDescriptor::ClipboardItem { item_ref: None }.persistence_key(),
            None
        );
    }

    #[test]
    fn decide_open_no_active_creates() {
        assert_eq!(decide_open(None, &sticky("s1")), OpenDecision::Create);
        assert_eq!(
            decide_open(None, &SourceDescriptor::Selection),
            OpenDecision::Create
        );
    }

    #[test]
    fn decide_open_same_sticky_activates() {
        assert_eq!(
            decide_open(Some("sticky:s1"), &sticky("s1")),
            OpenDecision::ActivateExisting
        );
    }

    #[test]
    fn decide_open_different_sticky_is_busy() {
        assert_eq!(
            decide_open(Some("sticky:s1"), &sticky("s2")),
            OpenDecision::Busy
        );
    }

    #[test]
    fn decide_open_ephemeral_reopen_is_busy() {
        // 临时来源（剪贴板/选区/能力结果）重复打开一律 Busy，不覆盖正文
        assert_eq!(
            decide_open(Some("sticky:s1"), &SourceDescriptor::Selection),
            OpenDecision::Busy
        );
        assert_eq!(
            decide_open(Some("sticky:s1"), &SourceDescriptor::Empty),
            OpenDecision::Busy
        );
    }

    #[test]
    fn view_policy_preferred_for_empty_and_sticky() {
        assert_eq!(
            markdown_view_policy(&SourceDescriptor::Empty),
            MarkdownViewPolicy::Preferred
        );
        assert_eq!(
            markdown_view_policy(&sticky("s1")),
            MarkdownViewPolicy::Preferred
        );
        assert_eq!(
            markdown_view_policy(&SourceDescriptor::Selection),
            MarkdownViewPolicy::Available
        );
        assert_eq!(
            markdown_view_policy(&SourceDescriptor::ClipboardItem { item_ref: None }),
            MarkdownViewPolicy::Available
        );
    }

    #[test]
    fn source_body_validation_rejects_over_envelope() {
        assert!(validate_source_body("hello").is_ok());
        let edge = "字".repeat(MAX_SOURCE_CHARS);
        assert!(validate_source_body(&edge).is_ok());
        let over = "字".repeat(MAX_SOURCE_CHARS + 1);
        let err = validate_source_body(&over).unwrap_err();
        assert!(matches!(err, EditorError::Unsupported { .. }));
    }

    #[test]
    fn error_serializes_with_kind_tag() {
        let e = EditorError::SourceConflict {
            expected_updated_at: 10,
            actual_updated_at: 11,
        };
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["kind"], "source_conflict");
        assert_eq!(json["expectedUpdatedAt"], 10);

        let busy = serde_json::to_value(EditorError::EditorBusy {
            active_title: Some("编辑便签".into()),
        })
        .unwrap();
        assert_eq!(busy["kind"], "editor_busy");
        assert_eq!(busy["activeTitle"], "编辑便签");
    }

    #[test]
    fn open_request_deserializes_camel_case() {
        let raw = serde_json::json!({
            "body": "hello",
            "title": "t",
            "source": { "kind": "sticky", "stickyId": "s1" }
        });
        let req: OpenEditorRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(req.body, "hello");
        assert_eq!(req.source, sticky("s1"));
    }

    // ── 0.23.2 保存目标 ─────────────────────────────────────────────────────

    #[test]
    fn default_target_follows_source() {
        assert_eq!(
            default_commit_target(&sticky("s1")),
            CommitTarget::UpdateSticky {
                sticky_id: "s1".into()
            }
        );
        for source in [
            SourceDescriptor::Empty,
            SourceDescriptor::Selection,
            SourceDescriptor::ClipboardItem { item_ref: None },
            SourceDescriptor::CapabilityResult {
                capability_id: "x".into(),
            },
        ] {
            assert_eq!(
                default_commit_target(&source),
                CommitTarget::ClipboardResult,
                "{source:?} 默认目标应为剪贴板结果"
            );
        }
    }

    fn commit_state<'a>(
        target: &'a CommitTarget,
        result_item_id: Option<&'a str>,
        file_identity: Option<FileIdentity>,
    ) -> CommitState<'a> {
        CommitState {
            target,
            result_item_id,
            file_identity,
            sticky_revision: Some(42),
            source_item_ref: Some("orig-1"),
        }
    }

    #[test]
    fn plan_sticky_carries_revision_baseline() {
        let target = CommitTarget::UpdateSticky {
            sticky_id: "s1".into(),
        };
        let plan = plan_commit(commit_state(&target, None, None), None);
        assert_eq!(
            plan,
            CommitPlan::UpdateSticky {
                sticky_id: "s1".into(),
                expected_revision: Some(42)
            }
        );
    }

    #[test]
    fn plan_clipboard_first_creates_then_updates_same_item() {
        let target = CommitTarget::ClipboardResult;
        assert_eq!(
            plan_commit(commit_state(&target, None, None), None),
            CommitPlan::CreateClipboardResult {
                origin_ref: Some("orig-1".into())
            }
        );
        assert_eq!(
            plan_commit(commit_state(&target, Some("res-1"), None), None),
            CommitPlan::UpdateClipboardResult {
                result_item_id: "res-1".into()
            }
        );
    }

    #[test]
    fn plan_confirmed_file_checks_identity_until_forced() {
        let target = CommitTarget::ConfirmedFile {
            path: "C:\\a.md".into(),
        };
        let identity = FileIdentity {
            size: 5,
            mtime_ms: 100,
        };
        assert_eq!(
            plan_commit(commit_state(&target, None, Some(identity)), None),
            CommitPlan::WriteConfirmedFile {
                path: "C:\\a.md".into(),
                identity
            }
        );
        // identity 尚未建立（首存落盘前）直接写
        assert_eq!(
            plan_commit(commit_state(&target, None, None), None),
            CommitPlan::OverwriteConfirmedFile {
                path: "C:\\a.md".into()
            }
        );
        // 用户显式覆盖
        let force = CommitTargetOverride::OverwriteConfirmedFile;
        assert_eq!(
            plan_commit(commit_state(&target, None, Some(identity)), Some(&force)),
            CommitPlan::OverwriteConfirmedFile {
                path: "C:\\a.md".into()
            }
        );
    }

    #[test]
    fn plan_override_save_to_switches_and_copy_does_not() {
        let target = CommitTarget::ClipboardResult;
        let save_to = CommitTargetOverride::SaveToFile {
            path: "D:\\out.md".into(),
        };
        assert_eq!(
            plan_commit(commit_state(&target, Some("res-1"), None), Some(&save_to)),
            CommitPlan::SaveToFile {
                path: "D:\\out.md".into()
            }
        );
        let save_copy = CommitTargetOverride::SaveCopyToFile {
            path: "D:\\copy.md".into(),
        };
        assert_eq!(
            plan_commit(commit_state(&target, Some("res-1"), None), Some(&save_copy)),
            CommitPlan::SaveCopyToFile {
                path: "D:\\copy.md".into()
            }
        );
    }

    #[test]
    fn commit_target_and_request_serde_roundtrip() {
        let encoded = serde_json::to_value(CommitTarget::ConfirmedFile {
            path: "D:\\a.md".into(),
        })
        .unwrap();
        assert_eq!(encoded["kind"], "confirmed_file");
        assert_eq!(encoded["path"], "D:\\a.md");

        // 请求覆盖字段：camelCase tagged + 可省略
        let raw = serde_json::json!({
            "sessionRef": "ed_x",
            "generation": 1,
            "revision": 3,
            "body": "text",
            "target": { "kind": "save_to_file", "path": "D:\\out.md" }
        });
        let req: CommitEditorRequest = serde_json::from_value(raw).unwrap();
        assert_eq!(
            req.target,
            Some(CommitTargetOverride::SaveToFile {
                path: "D:\\out.md".into()
            })
        );
        let bare = serde_json::json!({
            "sessionRef": "ed_x", "generation": 1, "revision": 3, "body": "t"
        });
        let req: CommitEditorRequest = serde_json::from_value(bare).unwrap();
        assert_eq!(req.target, None);
    }
}
