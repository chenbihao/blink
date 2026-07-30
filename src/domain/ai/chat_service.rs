//! ChatService — 对话窗口 Agent 与请求生命周期管理（0.12.1）。
//!
//! ## Phase 3B-1
//!
//! - memory 归 ChatService 所有，重建 AgentProvider 时复用，避免切换 Provider/Model 丢对话。
//! - 从 `Tier::Main` 懒解析并构造 AgentProvider。
//! - 缓存 key 与 `AIProviderRegistry` 共用同一 fingerprint 规则；配置或密钥变化后自动重建。
//!
//! ## Phase 3B-2
//!
//! - 同一时间只允许一个 active prompt，每次请求分配单调递增的 request_id。
//! - AbortHandle 统一承接 Esc / X / hide / Stop 中断。
//! - 自然完成时按 request_id compare-and-clear，旧任务不会误清新请求。
//! - Phase 4 消费 `ChatPromptHandle.chunks` 并定向 emit 前端。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::domain::ai::memory::{
    MemoryLoadResult, SqliteConversationMemory, estimate_messages_tokens, estimate_tokens,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::domain::ai::agent_provider::{AgentProvider, ChatStreamChunk};
use crate::domain::ai::prompt::chat_system_prompt_with_skills;
use crate::domain::ai::provider::AIError;
use crate::domain::ai::registry::{AIProviderRegistry, ResolvedProviderEntries};
use crate::domain::ai::skill::{SkillRegistry, parse_skill_command};
use crate::domain::ai::tool_adapter::{PendingConfirms, build_agent_tools};
use crate::domain::capability::CapabilityRegistry;
use crate::domain::config::ai_config::Tier;
use crate::domain::event::DomainEnv;
use crate::domain::event_names::EventNames;
use crate::domain::mcp::McpClientManager;

/// Agent 缓存 key——provider/model/fingerprint/preamble_hash/MCP epoch 任一变化即 cache miss。
#[derive(PartialEq)]
struct AgentCacheKey {
    provider_id: String,
    model_id: String,
    fingerprint: String,
    preamble_hash: u64,
    /// MCP tool 池版本号——拓扑变化时 bump，触发 Agent 重建。
    mcp_epoch: u64,
}

struct CachedAgent {
    key: AgentCacheKey,
    provider: Arc<AgentProvider>,
}

struct ActiveChatRequest {
    request_id: u64,
    conversation_id: String,
    abort_handle: AbortHandle,
}

/// 可序列化的 active request 快照，供 Phase 4 `get_chat_status` 使用。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActiveChatStatus {
    pub request_id: u64,
    pub conversation_id: String,
}

// ── 0.13.6: 上下文窗口状态 ──────────────────────────────────────────────────────

/// 聊天窗口上下文窗口状态（0.13.6）。
///
/// 每次 prompt 前通过 `blink://chat-context-status` 事件推送到前端，
/// 驱动 composer bar 上的环形进度条指示器。
#[derive(Clone, Debug, serde::Serialize)]
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
}

// ── 0.13.6: Skill 激活 Signal ──────────────────────────────────────────────────

/// Skill 激活事件（推送 `blink://chat-skill-activated`）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct SkillActivationSignal {
    pub request_id: u64,
    pub skills: Vec<SkillActivationInfo>,
}

/// 单个 Skill 激活信息。
#[derive(Clone, Debug, serde::Serialize)]
pub struct SkillActivationInfo {
    pub name: String,
    pub source: String,
    /// "explicit" = /skill 指令激活, "auto" = 关键词/正则自动触发。
    pub trigger_type: &'static str,
}

/// ChatService 状态快照。
///
/// 0.12.2 扩展：`provider_name` / `model_name` 供前端 header 标签实时展示
/// 当前生效的 Provider + Model（selected 优先，否则 Main 档回落）。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatStatus {
    pub active: Option<ActiveChatStatus>,
    pub provider_configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
}

/// chat 窗口运行时选中的模型（0.12.2 §4.4）。
///
/// `None` 表示用 `Tier::Main` 默认；`Some` 表示用户在模型选择器里显式选了
/// 某个 provider+model。存内存（RwLock），重启回落 Main，0.12.3 持久化。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChatModelSelection {
    pub provider_id: String,
    pub model_id: String,
    pub provider_display_name: String,
    pub model_display_name: String,
}

/// `prompt()` 返回给 Phase 4 IPC 层的请求句柄。
///
/// IPC 层持有 `chunks` 并逐项包装 request_id / conversation_id 后 `emit_to("chat", ...)`。
pub struct ChatPromptHandle {
    pub request_id: u64,
    pub conversation_id: String,
    pub chunks: mpsc::UnboundedReceiver<ChatStreamChunk>,
}

/// 定向发送到 chat 窗口的流式事件包装（Phase 4）。
///
/// 所有 `blink://chat-stream` 事件必须携带 `request_id` 和 `conversation_id`，
/// 前端只消费当前 request_id，防止已中止请求的尾部 chunk 混入新回复。
#[derive(Clone, serde::Serialize)]
pub struct ChatStreamEvent {
    pub request_id: u64,
    pub conversation_id: String,
    pub chunk: ChatStreamChunk,
}

/// ChatService 请求错误。
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("已有对话请求正在生成（request_id={}）", .0.request_id)]
    AlreadyActive(ActiveChatStatus),
    #[error(transparent)]
    Provider(#[from] AIError),
}

/// active request 的并发状态，独立封装以便纯逻辑测试。
struct RequestTracker {
    next_request_id: AtomicU64,
    active: Mutex<Option<ActiveChatRequest>>,
}

impl RequestTracker {
    fn new() -> Self {
        // 0 保留为“无请求”，首个真实 request_id 从 1 开始。
        Self {
            next_request_id: AtomicU64::new(1),
            active: Mutex::new(None),
        }
    }

    fn next_id(&self) -> u64 {
        self.next_request_id.fetch_add(1, Ordering::Relaxed)
    }

    fn status(&self) -> Option<ActiveChatStatus> {
        self.active
            .lock()
            .expect("chat active lock poisoned")
            .as_ref()
            .map(|active| ActiveChatStatus {
                request_id: active.request_id,
                conversation_id: active.conversation_id.clone(),
            })
    }

    fn install(&self, request: ActiveChatRequest) -> Result<(), ActiveChatStatus> {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        if let Some(current) = active.as_ref() {
            return Err(ActiveChatStatus {
                request_id: current.request_id,
                conversation_id: current.conversation_id.clone(),
            });
        }
        *active = Some(request);
        Ok(())
    }

    fn abort(&self, request_id: u64) -> bool {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        if active
            .as_ref()
            .is_none_or(|request| request.request_id != request_id)
        {
            return false;
        }
        let request = active.take().expect("active request checked above");
        request.abort_handle.abort();
        true
    }

    fn abort_active(&self) -> bool {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        let Some(request) = active.take() else {
            return false;
        };
        request.abort_handle.abort();
        true
    }

    fn clear_if(&self, request_id: u64) -> bool {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        if active
            .as_ref()
            .is_none_or(|request| request.request_id != request_id)
        {
            return false;
        }
        active.take();
        true
    }
}

/// 对话服务。
pub struct ChatService {
    emitter: Arc<dyn DomainEnv>,
    ai_registry: Arc<AIProviderRegistry>,
    capability_registry: Arc<CapabilityRegistry>,
    pending_confirms: Arc<PendingConfirms>,
    /// 0.13.0: MCP client 管理器——collect_tools() 拉 MCP tool 进对话窗口 tool 池。
    mcp_client: Arc<McpClientManager>,
    memory: Arc<SqliteConversationMemory>,
    /// 0.13.3: Skill 注册表——启动时扫描，可手动刷新。
    skill_registry: SkillRegistry,
    cached_agent: RwLock<Option<CachedAgent>>,
    requests: RequestTracker,
    /// 串行化 prompt 启动过程，防止两个并发 IPC 同时通过 active 检查。
    start_gate: tokio::sync::Mutex<()>,
    /// 运行时选中的模型（None = 用 Tier::Main 默认）。0.12.2 §4.4。
    selected: RwLock<Option<ChatModelSelection>>,
    /// 配置库连接池（持久化模型选择到 config 表）。
    config_pool: sqlx::SqlitePool,
    /// 0.13.6: 上次计算的上下文窗口状态（供 `get_context_window_status` command 查询）。
    last_context_status: RwLock<Option<ContextWindowStatus>>,
}

impl ChatService {
    /// 构造 ChatService。AgentProvider 首次 prompt 时才懒构造，不增加启动路径耗时。
    ///
    /// 0.12.3：memory 从 `InMemoryConversationMemory` 换为 `SqliteConversationMemory`，
    /// 持久化到 AI 库，重启不丢历史。
    /// 0.13.1：memory 持有具体类型 `Arc<SqliteConversationMemory>`（不再是 trait object），
    /// 供 `AgentProvider::new` 注入 `model.context_window` 驱动 token-aware 裁剪。
    pub async fn new(
        emitter: Arc<dyn DomainEnv>,
        ai_registry: Arc<AIProviderRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
        pending_confirms: Arc<PendingConfirms>,
        mcp_client: Arc<McpClientManager>,
        ai_pool: sqlx::SqlitePool,
        config_pool: sqlx::SqlitePool,
    ) -> Self {
        // 从配置库加载持久化的模型选择（0.12.7）
        // 0.14.7 W1: async 边界收敛在调用方（wiring），domain 内不再嵌套 runtime
        let selected = Self::load_selected_model(&config_pool, &ai_registry).await;
        Self {
            emitter,
            ai_registry,
            capability_registry,
            pending_confirms,
            mcp_client,
            memory: Arc::new(crate::domain::ai::memory::SqliteConversationMemory::new(
                ai_pool,
            )),
            skill_registry: SkillRegistry::new(),
            cached_agent: RwLock::new(None),
            last_context_status: RwLock::new(None),
            requests: RequestTracker::new(),
            start_gate: tokio::sync::Mutex::new(()),
            selected: RwLock::new(selected),
            config_pool,
        }
    }

    /// 从配置库加载持久化的模型选择（async）。
    ///
    /// 读取 `chat:selected_model` config key（格式 `"{provider_id}:{model_id}"`），
    /// 校验 provider/model 仍存在且有 Chat 能力后恢复选择。失效则返回 None（回落 Main）。
    async fn load_selected_model(
        config_pool: &sqlx::SqlitePool,
        ai_registry: &AIProviderRegistry,
    ) -> Option<ChatModelSelection> {
        let selection_id =
            crate::infra::data::config::get_config(config_pool, "chat:selected_model").await?;

        let (provider_id, model_id) = selection_id.split_once(':')?;
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }

        // 校验 provider/model 仍存在
        let config = ai_registry.config_snapshot();
        let provider = config.providers.iter().find(|p| p.id == provider_id)?;
        let model = provider.models.iter().find(|m| m.id == model_id)?;
        if !model
            .capabilities
            .contains(&crate::domain::config::ai_config::ModelCapability::Chat)
        {
            return None;
        }

        let model_name = if model.display_name.is_empty() {
            model.id.clone()
        } else {
            model.display_name.clone()
        };
        tracing::info!(
            provider = %provider.display_name,
            model = %model_name,
            "ChatService: 从配置恢复持久化模型选择"
        );
        Some(ChatModelSelection {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            provider_display_name: provider.display_name.clone(),
            model_display_name: model_name,
        })
    }

    /// 解析当前应使用的 Provider+Model entries（0.12.2 §4.4）。
    ///
    /// 优先级：`selected`（用户在模型选择器显式选的）→ `Tier::Main`（回落）。
    /// selected 引用已失效（provider/model 被删）时自动回落 Main 并清 selected。
    ///
    /// 返回 `ResolvedProviderEntries`（含 cache_key，供缓存命中判断）。
    fn resolve_current_entries(&self) -> Result<ResolvedProviderEntries, AIError> {
        let selected = self
            .selected
            .read()
            .expect("selected lock poisoned")
            .clone();
        if let Some(sel) = selected {
            match self
                .ai_registry
                .resolve_explicit_entries(&sel.provider_id, &sel.model_id)
            {
                Ok(entries) => return Ok(entries),
                Err(AIError::NotConfigured) => {
                    // selected 引用的 model 已被删/禁用——清空 selected 回落 Main
                    tracing::warn!(
                        provider_id = %sel.provider_id,
                        model_id = %sel.model_id,
                        "ChatService: 选中的模型已不可用，回落 Main 档"
                    );
                    *self.selected.write().expect("selected lock poisoned") = None;
                }
                Err(other) => return Err(other),
            }
        }
        self.ai_registry.resolve_entries(Tier::Main)
    }

    /// 返回当前生效的 AgentProvider；配置未变时复用，变化时锁外重建。
    ///
    /// 0.12.2：生效模型由 `resolve_current_entries()` 决定（selected 优先，Main 回落）。
    /// 两次解析用于防止构造期间设置被修改：若 key 已变化，丢弃刚构造的旧实例并重试。
    /// 最多重试 `MAX_PROVIDER_RETRY` 次，防止极端情况下无限循环。
    ///
    /// 0.12.6：`preamble` 参数支持分组级系统提示词——不同分组的 preamble hash
    /// 不同，cache key 自然失配，触发重建。传空字符串等同默认 `chat_system_prompt()`。
    pub(crate) async fn ensure_provider(
        &self,
        preamble: &str,
    ) -> Result<Arc<AgentProvider>, AIError> {
        const MAX_PROVIDER_RETRY: usize = 3;
        let mut retry_count = 0;
        let preamble_hash = hash_preamble(preamble);
        loop {
            if retry_count >= MAX_PROVIDER_RETRY {
                tracing::warn!("ChatService: ensure_provider 达到最大重试次数，放弃");
                return Err(AIError::Cancelled);
            }
            retry_count += 1;
            let resolved = self.resolve_current_entries()?;

            // 0.13.7: lazy connect——确保 MCP server 已连接后再读 epoch，拿到最新 tool 池版本。
            // MCP 拓扑变化（server 连接/断开/disabled_tools 变化）会 bump epoch，
            // 使 AgentCacheKey 失配，触发 Agent 重建——修「首次连接慢的 server 未进 agent 缓存」。
            self.mcp_client.ensure_connected(&self.config_pool).await;
            let mcp_epoch = self.mcp_client.tool_pool_epoch();

            let cache_key = AgentCacheKey {
                provider_id: resolved.cache_key.0.clone(),
                model_id: resolved.cache_key.1.clone(),
                fingerprint: resolved.cache_key.2.clone(),
                preamble_hash,
                mcp_epoch,
            };

            if let Some(provider) = self.cached_provider(&cache_key) {
                return Ok(provider);
            }

            // Client/Agent 构造可能读取 Credential Manager，必须在 ChatService 锁外进行。
            // 0.13.0: MCP tool 通过 external_tools 入口进池——collect_tools() 从已连接的
            // MCP server 拉 tool（过滤 disabled_tools），与内置 Capability/Action 并列。
            let external_tools = self.mcp_client.collect_tools().await;
            let tools = build_agent_tools(
                &self.capability_registry,
                external_tools,
                self.emitter.clone(),
                self.pending_confirms.clone(),
            );
            let provider = Arc::new(
                AgentProvider::new(
                    &resolved.provider,
                    &resolved.model,
                    tools,
                    preamble,
                    self.memory.clone(),
                )
                .await?,
            );

            // 构造期间配置可能已更新。只提交仍对应当前 key 的实例。
            let latest = self.resolve_current_entries()?;
            if latest.cache_key != resolved.cache_key {
                tracing::debug!(
                    old_provider = %resolved.provider.id,
                    old_model = %resolved.model.id,
                    new_provider = %latest.provider.id,
                    new_model = %latest.model.id,
                    "ChatService: Agent 构造期间配置变化，丢弃旧实例并重试"
                );
                continue;
            }

            *self
                .cached_agent
                .write()
                .expect("chat agent cache lock poisoned") = Some(CachedAgent {
                key: cache_key,
                provider: provider.clone(),
            });
            tracing::info!(
                provider = %resolved.provider.display_name,
                model = %resolved.model.id,
                "ChatService: AgentProvider 已就绪"
            );
            return Ok(provider);
        }
    }

    /// 设置运行时选中模型（0.12.2 §4.4）。
    ///
    /// `Some` = 用户在模型选择器选了某个 provider+model；`None` = 恢复 Main 档默认。
    /// 写入后清 cached_agent，下次 prompt 按新选择重建 AgentProvider。memory 不动。
    pub fn select_model(&self, selection: Option<ChatModelSelection>) {
        // 持久化到配置库（0.12.7）
        let key = selection
            .as_ref()
            .map(|s| format!("{}:{}", s.provider_id, s.model_id));
        let pool = self.config_pool.clone();
        tokio::spawn(async move {
            if let Some(id) = key {
                let _ =
                    crate::infra::data::config::set_config(&pool, "chat:selected_model", &id).await;
            } else {
                let _ =
                    crate::infra::data::config::delete_config(&pool, "chat:selected_model").await;
            }
        });
        *self.selected.write().expect("selected lock poisoned") = selection.clone();
        *self
            .cached_agent
            .write()
            .expect("chat agent cache lock poisoned") = None;
        if let Some(sel) = &selection {
            tracing::info!(
                provider = %sel.provider_display_name,
                model = %sel.model_display_name,
                "ChatService: 用户切换模型"
            );
        } else {
            tracing::info!("ChatService: 模型选择恢复 Main 档");
        }
    }

    /// 当前选中的模型快照（供 commands 层 `get_chat_models` 标注 is_selected）。
    pub fn current_selection(&self) -> Option<ChatModelSelection> {
        self.selected
            .read()
            .expect("selected lock poisoned")
            .clone()
    }

    fn cached_provider(&self, key: &AgentCacheKey) -> Option<Arc<AgentProvider>> {
        self.cached_agent
            .read()
            .expect("chat agent cache lock poisoned")
            .as_ref()
            .filter(|cached| &cached.key == key)
            .map(|cached| cached.provider.clone())
    }

    /// 启动一个 prompt，返回流式 chunk receiver 给 Phase 4 IPC 层。
    ///
    /// `start_gate` 只串行化启动过程，不影响 status/abort；active 安装后才放行 Agent task，
    /// 避免极短请求在安装 active 之前完成而留下僵尸状态。
    ///
    /// 0.12.6：`group_system_prompt` 注入分组级系统提示词——构造 preamble 时
    /// 追加到基础 chat system prompt 之后，影响 Agent 的行为约束。
    /// 0.13.3：preamble 集成 Skill 渐进式披露——阶段 1 摘要常驻 + 阶段 2 触发全文注入。
    /// 支持 `/skill <name>` 显式激活指令。
    pub async fn prompt(
        self: &Arc<Self>,
        conversation_id: String,
        message: String,
        group_system_prompt: Option<String>,
    ) -> Result<ChatPromptHandle, ChatError> {
        let _start_guard = self.start_gate.lock().await;
        if let Some(active) = self.requests.status() {
            return Err(ChatError::AlreadyActive(active));
        }

        let request_id = self.requests.next_id();
        let (chunk_tx, chunk_rx) = mpsc::unbounded_channel();
        let (start_tx, start_rx) = oneshot::channel();
        let conversation_for_task = conversation_id.clone();
        let weak_service: Weak<Self> = Arc::downgrade(self);

        // 0.13.3：构建 skill-aware preamble
        // 1. 检查 /skill 显式激活指令
        // 2. 阶段 1：所有 Skill 摘要常驻
        // 3. 阶段 2：触发匹配（关键词/正则）或显式激活的 Skill 全文注入
        let (effective_message, triggered_skills) = self.resolve_skill_triggers(&message);

        let skill_summaries = self.skill_registry.summaries();
        let preamble = chat_system_prompt_with_skills(
            group_system_prompt.as_deref(),
            &skill_summaries,
            &triggered_skills,
        );

        if !triggered_skills.is_empty() {
            tracing::info!(
                request_id,
                triggered = triggered_skills.len(),
                skills = ?triggered_skills.iter().map(|s| format!("{}({})", s.name, s.source.display_name())).collect::<Vec<_>>(),
                "ChatService: Skill 已激活"
            );
            // 0.13.6: 推送 Skill 激活事件到前端，渲染 Signal 消息
            let signal = SkillActivationSignal {
                request_id,
                skills: triggered_skills
                    .iter()
                    .map(|s| SkillActivationInfo {
                        name: s.name.clone(),
                        source: s.source.display_name().to_string(),
                        trigger_type: if message.starts_with("/skill")
                            || message.starts_with("/SKILL")
                        {
                            "explicit"
                        } else {
                            "auto"
                        },
                    })
                    .collect(),
            };
            let _ = self.emitter.emit_to(
                "chat",
                EventNames::CHAT_SKILL_ACTIVATED,
                serde_json::to_value(&signal).unwrap_or_default(),
            );
        }

        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            let Some(service) = weak_service.upgrade() else {
                return;
            };

            // Provider 构造也放进可 abort 的 task：窗口在冷构造期间关闭时仍能立即中断。
            match service.ensure_provider(&preamble).await {
                Ok(provider) => {
                    // 0.13.6: 在 stream_prompt 前计算上下文窗口状态并推送前端
                    // 传入 pending message + preamble，因为此时消息尚未写入 DB，
                    // 纯靠 load_with_stats 读 DB 会得到空列表 → token=0（修「永远是 0%」bug）
                    let context_status = service
                        .compute_context_status(
                            &conversation_for_task,
                            Some(&effective_message),
                            Some(&preamble),
                        )
                        .await;
                    let _ = service.emitter.emit_to(
                        "chat",
                        EventNames::CHAT_CONTEXT_STATUS,
                        serde_json::to_value(&context_status).unwrap_or_default(),
                    );

                    // 0.12.9：移除时间注入到 user message 末尾——之前将
                    // [当前时间：...] 追加到 user msg 后被 rig memory 持久化到 DB，
                    // 导致：
                    // 1. 切换对话重新加载时用户消息气泡显示时间后缀
                    // 2. 标题生成 LLM 可能看到时间文本，生成 [当前时间：...] 作为标题
                    //
                    // 时间上下文如未来需要，应通过 non-persistent 机制注入（如
                    // rig agent 的 runtime preamble 或独立 system message），
                    // 不能污染 conversation memory。
                    //
                    // 当前发送干净的用户消息给 agent：
                    provider
                        .stream_prompt(&conversation_for_task, &effective_message, chunk_tx)
                        .await;
                }
                Err(error) => {
                    // 0.12.9：provider 构造失败时记录日志，便于排查配置/密钥问题
                    tracing::warn!(
                        target: crate::infra::utils::perf::ai_slo::TARGET,
                        conversation_id = %conversation_for_task,
                        error = %error,
                        "ChatService: provider 构造失败，流式请求无法启动"
                    );
                    let _ = chunk_tx.send(ChatStreamChunk::Error {
                        message: error.to_string(),
                    });
                }
            }
            if service.requests.clear_if(request_id) {
                tracing::debug!(request_id, "ChatService: 请求自然完成，active 已清除");
            }
        });
        let abort_handle = task.abort_handle();
        // task 的生命周期由 AbortHandle + Tokio runtime 管理；不 await，避免阻塞 prompt IPC。
        drop(task);

        self.requests
            .install(ActiveChatRequest {
                request_id,
                conversation_id: conversation_id.clone(),
                abort_handle,
            })
            .map_err(ChatError::AlreadyActive)?;

        if start_tx.send(()).is_err() {
            self.requests.clear_if(request_id);
            return Err(ChatError::Provider(AIError::Cancelled));
        }

        tracing::info!(
            request_id,
            conversation_id = %conversation_id,
            "ChatService: prompt 已启动"
        );
        Ok(ChatPromptHandle {
            request_id,
            conversation_id,
            chunks: chunk_rx,
        })
    }

    /// 精确中止指定 request_id。过期或不存在返回 false，不影响当前新请求。
    pub fn abort(&self, request_id: u64) -> bool {
        let aborted = self.requests.abort(request_id);
        if aborted {
            tracing::info!(request_id, "ChatService: 请求已中止");
        }
        aborted
    }

    /// 中止当前 active request，供 X / Esc / hide 生命周期入口调用。
    pub fn abort_active(&self) -> bool {
        let active = self.requests.status();
        let aborted = self.requests.abort_active();
        if aborted && let Some(active) = active {
            tracing::info!(
                request_id = active.request_id,
                "ChatService: active 请求已中止"
            );
        }
        aborted
    }

    /// 当前 active request 上下文；Phase 4 注入 Dangerous confirm payload。
    pub fn current_request_context(&self) -> Option<(u64, String)> {
        self.requests
            .status()
            .map(|active| (active.request_id, active.conversation_id))
    }

    /// 返回 chat 状态快照。
    ///
    /// 0.12.2：`provider_name`/`model_name` 反映当前生效模型（selected 优先，Main 回落），
    /// 供前端 header 标签展示。`provider_configured` 沿用 Main 档语义（兼容旧前端）。
    pub fn status(&self) -> ChatStatus {
        let resolved = self.resolve_current_entries().ok();
        let (provider_name, model_name) = match &resolved {
            Some(r) => {
                let model_display = if r.model.display_name.is_empty() {
                    r.model.id.clone()
                } else {
                    r.model.display_name.clone()
                };
                (Some(r.provider.display_name.clone()), Some(model_display))
            }
            None => (None, None),
        };
        ChatStatus {
            active: self.requests.status(),
            provider_configured: self.ai_registry.resolve_entries(Tier::Main).is_ok(),
            provider_name,
            model_name,
        }
    }

    /// 配置变更后主动失效 Agent 缓存；memory 仍由 ChatService 持有。
    ///
    /// 0.12.2：额外校验 selected 引用的 model 是否还在——被删/禁用则清 selected，
    /// 回落 Main 档（避免下次 prompt 时 `resolve_current_entries` 才发现失效，
    /// 导致用户看到「刚选的模型突然不生效」的困惑）。
    pub fn notify_config_changed(&self) {
        *self
            .cached_agent
            .write()
            .expect("chat agent cache lock poisoned") = None;

        // selected 失效校验：引用的 provider/model 不存在或禁用则清空
        let selected = self
            .selected
            .read()
            .expect("selected lock poisoned")
            .clone();
        if let Some(sel) = selected {
            let still_valid = self
                .ai_registry
                .validate_model_exists(&sel.provider_id, &sel.model_id)
                .is_some();
            if !still_valid {
                tracing::warn!(
                    provider_id = %sel.provider_id,
                    model_id = %sel.model_id,
                    "ChatService: 选中的模型配置后已不可用，清除选择回落 Main 档"
                );
                *self.selected.write().expect("selected lock poisoned") = None;
            }
        }
        tracing::debug!("ChatService: 配置变化，AgentProvider 缓存已失效");
    }

    /// 应用用户从设置页修改的记忆策略配置（0.13.1 §3.7）。
    ///
    /// 在 `set_config('ai_config')` 命令处理中调用——保存 DB 后、`notify_config_changed`
    /// 之前。保留运行时注入的 `context_limit`（来自 `ModelEntry.context_window`），
    /// 只更新 `mode / window_size / trigger_ratio / compress_ratio`。
    pub async fn update_memory_config(&self, config: crate::domain::ai::memory::MemoryConfig) {
        self.memory.apply_config(config).await;
    }

    // ── 0.13.6: 上下文窗口状态 ──────────────────────────────────────────

    /// 计算当前对话的上下文窗口状态（0.13.6）。
    ///
    /// 调用 `memory.load_with_stats()` 获取窗口消息 + 压缩/召回统计，
    /// 估算 token 数并计算占用百分比。结果缓存在 `last_context_status` 供前端查询。
    ///
    /// `pending_message` 和 `preamble` 参数：在 stream_prompt 前调用时，
    /// 当前用户消息尚未写入 DB，系统提示词也不在消息列表中。
    /// 传入这两个参数可将它们的 token 纳入估算，避免首次对话显示 0%。
    pub async fn compute_context_status(
        &self,
        conversation_id: &str,
        pending_message: Option<&str>,
        preamble: Option<&str>,
    ) -> ContextWindowStatus {
        let result = match self.memory.load_with_stats(conversation_id).await {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(error = %e, "compute_context_status: load_with_stats 失败");
                MemoryLoadResult {
                    messages: Vec::new(),
                    dropped_count: 0,
                    recall_count: 0,
                }
            }
        };

        let config_handle = self.memory.config_handle();
        let cfg = config_handle.read().await;
        let context_limit = cfg.context_limit.unwrap_or(8192);

        let preamble_tokens = preamble.map(estimate_tokens).unwrap_or(0);
        let pending_message_tokens = pending_message.map(estimate_tokens).unwrap_or(0);
        let history_tokens = estimate_messages_tokens(&result.messages);
        let estimated_tokens = history_tokens + preamble_tokens + pending_message_tokens;
        let usage_percent = ((estimated_tokens * 100) / context_limit.max(1)).min(100) as u8;

        let status = ContextWindowStatus {
            estimated_tokens,
            context_limit,
            usage_percent,
            last_compressed: result.dropped_count > 0,
            last_compressed_count: result.dropped_count,
            last_recall_count: result.recall_count,
            preamble_tokens,
            pending_message_tokens,
        };

        // 缓存供 get_context_window_status command 查询
        *self
            .last_context_status
            .write()
            .expect("last_context_status lock poisoned") = Some(status.clone());

        tracing::debug!(
            conversation_id,
            estimated_tokens,
            context_limit,
            usage_percent,
            preamble_tokens,
            pending_message_tokens,
            dropped = result.dropped_count,
            recalled = result.recall_count,
            "compute_context_status: 上下文窗口状态已计算"
        );

        status
    }

    /// 返回上次计算的上下文窗口状态（供 `get_context_window_status` command）。
    /// 若从未计算过则返回 None。
    pub fn last_context_status(&self) -> Option<ContextWindowStatus> {
        self.last_context_status
            .read()
            .expect("last_context_status lock poisoned")
            .clone()
    }

    // ── 0.13.3 Skill 集成 ──────────────────────────────────────────────────

    /// 解析用户消息中的 Skill 触发，返回（实际发送给 AI 的消息, 触发的 Skill 列表）。
    ///
    /// 触发来源（合并去重）：
    /// 1. `/skill <name>` 显式激活——解析指令，查找 Skill，剩余消息作为实际消息
    /// 2. 关键词/正则自动触发——`SkillRegistry::match_triggers`
    ///
    /// `/skill` 指令的剩余消息为空时，实际消息保留原文（让 AI 看到 /skill 指令并回应）。
    fn resolve_skill_triggers(
        &self,
        message: &str,
    ) -> (String, Vec<crate::domain::ai::skill::SkillEntry>) {
        use crate::domain::ai::skill::SkillEntry;
        use std::collections::HashSet;

        let mut triggered: Vec<SkillEntry> = Vec::new();
        let mut effective_message = message.to_string();

        // 1. 检查 /skill 显式激活
        if let Some(cmd) = parse_skill_command(message) {
            if let Some(skill) = self.skill_registry.find_by_name(&cmd.name, cmd.source) {
                triggered.push(skill);
                // 剩余消息作为实际消息；为空时用原文（让 AI 看到指令并回应）
                if !cmd.remaining_message.is_empty() {
                    effective_message = cmd.remaining_message;
                }
                tracing::debug!(
                    skill = %cmd.name,
                    source = ?cmd.source,
                    "ChatService: /skill 显式激活"
                );
            } else {
                tracing::warn!(
                    skill = %cmd.name,
                    source = ?cmd.source,
                    "ChatService: /skill 未找到匹配的 Skill"
                );
                // 未找到时保留原文，让 AI 自然回应
            }
        }

        // 2. 关键词/正则自动触发（与显式激活合并去重）
        let auto_triggered = self.skill_registry.match_triggers(message);
        let existing_names: HashSet<String> = triggered
            .iter()
            .map(|s| format!("{}@{}", s.name, s.source.display_name()))
            .collect();
        for skill in auto_triggered {
            let key = format!("{}@{}", skill.name, skill.source.display_name());
            if !existing_names.contains(&key) {
                triggered.push(skill);
            }
        }

        (effective_message, triggered)
    }

    /// 刷新 Skill 注册表——重新扫描所有启用的来源目录。
    ///
    /// 供设置页「刷新」按钮和配置变更后调用。
    pub fn refresh_skills(&self, enabled_sources: &[crate::domain::ai::skill::SkillSource]) {
        self.skill_registry.scan(enabled_sources);
    }

    /// 0.13.6: 更新被禁用的 Skill 列表。
    ///
    /// 供 `set_skill_enabled` command 调用——前端切换 Skill 复选框后立即生效。
    pub fn update_skill_disabled(&self, disabled_skills: Vec<String>) {
        self.skill_registry.set_disabled_skills(disabled_skills);
    }

    /// 返回 SkillRegistry 的引用（供 command 层 list_skills 调用）。
    pub fn skill_registry(&self) -> &SkillRegistry {
        &self.skill_registry
    }
}

/// 从 Tauri state 获取当前 active request 上下文（request_id, conversation_id）。
///
/// 供 `tool_adapter` 在 emit dangerous confirm 时注入，前端按 request_id 校验事件归属。
/// ChatService 未注册时返回 `(0, String::new())`——confirm 仍可工作，只是前端无法校验归属。
pub fn current_request_context_from_env(env: &dyn DomainEnv) -> (u64, String) {
    if let Some(cs) = env.chat_service() {
        cs.current_request_context().unwrap_or((0, String::new()))
    } else {
        (0, String::new())
    }
}

/// 计算 preamble 的 hash 值，用于 AgentProvider 缓存 key 的第四元素（0.12.6）。
///
/// 不同分组的 system prompt 产生不同 hash，触发 cache miss 和 Agent 重建。
///
/// **注意**：`DefaultHasher` 是 SipHash1-3，稳定性不保证（跨 Rust 版本可能变）。
/// 当前仅用于进程内 cache key（不持久化），可接受。若未来需跨进程共享 cache，
/// 应换用稳定 hash（如 `ahash` 或固定 seed 的 `SipHash`）。
fn hash_preamble(preamble: &str) -> u64 {
    let mut hasher = DefaultHasher::new();
    preamble.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_request(request_id: u64, conversation_id: &str) -> ActiveChatRequest {
        let task = tokio::spawn(std::future::pending::<()>());
        let abort_handle = task.abort_handle();
        drop(task);
        ActiveChatRequest {
            request_id,
            conversation_id: conversation_id.to_string(),
            abort_handle,
        }
    }

    #[test]
    fn request_ids_are_monotonic_and_skip_zero() {
        let tracker = RequestTracker::new();
        assert_eq!(tracker.next_id(), 1);
        assert_eq!(tracker.next_id(), 2);
        assert_eq!(tracker.next_id(), 3);
    }

    #[tokio::test]
    async fn tracker_rejects_second_active_request() {
        let tracker = RequestTracker::new();
        tracker.install(pending_request(1, "c1")).unwrap();
        let existing = tracker.install(pending_request(2, "c2")).unwrap_err();
        assert_eq!(existing.request_id, 1);
        assert_eq!(existing.conversation_id, "c1");
        assert_eq!(tracker.status().unwrap().request_id, 1);
        tracker.abort_active();
    }

    #[tokio::test]
    async fn stale_completion_cannot_clear_new_request() {
        let tracker = RequestTracker::new();
        tracker.install(pending_request(1, "c1")).unwrap();
        assert!(tracker.abort(1));
        tracker.install(pending_request(2, "c2")).unwrap();

        assert!(!tracker.clear_if(1), "旧请求不得清除新 active");
        assert_eq!(tracker.status().unwrap().request_id, 2);
        assert!(tracker.clear_if(2));
        assert!(tracker.status().is_none());
    }

    #[tokio::test]
    async fn abort_requires_matching_request_id() {
        let tracker = RequestTracker::new();
        tracker.install(pending_request(7, "c7")).unwrap();
        assert!(!tracker.abort(6));
        assert_eq!(tracker.status().unwrap().request_id, 7);
        assert!(tracker.abort(7));
        assert!(tracker.status().is_none());
    }

    #[tokio::test]
    async fn abort_handle_cancels_underlying_task() {
        let tracker = RequestTracker::new();
        let (started_tx, started_rx) = oneshot::channel();
        let task = tokio::spawn(async move {
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        let abort_handle = task.abort_handle();
        tracker
            .install(ActiveChatRequest {
                request_id: 9,
                conversation_id: "c9".into(),
                abort_handle,
            })
            .unwrap();
        started_rx.await.unwrap();

        assert!(tracker.abort(9));
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[test]
    fn chat_error_reports_active_request_id() {
        let error = ChatError::AlreadyActive(ActiveChatStatus {
            request_id: 42,
            conversation_id: "c1".into(),
        });
        assert!(error.to_string().contains("42"));
    }
}
