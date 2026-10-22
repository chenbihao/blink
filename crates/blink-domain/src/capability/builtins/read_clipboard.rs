//! `read_clipboard` Capability（0.9.7 Step 2, 0.19.1 图片分支，0.27.2 文本护栏）。
//!
//! 读当前剪贴板 → `Text`（文本）或 `Blob{png}`（图片）。
//!
//! **0.19.1**：先试 CF_DIB（图片），有则返回 `Blob{image/png}`；
//! 无则 fallback 读 CF_UNICODETEXT（文本），返回 `Text`。
//! 图片"获取"与"识别/消费"正交分离——本 cap 只负责获取图片字节，
//! OCR/翻译/pin 等消费由 AI 组合其他 cap 完成。
//!
//! **0.27.2 文本护栏**：文本估算 ≤4000 tokens 原样返回；超过则返回 ≤2000 tokens 的
//! UTF-8 安全前缀，并在 content 尾部附结构化截断说明（原估算/实际范围/指引）。
//! 首版不提供剪贴板分页——如需更多内容请保存为文件后用 `read_text_file` 分页读取。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::ai::token_budget::{estimate_text_tokens, utf8_safe_token_prefix};
use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

/// 剪贴板文本完整返回的估算 token 上限。
const CLIPBOARD_FULL_TOKEN_LIMIT: usize = 4_000;
/// 超限时截断前缀的估算 token 上限。
const CLIPBOARD_PREFIX_TOKEN_LIMIT: usize = 2_000;

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
            description:
                "读取当前系统剪贴板内容。如果剪贴板包含图片则返回图片（PNG 字节），否则返回文本。"
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": {}
            }),
            sensitive: true, // 读剪贴板属隐私敏感数据（0.19.4 补齐）
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
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let result = crate::clipboard::read_current()
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        match result {
            crate::clipboard::ClipboardContent::ImagePng(png) => {
                tracing::debug!(bytes = png.len(), "read_clipboard: 读到图片");
                Ok(CapabilityResult::Blob {
                    mime: "image/png".into(),
                    bytes: png,
                    desc: None,
                })
            }
            crate::clipboard::ClipboardContent::Text(content) => {
                if content.is_empty() {
                    tracing::debug!("read_clipboard: 剪贴板为空");
                } else {
                    tracing::debug!(len = content.chars().count(), "read_clipboard: 读到文本");
                }
                let content = clamp_clipboard_text(content);
                Ok(CapabilityResult::Text {
                    content,
                    desc: None,
                })
            }
        }
    }
}

/// 文本护栏纯函数（0.27.2）：≤4000 tokens 原样返回；超过则截 ≤2000 tokens 前缀并附说明。
///
/// 说明写进 content 而非 `desc`——`desc` 是展示层副标题，AI 不读（CapabilityResult 契约）。
/// 说明含原估算 tokens / 返回字符范围 / 后续指引；不虚构分页接口。
pub(crate) fn clamp_clipboard_text(content: String) -> String {
    let full_tokens = estimate_text_tokens(&content);
    if full_tokens <= CLIPBOARD_FULL_TOKEN_LIMIT {
        return content;
    }
    let prefix = utf8_safe_token_prefix(&content, CLIPBOARD_PREFIX_TOKEN_LIMIT);
    let total_chars = content.chars().count();
    let prefix_chars = prefix.chars().count();
    tracing::debug!(
        full_tokens,
        prefix_tokens = estimate_text_tokens(&prefix),
        prefix_chars,
        total_chars,
        "read_clipboard: 文本超限，返回安全前缀"
    );
    format!(
        "{prefix}\n\n[剪贴板文本共约 {full_tokens} tokens，超过 {CLIPBOARD_FULL_TOKEN_LIMIT} 上限，\
         已截断为前 {prefix_chars}/{total_chars} 字符（约 {CLIPBOARD_PREFIX_TOKEN_LIMIT} tokens）。\
         如需完整内容，请将文本保存为文件后用 read_text_file 分页读取。]"
    )
}

inventory::submit!(crate::capability::CapabilityEntry {
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

    // ── 0.27.2 文本护栏（clamp_clipboard_text）──

    /// 短文本原样返回，无任何附加说明。
    #[test]
    fn clamp_short_text_untouched() {
        let out = clamp_clipboard_text("hello 剪贴板".to_string());
        assert_eq!(out, "hello 剪贴板");
    }

    /// 恰好在 4000 tokens 边界的 CJK 文本原样返回。
    #[test]
    fn clamp_exactly_at_limit_untouched() {
        let text = "字".repeat(4_000); // 4000 CJK = 4000 tokens
        let out = clamp_clipboard_text(text);
        assert_eq!(out, "字".repeat(4_000));
    }

    /// 超限文本：前缀 ≤2000 tokens、UTF-8 完整、说明含实际范围与指引。
    #[test]
    fn clamp_over_limit_returns_prefix_and_note() {
        let text = "字".repeat(9_000); // 9000 tokens > 4000
        let out = clamp_clipboard_text(text);
        assert!(out.starts_with(&"字".repeat(2_000)), "前缀应为前 2000 个 CJK 字");
        assert!(out.contains("9000 tokens"), "说明应含原估算 tokens");
        assert!(out.contains("2000/9000 字符"), "说明应含实际字符范围");
        assert!(out.contains("read_text_file"), "说明应指引分页读取而非虚构续读接口");
        assert!(!out.contains("继续读取"), "不得虚构分页调用");
    }

    /// ASCII 超限文本同样触发护栏（成本模型 4 char/token）。
    #[test]
    fn clamp_over_limit_ascii() {
        let text = "a".repeat(20_000); // 5000 tokens
        let out = clamp_clipboard_text(text.clone());
        assert!(out.contains("5000 tokens"));
        // 前缀 2000 tokens = 8000 chars；断言首段不是全文
        assert_ne!(out, text);
        assert!(out.len() < text.len() + 200);
    }
}
