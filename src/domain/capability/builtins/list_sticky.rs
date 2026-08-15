//! `list_sticky` Capability（0.19.3）。
//!
//! 列出所有活跃便签 → `Items`。
//!
//! **背景**：AI 需要知道当前桌面上有哪些便签（id/内容/位置/颜色）才能操作。
//! `list_sticky` 返回 `trashed=false` 的全部便签（按 updated_at 倒序），
//! 是 `set_sticky_geometry`（需 id）和 AI 上下文感知的前置依赖。
//!
//! **sensitive=true**（§3.4）：便签内容属用户隐私数据，AI 调用前需用户确认。
//!
//! **DangerClass::Safe**：只读操作，默认 Safe。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

/// `list_sticky` — 列出所有活跃便签。
///
/// 入参：`{}`（无参）。
/// 出参：`Items`，每项 data 含 `{id, content, x, y, w, h, color, always_on_top}`，
/// desc 为 content 前 30 字符（空内容时为 id）。
pub struct ListSticky;

#[async_trait::async_trait]
impl Capability for ListSticky {
    fn id(&self) -> &str {
        "list_sticky"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "list_sticky".into(),
            description: "列出所有未回收便签（包含桌面显示和已隐藏），返回每个便签的id、内容、可见性、位置(x/y)、尺寸(w/h)、颜色和置顶状态。按更新时间倒序排列。".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            sensitive: true, // 读便签内容属隐私
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
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "list_sticky 截止时刻已过".into(),
            });
        }

        let svc = ctx
            .env
            .sticky_service()
            .ok_or_else(|| CapabilityError::Internal {
                detail: "StickyService 不可用".into(),
            })?;

        let notes = svc.list_notes().await;

        let results: Vec<ItemResult> = notes
            .into_iter()
            .map(|note| {
                // desc: content 前 30 字符，空内容时用 id
                let desc = if note.content.is_empty() {
                    note.id.clone()
                } else {
                    note.content.chars().take(30).collect()
                };

                ItemResult {
                    data: json!({
                        "id": note.id,
                        "content": note.content,
                        "visible": note.visible,
                        "visibility": if note.visible { "shown" } else { "hidden" },
                        "x": note.x,
                        "y": note.y,
                        "w": note.width,
                        "h": note.height,
                        "color": note.color.as_str(),
                        "always_on_top": note.always_on_top,
                        "updated_at": note.updated_at,
                    }),
                    desc: Some(desc),
                    actions: vec![],
                }
            })
            .collect();

        tracing::debug!(count = results.len(), "list_sticky 完成");

        Ok(CapabilityResult::Items { items: results })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ListSticky) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_list_sticky() {
        assert_eq!(ListSticky.id(), "list_sticky");
    }

    #[test]
    fn schema_has_no_parameters() {
        let s = ListSticky.schema();
        assert_eq!(s.parameters["type"], "object");
        assert!(s.parameters["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ListSticky.schema();
        assert!(s.sensitive, "list_sticky 必须 sensitive=true");
    }

    #[test]
    fn danger_class_is_safe() {
        use crate::domain::capability::policy::DangerClass;
        assert_eq!(ListSticky.danger_class(), DangerClass::Safe);
    }

    #[test]
    fn schema_description_mentions_sticky() {
        let s = ListSticky.schema();
        assert!(
            s.description.contains("便签"),
            "schema description 应提及便签"
        );
    }
}
