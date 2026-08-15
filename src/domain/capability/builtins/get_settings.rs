//! `get_settings` Capability（0.19.8）——查询稳定设置白名单和当前值。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct GetSettings;

#[async_trait::async_trait]
impl Capability for GetSettings {
    fn id(&self) -> &str {
        "get_settings"
    }

    fn schema(&self) -> CapabilitySchema {
        let allowed_ids = crate::domain::config::ManagedSettingId::ALL.map(|id| id.id());
        let allowed_count = allowed_ids.len();
        CapabilitySchema {
            name: self.id().into(),
            description: "查询 Blink 允许 AI 管理的设置目录、当前值和合法取值。省略 ids 返回全部白名单；不会返回密钥、Provider、代理、插件、MCP、热键或底层 KV。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "ids": {
                        "type": "array",
                        "items": { "type": "string", "enum": allowed_ids },
                        "maxItems": allowed_count,
                        "uniqueItems": true,
                        "description": "可选的稳定 setting id 列表；省略时返回完整白名单"
                    }
                },
                "additionalProperties": false
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
                detail: "get_settings 截止时刻已过".into(),
            });
        }
        let requested = match args.get("ids") {
            None => None,
            Some(Value::Array(values)) => Some(
                values
                    .iter()
                    .map(|value| {
                        value.as_str().map(str::to_owned).ok_or_else(|| {
                            CapabilityError::InvalidArgs {
                                detail: "get_settings.ids 每项必须是 string".into(),
                            }
                        })
                    })
                    .collect::<Result<Vec<_>, _>>()?,
            ),
            Some(_) => {
                return Err(CapabilityError::InvalidArgs {
                    detail: "get_settings.ids 必须是 array".into(),
                });
            }
        };

        let mut settings = ctx
            .env
            .list_managed_settings()
            .await
            .map_err(|detail| CapabilityError::Internal { detail })?;
        if let Some(ids) = requested {
            for id in &ids {
                if crate::domain::config::ManagedSettingId::parse(id).is_none() {
                    return Err(CapabilityError::InvalidArgs {
                        detail: format!("未知或不允许查询的 setting id: {id}"),
                    });
                }
            }
            settings.retain(|setting| ids.contains(&setting.id));
        }

        Ok(CapabilityResult::Items {
            items: settings
                .into_iter()
                .map(|setting| ItemResult {
                    desc: Some(setting.description.clone()),
                    data: serde_json::to_value(setting).unwrap_or_default(),
                    actions: Vec::new(),
                })
                .collect(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(GetSettings) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_is_read_only_and_has_no_raw_key() {
        let schema = GetSettings.schema();
        assert_eq!(schema.name, "get_settings");
        assert!(!schema.sensitive);
        assert!(schema.parameters["properties"].get("key").is_none());
        assert!(schema.description.contains("不会返回密钥"));
    }
}
