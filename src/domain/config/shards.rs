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

/// 外观配置分片。theme / language / auto_start + log_level + window_opacity + ai_verbose_log + first_run。
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
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
    #[serde(default = "default_false")]
    pub ai_verbose_log: bool,
    /// 0.17.3：首次启动标记，default true。老用户升级时 serde 取 default true，
    /// 会看到一次引导窗口——可接受（看一次快捷键速查也有价值）。
    #[serde(default = "default_true")]
    pub first_run: bool,
}

impl Default for AppearanceConfig {
    fn default() -> Self {
        Self {
            theme: default_theme(),
            language: default_language(),
            auto_start: false,
            log_level: default_log_level(),
            window_opacity: default_window_opacity(),
            ai_verbose_log: false,
            first_run: true,
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

/// 建议行为分片。autosuggest + proactive。
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

impl Default for SuggestionConfig {
    fn default() -> Self {
        Self {
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
            proactive_enabled: false,
            empty_query_topn: 5,
        }
    }
}

impl ConfigKey for SuggestionConfig {
    const KEY: &'static str = "app.suggestion";
}

// ── ChordConfig ───────────────────────────────────────────────────────────────

/// Chord 交互分片。总开关 + 提示可见性 + 键位绑定（0.10.7）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChordConfig {
    #[serde(default = "default_false")]
    pub chord_enabled: bool,
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    #[serde(default)]
    pub bindings: crate::domain::chord::ChordBindings,
}

impl Default for ChordConfig {
    fn default() -> Self {
        Self {
            chord_enabled: false,
            chord_hint_visible: true,
            bindings: crate::domain::chord::ChordBindings::default(),
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
}

impl Default for StartMenuConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            scan_depth: 3,
            include_uwp: true,
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
}

impl Default for ScreenshotConfig {
    fn default() -> Self {
        Self { prewarm_ocr: true, scroll_debug: false, ocr_debug: false }
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
