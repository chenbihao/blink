//! Tool 适配层（0.12.0 §2.4）--把 Capability 包装成 `rig::tool::Tool`。
//!
//! ## 动机
//!
//! rig Agent 的 `AgentBuilder` 注册 tool 的方式与现有架构根本不同：
//! - **现有架构**：数据 + 外部调度--`Vec<ToolSchema>` -> 投影成 `Vec<ToolDefinition>`
//!   作为 `CompletionRequest` 参数，service.rs 收到 `tool_calls` 后手工执行
//! - **rig Agent**：代码 + 内部调度--`impl rig::tool::Tool` trait，Agent 内部自动循环
//!
//! 冲突：现有的 Capability / 插件 tool 全部接不进 rig Agent，0.12 对话窗口整个走不通。
//!
//! ## 解决方案
//!
//! 抽出 `ToolAdapter` 层，把 Capability 包装成 `impl rig::tool::Tool`：
//! - `CapabilityTool` 包装 `Arc<dyn Capability>`
//! - `ToolDyn` 动态分发--Args 用 `serde_json::Value`，避免给每个 Capability 写强类型 Args
//!
//! ## 0.14.2 边界钉死
//!
//! **删除 `ActionTool`**——AI tool 池只含 `CapabilityTool`。这把"AI 该不该调这个能力"
//! 的决策从"运行时每个 Action 自己标 danger_class"前置成"编译期只有 Capability 才能进
//! tool 池"。9 个保留 Action（lock/shutdown/restart/sleep/clear_history/exit_blink/
//! open_logs/open_data_dir/open_settings）不再出现在 AI tool 池。
//!
//! `open_url` / `open_path` / `reveal_in_explorer` 从 Action 提升为 Capability，
//! AI 通过 CapabilityTool 调用。0.21.7 起 execution 模块已删除，全量经 CapabilityRegistry。
//!
//! ## 四域墙（危险操作确认 + 闭环）
//!
//! **危险操作**（`danger_class == Dangerous` 或 `schema.sensitive == true`）不直接执行，
//! 也不返回"假消息"让 AI 误以为已执行（那样 AI 会基于错误假设继续生成）。
//! **对话窗口 rig agent loop 是无限轮**，正确的闭环是 `call` 内部 emit 确认事件后
//! **挂起 await 用户确认信号**：
//! - 用户确认 -> 继续执行，返回真实结果
//! - 用户拒绝 -> 返回"用户拒绝"消息，AI 可换路径
//! - 超时（60s）-> 返回"超时未执行"消息，不卡死 agent loop
//!
//! 信号回传：0.12.1 对话窗口前端监听 `blink://chat-confirm-action` 事件 -> 弹确认 UI ->
//! 调 `confirm_chat_action` command -> `PendingConfirms::resolve` 唤醒挂起的 `call`。
//!
//! **事件名 `blink://chat-confirm-action`** 统一用于对话窗口和主窗口 AI 模式--
//! 0.17.6 起，主窗口 AI 也走 ChatService，按 `conversation_id` 过滤区分。
//!
//! ## 工厂函数
//!
//! `build_agent_tools()` 从 `CapabilityRegistry` 收集所有可用能力，
//! 返回 `Vec<Box<dyn ToolDyn>>` 供对话窗口 Agent 使用。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rig_core::tool::ToolDyn;
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::Value;
use tokio::sync::{Mutex, RwLock, oneshot};

use crate::domain::capability::{Capability, CapabilityError, CapabilityRegistry, InvokeContext};
use crate::domain::config::ai_config::get_ai_config;
use crate::domain::config::shards::AiPermissionConfig;
use crate::domain::event::EventPort;
use crate::domain::event_names::EventNames;

// ── 常量 ─────────────────────────────────────────────────────────────────────

/// 危险操作确认超时（秒）--超时返回"未执行"，不卡死 agent loop。
///
/// 60s 给用户足够时间审视危险操作（如关机/清空历史）；超时即放弃，AI 收到超时消息可换路径。
const DANGEROUS_CONFIRM_TIMEOUT_SECS: u64 = 60;

// ── 危险确认闭环骨架（0.12.0 §2.4）────────────────────────────────────────────

/// 待确认的危险操作注册表--对话窗口 rig agent loop 的危险确认闭环核心。
///
/// `call` 挂起 await 期间，confirm_id -> oneshot Sender 存于此。用户确认/拒绝信号
/// 经 `confirm_chat_action` command 调 `resolve` 送回，唤醒挂起的 `call`。
///
/// **confirm_id**：`AtomicU64` 全局递增，不引入 uuid 依赖。
/// **不持久化**：进程重启即丢（pending 确认本就是瞬时状态，重启后 AI 重新发起即可）。
///
/// **双层 trusted 设计**（0.17.8）：
/// - 会话级 `HashSet<(conversation_id, tool_name)>`（进程内，重启即失）
/// - 持久化 DB 层（`config_pool` -> `ai_permission_memory` 表，跨会话保留）
/// - `is_trusted` 检查顺序：会话级命中 -> 跳过 DB；未命中 -> 查 DB -> 命中且未过期 -> 加入会话级
/// - `trust()` 同时写会话级 + DB（若 `memory_enabled`）
/// - `config_pool = None` 时降级为纯会话级（测试环境）
pub struct PendingConfirms {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
    /// 会话级信任列表——`(conversation_id, tool_name)`。
    trusted: Mutex<HashSet<(String, String)>>,
    /// 持久化配置库连接池（0.17.8：跨会话权限记忆）。
    /// `None` = 未启用持久化（测试环境），`Some` = 生产环境。
    config_pool: Option<sqlx::SqlitePool>,
    /// 权限记忆配置（0.17.8）。运行时可更新（用户在设置页改配置后同步）。
    memory_config: Arc<RwLock<AiPermissionConfig>>,
}

impl Default for PendingConfirms {
    fn default() -> Self {
        Self {
            next_id: AtomicU64::new(0),
            pending: Mutex::new(HashMap::new()),
            trusted: Mutex::new(HashSet::new()),
            config_pool: None,
            memory_config: Arc::new(RwLock::new(AiPermissionConfig::default())),
        }
    }
}

impl PendingConfirms {
    /// 构造空注册表（测试用，无 DB 持久化）。
    #[allow(dead_code)] // 测试专用，生产用 with_persistence
    pub fn new() -> Self {
        Self::default()
    }

    /// 带持久化的构造（生产用，0.17.8）。
    ///
    /// 传入配置库连接池 + 权限记忆配置，启用跨会话权限记忆。
    pub fn with_persistence(
        config_pool: sqlx::SqlitePool,
        memory_config: AiPermissionConfig,
    ) -> Self {
        Self {
            config_pool: Some(config_pool),
            memory_config: Arc::new(RwLock::new(memory_config)),
            ..Self::default()
        }
    }

    /// 更新权限记忆配置（用户在设置页改配置后调，0.17.8）。
    pub async fn update_memory_config(&self, config: AiPermissionConfig) {
        let mut guard = self.memory_config.write().await;
        *guard = config;
    }

    /// 注册一个待确认项，返回 `(confirm_id, receiver)`。
    ///
    /// receiver 收到：
    /// - `Ok(true)` -> 用户确认执行
    /// - `Ok(false)` -> 用户拒绝
    /// - `Err(_)` -> sender 被 drop（超时清理 / 注册表丢弃）
    #[allow(dead_code)] // 0.12.1 对话窗口 tool call 消费（经 build_agent_tools 间接调用）
    async fn register(&self) -> (u64, oneshot::Receiver<bool>) {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(id, tx);
        (id, rx)
    }

    /// 用户确认/拒绝（`confirm_chat_action` command 调）。
    ///
    /// 返回 `true` = 信号已送达；`false` = confirm_id 不存在（超时已清理 / 编号过期）。
    pub async fn resolve(&self, confirm_id: u64, approved: bool) -> bool {
        if let Some(tx) = self.pending.lock().await.remove(&confirm_id) {
            let _ = tx.send(approved);
            true
        } else {
            false
        }
    }

    /// 超时清理--await 超时后调，移除并 drop sender（receiver 收 `Err`）。
    async fn discard(&self, confirm_id: u64) {
        self.pending.lock().await.remove(&confirm_id);
    }

    /// 检查指定对话 + tool 是否已获用户信任。
    ///
    /// **双层 trusted 检查**（0.17.8）：
    /// 1. 会话级 HashSet 命中 -> 直接返回 true（跳过 DB 查询，热路径快）
    /// 2. 会话级未命中 -> 查持久化 DB（若 `memory_enabled`）
    /// 3. DB 命中且未过期 -> 视为 trusted + 加入会话级 HashSet（本次会话不再查 DB）
    /// 4. DB 未命中或已过期 -> 返回 false，触发确认弹窗
    pub async fn is_trusted(&self, conversation_id: &str, tool_name: &str) -> bool {
        // 1. 会话级 HashSet 命中 -> 快速返回
        if self
            .trusted
            .lock()
            .await
            .contains(&(conversation_id.to_string(), tool_name.to_string()))
        {
            return true;
        }

        // 2. 查持久化 DB（若 memory_enabled 且有 config_pool）
        let config = self.memory_config.read().await.clone();
        if !config.memory_enabled {
            return false;
        }
        if let Some(ref pool) = self.config_pool
            && crate::infra::data::permission_memory::is_tool_trusted(pool, tool_name).await
        {
            // 3. DB 命中且未过期 -> 加入会话级（本次会话不再查 DB）
            self.trusted
                .lock()
                .await
                .insert((conversation_id.to_string(), tool_name.to_string()));
            tracing::debug!(
                %tool_name,
                conversation_id = %conversation_id,
                "权限记忆命中持久化层，加入会话级"
            );
            return true;
        }
        false
    }

    /// 将指定对话 + tool 加入信任列表（用户确认后调）。
    ///
    /// **双层写入**（0.17.8）：同时写会话级 HashSet + 持久化 DB（若 `memory_enabled`）。
    async fn trust(&self, conversation_id: &str, tool_name: &str) {
        // 写会话级
        self.trusted
            .lock()
            .await
            .insert((conversation_id.to_string(), tool_name.to_string()));

        // 写 DB（若 memory_enabled 且有 config_pool）
        let config = self.memory_config.read().await.clone();
        if config.memory_enabled
            && let Some(ref pool) = self.config_pool
        {
            crate::infra::data::permission_memory::trust_tool(pool, tool_name, config.memory_days)
                .await;
        }
    }

    /// 清除指定对话的所有信任记录（对话删除时调）。
    ///
    /// **只清会话级**——持久化记忆跨会话保留，不受对话删除影响。
    pub async fn clear_trust(&self, conversation_id: &str) {
        let mut trusted = self.trusted.lock().await;
        trusted.retain(|(conv_id, _)| conv_id != conversation_id);
    }

    /// 清空所有持久化权限记忆（设置页"清除所有记忆"按钮调，0.17.8）。
    ///
    /// 只清 DB 持久化层，不影响会话级 `HashSet`。
    pub async fn clear_all_trusted_db(&self) {
        if let Some(ref pool) = self.config_pool {
            crate::infra::data::permission_memory::clear_all_trusted(pool).await;
        }
    }

    /// 清理指定插件的持久化权限记忆（插件禁用时调，0.17.8）。
    ///
    /// 按 `plugin_{id}:%` 前缀匹配 tool_name。
    pub async fn clear_plugin_trusted_db(&self, plugin_prefix: &str) {
        if let Some(ref pool) = self.config_pool {
            crate::infra::data::permission_memory::clear_plugin_trusted(pool, plugin_prefix).await;
        }
    }
}

/// 危险确认的结果。
enum ConfirmOutcome {
    /// 用户确认 -> 继续执行。
    Approved,
    /// 用户拒绝 -> 返回拒绝消息。
    Rejected,
    /// 超时 -> 返回超时消息。
    Timeout,
    /// 信号通道异常（sender 提前 drop）。
    Dropped,
}

/// 挂起等待用户确认危险操作（CapabilityTool 用）。
///
/// 生成 confirm_id -> emit `chat-confirm-action` 事件（含 confirm_id）->
/// `tokio::time::timeout` 等 receiver，超时 `DANGEROUS_CONFIRM_TIMEOUT_SECS` 秒。
///
/// **需 AppHandle emit**--遵循 AGENTS.md §7"Tauri 集成层免自动化"，此函数本身不单测，
/// 闭环的正确性靠 `PendingConfirms` 的 register/resolve/discard 纯逻辑单测保证。
#[allow(clippy::too_many_arguments)]
async fn await_dangerous_confirm(
    pending: &PendingConfirms,
    emitter: &dyn EventPort,
    confirm_id: u64,
    rx: oneshot::Receiver<bool>,
    tool_name: &str,
    tool_type: &'static str,
    arguments: &Value,
    request_id: u64,
    conversation_id: &str,
    target_window: &str,
) -> ConfirmOutcome {
    emit_dangerous_confirm(
        emitter,
        confirm_id,
        tool_name,
        tool_type,
        arguments,
        request_id,
        conversation_id,
        target_window,
    );
    // 挂起等用户确认信号（confirm_chat_action command -> resolve -> rx 收到）
    match tokio::time::timeout(Duration::from_secs(DANGEROUS_CONFIRM_TIMEOUT_SECS), rx).await {
        Ok(Ok(true)) => ConfirmOutcome::Approved,
        Ok(Ok(false)) => ConfirmOutcome::Rejected,
        Ok(Err(_)) => ConfirmOutcome::Dropped,
        Err(_) => {
            pending.discard(confirm_id).await;
            ConfirmOutcome::Timeout
        }
    }
}

/// 危险操作确认+执行的公共逻辑（CapabilityTool 用）。
///
/// 如果 `is_dangerous` 为 true：注册确认 -> emit 事件 -> 挂起等待 ->
/// - Approved: 继续执行（返回 None）
/// - Rejected/Timeout/Dropped: 返回 Ok(Some(msg)) 直接返回给 AI
///
/// 如果 `is_dangerous` 为 false：直接返回 None，调用方继续执行。
#[allow(clippy::too_many_arguments)]
async fn check_dangerous_confirm(
    is_dangerous: bool,
    rememberable: bool,
    pending: &PendingConfirms,
    emitter: &dyn EventPort,
    chat_service: Option<&std::sync::Arc<crate::domain::ai::chat_service::ChatService>>,
    tool_name: &str,
    tool_type: &'static str,
    args_value: &Value,
) -> Option<Result<String, rig_core::tool::ToolError>> {
    if !is_dangerous {
        return None;
    }

    let (req_id, conv_id, target_win) =
        crate::domain::ai::chat_service::current_request_context(chat_service);

    // 对话级信任：用户已确认过的危险操作自动放行，不再弹窗
    let trusted = rememberable && pending.is_trusted(&conv_id, tool_name).await;
    if may_reuse_confirmation(rememberable, trusted) {
        tracing::debug!(
        %tool_name,
        conversation_id = %conv_id,
        "危险操作已在本次对话内获用户信任，跳过确认"
        );
        return None;
    }

    tracing::warn!(%tool_name, "危险操作被 AI 调用，挂起等待用户确认");
    let (confirm_id, rx) = pending.register().await;
    match await_dangerous_confirm(
        pending,
        emitter,
        confirm_id,
        rx,
        tool_name,
        tool_type,
        args_value,
        req_id,
        &conv_id,
        &target_win,
    )
    .await
    {
        ConfirmOutcome::Approved => {
            tracing::info!(%tool_name, "用户确认执行危险 {tool_type}");
            if rememberable {
                // 加入对话级信任列表，后续同对话内不再弹窗
                pending.trust(&conv_id, tool_name).await;
            }
            None
        }
        ConfirmOutcome::Rejected => Some(Ok(format!("用户拒绝了操作: {tool_name}（未执行）"))),
        ConfirmOutcome::Timeout => Some(Ok(format!(
            "确认超时（{DANGEROUS_CONFIRM_TIMEOUT_SECS}秒未响应），未执行: {tool_name}"
        ))),
        ConfirmOutcome::Dropped => Some(Ok(format!("确认信号异常，未执行: {tool_name}"))),
    }
}

fn may_reuse_confirmation(rememberable: bool, trusted: bool) -> bool {
    rememberable && trusted
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// `ToolCallError` 的消息载体。
///
/// 避免滥用 `std::io::Error` 包装纯文本消息（io::Error 语义是 IO 失败，此处只是给 AI 的可读字符串）。
/// 原始错误类型在 `tracing::warn!` 中已通过 `Display` 记录，AI 侧只需可读消息。
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
struct ToolErrMsg(String);

/// 危险操作确认事件 payload。
#[derive(serde::Serialize, Clone)]
struct ConfirmPayload {
    /// 待确认项编号--前端确认后回传此 id 唤醒挂起的 `call`。
    confirm_id: u64,
    tool_name: String,
    tool_type: &'static str, // "capability" 或 "action"
    arguments: Value,
    danger_class: &'static str,
    /// Phase 4: 关联的请求和对话标识，前端按 request_id 校验归属。
    request_id: u64,
    conversation_id: String,
}

/// emit 危险操作确认事件到对话窗口前端（定向发送）。
///
/// **事件名 `blink://chat-confirm-action`** 统一用于对话窗口和主窗口 AI 模式：
/// 0.17.6 起，主窗口 AI 也走 ChatService，按 `conversation_id` 过滤区分，
/// 不再有独立的 `ai-confirm-action` 事件。
///
/// Phase 4：改用 `emit_to("chat")` 定向发送，不向主窗口和其他次级窗口广播。
#[allow(clippy::too_many_arguments)]
fn emit_dangerous_confirm(
    emitter: &dyn EventPort,
    confirm_id: u64,
    tool_name: &str,
    tool_type: &'static str,
    arguments: &Value,
    request_id: u64,
    conversation_id: &str,
    target_window: &str,
) {
    let payload = ConfirmPayload {
        confirm_id,
        tool_name: tool_name.to_string(),
        tool_type,
        arguments: arguments.clone(),
        danger_class: "Dangerous",
        request_id,
        conversation_id: conversation_id.to_string(),
    };
    // 0.17.6: 按 target_window 定向 emit（主窗口 AI / 对话窗口共用）。
    // target_window 为空时回落到 "chat"（兼容未注入场景）。
    let win = if target_window.is_empty() {
        "chat"
    } else {
        target_window
    };
    if let Err(e) = emitter.emit_to(
        win,
        EventNames::CHAT_CONFIRM_ACTION,
        serde_json::to_value(&payload).unwrap_or_default(),
    ) {
        tracing::debug!(error = %e, "emit chat-confirm-action failed");
    }
}

/// 从 `AIConfig.slo_hard_timeout_ms` 派生 tool 调用 deadline（P1.3 硬超时铁则）。
///
/// 对话窗口 Capability invoke 的硬超时，对齐主窗口 `service.rs` 的 `slo_hard_timeout_ms`。
/// `None` -> 用 `AIConfig::effective_hard_timeout_ms()` 的统一 20 秒默认值兜底。
fn derive_tool_deadline() -> Option<std::time::Instant> {
    let timeout_ms = get_ai_config().effective_hard_timeout_ms();
    Some(std::time::Instant::now() + Duration::from_millis(timeout_ms as u64))
}

// ── CapabilityTool ────────────────────────────────────────────────────────────

/// Capability 的 Tool 包装器。
///
/// 持有 `Arc<dyn Capability>`，实现 `ToolDyn` 以供 rig Agent 使用。
///
/// **设计要点**：
/// - `definition()` -> `schema.to_rig_tool()`（纯 schema 投影）
/// - `call()` -> 危险操作先挂起确认 -> `registry.invoke(id, args, ctx)` -> `CapabilityResult` -> `to_rig_tool_result()`
/// - `InvokeContext.deadline` 从 `slo_hard_timeout_ms` 派生（P1.3 硬超时）
///
/// **0.21.11 变更**：确认通过后调用不再直接 `cap.invoke()`，而是经 `CapabilityRegistry::invoke()`，
/// 统一来源检查、运行时检查、SLO 与 perf 埋点。
pub struct CapabilityTool {
    cap_id: String,
    cap: Arc<dyn Capability>,
    schema: crate::domain::capability::CapabilitySchema,
    emitter: Arc<dyn EventPort>,
    chat_service: Option<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>,
    cap_env: std::sync::Arc<dyn crate::domain::event::CapabilityEnv>,
    pending: Arc<PendingConfirms>,
    registry: Arc<CapabilityRegistry>,
    /// GUI surface（主进程注入）。None = CLI 等无 GUI 宿主，GUI starter 能力会被
    /// Registry 的 runtime 门禁拒绝。
    surface: Option<std::sync::Arc<dyn crate::domain::capability::SurfacePort>>,
}

impl CapabilityTool {
    /// 构造 CapabilityTool。
    pub fn new(
        cap: Arc<dyn Capability>,
        emitter: Arc<dyn EventPort>,
        chat_service: Option<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>,
        cap_env: std::sync::Arc<dyn crate::domain::event::CapabilityEnv>,
        pending: Arc<PendingConfirms>,
        registry: Arc<CapabilityRegistry>,
        surface: Option<std::sync::Arc<dyn crate::domain::capability::SurfacePort>>,
    ) -> Self {
        let schema = cap.schema();
        Self {
            cap_id: cap.id().to_string(),
            cap,
            schema,
            emitter,
            chat_service,
            cap_env,
            pending,
            registry,
            surface,
        }
    }

    /// 检查是否是危险操作（0.13.0 §9.2 修复：同时读 `sensitive` 字段）。
    ///
    /// `danger_class == Dangerous` 或 `schema.sensitive == true` 均触发确认弹窗。
    /// `sensitive` 标记的是读隐私数据的能力（如 `search_apps` / `search_clipboard_history`），
    /// 虽非"危险操作"但涉及隐私，AI 调用时同样需用户确认。
    fn requires_confirmation(&self) -> bool {
        self.cap.requires_ai_confirmation()
    }
}

impl ToolDyn for CapabilityTool {
    fn name(&self) -> String {
        self.cap.id().to_string()
    }

    fn definition<'a>(
        &'a self,
        // rig 传入的 prompt 是运行时上下文提示，用于动态调整 schema 描述。
        // Capability 的 schema 是静态投影，不随 prompt 变化，故故意忽略。
        _prompt: String,
    ) -> WasmBoxedFuture<'a, rig_core::completion::ToolDefinition> {
        Box::pin(async move { self.schema.to_rig_tool() })
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> WasmBoxedFuture<'a, Result<String, rig_core::tool::ToolError>> {
        Box::pin(async move {
            // 解析 JSON args（先解析，危险操作也要把 args 传给前端确认卡片）
            let args_value: Value = match serde_json::from_str(&args) {
                Ok(v) => v,
                Err(e) => return Err(rig_core::tool::ToolError::JsonError(e)),
            };

            // 危险操作确认（四域墙 + 闭环）
            if let Some(result) = check_dangerous_confirm(
                self.requires_confirmation(),
                self.cap.ai_confirmation_rememberable(),
                &self.pending,
                self.emitter.as_ref(),
                self.chat_service.as_ref(),
                self.cap.id(),
                "capability",
                &args_value,
            )
            .await
            {
                return result;
            }

            // 构造 InvokeContext（P1.3: 从 slo_hard_timeout_ms 派生 deadline）
            // 0.21.0: 携带 origin=LocalAi + 完整 runtime（AI 在主进程中运行，有 GUI surface）
            let ctx = InvokeContext {
                env: self.cap_env.as_ref(),
                origin: crate::domain::capability::InvocationOrigin::LocalAi,
                runtime: crate::domain::capability::RuntimeCapabilities {
                    surface: self.surface.as_deref(),
                    main_process: true,
                    desktop_session: true,
                },
                deadline: derive_tool_deadline(),
            };

            // 0.21.11: 删除重复门禁——确认通过后调用 CapabilityRegistry::invoke，
            // registry 内部会做 origin/runtime 检查，无需在此重复。
            // 这确保 AI tool 调用走统一路径，获取一致的门禁、SLO 和 perf 埋点。

            // 调用 Capability（0.21.11：统一经 CapabilityRegistry::invoke）
            match self.registry.invoke(&self.cap_id, args_value, &ctx).await {
                Ok(cap_result) => {
                    let stash = self.cap_env.image_stash();
                    let contents =
                        cap_result.to_rig_tool_result_with_stash(stash.map(|s| s.as_ref()));
                    Ok(crate::domain::capability::rig_tool_result_to_text(
                        &contents,
                    ))
                }
                Err(e) => {
                    // 原始错误类型记日志（保留类型信息供调试），AI 侧拿中文化消息
                    tracing::warn!(error = %e, capability = %self.cap_id, "capability invoke 失败");
                    let msg = capability_error_to_string(e);
                    Err(rig_core::tool::ToolError::ToolCallError(Box::new(
                        ToolErrMsg(msg),
                    )))
                }
            }
        })
    }
}

// ── 错误转换 ─────────────────────────────────────────────────────────────────

/// `CapabilityError` -> 用户可读字符串。
///
/// 投影到 `ToolError::ToolCallError` 的 message--让 AI 知道失败原因（可重试或换路径）。
fn capability_error_to_string(e: CapabilityError) -> String {
    match e {
        CapabilityError::InvalidArgs { detail } => format!("参数无效: {detail}"),
        CapabilityError::InvalidState { detail } => format!("状态无效: {detail}"),
        CapabilityError::Conflict { detail } => format!("并发冲突: {detail}"),
        CapabilityError::InvalidData { reason, detail } => {
            format!("数据无效（{reason}）: {detail}")
        }
        CapabilityError::Permission { detail } => format!("权限不足: {detail}"),
        CapabilityError::OriginDenied { origin, allowed } => {
            format!("来源不被允许: {origin} 不在允许集合内 ({allowed})")
        }
        CapabilityError::Unsupported { required, actual } => {
            format!("运行时不支持: 需要 {required}，当前可用 {actual}")
        }
        CapabilityError::Timeout { detail } => format!("超时: {detail}"),
        CapabilityError::Cancelled => "已取消".to_string(),
        CapabilityError::NotFound { id } => format!("未找到: {id}"),
        CapabilityError::Internal { detail } => format!("内部错误: {detail}"),
    }
}

// ── 工厂函数 ─────────────────────────────────────────────────────────────────

/// 构建对话窗口 Agent 使用的 tool 池（0.14.2：只含 Capability + 外部 tool）。
///
/// 从 `CapabilityRegistry` 收集所有可用能力，返回 `Vec<Box<dyn ToolDyn>>`
/// 供 rig `AgentBuilder` 使用。
///
/// **参数**：
/// - `cap_registry`: Capability 注册表
/// - `external_tools`: 外部 tool（如 MCP tool），直接进 tool 池，不经过 CapabilityRegistry
///   （0.13.0 §9.3：统一外部 tool 入口，为 MCP tool 留对称性）
/// - `emitter`: EventPort，用于 emit 确认事件
/// - `chat_service`: ChatService 引用，用于获取 request context（可能为 None）
/// - `cap_env`: CapabilityEnv，用于构造 InvokeContext
/// - `pending`: 危险确认注册表（`Arc<PendingConfirms>`，由 main.rs manage，对话窗口共享）
/// - `ai_allowlist`: AI 授权的 Capability id 集合（0.21.5）。`None` = 不过滤（兼容旧测试）；
///   `Some(set)` = 只包装 set 中的 Capability。
///
/// **返回**：所有可用的 tool（CapabilityTool + external_tools）
///
/// **0.14.2 变化**：删除了 `action_registry` 参数和 ActionTool 包装。AI tool 池
/// 只含 Capability。9 个保留 Action（lock/shutdown/...）不再出现在 tool 池——
/// 这把"AI 该不该调这个能力"从运行时过滤前置成编译期类型约束。
/// **危险操作**：危险 Capability 仍会被包装进 tool 池，但调用时挂起等用户确认。
/// **0.21.5 变化**：增加 `ai_allowlist` 参数——只包装 policy 允许 AI 且用户 enabled 的 Capability。
/// 纯对话模式传空 `HashSet`（tool 池为空）。
#[allow(dead_code)] // 0.12.1 对话窗口 AgentBuilder 消费
#[allow(clippy::too_many_arguments)]
pub fn build_agent_tools(
    cap_registry: Arc<CapabilityRegistry>,
    external_tools: Vec<Box<dyn ToolDyn>>,
    emitter: Arc<dyn EventPort>,
    chat_service: Option<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>,
    cap_env: std::sync::Arc<dyn crate::domain::event::CapabilityEnv>,
    pending: Arc<PendingConfirms>,
    surface: Option<std::sync::Arc<dyn crate::domain::capability::SurfacePort>>,
    ai_allowlist: Option<&std::collections::HashSet<String>>,
) -> Vec<Box<dyn ToolDyn>> {
    let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

    // 1. 包装所有 Capability（0.21.5: 仅包装 allowlist 中的）
    let mut cap_count = 0;
    let mut skipped_count = 0;
    for (id, cap) in cap_registry.entries() {
        // 0.21.5: allowlist 过滤——只包装用户授权的 Capability
        if let Some(allowlist) = ai_allowlist
            && !allowlist.contains(&id)
        {
            skipped_count += 1;
            continue;
        }

        let tool = CapabilityTool::new(
            cap,
            emitter.clone(),
            chat_service.clone(),
            cap_env.clone(),
            pending.clone(),
            cap_registry.clone(),
            surface.clone(),
        );
        tools.push(Box::new(tool));
        cap_count += 1;
    }

    // 2. 追加外部 tool（MCP tool 等，已包装为 ToolDyn，直接进池）
    let external_count = external_tools.len();
    tools.extend(external_tools);

    tracing::info!(
        capabilities = cap_count,
        skipped = skipped_count,
        external_tools = external_count,
        total_tools = tools.len(),
        "build_agent_tools: tool 池构建完成（0.21.5: AI allowlist 过滤）"
    );

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{CapabilityResult, CapabilitySchema, ItemResult};
    use serde_json::json;

    // 0.14.2: ActionTool 已删除，以下测试验证 AI tool 池只含 Capability。

    // ── PendingConfirms 单测（纯逻辑，0.12.0 §2.4 闭环骨架）──────────────────

    #[tokio::test]
    async fn pending_confirms_register_and_resolve_approved() {
        let pc = PendingConfirms::new();
        let (id, rx) = pc.register().await;
        // 确认 -> receiver 收 true
        assert!(pc.resolve(id, true).await, "已知 id 应送达");
        assert!(rx.await.unwrap(), "确认应收到 true");
    }

    #[tokio::test]
    async fn pending_confirms_resolve_rejected() {
        let pc = PendingConfirms::new();
        let (id, rx) = pc.register().await;
        assert!(pc.resolve(id, false).await);
        assert!(!rx.await.unwrap(), "拒绝应收到 false");
    }

    #[tokio::test]
    async fn pending_confirms_resolve_unknown_id_returns_false() {
        let pc = PendingConfirms::new();
        // 未注册的 id -> false（过期/编号不存在）
        assert!(!pc.resolve(9999, true).await, "未知 id 应返回 false");
    }

    #[tokio::test]
    async fn pending_confirms_resolve_twice_second_fails() {
        let pc = PendingConfirms::new();
        let (id, _rx) = pc.register().await;
        assert!(pc.resolve(id, true).await, "首次 resolve 成功");
        // 第二次 resolve 同 id -> false（已 remove）
        assert!(!pc.resolve(id, true).await, "重复 resolve 应返回 false");
    }

    #[tokio::test]
    async fn pending_confirms_discard_drops_sender() {
        let pc = PendingConfirms::new();
        let (id, rx) = pc.register().await;
        pc.discard(id).await; // 超时清理 -> drop sender
        // receiver 收 Err（sender 被 drop）
        assert!(rx.await.is_err(), "discard 后 receiver 应收 Err");
    }

    #[tokio::test]
    async fn pending_confirms_id_monotonic() {
        let pc = PendingConfirms::new();
        let (id1, _rx1) = pc.register().await;
        let (id2, _rx2) = pc.register().await;
        let (id3, _rx3) = pc.register().await;
        assert!(id2 > id1, "id 应单调递增");
        assert!(id3 > id2);
    }

    // ── 对话级信任列表单测 ──────────────────────────────────────────────

    #[tokio::test]
    async fn trust_after_confirm_skips_subsequent_confirmation() {
        let pc = PendingConfirms::new();
        // 初始不信任
        assert!(!pc.is_trusted("conv1", "shutdown").await);
        // 模拟用户确认后加入信任
        pc.trust("conv1", "shutdown").await;
        // 再次检查应已信任
        assert!(pc.is_trusted("conv1", "shutdown").await);
    }

    #[tokio::test]
    async fn trust_is_per_conversation() {
        let pc = PendingConfirms::new();
        pc.trust("conv1", "shutdown").await;
        // 同一 tool 在不同对话不应共享信任
        assert!(pc.is_trusted("conv1", "shutdown").await);
        assert!(!pc.is_trusted("conv2", "shutdown").await);
    }

    #[tokio::test]
    async fn trust_is_per_tool() {
        let pc = PendingConfirms::new();
        pc.trust("conv1", "shutdown").await;
        // 同一对话内不同 tool 不共享信任
        assert!(pc.is_trusted("conv1", "shutdown").await);
        assert!(!pc.is_trusted("conv1", "lock").await);
    }

    #[tokio::test]
    async fn clear_trust_removes_only_target_conversation() {
        let pc = PendingConfirms::new();
        pc.trust("conv1", "shutdown").await;
        pc.trust("conv1", "lock").await;
        pc.trust("conv2", "sleep").await;
        // 清除 conv1 的信任
        pc.clear_trust("conv1").await;
        // conv1 的都清了
        assert!(!pc.is_trusted("conv1", "shutdown").await);
        assert!(!pc.is_trusted("conv1", "lock").await);
        // conv2 的不受影响
        assert!(pc.is_trusted("conv2", "sleep").await);
    }

    // ── derive_tool_deadline 单测 ──────────────────────────────────────────

    #[test]
    fn derive_tool_deadline_returns_some_future_instant() {
        // 纯逻辑：应返回一个未来的 Instant（无论 AIConfig 是否配置）。
        // 不依赖系统资源--get_ai_config 未初始化时返回 default（enabled=false，
        // slo_hard_timeout_ms=None -> 兜底统一 20 秒默认值。
        let now = std::time::Instant::now();
        let deadline = derive_tool_deadline();
        assert!(deadline.is_some(), "deadline 应为 Some");
        let d = deadline.unwrap();
        assert!(d > now, "deadline 应在当前时刻之后（含 20s 兜底或配置值）");
    }

    // ── capability_error_to_string 测试 ───────────────────────────────────

    #[test]
    fn capability_error_invalid_args_to_string() {
        let e = CapabilityError::InvalidArgs {
            detail: "缺少 query".into(),
        };
        assert!(capability_error_to_string(e).contains("参数无效"));
        assert!(
            capability_error_to_string(CapabilityError::InvalidArgs {
                detail: "缺少 query".into()
            })
            .contains("缺少 query")
        );
    }

    #[test]
    fn capability_error_cancelled_to_string() {
        assert_eq!(
            capability_error_to_string(CapabilityError::Cancelled),
            "已取消"
        );
    }

    #[test]
    fn capability_error_not_found_to_string() {
        let e = CapabilityError::NotFound {
            id: "test_cap".into(),
        };
        assert!(capability_error_to_string(e).contains("test_cap"));
    }

    #[test]
    fn capability_error_all_variants_have_messages() {
        // 确保所有变体都有可读消息，不会 panic
        let cases = [
            CapabilityError::InvalidArgs { detail: "x".into() },
            CapabilityError::InvalidState { detail: "x".into() },
            CapabilityError::Conflict { detail: "x".into() },
            CapabilityError::InvalidData {
                reason: "binary".into(),
                detail: "x".into(),
            },
            CapabilityError::Permission { detail: "x".into() },
            CapabilityError::OriginDenied {
                origin: "mcp".into(),
                allowed: "all".into(),
            },
            CapabilityError::Unsupported {
                required: "gui".into(),
                actual: "none".into(),
            },
            CapabilityError::Timeout { detail: "x".into() },
            CapabilityError::Cancelled,
            CapabilityError::NotFound { id: "x".into() },
            CapabilityError::Internal { detail: "x".into() },
        ];
        for e in cases {
            let msg = capability_error_to_string(e);
            assert!(!msg.is_empty(), "错误消息不应为空");
        }
    }

    // ── Capability 统一确认策略（不需要 AppHandle）──────────────────────

    struct ConfirmationCap {
        sensitive: bool,
        danger: crate::domain::capability::policy::DangerClass,
    }

    #[async_trait::async_trait]
    impl Capability for ConfirmationCap {
        fn id(&self) -> &str {
            "confirmation_test"
        }

        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema {
                sensitive: self.sensitive,
                ..CapabilitySchema::empty("confirmation_test", "test")
            }
        }

        // 0.21.0: policy 是唯一真源，danger_class / requires_ai_confirmation 从此投影
        fn policy(&self) -> crate::domain::capability::CapabilityPolicy {
            use crate::domain::capability::*;
            let danger = self.danger;
            let sensitive = self.sensitive;
            CapabilityPolicy {
                danger,
                sensitive,
                confirmation: if danger == DangerClass::Dangerous {
                    ConfirmationPolicy::dangerous(true)
                } else if sensitive {
                    ConfirmationPolicy::sensitive()
                } else {
                    ConfirmationPolicy::safe()
                },
                ..Default::default()
            }
        }

        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            unreachable!("确认策略测试不执行 capability")
        }
    }

    #[test]
    fn capability_confirmation_covers_dangerous_and_sensitive() {
        use crate::domain::capability::policy::DangerClass;

        let safe = ConfirmationCap {
            sensitive: false,
            danger: DangerClass::Safe,
        };
        let sensitive = ConfirmationCap {
            sensitive: true,
            danger: DangerClass::Safe,
        };
        let dangerous = ConfirmationCap {
            sensitive: false,
            danger: DangerClass::Dangerous,
        };

        assert!(!safe.requires_ai_confirmation());
        assert!(sensitive.requires_ai_confirmation());
        assert!(dangerous.requires_ai_confirmation());
    }

    #[test]
    fn non_rememberable_capability_never_reuses_tool_trust() {
        assert!(!may_reuse_confirmation(false, true));
        assert!(may_reuse_confirmation(true, true));
        assert!(!may_reuse_confirmation(true, false));
    }

    // ── CapabilityRegistry::entries 测试 ──────────────────────────────────

    /// Mock Capability--避免构造 AppHandle（遵循 AGENTS.md §7）。
    struct MockCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for MockCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "mock for test")
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Done {
                summary: "mock".into(),
            })
        }
    }

    #[test]
    fn capability_registry_entries_returns_registered() {
        let reg = CapabilityRegistry::default();
        let cap = Arc::new(MockCap {
            id_val: "entries_test_cap",
        }) as Arc<dyn Capability>;
        reg.register(cap).unwrap();
        let entries = reg.entries();
        assert!(entries.iter().any(|(id, _)| id == "entries_test_cap"));
    }

    #[test]
    fn capability_registry_entries_empty_when_no_register() {
        // default() 从 inventory 收集，测试环境可能有也可能没有
        // 只验证 entries() 不 panic
        let reg = CapabilityRegistry::default();
        let _ = reg.entries();
    }

    // ── 0.14.2: AI tool 池只含 Capability 验证 ──────────────────────────────

    /// 0.14.2 验收点：AI tool 池只含 Capability，不含 Action。
    ///
    /// `build_agent_tools` 签名已删除 `action_registry` 参数——编译期保证
    /// AI tool 池无法包含 Action。这里验证 CapabilityRegistry 包含
    /// open_url/open_path/reveal_in_explorer（从 Action 提升的新 Capability）。
    #[test]
    fn ai_tool_pool_only_contains_capabilities() {
        let cap_reg = CapabilityRegistry::default();
        // 0.14.2 新增的 3 个 Capability 应在注册表中
        assert!(
            cap_reg.get("open_url").is_some(),
            "open_url 应注册为 Capability"
        );
        assert!(
            cap_reg.get("open_path").is_some(),
            "open_path 应注册为 Capability"
        );
        assert!(
            cap_reg.get("reveal_in_explorer").is_some(),
            "reveal_in_explorer 应注册为 Capability"
        );
    }

    /// 0.21.1 验收点：旧 13 个 Action 已全量迁为 Capability，现在都在 CapabilityRegistry 中。
    ///
    /// Dangerous 类（lock/shutdown/restart/sleep/clear_history/exit_blink）虽已迁为
    /// Capability，但 `ai_default: Off` 意味着不会进入推荐 allowlist。
    /// Safe GUI 类（open_settings/open_logs/open_data_dir）`ai_default: On` 但 `mcp_default: Forbidden`。
    #[test]
    fn retained_actions_now_in_capability_registry() {
        let cap_reg = CapabilityRegistry::default();
        // 13 个旧 Action 现在都应在 CapabilityRegistry 中
        for action_id in [
            "lock",
            "shutdown",
            "restart",
            "sleep",
            "clear_history",
            "exit_blink",
            "open_logs",
            "open_data_dir",
            "open_settings",
            "sticky_manager",
            "edit_clipboard_image",
            "blink_print_debug_info",
            "blink_debug_inithook",
        ] {
            assert!(
                cap_reg.get(action_id).is_some(),
                "{action_id} 应已在 CapabilityRegistry 中（0.21.1 迁移）"
            );
        }
    }

    // ── CapabilityTool round-trip 测试（0.12.0 §2.8 验收点）──────────────
    //
    // 验证 schema -> invoke -> to_rig_tool_result 完整链路（避开 AppHandle，
    // 遵循 AGENTS.md §7"Tauri 集成层免自动化"）。
    // 思路：直接对 MockCap 做 schema -> invoke -> 投影，验证输出正确性。

    /// MockCap 变体--返回 Text 结果。
    struct MockTextCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for MockTextCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "text mock")
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Text {
                content: "hello from mock".into(),
                desc: None,
            })
        }
    }

    /// MockCap 变体--返回 Items 结果。
    struct MockItemsCap {
        id_val: &'static str,
    }

    #[async_trait::async_trait]
    impl Capability for MockItemsCap {
        fn id(&self) -> &str {
            self.id_val
        }
        fn schema(&self) -> CapabilitySchema {
            CapabilitySchema::empty(self.id_val, "items mock")
        }
        async fn invoke(
            &self,
            _args: Value,
            _ctx: &InvokeContext<'_>,
        ) -> Result<CapabilityResult, CapabilityError> {
            Ok(CapabilityResult::Items {
                items: vec![ItemResult {
                    data: json!({ "name": "result1", "path": "/test" }),
                    desc: None,
                    actions: vec![],
                }],
            })
        }
    }

    #[tokio::test]
    async fn roundtrip_text_cap_schema_invoke_projection() {
        // schema -> invoke -> to_rig_tool_result -> rig_tool_result_to_text
        let cap = MockTextCap { id_val: "rt_text" };

        // 1. schema 可获取
        let schema = cap.schema();
        assert_eq!(schema.name, "rt_text");

        // 2. invoke 返回 Text
        // InvokeContext 需要 AppHandle--这里跳过 invoke 调用本身，
        // 直接验证 MockCap 返回的 CapabilityResult 的投影链路。
        // （invoke 的 MockCap 逻辑在 registry::register 测试中已覆盖）
        let result = CapabilityResult::Text {
            content: "hello from mock".into(),
            desc: None,
        };

        // 3. to_rig_tool_result 产生 ToolResultContent
        let contents = result.to_rig_tool_result();
        assert_eq!(contents.len(), 1);

        // 4. rig_tool_result_to_text 提取文本
        let text = crate::domain::capability::rig_tool_result_to_text(&contents);
        assert_eq!(text, "hello from mock");
    }

    #[tokio::test]
    async fn roundtrip_items_cap_schema_invoke_projection() {
        let cap = MockItemsCap { id_val: "rt_items" };

        // 1. schema 可获取
        let schema = cap.schema();
        assert_eq!(schema.name, "rt_items");

        // 2. 模拟 invoke 返回 Items
        let result = CapabilityResult::Items {
            items: vec![ItemResult {
                data: json!({ "name": "result1", "path": "/test" }),
                desc: None,
                actions: vec![],
            }],
        };

        // 3. to_rig_tool_result -> JSON 文本
        let contents = result.to_rig_tool_result();
        assert_eq!(contents.len(), 1);

        // 4. rig_tool_result_to_text 提取 JSON
        let text = crate::domain::capability::rig_tool_result_to_text(&contents);
        assert!(text.contains("result1"));
        assert!(text.contains("/test"));
    }

    // ── 0.17.8 双层 trusted 单测（7 项核心场景）──────────────────────────────
    //
    // 验收点见 phases/0.17-enhancement-polish.md §六 0.17.8。
    // 使用 in-memory SQLite + PendingConfirms::with_persistence 测试双层逻辑。

    use crate::infra::data::permission_memory::{
        self, clear_all_trusted as db_clear_all, init_db, is_tool_trusted, trust_tool,
    };
    use sqlx::sqlite::SqlitePoolOptions;

    /// 创建内存 SQLite 池 + 初始化 ai_permission_memory 表。
    async fn test_pool() -> sqlx::SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        init_db(&pool).await.expect("init table");
        pool
    }

    /// 场景 1：会话级命中时不查 DB（快路径）。
    ///
    /// trust() 写入会话级 + DB 后，清空 DB。is_trusted 仍应返回 true
    /// （说明走的是会话级 HashSet，没查 DB）。
    #[tokio::test]
    async fn perm_session_hit_skips_db() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        // trust 写入会话级 + DB
        pc.trust("conv1", "shutdown").await;
        // 清空 DB
        db_clear_all(&pool).await;
        // is_trusted 应仍返回 true——来自会话级，未查 DB
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "会话级命中应跳过 DB 查询"
        );
    }

    /// 场景 2：会话级未命中 -> 查 DB -> 命中且未过期 -> 加入会话级。
    ///
    /// 直接写 DB，调 is_trusted 应返回 true（来自 DB）。
    /// 再清空 DB 后再调 is_trusted，仍应返回 true（第一次已加入会话级）。
    #[tokio::test]
    async fn perm_db_hit_promotes_to_session() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        // 直接写 DB（绕过 trust()）
        trust_tool(&pool, "shutdown", 7).await;
        // 第一次 is_trusted：会话级未命中 -> 查 DB -> 命中 -> 加入会话级
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "DB 命中应返回 true"
        );
        // 清空 DB
        db_clear_all(&pool).await;
        // 第二次 is_trusted：会话级已命中（第一次已加入），不查 DB
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "第一次 DB 命中后应已加入会话级，第二次不再查 DB"
        );
    }

    /// 场景 3：DB 过期返回 false 并删除行。
    ///
    /// 手动写入已过期的行，is_trusted 应返回 false。
    /// 行应被实时删除（直接查 DB 验证）。
    #[tokio::test]
    async fn perm_db_expired_returns_false_and_deletes() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        // 手动写入已过期记录
        let now = permission_memory::now_ts();
        sqlx::query(
            "INSERT INTO ai_permission_memory (tool_name, trusted_at, expires_at) VALUES (?1, ?2, ?3)",
        )
        .bind("lock")
        .bind(now - 86_400 * 10) // 10 天前确认
        .bind(now - 1)            // 1 秒前过期
        .execute(&pool)
        .await
        .unwrap();

        // is_trusted 应返回 false（已过期）
        assert!(!pc.is_trusted("conv1", "lock").await, "过期行应返回 false");

        // 行应已被删除
        assert!(
            !is_tool_trusted(&pool, "lock").await,
            "过期行应已被实时删除"
        );
    }

    /// 场景 4：trust() 同时写会话级和 DB。
    ///
    /// 调 trust() 后，会话级和 DB 都应有记录。
    /// 验证方式：清空会话级后 is_trusted 仍返回 true（DB 有记录）；
    /// 清空 DB 后会话级仍返回 true（会话级有记录）。
    #[tokio::test]
    async fn perm_trust_writes_both_layers() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        pc.trust("conv1", "shutdown").await;

        // DB 层有记录
        assert!(
            is_tool_trusted(&pool, "shutdown").await,
            "trust() 应写入 DB 持久化层"
        );
        // 会话层有记录（清空 DB 后仍返回 true）
        db_clear_all(&pool).await;
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "trust() 应写入会话级 HashSet"
        );
    }

    /// 场景 5：memory_enabled = false 时不查 DB。
    ///
    /// 配置 memory_enabled = false，DB 有记录，is_trusted 应返回 false。
    /// 开启后应返回 true（开始查 DB）。
    #[tokio::test]
    async fn perm_memory_disabled_does_not_query_db() {
        let pool = test_pool().await;
        let disabled_config = AiPermissionConfig {
            memory_enabled: false,
            memory_days: 7,
        };
        let pc = PendingConfirms::with_persistence(pool.clone(), disabled_config);

        // DB 有记录
        trust_tool(&pool, "shutdown", 7).await;

        // memory_enabled = false -> 不查 DB -> 返回 false
        assert!(
            !pc.is_trusted("conv1", "shutdown").await,
            "memory_enabled = false 时不应查 DB"
        );

        // 开启 memory_enabled
        pc.update_memory_config(AiPermissionConfig::default()).await;

        // 现在应查 DB -> 返回 true
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "开启 memory_enabled 后应查 DB 并返回 true"
        );
    }

    /// 场景 6：clear_all_trusted_db() 清空 DB 但不影响会话级。
    ///
    /// trust() 写入双层后，调 clear_all_trusted_db()。
    /// DB 应清空，但会话级 is_trusted 仍返回 true。
    #[tokio::test]
    async fn perm_clear_all_db_preserves_session() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        pc.trust("conv1", "shutdown").await;
        pc.trust("conv1", "lock").await;

        // 清空 DB
        pc.clear_all_trusted_db().await;

        // DB 已清空
        assert!(!is_tool_trusted(&pool, "shutdown").await, "DB 应已清空");
        assert!(!is_tool_trusted(&pool, "lock").await, "DB 应已清空");

        // 会话级不受影响
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "会话级不应受 clear_all_trusted_db 影响"
        );
        assert!(
            pc.is_trusted("conv1", "lock").await,
            "会话级不应受 clear_all_trusted_db 影响"
        );
    }

    /// 场景 7：clear_trust() 只清会话级，不影响持久化 DB。
    ///
    /// trust() 写入双层后，调 clear_trust(conv1)。
    /// 会话级应清空，但 DB 记录保留。
    #[tokio::test]
    async fn perm_clear_trust_session_preserves_db() {
        let pool = test_pool().await;
        let pc = PendingConfirms::with_persistence(pool.clone(), AiPermissionConfig::default());

        pc.trust("conv1", "shutdown").await;

        // 清空会话级
        pc.clear_trust("conv1").await;

        // DB 记录保留
        assert!(
            is_tool_trusted(&pool, "shutdown").await,
            "clear_trust 不应影响 DB 持久化记录"
        );

        // is_trusted 应从 DB 重新命中（会话级已清，DB 还在）
        assert!(
            pc.is_trusted("conv1", "shutdown").await,
            "会话级清空后应从 DB 重新命中"
        );
    }
}
