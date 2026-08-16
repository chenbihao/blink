//! `ocr_image` Capability（0.11.7-c，0.11.7-f 走 backend 注入）。
//!
//! 接收 PNG 字节，返回 OCR 识别结果（文本 + 行级坐标）。
//! 通过 `ocr_engine::backend()` 拿注入的 backend，可测试替换（`install_backend`）。

use std::sync::Arc;

use serde_json::{Value, json};

use super::image_input::resolve_png_input;
use super::ocr_engine::{OcrResult, backend};
use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
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
            description: "识别图片中的文字，返回识别文本和每行文字的位置坐标。支持中文和英文。图片来源：image_ref（来自截图/剪贴板等能力返回的引用）或 png（原始 PNG 字节数组），二选一。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "image_ref": {
                        "type": "string",
                        "description": "图片引用（来自 read_clipboard/screenshot 等能力返回的 image_ref，与 png 二选一）"
                    },
                    "png": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "PNG 图片字节数组（与 image_ref 二选一）"
                    }
                }
            }),
            sensitive: true, // 0.21.1 §4.1b：识图输出用户内容，对齐 analyze_image_palette
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
        // 提取 PNG 字节：image_ref 或 png 二选一（0.19.4）
        let stash = ctx.env.image_stash();
        let png_bytes = resolve_png_input(&args, stash.map(|s| s.as_ref()), "png")?;

        // 调用注入的 OCR backend
        let b = backend();
        let result = b
            .recognize(&png_bytes)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Text {
            content: serde_json::to_string(&result as &OcrResult)
                .unwrap_or_else(|_| result.text.clone()),
            desc: None,
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OcrImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::super::ocr_engine::{FakeOcrBackend, install_backend};
    use super::*;

    #[test]
    fn id_is_ocr_image() {
        assert_eq!(OcrImage.id(), "ocr_image");
    }

    #[test]
    fn schema_has_png_and_image_ref_params() {
        let s = OcrImage.schema();
        assert_eq!(s.name, "ocr_image");
        assert_eq!(s.parameters["properties"]["png"]["type"], "array");
        assert_eq!(s.parameters["properties"]["image_ref"]["type"], "string");
        // 0.19.4: png 不再 required，与 image_ref 二选一
        let required = s.parameters.get("required");
        assert!(required.is_none() || required.unwrap().as_array().unwrap().is_empty());
    }

    /// Capability 通过 backend() 拿注入的 FakeOcrBackend。
    /// 用一个 minimal PNG 字节序列（PNG 魔数 + 简单 header）绕过 empty check。
    #[tokio::test]
    async fn uses_injected_backend_for_recognition() {
        install_backend(Arc::new(FakeOcrBackend::returning("injected-fake-text")));

        // minimal 8x8 PNG magic + IHDR + IEND（不严格合法但足够绕过 empty check；
        // FakeBackend 不解码只返回预设，PNG 内容不重要）
        let fake_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        let _args = json!({
            "png": fake_png.iter().map(|b| json!(b)).collect::<Vec<_>>()
        });

        // 无法直接构造 InvokeContext（需要 AppHandle），跳过 Capability::invoke 完整链路，
        // 直接测 backend 注入语义（本文件核心逻辑就是 backend() 调用 + 参数解析）。
        let b = super::super::ocr_engine::backend();
        let result = b.recognize(&fake_png).await.unwrap();
        assert_eq!(result.text, "injected-fake-text");
    }
}
