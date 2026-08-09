//! `update_sticky` Capability（0.19.5）——带乐观并发的便签正文更新。

use std::sync::Arc;

use serde_json::{Value, json};

use super::read_sticky::required_id;
use super::sticky_common::map_sticky_error;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
};

pub struct UpdateSticky;

#[async_trait::async_trait]
impl Capability for UpdateSticky {
    fn id(&self) -> &str {
        "update_sticky"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "更新便签正文。必须携带 read_sticky/list_sticky 返回的 expected_updated_at；版本冲突时不会覆盖用户的新编辑。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "便签 id" },
                    "content": { "type": "string", "description": "新的完整正文" },
                    "expected_updated_at": {
                        "type": "integer",
                        "description": "读取便签时获得的 updated_at 版本"
                    }
                },
                "required": ["id", "content", "expected_updated_at"]
            }),
            sensitive: true,
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "update_sticky 截止时刻已过".into(),
            });
        }
        let id = required_id(&args, self.id())?;
        let content = args.get("content").and_then(Value::as_str).ok_or_else(|| {
            CapabilityError::InvalidArgs {
                detail: "update_sticky: 缺少 content 参数".into(),
            }
        })?;
        let expected = args
            .get("expected_updated_at")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "update_sticky: 缺少 expected_updated_at 参数".into(),
            })?;
        let service = ctx
            .env
            .sticky_service()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "StickyService 不可用".into(),
            })?;
        let updated_at = service
            .update_content(id, content, Some(expected))
            .await
            .map_err(map_sticky_error)?;

        tracing::info!(sticky_id = %id, updated_at, "update_sticky: 便签正文已更新");
        Ok(CapabilityResult::Items {
            items: vec![ItemResult {
                data: json!({ "id": id, "updated_at": updated_at }),
                desc: Some("便签已更新".into()),
                actions: vec![],
            }],
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(UpdateSticky) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_requires_revision_and_is_sensitive() {
        let schema = UpdateSticky.schema();
        assert!(schema.sensitive);
        let required = schema.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("expected_updated_at")));
    }
}
