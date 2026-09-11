//! 内容编辑器域（0.23.1）——单 EditorSession 的类型与纯逻辑。
//!
//! **定位**：框架无关。本模块不 `use tauri`（arch_guard 强制），不持有窗口、
//! DB 或事件副作用；会话的编排（窗口绑定、保存副作用、事件发射）在应用层
//! `crate::app::editor::EditorSessionService`。
//!
//! **核心规则**（docs/phases/0.23-editor-voice-ai-workflow.md §3.1/§3.3/§3.5）：
//! - 全进程最多一个活动 EditorSession；再次打开同源激活现有窗口，异源返回 `EditorBusy`。
//! - `SourceDescriptor` 只描述来源；保存目标由来源按默认映射推导（0.23.2 起可显式选择）。
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
}

/// 提交结果——按当前 CommitTarget 保存后的新基线。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CommitOutcome {
    /// 便签来源：写入后的新 `updated_at`；其余来源为 None。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<i64>,
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
}
