//! `read_clipboard` Capability（0.9.7 Step 2, 0.19.1 图片分支）。
//!
//! 读当前剪贴板 → `Text`（文本）或 `Blob{png}`（图片）。
//!
//! **0.19.1**：先试 CF_DIB（图片），有则返回 `Blob{image/png}`；
//! 无则 fallback 读 CF_UNICODETEXT（文本），返回 `Text`。
//! 图片"获取"与"识别/消费"正交分离——本 cap 只负责获取图片字节，
//! OCR/翻译/pin 等消费由 AI 组合其他 cap 完成。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `read_clipboard` — 读当前剪贴板内容（文本或图片）。
///
/// 入参：`{}`（无参）。
/// 出参：`Blob { mime: "image/png", bytes }`（图片剪贴板）；
///       `Text { content }`（文本剪贴板）；空剪贴板返回 `Text { content: "" }`。
pub struct ReadClipboard;

#[async_trait::async_trait]
impl Capability for ReadClipboard {
    fn id(&self) -> &str {
        "read_clipboard"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "read_clipboard".into(),
            description: "读取当前系统剪贴板内容。如果剪贴板包含图片则返回图片（PNG 字节），否则返回文本。".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            sensitive: true, // 读剪贴板属隐私敏感数据（0.19.4 补齐）
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let result = crate::domain::clipboard::read_current()
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        match result {
            crate::domain::clipboard::ClipboardContent::ImagePng(png) => {
                tracing::debug!(bytes = png.len(), "read_clipboard: 读到图片");
                Ok(CapabilityResult::Blob {
                    mime: "image/png".into(),
                    bytes: png,
                    desc: None,
                })
            }
            crate::domain::clipboard::ClipboardContent::Text(content) => {
                if content.is_empty() {
                    tracing::debug!("read_clipboard: 剪贴板为空");
                } else {
                    tracing::debug!(len = content.chars().count(), "read_clipboard: 读到文本");
                }
                Ok(CapabilityResult::Text { content, desc: None })
            }
        }
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(ReadClipboard) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_read_clipboard() {
        assert_eq!(ReadClipboard.id(), "read_clipboard");
    }

    #[test]
    fn schema_has_no_parameters() {
        let s = ReadClipboard.schema();
        assert_eq!(s.parameters["type"], "object");
        // 无 properties（空 object）
        assert!(s.parameters["properties"].as_object().unwrap().is_empty());
    }

    #[test]
    fn schema_description_mentions_image() {
        let s = ReadClipboard.schema();
        assert!(
            s.description.contains("图片"),
            "schema description 应提及图片"
        );
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = ReadClipboard.schema();
        assert!(
            s.sensitive,
            "read_clipboard 必须 sensitive=true（读取用户剪贴板属隐私数据）"
        );
    }
}
