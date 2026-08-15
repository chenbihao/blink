//! `open_chat` Capability（0.21.2）——Chord `chat` binding 的 GUI starter target。
//!
//! 打开 AI 对话窗口即完成。可选 `prefill` 参数填充对话输入框（仅填充不自动发送）。
//! 需要 GUI_SURFACE 运行时。AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。
//!
//! **与旧 ChordAction 的关系**：旧 `ChatAction` 实现 `Action::execute()`，
//! 内部调 `DomainEnv::show_chat_window` + `hide_main_window`。
//! 本 Capability 取代该执行路径，走 `SurfacePort::open_chat` + `hide_main_window`。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct OpenChat;

#[async_trait::async_trait]
impl Capability for OpenChat {
    fn id(&self) -> &str {
        "open_chat"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_chat".into(),
            description: "Open the AI chat window. Optional 'prefill' text fills the chat input box without sending.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prefill": {
                        "type": "string",
                        "description": "Optional text to prefill the chat input box (not auto-sent)"
                    }
                }
            }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let surface = ctx.runtime.surface.ok_or_else(|| CapabilityError::Unsupported {
            required: RuntimeRequirement::GUI_SURFACE.to_string(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;

        // 读取可选 prefill 参数
        let prefill: Option<&str> = args
            .get("prefill")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty());

        // 先隐藏主窗再打开对话窗口（与旧 ChordAction 行为一致）
        surface.hide_main_window("open_chat");
        surface.open_chat(prefill).map_err(|e| CapabilityError::Internal {
            detail: e.to_string(),
        })?;

        Ok(CapabilityResult::Done {
            summary: "已打开 AI 对话".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenChat) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_chat() {
        assert_eq!(OpenChat.id(), "open_chat");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = OpenChat.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
        assert!(p.allows_origin(crate::domain::capability::InvocationOrigin::LocalSurface));
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::Mcp));
    }

    #[test]
    fn schema_has_prefill_param() {
        let s = OpenChat.schema();
        assert!(s.parameters["properties"]["prefill"]["type"].is_string());
    }
}
