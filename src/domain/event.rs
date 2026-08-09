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
//! - `wait_frame_after_hide` 和 `create_sticky_and_show` 是 async 方法，用 `async_trait` 标注
//! - 0.19.3：`CapabilityEnv` 新增便签窗口操作方法（`sticky_service` / `create_sticky_and_show`），
//!   使 Capability 能触达便签窗口副作用，不直接暴露 AppHandle

use std::sync::Arc;

use crate::domain::ai::chat_service::ChatService;
use crate::domain::capability::{CapabilityRegistry, ImageStash};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
use crate::domain::sticky::StickyService;
use crate::infra::data::pools::DbPools;
use crate::infra::platform::screenshot::ScreenCaptureMeta;

/// Capability 可见的最小运行时环境。
///
/// 该接口刻意不包含事件、窗口和进程控制。AI 即使拿到某个 Capability，
/// 也只能通过这里声明的数据/服务依赖工作，不能借 `InvokeContext` 越权进入 Action 域。
///
/// **0.19.3**：新增便签窗口操作方法（`sticky_service` / `create_sticky_and_show`），
/// 使便签能力化 Capability 能触达窗口副作用。这与 `open_url` 等 Capability 产生
/// OS 级窗口副作用（开浏览器）同属一类——"可逆窗口副作用"，不违反 §A4 "不碰 UI" 边界
/// （不直接操作前端 DOM/事件流）。参见 phase 0.19 §3.2/§3.3 决策。
#[async_trait::async_trait]
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

    // ── 便签窗口操作（0.19.3 便签能力化桥接）──────────────────────────

    /// 便签服务——供 `list_sticky` / `set_sticky_geometry` cap 直接调
    /// `list_notes()` / `update_geometry()` 等 DB 操作。
    ///
    /// 返回 `Option`——CLI/MCP 最小运行时可能不构造完整服务栈。
    fn sticky_service(&self) -> Option<&Arc<StickyService>>;

    /// 创建便签并显示桌面窗口（0.19.3）。
    ///
    /// `content` 为便签正文；`x`/`y`/`w`/`h` 为可选位置尺寸（物理像素），
    /// `None` 则居中到当前前台窗口所在显示器（复用 `center_of_active_monitor`）。
    ///
    /// 返回创建的便签 id。实现负责持久化 + 创建桌面窗口 + emit 创建事件。
    async fn create_sticky_and_show(
        &self,
        content: &str,
        x: Option<i32>,
        y: Option<i32>,
        w: Option<i32>,
        h: Option<i32>,
    ) -> Result<String, String>;

    // ── 图片暂存（0.19.4 ImageStash 引用闭环）──────────────────────────

    /// 进程级图片暂存——投影层把 image/* Blob 字节移入 stash 并生成 `image_ref`，
    /// 后续 tool 只传 ref，不把图片编码进 LLM 上下文。
    ///
    /// 返回 `None`——CLI/MCP 最小运行时不构造，投影层降级为摘要。
    fn image_stash(&self) -> Option<&Arc<ImageStash>>;

    // ── pin 窗口操作（0.19.3 pin 能力化桥接）──────────────────────────

    /// 显示钉图窗口（0.19.3）。
    ///
    /// `png_bytes` 为 PNG 图片字节；`x`/`y` 为图片左上的物理像素坐标
    /// （窗口会在此基础上偏移 `-PIN_PAD` 给发光区留空间）。
    ///
    /// `show_translating` 对 AI pin 场景无意义，桥接层固定传 `false`。
    /// 返回 `Err` 表示窗口创建/更新失败。
    fn show_pin_window(&self, png_bytes: Vec<u8>, x: i32, y: i32) -> Result<(), String>;
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
    ///
    /// CLI/MCP 的最小运行时可能不构造完整 Capability 栈，因此显式返回 `Option`，
    /// 由调用方转成可恢复错误或降级为空 tool 池，禁止在环境 getter 内 panic。
    #[allow(dead_code)] // build_agent_tools 直接传参消费，测试验证最小运行时
    fn cap_registry(&self) -> Option<&Arc<CapabilityRegistry>>;

    /// 对话窗口服务（可能尚未构造，返回 Option）。
    fn chat_service(&self) -> Option<&Arc<ChatService>>;

    // ── 窗口操作（chord action 专用）──────────────────────────────────

    /// 显示对话窗口。
    ///
    /// 0.16.2：`initial_text` 非空时，窗口显示后通过 `blink://chat-prefill` 事件
    /// 把文本推给 chat 前端填充输入框（仅填充不自动发送）。
    fn show_chat_window(&self, initial_text: Option<&str>) -> Result<(), String>;

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

    /// 显示便签管理窗口（0.16.10）。
    fn show_sticky_manager(&self) -> Result<(), String>;

    /// 打开内容编辑器窗口（0.16.9）。
    ///
    /// `body` 为编辑器初始文本，`origin` 标识来源（"chord" / "clipboard" / "sticky"），
    /// `origin_ref` 为原实体 id（剪贴板记录 id 或便签 id），`save_policy` 为保存策略
    /// （"clipboard_new" / "sticky_update"）。实现负责存入 PendingEditorPayload 并创建窗口。
    fn show_content_editor(
        &self,
        body: &str,
        title: Option<&str>,
        origin: &str,
        origin_ref: Option<&str>,
        save_policy: &str,
    ) -> Result<(), String>;

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
