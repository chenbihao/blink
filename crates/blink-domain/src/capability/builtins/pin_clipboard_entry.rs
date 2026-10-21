//! 0.26.16：剪贴板历史条目置顶——按档位设置 `pinned_until`（惰性过期）。
//!
//! 只改排序状态，不读不改条目内容；再次置顶即重置到期时间（续期语义）。
//! 三类条目（文本/图片/文件）统一走类型化 `ClipboardEntryRef`，
//! 与 `manage_clipboard_history` 同为本地显式操作入口。
use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use crate::clipboard::history::{ClipboardEntryKind, ClipboardEntryRef, validate_entries};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// 置顶时长上限（小时）：1 年——挡脏参数，不做档位白名单（档位由 UI 提供）。
const MAX_PIN_HOURS: u32 = 8760;

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Operation {
    Pin,
    Unpin,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    operation: Operation,
    entries: Vec<ClipboardEntryRef>,
    /// `operation=pin` 必填（1..=8760 小时）；`unpin` 忽略。
    #[serde(default)]
    hours: Option<u32>,
}

pub struct PinClipboardEntry;

#[async_trait::async_trait]
impl Capability for PinClipboardEntry {
    fn id(&self) -> &str {
        "pin_clipboard_entry"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Pin or unpin clipboard history entries to keep them at the top".into(),
            parameters: json!({"type":"object","properties":{
                "operation":{"enum":["pin","unpin"],"description":"pin sets a new expiry, unpin clears it"},
                "entries":{"type":"array","minItems":1,"maxItems":2000,"items":{"type":"object","properties":{
                    "kind":{"enum":["text","image","file"]},"id":{"type":"string"}},"required":["kind","id"],"additionalProperties":false}},
                "hours":{"type":"integer","minimum":1,"maximum":MAX_PIN_HOURS,"description":"pin duration in hours; required for pin, ignored for unpin"}},
                "required":["operation","entries"],"additionalProperties":false}),
            ..Default::default()
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::NONE,
            danger: DangerClass::Safe,
            // 只改排序状态不读内容；置顶会延长条目保留（清理豁免），
            // 但出口不含 AI/MCP，无泄露面。
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
        let request: Request =
            serde_json::from_value(args).map_err(|e| CapabilityError::InvalidArgs {
                detail: e.to_string(),
            })?;
        let entries = validate_entries(request.entries)
            .map_err(|detail| CapabilityError::InvalidArgs { detail })?;
        let until = match request.operation {
            Operation::Unpin => 0,
            Operation::Pin => {
                let hours = request.hours.ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "置顶必须指定时长（hours）".into(),
                })?;
                if !(1..=MAX_PIN_HOURS).contains(&hours) {
                    return Err(CapabilityError::InvalidArgs {
                        detail: format!("置顶时长须在 1–{MAX_PIN_HOURS} 小时之间"),
                    });
                }
                chrono::Utc::now().timestamp() + i64::from(hours) * 3600
            }
        };
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "剪贴板置顶操作已超时".into(),
            });
        }

        // 各独立库分别提交；缺条目按无效引用报错（不静默成功）
        let pools = ctx.env.db_pools();
        for entry in &entries {
            let found = match entry.kind {
                ClipboardEntryKind::Text => {
                    blink_infra::data::clipboard::set_pinned_until(&pools.history, &entry.id, until)
                        .await
                }
                ClipboardEntryKind::Image => {
                    blink_infra::data::clipboard_images::set_image_pinned_until(
                        &pools.cache,
                        &entry.id,
                        until,
                    )
                    .await
                }
                ClipboardEntryKind::File => {
                    blink_infra::data::clipboard_files::set_file_pinned_until(
                        &pools.history,
                        &entry.id,
                        until,
                    )
                    .await
                }
            }
            .map_err(|e| CapabilityError::Internal {
                detail: format!("置顶状态写入失败: {e}"),
            })?;
            if !found {
                return Err(CapabilityError::InvalidState {
                    detail: format!("剪贴板条目不存在: {}", entry.id),
                });
            }
        }
        let operation = match until {
            0 => "取消置顶",
            _ => "置顶",
        };
        tracing::info!(operation, count = entries.len(), until, "剪贴板条目置顶状态更新");
        Ok(CapabilityResult::Done {
            summary: format!("{operation} {} 条剪贴板条目", entries.len()),
        })
    }
}

inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(PinClipboardEntry) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_capability_is_local_safe_and_registered() {
        let registry = crate::capability::CapabilityRegistry::new();
        assert!(registry.get("pin_clipboard_entry").is_some());
        let policy = PinClipboardEntry.policy();
        assert_eq!(policy.danger, DangerClass::Safe);
        assert!(!policy.confirmation.required, "本地显式菜单操作不弹确认");
        for origin in [
            crate::capability::InvocationOrigin::LocalAi,
            crate::capability::InvocationOrigin::Cli,
            crate::capability::InvocationOrigin::Mcp,
        ] {
            assert!(!policy.allowed_origins.contains(origin));
        }
    }

    #[test]
    fn request_rejects_unknown_shapes_and_unpin_ignores_hours() {
        // 未知字段 / 未知 kind 拒绝
        assert!(
            serde_json::from_value::<Request>(json!({
                "operation":"pin","entries":[{"kind":"audio","id":"x"}],"hours":1
            }))
            .is_err()
        );
        // unpin 不需要 hours
        let req: Request = serde_json::from_value(json!({
            "operation":"unpin","entries":[{"kind":"text","id":"clip_1"}]
        }))
        .unwrap();
        assert!(req.hours.is_none());
    }

    #[test]
    fn pin_hours_bounds_are_enforced_by_schema() {
        let schema = PinClipboardEntry.schema();
        let hours = &schema.parameters["properties"]["hours"];
        assert_eq!(hours["minimum"], 1);
        assert_eq!(hours["maximum"], MAX_PIN_HOURS);
    }
}
