//! `sticky_manager` Capability（0.21.1）——从 Action 迁移为 Safe GUI Capability。
//!
//! 打开便签管理窗口即完成。需要 GUI_SURFACE 运行时。
//! AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

pub struct StickyManager;

#[async_trait::async_trait]
impl Capability for StickyManager {
    fn id(&self) -> &str {
        "sticky_manager"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "sticky_manager".into(),
            description: "Open the sticky notes manager window".into(),
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
        surface.hide_main_window("sticky_manager");
        surface
            .open_sticky_manager()
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;
        Ok(CapabilityResult::Done {
            summary: "已打开便签管理".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(StickyManager) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_sticky_manager() {
        assert_eq!(StickyManager.id(), "sticky_manager");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = StickyManager.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }
}
