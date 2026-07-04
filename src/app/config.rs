//! 配置管理：SQLite 持久化 + 默认值 + 类型安全。
//!
//! 配置存储在 SQLite 的 `config` 表中，格式为 (key, value, updated_at)。
//! 本模块提供类型安全的配置读写，以及默认值处理。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

// ── 配置结构体 ──────────────────────────────────────────────────────────────────

/// 快捷键配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    /// 修饰键列表（ctrl, shift, alt, meta/win）
    pub modifiers: Vec<String>,
    /// 主键（字母、数字、功能键等）
    pub key: String,
    /// 显示名称（如 "RightAlt", "Ctrl+Shift+Space"）
    pub display: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            modifiers: vec!["alt".to_string()],
            key: " ".to_string(),
            display: "Alt+Space".to_string(),
        }
    }
}

/// 日志级别默认值（旧配置无此字段时用 serde default 补，不丢其他配置）。
fn default_log_level() -> String {
    "error".to_string()
}

fn default_surface_takeover_enabled() -> bool {
    true
}

// ── AppConfig 新增字段默认值（0.5） ────────────────────────────────────────────────

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

// ── Autosuggestion 默认值（0.8.1 §2.8）────────────────────────────────────────

fn default_autosuggest_min_score() -> f64 {
    0.7
}

fn default_autosuggest_tab_key() -> String {
    "Tab".to_string()
}

// ── 搜索引擎配置（三层独立控制）────────────────────────────────────────────────

/// 应用搜索配置（StartMenuEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StartMenuConfig {
    /// 是否启用应用搜索
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 开始菜单扫描深度
    #[serde(default = "default_3")]
    pub scan_depth: u32,
    /// 是否包含 UWP/MSIX 应用（通过 shell:AppsFolder 枚举）
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

/// 计算器配置（CalcEngine）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CalcConfig {
    /// 是否启用计算器
    #[serde(default = "default_true")]
    pub enabled: bool,
}

impl Default for CalcConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
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

/// 文件搜索配置（FileEngine）。
///
/// 数据源模式 `data_source`：
/// - `"auto"`（默认）：优先 Everything，不可用时降级本地扫描
/// - `"everything"`：只用 Everything HTTP，不可用则无文件结果
/// - `"local"`：只用本地目录扫描，不尝试 Everything
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchConfig {
    /// 是否启用文件搜索（总开关）
    #[serde(default = "default_file_search_enabled")]
    pub enabled: bool,
    /// 数据源模式：auto / everything / local
    #[serde(default = "default_data_source")]
    pub data_source: String,
    /// Everything HTTP Server 端口
    #[serde(default = "default_file_search_everything_port")]
    pub everything_port: u16,
    /// 每次检索最大结果数（Everything）
    #[serde(default = "default_file_search_max_results")]
    pub max_results: u32,
    /// 本地扫描目录列表
    #[serde(default = "default_local_dirs")]
    pub local_dirs: Vec<String>,
    /// 本地扫描最大深度
    #[serde(default = "default_local_max_depth")]
    pub local_max_depth: u32,
    /// 本地缓存有效期（秒）
    #[serde(default = "default_local_cache_ttl")]
    pub local_cache_ttl_sec: u64,
    /// 本地搜索最多返回数
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

/// 应用配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    /// 快捷键配置
    pub hotkey: HotkeyConfig,
    /// tap 阈值（毫秒）
    pub tap_threshold: u64,
    /// 看门狗 grace period（毫秒）
    pub grace_period: u64,
    /// 开机自启
    pub auto_start: bool,
    /// 语言（zh/en）
    pub language: String,
    /// 日志级别（error/info/debug/trace）
    #[serde(default = "default_log_level")]
    pub log_level: String,
    /// 是否允许插件 takeover(接管返回区);false 时所有 takeover 降级 priority。
    #[serde(default = "default_surface_takeover_enabled")]
    pub surface_takeover_enabled: bool,
    /// 主题（auto/light/dark）
    #[serde(default = "default_theme")]
    pub theme: String,
    /// 是否记录搜索历史
    #[serde(default = "default_true")]
    pub search_history_enabled: bool,
    /// 搜索历史保留天数
    #[serde(default = "default_30")]
    pub search_history_days: u32,
    /// 最多显示结果数
    #[serde(default = "default_50")]
    pub max_results: u32,
    /// 每页显示结果数
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    /// 是否启用主动建议（空 query 历史 top-N）
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    /// 空 query 显示历史常用数
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
    /// 剪贴板历史配置
    #[serde(default)]
    pub clipboard: crate::infra::data::clipboard::ClipboardConfig,
    /// 用户禁用的内置动作 id 列表（0.8.0 §1.3）。
    /// 存 action id（如 `"shutdown"`），BuiltinEngine 召回时跳过。
    /// 默认空——所有动作默认启用；用户在设置页勾选后追加。
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    /// 是否启用 Autosuggestion / Ghost Text（0.8.1 §2.8）。默认 true。
    /// 关闭后 `SearchService::search` 恒返回 `completion_hint: None`。
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    /// Autosuggest fuzzy 阈值（0.8.1 §2.8），归一化到 [0,1]。默认 0.7。
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    /// Autosuggest 接受补全的键位（0.8.1 §2.8）。默认 "Tab"。
    /// 前端消费；后端仅持久化 + 广播回前端。可选值当前仅 `"Tab"` / `"ArrowRight"`。
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            hotkey: HotkeyConfig::default(),
            tap_threshold: 300,
            grace_period: 500,
            auto_start: false,
            language: "zh".to_string(),
            log_level: "error".to_string(),
            surface_takeover_enabled: true,
            theme: default_theme(),
            search_history_enabled: default_true(),
            search_history_days: default_30(),
            max_results: default_50(),
            page_size: default_page_size(),
            proactive_enabled: default_false(),
            empty_query_topn: default_5(),
            clipboard: crate::infra::data::clipboard::ClipboardConfig::default(),
            disabled_builtin_actions: Vec::new(),
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
        }
    }
}

/// 通用配置（0.5）：用户可调的外观与行为项。
/// 聚合更新（`update_general_config`）避免单字段命令爆炸。
/// `proactive_enabled` / `empty_query_topn` 属 P3 主动建议，暂不纳入（字段仍保留在 AppConfig）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    /// 主题：auto / light / dark
    pub theme: String,
    /// 是否记录搜索历史
    pub search_history_enabled: bool,
    /// 搜索历史保留天数（0 = 永久保留）
    pub search_history_days: u32,
    /// 融合后返回前端的最大结果数
    pub max_results: u32,
    /// 每页显示结果数
    pub page_size: u32,
}

impl From<&AppConfig> for GeneralConfig {
    fn from(c: &AppConfig) -> Self {
        Self {
            theme: c.theme.clone(),
            search_history_enabled: c.search_history_enabled,
            search_history_days: c.search_history_days,
            max_results: c.max_results,
            page_size: c.page_size,
        }
    }
}

// ── 配置操作函数 ────────────────────────────────────────────────────────────────

/// 初始化配置：如果配置不存在，写入默认值（首次运行）。
/// 语言默认值按系统语言推断（中文系→zh，其余→en）；仅首次生效，用户在设置页
/// 改过后以此为准。
pub async fn init_config(pool: &SqlitePool) -> Result<(), String> {
    let existing = crate::infra::data::history::get_all_config(pool).await;
    if existing.is_empty() {
        let mut config = AppConfig::default();
        config.language = crate::infra::platform::locale::detect_system_language();
        tracing::info!(language = %config.language, "首次运行，按系统语言设置默认语言");
        save_config(pool, &config).await?;
    }

    // 一次性迁移：修正旧版默认值 key:"space" → key:" "（空格字符，匹配 vk_to_key）。
    // 仅当热键 display 含 "Space" 且 key 是错误的 "space" 时修正。
    {
        let mut config = get_config(pool).await;
        if config.hotkey.key == "space" && config.hotkey.display.contains("Space") {
            config.hotkey.key = " ".to_string();
            save_config(pool, &config).await?;
            tracing::info!("迁移：修正热键 key 'space' → ' '");
        }
    }

    Ok(())
}

/// 获取完整配置。
pub async fn get_config(pool: &SqlitePool) -> AppConfig {
    let config_json = crate::infra::data::history::get_config(pool, "app_config").await;
    match config_json {
        Some(json) => serde_json::from_str(&json).unwrap_or_default(),
        None => AppConfig::default(),
    }
}

/// 保存完整配置。
pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, "app_config", &json).await;
    Ok(())
}

/// 更新快捷键配置。
pub async fn update_hotkey(pool: &SqlitePool, hotkey: HotkeyConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.hotkey = hotkey;
    save_config(pool, &config).await
}

/// 更新 tap 阈值。
pub async fn update_tap_threshold(pool: &SqlitePool, threshold: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.tap_threshold = threshold;
    save_config(pool, &config).await
}

/// 更新 grace period。
pub async fn update_grace_period(pool: &SqlitePool, period: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.grace_period = period;
    save_config(pool, &config).await
}

/// 更新开机自启设置。
pub async fn update_auto_start(pool: &SqlitePool, auto_start: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.auto_start = auto_start;
    save_config(pool, &config).await
}

/// 更新语言设置。
pub async fn update_language(pool: &SqlitePool, language: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.language = language;
    save_config(pool, &config).await
}

/// 更新日志级别。
pub async fn update_log_level(pool: &SqlitePool, level: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.log_level = level;
    save_config(pool, &config).await
}

// ── 内置动作 disable 列表（0.8.0 §1.3）──────────────────────────────────────────

/// 获取当前 disable 的内置动作 id 列表（快照读）。
///
/// 设置页初始化时读一次（`list_builtin_actions` command），启动时读一次注入到
/// SearchService 内存快照。
pub async fn get_disabled_builtin_actions(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_builtin_actions
}

/// 更新 disable 列表（设置页勾选后调用）。**幂等**：内部去重排序。
pub async fn update_disabled_builtin_actions(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_builtin_actions = normalized;
    save_config(pool, &config).await
}

// ── Autosuggestion 配置（0.8.1 §2.8）──────────────────────────────────────────

/// 更新 Autosuggestion 配置（设置页调用）。
///
/// 命令层调完后应同步调用 `SearchService::update_autosuggest_config` 让搜索热路径
/// 即时生效。tab_key 只影响前端键位监听，无热更新副作用。
pub async fn update_autosuggest_config(
    pool: &SqlitePool,
    enabled: bool,
    min_score: f64,
    tab_key: String,
) -> Result<(), String> {
    // 阈值夹到 [0, 1]，避免设置页误传导致命中永不触发
    let min_score = min_score.clamp(0.0, 1.0);
    let mut config = get_config(pool).await;
    config.autosuggest_enabled = enabled;
    config.autosuggest_min_score = min_score;
    config.autosuggest_tab_key = tab_key;
    save_config(pool, &config).await
}

/// 更新通用配置（主题 / 搜索历史 / 结果数）。仅持久化；
/// max_results 的运行时热更新由命令层通知 SearchService（热路径零 IO），
/// theme 由各窗口启动/shown 时读 config 生效（设置页本身即时预览）。
pub async fn update_general_config(pool: &SqlitePool, general: &GeneralConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.theme = general.theme.clone();
    config.search_history_enabled = general.search_history_enabled;
    config.search_history_days = general.search_history_days;
    config.max_results = general.max_results;
    config.page_size = general.page_size;
    save_config(pool, &config).await
}

/// 获取引擎配置（通用 API）。
pub async fn get_engine_config(pool: &SqlitePool, engine_id: &str) -> Option<serde_json::Value> {
    let key = format!("engine:{}", engine_id);
    crate::infra::data::history::get_config(pool, &key).await.and_then(|json| {
        serde_json::from_str(&json).ok()
    })
}

/// 更新引擎配置（通用 API）。
pub async fn set_engine_config(pool: &SqlitePool, engine_id: &str, config: &serde_json::Value) -> Result<(), String> {
    let key = format!("engine:{}", engine_id);
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json).await;
    Ok(())
}

/// 获取文件搜索配置（key=`engine:file_search`）。不存在返回默认。
pub async fn get_file_search_config(pool: &SqlitePool) -> FileSearchConfig {
    get_engine_config(pool, "file_search")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新文件搜索配置（写入 engine:file_search）。
pub async fn update_file_search(pool: &SqlitePool, file_search: FileSearchConfig) -> Result<(), String> {
    let engine_json = serde_json::to_value(file_search).map_err(|e| e.to_string())?;
    set_engine_config(pool, "file_search", &engine_json).await?;
    tracing::debug!("文件搜索配置已更新");
    Ok(())
}

// ── 应用搜索配置（0.8，engine:start_menu）──────────────────────────────────────

/// 获取应用搜索配置（key=`engine:start_menu`）。不存在返回默认。
pub async fn get_start_menu_config(pool: &SqlitePool) -> StartMenuConfig {
    get_engine_config(pool, "start_menu")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新应用搜索配置。
pub async fn update_start_menu_config(pool: &SqlitePool, config: &StartMenuConfig) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "start_menu", &json).await?;
    tracing::debug!(enabled = config.enabled, scan_depth = config.scan_depth, "应用搜索配置已更新");
    Ok(())
}

// ── 计算器配置（0.8，engine:calc）──────────────────────────────────────────────

/// 获取计算器配置（key=`engine:calc`）。不存在返回默认。
pub async fn get_calc_config(pool: &SqlitePool) -> CalcConfig {
    get_engine_config(pool, "calc")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

/// 更新计算器配置。
pub async fn update_calc_config(pool: &SqlitePool, config: &CalcConfig) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "calc", &json).await?;
    tracing::debug!(enabled = config.enabled, "计算器配置已更新");
    Ok(())
}

// ── 插件配置（0.5.1，见 0.5 设计 §2.4）───────────────────────────────────────────
//
// PluginConfig 只管 enabled + settings。trigger/surface 不在此——继续由 manifest
// 单一来源驱动 RuleRouter（详见 0.5 §2.4 评审修订说明）。

fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

/// 用户自定义触发关键字。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTrigger {
    /// 触发关键字
    pub keyword: String,
    /// 是否启用
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// 呈现模式覆盖：auto/inline/priority/takeover（None 用默认）
    #[serde(default)]
    pub surface: Option<String>,
}

/// 插件独立配置。settings 是 free-form JSON（manifest 声明 schema,core 只存不解释）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "CompatPluginConfig")]
pub struct PluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    /// 已禁用的默认触发词列表（用户 ban 掉的）
    #[serde(default)]
    pub disabled_default_triggers: Vec<String>,
    /// 用户自定义触发关键字
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

/// 兼容旧配置格式（用于数据迁移：disable_default_triggers: bool → disabled_default_triggers: Vec<String>）
#[derive(Debug, Deserialize)]
struct CompatPluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    // 旧格式：bool（是否禁用所有默认触发词）
    #[serde(default)]
    pub disable_default_triggers: Option<bool>,
    // 新格式：Vec<String>（被 ban 的具体触发词列表）
    #[serde(default)]
    pub disabled_default_triggers: Option<Vec<String>>,
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

impl From<CompatPluginConfig> for PluginConfig {
    fn from(compat: CompatPluginConfig) -> Self {
        // 优先用新格式；如果是旧格式且为 true，则暂时用空列表（下次保存时自动迁移）
        let disabled_default_triggers = compat
            .disabled_default_triggers
            .unwrap_or_default();

        Self {
            enabled: compat.enabled,
            settings: compat.settings,
            disabled_default_triggers,
            custom_triggers: compat.custom_triggers,
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settings: serde_json::Value::Null,
            disabled_default_triggers: Vec::new(),
            custom_triggers: Vec::new(),
        }
    }
}

/// 获取单个插件配置（key=`plugin:{id}`）。不存在返回 None。
pub async fn get_plugin_config(pool: &SqlitePool, plugin_id: &str) -> Option<PluginConfig> {
    let key = format!("plugin:{plugin_id}");
    crate::infra::data::history::get_config(pool, &key)
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
}

/// 设置插件配置（upsert,写 `plugin:{id}`）。
pub async fn set_plugin_config(
    pool: &SqlitePool,
    plugin_id: &str,
    config: &PluginConfig,
) -> Result<(), String> {
    let key = format!("plugin:{plugin_id}");
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json).await;
    tracing::debug!(plugin_id, enabled = config.enabled, "插件配置已更新");
    Ok(())
}

/// 获取所有插件配置（key 前缀 `plugin:`）。返回 (plugin_id, config) 列表。
#[allow(dead_code)] // 插件系统骨架，0.3+ 启用
pub async fn get_all_plugin_config(pool: &SqlitePool) -> Vec<(String, PluginConfig)> {
    crate::infra::data::history::get_all_config(pool)
        .await
        .into_iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("plugin:")?;
            let cfg = serde_json::from_str(&v).ok()?;
            Some((id.to_string(), cfg))
        })
        .collect()
}

impl PluginConfig {
    /// 合并 manifest triggers 和自定义 triggers，返回最终生效列表。
    pub fn effective_triggers(
        &self,
        manifest_triggers: &[crate::domain::plugin::PluginTrigger],
    ) -> Vec<crate::domain::plugin::PluginTrigger> {
        let mut result = Vec::new();

        // 1. 加默认 triggers（排除被 ban 的）
        for trigger in manifest_triggers {
            match trigger {
                crate::domain::plugin::PluginTrigger::Keyword { keyword, .. } => {
                    if self.disabled_default_triggers.contains(keyword) {
                        continue;
                    }
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Regex { .. } => {
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Context { .. } => {
                    // Context 触发不参与 disabled_default_triggers 过滤（用户想禁用整个 Context
                    // 触发应该走「插件 disable」，而不是关键词黑名单）。原样透传。
                    result.push(trigger.clone());
                }
            }
        }

        // 2. 加自定义 triggers
        for ct in &self.custom_triggers {
            if ct.enabled {
                result.push(crate::domain::plugin::PluginTrigger::Keyword {
                    keyword: ct.keyword.clone(),
                    exclusive: true,
                });
            }
        }

        result
    }
}

// ── Context 层配置（0.5.2，见 0.5 设计 §2.5）───────────────────────────────────

/// Context 层配置：控制唤起时的环境采集行为。
/// - enabled: 总开关，关闭后完全不采集
/// - clipboard_enabled: 是否采集剪贴板文本
/// - selection_enabled: 是否启用划词感知（鼠标划选文本 → UIA 抓取 → 缓存，供 invoke 时读取）
/// - sensitive_apps: 敏感应用进程名黑名单（如密码管理器），前台为这些应用时不采集（隐私保护）
#[derive(Debug, Clone, Serialize, Deserialize)]
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

impl ContextConfig {
    /// 判断进程名是否在敏感应用黑名单（大小写不敏感）。
    pub fn is_sensitive(&self, process_name: &str) -> bool {
        let name = process_name.to_ascii_lowercase();
        self.sensitive_apps
            .iter()
            .any(|s| s.trim().eq_ignore_ascii_case(&name))
    }
}

/// 获取 Context 配置（key=`context:config`）。不存在或解析失败返回默认。
pub async fn get_context_config(pool: &SqlitePool) -> ContextConfig {
    crate::infra::data::history::get_config(pool, "context:config")
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

/// 设置 Context 配置（upsert,写 `context:config`）。
pub async fn set_context_config(
    pool: &SqlitePool,
    config: &ContextConfig,
) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, "context:config", &json).await;
    tracing::debug!(
        enabled = config.enabled,
        clipboard = config.clipboard_enabled,
        selection = config.selection_enabled,
        sensitive_count = config.sensitive_apps.len(),
        "Context 配置已更新"
    );
    Ok(())
}

// ── TODO: 方案 B - Trait 抽象（后续重构）────────────────────────────────────────
//
// 当需要支持多平台时，可以将配置管理抽象为 trait：
//
// ```rust
// pub trait ConfigManager {
//     async fn get_config(&self) -> AppConfig;
//     async fn save_config(&self, config: &AppConfig) -> Result<(), String>;
//     async fn get_hotkey(&self) -> HotkeyConfig;
//     async fn update_hotkey(&self, hotkey: HotkeyConfig) -> Result<(), String>;
//     // ... 其他配置操作
// }
//
// // 每个平台实现自己的 ConfigManager
// pub struct SqliteConfigManager { pool: SqlitePool }
// pub struct FileConfigManager { path: PathBuf }  // macOS/Linux 可能用文件
// ```
//
// 这样可以：
// 1. 支持不同的存储后端（SQLite、文件、注册表等）
// 2. 支持平台特定的配置项
// 3. 更容易进行单元测试（mock ConfigManager）
//