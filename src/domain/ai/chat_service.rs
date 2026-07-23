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

use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};
use tokio::sync::{mpsc, oneshot};
use tokio::task::AbortHandle;

use crate::app::ai_config::Tier;
use crate::domain::ai::agent_provider::{AgentProvider, ChatStreamChunk};
use crate::domain::ai::prompt::chat_system_prompt;
use crate::domain::ai::provider::AIError;
use crate::domain::ai::registry::AIProviderRegistry;
use crate::domain::ai::tool_adapter::{PendingConfirms, build_agent_tools};
use crate::domain::capability::CapabilityRegistry;
use crate::domain::execution::ActionRegistry;

type AgentCacheKey = (String, String, String);

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

/// ChatService 状态快照。
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct ChatStatus {
    pub active: Option<ActiveChatStatus>,
    pub provider_configured: bool,
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
#[derive(Debug)]
pub enum ChatError {
    AlreadyActive(ActiveChatStatus),
    Provider(AIError),
}

impl std::fmt::Display for ChatError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyActive(active) => write!(
                f,
                "已有对话请求正在生成（request_id={}）",
                active.request_id
            ),
            Self::Provider(error) => write!(f, "{error}"),
        }
    }
}

impl std::error::Error for ChatError {}

impl From<AIError> for ChatError {
    fn from(value: AIError) -> Self {
        Self::Provider(value)
    }
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
    app: tauri::AppHandle,
    ai_registry: Arc<AIProviderRegistry>,
    capability_registry: Arc<CapabilityRegistry>,
    action_registry: Arc<ActionRegistry>,
    pending_confirms: Arc<PendingConfirms>,
    memory: Arc<dyn ConversationMemory>,
    cached_agent: RwLock<Option<CachedAgent>>,
    requests: RequestTracker,
    /// 串行化 prompt 启动过程，防止两个并发 IPC 同时通过 active 检查。
    start_gate: tokio::sync::Mutex<()>,
}

impl ChatService {
    /// 构造 ChatService。AgentProvider 首次 prompt 时才懒构造，不增加启动路径耗时。
    pub fn new(
        app: tauri::AppHandle,
        ai_registry: Arc<AIProviderRegistry>,
        capability_registry: Arc<CapabilityRegistry>,
        action_registry: Arc<ActionRegistry>,
        pending_confirms: Arc<PendingConfirms>,
    ) -> Self {
        Self {
            app,
            ai_registry,
            capability_registry,
            action_registry,
            pending_confirms,
            memory: Arc::new(InMemoryConversationMemory::new()),
            cached_agent: RwLock::new(None),
            requests: RequestTracker::new(),
            start_gate: tokio::sync::Mutex::new(()),
        }
    }

    /// 返回当前 Main 档对应的 AgentProvider；配置未变时复用，变化时锁外重建。
    ///
    /// 两次解析用于防止构造期间设置被修改：若 key 已变化，丢弃刚构造的旧实例并重试。
    /// 最多重试 `MAX_PROVIDER_RETRY` 次，防止极端情况下无限循环。
    pub(crate) fn ensure_provider(&self) -> Result<Arc<AgentProvider>, AIError> {
        const MAX_PROVIDER_RETRY: usize = 3;
        let mut retry_count = 0;
        loop {
            if retry_count >= MAX_PROVIDER_RETRY {
                tracing::warn!("ChatService: ensure_provider 达到最大重试次数，放弃");
                return Err(AIError::Cancelled);
            }
            retry_count += 1;
            let resolved = self.ai_registry.resolve_entries(Tier::Main)?;

            if let Some(provider) = self.cached_provider(&resolved.cache_key) {
                return Ok(provider);
            }

            // Client/Agent 构造可能读取 Credential Manager，必须在 ChatService 锁外进行。
            let tools = build_agent_tools(
                &self.capability_registry,
                &self.action_registry,
                &self.app,
                self.pending_confirms.clone(),
            );
            let preamble = chat_system_prompt();
            let provider = Arc::new(AgentProvider::new(
                &resolved.provider,
                &resolved.model,
                tools,
                &preamble,
                self.memory.clone(),
            )?);

            // 构造期间配置可能已更新。只提交仍对应当前 key 的实例。
            let latest = self.ai_registry.resolve_entries(Tier::Main)?;
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
                key: resolved.cache_key,
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
    pub async fn prompt(
        self: &Arc<Self>,
        conversation_id: String,
        message: String,
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

        let task = tokio::spawn(async move {
            if start_rx.await.is_err() {
                return;
            }
            let Some(service) = weak_service.upgrade() else {
                return;
            };

            // Provider 构造也放进可 abort 的 task：窗口在冷构造期间关闭时仍能立即中断。
            match service.ensure_provider() {
                Ok(provider) => {
                    provider
                        .stream_prompt(&conversation_for_task, &message, chunk_tx)
                        .await;
                }
                Err(error) => {
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

    /// 返回 chat 状态快照。provider_configured 只检查 Main 档引用，不触发 Agent 构造。
    pub fn status(&self) -> ChatStatus {
        ChatStatus {
            active: self.requests.status(),
            provider_configured: self.ai_registry.resolve_entries(Tier::Main).is_ok(),
        }
    }

    /// 配置变更后主动失效 Agent 缓存；memory 仍由 ChatService 持有。
    pub fn notify_config_changed(&self) {
        *self
            .cached_agent
            .write()
            .expect("chat agent cache lock poisoned") = None;
        tracing::debug!("ChatService: 配置变化，AgentProvider 缓存已失效");
    }
}

/// 从 Tauri state 获取当前 active request 上下文（request_id, conversation_id）。
///
/// 供 `tool_adapter` 在 emit dangerous confirm 时注入，前端按 request_id 校验事件归属。
/// ChatService 未注册时返回 `(0, String::new())`——confirm 仍可工作，只是前端无法校验归属。
pub fn current_request_context_from_app(app: &tauri::AppHandle) -> (u64, String) {
    use tauri::Manager;
    if let Some(cs) = app.try_state::<Arc<ChatService>>() {
        cs.current_request_context().unwrap_or((0, String::new()))
    } else {
        (0, String::new())
    }
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
