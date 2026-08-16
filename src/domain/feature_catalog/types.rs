//! FeatureCatalog 类型定义（0.21.4）。
//!
//! 所有类型均为 `Serialize` + `Clone`，供 Tauri command 直接返回前端。
//! `feature_id` / `capability_id` / `binding_id` 三类 id 分栏（§3.6）。

use serde::{Deserialize, Serialize};

// ── Feature 身份与分组 ───────────────────────────────────────────────────────

/// 功能分组（§5.5 第 1 条：六组 + 其他插件能力）。
///
/// 序列化为 snake_case 字符串供前端 i18n key 或直接渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureGroup {
    /// 应用/文件与链接
    AppsFilesLinks,
    /// 剪贴板与文本
    ClipboardText,
    /// 图片与颜色
    ImageColor,
    /// 便签与内容
    StickyContent,
    /// 窗口与系统
    WindowSystem,
    /// Blink 管理与诊断
    BlinkManagement,
    /// 无法归类的插件能力
    OtherPlugin,
}

impl FeatureGroup {
    /// 从 capability id 推断分组（仅用于 builtin capability 归类）。
    ///
    /// 插件 capability 不走此函数——插件走 `FeatureSource::Plugin` 并归入 `OtherPlugin`，
    /// 除非其 capability id 恰好匹配 builtin 分组规则（理论上不会发生，因为插件 id
    /// 带 `plugin_` 前缀）。
    pub fn infer_from_capability_id(cap_id: &str) -> Self {
        match cap_id {
            // 应用/文件与链接
            "open_url" | "open_path" | "reveal_in_explorer" | "search_apps" | "search_files" => {
                Self::AppsFilesLinks
            }

            // 剪贴板与文本
            "read_clipboard"
            | "write_clipboard"
            | "search_clipboard_history"
            | "read_clipboard_history_image"
            | "list_clipboard_images" => Self::ClipboardText,

            // 图片与颜色
            "screenshot"
            | "ocr_image"
            | "analyze_image_palette"
            | "start_region_capture"
            | "edit_clipboard_image" => Self::ImageColor,

            // 便签与内容
            "list_sticky"
            | "read_sticky"
            | "create_sticky"
            | "update_sticky"
            | "trash_sticky"
            | "set_sticky_geometry"
            | "set_sticky_visibility"
            | "pin_image"
            | "sticky_manager"
            | "start_content_editor" => Self::StickyContent,

            // 窗口与系统
            "list_windows" | "lock" | "shutdown" | "restart" | "sleep" => Self::WindowSystem,

            // Blink 管理与诊断
            "open_settings"
            | "open_logs"
            | "open_data_dir"
            | "exit_blink"
            | "clear_history"
            | "blink_print_debug_info"
            | "blink_debug_inithook"
            | "get_settings"
            | "update_setting"
            | "open_chat"
            | "open_clipboard_mode" => Self::BlinkManagement,

            _ => Self::OtherPlugin,
        }
    }

    /// 从 builtin descriptor id 推断分组。
    /// descriptor id 与 capability id 在 0.21.3 后一致（§5.4 第 1 条），
    /// 但 `open_url`/`open_path`/`reveal_in_explorer` 是参数化 descriptor，
    /// 归类与 capability 相同。
    pub fn infer_from_descriptor_id(desc_id: &str) -> Self {
        Self::infer_from_capability_id(desc_id)
    }
}

/// 功能来源——标识目录项从哪个体系聚合而来。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FeatureSource {
    /// 内置 descriptor（BuiltinEngine::ACTIONS）
    Builtin,
    /// Chord binding（ChordRegistry）
    Chord,
    /// 插件 Capability（PluginCapabilityAdapter）
    Plugin,
    /// 仅注册到 CapabilityRegistry 但无 descriptor/binding 的 builtin capability
    /// （如 0.21.1 迁移后未在 BuiltinEngine::ACTIONS 中列出但已注册的 capability）
    BuiltinCapability,
}

// ── Binding 摘要 ─────────────────────────────────────────────────────────────

/// 本地 binding 类型——标识触发入口形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingKind {
    /// 搜索关键词触发（BuiltinEngine descriptor）
    SearchKeyword,
    /// Context 自动触发（clipboard is url / file path 等）
    ContextBinding,
    /// Chord 键位触发（Alt+字母）
    ChordKey,
}

/// 单个 binding 的摘要信息。
#[derive(Debug, Clone, Serialize)]
pub struct BindingSummary {
    /// binding 唯一标识（与各 binding store 的 key 一致）。
    pub binding_id: String,
    /// binding 类型。
    pub kind: BindingKind,
    /// 当前是否启用（从各 binding store 聚合的真实状态）。
    pub enabled: bool,
    /// 人类可读的触发描述（如 "关键词：设置" / "Alt+S" / "剪贴板是 URL"）。
    /// 已按当前语言解析，前端直接展示。
    pub trigger_label: String,
}

// ── 可用性与出口状态 ─────────────────────────────────────────────────────────

/// 本地可用性——反映功能在当前运行时是否可执行。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalAvailability {
    /// 可用
    Available,
    /// 不可用（插件已禁用 / binding 已禁用 / descriptor 被 disable）
    Disabled,
    /// 来源不可用（插件未加载 / capability 未注册——已移除的残留 id）
    SourceUnavailable,
}

/// AI 出口状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
#[allow(dead_code)] // NotApplicable: Interaction-only 无对应 Capability 的投影占位
pub enum CatalogExitStatus {
    /// 已授权（在用户 allowlist 中——0.21.5 落地，当前从 policy.ai_default 投影）
    Enabled,
    /// 未授权
    Disabled,
    /// 代码级禁止（policy.allowed_origins 不含 AI）
    CodeForbidden,
    /// 不适用（Interaction-only，无对应 Capability）
    NotApplicable,
}

/// Capability 的投影信息（不复制 schema/policy 全文，只取展示所需字段）。
#[derive(Debug, Clone, Serialize)]
pub struct CatalogCapabilityProjection {
    /// Capability id（与 `Capability::id()` 一致）。
    pub capability_id: String,
    /// 来源（builtin / plugin）
    pub source: FeatureSource,
    /// danger 等级（从 `policy().danger` 投影）
    pub danger: String,
    /// 是否敏感读取（从 `policy().sensitive` 投影）
    pub sensitive: bool,
    /// 是否需要人机确认（从 `policy().requires_confirmation()` 投影）
    pub requires_confirmation: bool,
    /// AI 出口状态（从 `policy().allowed_origins` + `ai_default` 投影）
    pub ai_status: CatalogExitStatus,
    /// MCP 出口状态（从 `policy().allowed_origins` + `mcp_default` 投影）
    pub mcp_status: CatalogExitStatus,
    /// 运行时要求描述（从 `policy().runtime_requirement` 投影）
    pub runtime_requirement: String,
    /// schema description（人类可读的能力描述，已按当前语言或原文返回）
    pub description: String,
}

// ── FeatureCatalogItem —— 目录项 ─────────────────────────────────────────────

/// 功能目录项——同一产品功能只有一个目录身份（§5.5 退出条件）。
#[derive(Debug, Clone, Serialize)]
pub struct FeatureCatalogItem {
    /// Feature 唯一标识（§3.6：`FeatureId("blink.region_capture")` 格式）。
    /// 当前实现：builtin descriptor 用 descriptor id；chord 用 `chord.{binding_id}`；
    /// plugin capability 用 `plugin.{capability_id}`。
    pub feature_id: String,
    /// 面向用户的标题（已按当前语言解析）。
    pub title: String,
    /// 面向用户的说明（已按当前语言解析）。
    pub description: String,
    /// 所属分组。
    pub group: FeatureGroup,
    /// 来源（builtin / chord / plugin / builtin_capability）。
    pub source: FeatureSource,
    /// 对应的 Capability id（如果有；Interaction-only 如 voice_input 为 None）。
    pub capability_id: Option<String>,
    /// 本地 binding 列表（可能为空——某些 capability 无直接 binding）。
    pub bindings: Vec<BindingSummary>,
    /// 本地可用性。
    pub local_availability: LocalAvailability,
    /// Capability 投影（如果有）。
    pub capability_projection: Option<CatalogCapabilityProjection>,
    /// 不可用原因（当 local_availability != Available 时填充）。
    pub unavailable_reason: Option<String>,
}

// ── 批量操作 ──────────────────────────────────────────────────────────────────

/// binding 批量操作类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingOpKind {
    /// 启用 binding
    Enable,
    /// 禁用 binding
    Disable,
}

/// 单个 binding 操作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingOp {
    /// 操作类型。
    pub op: BindingOpKind,
    /// binding 类型（决定写回哪个 binding store）。
    pub kind: BindingKind,
    /// binding id（各 store 的 key）。
    pub binding_id: String,
}

/// 单个 binding 操作的结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "snake_case")]
pub struct ApplyBindingResult {
    /// 操作的 binding id。
    pub binding_id: String,
    /// 是否成功。
    pub success: bool,
    /// 失败原因（success=false 时填充）。
    pub error: Option<String>,
}
