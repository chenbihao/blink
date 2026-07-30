//! 领域环境抽象（0.14.6 §2.2）—— domain 层与 Tauri 框架解耦的统一接口。
//!
//! **动机**：domain 层应框架无关，但多个文件直接 `use tauri::` 通过 `AppHandle`
//! emit 事件 / 取 managed state / 调窗口操作。本 trait 把这三类操作抽象为
//! `DomainEnv`，domain 只依赖此 trait，app 层提供 `TauriDomainEnv` 实现。
//!
//! **设计权衡**：
//! - `emit` / `emit_to` 接收 `serde_json::Value`（非泛型 `S: Serialize`）以保证对象安全
//! - 状态访问返回 `&Arc<T>`（引用而非 clone），靠 `OnceLock` 内部存储支撑生命周期
//! - 窗口操作仅 chord action 消费（show_chat / hide / screenshot overlay 等）
//! - `wait_frame_after_hide` 是唯一 async 方法，用 `async_trait` 标注

use std::sync::Arc;

use crate::domain::ai::chat_service::ChatService;
use crate::domain::capability::CapabilityRegistry;
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
use crate::infra::data::pools::DbPools;
use crate::infra::platform::screenshot::ScreenCaptureMeta;

/// Capability 可见的最小运行时环境。
///
/// 该接口刻意不包含事件、窗口和进程控制。AI 即使拿到某个 Capability，
/// 也只能通过这里声明的数据/服务依赖工作，不能借 `InvokeContext` 越权进入 Action 域。
pub trait CapabilityEnv: Send + Sync {
    /// SQLite 四库连接池。
    fn db_pools(&self) -> &DbPools;

    /// 插件引擎（CLI/MCP 最小运行时可能不构造）。
    fn plugin_engine(&self) -> Option<&Arc<PluginEngine>>;

    /// 搜索服务（多路引擎 + 路由融合）。
    ///
    /// CLI/MCP 的最小运行时可能不构造完整搜索栈，因此显式返回 `Option`，
    /// 由 Capability 转成可恢复错误，禁止在环境 getter 内 panic。
    fn search_service(&self) -> Option<&Arc<SearchService>>;
}

/// 领域环境——domain 层与 Tauri 之间的抽象边界。
///
/// domain 代码通过此 trait emit 事件、访问 managed state、操作窗口，
/// 不再直接 `use tauri::`。app 层 `TauriDomainEnv` 负责桥接到 Tauri 运行时。
#[async_trait::async_trait]
pub trait DomainEnv: CapabilityEnv {
    /// 降权为 Capability 可见的最小环境。
    fn capability_env(&self) -> &dyn CapabilityEnv;

    // ── 事件发射（替代 tauri::Emitter）──────────────────────────────────

    /// 广播事件到所有前端窗口。
    fn emit(&self, event: &str, payload: serde_json::Value) -> Result<(), String>;

    /// 定向发送事件到指定 label 的窗口（如 `"chat"`）。
    fn emit_to(&self, target: &str, event: &str, payload: serde_json::Value) -> Result<(), String>;

    // ── 状态访问（替代 tauri::Manager state::<T>()）────────────────────

    /// Capability 注册表（开放能力，AI tool 池来源）。
    fn cap_registry(&self) -> &Arc<CapabilityRegistry>;

    /// 对话窗口服务（可能尚未构造，返回 Option）。
    fn chat_service(&self) -> Option<&Arc<ChatService>>;

    // ── 窗口操作（chord action 专用）──────────────────────────────────

    /// 显示对话窗口。
    fn show_chat_window(&self) -> Result<(), String>;

    /// 隐藏主窗口。
    fn hide_main_window(&self, reason: &str);

    /// 隐藏主窗口用于截图（cloak 路径，无 Win11 fade 动画）。
    fn hide_for_screenshot(&self);

    /// 撤销截图用 cloak。
    fn unhide_after_screenshot(&self);

    /// 显示截图选区 overlay 窗口。
    fn show_screenshot_overlay(&self, meta: &ScreenCaptureMeta) -> Result<(), String>;

    /// 显示 + 聚焦主窗口。
    fn invoke_main_window(&self);

    /// 打开设置窗口。
    fn open_settings(&self);

    /// 退出应用进程。
    fn exit_app(&self);

    /// 等待 DWM 完成一次不含主窗的新合成（截图前用）。
    async fn wait_frame_after_hide(&self);
}

// ── 便捷 helper ──────────────────────────────────────────────────────────

/// 将任意 `Serialize` payload 序列化为 `Value` 后 emit。
///
/// 因 `DomainEnv::emit` 接收 `serde_json::Value`（保证对象安全），
/// 调用方需先用此函数转换 payload。
pub fn emit_serialized(
    env: &dyn DomainEnv,
    event: &str,
    payload: &impl serde::Serialize,
) -> Result<(), String> {
    let value = serde_json::to_value(payload).map_err(|e| e.to_string())?;
    env.emit(event, value)
}

/// 将任意 `Serialize` payload 序列化为 `Value` 后定向 emit。
#[allow(dead_code)]
pub fn emit_to_serialized(
    env: &dyn DomainEnv,
    target: &str,
    event: &str,
    payload: &impl serde::Serialize,
) -> Result<(), String> {
    let value = serde_json::to_value(payload).map_err(|e| e.to_string())?;
    env.emit_to(target, event, value)
}
