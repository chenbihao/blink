//! `update_setting` Capability（0.19.8）——经逐次确认更新一个白名单设置。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

pub struct UpdateSetting;

#[async_trait::async_trait]
impl Capability for UpdateSetting {
    fn id(&self) -> &str {
        "update_setting"
    }

    fn schema(&self) -> CapabilitySchema {
        let allowed_ids = crate::domain::config::ManagedSettingId::ALL.map(|id| id.id());
        CapabilitySchema {
            name: self.id().into(),
            description: "修改一个 get_settings 白名单内的 Blink 设置。必须原样传回刚查询到的 old_value；每次调用都要求用户确认，不会记忆授权。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "setting_id": { "type": "string", "enum": allowed_ids, "description": "get_settings 返回的稳定 setting id" },
                    "old_value": { "description": "get_settings 返回的 current_value；用于确认展示和并发保护" },
                    "new_value": { "description": "符合该设置类型、枚举或范围的新值" }
                },
                "required": ["setting_id", "old_value", "new_value"],
                "additionalProperties": false
            }),
            ..Default::default()
        }
    }

    // 0.21.0: policy 是唯一真源——Dangerous + 不可记忆 + local+AI+CLI
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_AND_CLI,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(false), // 不可记忆
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "update_setting 截止时刻已过".into(),
            });
        }
        let setting_id = args
            .get("setting_id")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "update_setting: 缺少 setting_id".into(),
            })?;
        let old_value =
            args.get("old_value")
                .cloned()
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "update_setting: 缺少 old_value".into(),
                })?;
        let new_value =
            args.get("new_value")
                .cloned()
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "update_setting: 缺少 new_value".into(),
                })?;

        let result = ctx
            .env
            .update_managed_setting(setting_id, old_value, new_value)
            .await
            .map_err(|detail| CapabilityError::InvalidArgs { detail })?;
        let summary =
            serde_json::to_string(&result).map_err(|error| CapabilityError::Internal {
                detail: format!("设置更新结果序列化失败: {error}"),
            })?;
        Ok(CapabilityResult::Text {
            content: summary,
            desc: Some("设置已更新并立即生效".into()),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(UpdateSetting) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_is_dangerous_and_never_remembered() {
        assert_eq!(UpdateSetting.danger_class(), DangerClass::Dangerous);
        assert!(UpdateSetting.requires_ai_confirmation());
        assert!(!UpdateSetting.ai_confirmation_rememberable());
    }

    #[test]
    fn schema_requires_preview_values_and_hides_raw_kv() {
        let schema = UpdateSetting.schema();
        let required = schema.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|value| value == "setting_id"));
        assert!(required.iter().any(|value| value == "old_value"));
        assert!(required.iter().any(|value| value == "new_value"));
        assert!(schema.parameters["properties"].get("key").is_none());
        assert_eq!(
            schema.parameters["properties"]["setting_id"]["enum"]
                .as_array()
                .unwrap()
                .len(),
            crate::domain::config::ManagedSettingId::ALL.len()
        );
    }
}
