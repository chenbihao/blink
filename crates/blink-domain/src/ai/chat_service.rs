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

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock, Weak};

use crate::ai::memory::{
    EphemeralConversationMemory, SqliteConversationMemory, estimate_tokens,
};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::ai::agent_provider::{AgentProvider, ChatStreamChunk};
use crate::ai::attachment::{
    AttachmentKind, ResolvedAttachment, TEXT_FILE_MAX_BYTES, TEXT_TOTAL_TOKEN_LIMIT, TextInjection,
    TextProbeStatus, build_text_injection, classify_text_probe, text_total_overrun,
};
use crate::ai::prompt::{
    chat_system_prompt_with_skills, pure_chat_system_prompt_with_group,
};
use crate::ai::provider::AIError;
use crate::ai::registry::{AIProviderRegistry, ResolvedProviderEntries};
use crate::ai::thinking::{ThinkingCapability, ThinkingIntent};
use crate::ai::skill::{SkillRegistry, SkillSummary, parse_skill_command};
use crate::ai::tool_adapter::{
    PendingConfirms, build_agent_tools, count_allowlisted_capabilities,
};
use crate::capability::CapabilityRegistry;
use crate::config::ai_config::{ChatAgentMode, ChatConfig, Tier};
use crate::event::EventPort;
use crate::event_names::EventNames;
use crate::mcp::McpClientManager;
use crate::resource::{DefaultResourceStore, ResourceRef, ResourceUse};

// 0.21.23 结构收敛：内聚子模块（外部契约零变化，纯内部重组）
// - context_status：上下文状态计算 + scoped 缓存 + 历史预算注入
// - summary_task：摘要水位/边界/合并 + 裸 Agent LLM 调用 + 记忆健康度
// - interceptor：校准采样 + effort 400 自愈 + Done 分发
mod context_status;
mod interceptor;
mod summary_task;

use context_status::ContextStatusCache;
pub use context_status::ContextWindowStatus;
pub use summary_task::{CompactJobEvent, ManualCompactPrecheck, MemoryHealthSummary};

// Agent 缓存 key——provider/model/fingerprint/preamble_hash/MCP epoch/kind 任一变化即 cache miss。
// ── 0.17.6: 对话类型 ────────────────────────────────────────────────────────

/// 对话类型（0.17.6）。
///
/// 决定 `ChatService` 使用哪种 memory：
/// - `Persistent`：`SqliteConversationMemory`，写入 `blink_ai.db`（对话窗口）
/// - `Ephemeral`：`EphemeralConversationMemory`，进程内不持久化（主窗口 AI）
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConversationKind {
    Persistent,
    Ephemeral,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct AgentCacheKey {
    provider_id: String,
    model_id: String,
    fingerprint: String,
    preamble_hash: u64,
    /// MCP tool 池版本号——拓扑变化时 bump，触发 Agent 重建。
    mcp_epoch: u64,
    /// 0.17.6: 对话类型——Persistent / Ephemeral 使用不同 memory，需独立缓存。
    kind: ConversationKind,
    /// 有扩展与无扩展的 Agent 必须隔离，禁止跨模式复用。
    extensions_enabled: bool,
    /// 0.21.5: AI capability allowlist fingerprint——allowlist 变化时触发 Agent 重建。
    /// 空池的 fingerprint 为 0；非空池为 sorted ids 的 hash。
    ai_capability_epoch: u64,
}

/// 单次 prompt 的 Agent 装配计划。三类扩展共用同一模式快照，避免边界漂移。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct AgentAssemblyPlan {
    extensions_enabled: bool,
}

impl AgentAssemblyPlan {
    fn for_mode(mode: ChatAgentMode, kind: ConversationKind) -> Self {
        let extensions_enabled = match mode {
            ChatAgentMode::Full => true,
            ChatAgentMode::MainChatOnly => kind == ConversationKind::Persistent,
            ChatAgentMode::PureChat => false,
        };
        Self { extensions_enabled }
    }

    fn includes_extensions(self) -> bool {
        self.extensions_enabled
    }
}

fn encode_agent_mode(mode: ChatAgentMode) -> u8 {
    match mode {
        ChatAgentMode::Full => 0,
        ChatAgentMode::MainChatOnly => 1,
        ChatAgentMode::PureChat => 2,
    }
}

fn decode_agent_mode(value: u8) -> ChatAgentMode {
    match value {
        1 => ChatAgentMode::MainChatOnly,
        2 => ChatAgentMode::PureChat,
        _ => ChatAgentMode::Full,
    }
}

struct CachedAgent {
    key: AgentCacheKey,
    provider: Arc<AgentProvider>,
}

struct ActiveChatRequest {
    request_id: u64,
    conversation_id: String,
    /// 0.17.6: 活跃请求所在窗口（"chat" / "main"），供 AlreadyActive 错误提示。
    target_window: String,
    abort_handle: AbortHandle,
    /// 0.23.4: 请求来源（§3.10 全局 AI 并发）。Editor 整理请求的取消路径唯一
    /// （cancel_editor_transform / 自然完成），chat 侧 abort 不得误杀——
    /// abort* 路径对 Editor 来源直接拒绝（见 `RequestTracker`）。
    origin: RequestOrigin,
}

/// 0.23.4: 活跃请求来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RequestOrigin {
    /// 对话/主窗口 prompt（abort 路径可中止底层任务）。
    Chat,
    /// 编辑器整理请求（abort 路径拒绝；取消走 EditorTransformService）。
    Editor,
}

/// 可序列化的 active request 快照，供 Phase 4 `get_chat_status` 使用。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ActiveChatStatus {
    pub request_id: u64,
    pub conversation_id: String,
    /// 0.17.6: 活跃请求所在窗口标签。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target_window: Option<String>,
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
///
/// 0.21.17 扩展：`supports_effort_levels` / `reasoning_effort` 供前端思考控件
/// 决定显示"强度下拉"还是"简单开关"，并恢复当前模型的思考强度配置。
/// 0.27.15 扩展：`thinking_capability`——能力快照（形态/档位/可关/可开·默认/
/// 预算范围/未知标记），前端按能力诚实渲染；旧字段保留兼容。
#[derive(Clone, Debug, serde::Serialize)]
pub struct ChatStatus {
    pub active: Option<ActiveChatStatus>,
    pub provider_configured: bool,
    pub provider_name: Option<String>,
    pub model_name: Option<String>,
    pub supports_effort_levels: bool,
    pub reasoning_effort: Option<String>,
    pub thinking_capability: Option<crate::ai::thinking::ThinkingCapability>,
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
    /// 0.17.6: 目标窗口标签（IPC 层据此 emit_to 正确窗口）。
    pub target_window: String,
    /// 冻结的历史代次，供 IPC 转发的迟到增量写入执行 CAS。
    pub history_revision: Option<i64>,
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

/// 0.27.9：附件 wire 载荷与校验/注入纯函数收敛在 `ai::attachment` 模块；
/// 此处 re-export 维持 `chat_service::ChatAttachmentInput` 旧路径兼容
/// （`chat_prompt` command 层引用）。
pub use crate::ai::attachment::ChatAttachmentInput;

/// ChatService 请求错误。
#[derive(Debug, thiserror::Error)]
pub enum ChatError {
    #[error("已有对话请求正在生成（request_id={}）", .0.request_id)]
    AlreadyActive(ActiveChatStatus),
    /// 附件校验/解引用失败——逐项明细（§5.9.2 禁止静默丢项），前端保留草稿。
    #[error("附件无法附加：{details}")]
    Attachment { details: String },
    #[error(transparent)]
    Provider(#[from] AIError),
}

/// active request 的并发状态，独立封装以便纯逻辑测试。
struct RequestTracker {
    next_request_id: AtomicU64,
    active: Mutex<Option<ActiveChatRequest>>,
}

/// 0.27.9：附件请求级绑定 guard——prompt 任务持有，随任务结束（正常/失败/
/// abort drop）清除 `attachment_refs`，保证 `read_attachment_text` 只在
/// 本轮请求生命周期内可用。
struct AttachmentRefsGuard {
    service: Weak<ChatService>,
    /// 是否真的写入了绑定（无附件时不写也不清，避免误清上一轮残留）。
    had_refs: bool,
}

impl AttachmentRefsGuard {
    fn set_if_any(service: &Arc<ChatService>, resolved: &[ResolvedAttachment]) -> Self {
        let had_refs = !resolved.is_empty();
        *service
            .attachment_refs
            .write()
            .expect("attachment_refs lock poisoned") = if had_refs {
            Some(
                resolved
                    .iter()
                    .map(|a| a.r#ref().to_string())
                    .collect::<Vec<_>>(),
            )
        } else {
            None
        };
        Self {
            service: Arc::downgrade(service),
            had_refs,
        }
    }
}

impl Drop for AttachmentRefsGuard {
    fn drop(&mut self) {
        if !self.had_refs {
            return;
        }
        if let Some(service) = self.service.upgrade() {
            *service
                .attachment_refs
                .write()
                .expect("attachment_refs lock poisoned") = None;
        }
    }
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
                target_window: Some(active.target_window.clone()),
            })
    }

    fn install(&self, request: ActiveChatRequest) -> Result<(), ActiveChatStatus> {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        if let Some(current) = active.as_ref() {
            return Err(ActiveChatStatus {
                request_id: current.request_id,
                conversation_id: current.conversation_id.clone(),
                target_window: Some(current.target_window.clone()),
            });
        }
        *active = Some(request);
        Ok(())
    }

    fn abort(&self, request_id: u64) -> bool {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        let Some(request) = active.as_ref() else {
            return false;
        };
        if request.request_id != request_id || request.origin != RequestOrigin::Chat {
            return false;
        }
        let request = active.take().expect("active request checked above");
        request.abort_handle.abort();
        true
    }

    fn abort_active(&self) -> bool {
        let mut active = self.active.lock().expect("chat active lock poisoned");
        // 0.23.4: Editor 来源不可被 chat 生命周期 abort 误杀——返回 false 且
        // 槽位保持，编辑器请求只能经自身取消路径或自然完成释放。
        if active
            .as_ref()
            .is_some_and(|request| request.origin != RequestOrigin::Chat)
        {
            return false;
        }
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
    emitter: Arc<dyn EventPort>,
    /// CapabilityEnv 引用——用于构造 InvokeContext 和 resource_store。
    cap_env: Arc<dyn crate::event::CapabilityEnv>,
    /// GUI surface（主进程 Some，CLI 等无 GUI 宿主 None）——AI tool 调 GUI starter
    /// 能力（open_settings 等）时经 InvokeContext 传给 Registry runtime 门禁。
    surface: Option<std::sync::Arc<dyn crate::capability::SurfacePort>>,
    ai_registry: Arc<AIProviderRegistry>,
    capability_registry: Arc<CapabilityRegistry>,
    pending_confirms: Arc<PendingConfirms>,
    /// 0.13.0: MCP client 管理器——collect_tools() 拉 MCP tool 进对话窗口 tool 池。
    mcp_client: Arc<McpClientManager>,
    /// 0.17.6: 持久化对话记忆（对话窗口使用，写入 SQLite）。
    persistent_memory: Arc<SqliteConversationMemory>,
    /// 0.17.6: 临时对话记忆（主窗口 AI 使用，进程内不持久化）。
    ephemeral_memory: Arc<EphemeralConversationMemory>,
    /// 0.13.3: Skill 注册表——启动时扫描，可手动刷新。
    skill_registry: SkillRegistry,
    cached_agent: RwLock<Option<CachedAgent>>,
    requests: RequestTracker,
    /// 串行化 prompt 启动过程，防止两个并发 IPC 同时通过 active 检查。
    start_gate: tokio::sync::Mutex<()>,
    /// 运行时选中的模型（None = 用 Tier::Main 默认）。0.12.2 §4.4。
    /// 仅用于 Persistent 对话（对话窗口）。
    selected: RwLock<Option<ChatModelSelection>>,
    /// 0.17.9: Ephemeral 对话（主窗口 AI）的独立模型选择。
    /// None = 用 Tier::Light 默认（Light 空则降级 Main）；Some = 用户显式选了模型。
    ephemeral_selected: RwLock<Option<ChatModelSelection>>,
    /// 配置库连接池（持久化模型选择到 config 表）。
    config_pool: sqlx::SqlitePool,
    /// 0.13.6: 上次计算的上下文窗口状态（供 `get_context_window_status` command 查询）。
    /// P0-2: 替换为 ContextStatusCache，按 conversation_id 查询，32 条上限淘汰。
    last_context_status: ContextStatusCache,
    /// 0.21.17: 真实 usage 校准器——按 provider_id + model_id 维护样本，修正启发式估算偏差。
    calibrator: crate::ai::token_budget::UsageCalibrator,
    /// 当前 Agent 能力模式；由 AIConfig 热更新，不修改其他 Capability 出口。
    agent_mode: AtomicU8,
    /// 主窗口模型策略：`light` / `main` / `provider_id:model_id`。
    ephemeral_model_policy: RwLock<String>,
    /// 0.21.19.1 F5: 摘要任务并发竞态防护——compare_exchange 抢占，
    /// 确保同一时间只有一个摘要任务在跑。
    summary_in_flight: std::sync::atomic::AtomicBool,
    /// 0.27.9：当前活跃请求已明确附加的 resource_ref 集合——
    /// `read_attachment_text` 的请求级绑定校验数据源（经
    /// `CapabilityEnv::active_chat_attachment_refs` 暴露给 Capability）。
    /// AI 请求全进程单活跃，至多一个集合。
    attachment_refs: std::sync::RwLock<Option<Vec<String>>>,
}

impl ChatService {
    /// 构造 ChatService。AgentProvider 首次 prompt 时才懒构造，不增加启动路径耗时。
    ///
    /// 0.12.3：memory 从 `InMemoryConversationMemory` 换为 `SqliteConversationMemory`，
    /// 持久化到 AI 库，重启不丢历史。
    /// 0.13.1：memory 持有具体类型 `Arc<SqliteConversationMemory>`（不再是 trait object），
    /// 供 `AgentProvider::new` 注入 `model.context_window` 驱动 token-aware 裁剪。
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        emitter: Arc<dyn EventPort>,
        cap_env: Arc<dyn crate::event::CapabilityEnv>,
        surface: Option<std::sync::Arc<dyn crate::capability::SurfacePort>>,
        ai_registry: Arc<AIProviderRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
        pending_confirms: Arc<PendingConfirms>,
        mcp_client: Arc<McpClientManager>,
        ai_pool: sqlx::SqlitePool,
        config_pool: sqlx::SqlitePool,
    ) -> Self {
        let chat_config = ai_registry.config_snapshot().chat_config;
        let agent_mode = chat_config.effective_agent_mode();
        let ephemeral_model_policy = chat_config.main_window_model.clone();
        let skill_registry = SkillRegistry::new();
        skill_registry.set_enabled(chat_config.skill_config.enabled);
        // 从配置库加载持久化的模型选择（0.12.7）
        // 0.14.7 W1: async 边界收敛在调用方（wiring），domain 内不再嵌套 runtime
        let selected = Self::load_selected_model(&config_pool, &ai_registry).await;
        // 0.17.9: 加载 Ephemeral 独立模型选择
        let ephemeral_selected =
            Self::load_ephemeral_selected_model(&config_pool, &ai_registry).await;
        Self {
            emitter,
            cap_env,
            surface,
            ai_registry,
            capability_registry,
            pending_confirms,
            mcp_client,
            persistent_memory: Arc::new(crate::ai::memory::SqliteConversationMemory::new(
                ai_pool,
            )),
            ephemeral_memory: Arc::new(
                crate::ai::memory::EphemeralConversationMemory::new(),
            ),
            skill_registry,
            cached_agent: RwLock::new(None),
            last_context_status: ContextStatusCache::new(),
            calibrator: crate::ai::token_budget::UsageCalibrator::new(),
            agent_mode: AtomicU8::new(encode_agent_mode(agent_mode)),
            ephemeral_model_policy: RwLock::new(ephemeral_model_policy),
            summary_in_flight: std::sync::atomic::AtomicBool::new(false),
            attachment_refs: std::sync::RwLock::new(None),
            requests: RequestTracker::new(),
            start_gate: tokio::sync::Mutex::new(()),
            selected: RwLock::new(selected),
            ephemeral_selected: RwLock::new(ephemeral_selected),
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
            blink_infra::data::config::get_config(config_pool, "chat:selected_model").await?;

        let (provider_id, model_id) = selection_id.split_once(':')?;
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }

        // 校验 provider/model 仍存在
        let config = ai_registry.config_snapshot();
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)?;
        let model = provider.models.iter().find(|m| m.id == model_id)?;
        if !model
            .capabilities
            .contains(&crate::config::ai_config::ModelCapability::Chat)
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

    /// 0.17.9: 从配置库加载 Ephemeral 对话的持久化模型选择。
    ///
    /// 读取 `chat:ephemeral_selected_model` config key，校验后恢复。失效则返回 None（回落 Light）。
    async fn load_ephemeral_selected_model(
        config_pool: &sqlx::SqlitePool,
        ai_registry: &AIProviderRegistry,
    ) -> Option<ChatModelSelection> {
        let selection_id =
            blink_infra::data::config::get_config(config_pool, "chat:ephemeral_selected_model")
                .await?;

        let (provider_id, model_id) = selection_id.split_once(':')?;
        if provider_id.is_empty() || model_id.is_empty() {
            return None;
        }

        let config = ai_registry.config_snapshot();
        let provider = config
            .providers
            .iter()
            .find(|p| p.id == provider_id && p.enabled)?;
        let model = provider.models.iter().find(|m| m.id == model_id)?;
        if !model
            .capabilities
            .contains(&crate::config::ai_config::ModelCapability::Chat)
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
            "ChatService: 从配置恢复 Ephemeral 模型选择"
        );
        Some(ChatModelSelection {
            provider_id: provider_id.to_string(),
            model_id: model_id.to_string(),
            provider_display_name: provider.display_name.clone(),
            model_display_name: model_name,
        })
    }

    /// 解析当前应使用的 Provider+Model entries（0.12.2 §4.4 / 0.17.9 分路）。
    ///
    /// 按 `kind` 分路：
    /// - `Persistent`：`selected` 优先 → `Tier::Main` 回落（行为不变）
    /// - `Ephemeral`：`ephemeral_selected` 优先 → `Tier::Light` 回落（Light 空则降级 Main）
    ///
    /// selected/ephemeral_selected 引用已失效（provider/model 被删）时自动清空并回落。
    ///
    /// 返回 `ResolvedProviderEntries`（含 cache_key，供缓存命中判断）。
    /// 0.21.17: 改为 pub，供 `compress_context_now` 等 management command 调用。
    pub fn resolve_current_entries(
        &self,
        kind: ConversationKind,
    ) -> Result<ResolvedProviderEntries, AIError> {
        match kind {
            ConversationKind::Persistent => {
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
                            tracing::warn!(
                                provider_id = %sel.provider_id,
                                model_id = %sel.model_id,
                                "ChatService: Persistent 选中的模型已不可用，回落 Main 档"
                            );
                            *self.selected.write().expect("selected lock poisoned") = None;
                        }
                        Err(other) => return Err(other),
                    }
                }
                self.ai_registry.resolve_entries(Tier::Main)
            }
            ConversationKind::Ephemeral => {
                let policy = self
                    .ephemeral_model_policy
                    .read()
                    .expect("ephemeral model policy lock poisoned")
                    .clone();
                if policy == "main" {
                    return self.ai_registry.resolve_entries(Tier::Main);
                }
                if policy != "light"
                    && let Some((provider_id, model_id)) = policy.split_once(':')
                {
                    match self
                        .ai_registry
                        .resolve_explicit_entries(provider_id, model_id)
                    {
                        Ok(entries) => return Ok(entries),
                        Err(AIError::NotConfigured) => {
                            tracing::warn!(
                                provider_id,
                                model_id,
                                "ChatService: 主窗口自选模型不可用，回落 Light 档"
                            );
                        }
                        Err(other) => return Err(other),
                    }
                }
                let selected = self
                    .ephemeral_selected
                    .read()
                    .expect("ephemeral_selected lock poisoned")
                    .clone();
                if let Some(sel) = selected {
                    match self
                        .ai_registry
                        .resolve_explicit_entries(&sel.provider_id, &sel.model_id)
                    {
                        Ok(entries) => return Ok(entries),
                        Err(AIError::NotConfigured) => {
                            tracing::warn!(
                                provider_id = %sel.provider_id,
                                model_id = %sel.model_id,
                                "ChatService: Ephemeral 选中的模型已不可用，回落 Light 档"
                            );
                            *self
                                .ephemeral_selected
                                .write()
                                .expect("ephemeral_selected lock poisoned") = None;
                        }
                        Err(other) => return Err(other),
                    }
                }
                // Ephemeral 默认走 Light 档（Light 空则 resolve_tier 自动降级 Main）
                self.ai_registry.resolve_entries(Tier::Light)
            }
        }
    }

    /// 返回当前生效的 AgentProvider；配置未变时复用，变化时锁外重建。
    ///
    /// 0.12.2：生效模型由 `resolve_current_entries()` 决定（selected 优先，Main 回落）。
    /// 两次解析用于防止构造期间设置被修改：若 key 已变化，丢弃刚构造的旧实例并重试。
    /// 最多重试 `MAX_PROVIDER_RETRY` 次，防止极端情况下无限循环。
    ///
    /// 0.12.6：`preamble` 参数支持分组级系统提示词——不同分组的 preamble hash
    /// 不同，cache key 自然失配，触发重建。传空字符串等同默认 `chat_system_prompt()`。
    async fn ensure_provider(
        self: &Arc<Self>,
        preamble: &str,
        kind: ConversationKind,
        plan: AgentAssemblyPlan,
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
            let resolved = self.resolve_current_entries(kind)?;

            // 0.19.11: prompt 复用预热中的 single-flight，最多等待独立的 5 秒 MCP
            // 准备预算。预算到期即以已就绪 tool snapshot 继续；后台连接完成后通过
            // epoch 让下一轮 Agent 自然重建，不占用模型/Capability 的 20 秒硬超时。
            let mcp_epoch = if plan.includes_extensions() {
                self.mcp_client
                    .prepare_enabled(
                        &self.config_pool,
                        crate::mcp::client::PROMPT_PREPARE_BUDGET,
                    )
                    .await;
                self.mcp_client.tool_pool_epoch()
            } else {
                0
            };

            // 0.21.5: 加载 AI capability allowlist + 计算 fingerprint
            // allowlist 变化时 fingerprint 不同，触发 Agent 重建
            let ai_allowlist = if plan.includes_extensions() {
                crate::config::ai_capability_access::AiCapabilityAccessStore::enabled_set(
                    &self.config_pool,
                )
                .await
            } else {
                std::collections::HashSet::new()
            };
            // 0.28.1：fingerprint 覆盖 allowlist ∩ 注册表实际条目——
            // 插件 tool 运行期注册/撤销（owner 维度变化）时，即使 allowlist
            // 不变也必须让陈旧 Agent 缓存失效，否则旧 Agent 持已注销的死工具。
            let ai_capability_epoch =
                compute_allowlist_fingerprint(&ai_allowlist, Some(&self.capability_registry));

            let cache_key = AgentCacheKey {
                provider_id: resolved.cache_key.0.clone(),
                model_id: resolved.cache_key.1.clone(),
                fingerprint: resolved.cache_key.2.clone(),
                preamble_hash,
                mcp_epoch,
                kind,
                extensions_enabled: plan.extensions_enabled,
                ai_capability_epoch,
            };

            // 0.21.21: context_limit 注入挪到缓存命中 early-return 之前——
            // 保证每轮都注入当前对话 resolved model 的值，缓存命中时不跳过。
            // memory 的 load_inner 裁剪 fallback 链
            // history_budget → context_limit → DEFAULT_CONTEXT_LIMIT 仍消费注入值。
            // 注意：compute_context_status 现已直读 resolved.model.context_window，
            // 此注入仅供 memory load_inner 裁剪 fallback 使用（history_budget 优先级不变）。
            if kind == ConversationKind::Persistent {
                let catalog_limit = resolved
                    .model
                    .context_window
                    .is_none()
                    .then(|| {
                        crate::ai::model_catalog::lookup_context_window(&resolved.model.id)
                    })
                    .flatten();
                let tier_kind = match resolved.provider.kind {
                    crate::config::ai_config::ProviderKind::OpenAICompatible => {
                        crate::ai::token_budget::TierProviderKind::OpenAiCompatible
                    }
                    crate::config::ai_config::ProviderKind::AnthropicMessages => {
                        crate::ai::token_budget::TierProviderKind::Anthropic
                    }
                    crate::config::ai_config::ProviderKind::GeminiGenerateContent => {
                        crate::ai::token_budget::TierProviderKind::Gemini
                    }
                    crate::config::ai_config::ProviderKind::OllamaHttp => {
                        crate::ai::token_budget::TierProviderKind::Ollama
                    }
                };
                let context_limit = Some(
                    resolved
                        .model
                        .context_window
                        .or(catalog_limit)
                        .map(|value| value as usize)
                        .unwrap_or_else(|| {
                            crate::ai::token_budget::tiered_fallback_context_limit(
                                tier_kind,
                                resolved.provider.base_url.as_deref(),
                            )
                        }),
                );
                self.persistent_memory
                    .update_context_limit(context_limit)
                    .await;
                tracing::debug!(
                    model = %resolved.model.id,
                    context_limit = ?context_limit,
                    "ChatService: 已注入 context_limit 到 persistent_memory (缓存命中前)"
                );
            }

            if let Some(provider) = self.cached_provider(&cache_key) {
                return Ok(provider);
            }

            // Client/Agent 构造可能读取 Credential Manager，必须在 ChatService 锁外进行。
            // 0.13.0: MCP tool 通过 external_tools 入口进池——collect_tools() 从已连接的
            // MCP server 拉 tool（过滤 disabled_tools），与内置 Capability/Action 并列。
            // 0.21.5: 只包装 AI allowlist 中的 Capability（用户授权 + policy 允许 AI）。
            // allowlist 已在 cache_key 构造前加载（ai_allowlist），此处复用。
            // 0.42: build_agent_tools 返回 (DynamicTools, McpToolBatch)
            let (dynamic_tools, mcp_tools): (
                Vec<rig_agent::tool::DynamicTool>,
                Vec<rig_rmcp::McpTool>,
            ) = if plan.includes_extensions() {
                let external_tools = self.mcp_client.collect_tools().await;
                build_agent_tools(
                    self.capability_registry.clone(),
                    external_tools,
                    self.emitter.clone(),
                    Some(self.clone()),
                    self.cap_env.clone(),
                    self.pending_confirms.clone(),
                    self.surface.clone(),
                    Some(&ai_allowlist),
                )
            } else {
                tracing::debug!("ChatService: 纯对话模式，Agent tool schema 为空");
                (Vec::new(), Vec::new())
            };
            // 0.17.6: 按 kind 选撞 memory——Persistent 用 SQLite，Ephemeral 用进程内。
            // 0.21.21: context_limit 注入已挪到缓存命中之前（上方），此处不再重复。
            // 0.27.13: 记忆抽象换为本仓 ChatMemory（rig 0.45 已移除 ConversationMemory）。
            let memory: Arc<dyn crate::ai::memory::ChatMemory> = match kind {
                ConversationKind::Persistent => self.persistent_memory.clone(),
                ConversationKind::Ephemeral => self.ephemeral_memory.clone(),
            };
            let provider = Arc::new(
                AgentProvider::new(
                    &resolved.provider,
                    &resolved.model,
                    dynamic_tools,
                    mcp_tools,
                    preamble,
                    memory,
                )
                .await?,
            );

            // 构造期间配置可能已更新。只提交仍对应当前 key 的实例。
            let latest = self.resolve_current_entries(kind)?;
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
            let latest_plan = AgentAssemblyPlan::for_mode(self.agent_mode(), kind);
            if latest_plan != plan {
                tracing::debug!(
                    requested_extensions = plan.extensions_enabled,
                    "ChatService: Agent 构造期间对话模式变化，丢弃旧实例"
                );
                return Err(AIError::Cancelled);
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
                    blink_infra::data::config::set_config(&pool, "chat:selected_model", &id).await;
            } else {
                let _ =
                    blink_infra::data::config::delete_config(&pool, "chat:selected_model").await;
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

    /// 0.17.9: 设置 Ephemeral 对话（主窗口 AI）的运行时选中模型。
    ///
    /// `Some` = 用户在设置页为主窗口 AI 选了某个 provider+model；
    /// `None` = 恢复 Light 档默认（Light 空则降级 Main）。
    /// 写入后清 cached_agent，下次 prompt 按新选择重建 AgentProvider。
    pub fn select_ephemeral_model(&self, selection: Option<ChatModelSelection>) {
        // 持久化到配置库
        let key = selection
            .as_ref()
            .map(|s| format!("{}:{}", s.provider_id, s.model_id));
        let pool = self.config_pool.clone();
        tokio::spawn(async move {
            if let Some(id) = key {
                let _ = blink_infra::data::config::set_config(
                    &pool,
                    "chat:ephemeral_selected_model",
                    &id,
                )
                .await;
            } else {
                let _ = blink_infra::data::config::delete_config(
                    &pool,
                    "chat:ephemeral_selected_model",
                )
                .await;
            }
        });
        *self
            .ephemeral_selected
            .write()
            .expect("ephemeral_selected lock poisoned") = selection.clone();
        *self
            .ephemeral_model_policy
            .write()
            .expect("ephemeral model policy lock poisoned") = selection
            .as_ref()
            .map(|s| format!("{}:{}", s.provider_id, s.model_id))
            .unwrap_or_else(|| "light".to_string());
        *self
            .cached_agent
            .write()
            .expect("chat agent cache lock poisoned") = None;
        if let Some(sel) = &selection {
            tracing::info!(
                provider = %sel.provider_display_name,
                model = %sel.model_display_name,
                "ChatService: 主窗口 AI 模型切换"
            );
        } else {
            tracing::info!("ChatService: 主窗口 AI 模型恢复 Light 档");
        }
    }

    /// 0.17.9: 当前 Ephemeral 选中的模型快照（供 commands 层 `get_ephemeral_models` 标注 is_selected）。
    pub fn current_ephemeral_selection(&self) -> Option<ChatModelSelection> {
        self.ephemeral_selected
            .read()
            .expect("ephemeral_selected lock poisoned")
            .clone()
    }

    /// 当前主窗口模型策略，供设置与模型标签投影使用。
    pub fn current_ephemeral_model_policy(&self) -> String {
        self.ephemeral_model_policy
            .read()
            .expect("ephemeral model policy lock poisoned")
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

    /// 0.21.17: 返回当前缓存的 AgentProvider 引用（不校验 key）。
    ///
    /// 供 `compress_context_now` 等不需要重建 Agent 的 command 使用——
    /// 从缓存中获取已构造的 AgentProvider，访问其工具快照等只读数据。
    /// 若缓存为空（Agent 未构造），返回 None。
    pub fn cached_agent_ref(&self) -> Option<Arc<AgentProvider>> {
        self.cached_agent
            .read()
            .expect("chat agent cache lock poisoned")
            .as_ref()
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
    #[allow(clippy::too_many_arguments)]
    pub async fn prompt(
        self: &Arc<Self>,
        conversation_id: String,
        message: String,
        attachments: Vec<ChatAttachmentInput>,
        group_system_prompt: Option<String>,
        kind: ConversationKind,
        target_window: String,
        thinking_enabled: bool,
        reasoning_effort: Option<String>,
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

        let plan = AgentAssemblyPlan::for_mode(self.agent_mode(), kind);

        // 0.13.3：构建 skill-aware preamble
        // 1. 检查 /skill 显式激活指令
        // 2. 阶段 1：所有 Skill 摘要常驻
        // 3. 阶段 2：触发匹配（关键词/正则）或显式激活的 Skill 全文注入
        let (visible_message, triggered_skills) = if plan.includes_extensions() {
            self.resolve_skill_triggers(&message)
        } else {
            (message.clone(), Vec::new())
        };

        // 0.27.9：模型输入与用户可见正文分离——附件先逐项校验（无效项不静默
        // 丢弃，逐项报错），再解引用构建本轮快照；预写落库、标题截断与 LLM
        // 命名全部使用用户可见正文（`message`/`visible_message`），持久化历史
        // 中不出现 `rref_` 技术块（历史只落 kind+name 徽标）。
        let (valid_inputs, invalid) = crate::ai::attachment::validate_inputs(attachments);
        if !invalid.is_empty() {
            let details = invalid
                .iter()
                .map(|item| {
                    format!(
                        "第{}项（{}）：{}",
                        item.index + 1,
                        if item.display.is_empty() { "未命名" } else { &item.display },
                        item.reason.as_str()
                    )
                })
                .collect::<Vec<_>>()
                .join("；");
            return Err(ChatError::Attachment { details });
        }
        // 0.27.10 前后端双校验（§3.4）：带图片请求要求当前模型声明 Vision 能力；
        // 失败保留草稿（前端不清输入），提示切换模型或移除图片。检查放在
        // resolve_attachments（逐图解码/归一化，最重的一步）之前——非 Vision
        // 模型不白付解码开销。
        let current_entries = self.resolve_current_entries(kind);
        if valid_inputs
            .iter()
            .any(|a| a.kind == crate::ai::attachment::AttachmentKind::Image)
        {
            let vision_ok = current_entries
                .as_ref()
                .map(|entries| {
                    entries
                        .model
                        .capabilities
                        .contains(&crate::config::ai_config::ModelCapability::Vision)
                })
                .unwrap_or(false);
            if !vision_ok {
                return Err(ChatError::Attachment {
                    details: "当前模型未启用「视觉输入」能力，无法发送图片附件。请在设置中为该模型勾选视觉输入、切换到支持视觉的模型，或移除图片附件"
                        .into(),
                });
            }
        }
        // 0.27.15：本轮思考能力快照——发送时冻结（与 AgentProvider 构造期解析
        // 同一真源），消息级元数据与请求编码共享同一结论。
        let thinking_cap = current_entries
            .as_ref()
            .ok()
            .map(|entries| {
                crate::ai::thinking::resolve_thinking_capability(
                    entries.provider.kind,
                    crate::ai::compat_dialect::resolve_for_entry(&entries.provider),
                    entries.provider.base_url.as_deref(),
                    &entries.model.id,
                    entries.model.thinking_style,
                )
            })
            .unwrap_or_else(ThinkingCapability::unknown);
        let (resolved_attachments, image_parts, pending_image_tokens_est) =
            self.resolve_attachments(&valid_inputs).await?;

        let model_input =
            crate::ai::attachment::compose_model_input(&visible_message, &resolved_attachments);
        if !resolved_attachments.is_empty() {
            tracing::debug!(
                request_id,
                attachments = resolved_attachments.len(),
                "ChatService: 本轮模型输入已注入附件上下文（不进入持久化历史）"
            );
        }
        // 绑定本轮已附加 ref（read_attachment_text 请求级校验数据源）；
        // 任务结束（正常/失败/abort）由 guard 清除。
        let attachment_guard = AttachmentRefsGuard::set_if_any(self, &resolved_attachments);

        // 0.21.16：发出即保存——Persistent 模式预写用户消息 + 建对话记录。
        // 与 `append` 的「跳过已预写 user」去重配合，正常完成后不产生重复行；
        // 中断/失败时用户消息已落库，侧边栏立即可见。
        // 失败不阻塞对话（warn-and-continue），与「持久化分组失败不影响对话」一致。
        // 0.27.9：附件以 kind+name 徽标随预写行落库（ref 不落库，兼容旧纯音频名记录）。
        // 0.27.15：当轮思考意图冻结为消息级元数据（`messages.thinking_state`）——
        // 历史/导出按当轮状态呈现，旧消息缺元数据时前端标「未知」不猜测。
        let badges = crate::ai::attachment::to_badges(&resolved_attachments);
        let thinking_state_json = {
            let state = serde_json::json!({
                "intent": match ThinkingIntent::from_request(
                    thinking_cap.supports_effort(),
                    reasoning_effort.as_deref(),
                    thinking_enabled,
                ) {
                    ThinkingIntent::FollowDefault => "default",
                    ThinkingIntent::Disable => "off",
                    ThinkingIntent::EnableDefault => "on",
                    ThinkingIntent::Effort(ref level) => level,
                },
                "enabled": thinking_enabled,
                "effort": reasoning_effort,
            });
            serde_json::to_string(&state).ok()
        };
        if kind == ConversationKind::Persistent
            && let Err(e) = self
                .persistent_memory()
                .persist_user_message(
                    &conversation_id,
                    &visible_message,
                    &badges,
                    thinking_state_json.as_deref(),
                )
                .await
        {
            tracing::warn!(
                conversation_id = %conversation_id,
                error = %e,
                "ChatService: 预写用户消息失败（不影响对话）"
            );
        }

        let skill_summaries = if plan.includes_extensions() {
            self.skill_registry.summaries()
        } else {
            Vec::new()
        };
        // 0.21.22: 工具来源摘要——仅 extensions 模式注入 preamble
        let tool_sources = if plan.includes_extensions() {
            let mcp_servers = self.mcp_client.tool_source_summary().await;
            // builtin_count 与 build_agent_tools 的过滤同口径（allowlist ∩ 注册表，
            // 0.21.22.1 起走 count_allowlisted_capabilities 单一实现）——直接
            // allowlist.len() 会把已注销的陈旧授权计入，高于实际入池数。
            let ai_allowlist =
                crate::config::ai_capability_access::AiCapabilityAccessStore::enabled_set(
                    &self.config_pool,
                )
                .await;
            crate::ai::prompt::ToolSourceSummary {
                builtin_count: count_allowlisted_capabilities(
                    &self.capability_registry,
                    &ai_allowlist,
                ),
                mcp_servers,
            }
        } else {
            crate::ai::prompt::ToolSourceSummary::default()
        };
        let preamble = if !plan.includes_extensions() {
            pure_chat_system_prompt_with_group(group_system_prompt.as_deref())
        } else {
            chat_system_prompt_with_skills(
                group_system_prompt.as_deref(),
                &skill_summaries,
                &triggered_skills,
                &tool_sources,
            )
        };

        // 0.21.16: 启动前计算摘要字段——preamble 随后 move 进后台 task，
        // 在此先取 token 估算值供「prompt 已启动」日志使用。
        let preamble_tokens = estimate_tokens(&preamble);
        let skill_count = triggered_skills.len();

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
                &target_window,
                EventNames::CHAT_SKILL_ACTIVATED,
                serde_json::to_value(&signal).unwrap_or_default(),
            );
        }

        let history_revision = if kind == ConversationKind::Persistent {
            blink_infra::data::conversations::get_summary_revision(self.persistent_memory.pool(), &conversation_id)
                .await.ok().map(|(_, revision)| revision)
        } else { None };
        let target_window_for_task = target_window.clone();
        let kind_for_task = kind;
        // pending 图片 token 估算（预算用）+ 任务内多模态 parts（move）
        let pending_image_tokens = pending_image_tokens_est;
        let image_parts_for_task = image_parts;
        let task = tokio::spawn(async move {
            // 附件绑定随任务生命周期——正常结束/失败/abort（drop）都清除
            let _attachment_guard = attachment_guard;
            if start_rx.await.is_err() {
                return;
            }
            let Some(service) = weak_service.upgrade() else {
                return;
            };

            // Provider 构造也放进可 abort 的 task：窗口在冷构造期间关闭时仍能立即中断。
            match service
                .ensure_provider(&preamble, kind_for_task, plan)
                .await
            {
                Ok(provider) => {
                    // 0.13.6: 在 stream_prompt 前计算上下文窗口状态并推送前端
                    // 传入 pending message + preamble，因为此时消息尚未写入 DB，
                    // 纯靠 load_with_stats 读 DB 会得到空列表 → token=0（修「永远是 0%」bug）
                    //
                    // 0.21.17: 传入 AgentProvider + ResolvedProviderEntries，
                    // 让预算估算使用真实工具快照、真实 max_tokens 和 calibrator 系数。
                    let resolved = match service.resolve_current_entries(kind_for_task) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!(error = %e, "prompt: resolve_current_entries 失败，跳过 context status");
                            let _ = chunk_tx.send(ChatStreamChunk::Error {
                                message: e.to_string(),
                            });
                            if service.requests.clear_if(request_id) {
                                tracing::debug!(
                                    request_id,
                                    "ChatService: 请求失败后 active 已清除"
                                );
                            }
                            return;
                        }
                    };
                    let context_status = service
                        .compute_context_status(
                            &conversation_for_task,
                            Some(&model_input),
                            Some(&preamble),
                            pending_image_tokens,
                            &provider,
                            &resolved,
                        )
                        .await;
                    let _ = service.emitter.emit_to(
                        &target_window_for_task,
                        EventNames::CHAT_CONTEXT_STATUS,
                        serde_json::to_value(&context_status).unwrap_or_default(),
                    );

                    // 0.12.9：移除时间注入到 user message 末尾——之前将
                    // [当前时间：...] 追加到 user msg 后被 rig memory 持久化到 DB，
                    // 导致：
                    // 1. 切换对话重新加载时用户消息气泡显示时间后缀
                    // 2. 标题生成 LLM 可能会看到时间文本，生成 [当前时间：...] 作为标题
                    //
                    // 时间上下文如未来需要，应通过 non-persistent 机制注入（如
                    // rig agent 的 runtime preamble 或独立 system message），
                    // 不能污染 conversation memory。
                    //
                    // 当前发送干净的用户消息给 agent。0.21.23: 校准采样（P0-1 用
                    // raw_estimated_tokens 未校准基线，避免 ratio 反馈回路）、
                    // effort 400 自愈、Done 分发（Done 后重算 + 摘要触发用 Done 后
                    // 水位）整体下沉 `interceptor::run_intercepted_stream`。
                    interceptor::run_intercepted_stream(
                        &service,
                        &provider,
                        &resolved,
                        &conversation_for_task,
                        &model_input,
                        image_parts_for_task,
                        chunk_tx,
                        thinking_enabled,
                        reasoning_effort.clone(),
                        context_status.raw_estimated_tokens as u32,
                        kind_for_task == ConversationKind::Persistent,
                        request_id,
                    )
                    .await;
                }
                Err(error) => {
                    // 0.12.9：provider 构造失败时记录日志，便于排查配置/密钥问题
                    tracing::warn!(
                        target: blink_infra::utils::perf::ai_slo::TARGET,
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
                target_window: target_window.clone(),
                abort_handle,
                origin: RequestOrigin::Chat,
            })
            .map_err(ChatError::AlreadyActive)?;

        if start_tx.send(()).is_err() {
            self.requests.clear_if(request_id);
            return Err(ChatError::Provider(AIError::Cancelled));
        }

        tracing::info!(
            request_id,
            conversation_id = %conversation_id,
            kind = ?kind,
            target_window = %target_window,
            message_tokens = estimate_tokens(&message),
            message_chars = message.chars().count(),
            preamble_tokens,
            skill_count,
            "ChatService: prompt 已启动"
        );
        Ok(ChatPromptHandle {
            request_id,
            conversation_id,
            chunks: chunk_rx,
            target_window,
            history_revision,
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

    // ── 0.27.9：附件解引用与请求级绑定 ───────────────────────────────────

    /// 解引用并校验附件（§3.3：按 kind 分派；无效/超限逐项报错）。
    ///
    /// - audio：ref 不在本层解引用（由 `transcribe_audio` 工具消费）；
    /// - text_file：`AttachmentText` 用途 open + 有界整读 + UTF-8 探测 +
    ///   token 预算截断，文件身份变化由 store open 的 identity 复核拒绝；
    /// - image：0.27.10 接入，本阶段明确拒绝。
    async fn resolve_attachments(
        &self,
        inputs: &[ChatAttachmentInput],
    ) -> Result<
        (
            Vec<ResolvedAttachment>,
            Vec<crate::ai::agent_provider::VisionImagePart>,
            usize,
        ),
        ChatError,
    > {
        if inputs.is_empty() {
            return Ok((Vec::new(), Vec::new(), 0));
        }
        let store = self.cap_env.resource_store().cloned().ok_or_else(|| {
            ChatError::Attachment {
                details: "运行时未启用资源库，无法处理附件".into(),
            }
        })?;
        let mut resolved: Vec<ResolvedAttachment> = Vec::with_capacity(inputs.len());
        let mut errors: Vec<String> = Vec::new();
        let mut text_injections: Vec<TextInjection> = Vec::new();
        let mut image_parts: Vec<crate::ai::agent_provider::VisionImagePart> = Vec::new();
        let mut normalized_images: Vec<crate::ai::vision::NormalizedImage> = Vec::new();
        let mut pending_image_tokens: usize = 0;
        for (index, input) in inputs.iter().enumerate() {
            match input.kind {
                AttachmentKind::Audio => {
                    resolved.push(ResolvedAttachment::Audio {
                        r#ref: input.r#ref.clone(),
                        display: input.display.clone(),
                    });
                }
                AttachmentKind::TextFile => {
                    match Self::resolve_text_file(&store, input).await {
                        Ok(injection) => {
                            text_injections.push(injection.clone());
                            resolved.push(ResolvedAttachment::TextFile {
                                r#ref: input.r#ref.clone(),
                                display: input.display.clone(),
                                injection,
                            });
                        }
                        Err(reason) => {
                            errors.push(format!("第{}项（{}）：{}", index + 1, input.display, reason));
                        }
                    }
                }
                AttachmentKind::Image => {
                    match Self::resolve_image(&store, input).await {
                        Ok(normalized) => {
                            image_parts.push(crate::ai::agent_provider::VisionImagePart {
                                mime: normalized.mime().to_string(),
                                bytes: normalized.bytes.clone(),
                            });
                            pending_image_tokens += normalized.estimated_tokens;
                            normalized_images.push(normalized);
                            resolved.push(ResolvedAttachment::Image {
                                r#ref: input.r#ref.clone(),
                                display: input.display.clone(),
                            });
                        }
                        Err(reason) => {
                            errors.push(format!("第{}项（{}）：{}", index + 1, input.display, reason.0));
                        }
                    }
                }
            }
        }
        if !errors.is_empty() {
            return Err(ChatError::Attachment {
                details: errors.join("；"),
            });
        }
        let overrun = text_total_overrun(&text_injections);
        if overrun > 0 {
            return Err(ChatError::Attachment {
                details: format!(
                    "文本附件合计超出 {TEXT_TOTAL_TOKEN_LIMIT} tokens 上限（超出约 {overrun}），请移除部分附件或缩小内容"
                ),
            });
        }
        // 图片合计编码总量（base64 展开后 ≤16 MiB，§3.4）
        let refs: Vec<&crate::ai::vision::NormalizedImage> =
            normalized_images.iter().collect();
        if let Err(e) = crate::ai::vision::check_total_encoded(&refs) {
            return Err(ChatError::Attachment { details: e.0 });
        }
        Ok((resolved, image_parts, pending_image_tokens))
    }

    /// 单个图片附件解析：VisionImage 用途 open（身份复核在 store 内）→
    /// 有界读取 → 归一化（CPU 密集，spawn_blocking；§3.4 限制先于大分配）。
    async fn resolve_image(
        store: &Arc<DefaultResourceStore>,
        input: &ChatAttachmentInput,
    ) -> Result<crate::ai::vision::NormalizedImage, crate::ai::vision::VisionError> {
        let mut opened = store
            .open(
                &ResourceRef::from_token(&input.r#ref),
                ResourceUse::VisionImage,
            )
            .map_err(|e| {
                crate::ai::vision::VisionError(format!(
                    "图片引用不可用（{}）——可能已过期或被撤销，请重新附加",
                    e.kind.as_str()
                ))
            })?;
        let bytes = tokio::task::spawn_blocking(move || {
            opened.read_all_bounded(crate::ai::vision::MAX_ENCODED_BYTES_PER_IMAGE)
        })
        .await
        .map_err(|e| crate::ai::vision::VisionError(format!("图片读取任务失败: {e}")))?
        .map_err(|e| {
            crate::ai::vision::VisionError(format!(
                "图片读取失败（{}）——资源可能已变化，请重新附加",
                e.kind.as_str()
            ))
        })?;
        let display_len = input.display.chars().count();
        tokio::task::spawn_blocking(move || {
            let normalized = crate::ai::vision::normalize_image(bytes.to_vec())?;
            tracing::debug!(
                display_len,
                width = normalized.width,
                height = normalized.height,
                bytes = normalized.bytes.len(),
                "vision image normalized"
            );
            Ok(normalized)
        })
        .await
        .map_err(|e| crate::ai::vision::VisionError(format!("图片归一化任务失败: {e}")))?
    }

    /// 单个文本附件解析——store open（身份复核）→ 有界整读 → 探测 → 注入切片。
    async fn resolve_text_file(
        store: &Arc<DefaultResourceStore>,
        input: &ChatAttachmentInput,
    ) -> Result<TextInjection, String> {
        let mut opened = store
            .open(
                &ResourceRef::from_token(&input.r#ref),
                ResourceUse::AttachmentText,
            )
            .map_err(|e| {
                format!(
                    "附件引用不可用（{}）——可能已过期或文件发生变化，请重新附加",
                    e.kind.as_str()
                )
            })?;
        let bytes = tokio::task::spawn_blocking(move || {
            opened.read_all_bounded(TEXT_FILE_MAX_BYTES)
        })
        .await
        .map_err(|e| format!("附件读取任务失败: {e}"))?
        .map_err(|e| {
            format!(
                "附件读取失败（{}）——文件可能已变化，请重新附加",
                e.kind.as_str()
            )
        })?;
        match classify_text_probe(&bytes) {
            TextProbeStatus::Utf8 => {}
            TextProbeStatus::Empty => {
                return Ok(TextInjection {
                    text: String::new(),
                    token_est: 0,
                    truncated: false,
                    original_token_est: 0,
                });
            }
            TextProbeStatus::Binary => {
                return Err("文件包含二进制内容（NUL 字节），不能按文本附加".into());
            }
            TextProbeStatus::NonUtf8 => {
                return Err("文件不是 UTF-8 编码，请先转换为 UTF-8 再附加".into());
            }
        }
        let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(&bytes);
        let text =
            std::str::from_utf8(body).map_err(|_| "文件包含无效 UTF-8 序列".to_string())?;
        Ok(build_text_injection(text))
    }

    /// 当前活跃请求的附件 ref 集合（`CapabilityEnv::active_chat_attachment_refs`
    /// 的数据源；None = 无绑定）。
    pub fn active_attachment_refs(&self) -> Option<Vec<String>> {
        self.attachment_refs
            .read()
            .expect("attachment_refs lock poisoned")
            .clone()
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

    // ── 0.23.4: Editor 整理请求接入全局单活跃协调器（phase 文档 §3.10）──────

    /// 把编辑器整理请求注册进同一全局单活跃协调器（与 prompt 共用 tracker）。
    ///
    /// `start_gate` 串行化"检查 + 安装"，与 `prompt()` 互斥；任一方向冲突都
    /// 返回当前活跃请求状态（前端据 `target_window` 提示"AI 正在 X 中处理"）。
    ///
    /// **取消路径唯一**：editor 请求不挂真实任务 AbortHandle——chat 侧
    /// `abort/abort_active` 对 Editor 来源直接拒绝（见 tracker），取消只能经
    /// `cancel_editor_transform` 或自然完成，由 `EditorTransformService` 全权
    /// 管理任务生命周期并在收尾时调用 [`Self::release_editor_transform`]。
    pub async fn register_editor_transform(&self) -> Result<u64, ActiveChatStatus> {
        let _start_guard = self.start_gate.lock().await;
        if let Some(active) = self.requests.status() {
            return Err(active);
        }
        let request_id = self.requests.next_id();
        // 空转 handle：editor 请求不可被 tracker abort（取消路径唯一）。
        // tokio 未公开 AbortHandle 构造器，用即完成任务的 handle 兜底——
        // 即便被误 abort 也无任务可杀。
        let inert_task = tokio::spawn(async {});
        self.requests
            .install(ActiveChatRequest {
                request_id,
                conversation_id: format!("editor-transform-{request_id}"),
                target_window: "editor".to_string(),
                abort_handle: inert_task.abort_handle(),
                origin: RequestOrigin::Editor,
            })
            .expect("start_gate 已确认槽位为空");
        tracing::info!(
            request_id,
            "ChatService: editor 整理请求已注册（全局单活跃）"
        );
        Ok(request_id)
    }

    /// 释放编辑器整理请求槽位（完成/失败/取消收尾时调用；幂等）。
    pub fn release_editor_transform(&self, request_id: u64) -> bool {
        let released = self.requests.clear_if(request_id);
        if released {
            tracing::debug!(request_id, "ChatService: editor 整理请求槽位已释放");
        }
        released
    }

    /// 0.17.6a: 导出临时对话的全部消息（供 promote 为持久对话用）。
    pub async fn export_ephemeral_messages(
        &self,
        conversation_id: &str,
    ) -> Vec<rig_core::completion::Message> {
        self.ephemeral_memory.export_messages(conversation_id).await
    }

    /// 0.17.6a: 清空临时对话记忆（promote 后清理）。
    pub async fn remove_ephemeral_conversation(&self, conversation_id: &str) {
        self.ephemeral_memory.remove(conversation_id).await;
    }

    /// 历史修改与 prompt 启动共用门闩，禁止生成过程继续写回被截断的历史。
    pub async fn truncate_history(
        &self,
        conversation_id: &str,
        keep_count: i64,
        message_id: Option<i64>,
        ephemeral_turn_from_end: Option<usize>,
    ) -> Result<(), String> {
        let _guard = self.start_gate.lock().await;
        if self.current_request_context().is_some() {
            return Err("已有对话正在生成，请停止后重试".into());
        }
        if let Some(from_end) = ephemeral_turn_from_end {
            self.ephemeral_memory
                .truncate_user_turn(conversation_id, from_end)
                .await?;
        } else if let Some(id) = message_id {
            blink_infra::data::conversations::truncate_messages_from_id(
                self.persistent_memory.pool(),
                conversation_id,
                id,
            )
            .await?;
        } else {
            blink_infra::data::conversations::truncate_messages(
                self.persistent_memory.pool(),
                conversation_id,
                keep_count,
            )
            .await?;
        }
        self.last_context_status.invalidate_conversation(conversation_id);
        Ok(())
    }

    /// 0.17.6a: 获取持久化对话记忆的引用（供 promote 写入消息用）。
    pub fn persistent_memory(&self) -> &Arc<SqliteConversationMemory> {
        &self.persistent_memory
    }

    /// 0.21.18: 设置页 System Prompt Token 估算用：当前 Skill 阶段 1 摘要快照。
    pub fn skill_summaries(&self) -> Vec<SkillSummary> {
        self.skill_registry.summaries()
    }

    /// 当前 active request 上下文；Phase 4 注入 Dangerous confirm payload。
    pub fn current_request_context(&self) -> Option<(u64, String, String)> {
        self.requests.status().map(|active| {
            (
                active.request_id,
                active.conversation_id,
                active.target_window.unwrap_or_default(),
            )
        })
    }

    /// 返回 chat 状态快照。
    ///
    /// 0.12.2：`provider_name`/`model_name` 反映当前生效模型（selected 优先，Main 回落），
    /// 供前端 header 标签展示。
    /// 0.21.20：`provider_configured` 改为基于 `resolve_current_entries` 而非裸 `Tier::Main`。
    /// 此前仅查 Main 档，用户在模型选择器选了模型但 Main 档未配置时，`provider_configured=false`
    /// 导致前端显示"请先在设置中配置供应商"空状态，与左下角已选模型矛盾。
    pub fn status(&self) -> ChatStatus {
        // 0.17.9: status 供对话窗口调用，走 Persistent 路径
        let resolved = self
            .resolve_current_entries(ConversationKind::Persistent)
            .ok();
        let (provider_name, model_name, thinking_capability, reasoning_effort) = match &resolved {
            Some(r) => {
                let model_display = if r.model.display_name.is_empty() {
                    r.model.id.clone()
                } else {
                    r.model.display_name.clone()
                };
                // 0.27.15：能力快照单一解析——旧 supports_effort 字段由其投影
                let cap = crate::ai::thinking::resolve_thinking_capability(
                    r.provider.kind,
                    crate::ai::compat_dialect::resolve_for_entry(&r.provider),
                    r.provider.base_url.as_deref(),
                    &r.model.id,
                    r.model.thinking_style,
                );
                (
                    Some(r.provider.display_name.clone()),
                    Some(model_display),
                    Some(cap),
                    r.model.reasoning_effort.clone(),
                )
            }
            None => (None, None, None, None),
        };
        ChatStatus {
            active: self.requests.status(),
            provider_configured: resolved.is_some(),
            provider_name,
            model_name,
            supports_effort_levels: thinking_capability
                .as_ref()
                .is_some_and(|c| c.supports_effort()),
            reasoning_effort,
            thinking_capability,
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
        // 0.17.9: ephemeral_selected 失效校验
        let ephemeral_selected = self
            .ephemeral_selected
            .read()
            .expect("ephemeral_selected lock poisoned")
            .clone();
        if let Some(sel) = ephemeral_selected {
            let still_valid = self
                .ai_registry
                .validate_model_exists(&sel.provider_id, &sel.model_id)
                .is_some();
            if !still_valid {
                tracing::warn!(
                    provider_id = %sel.provider_id,
                    model_id = %sel.model_id,
                    "ChatService: Ephemeral 选中的模型配置后已不可用，清除选择回落 Light 档"
                );
                *self
                    .ephemeral_selected
                    .write()
                    .expect("ephemeral_selected lock poisoned") = None;
            }
        }
        tracing::debug!("ChatService: 配置变化，AgentProvider 缓存已失效");
    }

    /// 热更新对话能力模式与主窗口模型策略。
    pub fn update_chat_config(&self, config: &ChatConfig) {
        let mode = config.effective_agent_mode();
        let encoded = encode_agent_mode(mode);
        let previous = self.agent_mode.swap(encoded, Ordering::AcqRel);
        let mut policy = self
            .ephemeral_model_policy
            .write()
            .expect("ephemeral model policy lock poisoned");
        let policy_changed = *policy != config.main_window_model;
        if policy_changed {
            *policy = config.main_window_model.clone();
            *self
                .ephemeral_selected
                .write()
                .expect("ephemeral_selected lock poisoned") = None;
        }
        drop(policy);
        if previous != encoded || policy_changed {
            *self
                .cached_agent
                .write()
                .expect("chat agent cache lock poisoned") = None;
            tracing::info!(?mode, main_window_model = %config.main_window_model, "ChatService: 对话配置已更新");
        }
    }

    fn agent_mode(&self) -> ChatAgentMode {
        decode_agent_mode(self.agent_mode.load(Ordering::Acquire))
    }

    /// 两类窗口是否都处于纯对话。供 MCP 预热与持久对话投影守门。
    pub fn is_pure_chat_mode(&self) -> bool {
        self.agent_mode() == ChatAgentMode::PureChat
    }

    /// 应用用户从设置页修改的记忆策略配置（0.13.1 §3.7）。
    ///
    /// 在 `set_config('ai_config')` 命令处理中调用——保存 DB 后、`notify_config_changed`
    /// 之前。保留运行时注入的 `context_limit`（来自 `ModelEntry.context_window`），
    /// 只更新 `mode / window_size / trigger_ratio / compress_ratio`。
    pub async fn update_memory_config(&self, config: crate::ai::memory::MemoryConfig) {
        self.persistent_memory.apply_config(config).await;
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
    ) -> (String, Vec<crate::ai::skill::SkillEntry>) {
        use crate::ai::skill::SkillEntry;
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

    /// 应用 Skill 运行时配置。关闭时立即旁路并清空；开启时在门闩关闭期间完成重扫，
    /// 然后按当前单 Skill 禁用清单恢复可见状态。
    pub fn apply_skill_config(&self, config: &crate::config::ai_config::SkillConfig) {
        self.skill_registry.set_enabled(false);
        self.skill_registry
            .set_disabled_skills(config.disabled_skills.clone());
        if !config.enabled {
            self.skill_registry.clear();
            tracing::info!("ChatService: Skill 运行时已关闭并清空");
            return;
        }

        self.skill_registry.scan(&config.enabled_sources());
        self.skill_registry.set_enabled(true);
        tracing::info!(
            count = self.skill_registry.count(),
            disabled = config.disabled_skills.len(),
            "ChatService: Skill 运行时已重载"
        );
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

/// 获取当前 active request 上下文（request_id, conversation_id）。
///
/// 供 `tool_adapter` 在 emit dangerous confirm 时注入，前端按 request_id 校验事件归属。
/// ChatService 未注册时返回 `(0, String::new())`——confirm 仍可工作，只是前端无法校验归属。
pub fn current_request_context(chat: Option<&Arc<ChatService>>) -> (u64, String, String) {
    chat.and_then(|cs| cs.current_request_context())
        .unwrap_or((0, String::new(), String::new()))
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

/// 0.21.5: 计算 AI capability allowlist 的稳定 fingerprint。
///
/// 空集合返回 0（纯对话模式）；非空集合对 sorted ids 做 hash。
/// allowlist 变化时 fingerprint 不同，触发 Agent cache miss → 重建。
///
/// 0.28.1：`registry` 传入时按 **allowlist ∩ 注册表实际条目** 计算——
/// 授权了但已注销（插件撤销）的 id 不计入；插件 tool 注册/撤销会改变
/// 交集，触发重建。语义与 `count_allowlisted_capabilities` 的口径一致。
fn compute_allowlist_fingerprint(
    allowlist: &std::collections::HashSet<String>,
    registry: Option<&CapabilityRegistry>,
) -> u64 {
    if allowlist.is_empty() {
        return 0;
    }
    let effective: Vec<&str> = match registry {
        Some(reg) => {
            let entries = reg.entries();
            let registered: std::collections::HashSet<&str> =
                entries.iter().map(|(id, _)| id.as_str()).collect();
            let mut ids: Vec<&str> = allowlist
                .iter()
                .map(|id| id.as_str())
                .filter(|id| registered.contains(id))
                .collect();
            ids.sort();
            ids
        }
        None => {
            let mut ids: Vec<&str> = allowlist.iter().map(|id| id.as_str()).collect();
            ids.sort();
            ids
        }
    };
    let mut hasher = DefaultHasher::new();
    for id in effective {
        id.hash(&mut hasher);
    }
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    // 0.27.9：附件校验/组装纯函数测试已随实现迁至 `ai::attachment` 模块
    //（compose_model_input / validate_inputs / 探测与截断管道）。

    #[test]
    fn agent_mode_scopes_extensions_by_window() {
        assert!(
            AgentAssemblyPlan::for_mode(ChatAgentMode::Full, ConversationKind::Ephemeral)
                .includes_extensions()
        );
        assert!(
            AgentAssemblyPlan::for_mode(ChatAgentMode::MainChatOnly, ConversationKind::Persistent,)
                .includes_extensions()
        );
        assert!(
            !AgentAssemblyPlan::for_mode(ChatAgentMode::MainChatOnly, ConversationKind::Ephemeral,)
                .includes_extensions()
        );
        assert!(
            !AgentAssemblyPlan::for_mode(ChatAgentMode::PureChat, ConversationKind::Persistent)
                .includes_extensions()
        );
    }

    #[test]
    fn agent_cache_key_separates_pure_chat_mode() {
        let base = AgentCacheKey {
            provider_id: "p".into(),
            model_id: "m".into(),
            fingerprint: "f".into(),
            preamble_hash: 1,
            mcp_epoch: 0,
            kind: ConversationKind::Persistent,
            extensions_enabled: true,
            ai_capability_epoch: 0,
        };
        assert_ne!(
            base,
            AgentCacheKey {
                extensions_enabled: false,
                ..base.clone()
            }
        );
    }

    // ── 0.21.5: ai_capability_epoch 单测 ─────────────────────────────────

    #[test]
    fn agent_cache_key_separates_by_ai_capability_epoch() {
        let base = AgentCacheKey {
            provider_id: "p".into(),
            model_id: "m".into(),
            fingerprint: "f".into(),
            preamble_hash: 1,
            mcp_epoch: 0,
            kind: ConversationKind::Persistent,
            extensions_enabled: true,
            ai_capability_epoch: 100,
        };
        // 不同 epoch 应导致 cache miss
        assert_ne!(
            base,
            AgentCacheKey {
                ai_capability_epoch: 200,
                ..base.clone()
            }
        );
    }

    #[test]
    fn fingerprint_empty_allowlist_is_zero() {
        let set = std::collections::HashSet::new();
        assert_eq!(compute_allowlist_fingerprint(&set, None), 0);
    }

    /// 0.28.1：allowlist 项在注册表注销后不计入 fingerprint——
    /// 插件 tool 撤销必须让陈旧 Agent 缓存失效。
    #[test]
    fn fingerprint_drops_unregistered_allowlist_ids() {
        let reg = CapabilityRegistry::default();
        struct Cap1;
        #[async_trait::async_trait]
        impl crate::capability::Capability for Cap1 {
            fn id(&self) -> &str {
                "alive_cap"
            }
            fn schema(&self) -> crate::capability::CapabilitySchema {
                crate::capability::CapabilitySchema::default()
            }
            async fn invoke(
                &self,
                _args: serde_json::Value,
                _ctx: &crate::capability::InvokeContext<'_>,
            ) -> Result<crate::capability::CapabilityResult, crate::capability::CapabilityError>
            {
                unreachable!()
            }
        }
        reg.register(Arc::new(Cap1)).unwrap();

        let mut set = std::collections::HashSet::new();
        set.insert("alive_cap".to_string());
        set.insert("revoked_plugin_tool".to_string());
        // 注册表只剩 alive_cap → 交集指纹 == 只含 alive_cap 的指纹
        let mut only_alive = std::collections::HashSet::new();
        only_alive.insert("alive_cap".to_string());
        assert_eq!(
            compute_allowlist_fingerprint(&set, Some(&reg)),
            compute_allowlist_fingerprint(&only_alive, None)
        );
    }

    #[test]
    fn fingerprint_non_empty_allowlist_is_nonzero() {
        let mut set = std::collections::HashSet::new();
        set.insert("open_url".to_string());
        set.insert("search_apps".to_string());
        assert_ne!(compute_allowlist_fingerprint(&set, None), 0);
    }

    #[test]
    fn fingerprint_is_order_independent() {
        // HashSet 本身无序，但 fingerprint 应对同一集合稳定
        let mut set1 = std::collections::HashSet::new();
        set1.insert("cap_a".into());
        set1.insert("cap_b".into());
        set1.insert("cap_c".into());

        let mut set2 = std::collections::HashSet::new();
        set2.insert("cap_c".into());
        set2.insert("cap_a".into());
        set2.insert("cap_b".into());

        assert_eq!(
            compute_allowlist_fingerprint(&set1, None),
            compute_allowlist_fingerprint(&set2, None)
        );
    }

    #[test]
    fn fingerprint_changes_when_allowlist_changes() {
        let mut set1 = std::collections::HashSet::new();
        set1.insert("cap_a".into());
        set1.insert("cap_b".into());

        let mut set2 = std::collections::HashSet::new();
        set2.insert("cap_a".into());
        set2.insert("cap_b".into());
        set2.insert("cap_c".into()); // 新增一个

        assert_ne!(
            compute_allowlist_fingerprint(&set1, None),
            compute_allowlist_fingerprint(&set2, None)
        );
    }

    #[test]
    fn fingerprint_changes_when_capability_removed() {
        let mut set1 = std::collections::HashSet::new();
        set1.insert("cap_a".into());
        set1.insert("cap_b".into());

        let mut set2 = std::collections::HashSet::new();
        set2.insert("cap_a".into());
        // 移除了 cap_b

        assert_ne!(
            compute_allowlist_fingerprint(&set1, None),
            compute_allowlist_fingerprint(&set2, None)
        );
    }

    fn pending_request(request_id: u64, conversation_id: &str) -> ActiveChatRequest {
        let task = tokio::spawn(std::future::pending::<()>());
        let abort_handle = task.abort_handle();
        drop(task);
        ActiveChatRequest {
            request_id,
            conversation_id: conversation_id.to_string(),
            target_window: "chat".to_string(),
            abort_handle,
            origin: RequestOrigin::Chat,
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
                target_window: "chat".to_string(),
                abort_handle,
                origin: RequestOrigin::Chat,
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
            target_window: Some("chat".to_string()),
        });
        assert!(error.to_string().contains("42"));
    }

    // ── 0.23.4: Editor 整理请求接入全局单活跃 ─────────────────────────────

    fn editor_request(request_id: u64) -> ActiveChatRequest {
        let inert = tokio::spawn(async {});
        ActiveChatRequest {
            request_id,
            conversation_id: format!("editor-transform-{request_id}"),
            target_window: "editor".to_string(),
            abort_handle: inert.abort_handle(),
            origin: RequestOrigin::Editor,
        }
    }

    #[tokio::test]
    async fn editor_request_conflicts_with_chat_request_both_ways() {
        // chat 在先：editor 注册被拒并看到 chat 活跃状态
        let tracker = RequestTracker::new();
        tracker.install(pending_request(1, "c1")).unwrap();
        let err = tracker.install(editor_request(2)).unwrap_err();
        assert_eq!(err.target_window.as_deref(), Some("chat"));

        // editor 在先：chat 安装被拒并看到 editor 活跃状态
        let tracker = RequestTracker::new();
        tracker.install(editor_request(1)).unwrap();
        let err = tracker.install(pending_request(2, "c2")).unwrap_err();
        assert_eq!(err.target_window.as_deref(), Some("editor"));
        assert_eq!(err.conversation_id, "editor-transform-1");
        tracker.clear_if(1);
    }

    #[tokio::test]
    async fn editor_request_cannot_be_aborted_via_tracker() {
        let tracker = RequestTracker::new();
        tracker.install(editor_request(5)).unwrap();

        // 指定 id 与 abort_active 都拒绝 Editor 来源，槽位保持占用
        assert!(!tracker.abort(5));
        assert!(!tracker.abort_active());
        assert_eq!(tracker.status().unwrap().request_id, 5);

        // 只有释放路径（编辑器自身取消/完成收尾）能清槽
        assert!(tracker.clear_if(5));
        assert!(tracker.status().is_none());
    }

    #[tokio::test]
    async fn chat_abort_still_works_after_editor_release() {
        let tracker = RequestTracker::new();
        tracker.install(editor_request(1)).unwrap();
        assert!(tracker.clear_if(1));
        tracker.install(pending_request(2, "c2")).unwrap();
        assert!(tracker.abort_active());
        assert!(tracker.status().is_none());
    }
}
