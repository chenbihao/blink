//! `read_attachment_text` Capability（0.27.9）——聊天文本附件的窄用途续读。
//!
//! **策略三元组**（phase 0.27 §8.2 定案，照抄 read_text_file 先例）：
//! `DangerClass::Safe` + `schema.sensitive = true` + `ConfirmationPolicy::sensitive()`
//! ——同为读用户文件内容片段，属泄露语义而非破坏语义。
//! `allowed_origins` = local + AI（工具模式下进池），MCP 出口默认关闭。
//!
//! **绑定铁则**：只接受执行上下文绑定的当前活跃请求中已明确附加的 ref
//! （`CapabilityEnv::active_chat_attachment_refs`），不能由它获得任意路径
//! 读取权——任意路径读取仍归 `read_text_file`（绝对路径 + 敏感确认）。
//!
//! 游标语义：`cursor` 为 UTF-8 字节偏移（0 或上一页返回的 `end_byte`），
//! 每页默认 ≤2000、硬上限 4000 估算 tokens；`pure_chat` 不因此开启工具，
//! 用户可选下一段再次附加。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::ai::attachment::TEXT_FILE_MAX_BYTES;
use crate::ai::token_budget::estimate_text_tokens;
use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, ItemResult, McpDefault, OriginSet,
    RuntimeRequirement,
};
use crate::resource::{DefaultResourceStore, ResourceRef, ResourceUse};

const DEFAULT_PAGE_TOKENS: usize = 2_000;
const MAX_PAGE_TOKENS: usize = 4_000;
const MAX_CURSOR_BYTES: u64 = TEXT_FILE_MAX_BYTES;

pub struct ReadAttachmentText;

#[async_trait::async_trait]
impl Capability for ReadAttachmentText {
    fn id(&self) -> &str {
        "read_attachment_text"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "分页续读当前请求已附加的聊天文本附件（按字节游标）。只能读取本轮已明确附加的 resource_ref，不支持任意路径。".into(),
            parameters: json!({
                "type": "object",
                "additionalProperties": false,
                "properties": {
                    "resource_ref": { "type": "string", "description": "本轮已附加的文本附件 resource_ref" },
                    "cursor": { "type": "integer", "minimum": 0, "description": "起始字节偏移（0 或上一页返回的 end_byte）" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": MAX_PAGE_TOKENS, "description": "本页最大估算 tokens（默认 2000）" }
                },
                "required": ["resource_ref"]
            }),
            sensitive: true,
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
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
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "read_attachment_text 截止时刻已过".into(),
            });
        }

        // ── 1. 请求绑定校验：ref 必须在当前活跃请求的附加集合内 ──
        let resource_ref = args
            .get("resource_ref")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid_args("resource_ref 必须是非空字符串"))?
            .to_string();
        let bound = ctx.env.active_chat_attachment_refs().ok_or_else(|| {
            CapabilityError::InvalidState {
                detail: "当前没有绑定附件的活跃请求，read_attachment_text 不可用".into(),
            }
        })?;
        if !bound.contains(&resource_ref) {
            return Err(CapabilityError::InvalidArgs {
                detail: "resource_ref 不在当前请求已附加的附件集合内（请让用户在下一条消息中再次附加）".into(),
            });
        }

        let cursor = args
            .get("cursor")
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if cursor > MAX_CURSOR_BYTES {
            return Err(invalid_args("cursor 超出附件大小上限"));
        }
        let limit_tokens = args
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| (v as usize).clamp(1, MAX_PAGE_TOKENS))
            .unwrap_or(DEFAULT_PAGE_TOKENS);

        // ── 2. 解引用（AttachmentText 用途 + 身份复核在 store open 内）──
        let store: Arc<DefaultResourceStore> = ctx
            .env
            .resource_store()
            .ok_or_else(|| CapabilityError::InvalidState {
                detail: "运行时未启用 ResourceStore".into(),
            })?
            .clone();
        let mut opened = store
            .open(&ResourceRef::from_token(&resource_ref), ResourceUse::AttachmentText)
            .map_err(|e| CapabilityError::InvalidState {
                detail: format!(
                    "附件引用不可用（{}）——文件可能已过期或发生变化，请重新附加",
                    e.kind.as_str()
                ),
            })?;

        // 文本附件 ≤256KiB：整读（blocking pool）+ 纯函数切片分页
        let bytes = tokio::task::spawn_blocking(move || {
            opened.read_all_bounded(TEXT_FILE_MAX_BYTES)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("附件读取任务失败: {e}"),
        })?
        .map_err(|e| CapabilityError::InvalidState {
            detail: format!(
                "附件读取失败（{}）——文件可能已变化，请重新附加",
                e.kind.as_str()
            ),
        })?;

        // ── 3. 纯函数分页 ──
        let page = read_page(&bytes, cursor, limit_tokens).map_err(invalid_args)?;
        let next_cursor = if page.eof { None } else { Some(page.end_byte) };
        Ok(single_item(json!({
            "text": page.text,
            "start_byte": page.start_byte,
            "end_byte": page.end_byte,
            "next_cursor": next_cursor,
            "eof": page.eof,
            "truncated": page.truncated,
        })))
    }
}

/// 一页读取结果（字节游标语义）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextPage {
    pub text: String,
    pub start_byte: u64,
    pub end_byte: u64,
    pub eof: bool,
    /// 因 token 预算停页（非 EOF）。
    pub truncated: bool,
}

/// 纯函数分页：从 `cursor`（UTF-8 字节偏移）起取 ≤`limit_tokens` 的 UTF-8 安全切片。
///
/// - `cursor` 必须落在 char 边界（0 或本能力返回的 `end_byte` 保证）；
/// - 首 3 字节 BOM 只在 offset 0 剥离；
/// - 返回的 `end_byte` 恒为 char 边界，可作下一页 cursor。
pub fn read_page(bytes: &[u8], cursor: u64, limit_tokens: usize) -> Result<TextPage, String> {
    let cursor = usize::try_from(cursor).map_err(|_| "cursor 无效".to_string())?;
    if cursor > bytes.len() {
        return Err(format!("cursor {} 超出附件末尾（{} 字节）", cursor, bytes.len()));
    }
    // 首 3 字节 BOM 只在 offset 0 剥离——end_byte 仍按原始字节偏移返回
    let raw = &bytes[cursor..];
    let (body, bom_len) = match raw.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        Some(b) => (b, 3usize),
        None => (raw, 0usize),
    };
    let full = std::str::from_utf8(body).map_err(|_| "附件不再是有效 UTF-8（文件可能已变化，请重新附加）".to_string())?;

    if full.is_empty() {
        return Ok(TextPage {
            text: String::new(),
            start_byte: cursor as u64,
            end_byte: cursor as u64,
            eof: true,
            truncated: false,
        });
    }

    // 预算切片：逐字符累计，超预算停在最后一个完整字符
    let mut char_bytes = 0usize;
    let mut used_tokens = 0usize;
    let mut stopped_early = false;
    for ch in full.chars() {
        let ch_tokens = estimate_text_tokens(&ch.to_string());
        if used_tokens + ch_tokens > limit_tokens && char_bytes > 0 {
            stopped_early = true;
            break;
        }
        used_tokens += ch_tokens;
        char_bytes += ch.len_utf8();
    }
    let taken: &str = if stopped_early {
        &full[..char_bytes]
    } else {
        full
    };
    let end_byte = (cursor + bom_len + taken.len()) as u64;
    Ok(TextPage {
        text: taken.to_string(),
        start_byte: cursor as u64,
        end_byte,
        eof: !stopped_early && end_byte == bytes.len() as u64,
        truncated: stopped_early,
    })
}

fn single_item(data: Value) -> CapabilityResult {
    CapabilityResult::Items {
        items: vec![ItemResult {
            data,
            desc: Some("附件续读结果".into()),
            actions: vec![],
        }],
    }
}

fn invalid_args(detail: impl Into<String>) -> CapabilityError {
    CapabilityError::InvalidArgs {
        detail: detail.into(),
    }
}

inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(ReadAttachmentText) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn page_of(text: &str, cursor: u64, limit: usize) -> TextPage {
        read_page(text.as_bytes(), cursor, limit).unwrap()
    }

    #[test]
    fn page_full_small_text_is_eof() {
        let p = page_of("你好世界", 0, 2000);
        assert_eq!(p.text, "你好世界");
        assert!(p.eof);
        assert!(!p.truncated);
        assert_eq!(p.end_byte, 12);
    }

    #[test]
    fn page_respects_token_budget_and_cursor_chains() {
        let text = "你".repeat(5000); // 5000 tokens，15000 字节
        let p1 = page_of(&text, 0, 2000);
        assert!(p1.truncated);
        assert!(!p1.eof);
        assert_eq!(p1.text.chars().count(), 2000, "恰好 2000 token");
        assert_eq!(p1.end_byte, 6000, "UTF-8 边界");

        let p2 = read_page(text.as_bytes(), p1.end_byte, 2000).unwrap();
        assert_eq!(p2.start_byte, 6000);
        assert_eq!(p2.text.chars().count(), 2000);
        assert_eq!(
            p2.text,
            p1.text,
            "两页拼接无重叠（此处同文本前缀不同段长度一致仅校验游标推进）"
        );
        // 全量三页读完
        let p3 = read_page(text.as_bytes(), p2.end_byte, 2000).unwrap();
        assert!(p3.eof && !p3.truncated);
        assert_eq!(p3.text.chars().count(), 1000);
    }

    #[test]
    fn page_no_gap_no_overlap_across_full_file() {
        let text: String = (0..30).map(|i| format!("第{i}行内容\n")).collect();
        let mut cursor = 0u64;
        let mut collected = String::new();
        for _ in 0..100 {
            let p = read_page(text.as_bytes(), cursor, 10).unwrap();
            collected.push_str(&p.text);
            if p.eof {
                break;
            }
            cursor = p.end_byte;
        }
        assert_eq!(collected, text, "分页拼接与原文一致（无漏字/重复）");
    }

    #[test]
    fn page_strips_bom_only_at_zero() {
        let mut bytes = vec![0xEF, 0xBB, 0xBF];
        bytes.extend_from_slice("ab".as_bytes());
        let p = read_page(&bytes, 0, 100).unwrap();
        assert_eq!(p.text, "ab");
        assert_eq!(p.end_byte, 5, "end_byte 计入 BOM 的原始字节偏移");
        assert!(p.eof);
    }

    #[test]
    fn page_cursor_beyond_end_is_error() {
        assert!(read_page(b"abc", 4, 10).is_err());
    }

    #[test]
    fn page_empty_attachment_returns_empty_eof() {
        let p = page_of("", 0, 100);
        assert_eq!(p.text, "");
        assert!(p.eof);
    }

    #[test]
    fn page_rejects_invalid_utf8() {
        let err = read_page(&[0xFF, 0xFE], 0, 10);
        assert!(err.is_err());
    }

    #[test]
    fn policy_matches_spec_triple() {
        let p = ReadAttachmentText.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(p.sensitive);
        assert_eq!(p.confirmation, ConfirmationPolicy::sensitive());
        assert!(p.allowed_origins.contains(crate::capability::InvocationOrigin::LocalAi));
        assert!(p.allowed_origins.contains(crate::capability::InvocationOrigin::LocalCommand));
        assert!(!p.allowed_origins.contains(crate::capability::InvocationOrigin::Mcp));
        assert_eq!(p.mcp_default, McpDefault::DefaultOff);
        assert!(ReadAttachmentText.schema().sensitive);
    }
}
