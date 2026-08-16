//! `set_sticky_visibility` Capability——显示或隐藏未回收便签的桌面窗口。

use std::sync::Arc;

use serde_json::{Value, json};

use super::read_sticky::required_id;
use super::sticky_common::map_sticky_workflow_error;
use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

pub struct SetStickyVisibility;

#[async_trait::async_trait]
impl Capability for SetStickyVisibility {
    fn id(&self) -> &str {
        "set_sticky_visibility"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description:
                "显示或隐藏一个未回收便签的桌面窗口。隐藏不会把便签移入回收站，之后可重新显示。"
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "便签 id" },
                    "visible": { "type": "boolean", "description": "true 显示到桌面，false 仅隐藏桌面窗口" }
                },
                "required": ["id", "visible"]
            }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_AND_CLI,
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
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "set_sticky_visibility 截止时刻已过".into(),
            });
        }
        let id = required_id(&args, self.id())?;
        let visible = args
            .get("visible")
            .and_then(Value::as_bool)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_visibility: 缺少 visible 布尔参数".into(),
            })?;
        ctx.env
            .set_sticky_visibility_and_notify(id, visible)
            .await
            .map_err(map_sticky_workflow_error)?;

        tracing::info!(sticky_id = %id, visible, "set_sticky_visibility: 便签可见性已更新");
        Ok(CapabilityResult::Done {
            summary: if visible {
                format!("便签 {id} 已显示到桌面")
            } else {
                format!("便签 {id} 的桌面窗口已隐藏；便签仍保留且未进入回收站")
            },
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(SetStickyVisibility) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_distinguishes_hiding_from_trashing() {
        let schema = SetStickyVisibility.schema();
        assert!(schema.description.contains("不会把便签移入回收站"));
        assert_eq!(schema.parameters["required"], json!(["id", "visible"]));
    }
}
