//! SQLite 持久化对话记忆（0.12.3 Phase A → 0.13.1 token-aware 压缩）。
//!
//! 实现 rig `ConversationMemory` trait，将对话历史持久化到 `blink_ai.db`。
//! 替代 0.12.1 的 `InMemoryConversationMemory`（进程内，重启丢）。
//!
//! ## 窗口策略（0.13.1）
//!
//! 两种模式：
//! - **FixedCount**（0.12.3 行为，向后兼容）：固定返回最近 N 条消息
//! - **TokenAware**（0.13.1 默认）：估算窗口总 token，超限则从旧端移出
//!
//! TokenAware 模式下：
//! 1. 加载较大批次（200 条）消息
//! 2. 估算总 token
//! 3. 总 token > limit * trigger_ratio（默认 0.8）→ 从旧端移出
//! 4. 移出到总 token ≤ limit * compress_ratio（默认 0.7）
//! 5. 移出后复用 `drop_leading_orphan_tool_results()` 处理孤立 ToolResult
//!
//! **DB 保留完整历史**——load() 裁剪是 in-memory 的，DB 消息不丢。
//!
//! ## token 估算（0.13.1 §3.3）
//!
//! 启发式估算（不引入 tokenizer 依赖）：
//! - CJK 字符占比 > 30% → chars / 1.5（中文档）
//! - 否则 → chars / 4（英文件，OpenAI 经验值）
//!
//! 误差 ±20% 可接受——压缩阈值留 buffer（80% 触发，不要求精确）。
//!
//! ## 自动创建对话
//!
//! `append()` 时若 conversation 记录不存在，自动创建（`INSERT OR IGNORE`）。
//! 标题从首条 User 消息提取（前 50 字符），无 User 消息时为空。
//!
//! ## content 序列化
//!
//! `messages.content` 列存 `serde_json::to_string(&Message)`，完整保留
//! text / tool_call / tool_result。`Message` 有 `Serialize/Deserialize`。

use std::collections::HashMap;
use std::sync::Arc;

use rig_core::completion::message::{AssistantContent, Reasoning, Text, UserContent};
use rig_core::completion::Message;
use rig_core::memory::{ConversationMemory, MemoryError};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

/// 滑动窗口大小（FixedCount 模式，0.12.3 §5.3：最近 20 条消息）。
const SLIDING_WINDOW_SIZE: i64 = 20;

/// 临时对话最大保留消息数（0.21.18）。超出丢最旧——主窗口临时对话通常
/// 短轮次，上限只为防长会话内存与请求无界增长；不做 token 感知压缩。
const MAX_EPHEMERAL_MESSAGES: usize = 50;

/// TokenAware 模式加载批次大小（加载较多消息后 in-memory 裁剪）。
pub const TOKEN_AWARE_LOAD_BATCH: i64 = 200;

/// 标题最大字符数（从首条 User 消息提取）。
const TITLE_MAX_CHARS: usize = 50;

/// 保守默认 context limit（ModelEntry.context_window 缺失时使用）。
///
/// 0.21.21: 常量收敛到 `token_budget::FALLBACK_CONTEXT_LIMIT`，此处重导出保持
/// 外部调用路径 `crate::domain::ai::memory::DEFAULT_CONTEXT_LIMIT` 兼容。
pub use crate::domain::ai::token_budget::FALLBACK_CONTEXT_LIMIT as DEFAULT_CONTEXT_LIMIT;


// 0.21.17: token 估算统一收敛到 `token_budget` 模块，此处仅重导出。
pub use crate::domain::ai::token_budget::estimate_text_tokens as estimate_tokens;
#[allow(unused_imports)]
pub use crate::domain::ai::token_budget::is_cjk;

// ── MemoryLoadResult（0.13.6）──────────────────────────────────────────────────

/// `load_with_stats()` 返回值——消息 + 压缩/召回统计（0.13.6）。
///
/// 供 `ChatService::compute_context_status()` 计算上下文窗口占用指示器数据。
#[derive(Debug, Clone)]
pub struct MemoryLoadResult {
    /// 加载的消息列表（已裁剪 + 已召回注入 + 摘要块注入）。
    pub messages: Vec<Message>,
    /// 本次 load 被裁剪移出的消息数（token_aware_truncate）。
    pub dropped_count: usize,
    /// 本次 load FTS5 召回的消息数。
    pub recall_count: usize,
    /// 0.21.19: 被摘要覆盖（水位线以下）的消息数。
    pub summarized_count: usize,
    /// 0.21.19: 估算的摘要块 token 数（注入窗口的 `<summary>` 块）。
    pub summary_tokens: usize,
}

// ── MemoryConfig（0.13.1）──────────────────────────────────────────────────────

/// 记忆窗口模式。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum WindowMode {
    /// 固定条数滑动窗口（0.12.3 行为，向后兼容）。
    FixedCount,
    /// Token-aware 窗口（0.13.1 默认）。
    #[default]
    TokenAware,
}

/// 0.21.20: FTS5 召回范围（跨对话召回开关）。
///
/// 控制召回检索的 `conversation_id` 范围：仅当前对话或跨所有对话。
/// AllConversations 时必须排除 `__` 前缀的内部临时对话（摘要/合并任务产生的 orphan）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum RecallScope {
    /// 仅当前对话（默认，向后兼容）。
    #[default]
    ThisConversation,
    /// 跨所有对话（排除 `__` 前缀内部对话）。
    AllConversations,
}

/// 记忆策略配置（0.13.1 + 0.13.2）。
///
/// 存储在配置库 `memory:config` key 下。`context_limit` 由 `ModelEntry.context_window`
/// 在运行时注入（模型切换时更新），不从配置文件读取。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MemoryConfig {
    /// 窗口模式。
    #[serde(default)]
    pub mode: WindowMode,

    /// FixedCount 模式的窗口大小。
    #[serde(default = "default_window_size")]
    pub window_size: i64,

    /// Context token 上限（从 `ModelEntry.context_window` 注入）。
    /// None 时使用保守默认（32K）。
    #[serde(skip)] // 运行时注入，不持久化
    pub context_limit: Option<usize>,

    /// 触发裁剪的 token 占比（默认 0.8）。
    ///
    /// 基数是「历史可用预算」而非裸 context_limit——
    /// 预算已扣除系统提示、工具定义、输出预留、安全余量、召回块预留。
    /// 0.8 是刻意双重缓冲（预算本身已含安全余量）。
    #[serde(default = "default_trigger_ratio")]
    pub trigger_ratio: f64,

    /// 裁剪目标 token 占比（默认 0.7）。
    ///
    /// 基数同 `trigger_ratio`——历史可用预算。裁剪后剩余 token ≤ 预算 × compress_ratio。
    #[serde(default = "default_compress_ratio")]
    pub compress_ratio: f64,

    /// 历史可用 token 预算（运行时注入，0.21.18）。
    ///
    /// = context_limit − (system + tools + pending + 输出预留 + 安全余量 + 召回块预留)。
    /// None 时回退旧逻辑（裸 context_limit × trigger/compress）。
    /// 每轮 prompt 由 `compute_context_status` 注入；模型切换时置 None（等下一轮重新注入）。
    #[serde(skip)]
    pub history_budget: Option<usize>,

    /// FTS5 召回开关（0.13.2，默认开）。
    /// 开启后，被窗口裁剪的旧消息归档到 FTS5，load() 时 BM25 检索召回相关旧上下文。
    #[serde(default = "default_recall_enabled")]
    pub recall_enabled: bool,

    /// FTS5 召回 Top-K（0.13.2，默认 3）。
    /// 每次召回的最多消息条数。
    #[serde(default = "default_recall_top_k")]
    pub recall_top_k: i64,

    /// 0.21.19: 启用摘要压缩（默认 false）。
    ///
    /// 开启后超出窗口的旧消息会被 LLM 压缩为摘要（调用当前模型，消耗少量 token）；
    /// 关闭则仅裁剪归档（纯截断行为）。已生成的摘要不删除，重新开启后继续可用。
    #[serde(default)]
    pub summary_enabled: bool,

    /// 0.21.20: 召回范围（默认 ThisConversation）。
    ///
    /// AllConversations 时 FTS5 检索跨所有对话的已归档消息（排除 `__` 前缀内部对话）。
    #[serde(default)]
    pub recall_scope: RecallScope,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: default_trigger_ratio(),
            compress_ratio: default_compress_ratio(),
            history_budget: None,
            recall_enabled: default_recall_enabled(),
            recall_top_k: default_recall_top_k(),
            summary_enabled: false,
            recall_scope: RecallScope::default(),
        }
    }
}

fn default_window_size() -> i64 {
    SLIDING_WINDOW_SIZE
}

fn default_trigger_ratio() -> f64 {
    0.8
}

fn default_compress_ratio() -> f64 {
    0.7
}

fn default_recall_enabled() -> bool {
    true
}

fn default_recall_top_k() -> i64 {
    3
}

// 0.21.17: `is_cjk` 和 `estimate_tokens` 已收敛到 `token_budget` 模块。
// 此处通过上方 `pub use` 重导出，保持外部调用路径 `crate::domain::ai::memory::estimate_tokens` 兼容。

/// 从 `Message` 提取文本内容（用于 token 估算）。
///
/// 提取 Text / ToolCall / ToolResult 的文本部分，忽略图片等二进制内容。
pub fn extract_message_text(msg: &Message) -> String {
    let mut text = String::new();
    match msg {
        Message::System { content } => {
            text.push_str(content);
        }
        Message::User { content } => {
            for item in content.iter() {
                match item {
                    UserContent::Text(t) => text.push_str(&t.text),
                    UserContent::ToolResult(tr) => {
                        for c in tr.content.iter() {
                            if let rig_core::completion::message::ToolResultContent::Text(t) = c {
                                text.push_str(&t.text);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        Message::Assistant { content, .. } => {
            for item in content.iter() {
                match item {
                    AssistantContent::Text(t) => text.push_str(&t.text),
                    AssistantContent::ToolCall(tc) => {
                        text.push_str(&tc.function.name);
                        text.push_str(&tc.function.arguments.to_string());
                    }
                    _ => {}
                }
            }
        }
    }
    text
}

/// 估算消息列表的总 token 数。
#[allow(dead_code)] // 测试中使用；token_aware_truncate 优化后已内联预计算
pub fn estimate_messages_tokens(messages: &[Message]) -> usize {
    messages
        .iter()
        .map(|m| estimate_tokens(&extract_message_text(m)))
        .sum()
}

// ── token-aware 裁剪 ──────────────────────────────────────────────────────────

/// Token-aware 窗口裁剪（0.13.1 §3.4 + 0.13.2 归档）。
///
/// 如果总 token > limit * trigger_ratio，从旧端逐条移出直到总 token ≤ limit * compress_ratio。
/// 返回被移出的消息（供 0.13.2 FTS5 归档）。
pub fn token_aware_truncate(
    messages: &mut Vec<Message>,
    context_limit: usize,
    trigger_ratio: f64,
    compress_ratio: f64,
) -> Vec<Message> {
    // 预计算每条消息的 token 数（O(n)），避免循环内重复估算（原 O(n²)）
    let per_message_tokens: Vec<usize> = messages
        .iter()
        .map(|m| estimate_tokens(&extract_message_text(m)))
        .collect();

    // 0.21.19.1: 裁剪判定复用 compute_truncate_boundary 纯函数（唯一真源）
    let Some(drop_count) = compute_truncate_boundary(
        &per_message_tokens,
        context_limit,
        trigger_ratio,
        compress_ratio,
    ) else {
        return Vec::new();
    };

    let dropped_tokens: usize = per_message_tokens[..drop_count].iter().sum();
    let total_tokens: usize = per_message_tokens.iter().sum();

    // 一次性 drain，比逐条 remove(0) 高效（O(drop_count) vs O(n*drop_count)）
    let dropped: Vec<Message> = messages.drain(0..drop_count).collect();

    if !dropped.is_empty() {
        tracing::debug!(
            dropped_count = dropped.len(),
            remaining = messages.len(),
            total_tokens_before = total_tokens,
            total_tokens_after = total_tokens - dropped_tokens,
            context_limit,
            "token-aware 裁剪：从旧端移出消息"
        );
    }

    dropped
}

/// 0.21.19.1: 计算基于 token 的截断边界（纯函数，`load_inner` 与摘要任务共用）。
///
/// 输入 `messages_tokens` 按时间顺序排列（旧 → 新），每元素为单条消息的 token 数。
/// 若总 token 超过 `budget × trigger_ratio`，则从旧端逐条计入需移出的数量，
/// 直到剩余 token ≤ `budget × compress_ratio`，返回需移出的消息条数。
/// 总 token 未达触发阈值时返回 `None`（无需裁剪）。
///
/// **这是裁剪判定的唯一真源**——`load_inner` 的 `token_aware_truncate` 与摘要任务的
/// 压缩边界计算共用此函数，避免出现两份口径各算各的（0.21.19 的 P0 缺陷根因）。
pub fn compute_truncate_boundary(
    messages_tokens: &[usize],
    budget: usize,
    trigger_ratio: f64,
    compress_ratio: f64,
) -> Option<usize> {
    let trigger_threshold = (budget as f64 * trigger_ratio) as usize;
    let compress_target = (budget as f64 * compress_ratio) as usize;

    let total_tokens: usize = messages_tokens.iter().sum();
    if total_tokens <= trigger_threshold {
        return None;
    }

    // 从旧端逐条计入需移出的数量，直到剩余 ≤ compress_target
    let mut dropped_tokens = 0usize;
    let mut drop_count = 0;
    for &tokens in messages_tokens {
        if total_tokens - dropped_tokens <= compress_target {
            break;
        }
        dropped_tokens += tokens;
        drop_count += 1;
    }

    if drop_count == 0 {
        None
    } else {
        Some(drop_count)
    }
}

// ── SqliteConversationMemory ──────────────────────────────────────────────────

/// SQLite 持久化对话记忆。
///
/// 持有 `SqlitePool`（AI 库）+ `MemoryConfig`（窗口策略），实现 rig
/// `ConversationMemory` trait。Agent 构造时注入，rig 自动按 `conversation_id`
/// load/append/clear。
///
/// `config` 用 `Arc<RwLock<>>` 包裹，支持运行时更新（模型切换时注入
/// `context_limit`）。`ChatService` 持有同一 `Arc` 的克隆，在 `ensure_provider`
/// 时更新 `context_limit`。
pub struct SqliteConversationMemory {
    pool: SqlitePool,
    config: Arc<RwLock<MemoryConfig>>,
    /// 流式「实况回合」标记：conversation_id -> 已写出的部分 assistant 消息 id。
    ///
    /// 正常跑完时 `append` 据此删除部分回复行、用 rig 的最终完整消息替换；
    /// 中断/崩溃时该行保留在 DB，让用户下次进入能看到断在哪。
    live_turns: RwLock<HashMap<String, i64>>,
    /// 「发出即保存」预写的当前 user（conversation_id -> user 文本）。
    ///
    /// 预写保证中断/失败时用户消息已落库；但 rig 的 `stream_prompt` 会先把
    /// 记忆 load 出来再**追加一次**当前 prompt——若 load 也带上这条预写 user，
    /// 请求上下文里同一 user 就会出现两次（模型看到"用户询问了我两次"）。
    /// `load` 据此丢弃尾部匹配的预写 user；`append` 完成本轮后清除标记。
    pending_users: RwLock<HashMap<String, String>>,
}

impl SqliteConversationMemory {
    /// 构造默认配置的 memory（TokenAware 模式，context_limit 用保守默认）。
    pub fn new(pool: SqlitePool) -> Self {
        Self::with_config(pool, Arc::new(RwLock::new(MemoryConfig::default())))
    }

    /// 构造带共享配置的 memory。
    ///
    /// `config` 由调用方持有克隆，用于运行时更新 `context_limit`（模型切换时）。
    pub fn with_config(pool: SqlitePool, config: Arc<RwLock<MemoryConfig>>) -> Self {
        Self {
            pool,
            config,
            live_turns: RwLock::new(HashMap::new()),
            pending_users: RwLock::new(HashMap::new()),
        }
    }

    /// 获取共享配置句柄（供设置页 IPC 读写 memory 配置，0.13.1.4 将使用）。
    #[allow(dead_code)]
    pub fn config_handle(&self) -> Arc<RwLock<MemoryConfig>> {
        self.config.clone()
    }

    /// 0.21.19: 获取数据库连接池引用（供摘要任务等需要直接访问 DB 的场景）。
    ///
    /// 摘要任务需要读取水位、加载被裁消息、插入摘要——这些都在 data 层，
    /// 通过 pool 直接调用，不经过 rig memory trait。
    pub fn pool(&self) -> &SqlitePool {
        &self.pool
    }

    /// 更新 context_limit（模型切换时调用）。
    ///
    /// `limit` 为 None 时使用保守默认（32K）。
    ///
    /// 0.21.18: 同时将 `history_budget` 置 None——旧预算基于旧窗口，必须失效，
    /// 等下一轮 `compute_context_status` 重新注入。
    pub async fn update_context_limit(&self, limit: Option<usize>) {
        let mut cfg = self.config.write().await;
        cfg.context_limit = limit.or(Some(DEFAULT_CONTEXT_LIMIT));
        cfg.history_budget = None;
    }

    /// 注入历史可用预算（每轮 prompt 由 `compute_context_status` 调用，0.21.18）。
    ///
    /// `budget` 为 None 时回退到裸 `context_limit` 基准。
    pub async fn update_history_budget(&self, budget: Option<usize>) {
        let mut cfg = self.config.write().await;
        cfg.history_budget = budget;
    }

    /// 0.13.6: 带统计的 load——返回消息 + 压缩/召回统计。
    ///
    /// 与 `ConversationMemory::load()` 逻辑一致，额外返回 dropped_count / recall_count。
    /// 供 `ChatService::compute_context_status()` 计算上下文窗口占用指示器。
    ///
    /// **注意**：此方法与 rig Agent 内部调用的 `load()` 不会冲突——
    /// `load()` 的裁剪是 in-memory 的（DB 保留完整历史），FTS5 归档幂等（content hash 去重）。
    pub async fn load_with_stats(
        &self,
        conversation_id: &str,
    ) -> Result<MemoryLoadResult, MemoryError> {
        self.load_inner(conversation_id).await
    }

    /// load 核心逻辑——加载 + 裁剪 + 归档 + 召回，返回消息 + 统计（0.13.6）。
    ///
    /// `ConversationMemory::load()` 和 `load_with_stats()` 的共用底层。
    async fn load_inner(&self, conversation_id: &str) -> Result<MemoryLoadResult, MemoryError> {
        let pool = self.pool.clone();
        let config = self.config.clone();
        let cfg = config.read().await;

        let load_limit = match cfg.mode {
            WindowMode::FixedCount => cfg.window_size,
            WindowMode::TokenAware => TOKEN_AWARE_LOAD_BATCH,
        };

        // 0.21.19: 摘要启用时，读水位并跳过已摘要的消息
        let mut summarized_count = 0usize;
        let mut summary_tokens = 0usize;

        let rows = if cfg.summary_enabled {
            let watermark =
                crate::infra::data::conversations::get_summarized_until(&pool, conversation_id)
                    .await
                    .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            let (rows, skipped) =
                crate::infra::data::conversations::load_recent_messages_after_watermark(
                    &pool,
                    conversation_id,
                    load_limit,
                    watermark,
                )
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            summarized_count = skipped;
            rows
        } else {
            crate::infra::data::conversations::load_recent_messages(
                &pool,
                conversation_id,
                load_limit,
            )
            .await
            .map_err(|e| MemoryError::Backend(Box::from(e)))?
        };

        let mut messages = Vec::with_capacity(rows.len());
        for (_role, content) in rows {
            match serde_json::from_str::<Message>(&content) {
                Ok(msg) => messages.push(msg),
                Err(e) => {
                    // 0.42: rig 0.39 历史消息格式不兼容（字段结构变化）。
                    // 不删数据、不 panic——跳过损坏行，让用户继续使用对话。
                    // 打 warn 日志供排查，后续可加迁移逻辑。
                    tracing::warn!(
                        error = %e,
                        role = %_role,
                        content_len = content.len(),
                        "load_inner: 消息反序列化失败，跳过（可能为旧版格式）"
                    );
                    continue;
                }
            }
        }

        let mut dropped_count = 0usize;

        // 0.13.1: token-aware 裁剪（仅 TokenAware 模式）
        // 0.13.2: 裁剪出的消息归档到 FTS5
        // 0.21.18: 裁剪基准从裸 context_limit 换为「历史可用预算」——
        // history_budget 已预扣 system/tools/pending/输出预留/安全余量/召回块预留。
        // Fallback 链：history_budget → context_limit → DEFAULT_CONTEXT_LIMIT
        if cfg.mode == WindowMode::TokenAware {
            let budget_base = cfg
                .history_budget
                .or(cfg.context_limit)
                .unwrap_or(DEFAULT_CONTEXT_LIMIT);
            let dropped_msgs = token_aware_truncate(
                &mut messages,
                budget_base,
                cfg.trigger_ratio,
                cfg.compress_ratio,
            );
            dropped_count = dropped_msgs.len();

            if !dropped_msgs.is_empty() {
                // 0.13.2: 归档被挤出的消息到 FTS5（幂等）
                for msg in &dropped_msgs {
                    let text = extract_message_text(msg);
                    if text.is_empty() {
                        continue;
                    }
                    let role = Self::message_role(msg);
                    let hash = Self::content_hash(&text);
                    if let Err(e) = crate::infra::data::conversations::archive_to_fts(
                        &pool,
                        conversation_id,
                        role,
                        &text,
                        &hash,
                    )
                    .await
                    {
                        tracing::warn!(error = %e, "FTS5 归档失败（非致命）");
                    }
                }
                tracing::info!(
                    conversation_id,
                    dropped = dropped_msgs.len(),
                    budget_base,
                    "归档钩子：{} 条消息已归档到 FTS5",
                    dropped_msgs.len()
                );
            }
        }

        // 0.12.8: 滑动窗口/token 裁剪可能截断 ToolCall/ToolResult 配对——
        // 丢弃开头所有孤立的 ToolResult 消息。
        drop_leading_orphan_tool_results(&mut messages);

        // 0.21.19: 摘要块注入——在 <memory> 块之前插入 <summary> 块
        if cfg.summary_enabled && summarized_count > 0 {
            let summaries =
                crate::infra::data::conversations::load_summaries(&pool, conversation_id)
                    .await
                    .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            if !summaries.is_empty() {
                let contents: Vec<String> = summaries.iter().map(|s| s.content.clone()).collect();
                summary_tokens =
                    crate::domain::ai::summary::estimate_summary_block_tokens(&contents);
                let block = crate::domain::ai::summary::format_summary_block(&contents);
                tracing::debug!(
                    conversation_id,
                    summary_segments = summaries.len(),
                    summary_tokens,
                    summarized_count,
                    "摘要块注入：{} 段摘要，{} 条消息已被覆盖",
                    summaries.len(),
                    summarized_count
                );
                messages.insert(0, Message::System { content: block });
            }
        }

        let mut recall_count = 0usize;

        // 0.13.2: FTS5 召回——从最后一条 User 消息提取 query，BM25 检索旧上下文
        if cfg.recall_enabled && cfg.recall_top_k > 0 {
            let query = Self::extract_last_user_text(&messages);
            if !query.is_empty() {
                match crate::infra::data::conversations::search_memory_fts(
                    &pool,
                    conversation_id,
                    &query,
                    cfg.recall_top_k,
                    cfg.recall_scope,
                )
                .await
                {
                    Ok(recalls) if !recalls.is_empty() => {
                        recall_count = recalls.len();
                        let memory_block = Self::format_recall_block(&recalls);
                        tracing::debug!(
                            conversation_id,
                            recall_count,
                            query = %query,
                            "FTS5 召回：注入 {} 条历史上下文",
                            recall_count
                        );
                        // 在窗口最前方插入 <memory> 系统消息
                        messages.insert(
                            0,
                            Message::System {
                                content: memory_block,
                            },
                        );
                    }
                    Ok(_) => {} // 无召回结果
                    Err(e) => {
                        tracing::warn!(error = %e, "FTS5 召回失败（非致命）");
                    }
                }
            }
        }

        // drop cfg guard before returning
        drop(cfg);

        // 0.21.16 修复：丢弃「发出即保存」预写的当前 user——rig 会把当前 prompt
        // 再追加一次，load 若带上这条预写 user，请求上下文里同一 user 就重复了。
        // 放在返回前丢弃，保证：FTS 召回 query 仍基于当前 user（它是最新消息）、
        // token-aware 裁剪把当前 user 计入预算（rig 会加回来，预算不能偏松）。
        // 仅当尾部 user 文本与 pending 标记一致才丢（防误伤历史里真实的同文 user）。
        if let Some(pending) = self
            .pending_users
            .read()
            .await
            .get(conversation_id)
            .cloned()
            && let Some(last) = messages.last()
            && matches!(last, Message::User { .. })
            && extract_message_text(last) == pending
        {
            messages.pop();
        }

        Ok(MemoryLoadResult {
            messages,
            dropped_count,
            recall_count,
            summarized_count,
            summary_tokens,
        })
    }

    /// 应用用户从设置页修改的记忆策略配置（0.13.1 §3.7）。
    ///
    /// `new_config` 来自 `AIConfig.chat_config.memory_config`（serde 反序列化，
    /// `context_limit` 为 None）。此方法保留运行时已注入的 `context_limit`，
    /// 只更新 `mode / window_size / trigger_ratio / compress_ratio` 四个用户可配字段。
    ///
    /// 调用时机：`set_config('ai_config')` 命令处理中，保存 DB 后调用。
    pub async fn apply_config(&self, mut new_config: MemoryConfig) {
        let mut cfg = self.config.write().await;
        // 保留运行时注入的 context_limit（来自 ModelEntry.context_window）
        new_config.context_limit = cfg.context_limit;
        // 0.21.18: 保留运行时注入的 history_budget（来自 compute_context_status）
        new_config.history_budget = cfg.history_budget;
        tracing::debug!(
            mode = ?new_config.mode,
            window_size = new_config.window_size,
            trigger_ratio = new_config.trigger_ratio,
            compress_ratio = new_config.compress_ratio,
            recall_enabled = new_config.recall_enabled,
            recall_top_k = new_config.recall_top_k,
            summary_enabled = new_config.summary_enabled,
            recall_scope = ?new_config.recall_scope,
            context_limit = ?new_config.context_limit,
            history_budget = ?new_config.history_budget,
            "apply_config: 更新记忆策略配置"
        );
        *cfg = new_config;
    }

    // ── 发出即保存 / 实况回合（0.21.16）─────────────────────────────────────

    /// 在 prompt 启动时（发出即保存）预写用户消息 + 建对话记录。
    ///
    /// 幂等去重：尾部已是相同 user 消息（重发/重试）则跳过，不产生重复行。
    /// rig 结束时的 `append` 会跳过这条已写的 user 消息，只补写 assistant。
    pub async fn persist_user_message(
        &self,
        conversation_id: &str,
        user_msg: &str,
    ) -> Result<(), String> {
        let pool = self.pool.clone();

        // 0.21.18: 清掉上一轮中断残留的实况标记——断点行留在 DB 供回溯，
        // 但下一轮不再复用其 id（否则新回合部分回复会覆盖旧断点内容）。
        self.live_turns.write().await.remove(conversation_id);

        // 去重：尾部已是同文 user 消息 → 已保存过，跳过
        if let Some((role, content)) =
            crate::infra::data::conversations::load_last_message(&pool, conversation_id).await?
            && role == "user"
            && Self::user_text_matches(&content, user_msg)
        {
            // 已预写过（如上一轮中断残留）：同样标记为 pending，让 load 丢弃
            // 这条将被 rig 追加的 user，避免请求上下文重复。
            self.pending_users
                .write()
                .await
                .insert(conversation_id.to_string(), user_msg.to_string());
            return Ok(());
        }

        let msg = Message::User {
            content: vec![UserContent::Text(Text::new(user_msg))],
        };
        let title: String = user_msg.chars().take(TITLE_MAX_CHARS).collect();
        crate::infra::data::conversations::create_conversation(
            &pool,
            conversation_id,
            if title.is_empty() { None } else { Some(&title) },
        )
        .await?;
        crate::infra::data::conversations::append_message(
            &pool,
            conversation_id,
            "user",
            &serde_json::to_string(&msg).map_err(|e| e.to_string())?,
        )
        .await?;
        crate::infra::data::conversations::touch_conversation(&pool, conversation_id).await?;
        // 标记本 user 为「待 rig 追加」——load 时丢弃尾部匹配项，避免请求上下文重复
        self.pending_users
            .write()
            .await
            .insert(conversation_id.to_string(), user_msg.to_string());
        Ok(())
    }

    /// 流式期间增量写入部分 assistant 回复（节流由调用方控制）。
    ///
    /// 首次写入 INSERT 并记录消息 id；后续 UPDATE 同一行。正常跑完时 `append`
    /// 会删除该行并用 rig 的最终完整消息替换，避免残留部分回复 / 重复行。
    /// 中断/崩溃时该行保留，供下次进入查看断点。
    pub async fn persist_assistant_delta(
        &self,
        conversation_id: &str,
        text: &str,
        thinking: &str,
    ) -> Result<(), String> {
        if text.trim().is_empty() && thinking.trim().is_empty() {
            return Ok(());
        }
        let pool = self.pool.clone();
        let msg = Self::build_assistant_message(text, thinking);
        let content = serde_json::to_string(&msg).map_err(|e| e.to_string())?;

        let mut live = self.live_turns.write().await;
        if let Some(id) = live.get(conversation_id).copied() {
            if crate::infra::data::conversations::update_message_content(&pool, id, &content)
                .await?
            {
                return Ok(());
            }
            // 行已被外部删除（如 truncate/clear）→ 回退为重新插入
            live.remove(conversation_id);
        }
        let id = crate::infra::data::conversations::append_message(
            &pool,
            conversation_id,
            "assistant",
            &content,
        )
        .await?;
        live.insert(conversation_id.to_string(), id);
        Ok(())
    }

    /// 构造部分 assistant 消息（Reasoning 在前、Text 在后，与 rig 落库顺序一致）。
    fn build_assistant_message(text: &str, thinking: &str) -> Message {
        let mut content: Vec<AssistantContent> = Vec::new();
        if !thinking.is_empty() {
            content.push(AssistantContent::Reasoning(Reasoning::new(thinking)));
        }
        if !text.is_empty() {
            content.push(AssistantContent::Text(Text::new(text)));
        }
        Message::Assistant { id: None, content }
    }

    /// 判断序列化消息的文本是否与预期 user 文本一致（去重用）。
    fn user_text_matches(content_json: &str, expected: &str) -> bool {
        let Ok(msg) = serde_json::from_str::<Message>(content_json) else {
            return false;
        };
        extract_message_text(&msg) == expected
    }

    /// 从 `Message` 变体提取 role 字符串（与 serde tag naming 一致）。
    fn message_role(msg: &Message) -> &'static str {
        match msg {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
        }
    }

    /// 从消息列表中提取首条 User 消息的文本作为标题。
    fn extract_title(messages: &[Message]) -> String {
        for msg in messages {
            if let Message::User { content } = msg {
                for item in content.iter() {
                    if let UserContent::Text(t) = item {
                        return t.text.chars().take(TITLE_MAX_CHARS).collect();
                    }
                }
            }
        }
        String::new()
    }

    /// 0.13.2: 计算消息文本的 content hash（用于 FTS5 幂等去重）。
    ///
    /// 用 FNV-1a 简单 hash——不需要密码学强度，只需确定性去重。
    fn content_hash(text: &str) -> String {
        // FNV-1a 64-bit
        const FNV_OFFSET: u64 = 0xcbf29ce484222325;
        const FNV_PRIME: u64 = 0x100000001b3;
        let mut hash = FNV_OFFSET;
        for byte in text.as_bytes() {
            hash ^= *byte as u64;
            hash = hash.wrapping_mul(FNV_PRIME);
        }
        format!("{hash:016x}")
    }

    /// 0.13.2: 从消息列表中提取最后一条 User 消息的文本（作为 FTS5 检索 query）。
    fn extract_last_user_text(messages: &[Message]) -> String {
        for msg in messages.iter().rev() {
            if let Message::User { content } = msg {
                for item in content.iter() {
                    if let UserContent::Text(t) = item
                        && !t.text.is_empty()
                    {
                        return t.text.clone();
                    }
                }
            }
        }
        String::new()
    }

    /// 0.13.2: 格式化 FTS5 召回结果为 <memory> 系统消息块。
    ///
    /// 格式：
    /// ```text
    /// <memory>
    /// 以下是从历史对话中召回的相关上下文：
    ///
    /// [用户] ...
    /// [助手] ...
    /// </memory>
    /// ```
    fn format_recall_block(recalls: &[crate::infra::data::conversations::MemoryRecall]) -> String {
        let mut block = String::from("<memory>\n以下是从历史对话中召回的相关上下文：\n\n");
        for recall in recalls {
            let role_label = match recall.role.as_str() {
                "user" => "用户",
                "assistant" => "助手",
                "system" => "系统",
                _ => &recall.role,
            };
            // 截断过长的召回内容（避免注入过多 token）
            let content: String = recall.content.chars().take(500).collect();
            block.push_str(&format!("[{role_label}] {content}\n\n"));
        }
        block.push_str("</memory>");
        block
    }
}

/// 丢弃消息列表开头所有孤立的 ToolResult 消息（0.12.8）。
///
/// 滑动窗口截断可能导致窗口第一条是 `ToolResult`（rig 存为 `Message::User` +
/// `UserContent::ToolResult`），但其对应的 `ToolCall`（`Message::Assistant` +
/// `AssistantContent::ToolCall`）在窗口外。OpenAI 等 API 要求 ToolResult 必须有
/// 对应的 ToolCall，否则报错。
///
/// 此函数从开头扫描，跳过所有 **仅含** ToolResult content 的 User 消息，直到遇到
/// 第一条非 ToolResult 消息（Assistant text/ToolCall 或纯 User text）。
fn drop_leading_orphan_tool_results(messages: &mut Vec<Message>) {
    let mut drop_count = 0;
    for msg in messages.iter() {
        if let Message::User { content } = msg {
            // 检查是否 **全部** 是 ToolResult（纯 ToolResult 消息）
            let all_tool_results = content
                .iter()
                .all(|c| matches!(c, UserContent::ToolResult(_)));
            if all_tool_results && !content.is_empty() {
                drop_count += 1;
                continue;
            }
        }
        break;
    }
    if drop_count > 0 {
        tracing::debug!(
            dropped = drop_count,
            remaining = messages.len() - drop_count,
            "滑动窗口截断：丢弃开头孤立的 ToolResult 消息"
        );
        messages.drain(0..drop_count);
    }
}

impl ConversationMemory for SqliteConversationMemory {
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        Box::pin(async move {
            let result = self.load_inner(conversation_id).await?;
            Ok(result.messages)
        })
    }

    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        let pool = self.pool.clone();
        let live_turns = &self.live_turns;

        Box::pin(async move {
            // 合并实况回合（0.21.16）：删除流式期间写出的部分 assistant 行，
            // 由本轮完整消息替换——正常完成后不残留部分回复 / 重复行。
            let mut live = live_turns.write().await;
            if let Some(id) = live.remove(conversation_id)
                && let Err(e) =
                    crate::infra::data::conversations::delete_message_by_id(&pool, id).await
            {
                tracing::warn!(
                    conversation_id,
                    id,
                    error = %e,
                    "append: 清理实况部分回复行失败（不影响落库）"
                );
            }

            // 跳过已被「发出即保存」预写过的 user 消息（尾部同文 user 视为已写）
            let mut to_insert: &[Message] = &messages;
            if let Some(first) = messages.first()
                && matches!(first, Message::User { .. })
            {
                let last =
                    crate::infra::data::conversations::load_last_message(&pool, conversation_id)
                        .await
                        .map_err(|e| MemoryError::Backend(Box::from(e)))?;
                if let Some((role, content)) = last
                    && role == "user"
                    && Self::user_text_matches(&content, &extract_message_text(first))
                {
                    to_insert = &messages[1..];
                }
            }

            // 自动创建 conversation 记录（已存在则 IGNORE）
            let title = Self::extract_title(&messages);
            crate::infra::data::conversations::create_conversation(
                &pool,
                conversation_id,
                if title.is_empty() { None } else { Some(&title) },
            )
            .await
            .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            // 逐条插入消息（跳过已预写的 user）
            for msg in to_insert {
                let role = Self::message_role(msg);
                let content =
                    serde_json::to_string(msg).map_err(|e| MemoryError::Backend(Box::from(e)))?;
                crate::infra::data::conversations::append_message(
                    &pool,
                    conversation_id,
                    role,
                    &content,
                )
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            }

            // 本轮完成：清掉 pending 标记——此后该 user 是普通历史消息，load 不再丢弃
            self.pending_users.write().await.remove(conversation_id);

            // 更新 last_active_at
            crate::infra::data::conversations::touch_conversation(&pool, conversation_id)
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            crate::infra::data::conversations::clear_messages(&pool, conversation_id)
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            // 0.21.18: 对称清理 live_turns——clear 后不应残留实况回合标记
            self.live_turns.write().await.remove(conversation_id);
            self.pending_users.write().await.remove(conversation_id);
            Ok(())
        })
    }
}

// ── EphemeralConversationMemory（0.17.6）─────────────────────────────────────

/// 进程内临时对话记忆（不持久化）。
///
/// 主窗口 AI 模式使用此 memory——对话不写入 SQLite，进程重启即丢。
/// 供 `ChatService` 在 `ConversationKind::Ephemeral` 时注入 rig agent。
///
/// 设计依据：0.12.3 前的 `InMemoryConversationMemory`（已废弃）同思路，复用。
/// 临时对话不做 token 感知压缩——通常短轮次，全量留内存。
/// 0.21.18: 加 50 条上限防长会话内存与请求无界增长，超出丢最旧，
/// 裁剪后复用孤立 ToolResult 丢弃逻辑（与 Persistent 窗口语义一致）。
///
/// `export_messages` + `remove` 供 Chord-Q 提升流程使用：
/// 导出消息 → 写入 `SqliteConversationMemory` → 清空临时记忆。
/// 导出的是裁剪后窗口（最旧已丢），可接受——promote 保留近期上下文即可。
pub struct EphemeralConversationMemory {
    conversations: tokio::sync::RwLock<HashMap<String, Vec<Message>>>,
}

impl EphemeralConversationMemory {
    /// 构造空临时记忆。
    pub fn new() -> Self {
        Self {
            conversations: tokio::sync::RwLock::new(HashMap::new()),
        }
    }

    /// 导出指定对话的全部消息（供 promote 为持久对话用）。
    ///
    /// 返回消息 Vec 的克隆。对话不存在时返回空 Vec。
    pub async fn export_messages(&self, conversation_id: &str) -> Vec<Message> {
        self.conversations
            .read()
            .await
            .get(conversation_id)
            .cloned()
            .unwrap_or_default()
    }

    /// 删除指定对话的全部消息（promote 后清理临时记忆）。
    pub async fn remove(&self, conversation_id: &str) {
        self.conversations.write().await.remove(conversation_id);
    }
}

impl Default for EphemeralConversationMemory {
    fn default() -> Self {
        Self::new()
    }
}

impl ConversationMemory for EphemeralConversationMemory {
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        Box::pin(async move {
            Ok(self
                .conversations
                .read()
                .await
                .get(conversation_id)
                .cloned()
                .unwrap_or_default())
        })
    }

    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async move {
            let mut convs = self.conversations.write().await;
            let vec = convs.entry(conversation_id.to_string()).or_default();
            vec.extend(messages);
            // 0.21.18: 条数上限裁剪——超出丢最旧，复用孤立 ToolResult 丢弃逻辑
            if vec.len() > MAX_EPHEMERAL_MESSAGES {
                let overflow = vec.len() - MAX_EPHEMERAL_MESSAGES;
                vec.drain(0..overflow);
                drop_leading_orphan_tool_results(vec);
            }
            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        Box::pin(async move {
            self.conversations.write().await.remove(conversation_id);
            Ok(())
        })
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::Text;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        crate::infra::data::conversations::init_db(&pool)
            .await
            .expect("failed to init tables");
        pool
    }

    fn user_msg(text: &str) -> Message {
        Message::User {
            content: vec![UserContent::Text(Text::new(text))],
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: vec![rig_core::completion::message::AssistantContent::Text(
                Text::new(text),
            )],
        }
    }

    // ── 回归：发出即保存预写 user 不污染请求上下文，落库仍完整（0.21.16 bug）───────

    /// 预写 → load 丢弃预写 user（rig 会追加一次 prompt）→ append 清除标记 →
    /// 再次 load 恢复完整历史（预写 user 已是一轮普通历史）。
    #[tokio::test]
    async fn prewrite_user_skipped_in_load_then_restored_after_append() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 1. 发出即保存：预写当前 user → pending 标记 + DB 写入
        mem.persist_user_message("c1", "hello").await.unwrap();
        // 2. load：应丢弃预写 user（rig 会把 prompt 追加一次）
        let loaded = mem.load("c1").await.unwrap();
        assert!(
            loaded.is_empty(),
            "load 不应带出预写 user（避免与 rig 追加的 prompt 重复）: {loaded:?}"
        );

        // 3. 完成：rig append [user, assistant] → 跳过预写 user，补写 assistant，清标记
        mem.append("c1", vec![user_msg("hello"), assistant_msg("hi")])
            .await
            .unwrap();
        // 4. load：标记已清，预写 user 作为普通历史出现
        let loaded = mem.load("c1").await.unwrap();
        let texts: Vec<String> = loaded.iter().map(extract_message_text).collect();
        assert_eq!(
            texts,
            vec!["hello".to_string(), "hi".to_string()],
            "append 后 DB 应完整（user + assistant）: {texts:?}"
        );

        // 5. 第二轮：预写 world → load 只丢 world，hello/hi 仍在历史
        mem.persist_user_message("c1", "world").await.unwrap();
        let loaded = mem.load("c1").await.unwrap();
        let texts: Vec<String> = loaded.iter().map(extract_message_text).collect();
        assert_eq!(
            texts,
            vec!["hello".to_string(), "hi".to_string()],
            "第二轮 load 应丢 world、保留首轮历史: {texts:?}"
        );

        // 6. 第二轮完成 → 历史四段完整
        mem.append("c1", vec![user_msg("world"), assistant_msg("world reply")])
            .await
            .unwrap();
        let loaded = mem.load("c1").await.unwrap();
        let texts: Vec<String> = loaded.iter().map(extract_message_text).collect();
        assert_eq!(
            texts,
            vec![
                "hello".to_string(),
                "hi".to_string(),
                "world".to_string(),
                "world reply".to_string()
            ],
            "第二轮完成后 DB 应四段完整: {texts:?}"
        );
    }

    // ── 原有测试（0.12.3）──────────────────────────────────────────────────────

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 空对话 load 返回空
        assert!(mem.load("c1").await.unwrap().is_empty());

        // append 两条
        mem.append("c1", vec![user_msg("hello"), assistant_msg("hi")])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn isolation_between_conversations() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("a", vec![user_msg("hi a")]).await.unwrap();
        mem.append("b", vec![user_msg("hi b")]).await.unwrap();

        assert_eq!(mem.load("a").await.unwrap().len(), 1);
        assert_eq!(mem.load("b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn clear_removes_history() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("c", vec![user_msg("x")]).await.unwrap();
        mem.clear("c").await.unwrap();
        assert!(mem.load("c").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sliding_window_truncates_old_messages() {
        let pool = setup_pool().await;
        // FixedCount 模式测试
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::FixedCount,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool, config);

        // 写入 30 条消息
        for i in 0..30 {
            mem.append("c1", vec![user_msg(&format!("msg {i}"))])
                .await
                .unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        // 滑动窗口只返回最近 20 条
        assert_eq!(loaded.len(), 20);

        // 最近 20 条是 msg 10 ~ msg 29
        let first = match &loaded[0] {
            Message::User { content } => match content.first() {
                Some(UserContent::Text(t)) => t.text.clone(),
                _ => String::new(),
            },
            _ => String::new(),
        };
        assert!(
            first.contains("msg 10"),
            "first should be msg 10, got: {first}"
        );
    }

    #[tokio::test]
    async fn auto_creates_conversation_with_title() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("c1", vec![user_msg("Hello world this is a test")])
            .await
            .unwrap();

        let convs = crate::infra::data::conversations::list_conversations(&mem.pool)
            .await
            .unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(
            convs[0].title.as_deref(),
            Some("Hello world this is a test")
        );
    }

    #[tokio::test]
    async fn title_truncated_to_max_chars() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        let long_text = "x".repeat(100);
        mem.append("c1", vec![user_msg(&long_text)]).await.unwrap();

        let convs = crate::infra::data::conversations::list_conversations(&mem.pool)
            .await
            .unwrap();
        assert_eq!(
            convs[0].title.as_deref().unwrap().chars().count(),
            TITLE_MAX_CHARS
        );
    }

    #[tokio::test]
    async fn message_preserves_tool_call_and_result() {
        use rig_core::completion::message::{ToolCall, ToolFunction};

        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 写入带 tool_call 的 assistant 消息
        let assistant_with_tool = Message::Assistant {
            id: None,
            content: vec![rig_core::completion::message::AssistantContent::ToolCall(
                ToolCall::from_wire(
                    "call_1",
                    ToolFunction::new("search".to_string(), serde_json::json!({"q": "test"})),
                ),
            )],
        };

        mem.append("c1", vec![user_msg("search for test"), assistant_with_tool])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);

        // 第二条是 assistant 消息，应包含 ToolCall
        match &loaded[1] {
            Message::Assistant { content, .. } => match content.first() {
                Some(rig_core::completion::message::AssistantContent::ToolCall(tc)) => {
                    assert_eq!(tc.function.name, "search");
                }
                _ => panic!("expected ToolCall in assistant message"),
            },
            _ => panic!("expected Assistant message"),
        }
    }

    #[tokio::test]
    async fn arc_conversation_memory_forwards_to_inner() {
        let pool = setup_pool().await;
        let inner = std::sync::Arc::new(SqliteConversationMemory::new(pool));
        let mem: std::sync::Arc<dyn ConversationMemory> = inner.clone();

        mem.append("c", vec![user_msg("hello")]).await.unwrap();
        assert_eq!(mem.load("c").await.unwrap().len(), 1);
        mem.clear("c").await.unwrap();
        assert!(mem.load("c").await.unwrap().is_empty());
    }

    #[test]
    fn test_drop_leading_orphan_tool_results() {
        use rig_core::completion::message::{
            ToolCall, ToolFunction, ToolResult, ToolResultContent,
        };

        // 构造 ToolResult 消息（rig 存为 User + UserContent::ToolResult）
        fn tool_result_msg(id: &str) -> Message {
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    call: rig_core::message::ToolCallId::new_or_mint(id),
                    provider: None,
                    name: id.to_string(),
                    content: vec![ToolResultContent::text("ok")],
                })],
            }
        }

        // 场景1：开头是孤立 ToolResult → 应被丢弃
        {
            let mut msgs = vec![
                tool_result_msg("r1"),
                assistant_msg("done"),
                user_msg("next"),
            ];
            drop_leading_orphan_tool_results(&mut msgs);
            assert_eq!(msgs.len(), 2, "应丢弃开头的孤立 ToolResult");
            assert!(matches!(msgs[0], Message::Assistant { .. }));
        }

        // 场景2：开头多条孤立 ToolResult → 全部丢弃
        {
            let mut msgs = vec![
                tool_result_msg("r1"),
                tool_result_msg("r2"),
                assistant_msg("done"),
            ];
            drop_leading_orphan_tool_results(&mut msgs);
            assert_eq!(msgs.len(), 1, "应丢弃两条孤立 ToolResult");
        }

        // 场景3：开头是正常 User text → 不丢弃
        {
            let mut msgs = vec![user_msg("hello"), assistant_msg("hi")];
            drop_leading_orphan_tool_results(&mut msgs);
            assert_eq!(msgs.len(), 2, "不应丢弃非 ToolResult 消息");
        }

        // 场景4：空列表 → 不 panic
        {
            let mut msgs: Vec<Message> = vec![];
            drop_leading_orphan_tool_results(&mut msgs);
            assert!(msgs.is_empty());
        }

        // 场景5：开头是 Assistant ToolCall → 不丢弃（不是 ToolResult）
        {
            let tool_call = Message::Assistant {
                id: None,
                content: vec![rig_core::completion::message::AssistantContent::ToolCall(
                    ToolCall::from_wire(
                        "call_1",
                        ToolFunction::new("search".to_string(), serde_json::json!({})),
                    ),
                )],
            };
            let mut msgs = vec![tool_call.clone(), tool_result_msg("r1")];
            drop_leading_orphan_tool_results(&mut msgs);
            assert_eq!(msgs.len(), 2, "ToolCall 在前不应被丢弃");
        }
    }

    // ── 0.13.1 新增测试 ─────────────────────────────────────────────────────────

    #[test]
    fn estimate_tokens_english() {
        // "hello world" = 11 chars, 11/4 ≈ 2 tokens
        let tokens = estimate_tokens("hello world");
        assert!(
            (2..=3).contains(&tokens),
            "English estimate should be ~2-3, got {tokens}"
        );
    }

    #[test]
    fn estimate_tokens_chinese() {
        // "你好世界测试" = 6 CJK chars, 6/1.5 = 4 tokens
        let tokens = estimate_tokens("你好世界测试");
        assert!(
            (3..=6).contains(&tokens),
            "Chinese estimate should be ~3-6, got {tokens}"
        );
    }

    #[test]
    fn estimate_tokens_mixed() {
        // 30% CJK threshold: 3 CJK + 7 English = 10 chars, CJK ratio = 0.3
        // → English mode: 10/4 = 2 tokens
        let mixed = "你好世abcdefg";
        let tokens = estimate_tokens(mixed);
        assert!(tokens >= 2, "Mixed estimate should be >= 2, got {tokens}");
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    #[test]
    fn estimate_messages_tokens_sums_all() {
        let msgs = vec![
            user_msg("hello"),   // ~1 token
            assistant_msg("hi"), // ~1 token
        ];
        let total = estimate_messages_tokens(&msgs);
        assert!(total >= 2, "Total should be >= 2, got {total}");
    }

    #[test]
    fn token_aware_truncate_no_action_when_under_limit() {
        let mut msgs = vec![user_msg("short"), assistant_msg("reply")];
        let dropped = token_aware_truncate(&mut msgs, 8192, 0.8, 0.7);
        assert!(dropped.is_empty(), "Should not truncate when under limit");
        assert_eq!(msgs.len(), 2);
    }

    #[test]
    fn token_aware_truncate_removes_from_old_end() {
        // 构造超限消息：每条约 100 chars → ~25 tokens
        // 10 条 → ~250 tokens，limit=100, trigger=80, compress=70
        let mut msgs: Vec<Message> = (0..10)
            .map(|i| {
                user_msg(&format!(
                    "message number {i:03} with some padding text to make it longer"
                ))
            })
            .collect();
        let dropped = token_aware_truncate(&mut msgs, 100, 0.8, 0.7);
        assert!(!dropped.is_empty(), "Should have dropped some messages");
        assert!(
            msgs.len() < 10,
            "Should have fewer messages after truncation"
        );

        // 剩余消息应该是最新的（编号较大的）
        if let Message::User { content } = &msgs[0]
            && let Some(UserContent::Text(t)) = content.iter().next()
        {
            // 第一条剩余消息的编号应该 > 0（旧消息被移出）
            assert!(
                t.text.contains("message number"),
                "First message should be a user message"
            );
        }
    }

    #[test]
    fn token_aware_truncate_compresses_to_target() {
        // 构造消息使总量超过 trigger 但压缩后应在 compress 以下
        let mut msgs: Vec<Message> = (0..20)
            .map(|_i| user_msg(&"x".repeat(40))) // 每条 40 chars → ~10 tokens
            .collect();
        // 总量 ~200 tokens, limit=100, trigger=80, compress=70
        let dropped = token_aware_truncate(&mut msgs, 100, 0.8, 0.7);
        assert!(!dropped.is_empty());
        let remaining_tokens = estimate_messages_tokens(&msgs);
        // 剩余 token 应 ≤ 70 (compress_target)
        assert!(
            remaining_tokens <= 70,
            "Remaining tokens {remaining_tokens} should be <= 70"
        );
    }

    #[tokio::test]
    async fn token_aware_mode_load_truncates_long_history() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        // 设一个很小的 context_limit 来触发压缩
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool, config);

        // 写入大量长消息
        for i in 0..30 {
            let long_text = format!(
                "message {i:03} with substantial content to increase token count significantly"
            );
            mem.append("c1", vec![user_msg(&long_text)]).await.unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        // 应该被压缩——返回的消息数应远少于 30
        assert!(
            loaded.len() < 30,
            "Token-aware mode should truncate, got {} messages",
            loaded.len()
        );
    }

    #[tokio::test]
    async fn fixed_count_mode_backward_compatible() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::FixedCount,
            window_size: 5,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool, config);

        for i in 0..10 {
            mem.append("c1", vec![user_msg(&format!("msg {i}"))])
                .await
                .unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        // FixedCount 模式应严格返回 window_size 条
        assert_eq!(
            loaded.len(),
            5,
            "FixedCount mode should return exactly window_size messages"
        );
    }

    #[tokio::test]
    async fn db_retains_full_history_after_token_truncation() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool, config);

        for i in 0..20 {
            let long_text = format!(
                "message {i:03} with enough content to exceed the very small token limit we set"
            );
            mem.append("c1", vec![user_msg(&long_text)]).await.unwrap();
        }

        // load 应返回裁剪后的窗口
        let loaded = mem.load("c1").await.unwrap();
        assert!(loaded.len() < 20, "Should be truncated");

        // DB 应保留完整历史
        let all_rows =
            crate::infra::data::conversations::load_recent_messages(&mem.pool, "c1", 1000)
                .await
                .unwrap();
        assert_eq!(all_rows.len(), 20, "DB should retain all 20 messages");
    }

    #[tokio::test]
    async fn update_context_limit_changes_truncation_behavior() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool, config);

        // 写入 10 条中等长度消息
        for i in 0..10 {
            mem.append(
                "c1",
                vec![user_msg(&format!("message {i:03} with moderate content"))],
            )
            .await
            .unwrap();
        }

        // 大 limit → 不裁剪
        mem.update_context_limit(Some(100_000)).await;
        let loaded_big = mem.load("c1").await.unwrap();
        assert_eq!(loaded_big.len(), 10, "Large limit should not truncate");

        // 小 limit → 裁剪
        mem.update_context_limit(Some(50)).await;
        let loaded_small = mem.load("c1").await.unwrap();
        assert!(loaded_small.len() < 10, "Small limit should truncate");
    }

    #[test]
    fn memory_config_default_is_token_aware() {
        let cfg = MemoryConfig::default();
        assert_eq!(cfg.mode, WindowMode::TokenAware);
        assert_eq!(cfg.window_size, SLIDING_WINDOW_SIZE);
        assert!((cfg.trigger_ratio - 0.8).abs() < 0.001);
        assert!((cfg.compress_ratio - 0.7).abs() < 0.001);
    }

    #[test]
    fn memory_config_serializes_with_defaults() {
        // 最小 JSON（只有 mode）应能反序列化
        let json = r#"{"mode":"fixed_count"}"#;
        let cfg: MemoryConfig = serde_json::from_str(json).unwrap();
        assert_eq!(cfg.mode, WindowMode::FixedCount);
        assert_eq!(cfg.window_size, SLIDING_WINDOW_SIZE); // default
    }

    #[test]
    fn extract_message_text_handles_all_variants() {
        // System
        let sys = Message::System {
            content: "system prompt".into(),
        };
        assert!(extract_message_text(&sys).contains("system prompt"));

        // User text
        let usr = user_msg("hello user");
        assert!(extract_message_text(&usr).contains("hello user"));

        // Assistant text
        let ast = assistant_msg("hello assistant");
        assert!(extract_message_text(&ast).contains("hello assistant"));

        // Empty
        let empty = user_msg("");
        assert_eq!(extract_message_text(&empty), "");
    }

    // ── 0.13.2 FTS5 召回端到端测试 ──────────────────────────────────────────

    #[tokio::test]
    async fn token_aware_load_archives_dropped_messages_to_fts() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false, // 先关召回，验证归档
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        // 设很小的 context_limit 触发压缩
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入大量消息使窗口裁剪发生
        for i in 0..20 {
            let long_text = format!("message_{i:03} with substantial content about topic_{i}");
            mem.append("c1", vec![user_msg(&long_text)]).await.unwrap();
        }

        // load 应裁剪，被裁剪的消息应归档到 FTS5
        let loaded = mem.load("c1").await.unwrap();
        assert!(loaded.len() < 20, "应被裁剪");

        // 验证 FTS5 中有归档记录——搜索一个早期消息的关键词
        let recalls = crate::infra::data::conversations::search_memory_fts(
            &pool,
            "c1",
            "topic_0",
            10,
            crate::domain::ai::memory::RecallScope::ThisConversation,
        )
        .await
        .unwrap();
        assert!(!recalls.is_empty(), "被裁剪的消息应已归档到 FTS5");
    }

    #[tokio::test]
    async fn recall_injects_memory_block_when_enabled() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入一些关于 Rust 的消息，让它们被裁剪并归档
        for i in 0..15 {
            let text = format!("讨论 Rust async runtime topic_{i:03} with details");
            mem.append("c1", vec![user_msg(&text)]).await.unwrap();
        }

        // 第一次 load 触发归档
        let _ = mem.load("c1").await.unwrap();

        // 再追加一条关于 Rust 的消息（OR 语义下，任一关键词命中即召回）
        mem.append("c1", vec![user_msg("Rust async runtime")])
            .await
            .unwrap();

        // 第二次 load 应召回相关的旧消息并注入 <memory> 块
        let loaded = mem.load("c1").await.unwrap();

        // 检查是否有 <memory> 系统消息
        let has_memory = loaded.iter().any(|m| {
            if let Message::System { content } = m {
                content.contains("<memory>")
            } else {
                false
            }
        });
        assert!(has_memory, "开启召回后应注入 <memory> 系统消息块");
    }

    #[tokio::test]
    async fn recall_disabled_does_not_inject_memory_block() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        for i in 0..15 {
            let text = format!("讨论 Rust topic_{i:03} with details");
            mem.append("c1", vec![user_msg(&text)]).await.unwrap();
        }

        let _ = mem.load("c1").await.unwrap();
        mem.append("c1", vec![user_msg("Rust details")])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        let has_memory = loaded.iter().any(|m| {
            if let Message::System { content } = m {
                content.contains("<memory>")
            } else {
                false
            }
        });
        assert!(!has_memory, "关闭召回后不应注入 <memory> 块");
    }

    // ── 0.13.6 load_with_stats 测试 ──────────────────────────────────────────

    #[tokio::test]
    async fn load_with_stats_returns_compression_stats() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入大量消息触发压缩
        for i in 0..20 {
            let long_text = format!(
                "message {i:03} with enough content to exceed the very small token limit we set"
            );
            mem.append("c1", vec![user_msg(&long_text)]).await.unwrap();
        }

        let result = mem.load_with_stats("c1").await.unwrap();
        assert!(result.dropped_count > 0, "Should have dropped messages");
        assert!(result.messages.len() < 20, "Should be truncated");
    }

    #[tokio::test]
    async fn load_with_stats_returns_recall_stats() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: true,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(50);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入消息触发归档
        for i in 0..15 {
            let text = format!("讨论 Rust topic_{i:03} with details");
            mem.append("c1", vec![user_msg(&text)]).await.unwrap();
        }
        let _ = mem.load("c1").await.unwrap(); // 第一次 load 归档
        mem.append("c1", vec![user_msg("Rust details")])
            .await
            .unwrap();

        let result = mem.load_with_stats("c1").await.unwrap();
        assert!(result.recall_count > 0, "Should have recalled messages");
    }

    #[tokio::test]
    async fn load_with_stats_no_compression_when_under_limit() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        config.write().await.context_limit = Some(100_000);
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        mem.append("c1", vec![user_msg("short message")])
            .await
            .unwrap();

        let result = mem.load_with_stats("c1").await.unwrap();
        assert_eq!(result.dropped_count, 0, "Should not drop any messages");
        assert_eq!(result.messages.len(), 1);
    }

    // ── EphemeralConversationMemory 测试（0.17.6）──────────────────────────────

    #[tokio::test]
    async fn ephemeral_append_and_load_roundtrip() {
        let mem = EphemeralConversationMemory::new();

        // 空对话 load 返回空
        assert!(mem.load("c1").await.unwrap().is_empty());

        // append 两条消息
        mem.append("c1", vec![user_msg("hello"), assistant_msg("hi")])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn ephemeral_isolation_between_conversations() {
        let mem = EphemeralConversationMemory::new();

        mem.append("a", vec![user_msg("hi a")]).await.unwrap();
        mem.append("b", vec![user_msg("hi b")]).await.unwrap();

        assert_eq!(mem.load("a").await.unwrap().len(), 1);
        assert_eq!(mem.load("b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn ephemeral_clear_removes_history() {
        let mem = EphemeralConversationMemory::new();

        mem.append("c", vec![user_msg("x")]).await.unwrap();
        mem.clear("c").await.unwrap();
        assert!(mem.load("c").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ephemeral_export_messages_returns_clone() {
        let mem = EphemeralConversationMemory::new();

        mem.append("c1", vec![user_msg("hello"), assistant_msg("world")])
            .await
            .unwrap();

        // export 返回克隆，不影响内部状态
        let exported = mem.export_messages("c1").await;
        assert_eq!(exported.len(), 2);

        // 内部状态不变
        assert_eq!(mem.load("c1").await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn ephemeral_export_nonexistent_returns_empty() {
        let mem = EphemeralConversationMemory::new();
        assert!(mem.export_messages("nonexistent").await.is_empty());
    }

    #[tokio::test]
    async fn ephemeral_remove_deletes_conversation() {
        let mem = EphemeralConversationMemory::new();

        mem.append("c1", vec![user_msg("hello")]).await.unwrap();
        assert_eq!(mem.load("c1").await.unwrap().len(), 1);

        mem.remove("c1").await;
        assert!(mem.load("c1").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn ephemeral_does_not_persist_to_db() {
        // EphemeralConversationMemory 是纯内存实现，不写入 SQLite。
        // 验证：临时记忆 append 后，同 conversation_id 在 SqliteConversationMemory 中仍为空。
        let pool = setup_pool().await;
        let sqlite_mem = SqliteConversationMemory::new(pool);
        let ephemeral_mem = EphemeralConversationMemory::new();

        ephemeral_mem
            .append("ephemeral-1", vec![user_msg("temp message")])
            .await
            .unwrap();

        // SQLite 中同 ID 对话仍为空
        assert!(sqlite_mem.load("ephemeral-1").await.unwrap().is_empty());
    }

    // ── 0.42: 旧版消息格式安全加载测试 ──────────────────────────────────

    /// 验证 `load_inner` 遇到旧版/损坏的 JSON 时跳过该行而不 panic，
    /// 正常消息仍能加载。
    #[tokio::test]
    async fn load_inner_skips_legacy_messages_without_panicking() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool.clone());

        // 手动写入一条正常消息 + 一条旧格式（无法反序列化为 rig Message 的 JSON）
        let good_msg = Message::User {
            content: vec![UserContent::Text(Text::new("hello"))],
        };
        let good_json = serde_json::to_string(&good_msg).unwrap();
        crate::infra::data::conversations::create_conversation(&pool, "c1", Some("test"))
            .await
            .unwrap();
        crate::infra::data::conversations::append_message(&pool, "c1", "user", &good_json)
            .await
            .unwrap();

        // 写入一条损坏的 JSON（旧版格式）
        crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "assistant",
            "{\"old_format\": true, \"unknown_field\": 42}",
        )
        .await
        .unwrap();

        // 再写入一条正常消息
        let good_msg2 = Message::Assistant {
            id: None,
            content: vec![rig_core::completion::message::AssistantContent::Text(
                Text::new("world"),
            )],
        };
        let good_json2 = serde_json::to_string(&good_msg2).unwrap();
        crate::infra::data::conversations::append_message(&pool, "c1", "assistant", &good_json2)
            .await
            .unwrap();

        // load 应跳过损坏行，返回 2 条正常消息（而非报错）
        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(
            loaded.len(),
            2,
            "应跳过损坏行，加载 2 条正常消息，实际: {}",
            loaded.len()
        );
    }

    /// 验证 `load_inner` 全部消息都损坏时返回空 Vec 而非报错。
    #[tokio::test]
    async fn load_inner_all_corrupt_returns_empty_vec() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool.clone());

        crate::infra::data::conversations::create_conversation(&pool, "c2", Some("corrupt"))
            .await
            .unwrap();
        crate::infra::data::conversations::append_message(&pool, "c2", "user", "NOT_VALID_JSON{{{")
            .await
            .unwrap();

        let loaded = mem.load("c2").await.unwrap();
        assert!(
            loaded.is_empty(),
            "全部损坏时应返回空 Vec，实际: {} 条",
            loaded.len()
        );
    }

    // ── 0.21.18: history_budget 裁剪预算测试 ──────────────────────────────

    /// history_budget 优先于 context_limit 作为裁剪基准。
    ///
    /// context_limit 设大（100_000）但 history_budget 设小（50）→ 触发裁剪；
    /// 反之 history_budget 设大 → 不裁剪。证明新基准生效。
    #[tokio::test]
    async fn history_budget_overrides_context_limit_for_truncation() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false, // 关召回，隔离裁剪行为
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入 10 条中等长度消息
        for i in 0..10 {
            mem.append(
                "c1",
                vec![user_msg(&format!("message {i:03} with moderate content"))],
            )
            .await
            .unwrap();
        }

        // 1. context_limit 大 + history_budget 小 → 应裁剪
        mem.update_context_limit(Some(100_000)).await;
        mem.update_history_budget(Some(50)).await;
        let loaded_small_budget = mem.load("c1").await.unwrap();
        assert!(
            loaded_small_budget.len() < 10,
            "history_budget=50 应触发裁剪，实际: {} 条",
            loaded_small_budget.len()
        );

        // 2. history_budget 大 → 不裁剪
        mem.update_history_budget(Some(100_000)).await;
        let loaded_big_budget = mem.load("c1").await.unwrap();
        assert_eq!(
            loaded_big_budget.len(),
            10,
            "history_budget=100_000 不应裁剪，实际: {} 条",
            loaded_big_budget.len()
        );
    }

    /// update_context_limit 应将 history_budget 置 None（模型切换后旧预算失效）。
    #[tokio::test]
    async fn update_context_limit_invalidates_history_budget() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入 10 条中等长度消息
        for i in 0..10 {
            mem.append(
                "c1",
                vec![user_msg(&format!("message {i:03} with moderate content"))],
            )
            .await
            .unwrap();
        }

        // 注入小 history_budget → 裁剪
        mem.update_context_limit(Some(100_000)).await;
        mem.update_history_budget(Some(50)).await;
        let loaded_with_budget = mem.load("c1").await.unwrap();
        assert!(loaded_with_budget.len() < 10, "history_budget=50 应裁剪");

        // 模型切换 → update_context_limit 应使 history_budget 失效
        // 新 context_limit 仍是 100_000（大窗口），裁剪应回到 context_limit 基准 → 不裁剪
        mem.update_context_limit(Some(100_000)).await;
        let loaded_after_switch = mem.load("c1").await.unwrap();
        assert_eq!(
            loaded_after_switch.len(),
            10,
            "模型切换后 history_budget 失效，回退到 context_limit=100_000 不应裁剪"
        );
    }

    /// apply_config 应保留运行时注入的 history_budget。
    #[tokio::test]
    async fn apply_config_preserves_history_budget() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入消息
        for i in 0..10 {
            mem.append(
                "c1",
                vec![user_msg(&format!("message {i:03} with moderate content"))],
            )
            .await
            .unwrap();
        }

        // 注入 context_limit + history_budget
        mem.update_context_limit(Some(100_000)).await;
        mem.update_history_budget(Some(50)).await;

        // 模拟设置页改配置
        let new_config = MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.75, // 改了值
            compress_ratio: 0.65,
            history_budget: None, // serde 反序列化后应为 None
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false,
            recall_scope: RecallScope::ThisConversation,
        };
        mem.apply_config(new_config).await;

        // 验证 history_budget 被保留
        let config_handle = mem.config_handle();
        let cfg = config_handle.read().await;
        assert_eq!(
            cfg.history_budget,
            Some(50),
            "apply_config 后 history_budget 应保留为 Some(50)"
        );
        assert_eq!(cfg.trigger_ratio, 0.75, "trigger_ratio 应已更新为新值");
        drop(cfg);

        // 行为验证：history_budget=50 仍生效 → 裁剪
        let loaded = mem.load("c1").await.unwrap();
        assert!(
            loaded.len() < 10,
            "apply_config 后 history_budget 仍应驱动裁剪"
        );
    }

    // ── 0.21.18: 中断残留 live_turns 不被下一轮覆盖 ──────────────────────

    /// 验证流式中断后残留的 live_turn 标记不会被下一轮 persist_assistant_delta
    /// 复用——修复前第二条 delta 会 UPDATE 旧行，partial1 被覆盖只剩 3 条。
    #[tokio::test]
    async fn aborted_live_turn_not_overwritten_by_next_turn() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 第一轮：预写 user → 流式部分回复 → 中断（不调 append）
        mem.persist_user_message("c", "q1").await.unwrap();
        mem.persist_assistant_delta("c", "partial1", "")
            .await
            .unwrap();
        // 模拟中断：不调 append，live_turns 残留 "c" -> 旧行 id

        // 第二轮：persist_user_message 应清掉残留的 live_turns 标记
        mem.persist_user_message("c", "q2").await.unwrap();
        // 第二轮的流式部分回复——不应 UPDATE 第一轮的断点行
        mem.persist_assistant_delta("c", "partial2", "")
            .await
            .unwrap();

        // load 应返回 4 条：q1, partial1, q2, partial2
        let loaded = mem.load("c").await.unwrap();
        let texts: Vec<String> = loaded.iter().map(extract_message_text).collect();
        assert_eq!(
            texts,
            vec![
                "q1".to_string(),
                "partial1".to_string(),
                "q2".to_string(),
                "partial2".to_string(),
            ],
            "中断残留的 partial1 不应被下一轮覆盖，应有 4 条: {texts:?}"
        );
    }

    // ── 0.21.18: Ephemeral 条数上限测试 ──────────────────────────────────

    /// 连续 append 80 条单消息 → load ≤ 50，且首条是第 31 条（保留最新）。
    #[tokio::test]
    async fn ephemeral_append_caps_at_max() {
        let mem = EphemeralConversationMemory::new();

        // 写入 80 条 user 消息（编号 0..80）
        for i in 0..80 {
            mem.append("c1", vec![user_msg(&format!("msg {i}"))])
                .await
                .unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        assert!(
            loaded.len() <= MAX_EPHEMERAL_MESSAGES,
            "load 不应超过上限 {MAX_EPHEMERAL_MESSAGES}，实际: {}",
            loaded.len()
        );

        // 首条应是 msg 30（0..80 共 80 条，丢前 30 条，保留 30..80）
        let first_text = extract_message_text(&loaded[0]);
        assert!(
            first_text.contains("msg 30"),
            "首条应是最旧保留 msg 30，实际: {first_text}"
        );
        // 末条应是 msg 79
        let last_text = extract_message_text(loaded.last().unwrap());
        assert!(
            last_text.contains("msg 79"),
            "末条应是最新 msg 79，实际: {last_text}"
        );
    }

    /// 裁剪后开头的孤立 ToolResult 应被丢弃。
    #[tokio::test]
    async fn ephemeral_cap_drops_orphan_tool_results() {
        use rig_core::completion::message::{ToolResult, ToolResultContent};

        let mem = EphemeralConversationMemory::new();

        // 构造开头为纯 ToolResult 的消息（孤立，无对应 ToolCall）
        fn tool_result_msg(id: &str) -> Message {
            Message::User {
                content: vec![UserContent::ToolResult(ToolResult {
                    call: rig_core::message::ToolCallId::new_or_mint(id),
                    provider: None,
                    name: id.to_string(),
                    content: vec![ToolResultContent::text("ok")],
                })],
            }
        }

        // 写入 51 条 ToolResult + 1 条普通 user → 共 52 条，超出 50 上限
        // 裁剪后丢前 2 条，剩 49 条 ToolResult + 1 条 user
        // 但 drop_leading_orphan_tool_results 会继续丢弃开头的孤立 ToolResult
        for i in 0..51 {
            mem.append("c1", vec![tool_result_msg(&format!("r{i}"))])
                .await
                .unwrap();
        }
        mem.append("c1", vec![user_msg("hello")]).await.unwrap();

        let loaded = mem.load("c1").await.unwrap();
        // 裁剪 + 丢弃孤立 ToolResult 后，首条不应是纯 ToolResult
        let first = &loaded[0];
        let is_pure_tool_result = match first {
            Message::User { content } => {
                !content.is_empty()
                    && content
                        .iter()
                        .all(|c| matches!(c, UserContent::ToolResult(_)))
            }
            _ => false,
        };
        assert!(!is_pure_tool_result, "裁剪后开头的孤立 ToolResult 应被丢弃");
        // 应包含末尾的 user "hello"
        let texts: Vec<String> = loaded.iter().map(extract_message_text).collect();
        assert!(
            texts.contains(&"hello".to_string()),
            "应保留末尾 user 消息: {texts:?}"
        );
    }

    // ── 0.21.19: 摘要注入与回退测试 ──────────────────────────────────

    /// 摘要启用时，load 应跳过水位以下消息并注入 <summary> 块。
    #[tokio::test]
    async fn summary_enabled_injects_block_and_skips_summarized() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false, // 关召回，隔离摘要行为
            recall_top_k: 3,
            summary_enabled: true,
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 创建对话记录
        crate::infra::data::conversations::create_conversation(&pool, "c1", Some("test"))
            .await
            .unwrap();

        // 写入 4 条消息
        let msg_json = |text: &str| serde_json::to_string(&user_msg(text)).unwrap();
        let id1 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("旧消息1"),
        )
        .await
        .unwrap();
        let id2 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("旧消息2"),
        )
        .await
        .unwrap();
        let _id3 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("新消息1"),
        )
        .await
        .unwrap();
        let _id4 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("新消息2"),
        )
        .await
        .unwrap();

        // 插入摘要覆盖 id1..=id2，推进水位到 id2
        crate::infra::data::conversations::insert_summary_and_advance_watermark(
            &pool,
            "c1",
            0,
            id1,
            id2,
            "用户讨论了旧消息的摘要",
            10,
        )
        .await
        .unwrap();

        let result = mem.load_with_stats("c1").await.unwrap();

        // 应跳过 2 条已摘要消息
        assert_eq!(result.summarized_count, 2, "应跳过 2 条已摘要消息");

        // 应注入 <summary> 块
        let has_summary = result
            .messages
            .iter()
            .any(|m| matches!(m, Message::System { content } if content.contains("<summary>")));
        assert!(has_summary, "应注入 <summary> 系统消息块");

        // summary_tokens > 0
        assert!(result.summary_tokens > 0, "摘要块 token 应 > 0");

        // 消息应包含新消息，不含旧消息
        let texts: Vec<String> = result.messages.iter().map(extract_message_text).collect();
        assert!(texts.iter().any(|t| t.contains("新消息1")), "应包含新消息1");
        assert!(texts.iter().any(|t| t.contains("新消息2")), "应包含新消息2");
        assert!(
            !texts.iter().any(|t| t.contains("旧消息1")),
            "不应包含已摘要的旧消息1"
        );
        assert!(
            !texts.iter().any(|t| t.contains("旧消息2")),
            "不应包含已摘要的旧消息2"
        );
    }

    /// 摘要关闭时（summary_enabled=false），完全回退纯截断行为。
    #[tokio::test]
    async fn summary_disabled_falls_back_to_truncation() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: false, // 关闭摘要
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 创建对话记录
        crate::infra::data::conversations::create_conversation(&pool, "c1", Some("test"))
            .await
            .unwrap();

        // 写入消息
        let msg_json = |text: &str| serde_json::to_string(&user_msg(text)).unwrap();
        let id1 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("旧消息"),
        )
        .await
        .unwrap();
        let _id2 = crate::infra::data::conversations::append_message(
            &pool,
            "c1",
            "user",
            &msg_json("新消息"),
        )
        .await
        .unwrap();

        // 即便有摘要数据，关闭时也不应注入
        crate::infra::data::conversations::insert_summary_and_advance_watermark(
            &pool,
            "c1",
            0,
            id1,
            id1,
            "旧消息摘要",
            5,
        )
        .await
        .unwrap();

        let result = mem.load_with_stats("c1").await.unwrap();

        // 不应有摘要注入
        assert_eq!(
            result.summarized_count, 0,
            "关闭摘要时 summarized_count 应为 0"
        );
        assert_eq!(result.summary_tokens, 0, "关闭摘要时 summary_tokens 应为 0");

        // 不应有 <summary> 块
        let has_summary = result
            .messages
            .iter()
            .any(|m| matches!(m, Message::System { content } if content.contains("<summary>")));
        assert!(!has_summary, "关闭摘要时不应注入 <summary> 块");

        // 应返回全部消息（未被水位跳过）
        assert_eq!(result.messages.len(), 2, "应返回全部消息");
    }

    /// FixedCount 模式不受摘要影响——摘要只在 TokenAware 模式下生效。
    #[tokio::test]
    async fn fixed_count_mode_unaffected_by_summary() {
        let pool = setup_pool().await;
        let config = Arc::new(RwLock::new(MemoryConfig {
            mode: WindowMode::FixedCount,
            window_size: 10,
            context_limit: None,
            trigger_ratio: 0.8,
            compress_ratio: 0.7,
            history_budget: None,
            recall_enabled: false,
            recall_top_k: 3,
            summary_enabled: true, // 即使开启，FixedCount 模式也应正常工作
            recall_scope: RecallScope::ThisConversation,
        }));
        let mem = SqliteConversationMemory::with_config(pool.clone(), config);

        // 写入 5 条消息
        for i in 0..5 {
            mem.append("c1", vec![user_msg(&format!("msg {i}"))])
                .await
                .unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 5, "FixedCount 模式应返回全部 5 条消息");
    }

    // ── 0.21.19.1 F1: compute_truncate_boundary 纯函数单测 ────────────────────

    /// P0 回归核心用例：小预算 + 长消息、对话 < 200 条、超触发阈值 → 边界必须非空。
    /// 在旧实现（条数口径 200）下该用例必失败（不足 200 条 → 边界 None → 摘要不触发）。
    #[test]
    fn compute_truncate_boundary_small_budget_few_messages_nonempty() {
        // 4 条消息，每条 1000 token，总 4000
        let tokens = vec![1000usize, 1000, 1000, 1000];
        // budget=2000, trigger=0.8 → 阈值 1600；4000 > 1600 触发
        // compress=0.7 → 目标 1400；从旧端移出直到剩余 ≤ 1400
        // 移出 3 条（剩 1000 ≤ 1400）→ 边界 = Some(3)
        let boundary = compute_truncate_boundary(&tokens, 2000, 0.8, 0.7);
        assert_eq!(
            boundary,
            Some(3),
            "4 条 × 1000 token、budget 2000 → 应裁 3 条"
        );
    }

    /// 未达触发阈值 → None（无需裁剪）。
    #[test]
    fn compute_truncate_boundary_below_threshold_none() {
        let tokens = vec![100, 100, 100]; // 总 300
        let boundary = compute_truncate_boundary(&tokens, 1000, 0.8, 0.7); // 阈值 800
        assert_eq!(boundary, None, "总 300 < 阈值 800 → None");
    }

    /// 边界恰好等于水位（触发阈值恰好等于 total）→ 不触发（≤）。
    #[test]
    fn compute_truncate_boundary_at_threshold_none() {
        let tokens = vec![800usize]; // 总 800 = 阈值 800
        let boundary = compute_truncate_boundary(&tokens, 1000, 0.8, 0.7);
        assert_eq!(boundary, None, "总 = 阈值 → 不触发（<=）");
    }

    /// budget 为 0 → 阈值 0、目标 0；总 > 0 触发，移出全部直到剩余 ≤ 0。
    #[test]
    fn compute_truncate_boundary_zero_budget() {
        let tokens = vec![100, 200];
        // budget=0 → 阈值 0、目标 0；总 300 > 0 触发
        // 移出第 1 条（剩 200 > 0）→ 移出第 2 条（剩 0 ≤ 0）→ drop_count=2
        let boundary = compute_truncate_boundary(&tokens, 0, 0.8, 0.7);
        assert_eq!(boundary, Some(2), "budget=0 → 全部移出");
    }

    /// history_budget 缺失走 fallback 链——此测试验证调用方 fallback 逻辑的
    /// 等价性：None → context_limit → DEFAULT_CONTEXT_LIMIT。纯函数本身只接收一个 budget，
    /// fallback 在调用方（maybe_spawn_summary_task 与 load_inner 共用同一链）。
    #[test]
    fn compute_truncate_boundary_fallback_equivalence() {
        let tokens = vec![40000usize]; // 远超 32768 默认
        // DEFAULT_CONTEXT_LIMIT=32768, trigger=0.8 → 26214；40000 > 26214 触发
        // compress=0.7 → 22937；移出 1 条（剩 0 ≤ 22937）→ Some(1)
        let boundary = compute_truncate_boundary(&tokens, DEFAULT_CONTEXT_LIMIT, 0.8, 0.7);
        assert_eq!(boundary, Some(1));
    }

    /// 单条消息超预算 → 裁 1 条（剩 0）。
    #[test]
    fn compute_truncate_boundary_single_message_over_budget() {
        let tokens = vec![5000];
        let boundary = compute_truncate_boundary(&tokens, 1000, 0.8, 0.7);
        assert_eq!(boundary, Some(1));
    }

    /// 空消息列表 → None。
    #[test]
    fn compute_truncate_boundary_empty() {
        let boundary = compute_truncate_boundary(&[], 1000, 0.8, 0.7);
        assert_eq!(boundary, None);
    }
}
