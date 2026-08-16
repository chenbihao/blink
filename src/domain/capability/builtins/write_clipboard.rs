//! `write_clipboard` Capability（0.9.7 Step 2）。
//!
//! 写入剪贴板 → `Done`。图/文双模式。
//!
//! - `text` 模式：写 CF_UNICODETEXT（新写函数 `write_text_to_clipboard`）
//! - `image` 模式：写 CF_DIB（复用 `write_bgra_to_clipboard`，需 width/height）
//!
//! 这是截图链路的编排终点（screenshot → write_clipboard），
//! 也是 AI "把结果写到剪贴板" 的通用出口。

use std::sync::Arc;

use serde_json::{Value, json};

use super::image_input::{parse_byte_array, resolve_image_ref};
use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use crate::domain::clipboard::ClipboardWriteSource;

/// `write_clipboard` — 写入系统剪贴板（文本或图片）。
///
/// 入参：
/// - `{ "text": "..." }` — 写文本
/// - `{ "image_bytes": [...], "width": int, "height": int }` — 写图片（BGRA 格式）
///
/// 出参：`Done { summary }`。
pub struct WriteClipboard;

#[async_trait::async_trait]
impl Capability for WriteClipboard {
    fn id(&self) -> &str {
        "write_clipboard"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "write_clipboard".into(),
            description: "写入系统剪贴板。支持文本（text）、图片引用（image_ref，来自截图/剪贴板等能力返回）或 BGRA 图片（image_bytes + width + height）。三种模式互斥。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "要写入的文本内容"
                    },
                    "image_ref": {
                        "type": "string",
                        "description": "图片引用（来自 read_clipboard/screenshot 等能力返回的 image_ref，写入为 PNG）"
                    },
                    "image_bytes": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "BGRA 像素字节数组（需同时给 width/height）"
                    },
                    "width": {
                        "type": "integer",
                        "description": "图片宽度（像素，image_bytes 模式必填）"
                    },
                    "height": {
                        "type": "integer",
                        "description": "图片高度（像素，image_bytes 模式必填）"
                    }
                }
            }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::NONE,
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
        // 优先 text 模式（clone 成 String——spawn_blocking 要求 'static）
        if let Some(text) = args.get("text").and_then(Value::as_str).map(str::to_string) {
            // text 与 image_ref/image_bytes 互斥
            if args.get("image_ref").is_some() || args.get("image_bytes").is_some() {
                return Err(CapabilityError::InvalidArgs {
                    detail: "text 与 image_ref/image_bytes 不能同时提供".into(),
                });
            }
            let len = text.chars().count();
            crate::domain::clipboard::write_text(text, ClipboardWriteSource::Capability)
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: e.to_string(),
                })?;

            return Ok(CapabilityResult::Done {
                summary: format!("已写入文本（{len} 字）"),
            });
        }

        // image_ref 模式（0.19.4）：从 stash 解析 PNG，用 write_png_to_clipboard
        if args.get("image_ref").is_some() {
            // image_ref 与 image_bytes 互斥
            if args.get("image_bytes").is_some() {
                return Err(CapabilityError::InvalidArgs {
                    detail: "image_ref 与 image_bytes 不能同时提供".into(),
                });
            }
            let png = resolve_image_ref(&args, ctx.env.image_stash().map(|stash| stash.as_ref()))?;
            crate::domain::clipboard::write_png(png, ClipboardWriteSource::Capability)
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: e.to_string(),
                })?;

            return Ok(CapabilityResult::Done {
                summary: "已写入图片".into(),
            });
        }

        // image_bytes 模式（BGRA）
        let width = parse_dimension(&args, "width")?;
        let height = parse_dimension(&args, "height")?;
        let pixels = parse_byte_array(&args, "image_bytes")?;

        crate::domain::clipboard::write_bgra(
            pixels,
            width,
            height,
            ClipboardWriteSource::Capability,
        )
        .await
        .map_err(|e| match e {
            crate::domain::clipboard::ClipboardError::PixelLengthMismatch { .. }
            | crate::domain::clipboard::ClipboardError::PixelSizeOverflow { .. } => {
                CapabilityError::InvalidArgs {
                    detail: e.to_string(),
                }
            }
            _ => CapabilityError::Internal {
                detail: e.to_string(),
            },
        })?;

        Ok(CapabilityResult::Done {
            summary: format!("已写入图片（{width}x{height}）"),
        })
    }
}

fn parse_dimension(args: &Value, key: &str) -> Result<u32, CapabilityError> {
    args.get(key)
        .and_then(Value::as_u64)
        .and_then(|value| u32::try_from(value).ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: format!("image 模式缺少或无效的 {key}"),
        })
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(WriteClipboard) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_write_clipboard() {
        assert_eq!(WriteClipboard.id(), "write_clipboard");
    }

    #[test]
    fn schema_supports_text_and_image() {
        let s = WriteClipboard.schema();
        assert!(s.parameters["properties"]["text"]["type"] == "string");
        assert!(s.parameters["properties"]["image_bytes"]["type"] == "array");
        assert!(s.parameters["properties"]["width"]["type"] == "integer");
        assert!(s.parameters["properties"]["image_ref"]["type"] == "string");
    }

    #[test]
    fn schema_description_mentions_both_modes() {
        let s = WriteClipboard.schema();
        assert!(s.description.contains("文本"));
        assert!(s.description.contains("图片"));
    }
}
