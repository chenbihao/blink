//! `open_settings` Capability（0.21.1）——从 Action 迁移为 Safe GUI Capability。
//!
//! 打开设置窗口即完成。需要 GUI_SURFACE 运行时。
//! AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct OpenSettings;

#[async_trait::async_trait]
impl Capability for OpenSettings {
    fn id(&self) -> &str {
        "open_settings"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_settings".into(),
            description: "Open the Blink settings window".into(),
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
        let surface = ctx.runtime.surface.ok_or_else(|| CapabilityError::Unsupported {
            required: RuntimeRequirement::GUI_SURFACE.to_string(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;
        // 打开设置前先隐藏主窗口（与旧 Action 行为一致）
        surface.hide_main_window("open_settings");
        surface.open_settings().map_err(|e| CapabilityError::Internal {
            detail: e.to_string(),
        })?;
        Ok(CapabilityResult::Done { summary: "已打开设置".into() })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenSettings) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_settings() {
        assert_eq!(OpenSettings.id(), "open_settings");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = OpenSettings.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
        assert!(p.allows_origin(crate::domain::capability::InvocationOrigin::LocalAi));
        assert!(!p.allows_origin(crate::domain::capability::InvocationOrigin::Mcp));
    }

    #[test]
    fn schema_description_non_empty() {
        let s = OpenSettings.schema();
        assert!(!s.description.is_empty());
    }
}
