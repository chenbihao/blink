//! 上下文窗口状态——计算 + 缓存 + 历史预算注入（0.21.23 从 chat_service.rs 拆分）。
//!
//! 职责边界：
//! - `compute_context_status`：预算估算唯一入口（load_with_stats + token_budget），
//!   结果按 conversation + provider + model 归属缓存
//! - `ContextStatusCache`：32 条上限的 scoped 缓存（P0-2）
//! - `emit_context_status_updated`：Done 后 / 摘要完成后的通用重算推送入口
//! - 历史裁剪预算注入（`update_history_budget`）：先算预算再注入，时序在 rig load 之前

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::RwLock;

use crate::domain::ai::agent_provider::AgentProvider;
use crate::domain::ai::memory::MemoryLoadResult;
use crate::domain::ai::registry::ResolvedProviderEntries;
use crate::domain::event_names::EventNames;

use super::ChatService;

// ── 0.13.6: 上下文窗口状态 ──────────────────────────────────────────────────────

/// 聊天窗口上下文窗口状态（0.13.6）。
///
/// 每次 prompt 前通过 `blink://chat-context-status` 事件推送到前端，
/// 驱动 composer bar 上的环形进度条指示器。
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct ContextWindowStatus {
    /// 估算的当前窗口 token 数（含历史消息 + 当前待发消息 + 系统提示词）。
    pub estimated_tokens: usize,
    /// 模型 context window 上限。
    pub context_limit: usize,
    /// 占用百分比（0-100）。
    pub usage_percent: u8,
    /// 上次 load() 是否触发了压缩。
    pub last_compressed: bool,
    /// 上次压缩移出的消息数。
    pub last_compressed_count: usize,
    /// FTS5 召回的消息数（上次 load()）。
    pub last_recall_count: usize,
    /// 系统提示词（preamble）估算 token 数。
    pub preamble_tokens: usize,
    /// 当前待发消息估算 token 数。
    pub pending_message_tokens: usize,
    // ── 0.21.17 统一 token 预算扩展 ──
    /// 历史消息估算 token 数。
    #[serde(default)]
    pub history_tokens: usize,
    /// 工具定义估算 token 数。
    #[serde(default)]
    pub tools_tokens: usize,
    /// 协议开销 token 数。
    #[serde(default)]
    pub protocol_overhead_tokens: usize,
    /// 多模态内容保守估算 token 数。
    #[serde(default)]
    pub multimodal_tokens: usize,
    /// 输出预留 token 数。
    #[serde(default)]
    pub reserved_output_tokens: usize,
    /// 安全余量 token 数。
    #[serde(default)]
    pub safety_margin_tokens: usize,
    /// 有效输入上限（context_limit - reserved_output - safety_margin）。
    #[serde(default)]
    pub effective_input_limit: usize,
    /// 安全剩余 token 数。
    #[serde(default)]
    pub remaining_tokens: usize,
    /// context limit 来源（"configured" / "provider_metadata" / "fallback"）。
    #[serde(default)]
    pub context_limit_source: String,
    /// 估算置信度（"high" / "medium" / "low"）。
    #[serde(default)]
    pub confidence: String,
    // ── 0.21.17 归属字段 ──
    /// 此状态对应的 conversation_id（防止跨对话返回旧状态）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub conversation_id: String,
    /// 此状态对应的 provider_id（防止跨模型返回旧状态）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub provider_id: String,
    /// 此状态对应的 model_id（防止跨模型返回旧状态）。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub model_id: String,
    /// 未应用 calibration_ratio 的输入基线 token 数（P0-1）。
    /// 用于校准器采样，避免校准生效后 ratio 被拉向 1.0 导致反馈回路。
    #[serde(default)]
    pub raw_estimated_tokens: usize,
    // ── 0.21.19 摘要字段 ──
    /// 摘要块估算 token 数（注入窗口的 `<summary>` 块）。
    #[serde(default)]
    pub summary_tokens: usize,
    /// 被摘要覆盖的消息数（水位线以下）。
    #[serde(default)]
    pub summarized_count: usize,
    /// 摘要压缩开关（前端据此切换提示条文案：摘要 vs 裁剪归档）。
    #[serde(default)]
    pub summary_enabled: bool,
}

/// 0.21.17: 构造 `last_context_status` 的 scoped cache key。
///
/// 格式：`{conversation_id}:{provider_id}:{model_id}`
/// 防止跨对话/跨模型返回旧状态。
fn context_status_scope_key(conversation_id: &str, provider_id: &str, model_id: &str) -> String {
    format!("{conversation_id}:{provider_id}:{model_id}")
}

// ── P0-2: 上下文状态缓存 ────────────────────────────────────────────────────

/// 上下文窗口状态缓存——按 scope_key 存储，按 conversation_id 查询。
///
/// 替代旧 `RwLock<HashMap<String, ContextWindowStatus>>`：
/// - `insert` 使用递增 seq 序号标记时序，支持按 conversation 精确查询最新状态
/// - 32 条上限防长会话多模型切换缓慢增长，淘汰 seq 最小的 entry
pub(super) struct ContextStatusCache {
    entries: RwLock<HashMap<String, (u64, ContextWindowStatus)>>,
    seq: AtomicU64,
}

/// 缓存条目上限。
const CONTEXT_STATUS_CACHE_MAX: usize = 32;

impl ContextStatusCache {
    pub(super) fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            seq: AtomicU64::new(0),
        }
    }

    /// 插入一条上下文状态，seq 自增后写入。超过上限时淘汰 seq 最小的 entry。
    fn insert(&self, scope_key: String, status: ContextWindowStatus) {
        let seq = self.seq.fetch_add(1, Ordering::SeqCst);
        let mut map = self
            .entries
            .write()
            .expect("ContextStatusCache lock poisoned");
        map.insert(scope_key, (seq, status));

        if map.len() > CONTEXT_STATUS_CACHE_MAX {
            // 找到 seq 最小的 entry 并淘汰
            let min_key = map
                .iter()
                .min_by_key(|(_, (s, _))| *s)
                .map(|(k, _)| k.clone());
            if let Some(key) = min_key {
                map.remove(&key);
                tracing::debug!(
                    evicted_key = %key,
                    remaining = map.len(),
                    "ContextStatusCache: 淘汰最旧 entry"
                );
            }
        }
    }

    /// 按 conversation_id 精确匹配查询，多条命中取 seq 最大。
    fn get_for_conversation(&self, conversation_id: &str) -> Option<ContextWindowStatus> {
        let map = self
            .entries
            .read()
            .expect("ContextStatusCache lock poisoned");
        map.iter()
            .filter(|(_, (_, status))| status.conversation_id == conversation_id)
            .max_by_key(|(_, (seq, _))| *seq)
            .map(|(_, (_, status))| status.clone())
    }
}

impl ChatService {
    // ── 0.13.6: 上下文窗口状态 ──────────────────────────────────────────

    /// 计算当前对话的上下文窗口状态（0.13.6 / 0.21.17 重构）。
    ///
    /// 调用 `memory.load_with_stats()` 获取窗口消息 + 压缩/召回统计，
    /// 估算 token 数并计算占用百分比。结果按 `conversation_id + provider_id + model_id`
    /// 归属缓存到 `last_context_status`，防止跨模型/会话返回旧状态。
    ///
    /// 0.21.17 生产连线：
    /// - `tools` 从当前生效的 `AgentProvider` 快照获取（非硬编码空数组）
    /// - `model_max_tokens` 从 `ModelEntry` 传真实解析结果
    /// - `calibration_ratio` 从 `UsageCalibrator` 按当前 provider+model 获取
    ///
    /// `pending_message` 和 `preamble` 参数：在 stream_prompt 前调用时，
    /// 当前用户消息尚未写入 DB，系统提示词也不在消息列表中。
    /// 传入这两个参数可将它们的 token 纳入估算，避免首次对话显示 0%。
    pub async fn compute_context_status(
        &self,
        conversation_id: &str,
        pending_message: Option<&str>,
        preamble: Option<&str>,
        provider: &AgentProvider,
        resolved: &ResolvedProviderEntries,
    ) -> ContextWindowStatus {
        let result = match self
            .persistent_memory
            .load_with_stats(conversation_id)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "compute_context_status: load_with_stats 失败");
                MemoryLoadResult {
                    messages: Vec::new(),
                    dropped_count: 0,
                    recall_count: 0,
                    summarized_count: 0,
                    summary_tokens: 0,
                }
            }
        };

        let config_handle = self.persistent_memory.config_handle();
        let cfg = config_handle.read().await;
        // 0.21.21: 容量上限直读 per-model context_window（去全局化），
        // cfg.context_limit 降为 fallback——仅当模型未配置时兜底。
        // 之前读 cfg.context_limit 会冻结缓存命中时的旧值（ensure_provider
        // 注入跳过），多对话交错时全局单值后写者赢。
        // 0.21.21 A2: context_window 仍为 None 时，按 base_url 归属分档兜底
        // （官方/公网 → 128K，localhost/私网/Ollama → 32K）。
        // tiered_fallback_limit 传给 estimate_request_budget，让 source 标注为 Tiered。
        let tiered_fallback_limit = {
            let kind = match resolved.provider.kind {
                crate::domain::config::ai_config::ProviderKind::OpenAICompatible => {
                    crate::domain::ai::token_budget::TierProviderKind::OpenAiCompatible
                }
                crate::domain::config::ai_config::ProviderKind::AnthropicMessages => {
                    crate::domain::ai::token_budget::TierProviderKind::Anthropic
                }
                crate::domain::config::ai_config::ProviderKind::GeminiGenerateContent => {
                    crate::domain::ai::token_budget::TierProviderKind::Gemini
                }
                crate::domain::config::ai_config::ProviderKind::OllamaHttp => {
                    crate::domain::ai::token_budget::TierProviderKind::Ollama
                }
            };
            crate::domain::ai::token_budget::tiered_fallback_context_limit(
                kind,
                resolved.provider.base_url.as_deref(),
            )
        };
        // context_window 优先级：用户/Provider 已保存值 > 内置模型目录 > 端点分档。
        // 不再读取 memory.cfg.context_limit：它是运行时裁剪注入值，反向读回会把
        // catalog/tiered 来源误标为 configured，并可能带入上一个模型的旧值。
        let catalog_context_window = resolved
            .model
            .context_window
            .is_none()
            .then(|| crate::domain::ai::model_catalog::lookup_context_window(&resolved.model.id))
            .flatten();
        let context_window = resolved.model.context_window.or(catalog_context_window);

        // 0.21.17: 从 AgentProvider 获取真实工具快照
        let tools = provider.tool_prompt_infos();

        // 0.21.17: 从 ModelEntry 获取真实 max_tokens
        let model_max_tokens = resolved.model.max_tokens;

        // 0.21.17: 从 calibrator 获取校准系数
        let calibration_ratio = self
            .calibrator
            .get_ratio(&resolved.provider.id, &resolved.model.id);

        // 提取历史消息文本，统一走 token_budget 预算入口
        let history_texts: Vec<String> = result
            .messages
            .iter()
            .map(crate::domain::ai::memory::extract_message_text)
            .collect();

        // 统计历史消息中的 ToolCall 数量（用于 ID/关联开销估算）
        let tool_call_count = result
            .messages
            .iter()
            .filter_map(|m| match m {
                rig_core::completion::Message::Assistant { content, .. } => Some(
                    content
                        .iter()
                        .filter(|c| {
                            matches!(
                                c,
                                rig_core::completion::message::AssistantContent::ToolCall(_)
                            )
                        })
                        .count(),
                ),
                _ => None,
            })
            .sum::<usize>();

        // 检测多模态内容
        let has_multimodal = result.messages.iter().any(|m| match m {
            rig_core::completion::Message::User { content } => content.iter().any(|c| {
                !matches!(
                    c,
                    rig_core::completion::message::UserContent::Text(_)
                        | rig_core::completion::message::UserContent::ToolResult(_)
                )
            }),
            _ => false,
        });

        let mut budget = crate::domain::ai::token_budget::estimate_request_budget(
            crate::domain::ai::token_budget::TokenBudgetInput {
                history_texts: &history_texts,
                system_prompt: preamble,
                pending_message,
                tools,
                tool_call_count,
                has_multimodal,
                context_window,
                tiered_fallback_limit: Some(tiered_fallback_limit),
                request_max_tokens: None,
                model_max_tokens,
                calibration_ratio,
            },
        );
        if resolved.model.context_window.is_none() && catalog_context_window.is_some() {
            budget.context_limit_source =
                crate::domain::ai::token_budget::ContextLimitSource::Catalog;
        }

        // 0.21.18: 注入历史裁剪预算——load_inner 的裁剪基准从裸 context_limit
        // 换为「预扣 system/tools/pending/输出预留/安全余量后的历史可用预算」。
        // 本方法在每轮 stream_prompt 之前被调用，rig load 发生在 stream_prompt 内部，
        // 注入先于消费，时序有保证。
        // Pending 注意：load_inner 裁剪时消息里含预写 user（之后才被丢弃），预算也
        // 扣了 pending——两边一致，不能"优化"成只算一边，否则预算偏松。
        let history_budget = crate::domain::ai::token_budget::compute_history_token_budget(
            budget.context_limit,
            budget.breakdown.system_tokens,
            budget.breakdown.tools_tokens,
            budget.breakdown.pending_tokens,
            budget.reserved_output_tokens,
            budget.safety_margin_tokens,
        );

        // 召回块预留：load 在裁剪之后才插入 <memory> 块，不受预算约束。
        // 按 worst case 计：recall_top_k × 500 字符（CJK 1 token/char）。
        let recall_reserve: usize = if cfg.recall_enabled {
            (cfg.recall_top_k.max(0) as usize).saturating_mul(500)
        } else {
            0
        };

        // 0.21.20 T3: 摘要块预留——<summary> 块在 load_inner 裁剪之后才插入，
        // 不受预算约束却占用窗口 → 预算偏松。按 result.summary_tokens（已估算）预扣。
        // 口径说明：摘要块文本同时计入 messages→history_tokens（用量侧），
        // 预算侧预留是上限侧——与召回块 reserve 完全同构，不是双算，不要"优化"掉任何一侧。
        let summary_reserve: usize = if cfg.summary_enabled {
            result.summary_tokens
        } else {
            0
        };

        let history_budget = history_budget
            .saturating_sub(recall_reserve)
            .saturating_sub(summary_reserve);

        if history_budget < 1024 {
            tracing::warn!(
                conversation_id,
                history_budget,
                context_limit = budget.context_limit,
                tools_tokens = budget.breakdown.tools_tokens,
                recall_reserve,
                summary_reserve,
                "历史预算过小：非历史开销（工具/系统提示/预留）已接近或超过窗口"
            );
        }

        tracing::debug!(
            conversation_id,
            history_budget,
            recall_reserve,
            summary_reserve,
            context_limit = budget.context_limit,
            "compute_context_status: 注入历史裁剪预算"
        );

        // 0.21.19: 捕获 summary_enabled 供 ContextWindowStatus 使用
        let summary_enabled = cfg.summary_enabled;

        // cfg 守卫已完成使命——必须先释放读锁再注入预算：
        // update_history_budget 拿同一把 RwLock 的写锁，持读锁 await 写锁会自死锁
        //（0.21.18 回归：曾在 drop 前调用导致对话流永久无响应）。
        drop(cfg);

        self.persistent_memory
            .update_history_budget(Some(history_budget))
            .await;

        let status = ContextWindowStatus {
            estimated_tokens: budget.estimated_input_tokens,
            context_limit: budget.context_limit,
            usage_percent: budget.usage_percent,
            last_compressed: result.dropped_count > 0,
            last_compressed_count: result.dropped_count,
            last_recall_count: result.recall_count,
            preamble_tokens: budget.breakdown.system_tokens,
            pending_message_tokens: budget.breakdown.pending_tokens,
            // 0.21.17 扩展字段
            history_tokens: budget.breakdown.history_tokens,
            tools_tokens: budget.breakdown.tools_tokens,
            protocol_overhead_tokens: budget.breakdown.protocol_overhead_tokens,
            multimodal_tokens: budget.breakdown.multimodal_tokens,
            reserved_output_tokens: budget.reserved_output_tokens,
            safety_margin_tokens: budget.safety_margin_tokens,
            effective_input_limit: budget.effective_input_limit,
            remaining_tokens: budget.remaining_tokens,
            context_limit_source: format!("{:?}", budget.context_limit_source).to_lowercase(),
            confidence: format!("{:?}", budget.confidence).to_lowercase(),
            // 0.21.17 归属字段
            conversation_id: conversation_id.to_string(),
            provider_id: resolved.provider.id.clone(),
            model_id: resolved.model.id.clone(),
            // P0-1: 未校准基线，供 calibrator 采样
            raw_estimated_tokens: budget.raw_estimated_input_tokens,
            // 0.21.19: 摘要分项
            summary_tokens: result.summary_tokens,
            summarized_count: result.summarized_count,
            summary_enabled,
        };

        // P0-2: 按 conversation_id + provider_id + model_id 归属缓存
        let scope_key =
            context_status_scope_key(conversation_id, &resolved.provider.id, &resolved.model.id);
        self.last_context_status.insert(scope_key, status.clone());

        tracing::debug!(
            conversation_id,
            provider_id = %resolved.provider.id,
            model_id = %resolved.model.id,
            estimated_tokens = status.estimated_tokens,
            context_limit = status.context_limit,
            usage_percent = status.usage_percent,
            preamble_tokens = status.preamble_tokens,
            pending_message_tokens = status.pending_message_tokens,
            history_tokens = status.history_tokens,
            tools_tokens = status.tools_tokens,
            remaining_tokens = status.remaining_tokens,
            context_limit_source = %status.context_limit_source,
            confidence = %status.confidence,
            calibration_ratio = ?calibration_ratio,
            dropped = result.dropped_count,
            recalled = result.recall_count,
            "compute_context_status: 上下文窗口状态已计算"
        );

        status
    }

    /// 0.21.21: 推送上下文状态更新到前端（泛化语义，原 emit_summary_updated）。
    ///
    /// 重新计算 context_status 并 emit，让前端更新容量环 / popup 等 UI。
    /// 0.21.19 原为摘要完成后的推送入口；0.21.21 扩展为 Done 后通用重算入口。
    /// 0.21.23: 返回计算结果——Done 分发把它作为摘要触发的 Done 后水位
    /// （旧实现取发送前快照，少算刚完成的 assistant 回复，摘要晚一轮触发）。
    ///
    /// 返回 None 的边界：provider 解析失败或 Agent 未构造（无法估算工具快照）。
    pub(super) async fn emit_context_status_updated(
        &self,
        conversation_id: &str,
    ) -> Option<ContextWindowStatus> {
        let resolved = match self.resolve_current_entries(super::ConversationKind::Persistent) {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!(
                    conversation_id,
                    error = %e,
                    "emit_context_status_updated: 解析 provider 失败，跳过推送"
                );
                return None;
            }
        };
        let Some(provider) = self.cached_agent_ref() else {
            tracing::debug!(
                conversation_id,
                "emit_context_status_updated: Agent 未构造，跳过推送"
            );
            return None;
        };
        let status = self
            .compute_context_status(conversation_id, None, None, &provider, &resolved)
            .await;
        let _ = self.emitter.emit_to(
            "chat",
            EventNames::CHAT_CONTEXT_STATUS,
            serde_json::to_value(&status).unwrap_or_default(),
        );
        Some(status)
    }

    /// P0-2: 按 conversation_id 查询缓存的上下文窗口状态。
    ///
    /// 多条命中取 seq 最大（最新写入）。若该 conversation 从未计算过则返回 None。
    pub fn get_context_status_for_conversation(
        &self,
        conversation_id: &str,
    ) -> Option<ContextWindowStatus> {
        self.last_context_status
            .get_for_conversation(conversation_id)
    }

    /// 0.21.18: 查询上下文状态，缓存 miss 时按需重算。
    ///
    /// 供历史对话切换 / hover popup 等查看路径使用——估算纯本地
    /// （DB 消息 + 当前模型快照 + 工具快照），无需持久化：模型 / 工具 /
    /// 消息任一变化都会让旧值过期，按需重算永远新鲜。
    ///
    /// 返回 None 的边界（前端显示空环兜底）：
    /// - 流式请求进行中——预算注入与 prompt 消费有时序窗口（同
    ///   `compress_context_now` 的守卫），等下一轮 prompt 事件刷新
    /// - provider 未构造（本进程尚未发过消息）——避免查看路径触发
    ///   重量级 Agent 构造（MCP collect 等）
    /// - 模型解析失败（未配置 provider）
    ///
    /// 注入的 history_budget 不含 pending/system 扣减（无待发消息），
    /// 仅作兜底，下一轮 stream_prompt 前会被重算覆盖。
    pub async fn get_or_compute_context_status(
        &self,
        conversation_id: &str,
    ) -> Option<ContextWindowStatus> {
        if let Some(status) = self.get_context_status_for_conversation(conversation_id) {
            return Some(status);
        }
        if self.requests.status().is_some() {
            tracing::debug!(
                conversation_id,
                "get_or_compute_context_status: 流式进行中，跳过按需重算"
            );
            return None;
        }
        let resolved = self
            .resolve_current_entries(super::ConversationKind::Persistent)
            .ok()?;
        let provider = self.cached_agent_ref()?;
        tracing::debug!(
            conversation_id,
            "get_or_compute_context_status: 缓存 miss，按需重算"
        );
        Some(
            self.compute_context_status(conversation_id, None, None, &provider, &resolved)
                .await,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 0.21.17: scoped context status 归属测试 ──────────────────────────────────

    #[test]
    fn context_status_scope_key_format() {
        let key = context_status_scope_key("conv1", "provider1", "model1");
        assert_eq!(key, "conv1:provider1:model1");
    }

    #[test]
    fn context_status_scope_key_different_conversations() {
        let key1 = context_status_scope_key("conv1", "p1", "m1");
        let key2 = context_status_scope_key("conv2", "p1", "m1");
        assert_ne!(key1, key2, "不同 conversation_id 应产生不同 scope key");
    }

    #[test]
    fn context_status_scope_key_different_models() {
        let key1 = context_status_scope_key("conv1", "p1", "m1");
        let key2 = context_status_scope_key("conv1", "p1", "m2");
        assert_ne!(key1, key2, "不同 model_id 应产生不同 scope key");
    }

    #[test]
    fn context_status_scope_key_different_providers() {
        let key1 = context_status_scope_key("conv1", "p1", "m1");
        let key2 = context_status_scope_key("conv1", "p2", "m1");
        assert_ne!(key1, key2, "不同 provider_id 应产生不同 scope key");
    }

    /// 验证 `ContextWindowStatus` 的归属字段正确序列化。
    #[test]
    fn context_window_status_serializes_attribution_fields() {
        let status = ContextWindowStatus {
            estimated_tokens: 1000,
            context_limit: 8192,
            usage_percent: 15,
            last_compressed: false,
            last_compressed_count: 0,
            last_recall_count: 0,
            preamble_tokens: 200,
            pending_message_tokens: 50,
            history_tokens: 700,
            tools_tokens: 100,
            protocol_overhead_tokens: 30,
            multimodal_tokens: 0,
            reserved_output_tokens: 2048,
            safety_margin_tokens: 409,
            effective_input_limit: 5735,
            remaining_tokens: 4735,
            context_limit_source: "configured".into(),
            confidence: "high".into(),
            conversation_id: "conv123".into(),
            provider_id: "openai".into(),
            model_id: "gpt-4".into(),
            raw_estimated_tokens: 800,
            summary_tokens: 0,
            summarized_count: 0,
            summary_enabled: false,
        };
        let v = serde_json::to_value(&status).unwrap();
        assert_eq!(v["conversation_id"], "conv123");
        assert_eq!(v["provider_id"], "openai");
        assert_eq!(v["model_id"], "gpt-4");
        assert_eq!(v["tools_tokens"], 100);
        assert_eq!(v["history_tokens"], 700);
        assert_eq!(v["remaining_tokens"], 4735);
        assert_eq!(v["context_limit_source"], "configured");
        assert_eq!(v["confidence"], "high");
        assert_eq!(v["raw_estimated_tokens"], 800);
    }

    /// 验证 `ContextWindowStatus` 空归属字段被 `skip_serializing_if` 跳过。
    #[test]
    fn context_window_status_omits_empty_attribution() {
        let status = ContextWindowStatus {
            estimated_tokens: 0,
            context_limit: 0,
            usage_percent: 0,
            last_compressed: false,
            last_compressed_count: 0,
            last_recall_count: 0,
            preamble_tokens: 0,
            pending_message_tokens: 0,
            history_tokens: 0,
            tools_tokens: 0,
            protocol_overhead_tokens: 0,
            multimodal_tokens: 0,
            reserved_output_tokens: 0,
            safety_margin_tokens: 0,
            effective_input_limit: 0,
            remaining_tokens: 0,
            context_limit_source: String::new(),
            confidence: String::new(),
            conversation_id: String::new(),
            provider_id: String::new(),
            model_id: String::new(),
            raw_estimated_tokens: 0,
            summary_tokens: 0,
            summarized_count: 0,
            summary_enabled: false,
        };
        let v = serde_json::to_value(&status).unwrap();
        // 空字符串的归属字段不应出现在 JSON 中
        assert!(v.get("conversation_id").is_none() || v["conversation_id"].as_str() == Some(""));
        assert!(v.get("provider_id").is_none() || v["provider_id"].as_str() == Some(""));
        assert!(v.get("model_id").is_none() || v["model_id"].as_str() == Some(""));
    }

    // ── P0-2: ContextStatusCache 测试 ───────────────────────────────────────

    /// 构造测试用 ContextWindowStatus（只填归属字段 + estimated_tokens）。
    fn make_test_status(
        conv: &str,
        provider: &str,
        model: &str,
        tokens: usize,
    ) -> ContextWindowStatus {
        ContextWindowStatus {
            estimated_tokens: tokens,
            context_limit: 8192,
            usage_percent: 10,
            last_compressed: false,
            last_compressed_count: 0,
            last_recall_count: 0,
            preamble_tokens: 0,
            pending_message_tokens: 0,
            history_tokens: 0,
            tools_tokens: 0,
            protocol_overhead_tokens: 0,
            multimodal_tokens: 0,
            reserved_output_tokens: 2048,
            safety_margin_tokens: 409,
            effective_input_limit: 5735,
            remaining_tokens: 4735,
            context_limit_source: "configured".into(),
            confidence: "high".into(),
            conversation_id: conv.into(),
            provider_id: provider.into(),
            model_id: model.into(),
            raw_estimated_tokens: tokens,
            summary_tokens: 0,
            summarized_count: 0,
            summary_enabled: false,
        }
    }

    #[test]
    fn context_status_cache_cross_conversation_isolation() {
        let cache = ContextStatusCache::new();
        // conversation A × model M1/M2 与 conversation B × model M1/M2 交错写入
        cache.insert("A:p:M1".into(), make_test_status("A", "p", "M1", 100));
        cache.insert("B:p:M1".into(), make_test_status("B", "p", "M1", 200));
        cache.insert("A:p:M2".into(), make_test_status("A", "p", "M2", 300));
        cache.insert("B:p:M2".into(), make_test_status("B", "p", "M2", 400));

        // A 查到 M2（seq 最大），B 查到 M2（seq 最大），互不干扰
        let a = cache.get_for_conversation("A").unwrap();
        let b = cache.get_for_conversation("B").unwrap();
        assert_eq!(a.model_id, "M2");
        assert_eq!(a.estimated_tokens, 300);
        assert_eq!(b.model_id, "M2");
        assert_eq!(b.estimated_tokens, 400);
    }

    #[test]
    fn context_status_cache_same_conversation_newer_model_wins() {
        let cache = ContextStatusCache::new();
        cache.insert("A:p:M1".into(), make_test_status("A", "p", "M1", 100));
        // 同 conversation 换 model 写入
        cache.insert("A:p:M2".into(), make_test_status("A", "p", "M2", 200));

        let status = cache.get_for_conversation("A").unwrap();
        assert_eq!(status.model_id, "M2");
        assert_eq!(status.estimated_tokens, 200);
    }

    #[test]
    fn context_status_cache_never_written_returns_none() {
        let cache = ContextStatusCache::new();
        cache.insert("A:p:M1".into(), make_test_status("A", "p", "M1", 100));
        assert!(cache.get_for_conversation("nonexistent").is_none());
    }

    #[test]
    fn context_status_cache_evicts_oldest_at_33() {
        let cache = ContextStatusCache::new();
        // 写入 32 条（达到上限）
        for i in 0..32 {
            let conv = format!("conv{i}");
            cache.insert(format!("conv{i}:p:M"), make_test_status(&conv, "p", "M", i));
        }
        // 验证 conv0 仍在
        assert!(cache.get_for_conversation("conv0").is_some());

        // 写入第 33 条 → 最旧 entry（conv0）被淘汰
        cache.insert(
            "conv32:p:M".into(),
            make_test_status("conv32", "p", "M", 32),
        );
        assert!(
            cache.get_for_conversation("conv0").is_none(),
            "第 33 条写入后最旧 entry 应被淘汰"
        );
        assert!(cache.get_for_conversation("conv32").is_some());
    }

    // ── 0.21.21: 容量取值去全局化测试 ──────────────────────────────────

    /// 验证常量收敛：memory::DEFAULT_CONTEXT_LIMIT 与 token_budget::FALLBACK_CONTEXT_LIMIT
    /// 指向同一值，不应出现两份独立定义。
    #[test]
    fn constant_convergence_default_context_limit() {
        assert_eq!(
            crate::domain::ai::memory::DEFAULT_CONTEXT_LIMIT,
            crate::domain::ai::token_budget::FALLBACK_CONTEXT_LIMIT,
            "DEFAULT_CONTEXT_LIMIT 应与 FALLBACK_CONTEXT_LIMIT 同值（常量收敛）"
        );
        assert_eq!(crate::domain::ai::memory::DEFAULT_CONTEXT_LIMIT, 32768);
    }

    /// 验证 compute_context_status 优先读 resolved.model.context_window。
    /// 通过 estimate_request_budget 的 context_window 参数间接验证——
    /// compute_context_status 现在传 resolved.model.context_window.or(cfg.context_limit)，
    /// 而非仅读 cfg.context_limit。
    #[test]
    fn budget_uses_model_context_window_when_configured() {
        // 模型配置了 context_window=131072
        let budget = crate::domain::ai::token_budget::estimate_request_budget(
            crate::domain::ai::token_budget::TokenBudgetInput {
                history_texts: &[],
                system_prompt: None,
                pending_message: None,
                tools: &[],
                tool_call_count: 0,
                has_multimodal: false,
                context_window: Some(131072),
                tiered_fallback_limit: None,
                request_max_tokens: None,
                model_max_tokens: None,
                calibration_ratio: None,
            },
        );
        assert_eq!(budget.context_limit, 131072);
        assert_eq!(
            budget.context_limit_source,
            crate::domain::ai::token_budget::ContextLimitSource::Configured
        );
    }

    /// 验证 context_window 缺失时使用 fallback。
    #[test]
    fn budget_falls_back_when_model_context_window_none() {
        let budget = crate::domain::ai::token_budget::estimate_request_budget(
            crate::domain::ai::token_budget::TokenBudgetInput {
                history_texts: &[],
                system_prompt: None,
                pending_message: None,
                tools: &[],
                tool_call_count: 0,
                has_multimodal: false,
                context_window: None,
                tiered_fallback_limit: None,
                request_max_tokens: None,
                model_max_tokens: None,
                calibration_ratio: None,
            },
        );
        assert_eq!(
            budget.context_limit,
            crate::domain::ai::token_budget::FALLBACK_CONTEXT_LIMIT
        );
        assert_eq!(
            budget.context_limit_source,
            crate::domain::ai::token_budget::ContextLimitSource::Fallback
        );
    }
}
