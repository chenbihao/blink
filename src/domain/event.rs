//! 领域环境抽象（0.14.6 §2.2 / 0.21.14 最小 port 拆分）—— domain 层与 Tauri 框架解耦。
//!
//! **0.21.14 变更**：
//! - 旧巨型 `DomainEnv` trait 已删除，按消费者拆为最小 port。
//! - **`EventPort`**：只提供 `emit` / `emit_to`，领域事件发射专用。
//! - **`CapabilityEnv`**：Capability 可见的最小运行时（DB、plugin、search、sticky 等）。
//! - **`SurfacePort`**：GUI 窗口操作（定义在 `capability/policy.rs`）。
//! - 消费者只注入自身需要的 port 组合，不存在包含所有方法的 God Interface。
//!
//! **设计权衡**：
//! - `emit` / `emit_to` 接收 `serde_json::Value`（非泛型 `S: Serialize`）以保证对象安全
//! - 状态访问返回 `&Arc<T>`（引用而非 clone），靠 `OnceLock` 内部存储支撑生命周期
//! - 0.19.3：`CapabilityEnv` 新增便签窗口操作方法，使 Capability 能触达便签窗口副作用

use std::sync::Arc;

use crate::domain::capability::ImageStash;
use crate::domain::config::{ManagedSetting, ManagedSettingUpdate};
use crate::domain::plugin::PluginEngine;
use crate::domain::search::SearchService;
use crate::domain::sticky::{
    StickyChangeSource, StickyCloseOutcome, StickyColor, StickyNote, StickyService,
    StickyWorkflowError,
};
use crate::infra::data::pools::DbPools;

// ── EventPort ──────────────────────────────────────────────────────────────

/// 领域事件 port——只提供事件发射能力。
///
/// 消费者：SearchService、ChatService、CapabilityTool（危险确认弹窗）。
/// 不包含窗口操作、状态访问或进程控制。
pub trait EventPort: Send + Sync {
    /// 广播事件到所有前端窗口。
    fn emit(&self, event: &str, payload: serde_json::Value) -> Result<(), String>;

    /// 定向发送事件到指定 label 的窗口（如 `"chat"`）。
    fn emit_to(&self, target: &str, event: &str, payload: serde_json::Value) -> Result<(), String>;
}

/// 将任意 `Serialize` payload 序列化为 `Value` 后 emit。
///
/// 因 `EventPort::emit` 接收 `serde_json::Value`（保证对象安全），
/// 调用方需先用此函数转换 payload。
pub fn emit_serialized(
    port: &dyn EventPort,
    event: &str,
    payload: &impl serde::Serialize,
) -> Result<(), String> {
    let value = serde_json::to_value(payload).map_err(|e| e.to_string())?;
    port.emit(event, value)
}

// ── CapabilityEnv ───────────────────────────────────────────────────────────

/// Capability 可见的最小运行时环境。
///
/// 该接口刻意不包含事件发射、窗口和进程控制。AI 即使拿到某个 Capability，
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

    /// 列出 AI 可见的稳定设置白名单；不得返回底层 KV 或完整配置对象。
    async fn list_managed_settings(&self) -> Result<Vec<ManagedSetting>, String>;

    /// 字段级更新受控设置。`expected_old_value` 同时用于确认预览与并发保护。
    async fn update_managed_setting(
        &self,
        setting_id: &str,
        expected_old_value: serde_json::Value,
        new_value: serde_json::Value,
    ) -> Result<ManagedSettingUpdate, String>;

    // ── 便签窗口操作（0.19.3 便签能力化桥接）──────────────────────────

    /// 便签服务——供 `list_sticky` / `set_sticky_geometry` cap 直接调
    /// `list_notes()` / `update_geometry()` 等 DB 操作。
    ///
    /// 返回 `Option`——CLI/MCP 最小运行时可能不构造完整服务栈。
    fn sticky_service(&self) -> Option<&Arc<StickyService>>;

    /// 创建便签记录并广播创建事件；不负责显示窗口。
    async fn create_sticky_and_notify(
        &self,
        content: &str,
        color: StickyColor,
    ) -> Result<StickyNote, StickyWorkflowError>;

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

    /// 更新便签正文并广播变更事件。command 可不带 revision；Capability 必须带 revision。
    async fn update_sticky_content_and_notify(
        &self,
        sticky_id: &str,
        content: &str,
        expected_updated_at: Option<i64>,
        source: StickyChangeSource,
    ) -> Result<i64, StickyWorkflowError>;

    /// 设置便签桌面可见性、同步窗口并广播可见性事件。
    ///
    /// P0-1：返回新的 `updated_at`，前端 mutation queue 据此跟踪 revision。
    async fn set_sticky_visibility_and_notify(
        &self,
        sticky_id: &str,
        visible: bool,
    ) -> Result<i64, StickyWorkflowError>;

    /// 将便签移入废纸篓、隐藏对应窗口并广播回收事件。
    async fn trash_sticky_and_notify(&self, sticky_id: &str) -> Result<(), StickyWorkflowError>;

    /// 原子关闭便签（0.20.0）。
    ///
    /// 在同一后端工作流内完成 revision 校验、最终保存和 delete/trash 决策。
    /// 空内容 → 物理删除；非空 → 保存最终内容并移入回收站。
    /// 成功后隐藏窗口并广播对应事件（STICKY_DELETED 或 STICKY_TRASHED）。
    /// 失败时窗口不关闭，内容保留。
    async fn close_sticky_and_notify(
        &self,
        sticky_id: &str,
        final_content: &str,
        expected_updated_at: Option<i64>,
    ) -> Result<StickyCloseOutcome, StickyWorkflowError>;

    // ── 图片暂存（0.19.4 ImageStash 引用闭环）──────────────────────────

    /// 进程级图片暂存——投影层把 image/* Blob 字节移入 stash 并生成 `image_ref`，
    /// 后续 tool 只传 ref，不把图片编码进 LLM 上下文。
    ///
    /// 返回 `None`——CLI/MCP 最小运行时不构造，投影层降级为摘要。
    fn image_stash(&self) -> Option<&Arc<ImageStash>>;

    // ── pin 窗口操作（0.19.3 pin 能力化桥接）──────────────────────────

    /// 显示通用钉图窗口（0.19.6 command / Capability 共享语义）。
    ///
    /// `png_bytes` 为 PNG 图片字节；`x`/`y` 为可选的图片左上物理像素坐标。
    /// 任一坐标缺失时，按图片尺寸计算光标所在显示器的居中位置，再保留已给坐标。
    ///
    /// `show_translating` 对 AI pin 场景无意义，桥接层固定传 `false`。
    /// 返回最终使用的 `(x, y)`；`Err` 表示窗口创建/更新失败。
    fn show_pin_image(
        &self,
        png_bytes: Vec<u8>,
        x: Option<i32>,
        y: Option<i32>,
    ) -> Result<(i32, i32), String>;
}
