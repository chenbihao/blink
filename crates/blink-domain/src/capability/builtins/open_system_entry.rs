//! 有限身份的系统入口启动；目标来自当前目录，程序和参数分别传给平台。
use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use serde_json::{Value, json};
use std::sync::Arc;
pub struct OpenSystemEntry;
#[async_trait::async_trait]
impl Capability for OpenSystemEntry {
    fn id(&self) -> &str {
        "open_system_entry"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Open an enabled Windows system entry by its search result identity"
                .into(),
            parameters: json!({"type":"object","properties":{"entry_id":{"type":"string"}},"required":["entry_id"],"additionalProperties":false}),
            ..Default::default()
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::safe(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if !args
            .as_object()
            .is_some_and(|args| args.len() == 1 && args.contains_key("entry_id"))
        {
            return Err(CapabilityError::InvalidArgs {
                detail: "只允许 entry_id 参数".into(),
            });
        }
        let id = args
            .get("entry_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "缺少 entry_id".into(),
            })?;
        let service = ctx
            .env
            .search_service()
            .ok_or_else(|| CapabilityError::InvalidState {
                detail: "应用搜索服务尚未初始化".into(),
            })?;
        let entry =
            service
                .resolve_system_entry(id)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "系统入口不存在或来源已关闭，请重新搜索".into(),
                })?;
        let title = entry.title.clone();
        tokio::task::spawn_blocking(move || {
            blink_infra::platform::system_entries::launch(&entry.target)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: e.to_string(),
        })?
        .map_err(|detail| CapabilityError::Internal { detail })?;
        tracing::info!(entry_id = id, "系统入口启动请求已受理");
        Ok(CapabilityResult::Done {
            summary: format!("已打开: {title}"),
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(OpenSystemEntry) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finite_identity_only_and_new_origins_are_not_auto_enabled() {
        let schema = OpenSystemEntry.schema();
        assert_eq!(schema.parameters["additionalProperties"], false);
        assert_eq!(schema.parameters["required"], json!(["entry_id"]));
        assert!(schema.parameters["properties"].get("path").is_none());
        let policy = OpenSystemEntry.policy();
        assert_eq!(policy.ai_default, AiDefault::Off);
        assert_eq!(policy.mcp_default, McpDefault::DefaultOff);
        assert_eq!(
            policy.runtime_requirement,
            RuntimeRequirement::DESKTOP_SESSION
        );
    }
}
