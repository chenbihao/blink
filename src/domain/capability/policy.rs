//! Capability 策略类型墙（0.21.0）。
//!
//! 调用来源（`InvocationOrigin`）、运行时要求（`RuntimeRequirement`）、
//! 出口策略（`CapabilityPolicy`）和确认策略（`ConfirmationPolicy`）
//! 集中在此模块定义——它们是 Capability 的静态自述，在 invoke 前由
//! Registry 执行代码级门禁。
//!
//! **设计原则**：
//! - `Capability::policy()` 是风险、出口和运行时要求的唯一真源。
//! - `CapabilitySchema.sensitive` 兼容期从 policy 投影；完成后不保存第二份。
//! - Registry 在 invoke 前检查 origin、runtime 和用户授权；UI 隐藏 / tool list
//!   过滤只是第一层，不能替代 invoke-time 门禁。
//! - `DangerClass` 从 `execution` 迁到此模块，AI 确认与审计消费同一类型。

use serde::{Deserialize, Serialize};

// ── DangerClass 迁移 ─────────────────────────────────────────────────────────

/// 危险等级——安全枚举只有一份，Capability / AI 确认 / 审计共用。
///
/// **0.21.0**：从 `execution::schema` 迁到 `capability::policy`，
/// `execution` 模块 re-export 保持兼容期零 churn。
///
/// - `Safe`：可逆 / 只读 / 无副作用，AI 高置信可直接执行（Suggestion + Tab 或直接）
/// - `Dangerous`：不可逆 / 危险，**任何模式下都必须人机二次确认**
///
/// **默认 `Safe`**——`Capability` trait default impl 返回它；
/// Dangerous 动作必须显式在 `policy()` 中覆盖。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DangerClass {
    /// 可逆 / 只读 / 无副作用：打开文件、翻译、查询、复制。
    Safe,
    /// 不可逆 / 危险：删除、发送、覆盖写、执行命令、关机、锁屏。
    Dangerous,
}

impl Default for DangerClass {
    fn default() -> Self {
        Self::Safe
    }
}

// ── InvocationOrigin ─────────────────────────────────────────────────────────

/// 调用来源——标识谁触发了这次 Capability invoke。
///
/// invoke-time 门禁据此判断 origin 是否在 `CapabilityPolicy.allowed_origins` 中。
///
/// - `LocalSurface`：搜索、Chord、菜单等用户直接入口。
/// - `LocalCommand`：Tauri command 的兼容/Interaction 内部编排。
/// - `LocalAi`：主窗口 AI 和独立对话窗口，确认粒度仍按现有模式区分。
/// - `Cli`：无 GUI 默认环境。
/// - `Mcp`：外部 MCP server 调用，即使 server 运行在主进程也不能伪装成本地用户入口。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvocationOrigin {
    LocalSurface,
    LocalCommand,
    LocalAi,
    Cli,
    Mcp,
}

impl InvocationOrigin {
    /// 是否属于本地入口（Surface / Command / Ai）。
    #[allow(dead_code)] // 前瞻性 API：供 origin 分类逻辑消费
    pub fn is_local(self) -> bool {
        matches!(
            self,
            Self::LocalSurface | Self::LocalCommand | Self::LocalAi
        )
    }

    /// 是否属于 AI 入口。
    #[allow(dead_code)] // 前瞻性 API：供 origin 分类逻辑消费
    pub fn is_ai(self) -> bool {
        self == Self::LocalAi
    }

    /// 是否属于外部入口（CLI / MCP）。
    #[allow(dead_code)] // 前瞻性 API：供 origin 分类逻辑消费
    pub fn is_external(self) -> bool {
        matches!(self, Self::Cli | Self::Mcp)
    }
}

impl std::fmt::Display for InvocationOrigin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::LocalSurface => write!(f, "local_surface"),
            Self::LocalCommand => write!(f, "local_command"),
            Self::LocalAi => write!(f, "local_ai"),
            Self::Cli => write!(f, "cli"),
            Self::Mcp => write!(f, "mcp"),
        }
    }
}

// ── OriginSet ────────────────────────────────────────────────────────────────

/// 允许来源集合——`CapabilityPolicy.allowed_origins` 的类型。
///
/// 组合式 bitflags 风格，支持高效 `contains` 查询。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct OriginSet(u8);

impl OriginSet {
    #[allow(dead_code)] // 前瞻性常量：空集场景
    pub const NONE: Self = Self(0);
    pub const LOCAL_SURFACE: Self = Self(1);
    pub const LOCAL_COMMAND: Self = Self(2);
    pub const LOCAL_AI: Self = Self(4);
    pub const CLI: Self = Self(8);
    pub const MCP: Self = Self(16);

    /// 全部本地入口（Surface + Command + Ai）。
    pub const ALL_LOCAL: Self =
        Self(Self::LOCAL_SURFACE.0 | Self::LOCAL_COMMAND.0 | Self::LOCAL_AI.0);

    /// 全部来源。
    pub const ALL: Self = Self(Self::ALL_LOCAL.0 | Self::CLI.0 | Self::MCP.0);

    /// local + AI + CLI（不含 MCP，GUI 副作用类常用）。
    pub const LOCAL_AND_CLI: Self = Self(Self::ALL_LOCAL.0 | Self::CLI.0);

    /// 从单个 origin 构造。
    pub fn from_single(origin: InvocationOrigin) -> Self {
        match origin {
            InvocationOrigin::LocalSurface => Self::LOCAL_SURFACE,
            InvocationOrigin::LocalCommand => Self::LOCAL_COMMAND,
            InvocationOrigin::LocalAi => Self::LOCAL_AI,
            InvocationOrigin::Cli => Self::CLI,
            InvocationOrigin::Mcp => Self::MCP,
        }
    }

    /// 检查是否包含指定 origin。
    pub fn contains(self, origin: InvocationOrigin) -> bool {
        self.0 & Self::from_single(origin).0 != 0
    }

    /// 合并另一个集合。
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 是否为空集。
    #[allow(dead_code)] // 前瞻性 API：与 NONE 配套
    pub fn is_empty(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for OriginSet {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::fmt::Display for OriginSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == Self::ALL.0 {
            return write!(f, "all");
        }
        let mut parts = Vec::new();
        if self.contains(InvocationOrigin::LocalSurface) {
            parts.push("surface");
        }
        if self.contains(InvocationOrigin::LocalCommand) {
            parts.push("command");
        }
        if self.contains(InvocationOrigin::LocalAi) {
            parts.push("ai");
        }
        if self.contains(InvocationOrigin::Cli) {
            parts.push("cli");
        }
        if self.contains(InvocationOrigin::Mcp) {
            parts.push("mcp");
        }
        if parts.is_empty() {
            write!(f, "none")
        } else {
            write!(f, "{}", parts.join("|"))
        }
    }
}

// ── RuntimeRequirement ───────────────────────────────────────────────────────

/// 组合式运行时要求（bitflags 风格）——Gui 与 MainProcess 非互斥。
///
/// Capability 对运行基础设施的**静态声明**，让 CLI、MCP、AI、本地入口
/// 在执行前得到一致、可恢复的"不支持"结果，而不是运行到 getter panic
/// 或窗口调用失败。
///
/// | bit | 含义 | 典型 Capability | 运行时缺该 bit |
/// |---|---|---|---|
/// | `NONE` | 纯数据/计算，无环境依赖 | `search_apps`、`read_text_file` | 任何环境可执行 |
/// | `MAIN_PROCESS` | 需主进程服务 wiring（DB/事件桥/状态） | `update_setting`、`update_sticky` | CLI 进程/独立 MCP server 返回 Unsupported |
/// | `GUI_SURFACE` | 需 SurfacePort（打开/切换窗口） | `open_settings`、`create_sticky` | 无 GUI runtime 返回 Unsupported |
/// | `DESKTOP_SESSION` | 需交互桌面会话（截图、Shell 打开） | `screenshot`、`open_url` | 无头/服务会话返回 Unsupported |
///
/// `GUI_SURFACE` 蕴含 `MAIN_PROCESS`；可按位组合（如 `DESKTOP_SESSION | MAIN_PROCESS`）。
/// invoke 前比较 `InvokeContext.runtime` 实际可用集合，缺任一 bit 即返回
/// 结构化 `Unsupported`，禁止 panic。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct RuntimeRequirement(u8);

impl RuntimeRequirement {
    pub const NONE: Self = Self(0);
    pub const MAIN_PROCESS: Self = Self(1);
    pub const GUI_SURFACE: Self = Self(2);
    pub const DESKTOP_SESSION: Self = Self(4);

    /// `GUI_SURFACE` 蕴含 `MAIN_PROCESS`。
    pub fn normalize(self) -> Self {
        let mut bits = self.0;
        if bits & Self::GUI_SURFACE.0 != 0 {
            bits |= Self::MAIN_PROCESS.0;
        }
        Self(bits)
    }

    /// 检查 `actual` 是否满足 `self` 的所有要求。
    pub fn is_satisfied_by(self, actual: Self) -> bool {
        let required = self.normalize();
        let actual_normalized = actual.normalize();
        (required.0 & actual_normalized.0) == required.0
    }

    /// 合并。
    pub fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// 是否无任何运行时要求。
    #[allow(dead_code)] // 前瞻性 API：供 InvokeContext::runtime_satisfies 链路消费
    pub fn is_none(self) -> bool {
        self.0 == 0
    }
}

impl std::ops::BitOr for RuntimeRequirement {
    type Output = Self;
    fn bitor(self, rhs: Self) -> Self {
        self.union(rhs)
    }
}

impl std::fmt::Display for RuntimeRequirement {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.0 == 0 {
            return write!(f, "none");
        }
        let mut parts = Vec::new();
        if self.0 & Self::MAIN_PROCESS.0 != 0 {
            parts.push("main_process");
        }
        if self.0 & Self::GUI_SURFACE.0 != 0 {
            parts.push("gui_surface");
        }
        if self.0 & Self::DESKTOP_SESSION.0 != 0 {
            parts.push("desktop_session");
        }
        write!(f, "{}", parts.join("|"))
    }
}

// ── RuntimeCapabilities ──────────────────────────────────────────────────────

/// invoke 时实际可用的运行时能力——由调用方构造并传入 `InvokeContext`。
///
/// `surface` 为 `None` 表示无 GUI 运行时（CLI / 独立 MCP server）。
/// `MAIN_PROCESS` 由调用方根据是否在主进程中自行标记。
pub struct RuntimeCapabilities<'a> {
    pub surface: Option<&'a dyn SurfacePort>,
    /// 是否在 Blink 主进程中（有 DB / 事件桥 / 服务 wiring）。
    pub main_process: bool,
    /// 是否在交互桌面会话中（非无头/服务会话）。
    pub desktop_session: bool,
}

impl<'a> RuntimeCapabilities<'a> {
    /// 转为 `RuntimeRequirement` 位集，与 policy 做位运算比较。
    pub fn as_requirement(&self) -> RuntimeRequirement {
        let mut bits = 0u8;
        if self.main_process {
            bits |= RuntimeRequirement::MAIN_PROCESS.0;
        }
        if self.surface.is_some() {
            bits |= RuntimeRequirement::GUI_SURFACE.0;
        }
        if self.desktop_session {
            bits |= RuntimeRequirement::DESKTOP_SESSION.0;
        }
        RuntimeRequirement(bits)
    }
}

// ── SurfacePort ──────────────────────────────────────────────────────────────

/// GUI Capability 的最小权限端口——语义化窗口操作接口。
///
/// 禁止向所有 Capability 暴露 `emit(event)`、任意 window label、
/// 完整 `DomainEnv` 或 Tauri handle。插件 Capability 永远拿不到 SurfacePort。
///
/// **0.21.0**：trait 定义落地，具体注入由 app 层桥接 `TauriDomainEnv`。
/// 当前 `DomainEnv` 已有的 `open_settings` / `show_sticky_manager` 等方法
/// 在 0.21.1+ 迁移 Capability 时逐项接入此端口。
#[async_trait::async_trait]
pub trait SurfacePort: Send + Sync {
    fn open_settings(&self) -> Result<(), SurfaceError>;
    fn open_sticky_manager(&self) -> Result<(), SurfaceError>;
    fn open_chat(&self, prefill: Option<&str>) -> Result<(), SurfaceError>;
    fn open_clipboard_mode(&self) -> Result<(), SurfaceError>;
    /// 启动区域截图选区。async 因截图时序需等待 DWM 合成。
    async fn start_region_capture(&self) -> Result<(), SurfaceError>;
    fn start_image_editor(&self, source: EditorSourceRef) -> Result<(), SurfaceError>;
    fn start_content_editor(&self, request: ContentEditorRequest) -> Result<(), SurfaceError>;

    /// 隐藏主窗口（GUI starter Capability 打开新窗口前调用）。
    fn hide_main_window(&self, reason: &str);

    /// 退出应用进程。
    fn exit_app(&self);
}

/// SurfacePort 错误——不暴露内部窗口创建失败细节，只给语义化分类。
#[derive(Debug, Clone, thiserror::Error)]
pub enum SurfaceError {
    #[error("窗口创建失败: {detail}")]
    CreateFailed { detail: String },
    #[error("窗口不可用: {detail}")]
    Unavailable { detail: String },
}

// ── EditorSourceRef / ContentEditorRequest ───────────────────────────────────

/// 图片编辑器来源引用——避免传递大 Blob。
#[derive(Debug, Clone)]
#[allow(dead_code)] // StashRef 待 0.19.4 ImageStash 引用闭环完整落地后消费
pub enum EditorSourceRef {
    /// 剪贴板图片字节。
    ClipboardImage(Vec<u8>),
    /// ImageStash 引用 id。
    StashRef(String),
}

/// 内容编辑器请求——结构化 prefill。
#[derive(Debug, Clone)]
pub struct ContentEditorRequest {
    pub body: String,
    pub title: Option<String>,
    pub origin: String,
    pub origin_ref: Option<String>,
    pub save_policy: String,
}

// ── AiDefault / McpDefault ───────────────────────────────────────────────────

/// AI 出口默认授权——代码级策略，用户配置只能在其子集内授权。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiDefault {
    /// 默认开启（Safe 普通生产能力）。
    On,
    /// 默认关闭（Dangerous / local-only / 诊断类）。
    Off,
}

impl Default for AiDefault {
    fn default() -> Self {
        Self::Off
    }
}

/// MCP 出口默认授权——首版默认空，Dangerous 永远禁止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum McpDefault {
    /// 默认关闭，可由用户显式暴露（Safe + sensitive 可暴露）。
    DefaultOff,
    /// 代码级禁止（Dangerous / GUI starter / local-only）。
    Forbidden,
}

impl Default for McpDefault {
    fn default() -> Self {
        Self::DefaultOff
    }
}

// ── ConfirmationPolicy ───────────────────────────────────────────────────────

/// 确认策略——复用 0.17.8 确认记忆存储。
///
/// key 为 `capability_id`，不做宽泛旧 key 迁移；
/// 清除确认记忆时数据库与 session cache 同时失效。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConfirmationPolicy {
    /// 是否需要人机确认（Dangerous 或 sensitive 时为 true）。
    pub required: bool,
    /// 确认结果是否允许按 capability_id 记忆。
    /// 涉及字段级配置写入等每次参数都不同的操作必须返回 false。
    pub rememberable: bool,
}

impl Default for ConfirmationPolicy {
    fn default() -> Self {
        Self {
            required: false,
            rememberable: true,
        }
    }
}

impl ConfirmationPolicy {
    /// 构造 Dangerous 策略——必确认，可记忆（但 `shutdown` 等可覆写为不可记忆）。
    pub fn dangerous(rememberable: bool) -> Self {
        Self {
            required: true,
            rememberable,
        }
    }

    /// 构造 sensitive 读取策略——必确认，可记忆。
    pub fn sensitive() -> Self {
        Self {
            required: true,
            rememberable: true,
        }
    }

    /// 构造 Safe 无副作用策略——不需确认。
    pub fn safe() -> Self {
        Self {
            required: false,
            rememberable: true,
        }
    }
}

// ── CapabilityPolicy ─────────────────────────────────────────────────────────

/// Capability 出口策略——风险、运行时要求和出口授权的**唯一真源**。
///
/// `Capability::policy()` 返回此结构。Registry 在 invoke 前检查
/// origin、runtime 和用户授权；UI 隐藏 / tool list 过滤只是第一层。
///
/// **兼容期**：`CapabilitySchema.sensitive` 从 `policy.danger` 和
/// `policy.confirmation` 投影；完成后不保存第二份可漂移值。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CapabilityPolicy {
    /// 允许的调用来源——代码级硬上限。
    pub allowed_origins: OriginSet,
    /// 运行时要求——缺任一 bit 返回 Unsupported。
    pub runtime_requirement: RuntimeRequirement,
    /// 危险等级——Safe / Dangerous。
    pub danger: DangerClass,
    /// 是否读取敏感数据（隐私语义，独立于 danger）。
    pub sensitive: bool,
    /// AI 出口默认授权。
    pub ai_default: AiDefault,
    /// MCP 出口默认授权。
    pub mcp_default: McpDefault,
    /// 确认策略。
    pub confirmation: ConfirmationPolicy,
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::NONE,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::safe(),
        }
    }
}

impl CapabilityPolicy {
    /// 检查指定 origin 是否被允许。
    pub fn allows_origin(&self, origin: InvocationOrigin) -> bool {
        self.allowed_origins.contains(origin)
    }

    /// 检查指定运行时是否满足要求。
    pub fn runtime_satisfied(&self, actual: RuntimeRequirement) -> bool {
        self.runtime_requirement.is_satisfied_by(actual)
    }

    /// 从 policy 推导 `requires_ai_confirmation`。
    /// Dangerous 或 sensitive 都触发确认，但语义不同。
    pub fn requires_confirmation(&self) -> bool {
        self.danger == DangerClass::Dangerous || self.sensitive
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── DangerClass ──────────────────────────────────────────────────────

    #[test]
    fn danger_class_defaults_to_safe() {
        assert_eq!(DangerClass::default(), DangerClass::Safe);
    }

    #[test]
    fn danger_class_serializes_stable() {
        assert_eq!(
            serde_json::to_string(&DangerClass::Safe).unwrap(),
            "\"Safe\""
        );
        assert_eq!(
            serde_json::to_string(&DangerClass::Dangerous).unwrap(),
            "\"Dangerous\""
        );
    }

    // ── InvocationOrigin ─────────────────────────────────────────────────

    #[test]
    fn origin_is_local_correct() {
        assert!(InvocationOrigin::LocalSurface.is_local());
        assert!(InvocationOrigin::LocalCommand.is_local());
        assert!(InvocationOrigin::LocalAi.is_local());
        assert!(!InvocationOrigin::Cli.is_local());
        assert!(!InvocationOrigin::Mcp.is_local());
    }

    #[test]
    fn origin_is_ai_correct() {
        assert!(InvocationOrigin::LocalAi.is_ai());
        assert!(!InvocationOrigin::LocalSurface.is_ai());
    }

    #[test]
    fn origin_is_external_correct() {
        assert!(InvocationOrigin::Cli.is_external());
        assert!(InvocationOrigin::Mcp.is_external());
        assert!(!InvocationOrigin::LocalSurface.is_external());
    }

    #[test]
    fn origin_display_stable() {
        assert_eq!(InvocationOrigin::LocalSurface.to_string(), "local_surface");
        assert_eq!(InvocationOrigin::Mcp.to_string(), "mcp");
    }

    #[test]
    fn origin_serializes_snake_case() {
        let v = serde_json::to_value(InvocationOrigin::LocalAi).unwrap();
        assert_eq!(v, "local_ai");
        let v = serde_json::to_value(InvocationOrigin::LocalSurface).unwrap();
        assert_eq!(v, "local_surface");
    }

    // ── OriginSet ────────────────────────────────────────────────────────

    #[test]
    fn origin_set_all_contains_all() {
        let set = OriginSet::ALL;
        assert!(set.contains(InvocationOrigin::LocalSurface));
        assert!(set.contains(InvocationOrigin::LocalCommand));
        assert!(set.contains(InvocationOrigin::LocalAi));
        assert!(set.contains(InvocationOrigin::Cli));
        assert!(set.contains(InvocationOrigin::Mcp));
    }

    #[test]
    fn origin_set_none_is_empty() {
        assert!(OriginSet::NONE.is_empty());
    }

    #[test]
    fn origin_set_union_works() {
        let set = OriginSet::LOCAL_SURFACE | OriginSet::CLI;
        assert!(set.contains(InvocationOrigin::LocalSurface));
        assert!(set.contains(InvocationOrigin::Cli));
        assert!(!set.contains(InvocationOrigin::Mcp));
    }

    #[test]
    fn origin_set_all_local_excludes_external() {
        let set = OriginSet::ALL_LOCAL;
        assert!(set.contains(InvocationOrigin::LocalSurface));
        assert!(set.contains(InvocationOrigin::LocalAi));
        assert!(!set.contains(InvocationOrigin::Cli));
        assert!(!set.contains(InvocationOrigin::Mcp));
    }

    #[test]
    fn origin_set_local_and_cli_excludes_mcp() {
        let set = OriginSet::LOCAL_AND_CLI;
        assert!(set.contains(InvocationOrigin::LocalAi));
        assert!(set.contains(InvocationOrigin::Cli));
        assert!(!set.contains(InvocationOrigin::Mcp));
    }

    #[test]
    fn origin_set_display_all() {
        assert_eq!(OriginSet::ALL.to_string(), "all");
    }

    #[test]
    fn origin_set_display_none() {
        assert_eq!(OriginSet::NONE.to_string(), "none");
    }

    #[test]
    fn origin_set_display_partial() {
        let set = OriginSet::LOCAL_SURFACE | OriginSet::MCP;
        assert_eq!(set.to_string(), "surface|mcp");
    }

    // ── RuntimeRequirement ───────────────────────────────────────────────

    #[test]
    fn runtime_none_satisfied_by_anything() {
        assert!(RuntimeRequirement::NONE.is_satisfied_by(RuntimeRequirement::NONE));
        assert!(RuntimeRequirement::NONE.is_satisfied_by(RuntimeRequirement::GUI_SURFACE));
    }

    #[test]
    fn runtime_main_process_not_satisfied_by_none() {
        assert!(!RuntimeRequirement::MAIN_PROCESS.is_satisfied_by(RuntimeRequirement::NONE));
        assert!(RuntimeRequirement::MAIN_PROCESS.is_satisfied_by(RuntimeRequirement::MAIN_PROCESS));
    }

    #[test]
    fn runtime_gui_implies_main_process() {
        // GUI_SURFACE 蕴含 MAIN_PROCESS
        let req = RuntimeRequirement::GUI_SURFACE;
        // 即使 actual 只有 MAIN_PROCESS，也不满足 GUI_SURFACE
        assert!(!req.is_satisfied_by(RuntimeRequirement::MAIN_PROCESS));
        // 但 GUI_SURFACE 自身 normalize 后含 MAIN_PROCESS
        assert!(req.is_satisfied_by(RuntimeRequirement::GUI_SURFACE));
    }

    #[test]
    fn runtime_gui_normalize_includes_main() {
        let normalized = RuntimeRequirement::GUI_SURFACE.normalize();
        assert!(normalized.0 & RuntimeRequirement::MAIN_PROCESS.0 != 0);
    }

    #[test]
    fn runtime_desktop_session_union_main() {
        let req = RuntimeRequirement::DESKTOP_SESSION | RuntimeRequirement::MAIN_PROCESS;
        assert!(req.is_satisfied_by(req));
        assert!(!req.is_satisfied_by(RuntimeRequirement::DESKTOP_SESSION));
    }

    #[test]
    fn runtime_display() {
        assert_eq!(RuntimeRequirement::NONE.to_string(), "none");
        let req = RuntimeRequirement::GUI_SURFACE | RuntimeRequirement::DESKTOP_SESSION;
        let s = req.to_string();
        assert!(s.contains("gui_surface"));
        assert!(s.contains("desktop_session"));
    }

    // ── RuntimeCapabilities ──────────────────────────────────────────────

    #[test]
    fn runtime_capabilities_none_as_requirement() {
        let caps = RuntimeCapabilities {
            surface: None,
            main_process: false,
            desktop_session: false,
        };
        assert_eq!(caps.as_requirement(), RuntimeRequirement::NONE);
    }

    #[test]
    fn runtime_capabilities_full() {
        struct DummySurface;
        #[async_trait::async_trait]
        impl SurfacePort for DummySurface {
            fn open_settings(&self) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn open_sticky_manager(&self) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn open_chat(&self, _: Option<&str>) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn open_clipboard_mode(&self) -> Result<(), SurfaceError> {
                Ok(())
            }
            async fn start_region_capture(&self) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn start_image_editor(&self, _: EditorSourceRef) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn start_content_editor(&self, _: ContentEditorRequest) -> Result<(), SurfaceError> {
                Ok(())
            }
            fn hide_main_window(&self, _reason: &str) {}
            fn exit_app(&self) {}
        }
        let caps = RuntimeCapabilities {
            surface: Some(&DummySurface),
            main_process: true,
            desktop_session: true,
        };
        let req = caps.as_requirement();
        assert!(req.0 & RuntimeRequirement::GUI_SURFACE.0 != 0);
        assert!(req.0 & RuntimeRequirement::MAIN_PROCESS.0 != 0);
        assert!(req.0 & RuntimeRequirement::DESKTOP_SESSION.0 != 0);
    }

    // ── CapabilityPolicy ─────────────────────────────────────────────────

    #[test]
    fn policy_default_allows_all_origins() {
        let p = CapabilityPolicy::default();
        assert!(p.allows_origin(InvocationOrigin::LocalSurface));
        assert!(p.allows_origin(InvocationOrigin::Mcp));
    }

    #[test]
    fn policy_default_no_runtime_requirement() {
        let p = CapabilityPolicy::default();
        assert!(p.runtime_satisfied(RuntimeRequirement::NONE));
    }

    #[test]
    fn policy_default_no_confirmation() {
        let p = CapabilityPolicy::default();
        assert!(!p.requires_confirmation());
    }

    #[test]
    fn policy_dangerous_requires_confirmation() {
        let p = CapabilityPolicy {
            danger: DangerClass::Dangerous,
            confirmation: ConfirmationPolicy::dangerous(true),
            ..Default::default()
        };
        assert!(p.requires_confirmation());
    }

    #[test]
    fn policy_sensitive_requires_confirmation() {
        let p = CapabilityPolicy {
            sensitive: true,
            confirmation: ConfirmationPolicy::sensitive(),
            ..Default::default()
        };
        assert!(p.requires_confirmation());
        // sensitive 不改变 danger
        assert_eq!(p.danger, DangerClass::Safe);
    }

    #[test]
    fn policy_restricted_origins() {
        let p = CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_AND_CLI,
            ..Default::default()
        };
        assert!(p.allows_origin(InvocationOrigin::LocalAi));
        assert!(p.allows_origin(InvocationOrigin::Cli));
        assert!(!p.allows_origin(InvocationOrigin::Mcp));
    }

    #[test]
    fn policy_runtime_gate() {
        let p = CapabilityPolicy {
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            ..Default::default()
        };
        // CLI runtime（无 GUI）不满足
        assert!(!p.runtime_satisfied(RuntimeRequirement::NONE));
        // GUI runtime 满足
        assert!(p.runtime_satisfied(RuntimeRequirement::GUI_SURFACE));
    }

    // ── ConfirmationPolicy ───────────────────────────────────────────────

    #[test]
    fn confirmation_safe_not_required() {
        let c = ConfirmationPolicy::safe();
        assert!(!c.required);
        assert!(c.rememberable);
    }

    #[test]
    fn confirmation_dangerous_required() {
        let c = ConfirmationPolicy::dangerous(true);
        assert!(c.required);
        assert!(c.rememberable);
    }

    #[test]
    fn confirmation_dangerous_not_rememberable() {
        let c = ConfirmationPolicy::dangerous(false);
        assert!(c.required);
        assert!(!c.rememberable);
    }

    #[test]
    fn confirmation_sensitive_required_and_rememberable() {
        let c = ConfirmationPolicy::sensitive();
        assert!(c.required);
        assert!(c.rememberable);
    }

    // ── AiDefault / McpDefault ───────────────────────────────────────────

    #[test]
    fn ai_default_defaults_to_off() {
        assert_eq!(AiDefault::default(), AiDefault::Off);
    }

    #[test]
    fn mcp_default_defaults_to_default_off() {
        assert_eq!(McpDefault::default(), McpDefault::DefaultOff);
    }

    #[test]
    fn ai_default_serializes() {
        assert_eq!(serde_json::to_string(&AiDefault::On).unwrap(), "\"on\"");
        assert_eq!(serde_json::to_string(&AiDefault::Off).unwrap(), "\"off\"");
    }

    #[test]
    fn mcp_default_serializes() {
        assert_eq!(
            serde_json::to_string(&McpDefault::DefaultOff).unwrap(),
            "\"default_off\""
        );
        assert_eq!(
            serde_json::to_string(&McpDefault::Forbidden).unwrap(),
            "\"forbidden\""
        );
    }
}
