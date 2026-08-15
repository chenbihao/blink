//! `list_clipboard_images` Capability（0.19.1）。
//!
//! 列出最近的剪贴板图片历史元数据 → `Items`。
//!
//! **背景**：`search_clipboard_history` Capability 只查文本 `clipboard` 表（kind 硬编码
//! `"text"`），图片历史在独立的 `clipboard_images` 表，AI 无从得知 image_id。本 cap
//! 补上"AI 看到剪贴板有哪些图片"的入口，是"读最近剪贴板图片 pin 桌面"等场景的前置依赖。
//!
//! **与 `read_clipboard_history_image` 的配合**：AI 先调本 cap 拿到图片列表（含 id），
//! 再调 `read_clipboard_history_image` 按 id 取完整图片字节。
//!
//! **sensitive=true**：读剪贴板图片历史属隐私敏感数据，与 `search_clipboard_history` 同级。
//!
//! **无 actions**：list_clipboard_images 是感知能力，不直接操作图片。AI 拿到 id 后
//! 组合其他 cap（如 `read_clipboard_history_image` + `pin_image`）完成操作。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext, ItemResult,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};
use crate::infra::data::clipboard_images;

/// `list_clipboard_images` — 列出最近的剪贴板图片历史。
///
/// 入参：`{ "limit": int }`（可选，默认 10）。
/// 出参：`Items`，每项 data 含 `{id, width, height, source_app, source_path, created_at}`，
/// desc 为 `{width}x{height}` 或 source_app。
pub struct ListClipboardImages;

#[async_trait::async_trait]
impl Capability for ListClipboardImages {
    fn id(&self) -> &str {
        "list_clipboard_images"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "list_clipboard_images".into(),
            description: "列出最近的剪贴板图片历史。返回每张图片的 id、尺寸、来源应用和创建时间。AI 可据此用 read_clipboard_history_image 按 id 获取完整图片。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "limit": {
                        "type": "integer",
                        "description": "最大返回数（可选，默认 10）",
                        "default": 10
                    }
                }
            }),
            sensitive: true, // 读剪贴板图片历史属隐私敏感数据
        }
    }


    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::NONE,
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
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "list_clipboard_images 截止时刻已过".into(),
            });
        }

        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|n| n as i64)
            .unwrap_or(10)
            .clamp(1, 100);

        let pool = ctx.env.db_pools().cache.clone();

        // 铁则 1：用 deadline 包裹 DB 查询
        let items = tokio::time::timeout_at(
            ctx.deadline_or_far_future(),
            clipboard_images::query_recent_image_list(&pool, limit),
        )
        .await
        .map_err(|_| CapabilityError::Timeout {
            detail: "list_clipboard_images 超时".into(),
        })?;

        let results: Vec<ItemResult> = items
            .into_iter()
            .map(|item| {
                let data = json!({
                    "id": item.id,
                    "width": item.width,
                    "height": item.height,
                    "source_app": item.source_app,
                    "source_path": item.source_path,
                    "created_at": item.created_at,
                });
                // desc: 优先用 source_app，否则用尺寸
                let desc = item
                    .source_app
                    .as_ref()
                    .filter(|s| !s.is_empty())
                    .cloned()
                    .unwrap_or_else(|| format!("{}x{}", item.width, item.height));
                ItemResult {
                    data,
                    desc: Some(desc),
                    actions: vec![], // 感知能力，无直接操作
                }
            })
            .collect();

        tracing::debug!(count = results.len(), "list_clipboard_images 完成");
        Ok(CapabilityResult::Items { items: results })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ListClipboardImages) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_list_clipboard_images() {
        assert_eq!(ListClipboardImages.id(), "list_clipboard_images");
    }

    #[test]
    fn schema_limit_is_optional() {
        let s = ListClipboardImages.schema();
        // limit 可选，故无 required 数组
        assert!(s.parameters.get("required").is_none(), "limit 应可选");
        assert_eq!(s.parameters["properties"]["limit"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["limit"]["default"], 10);
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ListClipboardImages.schema();
        assert!(s.sensitive, "list_clipboard_images 必须 sensitive=true");
    }
}
