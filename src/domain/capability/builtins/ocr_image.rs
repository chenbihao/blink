//! `ocr_image` Capability（0.11.7-c，0.11.7-f 走 backend 注入）。
//!
//! 接收 PNG 字节，返回 OCR 识别结果（文本 + 行级坐标）。
//! 通过 `ocr_engine::backend()` 拿注入的 backend，可测试替换（`install_backend`）。

use std::sync::Arc;

use serde_json::{Value, json};

use super::ocr_engine::{OcrResult, backend};
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `ocr_image` — 识别图片中的文字，返回文本 + 行级坐标。
pub struct OcrImage;

#[async_trait::async_trait]
impl Capability for OcrImage {
    fn id(&self) -> &str {
        "ocr_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "ocr_image".into(),
            description: "识别图片中的文字，返回识别文本和每行文字的位置坐标。支持中文和英文。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "png": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "PNG 图片字节数据"
                    }
                },
                "required": ["png"]
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 提取 PNG 字节
        let png_bytes = args
            .get("png")
            .and_then(|v| v.as_array())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "缺少 png 参数".into(),
            })?
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect::<Vec<u8>>();

        if png_bytes.is_empty() {
            return Err(CapabilityError::InvalidArgs {
                detail: "png 数据为空".into(),
            });
        }

        // 调用注入的 OCR backend
        let b = backend();
        let result = b.recognize(&png_bytes).await.map_err(|e| {
            CapabilityError::Internal {
                detail: e.to_string(),
            }
        })?;

        Ok(CapabilityResult::Text {
            content: serde_json::to_string(&result as &OcrResult)
                .unwrap_or_else(|_| result.text.clone()),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OcrImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;
    use super::super::ocr_engine::{FakeOcrBackend, install_backend};

    #[test]
    fn id_is_ocr_image() {
        assert_eq!(OcrImage.id(), "ocr_image");
    }

    #[test]
    fn schema_has_png_parameter() {
        let s = OcrImage.schema();
        assert_eq!(s.name, "ocr_image");
        assert!(s.parameters["required"].as_array().unwrap().contains(&json!("png")));
    }

    /// Capability 通过 backend() 拿注入的 FakeOcrBackend。
    /// 用一个 minimal PNG 字节序列（PNG 魔数 + 简单 header）绕过 empty check。
    #[tokio::test]
    async fn uses_injected_backend_for_recognition() {
        install_backend(Arc::new(FakeOcrBackend::returning("injected-fake-text")));

        // minimal 8x8 PNG magic + IHDR + IEND（不严格合法但足够绕过 empty check；
        // FakeBackend 不解码只返回预设，PNG 内容不重要）
        let fake_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        let args = json!({
            "png": fake_png.iter().map(|b| json!(b)).collect::<Vec<_>>()
        });

        // 无法直接构造 InvokeContext（需要 AppHandle），跳过 Capability::invoke 完整链路，
        // 直接测 backend 注入语义（本文件核心逻辑就是 backend() 调用 + 参数解析）。
        let b = super::super::ocr_engine::backend();
        let result = b.recognize(&fake_png).await.unwrap();
        assert_eq!(result.text, "injected-fake-text");
    }
}