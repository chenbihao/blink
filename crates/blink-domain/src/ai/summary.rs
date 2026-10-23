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

use crate::ai::memory::estimate_tokens;

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
        let text = crate::ai::memory::extract_message_text(msg);
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

/// 0.27.8 手动压缩：完整 user turn 保留数（phase §3.8——手动压缩覆盖最近
/// 两轮完整 user turn 之前的上下文）。
pub const MANUAL_COMPACT_KEEP_TURNS: usize = 2;

/// 0.27.8 手动压缩：单段摘要输入的 token 预算（phase §5.8.1——长历史分段
/// 输入不超摘要模型预算）。
///
/// 由摘要模型 context window 推导：取 50%，钳制在 [2000, 24000]；
/// 未知窗口用 8000 保守值。集中命名，不散落硬编码。
pub fn manual_summary_input_budget(context_window: Option<u32>) -> usize {
    match context_window {
        Some(w) => ((w as usize) * 50 / 100).clamp(2_000, 24_000),
        None => 8_000,
    }
}

/// 0.27.8 手动压缩边界（phase §3.8/§5.8.1）。
///
/// `rows`：水位以上消息（升序 `(rowid, 是否真实用户提问)`）。"真实用户提问"
/// 指带文本的 user 消息——ToolResult 投递同为 user 角色但不算 turn 起点，
/// 由调用方预先判定（`is_user_prompt_row`）。
///
/// 完整 user turn = 该 user 消息之后还有消息（回复已到）。保留最近
/// `keep_turns` 个完整 turn，压缩其之前的全部消息；返回被压缩区间的
/// 最后一条 rowid（水位以上、保留区之前的消息）。不足边界（完整 turn
/// 数 ≤ keep_turns 或保留区从首行开始）返回 None（no-op，不动 DB）。
///
/// 边界总是落在某个 user 消息的前一条——turn 内的 assistant/工具消息与
/// 其 ToolResult 配对同进同退，不会被拆散。
pub fn compute_manual_compact_boundary(rows: &[(i64, bool)], keep_turns: usize) -> Option<i64> {
    if keep_turns == 0 || rows.is_empty() {
        return None;
    }
    // 完整 turn 起点索引（后续还有消息）
    let turn_starts: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(i, (_, is_user))| *is_user && i + 1 < rows.len())
        .map(|(i, _)| i)
        .collect();
    // 需要至少 keep_turns + 1 个完整 turn：保留 keep_turns 后仍有 ≥1 轮可压缩
    if turn_starts.len() < keep_turns + 1 {
        return None;
    }
    let keep_from = turn_starts[turn_starts.len() - keep_turns];
    if keep_from == 0 {
        return None; // 保留区从首行开始，无前缀可压缩
    }
    Some(rows[keep_from - 1].0)
}

/// 0.27.8 判定持久化消息行是否"真实用户提问"（turn 起点）。
///
/// role 为 user 且反序列化后含非空文本——ToolResult 投递（同为 user 角色）
/// 与损坏行都不算。参数用 (role, content JSON) 原始行形态，供
/// `load_window_messages_with_ids_after_watermark` 结果直接消费。
pub fn is_user_prompt_row(role: &str, content: &str) -> bool {
    use rig_core::completion::Message;
    if role != "user" {
        return false;
    }
    match serde_json::from_str::<Message>(content) {
        Ok(msg) => !crate::ai::memory::extract_message_text(&msg).is_empty(),
        Err(_) => false,
    }
}

/// 0.27.8 手动压缩输入分段（phase §5.8.1）。
///
/// 按消息粒度把 `tokens`（每条消息的估算 token）切成连续段，每段总量
/// ≤ `budget`；单条超过 budget 的消息独占一段（不再细切——由模型尽力，
/// 失败在任务层明示）。返回段区间 `[start, end)`（end-exclusive，相对
/// 消息数组索引）。空输入返回空 vec。
pub fn plan_summary_segments(tokens: &[usize], budget: usize) -> Vec<(usize, usize)> {
    if tokens.is_empty() || budget == 0 {
        return Vec::new();
    }
    let mut segments = Vec::new();
    let mut start = 0usize;
    let mut total = 0usize;
    for (index, &token) in tokens.iter().enumerate() {
        if index > start && total + token > budget {
            segments.push((start, index));
            start = index;
            total = 0;
        }
        total += token;
    }
    segments.push((start, tokens.len()));
    segments
}

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

    // ── 0.27.8 手动压缩：边界 / turn 判定 / 分段 / 预算 ─────────────────────

    /// 行便捷构造：(rowid, 是否用户提问)。u = 用户提问，a = 其他消息。
    fn rows(spec: &str) -> Vec<(i64, bool)> {
        spec.chars()
            .enumerate()
            .filter(|(_, c)| *c != '|')
            .map(|(i, c)| (i as i64 + 1, c == 'u'))
            .collect()
    }

    #[test]
    fn manual_boundary_keeps_last_two_turns() {
        // u a u a u a：3 个完整 turn，保留后 2，压缩第 1 turn（rowid 1-2）
        assert_eq!(
            compute_manual_compact_boundary(&rows("uauaua"), MANUAL_COMPACT_KEEP_TURNS),
            Some(2)
        );
        // turn 间穿插的工具投递行（a）跟随所属 turn，不被单独保留
        assert_eq!(
            compute_manual_compact_boundary(&rows("uaauaua"), MANUAL_COMPACT_KEEP_TURNS),
            Some(3)
        );
    }

    #[test]
    fn manual_boundary_noop_when_turns_insufficient() {
        // 0/1/2 个完整 turn：无前缀可压缩
        assert_eq!(compute_manual_compact_boundary(&rows(""), 2), None);
        assert_eq!(compute_manual_compact_boundary(&rows("uaua"), 2), None);
        assert_eq!(compute_manual_compact_boundary(&rows("uauau"), 2), None);
        // 最后一个 turn 未获回复（不完整）不计入保留数：ua ua u → 完整 turn 仅 2
        assert_eq!(compute_manual_compact_boundary(&rows("uauau"), 2), None);
        // keep_turns = 0 防御
        assert_eq!(compute_manual_compact_boundary(&rows("uauaua"), 0), None);
    }

    #[test]
    fn manual_boundary_tool_delivery_rows_not_turn_starts() {
        // t = ToolResult 投递（user 角色但无文本，不算 turn 起点）：
        // u t a | u t a | u a —— 实际 3 个完整 user turn（起点 u@1/4/7）
        let r = rows("utautaua");
        assert_eq!(compute_manual_compact_boundary(&r, 2), Some(3));
    }

    #[test]
    fn manual_boundary_boundary_never_splits_turn() {
        // 边界必须落在保留区首个 user 提问的前一行
        let r = rows("aauauaua");
        let boundary = compute_manual_compact_boundary(&r, 2).unwrap();
        // rows: a(1) a(2) u(3) a(4) u(5) a(6) u(7) a(8)；保留 u(5) 起 → 边界 = 4
        assert_eq!(boundary, 4);
    }

    #[test]
    fn user_prompt_row_detection() {
        use rig_core::completion::Message;
        use rig_core::completion::message::{Text, UserContent};
        let user_text = serde_json::to_string(&Message::User {
            content: vec![UserContent::Text(Text::new("你好"))],
        })
        .unwrap();
        // ToolResult 投递：user 角色但无 Text 内容
        let tool_delivery = serde_json::json!({
            "role": "user",
            "content": [{"type": "tool_result", "tool_use_id": "t1", "content": "ok"}]
        })
        .to_string();

        assert!(is_user_prompt_row("user", &user_text));
        assert!(!is_user_prompt_row("user", &tool_delivery), "ToolResult 投递不算 turn 起点");
        assert!(!is_user_prompt_row("assistant", &user_text));
        assert!(!is_user_prompt_row("user", "{bad json"));
    }

    #[test]
    fn summary_segments_within_budget_single() {
        assert_eq!(plan_summary_segments(&[100, 200, 300], 1000), vec![(0, 3)]);
        assert_eq!(plan_summary_segments(&[], 1000), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn summary_segments_splits_on_budget() {
        // 预算 500：[400, 300, 200] → [0,1) + [1,3)（300+200=500 ≤ 预算）
        assert_eq!(plan_summary_segments(&[400, 300, 200], 500), vec![(0, 1), (1, 3)]);
    }

    #[test]
    fn summary_segments_oversized_message_alone() {
        // 单条 900 超预算 500：独占一段，不细切
        assert_eq!(plan_summary_segments(&[100, 900, 100], 500), vec![(0, 1), (1, 2), (2, 3)]);
        // 预算 0 防御
        assert_eq!(plan_summary_segments(&[100], 0), Vec::<(usize, usize)>::new());
    }

    #[test]
    fn manual_summary_input_budget_clamped() {
        assert_eq!(manual_summary_input_budget(None), 8_000);
        assert_eq!(manual_summary_input_budget(Some(1_000)), 2_000); // 下限钳制
        assert_eq!(manual_summary_input_budget(Some(8_192)), 4_096); // 50%
        assert_eq!(manual_summary_input_budget(Some(1_000_000)), 24_000); // 上限钳制
    }
}
