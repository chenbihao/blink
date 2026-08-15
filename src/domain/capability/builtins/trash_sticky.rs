//! `trash_sticky` Capability（0.19.5）——把便签移入可恢复的废纸篓。

use std::sync::Arc;

use serde_json::{Value, json};

use super::read_sticky::required_id;
use super::sticky_common::map_sticky_workflow_error;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct TrashSticky;

#[async_trait::async_trait]
impl Capability for TrashSticky {
    fn id(&self) -> &str {
        "trash_sticky"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "将指定便签移入可恢复的废纸篓，并立即隐藏其桌面窗口；不会永久删除。"
                .into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "便签 id" }
                },
                "required": ["id"]
            }),
            ..Default::default()
        }
    }


    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
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
                detail: "trash_sticky 截止时刻已过".into(),
            });
        }
        let id = required_id(&args, self.id())?;
        ctx.env
            .trash_sticky_and_notify(id)
            .await
            .map_err(map_sticky_workflow_error)?;

        tracing::info!(sticky_id = %id, "trash_sticky: 便签已移入废纸篓并隐藏");
        Ok(CapabilityResult::Done {
            summary: format!("便签 {id} 已移入回收站；桌面窗口已关闭，可从回收站恢复"),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(TrashSticky) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::execution::DangerClass;

    #[test]
    fn schema_is_recoverable_and_safe() {
        let schema = TrashSticky.schema();
        assert!(schema.description.contains("不会永久删除"));
        assert_eq!(TrashSticky.danger_class(), DangerClass::Safe);
    }
}
