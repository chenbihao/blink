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

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
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
            description: "Open the AI chat window. Optional 'prefill' text and 'attachments' fill the chat input box as a draft (not auto-sent); 'new_conversation' starts a fresh conversation first.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prefill": {
                        "type": "string",
                        "description": "Optional text to prefill the chat input box (not auto-sent)"
                    },
                    "attachments": {
                        "type": "array",
                        "description": "Attachments signed by trusted entry points ({kind, ref, display}); appended to the chat draft, not auto-sent",
                        "items": {
                            "type": "object",
                            "properties": {
                                "kind": { "type": "string", "enum": ["audio", "text_file", "image"] },
                                "ref": { "type": "string" },
                                "display": { "type": "string" }
                            },
                            "required": ["kind", "ref", "display"]
                        }
                    },
                    "new_conversation": {
                        "type": "boolean",
                        "description": "Start a new conversation before applying the draft (default false)"
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
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: RuntimeRequirement::GUI_SURFACE.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;

        // 0.27.9 §8.2：入口侧草稿事务——prefill + attachments + new_conversation。
        // ref 必须已由可信签发入口签发（picker / 选中项确认 / 剪贴板历史链），
        // 本 Capability 只做形态校验，不签发资源。
        let draft: crate::ai::attachment::OpenChatDraft = if args.is_null() {
            Default::default()
        } else {
            serde_json::from_value(args).map_err(|e| CapabilityError::InvalidArgs {
                detail: format!("open_chat 参数无效: {e}"),
            })?
        };
        // 防御上限：附件草稿最多 8 项（与 chat_prompt 同限）
        if draft.attachments.len() > crate::ai::attachment::MAX_CHAT_ATTACHMENTS {
            return Err(CapabilityError::InvalidArgs {
                detail: "attachments 超过单轮上限 8 项".into(),
            });
        }

        // 先隐藏主窗再打开对话窗口（与旧 ChordAction 行为一致）
        surface.hide_main_window("open_chat");
        surface
            .open_chat(&draft)
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Done {
            summary: "已打开 AI 对话".into(),
        })
    }
}

inventory::submit!(crate::capability::CapabilityEntry {
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
        assert!(p.allows_origin(crate::capability::InvocationOrigin::LocalSurface));
        assert!(!p.allows_origin(crate::capability::InvocationOrigin::Mcp));
    }

    #[test]
    fn schema_has_prefill_param() {
        let s = OpenChat.schema();
        assert!(s.parameters["properties"]["prefill"]["type"].is_string());
    }
}
