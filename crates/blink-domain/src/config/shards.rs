//! 配置分片 struct 定义（0.14.6 §2.1 从 `app/config.rs` 迁入）。
//!
//! 包含 AppConfig 的 6 个分片 + 引擎配置分片 + ContextConfig + ScreenshotConfig。
//! ConfigKey impl 随各自 struct 定义一起。

use serde::{Deserialize, Serialize};

use super::store::ConfigKey;

// ── HotkeyConfig ─────────────────────────────────────────────────────────────

/// 快捷键配置。
///
/// **0.8.8 分片扩展**：原只承载按键数据,现在合并 `tap_threshold` / `grace_period`
/// 两字段,让 `app.hotkey` KV 分片包含所有"热键行为"配置(对齐 phases/0.8 §8.4)。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub modifiers: Vec<String>,
    pub key: String,
    pub display: String,
    #[serde(default = "default_tap_threshold")]
    pub tap_threshold: u64,
    #[serde(default = "default_grace_period")]
    pub grace_period: u64,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            modifiers: vec!["alt".to_string()],
            key: " ".to_string(),
            display: "Alt+Space".to_string(),
            tap_threshold: default_tap_threshold(),
            grace_period: default_grace_period(),
        }
    }
}

impl ConfigKey for HotkeyConfig {
    const KEY: &'static str = "app.hotkey";
}

fn default_tap_threshold() -> u64 {
    300
}
fn default_grace_period() -> u64 {
    500
}

// ── AppearanceConfig ──────────────────────────────────────────────────────────

/// 0.22.13：引导向导当前版本。
///
/// welcome 引导页随二进制发布、始终渲染最新内容；发布新引导时本值 +1，
/// 启动时 `onboarding_version < ONBOARDING_VERSION` 即弹出引导，完成/跳过后写回本值。
/// 存量老用户（0.22.13 前的记录）一次性迁移为 0（未完成当前版），升级后补看一次新版向导。
pub const ONBOARDING_VERSION: u32 = 1;

/// 外观配置分片。theme / language / auto_start + log_level + ai_http_body_log + window_opacity + onboarding_version。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppearanceConfig {
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_false")]
    pub auto_start: bool,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// AI HTTP 请求/响应体日志开关（0.21.16，默认关）。体量极大（带 MCP 工具池的请求体
    /// 一次可达几十 KB），仅排查 provider 兼容问题时开启；开启后以 debug 级打印。
    #[serde(default = "default_false")]
    pub ai_http_body_log: bool,
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
    /// 0.22.13：已完成/看过的最新引导版本。None = 0.22.13 前的存量记录
    /// （见 `app_config::get_config` 一次性迁移）；Some(n) = 已完成至第 n 版。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub onboarding_version: Option<u32>,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            language: default_language(),
            auto_start: false,
            log_level: default_log_level(),
            ai_http_body_log: false,
            window_opacity: default_window_opacity(),
            onboarding_version: None,
        }
    }
}

impl ConfigKey for AppearanceConfig {
    const KEY: &'static str = "app.appearance";
}

// ── SearchConfig ──────────────────────────────────────────────────────────────

/// 搜索行为分片。历史 / 结果数 / 分页 / surface takeover。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_true")]
    pub search_history_enabled: bool,
    #[serde(default = "default_30")]
    pub search_history_days: u32,
    #[serde(default = "default_50")]
    pub max_results: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default = "default_surface_takeover_enabled")]
    pub surface_takeover_enabled: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            search_history_enabled: true,
            search_history_days: 30,
            max_results: 50,
            page_size: default_page_size(),
            surface_takeover_enabled: true,
        }
    }
}

impl ConfigKey for SearchConfig {
    const KEY: &'static str = "app.search";
}

// ── SuggestionConfig ──────────────────────────────────────────────────────────

/// 建议行为分片。autosuggest + proactive + 0.24 双槽门槛/降频 + 展示策略开关组。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SuggestionConfig {
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
    /// Secondary 槽门槛（0.24 §3.4）：精确挡掉 0.55 兜底——第二槽也该是有把握的建议。
    #[serde(default = "default_secondary_min_rank")]
    pub secondary_min_rank: f64,
    /// 建议降频（0.24 §3.8 会话级抑制，设置页文案"建议降频"）：同 (kind, origin,
    /// 指纹) 连续 3 次未采纳 → 本会话抑制该候选。默认开（触发条件保守、误杀成本低）。
    #[serde(default = "default_true")]
    pub suppress_repeated: bool,
    /// 输入补全显示开关（0.24 §5.6 设置投影）：Completion 影子候选（首拼 → keyword）。
    #[serde(default = "default_true")]
    pub completion_enabled: bool,
    /// 环境建议显示开关：awareness 派生候选（选区/剪贴板翻译与 AI）。
    /// query 派生候选（如直接输入英文的翻译）不受此开关影响——它是输入意图不是环境。
    #[serde(default = "default_true")]
    pub context_suggestion_enabled: bool,
    /// AI 兜底建议显示开关：AskAi 候选（AI Provider 可用性 `ai_available` 之上的
    /// 建议域独立闸——关建议不连坐 AI 功能本身，AI Provider 配置留在其真源）。
    #[serde(default = "default_true")]
    pub ai_suggestion_enabled: bool,
    /// 第二建议槽显示开关（secondary 槽）：关闭时只出 primary 单槽。
    #[serde(default = "default_true")]
    pub secondary_enabled: bool,
}

impl Default for SuggestionConfig {
    fn default() -> Self {
        Self {
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
            proactive_enabled: false,
            empty_query_topn: 5,
            secondary_min_rank: default_secondary_min_rank(),
            suppress_repeated: true,
            completion_enabled: true,
            context_suggestion_enabled: true,
            ai_suggestion_enabled: true,
            secondary_enabled: true,
        }
    }
}

impl ConfigKey for SuggestionConfig {
    const KEY: &'static str = "app.suggestion";
}

fn default_secondary_min_rank() -> f64 {
    0.60
}

// ── ChordConfig ───────────────────────────────────────────────────────────────

/// Chord 交互分片。总开关 + 提示可见性 + 键位绑定（0.10.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChordConfig {
    #[serde(default = "default_true")]
    pub chord_enabled: bool,
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    #[serde(default)]
    pub bindings: crate::chord::ChordBindings,
}

impl Default for ChordConfig {
    fn default() -> Self {
        Self {
            chord_enabled: true,
            chord_hint_visible: true,
            bindings: crate::chord::ChordBindings::default(),
        }
    }
}

impl ConfigKey for ChordConfig {
    const KEY: &'static str = "app.chord";
}

// ── DisableConfig ─────────────────────────────────────────────────────────────

/// 三类 disable 黑名单聚合分片。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DisableConfig {
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    #[serde(default)]
    pub disabled_context_bindings: Vec<String>,
    #[serde(default)]
    pub disabled_chord_actions: Vec<String>,
}

impl ConfigKey for DisableConfig {
    const KEY: &'static str = "app.disable";
}

// ── StartMenuConfig ───────────────────────────────────────────────────────────

/// 应用搜索配置（StartMenuEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartMenuConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_3")]
    pub scan_depth: u32,
    #[serde(default = "default_true")]
    pub include_uwp: bool,
    #[serde(default = "default_true")]
    pub include_system_shortcuts: bool,
    #[serde(default)]
    pub discover_system_settings: bool,
}

impl Default for StartMenuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_depth: 3,
            include_uwp: true,
            include_system_shortcuts: true,
            discover_system_settings: false,
        }
    }
}

impl ConfigKey for StartMenuConfig {
    const KEY: &'static str = "engine:start_menu";
}

// ── CalcConfig ────────────────────────────────────────────────────────────────

/// 计算器配置（CalcEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalcConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CalcConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl ConfigKey for CalcConfig {
    const KEY: &'static str = "engine:calc";
}

// ── FileSearchConfig ──────────────────────────────────────────────────────────

/// 文件搜索配置（FileEngine）。
///
/// 数据源模式 `data_source`：
/// - `"auto"`（默认）：优先 Everything，不可用时降级本地扫描
/// - `"everything"`：只用 Everything HTTP，不可用则无文件结果
/// - `"local"`：只用本地目录扫描，不尝试 Everything
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchConfig {
    #[serde(default = "default_file_search_enabled")]
    pub enabled: bool,
    #[serde(default = "default_data_source")]
    pub data_source: String,
    #[serde(default = "default_file_search_everything_port")]
    pub everything_port: u16,
    #[serde(default = "default_file_search_max_results")]
    pub max_results: u32,
    #[serde(default = "default_local_dirs")]
    pub local_dirs: Vec<String>,
    #[serde(default = "default_local_max_depth")]
    pub local_max_depth: u32,
    #[serde(default = "default_local_cache_ttl")]
    pub local_cache_ttl_sec: u64,
    #[serde(default = "default_local_max_results")]
    pub local_max_results: u32,
}

impl Default for FileSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            data_source: "auto".to_string(),
            everything_port: 80,
            max_results: 20,
            local_dirs: default_local_dirs(),
            local_max_depth: 3,
            local_cache_ttl_sec: 300,
            local_max_results: 50,
        }
    }
}

// ── ContextConfig ─────────────────────────────────────────────────────────────

/// Context 层配置：控制唤起时的环境采集行为。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    #[serde(default = "default_true")]
    pub selection_enabled: bool,
    #[serde(default)]
    pub sensitive_apps: Vec<String>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            clipboard_enabled: true,
            selection_enabled: true,
            sensitive_apps: Vec::new(),
        }
    }
}

impl ConfigKey for ContextConfig {
    const KEY: &'static str = "context:config";
}

impl ContextConfig {
    /// 判断进程名是否在敏感应用黑名单（大小写不敏感）。
    pub fn is_sensitive(&self, process_name: &str) -> bool {
        let name = process_name.to_ascii_lowercase();
        self.sensitive_apps
            .iter()
            .any(|s| s.trim().eq_ignore_ascii_case(&name))
    }
}

// ── ScreenshotConfig ──────────────────────────────────────────────────────────

/// 截图 overlay 配置分片。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenshotConfig {
    #[serde(default = "default_true")]
    pub prewarm_ocr: bool,
    /// 长截图诊断开关（0.17.x 从 localStorage 迁移到 SQLite 配置库）。
    #[serde(default)]
    pub scroll_debug: bool,
    /// OCR 诊断开关（0.17.5：开启后截图工具栏显示 OCR 诊断按钮）。
    #[serde(default)]
    pub ocr_debug: bool,
    /// 控件级智能吸附（0.18.2：UIA 逐层 BFS 收集控件矩形，悬停吸附到子控件）。
    /// 默认开启；关闭后截图拖拽仅窗口级吸附。
    #[serde(default = "default_true")]
    pub control_snap: bool,
    /// 控件吸附 BFS 最大深度（0.18.2：控制 UIA 树遍历几层子元素）。
    /// 默认 15。范围 1-20。值越大能识别更深层的控件，但 COM 调用更多、超时风险更大。
    #[serde(default = "default_control_snap_depth")]
    pub control_snap_depth: u32,
    /// 控件吸附超时毫秒数（0.18.2：BFS deadline，超时后返回已收集的部分结果）。
    /// 默认 1000。异步收集不阻塞 overlay，宽松超时让更多应用在 budget 内到达有用控件层。
    #[serde(default = "default_control_snap_deadline_ms")]
    pub control_snap_deadline_ms: u32,
    /// 控件吸附最小尺寸（0.18.2：物理像素，控件宽或高低于此值则完全跳过：不收集为 hint 也不展开子树）。
    /// 默认 50。范围 1-200。跳过微型控件以节省 COM 调用预算。
    #[serde(default = "default_control_snap_min_size")]
    pub control_snap_min_size: u32,
    /// 窗口边缘吸附半径（0.21.x：控件级吸附开启时，鼠标落在窗口四边 R px 内吸附整个窗口，
    /// 保证在控件铺满窗口时仍能选到整窗）。默认 10。范围 1-200。
    #[serde(default = "default_window_edge_snap")]
    pub window_edge_snap: u32,
    /// 预选挖洞遮罩（0.25.14：智能吸附预选框内部不再被整屏暗罩盖住，框内提亮、
    /// 框外保持暗罩，对标 PixPin）。默认开启；关闭后回退整屏暗罩视觉。
    #[serde(default = "default_true")]
    pub preselection_cutout: bool,
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self {
            prewarm_ocr: true,
            scroll_debug: false,
            ocr_debug: false,
            control_snap: true,
            control_snap_depth: default_control_snap_depth(),
            control_snap_deadline_ms: default_control_snap_deadline_ms(),
            control_snap_min_size: default_control_snap_min_size(),
            window_edge_snap: default_window_edge_snap(),
            preselection_cutout: true,
        }
    }
}

impl ConfigKey for ScreenshotConfig {
    const KEY: &'static str = "screenshot:config";
}

// ── 默认值函数 ─────────────────────────────────────────────────────────────────

fn default_language() -> String {
    "zh".to_string()
}

fn default_log_level() -> String {
    "error".to_string()
}

fn default_surface_takeover_enabled() -> bool {
    true
}

fn default_window_opacity() -> f64 {
    1.0
}

fn default_theme() -> String {
    "auto".to_string()
}

fn default_true() -> bool {
    true
}

fn default_false() -> bool {
    false
}

fn default_30() -> u32 {
    30
}

fn default_50() -> u32 {
    50
}

fn default_page_size() -> u32 {
    9
}

fn default_3() -> u32 {
    3
}

fn default_5() -> u32 {
    5
}

fn default_control_snap_depth() -> u32 {
    15
}

fn default_control_snap_deadline_ms() -> u32 {
    1000
}

fn default_control_snap_min_size() -> u32 {
    50
}

fn default_window_edge_snap() -> u32 {
    10
}

fn default_autosuggest_min_score() -> f64 {
    0.7
}

fn default_autosuggest_tab_key() -> String {
    "Tab".to_string()
}

fn default_file_search_enabled() -> bool {
    true
}

fn default_data_source() -> String {
    "auto".to_string()
}

fn default_file_search_everything_port() -> u16 {
    80
}

fn default_file_search_max_results() -> u32 {
    20
}

fn default_local_dirs() -> Vec<String> {
    vec![
        "Desktop".to_string(),
        "Documents".to_string(),
        "Downloads".to_string(),
        "StartMenu".to_string(),
    ]
}

fn default_local_max_depth() -> u32 {
    3
}

fn default_local_cache_ttl() -> u64 {
    300
}

fn default_local_max_results() -> u32 {
    50
}

// ── AiPermissionConfig（0.17.8）──────────────────────────────────────────────────

/// AI 权限记忆配置分片——第 9 个 KV，key = `"app.ai_permission"`。
///
/// 控制 AI 危险操作确认的跨会话持久化记忆行为。
/// 独立于 `AIConfig`（第 7 分片），因为权限记忆是用户安全偏好，
/// 不应与 AI provider/model 配置耦合。
///
/// **设计**（见 phases/0.17-enhancement-polish.md §3.10 定案 4）：
/// - `memory_enabled`：总开关，默认 true。关闭时不查 DB，DB 数据保留。
/// - `memory_days`：记忆天数，默认 7。太短频繁确认体验差，太长安全性降低。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiPermissionConfig {
    /// 权限记忆总开关。默认 true——用户确认一次后在配置天数内不再重复询问。
    /// 关闭时 `is_trusted` 不查 DB 直接返回 false，DB 数据保留（重新开启后恢复）。
    #[serde(default = "default_true")]
    pub memory_enabled: bool,

    /// 记忆天数。默认 7 天。
    #[serde(default = "default_permission_days")]
    pub memory_days: u64,
}

impl Default for AiPermissionConfig {
    fn default() -> Self {
        Self {
            memory_enabled: true,
            memory_days: default_permission_days(),
        }
    }
}

impl ConfigKey for AiPermissionConfig {
    const KEY: &'static str = "app.ai_permission";
}

fn default_permission_days() -> u64 {
    7
}

// ── AiCapabilityAccessConfig（0.21.5）──────────────────────────────────────────

/// AI Capability 出口授权配置分片——key = `"ai.capability_access"`。
///
/// 控制哪些 Capability 可以进入 AI tool 池。
///
/// **设计**（见 phases/0.21 §3.4）：
/// - `schema_version`：配置 schema 版本，当前固定为 1。
/// - `profile`：初始化来源标记（`"recommended"` = 首次升级自动生成）。
///   只记录初始化来源，不在每次启动重新套默认；一旦持久化，后续只读 `enabled_capabilities`。
/// - `enabled_capabilities`：用户授权的 Capability id 集合，是 AI tool 池的唯一真源。
///
/// **推荐集合生成规则**（§3.4）：
/// - 代码允许 `LocalAi` 且 `DangerClass::Safe` 的普通生产能力默认开启。
/// - Safe + sensitive 的普通读取能力也默认开启，但调用时仍走 sensitive 确认。
/// - Dangerous、仅本地、诊断信息采集和诊断恢复类默认关闭。
/// - 用户修改后以持久化的 capability id 集合为真源；未来新增 Capability 不自动进入已有用户的 allowlist。
/// - 纯对话模式仍为空 tool 池.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiCapabilityAccessConfig {
    /// Schema 版本。当前固定为 1。
    #[serde(default = "default_ai_access_schema_version")]
    pub schema_version: u32,

    /// 初始化来源标记。`"recommended"` = 首次升级自动生成推荐集合。
    /// 只记录来源，不重新套默认。
    #[serde(default = "default_ai_access_profile")]
    pub profile: String,

    /// 用户授权的 Capability id 集合——AI tool 池的唯一真源。
    #[serde(default)]
    pub enabled_capabilities: Vec<String>,

    /// 种子状态标记。`false` = 未初始化，需要首次生成推荐集合；
    /// `true` = 已生成过推荐集合，用户关闭全部能力后应保持为空。
    /// 0.21.11 新增，解决用户清空 allowlist 后重启又被重新填充的问题。
    #[serde(default)]
    pub seeded: bool,
}

impl Default for AiCapabilityAccessConfig {
    fn default() -> Self {
        Self {
            schema_version: default_ai_access_schema_version(),
            profile: default_ai_access_profile(),
            enabled_capabilities: Vec::new(),
            seeded: false,
        }
    }
}

impl ConfigKey for AiCapabilityAccessConfig {
    const KEY: &'static str = "ai.capability_access";
}

fn default_ai_access_schema_version() -> u32 {
    1
}

fn default_ai_access_profile() -> String {
    "recommended".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 0.24 §3.4/§3.8/§5.6：SuggestionConfig 默认值钉死——展示策略开关组全开、
    /// 降频默认开、secondary 门槛 0.60（精确挡掉 0.55 兜底）。
    #[test]
    fn suggestion_config_defaults() {
        let c = SuggestionConfig::default();
        assert!(c.autosuggest_enabled);
        assert!((c.autosuggest_min_score - 0.7).abs() < 1e-9);
        assert_eq!(c.autosuggest_tab_key, "Tab");
        assert!((c.secondary_min_rank - 0.60).abs() < 1e-9);
        assert!(c.suppress_repeated);
        assert!(c.completion_enabled);
        assert!(c.context_suggestion_enabled);
        assert!(c.ai_suggestion_enabled);
        assert!(c.secondary_enabled);
        assert!(!c.proactive_enabled);
        assert_eq!(c.empty_query_topn, 5);
    }

    /// 旧格式 KV（0.24.5 前只有 enabled/min_score/tab_key）反序列化不得误关新特性：
    /// 五个新开关与 secondary_min_rank 走 serde 默认（phase §5.6 设置投影验收）。
    #[test]
    fn suggestion_config_legacy_json_keeps_new_defaults() {
        let legacy = r#"{"autosuggest_enabled":true,"autosuggest_min_score":0.8,"autosuggest_tab_key":"ArrowRight"}"#;
        let c: SuggestionConfig = serde_json::from_str(legacy).unwrap();
        assert!((c.autosuggest_min_score - 0.8).abs() < 1e-9);
        assert_eq!(c.autosuggest_tab_key, "ArrowRight");
        assert!(c.suppress_repeated);
        assert!(c.completion_enabled);
        assert!(c.context_suggestion_enabled);
        assert!(c.ai_suggestion_enabled);
        assert!(c.secondary_enabled);
        assert!((c.secondary_min_rank - 0.60).abs() < 1e-9);
    }

    /// 空 JSON / 部分字段 → 全默认（KV 损坏时的 `unwrap_or_default` 同语义兜底）。
    #[test]
    fn suggestion_config_empty_json_defaults() {
        let c: SuggestionConfig = serde_json::from_str("{}").unwrap();
        assert_eq!(c, SuggestionConfig::default());
    }

    /// 0.25.14：旧分片（无 preselection_cutout 字段）反序列化必须默认开启挖洞，
    /// 升级零迁移；显式 false 保持关闭。
    #[test]
    fn screenshot_config_legacy_json_keeps_cutout_default() {
        let legacy = r#"{"prewarm_ocr":true,"window_edge_snap":10}"#;
        let c: ScreenshotConfig = serde_json::from_str(legacy).unwrap();
        assert!(c.preselection_cutout, "旧配置缺字段应默认开启挖洞遮罩");
        let off = r#"{"prewarm_ocr":true,"preselectionCutout":false}"#;
        let c: ScreenshotConfig = serde_json::from_str(off).unwrap();
        assert!(!c.preselection_cutout, "显式关闭（camelCase）应保持关闭");
        let empty: ScreenshotConfig = serde_json::from_str("{}").unwrap();
        let default = ScreenshotConfig::default();
        assert_eq!(empty.prewarm_ocr, default.prewarm_ocr);
        assert_eq!(empty.control_snap, default.control_snap);
        assert_eq!(empty.window_edge_snap, default.window_edge_snap);
        assert_eq!(empty.preselection_cutout, default.preselection_cutout);
    }
}
