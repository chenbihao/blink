//! `read_clipboard_history_image` Capability（0.19.1）。
//!
//! 按 id 从剪贴板图片历史读取完整 PNG → `Blob{image/png}`。
//!
//! **与 `list_clipboard_images` 的配合**：AI 先调 `list_clipboard_images` 拿到
//! 图片元数据列表（含 id），再调本 cap 按 id 取完整图片字节。
//! 图片历史存储在 cache 库（`clipboard_images` 表），与文本历史独立。
//!
//! **sensitive=true**：读剪贴板图片历史属隐私敏感数据。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

/// `read_clipboard_history_image` — 按 id 读取剪贴板历史图片的完整 PNG。
///
/// 入参：`{ "id": String }`。
/// 出参：`Blob { mime: "image/png", bytes }`（找到图片）；
///       `Done { summary: "无此图片" }`（id 不存在时）。
pub struct ReadClipboardHistoryImage;

#[async_trait::async_trait]
impl Capability for ReadClipboardHistoryImage {
    fn id(&self) -> &str {
        "read_clipboard_history_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "read_clipboard_history_image".into(),
            description: "按 id 从剪贴板图片历史读取完整 PNG 图片。先用 list_clipboard_images 获取图片列表和 id，再用本工具按 id 取图片。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "剪贴板图片历史记录的 id（来自 list_clipboard_images 返回的 id 字段）"
                    }
                },
                "required": ["id"]
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
                detail: "read_clipboard_history_image 截止时刻已过".into(),
            });
        }

        let id =
            args.get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 id 参数".into(),
                })?;

        let pool = ctx.env.db_pools().cache.clone();

        // 铁则 1：用 deadline 包裹 DB 查询
        let png = tokio::time::timeout_at(
            ctx.deadline_or_far_future(),
            crate::domain::clipboard::load_history_png(&pool, id),
        )
        .await
        .map_err(|_| CapabilityError::Timeout {
            detail: format!("read_clipboard_history_image 超时（id: {id}）"),
        })?;

        match png {
            Ok(bytes) => {
                tracing::debug!(id = %id, bytes = bytes.len(), "read_clipboard_history_image: 找到图片");
                Ok(CapabilityResult::Blob {
                    mime: "image/png".into(),
                    bytes,
                    desc: None,
                })
            }
            Err(crate::domain::clipboard::ClipboardError::ImageNotFound { .. }) => {
                tracing::debug!(id = %id, "read_clipboard_history_image: 无此图片");
                Ok(CapabilityResult::Done {
                    summary: "无此图片".into(),
                })
            }
            Err(error) => Err(CapabilityError::Internal {
                detail: error.to_string(),
            }),
        }
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ReadClipboardHistoryImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_read_clipboard_history_image() {
        assert_eq!(
            ReadClipboardHistoryImage.id(),
            "read_clipboard_history_image"
        );
    }

    #[test]
    fn schema_requires_id() {
        let s = ReadClipboardHistoryImage.schema();
        assert_eq!(s.parameters["required"].as_array().unwrap()[0], "id");
        assert_eq!(s.parameters["properties"]["id"]["type"], "string");
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ReadClipboardHistoryImage.schema();
        assert!(
            s.sensitive,
            "read_clipboard_history_image 必须 sensitive=true"
        );
    }
}
