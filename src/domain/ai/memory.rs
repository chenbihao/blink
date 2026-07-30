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

use std::sync::Arc;

use rig_core::completion::Message;
use rig_core::completion::message::{AssistantContent, UserContent};
use rig_core::memory::{ConversationMemory, MemoryError};
use sqlx::SqlitePool;
use tokio::sync::RwLock;

/// 滑动窗口大小（FixedCount 模式，0.12.3 §5.3：最近 20 条消息）。
const SLIDING_WINDOW_SIZE: i64 = 20;

/// TokenAware 模式加载批次大小（加载较多消息后 in-memory 裁剪）。
const TOKEN_AWARE_LOAD_BATCH: i64 = 200;

/// 标题最大字符数（从首条 User 消息提取）。
const TITLE_MAX_CHARS: usize = 50;

/// 保守默认 context limit（ModelEntry.context_window 缺失时使用）。
const DEFAULT_CONTEXT_LIMIT: usize = 8192;

// ── MemoryLoadResult（0.13.6）──────────────────────────────────────────────────

/// `load_with_stats()` 返回值——消息 + 压缩/召回统计（0.13.6）。
///
/// 供 `ChatService::compute_context_status()` 计算上下文窗口占用指示器数据。
#[derive(Debug, Clone)]
pub struct MemoryLoadResult {
    /// 加载的消息列表（已裁剪 + 已召回注入）。
    pub messages: Vec<Message>,
    /// 本次 load 被裁剪移出的消息数（token_aware_truncate）。
    pub dropped_count: usize,
    /// 本次 load FTS5 召回的消息数。
    pub recall_count: usize,
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
    /// None 时使用保守默认（8K）。
    #[serde(skip)] // 运行时注入，不持久化
    pub context_limit: Option<usize>,

    /// 触发压缩的 token 占比（默认 0.8 = 80% of context_limit）。
    #[serde(default = "default_trigger_ratio")]
    pub trigger_ratio: f64,

    /// 压缩目标 token 占比（默认 0.7 = 70% of context_limit）。
    #[serde(default = "default_compress_ratio")]
    pub compress_ratio: f64,

    /// FTS5 召回开关（0.13.2，默认开）。
    /// 开启后，被窗口裁剪的旧消息归档到 FTS5，load() 时 BM25 检索召回相关旧上下文。
    #[serde(default = "default_recall_enabled")]
    pub recall_enabled: bool,

    /// FTS5 召回 Top-K（0.13.2，默认 3）。
    /// 每次召回的最多消息条数。
    #[serde(default = "default_recall_top_k")]
    pub recall_top_k: i64,
}

impl Default for MemoryConfig {
    fn default() -> Self {
        Self {
            mode: WindowMode::TokenAware,
            window_size: SLIDING_WINDOW_SIZE,
            context_limit: None,
            trigger_ratio: default_trigger_ratio(),
            compress_ratio: default_compress_ratio(),
            recall_enabled: default_recall_enabled(),
            recall_top_k: default_recall_top_k(),
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

// ── token 估算（0.13.1 §3.3）───────────────────────────────────────────────────

/// 判断字符是否为 CJK 字符（中文/日文/韩文）。
fn is_cjk(c: char) -> bool {
    matches!(c as u32,
        0x4E00..=0x9FFF   // CJK 统一表意文字
        | 0x3400..=0x4DBF  // CJK 扩展 A
        | 0x20000..=0x2A6DF // CJK 扩展 B
        | 0x3040..=0x309F  // 平假名
        | 0x30A0..=0x30FF  // 片假名
        | 0xAC00..=0xD7AF   // 韩文音节
    )
}

/// 启发式 token 估算（0.13.1 §3.3）。
///
/// CJK 字符占比 > 30% 用中文档（chars / 1.5），否则用英文件（chars / 4）。
/// 误差 ±20% 可接受——压缩阈值留 buffer。
pub fn estimate_tokens(text: &str) -> usize {
    if text.is_empty() {
        return 0;
    }
    let chars: Vec<char> = text.chars().collect();
    let total = chars.len();
    let cjk_count = chars.iter().filter(|c| is_cjk(**c)).count();
    let cjk_ratio = cjk_count as f64 / total as f64;

    let estimate = if cjk_ratio > 0.3 {
        // 中文为主：1 汉字 ≈ 1-2 token，取 1.5
        (total as f64 / 1.5) as usize
    } else {
        // 英文为主：1 token ≈ 4 chars
        total / 4
    };

    estimate.max(1)
}

/// 从 `Message` 提取文本内容（用于 token 估算）。
///
/// 提取 Text / ToolCall / ToolResult 的文本部分，忽略图片等二进制内容。
fn extract_message_text(msg: &Message) -> String {
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
    let trigger_threshold = (context_limit as f64 * trigger_ratio) as usize;
    let compress_target = (context_limit as f64 * compress_ratio) as usize;

    // 预计算每条消息的 token 数（O(n)），避免循环内重复估算（原 O(n²)）
    let per_message_tokens: Vec<usize> = messages
        .iter()
        .map(|m| estimate_tokens(&extract_message_text(m)))
        .collect();
    let total_tokens: usize = per_message_tokens.iter().sum();

    if total_tokens <= trigger_threshold {
        return Vec::new();
    }

    // 从旧端逐条计算需要移出的数量（累减 token，直到剩余 ≤ compress_target）
    let mut dropped_tokens = 0usize;
    let mut drop_count = 0;
    for &tokens in &per_message_tokens {
        if total_tokens - dropped_tokens <= compress_target {
            break;
        }
        dropped_tokens += tokens;
        drop_count += 1;
    }

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
        Self { pool, config }
    }

    /// 获取共享配置句柄（供设置页 IPC 读写 memory 配置，0.13.1.4 将使用）。
    #[allow(dead_code)]
    pub fn config_handle(&self) -> Arc<RwLock<MemoryConfig>> {
        self.config.clone()
    }

    /// 更新 context_limit（模型切换时调用）。
    ///
    /// `limit` 为 None 时使用保守默认（8K）。
    pub async fn update_context_limit(&self, limit: Option<usize>) {
        let mut cfg = self.config.write().await;
        cfg.context_limit = limit.or(Some(DEFAULT_CONTEXT_LIMIT));
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

        let rows = crate::infra::data::conversations::load_recent_messages(
            &pool,
            conversation_id,
            load_limit,
        )
        .await
        .map_err(|e| MemoryError::Backend(Box::from(e)))?;

        let mut messages = Vec::with_capacity(rows.len());
        for (_role, content) in rows {
            let msg: Message =
                serde_json::from_str(&content).map_err(|e| MemoryError::Backend(Box::from(e)))?;
            messages.push(msg);
        }

        let mut dropped_count = 0usize;

        // 0.13.1: token-aware 裁剪（仅 TokenAware 模式）
        // 0.13.2: 裁剪出的消息归档到 FTS5
        if cfg.mode == WindowMode::TokenAware {
            let context_limit = cfg.context_limit.unwrap_or(DEFAULT_CONTEXT_LIMIT);
            let dropped_msgs = token_aware_truncate(
                &mut messages,
                context_limit,
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
                    context_limit,
                    "归档钩子：{} 条消息已归档到 FTS5",
                    dropped_msgs.len()
                );
            }
        }

        // 0.12.8: 滑动窗口/token 裁剪可能截断 ToolCall/ToolResult 配对——
        // 丢弃开头所有孤立的 ToolResult 消息。
        drop_leading_orphan_tool_results(&mut messages);

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

        Ok(MemoryLoadResult {
            messages,
            dropped_count,
            recall_count,
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
        tracing::debug!(
            mode = ?new_config.mode,
            window_size = new_config.window_size,
            trigger_ratio = new_config.trigger_ratio,
            compress_ratio = new_config.compress_ratio,
            recall_enabled = new_config.recall_enabled,
            recall_top_k = new_config.recall_top_k,
            context_limit = ?new_config.context_limit,
            "apply_config: 更新记忆策略配置"
        );
        *cfg = new_config;
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
                    if let UserContent::Text(t) = item {
                        if !t.text.is_empty() {
                            return t.text.clone();
                        }
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
        match msg {
            Message::User { content } => {
                // 检查是否 **全部** 是 ToolResult（纯 ToolResult 消息）
                let all_tool_results = content
                    .iter()
                    .all(|c| matches!(c, UserContent::ToolResult(_)));
                if all_tool_results && !content.is_empty() {
                    drop_count += 1;
                    continue;
                }
            }
            _ => {}
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

        Box::pin(async move {
            // 自动创建 conversation 记录（已存在则 IGNORE）
            let title = Self::extract_title(&messages);
            crate::infra::data::conversations::create_conversation(
                &pool,
                conversation_id,
                if title.is_empty() { None } else { Some(&title) },
            )
            .await
            .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            // 逐条插入消息
            for msg in &messages {
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
            Ok(())
        })
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::Text;
    use rig_core::one_or_many::OneOrMany;

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
            content: OneOrMany::one(UserContent::Text(Text::new(text))),
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(rig_core::completion::message::AssistantContent::Text(
                Text::new(text),
            )),
        }
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
            recall_enabled: true,
            recall_top_k: 3,
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
            Message::User { content } => match &content.first() {
                UserContent::Text(t) => t.text.clone(),
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
            content: OneOrMany::one(rig_core::completion::message::AssistantContent::ToolCall(
                ToolCall::new(
                    "call_1".to_string(),
                    ToolFunction::new("search".to_string(), serde_json::json!({"q": "test"})),
                ),
            )),
        };

        mem.append("c1", vec![user_msg("search for test"), assistant_with_tool])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);

        // 第二条是 assistant 消息，应包含 ToolCall
        match &loaded[1] {
            Message::Assistant { content, .. } => match &content.first() {
                rig_core::completion::message::AssistantContent::ToolCall(tc) => {
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
                content: OneOrMany::one(UserContent::ToolResult(ToolResult {
                    id: id.to_string(),
                    call_id: None,
                    content: OneOrMany::one(ToolResultContent::text("ok")),
                })),
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
                content: OneOrMany::one(rig_core::completion::message::AssistantContent::ToolCall(
                    ToolCall::new(
                        "call_1".to_string(),
                        ToolFunction::new("search".to_string(), serde_json::json!({})),
                    ),
                )),
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
            tokens >= 2 && tokens <= 3,
            "English estimate should be ~2-3, got {tokens}"
        );
    }

    #[test]
    fn estimate_tokens_chinese() {
        // "你好世界测试" = 6 CJK chars, 6/1.5 = 4 tokens
        let tokens = estimate_tokens("你好世界测试");
        assert!(
            tokens >= 3 && tokens <= 6,
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
        if let Message::User { content } = &msgs[0] {
            if let Some(UserContent::Text(t)) = content.iter().next() {
                // 第一条剩余消息的编号应该 > 0（旧消息被移出）
                assert!(
                    t.text.contains("message number"),
                    "First message should be a user message"
                );
            }
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: false, // 先关召回，验证归档
            recall_top_k: 3,
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
        let recalls =
            crate::infra::data::conversations::search_memory_fts(&pool, "c1", "topic_0", 10)
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: false,
            recall_top_k: 3,
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
            recall_enabled: false,
            recall_top_k: 3,
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
            recall_enabled: true,
            recall_top_k: 3,
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
            recall_enabled: false,
            recall_top_k: 3,
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
}
