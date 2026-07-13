//! `write_clipboard` Capability（0.9.7 Step 2）。
//!
//! 写入剪贴板 → `Done`。图/文双模式。
//!
//! - `text` 模式：写 CF_UNICODETEXT（新写函数 `write_text_to_clipboard`）
//! - `image` 模式：写 CF_DIB（复用 `write_bgra_to_clipboard`，需 width/height）
//!
//! 这是截图链路的编排终点（capture_screen → crop_image → write_clipboard），
//! 也是 AI "把结果写到剪贴板" 的通用出口。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

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
            description: "写入系统剪贴板。支持文本（text）或图片（image_bytes + width + height）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "text": {
                        "type": "string",
                        "description": "要写入的文本内容（与 image_bytes 二选一）"
                    },
                    "image_bytes": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "BGRA 像素字节数组（与 text 二选一，需同时给 width/height）"
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
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 优先 text 模式（clone 成 String——spawn_blocking 要求 'static）
        if let Some(text) = args.get("text").and_then(Value::as_str).map(str::to_string) {
            let len = text.chars().count();
            tokio::task::spawn_blocking(move || {
                crate::infra::platform::clipboard::write_text_to_clipboard(&text)
            })
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("write_clipboard task 崩溃: {e}"),
            })?
            .map_err(|e| CapabilityError::Internal { detail: e })?;

            return Ok(CapabilityResult::Done {
                summary: format!("已写入文本（{len} 字）"),
            });
        }

        // image_bytes 模式
        let width = args
            .get("width")
            .and_then(Value::as_u64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "image 模式缺少 width".into(),
            })? as u32;
        let height = args
            .get("height")
            .and_then(Value::as_u64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "image 模式缺少 height".into(),
            })? as u32;
        let image_bytes = args
            .get("image_bytes")
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "image 模式缺少 image_bytes".into(),
            })?;

        // JSON array → Vec<u8>
        let pixels: Vec<u8> = image_bytes
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect();

        if pixels.len() != (width as usize) * (height as usize) * 4 {
            return Err(CapabilityError::InvalidArgs {
                detail: format!(
                    "像素长度不匹配: {} vs {} ({}x{}x4)",
                    pixels.len(),
                    (width as usize) * (height as usize) * 4,
                    width,
                    height
                ),
            });
        }

        tokio::task::spawn_blocking(move || {
            crate::infra::platform::clipboard::write_bgra_to_clipboard(&pixels, width, height)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("write_clipboard task 崩溃: {e}"),
        })?
        .map_err(|e| CapabilityError::Internal { detail: e })?;

        Ok(CapabilityResult::Done {
            summary: format!("已写入图片（{width}x{height}）"),
        })
    }
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
    }

    #[test]
    fn schema_description_mentions_both_modes() {
        let s = WriteClipboard.schema();
        assert!(s.description.contains("文本"));
        assert!(s.description.contains("图片"));
    }
}
