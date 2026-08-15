//! Capability 错误模型（0.9.7 §3.3）。
//!
//! 可序列化，投影到：
//! - AI：序列化成 `ToolResultContent::Text`，让 LLM 知道失败原因（可重试或换路径）
//! - 前端：`is_error=true` 条目，橙色展示（复用现有 AI error 项样式）
//! - CLI（0.11）：stderr + 非零退出码
//!
//! **Timeout vs Cancelled 的区分**（重要）：
//! - `Timeout`：**时间到了**——`deadline` 触发，调用方设的预算用尽。投影到 AI："工具超时，可重试"
//! - `Cancelled`：**用户主动放弃**——ESC / seq 过期 / future drop。投影到 AI：不报错（直接 abort 整条链）

use serde::Serialize;

/// 能力调用错误——覆盖参数、状态、并发、权限、时限与内部失败。
#[derive(Debug, Clone, Serialize, PartialEq, thiserror::Error)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[allow(dead_code)]
pub enum CapabilityError {
    /// 参数缺失/类型错（args 不符 schema）。
    #[error("参数错误: {detail}")]
    InvalidArgs { detail: String },
    /// 目标实体存在，但当前状态不允许该操作（如便签已在回收站）。
    #[error("状态错误: {detail}")]
    InvalidState { detail: String },
    /// 乐观并发冲突；调用方应重新读取最新版本再决定是否重试。
    #[error("并发冲突: {detail}")]
    Conflict { detail: String },
    /// 文件/载荷内容不符合能力契约（如二进制、非 UTF-8、超长单行）。
    #[error("数据无效（{reason}）: {detail}")]
    InvalidData { reason: String, detail: String },
    /// 权限不足（剪贴板被锁/无截图权限）。
    #[error("权限不足: {detail}")]
    Permission { detail: String },
    /// 调用来源不被允许（0.21.0）——`CapabilityPolicy.allowed_origins` 门禁拒绝。
    #[error("来源不被允许: {origin} 不在允许集合内 ({allowed})")]
    OriginDenied { origin: String, allowed: String },
    /// 运行时不满足要求（0.21.0）——缺 MAIN_PROCESS / GUI_SURFACE / DESKTOP_SESSION。
    /// 返回结构化错误而非 panic，让 CLI/MCP/无头环境得到可恢复结果。
    #[error("运行时不支持: 需要 {required}，当前可用 {actual}")]
    Unsupported { required: String, actual: String },
    /// 超时——`invoke` 检查 `ctx.is_expired()` 返回 true，或 `timeout_at` 触发。
    /// 投影到 AI："工具超时，可重试或换路径"。
    #[error("超时: {detail}")]
    Timeout { detail: String },
    /// 调用方取消——用户 ESC / seq 过期 / future drop。
    /// **不报错给 LLM**（直接 abort 整条 tool_call 链，与 AI stream abort 语义一致）。
    #[error("已取消")]
    Cancelled,
    /// 能力不存在（id 未注册——AI 幻觉调了不存在的 tool）。
    #[error("能力不存在: {id}")]
    NotFound { id: String },
    /// 内部错误（Win32 失败/IO 错误）。
    #[error("内部错误: {detail}")]
    Internal { detail: String },
}

impl CapabilityError {
    /// 投影到 rig `ToolResultContent::Text`——让 AI 知道失败原因（可重试或换路径）。
    ///
    /// **0.10 multi-turn**：AI tool_call 失败后，把错误文本喂回 LLM，
    /// 让模型自行决定重试 / 换路径 / 放弃。
    ///
    /// **Cancelled 不投影**——用户已切走，不报错给 LLM（直接 abort 整条链）。
    /// 调用方负责在 Cancelled 时不调此方法。
    ///
    /// **0.9.7 仅定义**，当前单轮流程走前端 `emit_ai_clear` 展示错误。
    #[allow(dead_code)] // 0.10 multi-turn 消费
    pub fn to_rig_tool_result_text(
        &self,
    ) -> Option<rig_core::completion::message::ToolResultContent> {
        use rig_core::completion::message::ToolResultContent;
        match self {
            CapabilityError::Cancelled => None, // 不报错给 LLM
            _ => Some(ToolResultContent::text(self.to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_args_serializes() {
        let e = CapabilityError::InvalidArgs {
            detail: "缺少 query 参数".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "invalid_args");
        assert_eq!(v["detail"], "缺少 query 参数");
    }

    #[test]
    fn cancelled_is_unit_variant() {
        // Cancelled 无载荷——unit variant，序列化只有 kind
        let e = CapabilityError::Cancelled;
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "cancelled");
        // 只有一个字段
        assert_eq!(v.as_object().unwrap().len(), 1);
    }

    #[test]
    fn not_found_carries_id() {
        let e = CapabilityError::NotFound {
            id: "nonexistent_cap".into(),
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "not_found");
        assert_eq!(v["id"], "nonexistent_cap");
    }

    /// 序列化稳定性——配置文件 / IPC 消息里的 error 字段稳定。
    #[test]
    fn error_variants_serialize_stable() {
        let cases = [
            (
                CapabilityError::InvalidArgs { detail: "x".into() },
                "invalid_args",
            ),
            (
                CapabilityError::InvalidState { detail: "x".into() },
                "invalid_state",
            ),
            (CapabilityError::Conflict { detail: "x".into() }, "conflict"),
            (
                CapabilityError::InvalidData {
                    reason: "binary".into(),
                    detail: "x".into(),
                },
                "invalid_data",
            ),
            (
                CapabilityError::Permission { detail: "x".into() },
                "permission",
            ),
            (
                CapabilityError::OriginDenied {
                    origin: "mcp".into(),
                    allowed: "all".into(),
                },
                "origin_denied",
            ),
            (
                CapabilityError::Unsupported {
                    required: "gui_surface".into(),
                    actual: "none".into(),
                },
                "unsupported",
            ),
            (CapabilityError::Timeout { detail: "x".into() }, "timeout"),
            (CapabilityError::Cancelled, "cancelled"),
            (CapabilityError::NotFound { id: "x".into() }, "not_found"),
            (CapabilityError::Internal { detail: "x".into() }, "internal"),
        ];
        for (err, expected_kind) in &cases {
            let v = serde_json::to_value(err).unwrap();
            assert_eq!(v["kind"], *expected_kind, "{err:?} 序列化 kind 不符");
        }
    }

    #[test]
    fn display_is_human_readable() {
        assert_eq!(
            CapabilityError::Timeout {
                detail: "5s".into()
            }
            .to_string(),
            "超时: 5s"
        );
        assert_eq!(CapabilityError::Cancelled.to_string(), "已取消");
    }

    // ── to_rig_tool_result_text 投影测试（0.9.7 Step 4）─────────────────────

    #[test]
    fn rig_projection_cancelled_returns_none() {
        // Cancelled 不投影给 LLM——直接 abort 整条 tool_call 链
        assert!(
            CapabilityError::Cancelled
                .to_rig_tool_result_text()
                .is_none()
        );
    }

    #[test]
    fn rig_projection_timeout_returns_text() {
        use rig_core::completion::message::ToolResultContent;
        let e = CapabilityError::Timeout {
            detail: "5s".into(),
        };
        let content = e.to_rig_tool_result_text().unwrap();
        assert!(matches!(content, ToolResultContent::Text(_)));
        if let ToolResultContent::Text(t) = &content {
            assert!(t.text().contains("超时"));
        } else {
            panic!("Timeout should project to Text");
        }
    }

    #[test]
    fn rig_projection_not_found_returns_text() {
        use rig_core::completion::message::ToolResultContent;
        let e = CapabilityError::NotFound {
            id: "nonexistent".into(),
        };
        let content = e.to_rig_tool_result_text().unwrap();
        assert!(matches!(content, ToolResultContent::Text(_)));
    }

    #[test]
    fn rig_projection_all_non_cancelled_return_some() {
        // 除 Cancelled 外,所有变体都应投影成 Some(Text)
        let cases = [
            CapabilityError::InvalidArgs { detail: "x".into() },
            CapabilityError::InvalidState { detail: "x".into() },
            CapabilityError::Conflict { detail: "x".into() },
            CapabilityError::InvalidData {
                reason: "binary".into(),
                detail: "x".into(),
            },
            CapabilityError::Permission { detail: "x".into() },
            CapabilityError::OriginDenied {
                origin: "mcp".into(),
                allowed: "all".into(),
            },
            CapabilityError::Unsupported {
                required: "gui".into(),
                actual: "none".into(),
            },
            CapabilityError::Timeout { detail: "x".into() },
            CapabilityError::NotFound { id: "x".into() },
            CapabilityError::Internal { detail: "x".into() },
        ];
        for e in &cases {
            assert!(e.to_rig_tool_result_text().is_some(), "{e:?} 应投影成 Some");
        }
    }
}
