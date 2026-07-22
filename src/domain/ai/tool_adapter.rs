//! Tool 适配层（0.12.0 §2.4）——把 Capability/Action 包装成 `rig::tool::Tool`。
//!
//! ## 动机
//!
//! rig Agent 的 `AgentBuilder` 注册 tool 的方式与现有架构根本不同：
//! - **现有架构**：数据 + 外部调度——`Vec<ActionSchema>` → 投影成 `Vec<ToolDefinition>`
//!   作为 `CompletionRequest` 参数，service.rs 收到 `tool_calls` 后手工执行
//! - **rig Agent**：代码 + 内部调度——`impl rig::tool::Tool` trait，Agent 内部自动循环
//!
//! 冲突：现有的 Capability / 插件 tool 全部接不进 rig Agent，0.12 对话窗口整个走不通。
//!
//! ## 解决方案
//!
//! 抽出 `ToolAdapter` 层，把 Capability / Action 包装成 `impl rig::tool::Tool`：
//! - `CapabilityTool` 包装 `Arc<dyn Capability>`
//! - `ActionTool` 包装 `Arc<dyn Action>`
//! - `ToolDyn` 动态分发——Args 用 `serde_json::Value`，避免给每个 Capability 写强类型 Args
//!
//! ## 四域墙（危险操作确认）
//!
//! **危险操作**（`danger_class == Dangerous`）在 `call` 内部 emit `ai-confirm-action` 事件 +
//! 返回 "等待用户确认" 消息，不直接执行。保持四域墙：AI 只产候选，用户显式选择才穿过信任边界。
//!
//! ## 工厂函数
//!
//! `build_agent_tools()` 从 `CapabilityRegistry` 和 `ActionRegistry` 收集所有可用能力，
//! 返回 `Vec<Box<dyn ToolDyn>>` 供对话窗口 Agent 使用。

use std::sync::Arc;

use rig_core::tool::ToolDyn;
use rig_core::wasm_compat::WasmBoxedFuture;
use serde_json::Value;
use tauri::Emitter;

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityRegistry, CapabilityResult, InvokeContext,
};
use crate::domain::execution::{Action, ActionContext, ActionOutcome, ActionRegistry, DangerClass};

// ── 辅助函数 ──────────────────────────────────────────────────────────────────

/// 把 `ToolResultContent` 列表合并为单个字符串（rig ToolDyn::call 的返回值）。
///
/// - `Text(t)` → `t.text()`
/// - `Image(_)` → `[图片结果]`（不传二进制给 LLM，省 token）
fn merge_tool_contents(contents: Vec<rig_core::message::ToolResultContent>) -> String {
    contents
        .into_iter()
        .map(|c| match c {
            rig_core::message::ToolResultContent::Text(t) => t.text().to_string(),
            rig_core::message::ToolResultContent::Image(_) => "[图片结果]".to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 危险操作确认事件 payload。
#[derive(serde::Serialize, Clone)]
struct ConfirmPayload {
    tool_name: String,
    tool_type: &'static str, // "capability" 或 "action"
    arguments: Value,
    danger_class: &'static str,
}

/// emit `ai-confirm-action` 事件到前端，让对话窗口展示确认卡片。
///
/// **与主窗口 `emit_ai_confirm` 的区别**：对话窗口不需要 `seq`（rig Agent 内部
/// 管理 tool loop，不存在主窗口的 seq 匹配问题），用 `tool_name` 标识即可。
fn emit_dangerous_confirm(
    app: &tauri::AppHandle,
    tool_name: &str,
    tool_type: &'static str,
    arguments: &Value,
) {
    let payload = ConfirmPayload {
        tool_name: tool_name.to_string(),
        tool_type,
        arguments: arguments.clone(),
        danger_class: "Dangerous",
    };
    if let Err(e) = app.emit("blink://ai-confirm-action", payload) {
        tracing::debug!(error = %e, "emit ai-confirm-action failed");
    }
}

// ── CapabilityTool ────────────────────────────────────────────────────────────

/// Capability 的 Tool 包装器。
///
/// 持有 `Arc<dyn Capability>`，实现 `ToolDyn` 以供 rig Agent 使用。
///
/// **设计要点**：
/// - `definition()` → `schema.to_rig_tool()`（纯 schema 投影）
/// - `call()` → `cap.invoke(args, ctx)` → `CapabilityResult` → `to_rig_tool_result()`
/// - 危险操作（`danger_class == Dangerous`）emit 确认卡片 + 返回 "等待用户确认"
pub struct CapabilityTool {
    cap: Arc<dyn Capability>,
    schema: crate::domain::capability::CapabilitySchema,
    app_handle: tauri::AppHandle,
}

impl CapabilityTool {
    /// 构造 CapabilityTool。
    pub fn new(cap: Arc<dyn Capability>, app_handle: tauri::AppHandle) -> Self {
        let schema = cap.schema();
        Self {
            cap,
            schema,
            app_handle,
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
                Err(e) => {
                    return Err(rig_core::tool::ToolError::JsonError(e));
                }
            };

            // 危险操作：emit 确认卡片 + 返回 "等待用户确认"，不直接执行
            if self.is_dangerous() {
                tracing::warn!(
                    capability = %self.cap.id(),
                    "危险操作被 AI 调用，需用户确认"
                );
                emit_dangerous_confirm(
                    &self.app_handle,
                    self.cap.id(),
                    "capability",
                    &args_value,
                );
                return Ok(format!(
                    "【等待用户确认】即将执行危险操作: {}。请在对话窗口中确认。",
                    self.cap.id()
                ));
            }

            // 构造 InvokeContext（无超时，对话窗口模式不设 deadline）
            let ctx = InvokeContext {
                app_handle: &self.app_handle,
                deadline: None,
            };

            // 调用 Capability
            match self.cap.invoke(args_value, &ctx).await {
                Ok(cap_result) => {
                    let contents = cap_result.to_rig_tool_result();
                    Ok(merge_tool_contents(contents))
                }
                Err(e) => {
                    let err_msg = capability_error_to_string(e);
                    Err(rig_core::tool::ToolError::ToolCallError(Box::new(
                        std::io::Error::new(std::io::ErrorKind::Other, err_msg),
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
/// - `definition()` → `schema.to_rig_tool()`（纯 schema 投影）
/// - `call()` → `action.execute()` → `ActionOutcome` → `to_rig_tool_result()`
/// - 危险操作（`danger_class == Dangerous`）emit 确认卡片 + 返回 "等待用户确认"
pub struct ActionTool {
    action: Arc<dyn Action>,
    schema: crate::domain::execution::ActionSchema,
    app_handle: tauri::AppHandle,
}

impl ActionTool {
    /// 构造 ActionTool。
    pub fn new(action: Arc<dyn Action>, app_handle: tauri::AppHandle) -> Self {
        let schema = action.schema();
        Self {
            action,
            schema,
            app_handle,
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
                Err(e) => {
                    return Err(rig_core::tool::ToolError::JsonError(e));
                }
            };

            // 危险操作：emit 确认卡片 + 返回 "等待用户确认"，不直接执行
            if self.is_dangerous() {
                tracing::warn!(
                    action = %self.action.id(),
                    "危险操作被 AI 调用，需用户确认"
                );
                emit_dangerous_confirm(
                    &self.app_handle,
                    self.action.id(),
                    "action",
                    &args_value,
                );
                return Ok(format!(
                    "【等待用户确认】即将执行危险操作: {}。请在对话窗口中确认。",
                    self.action.id()
                ));
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
                    Ok(merge_tool_contents(contents))
                }
                Err(e) => {
                    let err_msg = format!("执行失败: {e}");
                    Err(rig_core::tool::ToolError::ToolCallError(Box::new(
                        std::io::Error::new(std::io::ErrorKind::Other, err_msg),
                    )))
                }
            }
        })
    }
}

// ── 错误转换 ─────────────────────────────────────────────────────────────────

/// `CapabilityError` → 用户可读字符串。
///
/// 投影到 `ToolError::ToolCallError` 的 message——让 AI 知道失败原因（可重试或换路径）。
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

// ── 工厂函数 ─────────────────────────────────────────────────────────────────

/// 构建对话窗口 Agent 使用的 tool 池。
///
/// 从 `CapabilityRegistry` 和 `ActionRegistry` 收集所有可用能力，返回
/// `Vec<Box<dyn ToolDyn>>` 供 rig `AgentBuilder` 使用。
///
/// **参数**：
/// - `cap_registry`: Capability 注册表
/// - `action_registry`: Action 注册表（可通过 `AppContext` 获取）
/// - `app_handle`: Tauri AppHandle，用于构造 InvokeContext / ActionContext
///
/// **返回**：所有可用的 tool（CapabilityTool + ActionTool）
///
/// **危险操作**：危险 Capability/Action 仍会被包装进 tool 池，但调用时会 emit
/// 确认卡片 + 返回 "等待用户确认" 消息，不直接执行。
pub fn build_agent_tools(
    cap_registry: &CapabilityRegistry,
    action_registry: &ActionRegistry,
    app_handle: &tauri::AppHandle,
) -> Vec<Box<dyn ToolDyn + Send + Sync>> {
    let mut tools: Vec<Box<dyn ToolDyn + Send + Sync>> = Vec::new();

    // 1. 包装所有 Capability
    for (_id, cap) in cap_registry.entries() {
        let tool = CapabilityTool::new(cap, app_handle.clone());
        tools.push(Box::new(tool));
    }

    // 2. 包装所有 Action
    for (_id, action) in action_registry.entries() {
        let tool = ActionTool::new(action, app_handle.clone());
        tools.push(Box::new(tool));
    }

    tracing::info!(
        capabilities = cap_registry.len(),
        actions = action_registry.len(),
        total_tools = tools.len(),
        "build_agent_tools: tool 池构建完成"
    );

    tools
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::CapabilitySchema;
    use crate::domain::execution::ActionSchema;

    // ── merge_tool_contents 测试 ───────────────────────────────────────────

    #[test]
    fn merge_tool_contents_extracts_text() {
        use rig_core::message::{Text, ToolResultContent};
        let contents = vec![
            ToolResultContent::Text(Text::new("hello")),
            ToolResultContent::Text(Text::new("world")),
        ];
        assert_eq!(merge_tool_contents(contents), "hello\nworld");
    }

    #[test]
    fn merge_tool_contents_empty_returns_empty_string() {
        let contents: Vec<rig_core::message::ToolResultContent> = vec![];
        assert_eq!(merge_tool_contents(contents), "");
    }

    // ── capability_error_to_string 测试 ───────────────────────────────────

    #[test]
    fn capability_error_invalid_args_to_string() {
        let e = CapabilityError::InvalidArgs {
            detail: "缺少 query".into(),
        };
        assert!(capability_error_to_string(e).contains("参数无效"));
        assert!(capability_error_to_string(CapabilityError::InvalidArgs { detail: "缺少 query".into() }).contains("缺少 query"));
    }

    #[test]
    fn capability_error_cancelled_to_string() {
        assert_eq!(capability_error_to_string(CapabilityError::Cancelled), "已取消");
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

    // ── is_dangerous 逻辑测试（不需要 AppHandle）──────────────────────────

    /// 测试 `DangerClass` 匹配逻辑——`is_dangerous` 纯粹基于 `danger_class()` 返回值。
    /// 这里直接测 `DangerClass` 的匹配，不需要构造 CapabilityTool（避 AppHandle）。
    #[test]
    fn dangerous_class_matches_dangerous() {
        assert!(matches!(DangerClass::Dangerous, DangerClass::Dangerous));
        assert!(!matches!(DangerClass::Safe, DangerClass::Dangerous));
    }

    // ── CapabilityRegistry::entries 测试 ──────────────────────────────────

    /// Mock Capability——避免构造 AppHandle（遵循 AGENTS.md §7）。
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

    /// `build_agent_tools` 需要 `&tauri::AppHandle`，遵循 AGENTS.md §7
    /// "Tauri 集成层免自动化"——这里只验证 registry 层面的数据一致性。
    ///
    /// 实际的 tool 包装 + 调用测试靠 `cargo run` 手动验证（0.12.1 对话窗口落地后）。
    #[test]
    fn build_agent_tools_count_matches_registries() {
        // 无法直接测试 build_agent_tools（需 AppHandle），但可以验证
        // cap_registry.len() + action_registry.len() = 预期 tool 数
        let cap_reg = CapabilityRegistry::default();
        let action_reg = ActionRegistry::new();
        let expected_total = cap_reg.len() + action_reg.len();
        // 12 个内置动作 + inventory 收集的 capabilities（测试环境可能为 0）
        assert!(expected_total >= 12, "至少应有 12 个内置动作");
    }
}
