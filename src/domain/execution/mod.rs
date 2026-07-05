//! Execution 域（0.8.6 §8.1.1）—— 动作的统一执行入口。
//!
//! **动机**：0.8.0 ~ 0.8.5 积累了四份"动作"概念——`SearchAction`（引擎内部模型）、
//! `BuiltinActionKind`（内置动作分派 tag）、`PluginAction`（插件 wire protocol）、
//! `ChordAction` trait（Chord 动作契约）。本质是同一概念的四种投影。0.8.6 抽
//! `Action` trait + `ActionOutcome`，把执行入口物理归一到本模块。
//!
//! **四域约束**：Execution 域是信任边界最末端。`ActionOutcome` 描述副作用意图，
//! 真正执行（系统调用 / IPC / 前端通知）由 command 层调 `Action::execute` 后按
//! `ActionOutcome` 分派。
//!
//! **前端契约不变**：前端仍接收 `Action { kind, payload }` 形状（`search/mod.rs`），
//! 本模块是后端内部的执行抽象——外部 JSON 零变化。

mod builtin;
mod registry;

pub use registry::ActionRegistry;
// builtin 动作 structs 通过 registry 间接使用，不需要 re-export
#[allow(unused_imports)]
pub(crate) use builtin::*;

use serde_json::Value;

/// 动作执行上下文（0.8.6 §8.1.1）。
///
/// 包含执行所需的所有环境信息。不是所有字段都被所有动作消费——
/// `app_handle` 仅 `Emit` / 需要 Tauri 运行时的动作使用；`arg` 仅参数化动作使用。
pub struct ActionContext<'a> {
    pub app_handle: &'a tauri::AppHandle,
    /// 动作参数。参数化动作（OpenUrl / OpenPath / RevealInExplorer）从此取值。
    /// 无参动作忽略。
    pub arg: Option<Value>,
}

impl<'a> ActionContext<'a> {
    pub fn new(app_handle: &'a tauri::AppHandle, arg: Option<Value>) -> Self {
        Self { app_handle, arg }
    }

    /// 从 `arg` 抽出非空字符串——参数化动作专用。
    pub fn arg_as_str(&self, action_name: &str) -> Result<String, ExecError> {
        self.arg
            .as_ref()
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| ExecError::MissingArg(action_name.to_string()))
    }
}

/// 动作执行结果（副作用意图）。
///
/// **不是**最终返回值——command 层拿到 `ActionOutcome` 后按类型分派：
/// - `Open` → `open::that(path)`
/// - `Copy` → 写剪贴板 + 可选 hit 回写
/// - `Emit` → `app.emit(event, payload)`
/// - `Nop` → 无操作
///
/// 设计为 enum 而非 `Box<dyn FnOnce>` 的原因：可序列化、可日志、command 层分派清晰。
#[derive(Debug, Clone)]
pub enum ActionOutcome {
    /// 复制到剪贴板（计算结果 / 插件 Copy / 剪贴板历史）。
    /// `hit_id` 是命中回写通道（0.8.5 §6.4）——前端复制成功后 `record_clipboard_hit` 频率加权。
    #[allow(dead_code)] // 0.9 AI / 插件 adapter 消费；当前 Copy 走 SearchAction 路径
    Copy { text: String, hit_id: Option<String> },
    /// 打开路径 / URL / 应用。
    #[allow(dead_code)] // Chord action 预留；当前 Open 走 SearchAction 路径
    Open { path: String },
    /// 向前端发事件（Chord 的 fill-query / show-ball 等副作用）。
    Emit {
        event: String,
        payload: serde_json::Value,
    },
    /// 纯展示项，无副作用。
    Nop,
}

/// 动作执行错误。
#[derive(Debug)]
pub enum ExecError {
    /// 参数化动作缺少参数。
    MissingArg(String),
    /// 执行过程中的错误（打开失败、系统调用失败等）。
    Runtime(String),
}

impl std::fmt::Display for ExecError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExecError::MissingArg(action) => write!(f, "{action}: 缺少字符串参数"),
            ExecError::Runtime(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for ExecError {}

/// 统一动作 trait（0.8.6 §8.1.1 / §8.2.4）。
///
/// 一切副作用的统一入口。三种来源实现此 trait：
/// - **Builtin**：12 个内置动作（`builtin/*.rs`）
/// - **Plugin**：插件 wire protocol 的 adapter（`adapter.rs`，0.8.6 Phase 2）
/// - **Chord**：`ChordAction: Action` supertrait（`chord/mod.rs` 改造）
///
/// 0.9 AI Provider 可产 `ChatAction` / `RunAgentAction`，直接 `impl Action`。
#[async_trait::async_trait]
pub trait Action: Send + Sync {
    /// 唯一标识（`BuiltinActionRegistry` 的 key，如 `"open_settings"`）。
    fn id(&self) -> &str;

    /// 主显示名（走 `LocalizableText`，0.8.6 §8.2.4 i18n）。
    fn title(&self) -> &crate::domain::plugin::LocalizableText;

    /// 副显示名。
    fn subtitle(&self) -> &crate::domain::plugin::LocalizableText;

    /// 执行动作，返回副作用意图。
    async fn execute(&self, cx: &ActionContext<'_>) -> Result<ActionOutcome, ExecError>;
}
