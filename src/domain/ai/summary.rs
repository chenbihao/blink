//! 0.21.19: LLM 滚动摘要——纯逻辑模块。
//!
//! 被裁消息在轮次结束后被后台压缩成摘要，下一轮以 `<summary>` 块注入窗口头部。
//! 水位线模型：每 conversation 维护 `summarized_until`（消息 rowid 水位），
//! 摘要任务覆盖「当前水位 → 本轮压缩边界」区间，生成一条 summary 段并推进水位；
//! 水位与摘要同事务落库，防半写。load 时 rowid ≤ 水位的消息不进窗口，改注入摘要块。
//!
//! **架构前提**：rig 的 memory load 发生在 `stream_prompt` 内部，load 热路径上不能
//! 嵌套 LLM 调用。因此摘要必须在轮次结束后预生成落库，load 只读已落库摘要；
//! 任何摘要失败都回退现行纯截断，永不禁塞 prompt 主链路。
//!
//! **段合并**：summary 段 ≥ 3 条时合并最旧两段（走 LLM，输入为两段摘要文本）。
//! 不做无限滚动单摘要。

use crate::domain::ai::memory::estimate_tokens;

/// 摘要块在窗口中的注入格式。
///
/// 位于 `<memory>` 块之前；token 计入 history，`estimate_tokens` 估算。
pub fn format_summary_block(summaries: &[String]) -> String {
    if summaries.is_empty() {
        return String::new();
    }
    let mut block = String::from("<summary>\n");
    for (i, s) in summaries.iter().enumerate() {
        if i > 0 {
            block.push_str("\n---\n");
        }
        block.push_str(s);
        block.push('\n');
    }
    block.push_str("</summary>");
    block
}

/// 估算摘要块的总 token 数。
pub fn estimate_summary_block_tokens(summaries: &[String]) -> usize {
    if summaries.is_empty() {
        return 0;
    }
    let block = format_summary_block(summaries);
    estimate_tokens(&block)
}

/// 判断是否应触发段合并（summary 段 ≥ 3 条）。
pub fn should_merge_summaries(summary_count: usize) -> bool {
    summary_count >= 3
}

/// 计算本轮摘要任务应覆盖的消息区间。
///
/// 返回 `(start_rowid, end_rowid)`——当前水位 + 1 到本轮裁剪边界。
/// `watermark` 是已摘要到的 rowid；`compress_boundary` 是本轮被裁剪的最后一条消息 rowid。
/// 如果 `compress_boundary <= watermark` 表示没有新消息需要摘要，返回 None。
pub fn compute_summary_range(watermark: i64, compress_boundary: i64) -> Option<(i64, i64)> {
    if compress_boundary <= watermark {
        return None;
    }
    Some((watermark + 1, compress_boundary))
}

/// 计算下一个摘要段的 idx。
///
/// 当前段数即为下一个 idx（0-based，顺序递增）。
pub fn next_summary_idx(current_count: i64) -> i64 {
    current_count.max(0)
}

/// 段合并 prompt 构造。
///
/// 输入为两段摘要文本，输出为合并后的摘要。prompt 要求合并保留两段的关键信息。
pub fn build_merge_prompt(summary1: &str, summary2: &str) -> String {
    format!(
        "以下是两段对话摘要，请将它们合并为一段连贯的摘要。\
         保留所有关键信息：用户目标与偏好、决策及理由、关键路径/ID/命令、\
         工具调用结论、未决问题。删除重复内容，保持简洁。\n\n\
         --- 段1 ---\n{summary1}\n\n--- 段2 ---\n{summary2}\n\n\
         请直接输出合并后的摘要，不要包含任何前言或解释。"
    )
}

/// 摘要 prompt 模板。
///
/// 被裁消息的文本会拼接后注入此 prompt，要求模型压缩为摘要。
/// 保留：用户目标与偏好、决策及理由、关键路径/ID/命令、工具调用结论、未决问题。
/// 图片等多模态内容跳过。
pub const SUMMARY_PROMPT_TEMPLATE: &str = "\
你是对话摘要助手。以下是一段对话中被裁剪掉的旧消息（按时间顺序）。\
请将它们压缩为一段简洁的摘要，保留以下信息：
- 用户目标与偏好
- 重要决策及理由
- 关键路径、ID、命令
- 工具调用的结论
- 未解决的问题

跳过寒暄、重复内容和细节推导。保持客观叙述，用第三人称。\
摘要不超过 300 字。

--- 被裁消息 ---

{messages}

请直接输出摘要，不要包含前言或解释。";

/// 构造摘要请求 prompt。
///
/// `messages_text` 是被裁消息拼接后的文本（已格式化为 `[用户] xxx\n[助手] yyy`）。
pub fn build_summary_prompt(messages_text: &str) -> String {
    SUMMARY_PROMPT_TEMPLATE.replace("{messages}", messages_text)
}

/// 从消息列表构造被裁消息的文本拼接（用于摘要 prompt 输入）。
///
/// 跳过图片等多模态内容，只取文本部分。
pub fn format_messages_for_summary(messages: &[rig_core::completion::Message]) -> String {
    use rig_core::completion::Message;
    let mut buf = String::new();
    for msg in messages {
        let role = match msg {
            Message::System { .. } => "系统",
            Message::User { .. } => "用户",
            Message::Assistant { .. } => "助手",
        };
        let text = crate::domain::ai::memory::extract_message_text(msg);
        if text.is_empty() {
            continue;
        }
        if !buf.is_empty() {
            buf.push('\n');
        }
        buf.push_str(&format!("[{role}] {text}"));
    }
    buf
}

/// 摘要请求的 max_tokens 上限（D5）。
pub const SUMMARY_MAX_TOKENS: usize = 600;

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_summary_block_empty() {
        assert_eq!(format_summary_block(&[]), "");
    }

    #[test]
    fn format_summary_block_single() {
        let block = format_summary_block(&["摘要内容1".to_string()]);
        assert!(block.starts_with("<summary>\n"));
        assert!(block.ends_with("</summary>"));
        assert!(block.contains("摘要内容1"));
    }

    #[test]
    fn format_summary_block_multiple() {
        let block =
            format_summary_block(&["段1".to_string(), "段2".to_string(), "段3".to_string()]);
        assert!(block.starts_with("<summary>"));
        assert!(block.ends_with("</summary>"));
        assert!(block.contains("段1"));
        assert!(block.contains("段2"));
        assert!(block.contains("段3"));
        assert!(block.contains("---"));
    }

    #[test]
    fn estimate_summary_tokens_nonzero() {
        let tokens = estimate_summary_block_tokens(&["这是一段摘要".to_string()]);
        assert!(tokens > 0, "摘要块 token 应 > 0");
    }

    #[test]
    fn estimate_summary_tokens_empty() {
        assert_eq!(estimate_summary_block_tokens(&[]), 0);
    }

    #[test]
    fn should_merge_at_threshold() {
        assert!(!should_merge_summaries(0));
        assert!(!should_merge_summaries(1));
        assert!(!should_merge_summaries(2));
        assert!(should_merge_summaries(3));
        assert!(should_merge_summaries(5));
    }

    #[test]
    fn compute_summary_range_no_new_messages() {
        // compress_boundary <= watermark → None
        assert_eq!(compute_summary_range(10, 10), None);
        assert_eq!(compute_summary_range(10, 5), None);
    }

    #[test]
    fn compute_summary_range_with_new_messages() {
        // watermark=10, boundary=20 → (11, 20)
        assert_eq!(compute_summary_range(10, 20), Some((11, 20)));
    }

    #[test]
    fn compute_summary_range_from_zero() {
        // watermark=0, boundary=5 → (1, 5)
        assert_eq!(compute_summary_range(0, 5), Some((1, 5)));
    }

    #[test]
    fn next_summary_idx_test() {
        assert_eq!(next_summary_idx(0), 0);
        assert_eq!(next_summary_idx(2), 2);
        assert_eq!(next_summary_idx(-1), 0); // 负数防御
    }

    #[test]
    fn build_merge_prompt_contains_both() {
        let prompt = build_merge_prompt("段1内容", "段2内容");
        assert!(prompt.contains("段1内容"));
        assert!(prompt.contains("段2内容"));
        assert!(prompt.contains("合并"));
    }

    #[test]
    fn build_summary_prompt_contains_messages() {
        let prompt = build_summary_prompt("[用户] 你好\n[助手] 你好");
        assert!(prompt.contains("[用户] 你好"));
        assert!(prompt.contains("[助手] 你好"));
        assert!(prompt.contains("摘要"));
    }

    #[test]
    fn format_messages_for_summary_skips_empty() {
        use rig_core::completion::Message;
        use rig_core::completion::message::{AssistantContent, Text};
        let msgs = vec![
            Message::User {
                content: vec![rig_core::completion::message::UserContent::Text(Text::new(
                    "hello",
                ))],
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::Text(Text::new(""))], // 空 text 应跳过
            },
            Message::Assistant {
                id: None,
                content: vec![AssistantContent::Text(Text::new("world"))],
            },
        ];
        let formatted = format_messages_for_summary(&msgs);
        assert!(formatted.contains("[用户] hello"));
        assert!(formatted.contains("[助手] world"));
        assert!(!formatted.contains("[]")); // 空消息不应出现
    }
}
