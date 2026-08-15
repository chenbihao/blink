//! `read_sticky` Capability（0.19.5）——按 id 读取一条活跃便签。

use std::sync::Arc;

use serde_json::{Value, json};

use super::sticky_common::map_sticky_error;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct ReadSticky;

#[async_trait::async_trait]
impl Capability for ReadSticky {
    fn id(&self) -> &str {
        "read_sticky"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "按 id 读取一条活跃便签的正文、外观、几何和当前 updated_at 版本；已回收便签会明确报错。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "便签 id" }
                },
                "required": ["id"]
            }),
            sensitive: true,
        }
    }


    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "read_sticky 截止时刻已过".into(),
            });
        }
        let id = required_id(&args, self.id())?;
        let service = ctx
            .env
            .sticky_service()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "StickyService 不可用".into(),
            })?;
        let note = service
            .get_active_note(id)
            .await
            .map_err(map_sticky_error)?;

        tracing::debug!(sticky_id = %id, "read_sticky 完成");
        Ok(CapabilityResult::Items {
            items: vec![ItemResult {
                data: json!({
                    "id": note.id,
                    "content": note.content,
                    "format": note.format.as_str(),
                    "color": note.color.as_str(),
                    "visible": note.visible,
                    "x": note.x,
                    "y": note.y,
                    "w": note.width,
                    "h": note.height,
                    "always_on_top": note.always_on_top,
                    "created_at": note.created_at,
                    "updated_at": note.updated_at,
                }),
                desc: Some("便签详情".into()),
                actions: vec![],
            }],
        })
    }
}

pub(super) fn required_id<'a>(args: &'a Value, tool: &str) -> Result<&'a str, CapabilityError> {
    args.get("id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: format!("{tool}: 缺少 id 参数"),
        })
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ReadSticky) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_sensitive_and_requires_id() {
        let schema = ReadSticky.schema();
        assert!(schema.sensitive);
        assert_eq!(schema.parameters["required"], json!(["id"]));
    }
}
