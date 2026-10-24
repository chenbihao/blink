//! 摘要任务——水位 / 压缩边界 / 摘要生成 / 段合并（0.21.23 从 chat_service.rs 拆分）。
//!
//! 职责边界：
//! - `maybe_spawn_summary_task`：Done / 手动压缩后的摘要触发入口
//!   （in-flight 竞态防护 + summary_enabled 门控 + 水位判定 + LLM 摘要 + 落库推进水位）
//! - `maybe_merge_summaries`：摘要段 ≥ 3 时合并最旧两段
//! - 裸 Agent 构造与一次性 LLM 调用（无工具，杜绝 ToolCall 副作用）
//! - `MemoryHealthSummary`：记忆健康度一览数据（popup 展示）
//!
//! 架构约束：所有方法在拦截器 / command spawn 的独立 tokio task 中调用，
//! 不持锁跨 await，不阻塞主 prompt 链路。任何失败只 warn! 并回退纯截断——
//! 摘要是优化项，不是正确性依赖。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use tokio::sync::mpsc;

use crate::ai::agent_provider::{AgentProvider, ChatStreamChunk};
use crate::event_names::EventNames;
use blink_infra::data::conversations::{MERGE_CONV_PREFIX, SUMMARY_CONV_PREFIX};

use super::ChatService;

/// 0.21.19.1 F5: 摘要任务在途标志的 RAII guard——Drop 时释放 `summary_in_flight`。
///
/// 确保所有退出路径（含 early-return）都释放标志。guard 持有 `AtomicBool` 的引用，
/// 生命周期不超过 `ChatService`（函数内局部，安全）。
///
/// `disarm`：把释放责任移交给 spawn 出去的任务主体——手动压缩 job 全程
/// 持标志（分段 LLM 可达数分钟），主函数 spawn 成功后 disarm、返回时不再释放。
struct SummaryInFlightGuard<'a> {
    flag: &'a AtomicBool,
    armed: std::cell::Cell<bool>,
}

impl<'a> SummaryInFlightGuard<'a> {
    fn new(flag: &'a AtomicBool) -> Self {
        Self {
            flag,
            armed: std::cell::Cell::new(true),
        }
    }

    /// 解除本地释放责任（标志保持占用，由接管方负责最终复位）。
    fn disarm(&self) {
        self.armed.set(false);
    }
}

impl Drop for SummaryInFlightGuard<'_> {
    fn drop(&mut self) {
        if self.armed.get() {
            self.flag.store(false, Ordering::SeqCst);
        }
    }
}

/// 0.27.8 手动压缩任务 id 发号器（进程内单调递增；重复触发可识别，phase §3.8）。
static MANUAL_COMPACT_JOB_SEQ: AtomicU64 = AtomicU64::new(1);

/// 手动压缩任务内部分类哨兵串：任务结果为 no-op（未满边界，不动 DB）。
const MANUAL_COMPACT_NOOP: &str = "__noop__";
/// 手动压缩任务内部分类哨兵串：陈旧结果（水位已变，放弃提交）。
const MANUAL_COMPACT_STALE: &str = "__stale__";
/// 手动压缩加载水位以上消息的上限（防御性；正常会话远达不到）。
const MANUAL_COMPACT_LOAD_LIMIT: i64 = 100_000;

/// 0.27.8 手动压缩预检结果（`compact_conversation` command 投影 + 任务状态词）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ManualCompactPrecheck {
    /// 任务已启动，完成后经 `CHAT_COMPACT_JOB` 事件推送。
    Started { job_id: u64 },
    /// AI 请求进行中（流式生成中不允许压缩——与旧 compress_context_now 同门禁）。
    Busy,
    /// 已有摘要任务在途（重复触发可识别）。
    InFlight,
    /// 水位以上无消息（空会话/新会话）。
    NoContent,
    /// 未满手动边界（完整 user turn 不足），no-op 不动 DB。
    NoOp,
}

/// 0.27.8 手动压缩任务事件载荷（`CHAT_COMPACT_JOB`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct CompactJobEvent {
    pub job_id: u64,
    pub conversation_id: String,
    /// `running` / `succeeded` / `no_op` / `failed` / `cancelled`（陈旧结果归
    /// `cancelled`——本次任务未生效，原摘要水位保持）。
    pub status: &'static str,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 成功时被摘要覆盖的消息数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub covered_count: Option<usize>,
    /// 保留的完整 user turn 数。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kept_turns: Option<usize>,
}

/// 0.21.23: 记忆健康度一览（composer bar popup 展示）。
///
/// 收口「压缩策略开关、手动压缩、跨对话召回分散两处」的发现性问题——
/// 普通用户难走完「知道有压缩 → 打开摘要 → 知道何时生效」链路，
/// 一览让当前策略与最近一次摘要状态在容量环 popup 一眼可见。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct MemoryHealthSummary {
    /// 摘要压缩开关（关闭时旧消息只裁剪归档 + FTS 召回）。
    pub summary_enabled: bool,
    /// 摘要触发水位（trigger_ratio × 100，百分比）。
    pub trigger_percent: u8,
    /// 窗口模式（"token_aware" / "fixed_count"）。
    pub window_mode: String,
    /// 跨对话 FTS5 召回开关。
    pub recall_enabled: bool,
    /// 当前对话的摘要段数。
    pub summary_segments: i64,
    /// 最近一次摘要落库时间（unix 秒；无摘要为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_summary_at: Option<i64>,
    /// 被摘要覆盖的消息数（水位线以下，来自缓存的上下文状态）。
    pub summarized_count: usize,
    /// 上轮 load 裁剪移出的消息数（来自缓存的上下文状态）。
    pub last_compressed_count: usize,
}

impl ChatService {
    /// 0.21.20: 为摘要/合并任务构造无工具 AgentProvider（裸 Agent）。
    ///
    /// 摘要/合并调用复用 `cached_agent_ref()` 的 Agent——带全套工具池（Capability + MCP），
    /// 摘要模型若发起 ToolCall 会产生真实副作用。改为构造无工具 Agent 杜绝此风险。
    ///
    /// **不进 cached_agent 缓存**——后台任务非热路径、至多每轮 Done 一次，构造含凭据
    /// 读取可接受；避免缓存失效复杂度。失败则 warn + 返回 None（回退纯截断）。
    ///
    /// 构造签名参照 `ensure_provider` 中的 `AgentProvider::new` 调用，工具池传空 vec。
    /// preamble 用简短固定文本，不影响摘要质量（摘要 prompt 自带角色设定）。
    async fn build_bare_summary_agent(self: &Arc<Self>) -> Option<Arc<AgentProvider>> {
        let resolved = match self.resolve_current_entries(super::ConversationKind::Persistent) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "摘要任务: 解析 provider 失败，回退纯截断"
                );
                return None;
            }
        };
        // 摘要任务不需要工具——传空 vec 杜绝 ToolCall 副作用
        let memory: Arc<dyn rig_core::memory::ConversationMemory> = self.persistent_memory.clone();
        match AgentProvider::new(
            &resolved.provider,
            &resolved.model,
            Vec::new(),
            Vec::new(),
            "You are a conversation summarizer. Produce concise, faithful summaries.",
            memory,
        )
        .await
        {
            Ok(provider) => Some(Arc::new(provider)),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "摘要任务: 裸 Agent 构造失败，回退纯截断"
                );
                None
            }
        }
    }

    /// 裸 Agent 一次性文本调用（0.21.20 抽出，摘要/段合并共用）。
    ///
    /// spawn 流式任务 → 收集 Text 直到 Done → 超时/abort 兜底。
    /// 思考强制关、`SUMMARY_MAX_TOKENS` 限长；`conv_id` 用 `__` 前缀临时对话隔离
    /// （调用方负责 orphan 清理）。错误/超时/空结果返回 None（warn 已记录），
    /// 返回文本已 trim。
    async fn run_bare_llm_text_call(
        provider: &Arc<AgentProvider>,
        conv_id: &str,
        prompt: &str,
        timeout: std::time::Duration,
    ) -> Option<String> {
        let (tx, mut rx) = mpsc::unbounded_channel::<ChatStreamChunk>();
        let provider = provider.clone();
        let conv_id_for_task = conv_id.to_string();
        let prompt = prompt.to_string();

        let llm_task = tokio::spawn(async move {
            provider
                .stream_prompt_with_max_tokens(
                    &conv_id_for_task,
                    &prompt,
                    tx,
                    false,
                    None,
                    crate::ai::summary::SUMMARY_MAX_TOKENS as u64,
                )
                .await;
        });

        let mut text = String::new();
        let result = tokio::time::timeout(timeout, async {
            while let Some(chunk) = rx.recv().await {
                match chunk {
                    ChatStreamChunk::Text { text: t } => text.push_str(&t),
                    ChatStreamChunk::Done { .. } => break,
                    ChatStreamChunk::Error { message } => {
                        tracing::warn!(
                            conversation_id = %conv_id,
                            error = %message,
                            "裸 Agent LLM 调用返回错误"
                        );
                        return;
                    }
                    _ => {}
                }
            }
        })
        .await;

        // abort LLM task（无论成功或超时，都确保 task 结束）
        llm_task.abort();

        if result.is_err() {
            tracing::warn!(
                conversation_id = %conv_id,
                timeout_secs = timeout.as_secs(),
                "裸 Agent LLM 调用超时"
            );
            return None;
        }
        let trimmed = text.trim();
        if trimmed.is_empty() {
            tracing::warn!(conversation_id = %conv_id, "裸 Agent LLM 调用返回空结果");
            return None;
        }
        Some(trimmed.to_string())
    }

    /// 0.21.23: 摘要开关快照（Done 分发门控用；旧 compress_context_now 调用方
    /// 已随 0.27.8 删除，现仅 interceptor 消费）。
    pub async fn summary_enabled_snapshot(&self) -> bool {
        let cfg_handle = self.persistent_memory.config_handle();
        let cfg = cfg_handle.read().await;
        cfg.summary_enabled
    }

    /// 0.21.19: 判断并 spawn 后台摘要任务。
    ///
    /// 在流式 `Done` chunk 到达时调用。检查 `usage_percent` 是否达到阈值，
    /// 是则读取水位 + 压缩边界 → 加载被裁消息 → LLM 生成摘要 → 落库 + 推进水位。
    /// 段数 ≥ 3 时触发段合并。
    ///
    /// 0.21.23: `usage_percent` 由调用方传入——Done 分发路径传 Done 后重算的水位
    /// （旧实现取发送前快照，少算刚完成的 assistant 回复，摘要晚一轮触发）；
    /// 手动压缩路径传 100 强制触发。
    ///
    /// **架构约束**：此方法在拦截器的独立 tokio task 中调用，不持锁跨 await，
    /// 不阻塞主 prompt 链路。任何失败只 warn! 并回退纯截断——摘要是优化项，
    /// 不是正确性依赖。
    ///
    /// **LLM 调用**：构造无工具裸 Agent，用唯一 conversation ID
    /// （`__summary__<conv_id>`）走 stream_prompt 收集文本，避免污染原对话 memory。
    /// 思考强制关，max_tokens 600。
    pub async fn maybe_spawn_summary_task(
        self: &Arc<Self>,
        conversation_id: &str,
        usage_percent: u8,
    ) {
        // 0.21.19.1 F5: 并发竞态防护——compare_exchange 抢占，抢不到直接 return。
        // 两轮快速 Done 会并发跑两个摘要任务并读到同一水位，导致重复区间双份摘要落库。
        if self
            .summary_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!(conversation_id, "摘要任务: 已有在途摘要任务，跳过");
            return;
        }
        // 确保所有退出路径（含 early-return）都释放标志——配合 F3 内部 async 块收尾。
        // `_` 前缀抑制 unused 告警，Drop 在作用域结束时释放（含所有 early-return）。
        let _summary_guard = SummaryInFlightGuard::new(&self.summary_in_flight);

        // 0.21.19.1 F2: 检查 summary_enabled——关闭时直接 return，不产生 DB 写入与 LLM 调用。
        // 手动压缩路径（compress_context_now）也经此入口，开关关闭时点"立即裁剪"不再触发摘要。
        let (summary_enabled, trigger_ratio) = {
            let cfg_handle = self.persistent_memory.config_handle();
            let cfg = cfg_handle.read().await;
            (cfg.summary_enabled, cfg.trigger_ratio)
        };
        if !summary_enabled {
            tracing::debug!(
                conversation_id,
                "摘要任务: summary_enabled=false，跳过（手动压缩同此路径）"
            );
            return;
        }
        let trigger_threshold = (trigger_ratio * 100.0) as u8;
        if usage_percent < trigger_threshold {
            tracing::debug!(
                conversation_id,
                usage_percent,
                trigger_threshold,
                "摘要任务: usage 未达阈值，跳过"
            );
            return;
        }

        // ── F3: 主体包进内部 async 块，结束后无条件清理 orphan 对话 ──────────────
        // 所有 early-return 只退出此块，后续 orphan 清理与标志释放始终执行。
        let pool = self.persistent_memory.pool().clone();
        let orphan_ids = [
            format!("{}{conversation_id}", SUMMARY_CONV_PREFIX),
            format!("{}{conversation_id}", MERGE_CONV_PREFIX),
        ];

        // 内部块——借用 orphan_ids（块的 return 提前退出后清理仍执行）
        let _ = async {
            // 2. 读取水位
            let watermark = match blink_infra::data::conversations::get_summarized_until(
                &pool,
                conversation_id,
            )
            .await
            {
                Ok(wm) => wm,
                Err(e) => {
                    tracing::warn!(
                        conversation_id,
                        error = %e,
                        "摘要任务: 读取水位失败"
                    );
                    return;
                }
            };

            // 3. 确定压缩边界
            //    FixedCount 模式：条数口径（window_size 条保留，其余被裁）
            //    TokenAware 模式（0.21.19.1 F1）：token 口径——加载水位以上窗口消息，
            //    逐条 estimate_tokens，用 compute_truncate_boundary + fallback 链
            //    history_budget → context_limit → DEFAULT_CONTEXT_LIMIT（与 load_inner 一致）。
            //    边界 = 被裁的最后一条消息的 rowid。compute_summary_range 及后续流程不变。
            let cfg = {
                let cfg_handle = self.persistent_memory.config_handle();
                let cfg_guard = cfg_handle.read().await;
                cfg_guard.clone()
            };

            let compress_boundary = match cfg.mode {
                crate::ai::memory::WindowMode::FixedCount => {
                    // 条数口径：保留最近 window_size 条，之前的是被裁区间
                    match blink_infra::data::conversations::get_compress_boundary(
                        &pool,
                        conversation_id,
                        cfg.window_size,
                        watermark,
                    )
                    .await
                    {
                        Ok(Some(b)) => b,
                        Ok(None) => {
                            tracing::debug!(
                                conversation_id,
                                watermark,
                                "摘要任务: 无新被裁消息（FixedCount），跳过"
                            );
                            return;
                        }
                        Err(e) => {
                            tracing::warn!(
                                conversation_id,
                                error = %e,
                                "摘要任务: 计算压缩边界失败（FixedCount）"
                            );
                            return;
                        }
                    }
                }
                crate::ai::memory::WindowMode::TokenAware => {
                    // 0.21.19.1 F1: token 口径——与 load_inner 同一判定函数
                    let window_rows = match blink_infra::data::conversations::load_window_messages_with_ids_after_watermark(
                        &pool,
                        conversation_id,
                        crate::ai::memory::TOKEN_AWARE_LOAD_BATCH,
                        watermark,
                    )
                    .await
                    {
                        Ok(rows) => rows,
                        Err(e) => {
                            tracing::warn!(
                                conversation_id,
                                error = %e,
                                "摘要任务: 加载窗口消息失败（TokenAware）"
                            );
                            return;
                        }
                    };
                    if window_rows.is_empty() {
                        tracing::debug!(
                            conversation_id,
                            "摘要任务: 水位以上无消息（TokenAware），跳过"
                        );
                        return;
                    }
                    // 逐条估算 token（与 load_inner 一致：estimate_tokens(extract_message_text)）
                    let per_message_tokens: Vec<usize> = window_rows
                        .iter()
                        .map(|(_id, _role, content)| {
                            use rig_core::completion::Message;
                            match serde_json::from_str::<Message>(content) {
                                Ok(msg) => crate::ai::memory::estimate_tokens(
                                    &crate::ai::memory::extract_message_text(&msg),
                                ),
                                Err(_) => 0, // 损坏行不计 token，跳过
                            }
                        })
                        .collect();
                    // fallback 链：history_budget → context_limit → DEFAULT_CONTEXT_LIMIT
                    let budget = cfg
                        .history_budget
                        .or(cfg.context_limit)
                        .unwrap_or(crate::ai::memory::DEFAULT_CONTEXT_LIMIT);
                    match crate::ai::memory::compute_truncate_boundary(
                        &per_message_tokens,
                        budget,
                        cfg.trigger_ratio,
                        cfg.compress_ratio,
                    ) {
                        Some(drop_count) if drop_count > 0 => {
                            // 被裁的最后一条 = window_rows[drop_count - 1].id
                            window_rows[drop_count - 1].0
                        }
                        _ => {
                            tracing::debug!(
                                conversation_id,
                                budget,
                                total_tokens = per_message_tokens.iter().sum::<usize>(),
                                "摘要任务: 未达裁剪阈值或无被裁消息（TokenAware），跳过"
                            );
                            return;
                        }
                    }
                }
            };

            // 4. 计算摘要区间
            let range = match crate::ai::summary::compute_summary_range(
                watermark,
                compress_boundary,
            ) {
                Some(r) => r,
                None => {
                    tracing::debug!(
                        conversation_id,
                        watermark,
                        compress_boundary,
                        "摘要任务: 无新区间，跳过"
                    );
                    return;
                }
            };

            // 5. 加载被裁消息
            let rows = match blink_infra::data::conversations::load_messages_by_rowid_range(
                &pool,
                conversation_id,
                range.0,
                range.1,
            )
            .await
            {
                Ok(rows) => rows,
                Err(e) => {
                    tracing::warn!(
                        conversation_id,
                        error = %e,
                        start = range.0,
                        end = range.1,
                        "摘要任务: 加载被裁消息失败"
                    );
                    return;
                }
            };

            if rows.is_empty() {
                tracing::debug!(conversation_id, "摘要任务: 被裁消息为空，跳过");
                return;
            }

            // 6. 反序列化为 Message 列表
            use rig_core::completion::Message;
            let mut messages: Vec<Message> = Vec::with_capacity(rows.len());
            for (_role, content) in &rows {
                match serde_json::from_str::<Message>(content) {
                    Ok(msg) => messages.push(msg),
                    Err(e) => {
                        tracing::warn!(
                            conversation_id,
                            error = %e,
                            "摘要任务: 消息反序列化失败，跳过"
                        );
                    }
                }
            }

            if messages.is_empty() {
                tracing::debug!(conversation_id, "摘要任务: 无可摘要消息，跳过");
                return;
            }

            // 7. 构造摘要 prompt
            let messages_text = crate::ai::summary::format_messages_for_summary(&messages);
            let prompt = crate::ai::summary::build_summary_prompt(&messages_text);

            // 8. 0.21.20: 构造无工具裸 Agent（杜绝 ToolCall 副作用）
            let Some(provider) = self.build_bare_summary_agent().await else {
                tracing::debug!(conversation_id, "摘要任务: 裸 Agent 构造失败，跳过（回退纯截断）");
                return;
            };

            // 9. LLM 调用——用唯一 conversation ID 避免污染原对话 memory
            // D5: 思考强制关，max_tokens 600；30s 超时（摘要是后台优化，不能无限等待）
            let summary_conv_id = format!("{}{conversation_id}", SUMMARY_CONV_PREFIX);
            let Some(summary_text) = Self::run_bare_llm_text_call(
                &provider,
                &summary_conv_id,
                &prompt,
                std::time::Duration::from_secs(30),
            )
            .await
            else {
                return;
            };

            // 10. 落库 + 推进水位
            let summary_count = blink_infra::data::conversations::count_summaries(
                &pool,
                conversation_id,
            )
            .await
            .unwrap_or(0);
            let next_idx = crate::ai::summary::next_summary_idx(summary_count);
            let token_est = crate::ai::memory::estimate_tokens(&summary_text) as i64;

            if let Err(e) = blink_infra::data::conversations::insert_summary_and_advance_watermark(
                &pool,
                conversation_id,
                next_idx,
                range.0,
                range.1,
                &summary_text,
                token_est,
            )
            .await
            {
                tracing::warn!(
                    conversation_id,
                    error = %e,
                    "摘要任务: 落库失败"
                );
                return;
            }

            tracing::info!(
                conversation_id,
                summary_idx = next_idx,
                start_rowid = range.0,
                end_rowid = range.1,
                token_est,
                "摘要任务: 摘要已生成并落库"
            );

            // 11. 段合并——summary 段 ≥ 3 时合并最旧两段
            self.maybe_merge_summaries(conversation_id, &pool).await;

            // 12. 推送更新后的上下文状态到前端
            self.emit_context_status_updated(conversation_id).await;
        }
        .await;

        // 13. F3: 清理 LLM 调用产生的 orphan 对话数据（无条件执行，无论成功/失败）
        // stream_prompt 用 __summary__/__merge__ 前缀的 conversation_id，
        // rig 内部会 append 消息到这些临时对话——失败路径若不清理则泄漏到侧边栏。
        for orphan_id in &orphan_ids {
            if let Err(e) =
                blink_infra::data::conversations::delete_conversation(&pool, orphan_id).await
            {
                tracing::debug!(
                    conversation_id,
                    orphan_id,
                    error = %e,
                    "摘要任务: 清理 orphan 对话失败（可能不存在，忽略）"
                );
            }
        }
    }

    // ── 0.27.8 手动压缩（/compact 与 80% 告警按钮共用，phase §3.8/§5.8） ──────

    /// 手动压缩入口：预检（单活跃/在途/无内容/未满边界）后 spawn 后台任务。
    ///
    /// 与自动摘要的关键差异（§5.8）：
    /// - **不受全局 summary_enabled 约束**——显式 /compact 可生成，成功后为该
    ///   会话记录手动摘要启用状态（load 侧据此注入，§5.8.2）
    /// - **手动边界**：保留最近 `MANUAL_COMPACT_KEEP_TURNS` 个完整 user turn，
    ///   压缩其之前全部消息（自动路径按 token 水位裁剪边界，两者独立）
    /// - **冷构造 agent**：直接按当前模型配置构造无工具裸 Agent，不要求先发
    ///   消息建立缓存（旧 compress_context_now 的限制在此解除）
    /// - **CAS 提交**：冻结高水位，编辑/截断/删除推进过水位则陈旧结果放弃
    pub async fn start_manual_compact(
        self: &Arc<Self>,
        conversation_id: &str,
    ) -> ManualCompactPrecheck {
        // 1. AI 请求进行中不允许压缩（与旧 compress_context_now 同门禁：
        //    生成中的注入预算与 prompt 存在时序窗口）
        if self.current_request_context().is_some() {
            return ManualCompactPrecheck::Busy;
        }
        // 2. 摘要任务在途（自动或手动共用单槽）——重复触发可识别
        if self
            .summary_in_flight
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_err()
        {
            tracing::debug!(conversation_id, "手动压缩: 已有摘要任务在途，跳过");
            return ManualCompactPrecheck::InFlight;
        }
        // 占位 guard 只覆盖预检 early-return；spawn 成功后 disarm，释放责任
        // 移交给 run_manual_compact_job 内的 job guard（任务全程持标志）。
        let summary_guard = SummaryInFlightGuard::new(&self.summary_in_flight);

        // 3. 预检：水位以上是否有消息
        let pool = self.persistent_memory.pool().clone();
        let watermark = match blink_infra::data::conversations::get_summarized_until(
            &pool,
            conversation_id,
        )
        .await
        {
            Ok(wm) => wm,
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "手动压缩: 读取水位失败");
                // 水位读不出按 0 处理（全量候选），任务内再全量加载
                0
            }
        };
        let count = blink_infra::data::conversations::count_messages_after_watermark(
            &pool,
            conversation_id,
            watermark,
        )
        .await
        .unwrap_or(0);
        if count == 0 {
            return ManualCompactPrecheck::NoContent;
        }

        // 4. spawn 后台任务；job 事件按 job_id/conversation_id 推送状态
        let job_id = MANUAL_COMPACT_JOB_SEQ.fetch_add(1, Ordering::Relaxed);
        let chat = self.clone();
        let conv = conversation_id.to_string();
        tokio::spawn(async move {
            chat.run_manual_compact_job(job_id, &conv, watermark).await;
        });
        // spawn 成功（失败会 panic，guard 仍 armed 会释放）：单活跃标志由
        // 任务主体持有到终态，本函数返回不释放
        summary_guard.disarm();
        tracing::info!(conversation_id, job_id, "手动压缩: 任务已启动");
        ManualCompactPrecheck::Started { job_id }
    }

    /// 手动压缩后台任务主体：加载 → turn 边界 → 分段摘要 → 合并 → CAS 提交。
    ///
    /// 输入冻结 `frozen_watermark`（spawn 前读取）；提交前 CAS 校验水位未变。
    async fn run_manual_compact_job(
        self: Arc<Self>,
        job_id: u64,
        conversation_id: &str,
        frozen_watermark: i64,
    ) {
        // 单活跃占位接管：start_manual_compact 已 disarm，本 guard 覆盖任务
        // 全程（含所有 early-return 与 panic unwind），终态时释放标志
        let _job_guard = SummaryInFlightGuard::new(&self.summary_in_flight);
        self.emit_compact_job(&CompactJobEvent {
            job_id,
            conversation_id: conversation_id.to_string(),
            status: "running",
            error: None,
            covered_count: None,
            kept_turns: None,
        });

        let pool = self.persistent_memory.pool().clone();
        let orphan_ids = [
            format!("{}{conversation_id}", SUMMARY_CONV_PREFIX),
            format!("{}{conversation_id}", MERGE_CONV_PREFIX),
        ];

        // 任务主体；所有 early-return 只退出此块，orphan 清理与事件兜底随后执行
        let outcome: Result<usize, String> = async {
            // 1. 加载水位以上消息（升序）
            let rows = match blink_infra::data::conversations::
                load_window_messages_with_ids_after_watermark(
                    &pool,
                    conversation_id,
                    MANUAL_COMPACT_LOAD_LIMIT,
                    frozen_watermark,
                )
                .await
            {
                Ok(rows) => rows,
                Err(e) => return Err(format!("加载消息失败: {e}")),
            };

            // 2. 手动边界（保留最近两个完整 user turn）
            let flags: Vec<(i64, bool)> = rows
                .iter()
                .map(|(id, role, content)| {
                    (*id, crate::ai::summary::is_user_prompt_row(role, content))
                })
                .collect();
            let Some(boundary) = crate::ai::summary::compute_manual_compact_boundary(
                &flags,
                crate::ai::summary::MANUAL_COMPACT_KEEP_TURNS,
            ) else {
                return Err(MANUAL_COMPACT_NOOP.to_string()); // no-op 用哨兵串区分
            };

            // 3. 被压缩区间 = 水位以上、边界及之前的消息（边界本身被压缩）
            let target_rows: Vec<&(i64, String, String)> =
                rows.iter().filter(|(id, _, _)| *id <= boundary).collect();
            if target_rows.is_empty() {
                return Err(MANUAL_COMPACT_NOOP.to_string());
            }

            // 4. 反序列化 + token 估算（分段预算用）
            use rig_core::completion::Message;
            let mut messages: Vec<Message> = Vec::with_capacity(target_rows.len());
            for (_, _, content) in &target_rows {
                match serde_json::from_str::<Message>(content) {
                    Ok(msg) => messages.push(msg),
                    Err(e) => {
                        tracing::warn!(conversation_id, error = %e, "手动压缩: 消息反序列化失败，跳过该条");
                    }
                }
            }
            if messages.is_empty() {
                return Err(MANUAL_COMPACT_NOOP.to_string());
            }
            let tokens: Vec<usize> = messages
                .iter()
                .map(|msg| {
                    crate::ai::memory::estimate_tokens(&crate::ai::memory::extract_message_text(
                        msg,
                    ))
                })
                .collect();

            // 5. 分段预算（摘要模型 = 当前模型配置，冷构造无工具裸 Agent）
            let budget = match self.resolve_current_entries(super::ConversationKind::Persistent) {
                Ok(resolved) => crate::ai::summary::manual_summary_input_budget(
                    resolved.model.context_window,
                ),
                Err(e) => {
                    tracing::warn!(conversation_id, error = %e, "手动压缩: 解析模型失败，用默认预算");
                    crate::ai::summary::manual_summary_input_budget(None)
                }
            };
            let Some(provider) = self.build_bare_summary_agent().await else {
                return Err("摘要 Agent 构造失败（模型未配置或凭据不可用）".to_string());
            };

            // 6. 分段生成摘要（各段结果暂存，统一提交）
            let segments = crate::ai::summary::plan_summary_segments(&tokens, budget);
            let summary_conv_id = format!("{}{conversation_id}", SUMMARY_CONV_PREFIX);
            let mut segment_summaries: Vec<String> = Vec::with_capacity(segments.len());
            for (start, end) in &segments {
                let messages_text =
                    crate::ai::summary::format_messages_for_summary(&messages[*start..*end]);
                let prompt = crate::ai::summary::build_summary_prompt(&messages_text);
                let Some(summary) = Self::run_bare_llm_text_call(
                    &provider,
                    &summary_conv_id,
                    &prompt,
                    std::time::Duration::from_secs(60),
                )
                .await
                else {
                    return Err("摘要生成失败（模型返回空结果/超时）".to_string());
                };
                segment_summaries.push(summary);
            }

            // 7. 多段合并（fold：相邻两段逐次合并为一段）
            let merge_conv_id = format!("{}{conversation_id}", MERGE_CONV_PREFIX);
            let mut merged = segment_summaries;
            while merged.len() > 1 {
                let a = merged.remove(0);
                let b = merged.remove(0);
                let prompt = crate::ai::summary::build_merge_prompt(&a, &b);
                let Some(text) = Self::run_bare_llm_text_call(
                    &provider,
                    &merge_conv_id,
                    &prompt,
                    std::time::Duration::from_secs(30),
                )
                .await
                else {
                    return Err("分段摘要合并失败".to_string());
                };
                merged.push(text);
            }
            let Some(final_summary) = merged.into_iter().next() else {
                return Err("分段规划为空".to_string());
            };

            // 8. CAS 提交——水位已变（编辑/截断/删除/并发摘要）则陈旧结果放弃
            let covered_count = messages.len();
            let summary_count =
                blink_infra::data::conversations::count_summaries(&pool, conversation_id)
                    .await
                    .unwrap_or(0);
            let next_idx = crate::ai::summary::next_summary_idx(summary_count);
            let token_est = crate::ai::memory::estimate_tokens(&final_summary) as i64;
            let start_rowid = target_rows.first().map(|(id, _, _)| *id).unwrap_or(0);
            let committed = blink_infra::data::conversations::
                insert_summary_and_advance_watermark_if_unchanged(
                    &pool,
                    conversation_id,
                    next_idx,
                    start_rowid,
                    boundary,
                    &final_summary,
                    token_est,
                    frozen_watermark,
                )
                .await
                .map_err(|e| format!("摘要落库失败: {e}"))?;
            if !committed {
                tracing::info!(conversation_id, job_id, "手动压缩: 水位已变化，陈旧结果放弃提交");
                return Err(MANUAL_COMPACT_STALE.to_string()); // cancelled 哨兵
            }

            // 9. 记录手动摘要启用状态（全局摘要关闭时 load 仍注入，§5.8.2）
            if let Err(e) = blink_infra::data::conversations::set_conversation_manual_summary(
                &pool,
                conversation_id,
                true,
            )
            .await
            {
                // 标记失败不回滚摘要（摘要已提交有效）；只 warn——下次手动压缩会重写
                tracing::warn!(conversation_id, error = %e, "手动压缩: 写 manual_summary 标记失败");
            }

            // 10. 段合并 + 推送更新后的上下文状态
            self.maybe_merge_summaries(conversation_id, &pool).await;
            self.emit_context_status_updated(conversation_id).await;

            tracing::info!(
                conversation_id,
                job_id,
                start_rowid,
                boundary,
                covered_count,
                token_est,
                "手动压缩: 摘要已提交"
            );
            Ok(covered_count)
        }
        .await;

        // 11. 无条件清理 LLM 调用产生的 orphan 对话
        for orphan_id in &orphan_ids {
            if let Err(e) =
                blink_infra::data::conversations::delete_conversation(&pool, orphan_id).await
            {
                tracing::debug!(conversation_id, orphan_id, error = %e, "手动压缩: 清理 orphan 对话失败（可能不存在）");
            }
        }

        // 12. 终态事件（no-op / cancelled 用哨兵串区分失败）
        let event = match &outcome {
            Ok(covered_count) => CompactJobEvent {
                job_id,
                conversation_id: conversation_id.to_string(),
                status: "succeeded",
                error: None,
                covered_count: Some(*covered_count),
                kept_turns: Some(crate::ai::summary::MANUAL_COMPACT_KEEP_TURNS),
            },
            Err(message) if message == MANUAL_COMPACT_NOOP => CompactJobEvent {
                job_id,
                conversation_id: conversation_id.to_string(),
                status: "no_op",
                error: None,
                covered_count: None,
                kept_turns: None,
            },
            Err(message) if message == MANUAL_COMPACT_STALE => CompactJobEvent {
                job_id,
                conversation_id: conversation_id.to_string(),
                status: "cancelled",
                error: Some("对话内容已变化，本次压缩结果已放弃".to_string()),
                covered_count: None,
                kept_turns: None,
            },
            Err(message) => CompactJobEvent {
                job_id,
                conversation_id: conversation_id.to_string(),
                status: "failed",
                error: Some(message.clone()),
                covered_count: None,
                kept_turns: None,
            },
        };
        self.emit_compact_job(&event);
    }

    /// 推送手动压缩任务状态事件（定向 chat 窗口）。
    fn emit_compact_job(&self, event: &CompactJobEvent) {
        let payload = serde_json::to_value(event).unwrap_or_default();
        let _ = self.emitter.emit_to("chat", EventNames::CHAT_COMPACT_JOB, payload);
    }

    /// 0.21.19: 段合并——summary 段 ≥ 3 时合并最旧两段。
    ///
    /// 读取最旧两段摘要文本（不删除）→ LLM 合并 → 同事务删除旧两段 + 插入合并段 + 重排 idx。
    /// LLM 失败时不删除任何数据，保留原段。
    async fn maybe_merge_summaries(
        self: &Arc<Self>,
        conversation_id: &str,
        pool: &sqlx::SqlitePool,
    ) {
        let count =
            match blink_infra::data::conversations::count_summaries(pool, conversation_id).await {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(conversation_id, error = %e, "段合并: 查询段数失败");
                    return;
                }
            };

        if !crate::ai::summary::should_merge_summaries(count as usize) {
            return;
        }

        // 读取最旧两段（不删除，LLM 失败时保留原段）
        let pair = match blink_infra::data::conversations::read_oldest_two_summaries(
            pool,
            conversation_id,
        )
        .await
        {
            Ok(Some(pair)) => pair,
            Ok(None) => return,
            Err(e) => {
                tracing::warn!(conversation_id, error = %e, "段合并: 读取最旧两段失败");
                return;
            }
        };

        let (id1, sr1, er1, content1) = &pair[0];
        let (id2, sr2, er2, content2) = &pair[1];
        let merge_prompt = crate::ai::summary::build_merge_prompt(content1, content2);

        // 0.21.20: 构造无工具裸 Agent（杜绝 ToolCall 副作用）
        let provider = match self.build_bare_summary_agent().await {
            Some(p) => p,
            None => {
                tracing::warn!(conversation_id, "段合并: 裸 Agent 构造失败，跳过");
                return;
            }
        };

        // LLM 合并——`__merge__` 前缀临时对话隔离，30s 超时
        let merge_conv_id = format!("{}{conversation_id}", MERGE_CONV_PREFIX);
        let Some(merged_text) = Self::run_bare_llm_text_call(
            &provider,
            &merge_conv_id,
            &merge_prompt,
            std::time::Duration::from_secs(30),
        )
        .await
        else {
            return;
        };

        // 合并段覆盖原两段的 rowid 区间
        let merge_start = (*sr1).min(*sr2);
        let merge_end = (*er1).max(*er2);
        let token_est = crate::ai::memory::estimate_tokens(&merged_text) as i64;

        // 同事务删除旧两段 + 插入合并段 + 重排 idx
        if let Err(e) = blink_infra::data::conversations::replace_oldest_two_with_merged(
            pool,
            conversation_id,
            *id1,
            *id2,
            merge_start,
            merge_end,
            &merged_text,
            token_est,
        )
        .await
        {
            tracing::warn!(conversation_id, error = %e, "段合并: 落库失败");
            return;
        }

        tracing::info!(
            conversation_id,
            start_rowid = merge_start,
            end_rowid = merge_end,
            token_est,
            "段合并: 最旧两段已合并为一段"
        );
    }

    /// 0.21.23: 记忆健康度一览——聚合记忆配置 + 摘要段状态 + 缓存的上下文状态。
    ///
    /// 供 `get_composer_bar_snapshot` 聚合进 popup；估算纯本地（config + 两条
    /// 轻量 SQL + 缓存读取），无 LLM 调用。
    pub async fn memory_health_summary(&self, conversation_id: &str) -> MemoryHealthSummary {
        let cfg = {
            let cfg_handle = self.persistent_memory.config_handle();
            let cfg_guard = cfg_handle.read().await;
            cfg_guard.clone()
        };
        let pool = self.persistent_memory.pool();

        let summary_segments =
            blink_infra::data::conversations::count_summaries(pool, conversation_id)
                .await
                .unwrap_or(0);
        let last_summary_at =
            blink_infra::data::conversations::latest_summary_created_at(pool, conversation_id)
                .await
                .ok()
                .flatten();

        let (summarized_count, last_compressed_count) = self
            .get_context_status_for_conversation(conversation_id)
            .map(|s| (s.summarized_count, s.last_compressed_count))
            .unwrap_or((0, 0));

        MemoryHealthSummary {
            summary_enabled: cfg.summary_enabled,
            trigger_percent: (cfg.trigger_ratio * 100.0) as u8,
            window_mode: match cfg.mode {
                crate::ai::memory::WindowMode::TokenAware => "token_aware".to_string(),
                crate::ai::memory::WindowMode::FixedCount => "fixed_count".to_string(),
            },
            recall_enabled: cfg.recall_enabled,
            summary_segments,
            last_summary_at,
            summarized_count,
            last_compressed_count,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 0.21.19.1 F5: SummaryInFlightGuard Drop 行为 ──────────────────────────

    #[test]
    fn summary_in_flight_guard_releases_on_drop() {
        let flag = AtomicBool::new(false);
        // 抢占
        assert!(
            flag.compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                .is_ok()
        );
        // guard 持有引用——作用域结束后 Drop 应释放标志
        {
            let _guard = SummaryInFlightGuard::new(&flag);
            assert!(flag.load(Ordering::SeqCst), "guard 生存期内标志应为 true");
        }
        assert!(
            !flag.load(Ordering::SeqCst),
            "guard Drop 后标志应恢复 false"
        );
    }

    #[test]
    fn summary_in_flight_guard_releases_on_early_return() {
        let flag = AtomicBool::new(false);
        flag.store(true, Ordering::SeqCst);

        // 模拟 early-return 场景：guard 在函数内创建，函数通过 ? 提前退出后 Drop 仍执行
        fn inner(flag: &AtomicBool) -> Result<(), &'static str> {
            let _guard = SummaryInFlightGuard::new(flag);
            // 模拟 early return（如配置检查失败）
            Err("模拟错误")
        }

        let result = inner(&flag);
        assert!(result.is_err());
        assert!(
            !flag.load(Ordering::SeqCst),
            "early-return 后 guard Drop 仍应释放标志"
        );
    }

    #[test]
    fn summary_in_flight_guard_disarm_keeps_flag_held() {
        // 手动压缩任务模式：spawn 后 disarm，主函数返回不释放；任务主体
        // 的 guard Drop 才复位——否则任务运行期再触发会启动第二个任务
        let flag = AtomicBool::new(true);
        let job_guard = {
            let outer = SummaryInFlightGuard::new(&flag);
            outer.disarm();
            drop(outer);
            assert!(flag.load(Ordering::SeqCst), "disarm 后 Drop 不释放标志");
            SummaryInFlightGuard::new(&flag)
        };
        drop(job_guard);
        assert!(!flag.load(Ordering::SeqCst), "任务主体 guard Drop 释放标志");
    }

    // ── 0.21.19.1 F2: MemoryConfig 默认 summary_enabled = false ──────────────

    #[test]
    fn memory_config_default_summary_enabled_false() {
        let cfg = crate::ai::memory::MemoryConfig::default();
        assert!(
            !cfg.summary_enabled,
            "默认 summary_enabled 应为 false（摘要是可选优化项）"
        );
    }
}
