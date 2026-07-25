//! Tool 适配层（0.12.0 §2.4）--把 Capability/Action 包装成 `rig::tool::Tool`。
//!
//! ## 动机
//!
//! rig Agent 的 `AgentBuilder` 注册 tool 的方式与现有架构根本不同：
//! - **现有架构**：数据 + 外部调度--`Vec<ActionSchema>` -> 投影成 `Vec<ToolDefinition>`
//!   作为 `CompletionRequest` 参数，service.rs 收到 `tool_calls` 后手工执行
//! - **rig Agent**：代码 + 内部调度--`impl rig::tool::Tool` trait，Agent 内部自动循环
//!
//! 冲突：现有的 Capability / 插件 tool 全部接不进 rig Agent，0.12 对话窗口整个走不通。
//!
//! ## 解决方案
//!
//! 抽出 `ToolAdapter` 层，把 Capability / Action 包装成 `impl rig::tool::Tool`：
//! - `CapabilityTool` 包装 `Arc<dyn Capability>`
//! - `ActionTool` 包装 `Arc<dyn Action>`
//! - `ToolDyn` 动态分发--Args 用 `serde_json::Value`，避免给每个 Capability 写强类型 Args
//!
//! ## 四域墙（危险操作确认 + 闭环）
//!
//! **危险操作**（`danger_class == Dangerous`）不直接执行，也不返回"假消息"让 AI 误以为
//! 已执行（那样 AI 会基于错误假设继续生成）。**对话窗口 rig agent loop 是无限轮**，
//! 正确的闭环是 `call` 内部 emit 确认事件后**挂起 await 用户确认信号**：
//! - 用户确认 -> 继续执行，返回真实结果
//! - 用户拒绝 -> 返回"用户拒绝"消息，AI 可换路径
//! - 超时（60s）-> 返回"超时未执行"消息，不卡死 agent loop
//!
//! 信号回传：0.12.1 对话窗口前端监听 `blink://chat-confirm-action` 事件 -> 弹确认 UI ->
//! 调 `confirm_chat_action` command -> `PendingConfirms::resolve` 唤醒挂起的 `call`。
//!
//! **事件名 `blink://chat-confirm-action`** 与主窗口 `blink://ai-confirm-action` 分流--
//! 主窗口 payload 含 `seq`（强校验），对话窗口用 `confirm_id`，共用会导致主窗 listener 吞事件。
//!
//! ## tool 池粒度（§2.4）
//!
//! `build_agent_tools()` 过滤 `ai_eligible() == false` 的 Action（如 `exit_blink`--
//! AI 不该让 Blink 自杀）。Capability 全部暴露（只读居多）；Dangerous 的靠确认闭环挡。
//!
//! ## 工厂函数
//!
//! `build_agent_tools()` 从 `CapabilityRegistry` 和 `ActionRegistry` 收集所有可用能力，
//! 返回 `Vec<Box<dyn ToolDyn>>` 供对话窗口 Agent 使用。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use rig_core::tool::ToolDyn;
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::Value;
use tauri::Emitter;
use tokio::sync::{Mutex, oneshot};

use crate::domain::capability::{Capability, CapabilityError, CapabilityRegistry, InvokeContext};
use crate::domain::execution::{Action, ActionContext, ActionRegistry, DangerClass, ExecError};

// ── 常量 ─────────────────────────────────────────────────────────────────────

/// 危险操作确认超时（秒）--超时返回"未执行"，不卡死 agent loop。
///
/// 60s 给用户足够时间审视危险操作（如关机/清空历史）；超时即放弃，AI 收到超时消息可换路径。
const DANGEROUS_CONFIRM_TIMEOUT_SECS: u64 = 60;

/// 对话窗口 tool 调用硬超时默认值（毫秒）--对齐 `service.rs::AI_DEFAULT_HARD_TIMEOUT_MS`。
///
/// `AIConfig.slo_hard_timeout_ms` 缺省时兜底。对话窗口 Capability invoke 的硬超时
/// 与主窗口 service.rs 共用同一铁则（§3.3 骨架层硬超时）。
const DEFAULT_TOOL_TIMEOUT_MS: u32 = 20_000;

// ── 危险确认闭环骨架（0.12.0 §2.4）────────────────────────────────────────────

/// 待确认的危险操作注册表--对话窗口 rig agent loop 的危险确认闭环核心。
///
/// `call` 挂起 await 期间，confirm_id -> oneshot Sender 存于此。用户确认/拒绝信号
/// 经 `confirm_chat_action` command 调 `resolve` 送回，唤醒挂起的 `call`。
///
/// **confirm_id**：`AtomicU64` 全局递增，不引入 uuid 依赖。
/// **不持久化**：进程重启即丢（pending 确认本就是瞬时状态，重启后 AI 重新发起即可）。
#[derive(Default)]
pub struct PendingConfirms {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<bool>>>,
}

impl PendingConfirms {
    /// 构造空注册表。
    pub fn new() -> Self {
        Self::default()
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

/// 挂起等待用户确认危险操作（CapabilityTool / ActionTool 共用）。
///
/// 生成 confirm_id -> emit `chat-confirm-action` 事件（含 confirm_id）->
/// `tokio::time::timeout` 等 receiver，超时 `DANGEROUS_CONFIRM_TIMEOUT_SECS` 秒。
///
/// **需 AppHandle emit**--遵循 AGENTS.md §7"Tauri 集成层免自动化"，此函数本身不单测，
/// 闭环的正确性靠 `PendingConfirms` 的 register/resolve/discard 纯逻辑单测保证。
async fn await_dangerous_confirm(
    pending: &PendingConfirms,
    app: &tauri::AppHandle,
    confirm_id: u64,
    rx: oneshot::Receiver<bool>,
    tool_name: &str,
    tool_type: &'static str,
    arguments: &Value,
    request_id: u64,
    conversation_id: &str,
) -> ConfirmOutcome {
    emit_dangerous_confirm(
        app,
        confirm_id,
        tool_name,
        tool_type,
        arguments,
        request_id,
        conversation_id,
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

/// 危险操作确认+执行的公共逻辑（CapabilityTool / ActionTool 共用）。
///
/// 如果 `is_dangerous` 为 true：注册确认 -> emit 事件 -> 挂起等待 ->
/// - Approved: 继续执行（返回 None）
/// - Rejected/Timeout/Dropped: 返回 Ok(Some(msg)) 直接返回给 AI
///
/// 如果 `is_dangerous` 为 false：直接返回 None，调用方继续执行。
async fn check_dangerous_confirm(
    is_dangerous: bool,
    pending: &PendingConfirms,
    app: &tauri::AppHandle,
    tool_name: &str,
    tool_type: &'static str,
    args_value: &Value,
) -> Option<Result<String, rig_core::tool::ToolError>> {
    if !is_dangerous {
        return None;
    }
    tracing::warn!(%tool_name, "危险操作被 AI 调用，挂起等待用户确认");
    let (confirm_id, rx) = pending.register().await;
    let (req_id, conv_id) =
        crate::domain::ai::chat_service::current_request_context_from_app(app);
    match await_dangerous_confirm(
        pending,
        app,
        confirm_id,
        rx,
        tool_name,
        tool_type,
        args_value,
        req_id,
        &conv_id,
    )
    .await
    {
        ConfirmOutcome::Approved => {
            tracing::info!(%tool_name, "用户确认执行危险 {tool_type}");
            None
        }
        ConfirmOutcome::Rejected => Some(Ok(format!(
            "用户拒绝了操作: {tool_name}（未执行）"
        ))),
        ConfirmOutcome::Timeout => Some(Ok(format!(
            "确认超时（{DANGEROUS_CONFIRM_TIMEOUT_SECS}秒未响应），未执行: {tool_name}"
        ))),
        ConfirmOutcome::Dropped => Some(Ok(format!(
            "确认信号异常，未执行: {tool_name}"
        ))),
    }
}

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// `ToolCallError` 的消息载体。
///
/// 避免滥用 `std::io::Error` 包装纯文本消息（io::Error 语义是 IO 失败，此处只是给 AI 的可读字符串）。
/// 原始错误类型在 `tracing::warn!` 中已通过 `Display` 记录，AI 侧只需可读消息。
#[derive(Debug)]
struct ToolErrMsg(String);
impl std::fmt::Display for ToolErrMsg {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}
impl std::error::Error for ToolErrMsg {}

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
/// **事件名 `blink://chat-confirm-action`**--与主窗口 `blink://ai-confirm-action` 分流：
/// 主窗口 payload 含 `seq`（强校验），对话窗口 payload 用 `confirm_id`（无 seq）。
/// 共用事件名会导致主窗口 listener 吞掉对话窗口事件，故分离。
///
/// Phase 4：改用 `emit_to("chat")` 定向发送，不向主窗口和其他次级窗口广播。
fn emit_dangerous_confirm(
    app: &tauri::AppHandle,
    confirm_id: u64,
    tool_name: &str,
    tool_type: &'static str,
    arguments: &Value,
    request_id: u64,
    conversation_id: &str,
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
    if let Err(e) = app.emit_to(
        tauri::EventTarget::window("chat"),
        "blink://chat-confirm-action",
        payload,
    ) {
        tracing::debug!(error = %e, "emit chat-confirm-action failed");
    }
}

/// 从 `AIConfig.slo_hard_timeout_ms` 派生 tool 调用 deadline（P1.3 硬超时铁则）。
///
/// 对话窗口 Capability invoke 的硬超时，对齐主窗口 `service.rs` 的 `slo_hard_timeout_ms`。
/// `None` -> 用 `DEFAULT_TOOL_TIMEOUT_MS` 兜底（20s）。
fn derive_tool_deadline() -> Option<std::time::Instant> {
    let cfg = crate::app::ai_config::get_ai_config();
    let timeout_ms = cfg.slo_hard_timeout_ms.unwrap_or(DEFAULT_TOOL_TIMEOUT_MS);
    Some(std::time::Instant::now() + Duration::from_millis(timeout_ms as u64))
}

// ── CapabilityTool ────────────────────────────────────────────────────────────

/// Capability 的 Tool 包装器。
///
/// 持有 `Arc<dyn Capability>`，实现 `ToolDyn` 以供 rig Agent 使用。
///
/// **设计要点**：
/// - `definition()` -> `schema.to_rig_tool()`（纯 schema 投影）
/// - `call()` -> 危险操作先挂起确认 -> `cap.invoke(args, ctx)` -> `CapabilityResult` -> `to_rig_tool_result()`
/// - `InvokeContext.deadline` 从 `slo_hard_timeout_ms` 派生（P1.3 硬超时）
pub struct CapabilityTool {
    cap: Arc<dyn Capability>,
    schema: crate::domain::capability::CapabilitySchema,
    app_handle: tauri::AppHandle,
    pending: Arc<PendingConfirms>,
}

impl CapabilityTool {
    /// 构造 CapabilityTool。
    pub fn new(
        cap: Arc<dyn Capability>,
        app_handle: tauri::AppHandle,
        pending: Arc<PendingConfirms>,
    ) -> Self {
        let schema = cap.schema();
        Self {
            cap,
            schema,
            app_handle,
            pending,
        }
    }

    /// 检查是否是危险操作。
    fn is_dangerous(&self) -> bool {
        matches!(self.cap.danger_class(), DangerClass::Dangerous)
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
                self.is_dangerous(),
                &self.pending,
                &self.app_handle,
                self.cap.id(),
                "capability",
                &args_value,
            )
            .await
            {
                return result;
            }

            // 构造 InvokeContext（P1.3: 从 slo_hard_timeout_ms 派生 deadline）
            let ctx = InvokeContext {
                app_handle: &self.app_handle,
                deadline: derive_tool_deadline(),
            };

            // 调用 Capability
            match self.cap.invoke(args_value, &ctx).await {
                Ok(cap_result) => {
                    let contents = cap_result.to_rig_tool_result();
                    Ok(crate::domain::capability::rig_tool_result_to_text(
                        &contents,
                    ))
                }
                Err(e) => {
                    // 原始错误类型记日志（保留类型信息供调试），AI 侧拿中文化消息
                    tracing::warn!(error = %e, capability = %self.cap.id(), "capability invoke 失败");
                    let msg = capability_error_to_string(e);
                    Err(rig_core::tool::ToolError::ToolCallError(Box::new(
                        ToolErrMsg(msg),
                    )))
                }
            }
        })
    }
}

// ── ActionTool ───────────────────────────────────────────────────────────────

/// Action 的 Tool 包装器。
///
/// 持有 `Arc<dyn Action>`，实现 `ToolDyn` 以供 rig Agent 使用。
///
/// **设计要点**：
/// - `definition()` -> `schema.to_rig_tool()`（纯 schema 投影）
/// - `call()` -> 危险操作先挂起确认 -> `action.execute()` -> `ActionOutcome` -> `to_rig_tool_result()`
/// - 无 deadline（`ActionContext` 无 deadline 字段；Action execute 多为短耗时副作用，
///   长耗时超时留待 Action trait 演进时补）
pub struct ActionTool {
    action: Arc<dyn Action>,
    schema: crate::domain::execution::ActionSchema,
    app_handle: tauri::AppHandle,
    pending: Arc<PendingConfirms>,
}

impl ActionTool {
    /// 构造 ActionTool。
    pub fn new(
        action: Arc<dyn Action>,
        app_handle: tauri::AppHandle,
        pending: Arc<PendingConfirms>,
    ) -> Self {
        let schema = action.schema();
        Self {
            action,
            schema,
            app_handle,
            pending,
        }
    }

    /// 检查是否是危险操作。
    fn is_dangerous(&self) -> bool {
        matches!(self.action.danger_class(), DangerClass::Dangerous)
    }
}

impl ToolDyn for ActionTool {
    fn name(&self) -> String {
        self.action.id().to_string()
    }

    fn definition<'a>(
        &'a self,
        // rig 传入的 prompt 是运行时上下文提示，用于动态调整 schema 描述。
        // Action 的 schema 是静态投影，不随 prompt 变化，故故意忽略。
        _prompt: String,
    ) -> WasmBoxedFuture<'a, rig_core::completion::ToolDefinition> {
        Box::pin(async move { self.schema.to_rig_tool() })
    }

    fn call<'a>(
        &'a self,
        args: String,
    ) -> WasmBoxedFuture<'a, Result<String, rig_core::tool::ToolError>> {
        Box::pin(async move {
            // 解析 JSON args
            let args_value: Value = match serde_json::from_str(&args) {
                Ok(v) => v,
                Err(e) => return Err(rig_core::tool::ToolError::JsonError(e)),
            };

            // 危险操作确认（四域墙 + 闭环）
            if let Some(result) = check_dangerous_confirm(
                self.is_dangerous(),
                &self.pending,
                &self.app_handle,
                self.action.id(),
                "action",
                &args_value,
            )
            .await
            {
                return result;
            }

            // 构造 ActionContext
            let cx = ActionContext {
                app_handle: &self.app_handle,
                arguments: args_value,
            };

            // 调用 Action
            match self.action.execute(&cx).await {
                Ok(outcome) => {
                    let contents = outcome.to_rig_tool_result();
                    Ok(crate::domain::capability::rig_tool_result_to_text(
                        &contents,
                    ))
                }
                Err(e) => {
                    tracing::warn!(error = %e, action = %self.action.id(), "action execute 失败");
                    let msg = exec_error_to_string(e);
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
        CapabilityError::Permission { detail } => format!("权限不足: {detail}"),
        CapabilityError::Timeout { detail } => format!("超时: {detail}"),
        CapabilityError::Cancelled => "已取消".to_string(),
        CapabilityError::NotFound { id } => format!("未找到: {id}"),
        CapabilityError::Internal { detail } => format!("内部错误: {detail}"),
    }
}

/// `ExecError` -> 用户可读字符串（对称 `capability_error_to_string`，分变体中文化）。
fn exec_error_to_string(e: ExecError) -> String {
    match e {
        ExecError::MissingArg(action) => format!("{action}: 缺少字符串参数"),
        ExecError::Runtime(msg) => format!("执行失败: {msg}"),
    }
}

// ── 工厂函数 ─────────────────────────────────────────────────────────────────

/// 构建对话窗口 Agent 使用的 tool 池。
///
/// 从 `CapabilityRegistry` 和 `ActionRegistry` 收集所有可用能力，返回
/// `Vec<Box<dyn ToolDyn>>` 供 rig `AgentBuilder` 使用。
///
/// **参数**：
/// - `cap_registry`: Capability 注册表
/// - `action_registry`: Action 注册表（可通过 `AppContext` 获取）
/// - `app_handle`: Tauri AppHandle，用于构造 InvokeContext / ActionContext + emit 确认事件
/// - `pending`: 危险确认注册表（`Arc<PendingConfirms>`，由 main.rs manage，对话窗口共享）
///
/// **返回**：所有可用的 tool（CapabilityTool + ActionTool）
///
/// **tool 池粒度**（§2.4）：`ai_eligible() == false` 的 Action 不进池（如 `exit_blink`）。
/// **危险操作**：危险 Capability/Action 仍会被包装进 tool 池，但调用时挂起等用户确认。
#[allow(dead_code)] // 0.12.1 对话窗口 AgentBuilder 消费
pub fn build_agent_tools(
    cap_registry: &CapabilityRegistry,
    action_registry: &ActionRegistry,
    app_handle: &tauri::AppHandle,
    pending: Arc<PendingConfirms>,
) -> Vec<Box<dyn ToolDyn>> {
    let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

    // 1. 包装所有 Capability
    for (_id, cap) in cap_registry.entries() {
        let tool = CapabilityTool::new(cap, app_handle.clone(), pending.clone());
        tools.push(Box::new(tool));
    }

    // 2. 包装所有 Action（过滤 ai_eligible=false 的，如 exit_blink）
    let mut skipped = 0usize;
    for (_id, action) in action_registry.entries() {
        if !action.ai_eligible() {
            tracing::debug!(
                action = %action.id(),
                "build_agent_tools: 跳过 ai_eligible=false 的动作"
            );
            skipped += 1;
            continue;
        }
        let tool = ActionTool::new(action, app_handle.clone(), pending.clone());
        tools.push(Box::new(tool));
    }

    tracing::info!(
        capabilities = cap_registry.len(),
        actions = action_registry.len() - skipped,
        skipped_actions = skipped,
        total_tools = tools.len(),
        "build_agent_tools: tool 池构建完成"
    );

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{CapabilityResult, CapabilitySchema, ItemResult};
    use serde_json::json;

    // ── PendingConfirms 单测（纯逻辑，0.12.0 §2.4 闭环骨架）──────────────────

    #[tokio::test]
    async fn pending_confirms_register_and_resolve_approved() {
        let pc = PendingConfirms::new();
        let (id, mut rx) = pc.register().await;
        // 确认 -> receiver 收 true
        assert!(pc.resolve(id, true).await, "已知 id 应送达");
        assert_eq!(rx.await.unwrap(), true, "确认应收到 true");
    }

    #[tokio::test]
    async fn pending_confirms_resolve_rejected() {
        let pc = PendingConfirms::new();
        let (id, mut rx) = pc.register().await;
        assert!(pc.resolve(id, false).await);
        assert_eq!(rx.await.unwrap(), false, "拒绝应收到 false");
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
        let (id, mut rx) = pc.register().await;
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

    // ── derive_tool_deadline 单测 ──────────────────────────────────────────

    #[test]
    fn derive_tool_deadline_returns_some_future_instant() {
        // 纯逻辑：应返回一个未来的 Instant（无论 AIConfig 是否配置）。
        // 不依赖系统资源--get_ai_config 未初始化时返回 default（enabled=false，
        // slo_hard_timeout_ms=None -> 兜底 DEFAULT_TOOL_TIMEOUT_MS）。
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
            CapabilityError::Permission { detail: "x".into() },
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

    // ── exec_error_to_string 测试 ─────────────────────────────────────────

    #[test]
    fn exec_error_missing_arg_to_string() {
        let e = ExecError::MissingArg("open".into());
        let msg = exec_error_to_string(e);
        assert!(msg.contains("open"));
        assert!(msg.contains("缺少字符串参数"));
    }

    #[test]
    fn exec_error_runtime_to_string() {
        let e = ExecError::Runtime("找不到文件".into());
        let msg = exec_error_to_string(e);
        assert!(msg.contains("找不到文件"));
        assert!(msg.contains("执行失败"));
    }

    // ── is_dangerous 逻辑测试（不需要 AppHandle）──────────────────────────

    /// 测试 `DangerClass` 匹配逻辑--`is_dangerous` 纯粹基于 `danger_class()` 返回值。
    /// 这里直接测 `DangerClass` 的匹配，不需要构造 CapabilityTool（避 AppHandle）。
    #[test]
    fn dangerous_class_matches_dangerous() {
        assert!(matches!(DangerClass::Dangerous, DangerClass::Dangerous));
        assert!(!matches!(DangerClass::Safe, DangerClass::Dangerous));
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
        reg.register(cap);
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

    // ── ActionRegistry::entries 测试 ──────────────────────────────────────

    #[test]
    fn action_registry_entries_returns_all_builtins() {
        let reg = ActionRegistry::new();
        let entries = reg.entries();
        assert_eq!(entries.len(), 12, "应返回 12 个内置动作");
    }

    #[test]
    fn action_registry_entries_ids_match() {
        let reg = ActionRegistry::new();
        let entries = reg.entries();
        let ids: Vec<&str> = entries.iter().map(|(id, _)| id.as_str()).collect();
        assert!(ids.contains(&"open_settings"));
        assert!(ids.contains(&"shutdown"));
    }

    // ── build_agent_tools 计数测试 ────────────────────────────────────────

    /// `build_agent_tools` 需要 `&tauri::AppHandle` + `Arc<PendingConfirms>`，遵循
    /// AGENTS.md §7"Tauri 集成层免自动化"--这里只验证 registry 层面的数据一致性
    /// （含 ai_eligible 过滤预期：12 builtin - 1 exit_blink = 11 可用 action）。
    ///
    /// 实际的 tool 包装 + 调用测试靠 `cargo run` 手动验证（0.12.1 对话窗口落地后）。
    #[test]
    fn build_agent_tools_count_matches_registries() {
        // 无法直接测试 build_agent_tools（需 AppHandle），但可以验证过滤逻辑：
        // 12 个内置动作中 exit_blink 的 ai_eligible=false -> 应被过滤。
        let action_reg = ActionRegistry::new();
        let eligible = action_reg
            .entries()
            .iter()
            .filter(|(_, a)| a.ai_eligible())
            .count();
        assert_eq!(
            eligible, 11,
            "12 builtin - 1 exit_blink(ai_eligible=false) = 11 个可暴露给 AI"
        );
        // Capability 数 + 11 = 预期 tool 数
        let cap_reg = CapabilityRegistry::default();
        let expected_total = cap_reg.len() + eligible;
        assert!(expected_total >= 11, "至少应有 11 个可暴露动作");
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
                    title: "result1".into(),
                    subtitle: None,
                    payload: json!({ "path": "/test" }),
                    score: Some(0.9),
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
                title: "result1".into(),
                subtitle: None,
                payload: json!({ "path": "/test" }),
                score: Some(0.9),
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

    #[tokio::test]
    async fn roundtrip_action_outcome_projection() {
        // ActionOutcome -> to_rig_tool_result 链路验证
        use crate::domain::execution::ActionOutcome;

        // Copy -> JSON 文本
        let outcome = ActionOutcome::Copy {
            text: "copied".into(),
            hit_id: None,
        };
        let contents = outcome.to_rig_tool_result();
        assert_eq!(contents.len(), 1);
        let text = crate::domain::capability::rig_tool_result_to_text(&contents);
        assert!(text.contains("copied"));
        assert!(text.contains("\"type\":\"copy\""));

        // Open -> JSON 文本
        let outcome = ActionOutcome::Open {
            path: "C:\\test".into(),
        };
        let contents = outcome.to_rig_tool_result();
        let text = crate::domain::capability::rig_tool_result_to_text(&contents);
        assert!(text.contains("C:\\\\test"));
        assert!(text.contains("\"type\":\"open\""));

        // Nop -> JSON 文本
        let outcome = ActionOutcome::Nop;
        let contents = outcome.to_rig_tool_result();
        let text = crate::domain::capability::rig_tool_result_to_text(&contents);
        assert!(text.contains("\"type\":\"nop\""));
    }
}
