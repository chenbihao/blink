//! `trash_sticky` Capability（0.19.5）——把便签移入可恢复的废纸篓。

use std::sync::Arc;

use serde_json::{Value, json};

use super::read_sticky::required_id;
use super::sticky_common::map_sticky_error;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
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
        let service = ctx
            .env
            .sticky_service()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "StickyService 不可用".into(),
            })?;
        service.trash_note(id).await.map_err(map_sticky_error)?;
        ctx.env
            .hide_sticky_and_notify_trashed(id)
            .map_err(|detail| CapabilityError::Internal { detail })?;

        tracing::info!(sticky_id = %id, "trash_sticky: 便签已移入废纸篓并隐藏");
        Ok(CapabilityResult::Done {
            summary: format!("便签 {id} 已移入废纸篓（可恢复）"),
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
