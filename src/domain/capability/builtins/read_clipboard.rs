//! `read_clipboard` Capability（0.9.7 Step 2）。
//!
//! 读当前剪贴板 → `Text`（文本）或 `Blob{png}`（图片，暂不支持）。
//!
//! 当前实现：只读文本（CF_UNICODETEXT）。图片剪贴板读取留后续——现状截图写的是
//! CF_DIB，读回来需要 DIB→PNG 转换，0.9.7 暂不做（read 剪贴板图的使用场景稀少）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `read_clipboard` — 读当前剪贴板内容。
///
/// 入参：`{}`（无参）。
/// 出参：`Text { content }`（文本剪贴板）；空剪贴板返回 `Text { content: "" }`。
pub struct ReadClipboard;

#[async_trait::async_trait]
impl Capability for ReadClipboard {
    fn id(&self) -> &str {
        "read_clipboard"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "read_clipboard".into(),
            description: "读取当前系统剪贴板的文本内容。".into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let text =
            tokio::task::spawn_blocking(|| crate::infra::platform::clipboard::read_current_text())
                .await
                .map_err(|e| CapabilityError::Internal {
                    detail: format!("read_clipboard task 崩溃: {e}"),
                })?;

        let content = text.unwrap_or_default();
        if content.is_empty() {
            tracing::debug!("read_clipboard: 剪贴板为空或非文本");
        } else {
            tracing::debug!(len = content.chars().count(), "read_clipboard: 读到文本");
        }
        Ok(CapabilityResult::Text { content, desc: None })
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
}
