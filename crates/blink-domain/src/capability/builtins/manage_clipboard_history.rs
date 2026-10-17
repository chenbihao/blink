//! 本地剪贴板管理的唯一批量执行入口：类型化删除与逐张钉图。
use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, ItemResult, McpDefault, OriginSet,
    RuntimeRequirement,
};
use crate::clipboard::history::{ClipboardEntryKind, ClipboardEntryRef, validate_entries};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

static PIN_GATE: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const MAX_PIN_IMAGES: usize = 16;

fn validate_pin_entries(entries: &[ClipboardEntryRef]) -> Result<(), CapabilityError> {
    if entries.len() > MAX_PIN_IMAGES || entries.iter().any(|r| r.kind != ClipboardEntryKind::Image)
    {
        return Err(CapabilityError::InvalidArgs {
            detail: format!("一次最多钉 {MAX_PIN_IMAGES} 张图片，且只能包含图片"),
        });
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Delete,
    Pin,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: Operation,
    entries: Vec<ClipboardEntryRef>,
}

pub struct ManageClipboardHistory;
#[async_trait::async_trait]
impl Capability for ManageClipboardHistory {
    fn id(&self) -> &str {
        "manage_clipboard_history"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Manage explicitly selected local clipboard history entries".into(),
            parameters: json!({"type":"object","properties":{
                "operation":{"enum":["delete","pin"]},
                "entries":{"type":"array","minItems":1,"maxItems":2000,"items":{"type":"object","properties":{
                    "kind":{"enum":["text","image","file"]},"id":{"type":"string"}},"required":["kind","id"],"additionalProperties":false}}},
                "required":["operation","entries"],"additionalProperties":false}),
            ..Default::default()
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Dangerous,
            sensitive: true,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::dangerous(false),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let request: Request =
            serde_json::from_value(args).map_err(|e| CapabilityError::InvalidArgs {
                detail: e.to_string(),
            })?;
        let entries = validate_entries(request.entries)
            .map_err(|detail| CapabilityError::InvalidArgs { detail })?;
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "剪贴板批量操作已超时".into(),
            });
        }
        let operation = match &request.operation {
            Operation::Delete => "delete",
            Operation::Pin => "pin",
        };
        let outcomes = match request.operation {
            Operation::Delete => {
                crate::clipboard::history::delete_entries(ctx.env.db_pools(), entries).await
            }
            Operation::Pin => {
                validate_pin_entries(&entries)?;
                let _guard = PIN_GATE
                    .try_lock()
                    .map_err(|_| CapabilityError::InvalidState {
                        detail: "正在批量钉图，请稍后再试".into(),
                    })?;
                crate::clipboard::history::pin_entries(entries, |entry, position| async move {
                    if ctx.is_expired() {
                        return Err("钉图任务已超时".to_string());
                    }
                    let png =
                        crate::clipboard::load_history_png(&ctx.env.db_pools().cache, &entry.id)
                            .await
                            .map_err(|e| e.to_string())?;
                    ctx.env
                        .show_pin_image(png, position.map(|p| p.0), position.map(|p| p.1))
                        .map_err(|e| e.to_string())
                })
                .await
            }
        };
        let failed = outcomes.iter().filter(|o| !o.success).count();
        if failed > 0 {
            let error = outcomes
                .iter()
                .find_map(|o| o.error.as_deref())
                .unwrap_or("未知错误");
            tracing::error!(
                operation,
                count = outcomes.len(),
                failed,
                error,
                "剪贴板批量操作部分失败"
            );
        } else {
            tracing::info!(operation, count = outcomes.len(), "剪贴板批量操作完成");
        }
        Ok(CapabilityResult::Items {
            items: outcomes
                .into_iter()
                .map(|o| ItemResult {
                    data: json!(o),
                    desc: None,
                    actions: vec![],
                })
                .collect(),
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(ManageClipboardHistory) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn management_is_explicit_local_only_and_registered() {
        let registry = crate::capability::CapabilityRegistry::new();
        assert!(registry.get("manage_clipboard_history").is_some());
        let policy = ManageClipboardHistory.policy();
        assert!(
            policy
                .allowed_origins
                .contains(crate::capability::InvocationOrigin::LocalSurface)
        );
        for origin in [
            crate::capability::InvocationOrigin::LocalAi,
            crate::capability::InvocationOrigin::Cli,
            crate::capability::InvocationOrigin::Mcp,
        ] {
            assert!(!policy.allowed_origins.contains(origin));
        }
        assert_eq!(policy.mcp_default, McpDefault::Forbidden);
    }
    #[test]
    fn unknown_operations_and_kinds_are_rejected_before_any_side_effect() {
        for args in [
            json!({"operation":"clear","entries":[]}),
            json!({"operation":"delete","entries":[{"kind":"audio","id":"x"}]}),
        ] {
            assert!(serde_json::from_value::<Request>(args).is_err());
        }
    }
    #[test]
    fn pin_input_is_bounded_and_does_not_accept_text() {
        let entry = ClipboardEntryRef {
            kind: ClipboardEntryKind::Image,
            id: "img".into(),
        };
        assert!(validate_pin_entries(&vec![entry.clone(); 16]).is_ok());
        assert!(validate_pin_entries(&vec![entry; 17]).is_err());
        assert!(
            validate_pin_entries(&[ClipboardEntryRef {
                kind: ClipboardEntryKind::Text,
                id: "text".into()
            }])
            .is_err()
        );
    }
}
