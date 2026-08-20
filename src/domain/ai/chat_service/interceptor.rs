//! 流式拦截器——校准采样 + reasoning_effort 400 自愈 + Done 分发
//! （0.21.23 从 chat_service.rs `prompt()` 内嵌 task 拆分）。
//!
//! 数据流：`stream_prompt` 的 chunk 先经拦截 channel，再转发给 IPC 层的
//! `chunk_tx`。拦截器在转发前后做三件事：
//! 1. **校准采样**（P0-1）：Done chunk 的真实 usage 采样到 `UsageCalibrator`，
//!    仅纯文本流（无 ToolCall）采样——tool-loop 的 Done usage 口径与请求前估算不符
//! 2. **400 自愈**（0.21.20 T2）：Error chunk 可解析出支持的档位时不转发，
//!    经 oneshot 通知外层降级 effort 重试一次
//! 3. **Done 分发**（0.21.21 A3 + 0.21.23）：Done 后重算 context_status 并 emit，
//!    重算结果作为摘要触发的 Done 后水位

use std::sync::{Arc, Weak};

use tokio::sync::mpsc;

use crate::domain::ai::agent_provider::{AgentProvider, ChatStreamChunk};
use crate::domain::ai::registry::ResolvedProviderEntries;
use crate::domain::ai::thinking::{
    parse_supported_reasoning_efforts, pick_fallback_effort, thinking_supports_effort,
};

use super::ChatService;

// ── P0-1: 校准采样门控 ──────────────────────────────────────────────────────

/// 校准采样门控——跟踪本流是否出现过 ToolCall，决定 Done 时是否采样。
///
/// tool-loop 的 Done usage 来自 rig FinalResponse（最后一轮请求，含累积历史 +
/// 工具结果），与请求前估算口径不符，因此流中出现 ToolCall 时不采样。
/// 纯文本流（无 ToolCall）的 Done usage 口径一致，可安全采样。
struct CalibrationGate {
    saw_tool_call: bool,
}

impl CalibrationGate {
    fn new() -> Self {
        Self {
            saw_tool_call: false,
        }
    }

    /// 观察流式 chunk，ToolCall 变体置 saw_tool_call = true。
    fn observe(&mut self, chunk: &ChatStreamChunk) {
        if matches!(chunk, ChatStreamChunk::ToolCall { .. }) {
            self.saw_tool_call = true;
        }
    }

    /// 仅在纯文本流（未出现 ToolCall）时允许采样。
    fn should_sample(&self) -> bool {
        !self.saw_tool_call
    }
}

/// 运行带拦截器的流式请求（0.21.23 从 `prompt()` 内嵌 task 抽出）。
///
/// - 拦截器 task：校准采样 + 400 自愈检测 + chunk 转发 + Done 分发
/// - 外层重试循环：最多 2 次尝试（初始 + 1 次降级重试），降级信号经
///   oneshot 从拦截器送达（短 timeout 防竞态：Error chunk 可能略晚于流结束到达）
///
/// `raw_estimated_input_tokens` 取发送前计算的未校准基线（P0-1）；
/// `is_persistent` 门控 Done 后重算与摘要任务——Ephemeral 对话不在 SQLite，
/// 重算会读空库推无效状态，摘要任务白跑一轮水位查询。
#[allow(clippy::too_many_arguments)]
pub(super) async fn run_intercepted_stream(
    service: &Arc<ChatService>,
    provider: &Arc<AgentProvider>,
    resolved: &ResolvedProviderEntries,
    conversation_id: &str,
    message: &str,
    chunk_tx: mpsc::UnboundedSender<ChatStreamChunk>,
    thinking_enabled: bool,
    reasoning_effort: Option<String>,
    raw_estimated_input_tokens: u32,
    is_persistent: bool,
) {
    let weak_service: Weak<ChatService> = Arc::downgrade(service);

    let (intercept_tx, mut intercept_rx) = mpsc::unbounded_channel::<ChatStreamChunk>();
    let cal_provider_id = resolved.provider.id.clone();
    let cal_model_id = resolved.model.id.clone();
    let weak_for_cal = weak_service.clone();
    // 0.21.19: 摘要任务 spawn 所需的上下文
    let summary_conv_id = conversation_id.to_string();
    let weak_for_summary = weak_service.clone();

    // 0.21.20 T2: reasoning_effort 400 自愈——oneshot 信号通道。
    // 拦截器检测到可降级的 400 Error chunk 时，不转发该 Error，
    // 而是经此 oneshot 把 fallback effort 发给外层重试循环。
    // 外层在 stream_prompt().await 返回后用短 timeout 等信号（防竞态）。
    let (retry_tx, retry_rx) = tokio::sync::oneshot::channel::<String>();
    // retry_tx 用 Mutex<Option> 包裹——send() 消耗 self，只发一次，
    // 且需要在 spawn 的 async move 块中访问（需 Send 内部可变性）
    let retry_tx = std::sync::Mutex::new(Some(retry_tx));
    // retry_rx 只在外层循环中使用（不在 spawn 块中），
    // 用普通 Option + take() 即可，无需 Mutex
    let mut retry_rx_opt = Some(retry_rx);
    // 门禁：仅当确实发送了 reasoning_effort 时才可能降级
    let can_retry_effort = reasoning_effort.as_ref().is_some_and(|e| !e.is_empty())
        && thinking_supports_effort(
            resolved.provider.kind,
            resolved.provider.base_url.as_deref(),
            &resolved.model.id,
            resolved.model.thinking_style,
        );
    let retry_effort_for_interceptor = reasoning_effort.clone();

    tokio::spawn(async move {
        let mut gate = CalibrationGate::new();
        while let Some(chunk) = intercept_rx.recv().await {
            // P0-1: 逐 chunk 观察，跟踪是否出现 ToolCall
            gate.observe(&chunk);
            // 0.21.17: Done chunk 采样到 calibrator（P0-1: gate 门控）
            if let ChatStreamChunk::Done { ref usage, .. } = chunk
                && gate.should_sample()
                && let Some(service) = weak_for_cal.upgrade()
            {
                service.calibrator.record(
                    &cal_provider_id,
                    &cal_model_id,
                    raw_estimated_input_tokens,
                    usage,
                );
                tracing::debug!(
                    provider_id = %cal_provider_id,
                    model_id = %cal_model_id,
                    raw_estimated = raw_estimated_input_tokens,
                    actual = usage.input_tokens,
                    reported = usage.reported,
                    "calibrator: 已采样 usage（纯文本流）"
                );
            }
            if !gate.should_sample() && matches!(chunk, ChatStreamChunk::Done { .. }) {
                tracing::debug!(
                    provider_id = %cal_provider_id,
                    model_id = %cal_model_id,
                    "calibrator: 跳过采样（流中出现过 ToolCall）"
                );
            }
            // 0.21.19: Done chunk 时 spawn 后台摘要任务。
            // 必须先 send chunk 再 spawn——摘要是后台优化项，
            // 不能阻塞 Done chunk 传递给前端（否则前端卡在"生成中"）
            let is_done = matches!(chunk, ChatStreamChunk::Done { .. });

            // 0.21.20 T2: reasoning_effort 400 自愈检测。
            // 拦截 Error chunk，若门禁通过且能解析出支持的档位，
            // 则不转发 Error，经 oneshot 发 fallback 给外层重试。
            // （只触发一次——retry_tx 消费后后续 Error 原样转发）
            // send 失败 = 外层已超时放弃重试（rx 已 drop）——此时
            // 必须 fallback 到转发原 Error，否则前端既无重试也无报错。
            if !is_done
                && can_retry_effort
                && let ChatStreamChunk::Error { ref message } = chunk
                && let Some(supported) = parse_supported_reasoning_efforts(message)
            {
                let attempted = retry_effort_for_interceptor.as_deref().unwrap_or("");
                if let Some(fallback) = pick_fallback_effort(attempted, &supported) {
                    tracing::info!(
                        conversation_id = %summary_conv_id,
                        attempted = %attempted,
                        fallback = %fallback,
                        supported = ?supported,
                        "T2: reasoning_effort 400 自愈——降级重试"
                    );
                    // 不转发 Error chunk，发信号给外层；信号送达才算拦截成功
                    let signal_delivered = retry_tx
                        .lock()
                        .ok()
                        .and_then(|mut g| g.take())
                        .is_some_and(|tx| tx.send(fallback).is_ok());
                    if signal_delivered {
                        continue; // 跳过此 Error chunk
                    }
                }
            }

            if chunk_tx.send(chunk).is_err() {
                break;
            }
            // 0.21.21 A3: Done 后一律重算 context_status 并 emit——
            // 环/popup 不再滞后一轮（之前只在 summary_enabled 时重算）。
            // 0.21.23: 重算与摘要触发合并为同一 Done 后任务——重算结果即
            // Done 后水位（含刚完成的 assistant 回复），直接用于摘要触发判定。
            // 旧实现取发送前快照，少算刚完成的回复，摘要晚一轮触发
            // （边界计算不受影响，水位口径未变）。
            // 摘要后台任务完成时还会再推一次，属预期（前端幂等渲染）。
            // 仅 Persistent——Ephemeral 对话不落库，重算读空库、摘要白跑。
            if is_done && is_persistent && let Some(service) = weak_for_summary.upgrade() {
                let conv_id = summary_conv_id.clone();
                tokio::spawn(async move {
                    // 竞态守卫：与下一轮发送竞态用 requests.status() 守卫
                    if service.requests.status().is_some() {
                        tracing::debug!(
                            conversation_id = %conv_id,
                            "Done 后重算：检测到新请求已启动，跳过重算"
                        );
                        return;
                    }
                    let Some(status) = service.emit_context_status_updated(&conv_id).await else {
                        return;
                    };
                    // 0.21.19: 摘要触发（summary_enabled 时）——使用 Done 后水位
                    if !service.summary_enabled_snapshot().await {
                        return;
                    }
                    service
                        .maybe_spawn_summary_task(&conv_id, status.usage_percent)
                        .await;
                });
            }
        }
    });

    // 0.21.20 T2: 重试循环——最多 2 次尝试（初始 + 1 次降级重试）
    let provider = provider.clone();
    let conversation_id = conversation_id.to_string();
    let message = message.to_string();
    let mut current_effort = reasoning_effort.clone();
    for attempt in 0..2u8 {
        provider
            .stream_prompt(
                &conversation_id,
                &message,
                intercept_tx.clone(),
                thinking_enabled,
                current_effort.clone(),
            )
            .await;
        // 首次尝试后检查是否有降级信号（短 timeout 防竞态：
        // Error chunk 可能略晚于流结束到达拦截器）
        if attempt == 0 {
            // 取出 retry_rx（只等一次）
            if let Some(rx) = retry_rx_opt.take() {
                match tokio::time::timeout(std::time::Duration::from_millis(200), rx).await {
                    Ok(Ok(fallback)) => {
                        // 降级重试——换 effort 再跑一次
                        current_effort = Some(fallback);
                        continue;
                    }
                    Ok(Err(_)) | Err(_) => {
                        // 无信号或超时——首次结果即为最终结果
                        break;
                    }
                }
            } else {
                break;
            }
        } else {
            // 第二次（重试）流式的 Error 原样转发，不再自愈
            break;
        }
    }
    // 显式 drop intercept_tx——关闭拦截器 channel，让拦截器 task 的
    // recv 循环自然退出（intercept_rx.recv() 返回 None → break）
    drop(intercept_tx);
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── P0-1: CalibrationGate 测试 ──────────────────────────────────────────

    #[test]
    fn calibration_gate_pure_text_allows_sampling() {
        let mut gate = CalibrationGate::new();
        assert!(gate.should_sample(), "初始状态应允许采样");

        gate.observe(&ChatStreamChunk::Text {
            text: "hello".into(),
        });
        gate.observe(&ChatStreamChunk::Text {
            text: " world".into(),
        });
        assert!(gate.should_sample(), "纯文本流应允许采样");
    }

    #[test]
    fn calibration_gate_tool_call_blocks_sampling() {
        let mut gate = CalibrationGate::new();
        gate.observe(&ChatStreamChunk::Text {
            text: "let me check".into(),
        });
        gate.observe(&ChatStreamChunk::ToolCall {
            tool: "search".into(),
            call_id: "c1".into(),
            arguments: "{}".into(),
        });
        assert!(!gate.should_sample(), "出现 ToolCall 后不应允许采样");
    }

    #[test]
    fn calibration_gate_tool_result_does_not_block() {
        let mut gate = CalibrationGate::new();
        // ToolResult 不应影响判定——只看 ToolCall
        gate.observe(&ChatStreamChunk::ToolResult {
            call_id: "c1".into(),
            success: true,
            summary: "result".into(),
        });
        assert!(
            gate.should_sample(),
            "仅 ToolResult 不应阻止采样（只看 ToolCall）"
        );
    }
}
