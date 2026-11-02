//! `open_gif_editor` Capability（0.28.23）——录制编辑器统一入口。
//!
//! 有编辑会话原地激活（保留当前工作），无会话进入「最近录制」首页。
//! 需要GUI_SURFACE 运行时；AI 推荐 allowlist 默认开启；MCP 代码级禁止
//! （GUI 副作用）。与 `open_settings` 同模式的 Safe GUI starter。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

pub struct OpenGifEditor;

#[async_trait::async_trait]
impl Capability for OpenGifEditor {
    fn id(&self) -> &str {
        "open_gif_editor"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_gif_editor".into(),
            description: "Open the Blink recording editor (activates the current session or opens the recent recordings home)".into(),
            parameters: json!({ "type": "object", "properties": {} }),
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
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: RuntimeRequirement::GUI_SURFACE.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;
        // 与 open_settings 一致：打开新窗口前隐藏主窗
        surface.hide_main_window("open_gif_editor");
        surface
            .open_gif_editor()
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;
        Ok(CapabilityResult::Done {
            summary: "已打开录制编辑器".into(),
        })
    }
}

inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(OpenGifEditor) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_gif_editor() {
        assert_eq!(OpenGifEditor.id(), "open_gif_editor");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = OpenGifEditor.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
        assert!(p.allows_origin(crate::capability::InvocationOrigin::LocalAi));
        assert!(!p.allows_origin(crate::capability::InvocationOrigin::Mcp));
    }

    #[test]
    fn schema_description_non_empty() {
        let s = OpenGifEditor.schema();
        assert!(!s.description.is_empty());
    }
}
