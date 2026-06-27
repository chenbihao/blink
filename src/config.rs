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

fn default_5() -> u32 {
    5
}

// ── 文件搜索配置 ──────────────────────────────────────────────────────────────────

fn default_file_search_enabled() -> bool {
    true
}

fn default_file_search_everything_port() -> u16 {
    80
}

fn default_file_search_depth() -> u32 {
    3
}

fn default_file_search_max_results() -> u32 {
    20
}

/// 文件搜索配置。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSearchConfig {
    /// 是否启用文件搜索
    #[serde(default = "default_file_search_enabled")]
    pub enabled: bool,
    /// Everything HTTP Server 端口
    #[serde(default = "default_file_search_everything_port")]
    pub everything_port: u16,
    /// 本地扫描深度
    #[serde(default = "default_file_search_depth")]
    pub local_scan_depth: u32,
    /// 每次检索最大结果数
    #[serde(default = "default_file_search_max_results")]
    pub max_results: u32,
}

impl Default for FileSearchConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            everything_port: 80,
            local_scan_depth: 3,
            max_results: 20,
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
    /// 是否启用主动建议（空 query 历史 top-N）
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    /// 空 query 显示历史常用数
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
    /// ⚠️ 兼容层：旧版本 file_search 配置（0.5 已迁移到 engine:file_search，0.6 移除）
    #[serde(default)]
    #[deprecated = "已迁移到 engine:file_search，使用 get_file_search_config() 读取"]
    pub file_search: FileSearchConfig,
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
            proactive_enabled: default_false(),
            empty_query_topn: default_5(),
            file_search: FileSearchConfig::default(),
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
}

impl From<&AppConfig> for GeneralConfig {
    fn from(c: &AppConfig) -> Self {
        Self {
            theme: c.theme.clone(),
            search_history_enabled: c.search_history_enabled,
            search_history_days: c.search_history_days,
            max_results: c.max_results,
        }
    }
}

// ── 配置操作函数 ────────────────────────────────────────────────────────────────

/// 初始化配置：如果配置不存在，写入默认值（首次运行）。
/// 语言默认值按系统语言推断（中文系→zh，其余→en）；仅首次生效，用户在设置页
/// 改过后以此为准。
pub async fn init_config(pool: &SqlitePool) -> Result<(), String> {
    let existing = crate::history::get_all_config(pool).await;
    if existing.is_empty() {
        let mut config = AppConfig::default();
        config.language = crate::locale::detect_system_language();
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
    let config_json = crate::history::get_config(pool, "app_config").await;
    match config_json {
        Some(json) => serde_json::from_str(&json).unwrap_or_default(),
        None => AppConfig::default(),
    }
}

/// 保存完整配置。
pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::history::set_config(pool, "app_config", &json).await;
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

/// 更新通用配置（主题 / 搜索历史 / 结果数）。仅持久化；
/// max_results 的运行时热更新由命令层通知 SearchService（热路径零 IO），
/// theme 由各窗口启动/shown 时读 config 生效（设置页本身即时预览）。
pub async fn update_general_config(pool: &SqlitePool, general: &GeneralConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.theme = general.theme.clone();
    config.search_history_enabled = general.search_history_enabled;
    config.search_history_days = general.search_history_days;
    config.max_results = general.max_results;
    save_config(pool, &config).await
}

/// 获取引擎配置（通用 API）。
pub async fn get_engine_config(pool: &SqlitePool, engine_id: &str) -> Option<serde_json::Value> {
    let key = format!("engine:{}", engine_id);
    crate::history::get_config(pool, &key).await.and_then(|json| {
        serde_json::from_str(&json).ok()
    })
}

/// 更新引擎配置（通用 API）。
pub async fn set_engine_config(pool: &SqlitePool, engine_id: &str, config: &serde_json::Value) -> Result<(), String> {
    let key = format!("engine:{}", engine_id);
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::history::set_config(pool, &key, &json).await;
    Ok(())
}

/// ⚠️ 兼容层：获取文件搜索配置（优先读 engine:file_search，降级读旧 app_config.file_search）。
pub async fn get_file_search_config(pool: &SqlitePool) -> FileSearchConfig {
    // 1. 优先读新 key
    if let Some(cfg) = get_engine_config(pool, "file_search").await {
        if let Ok(fs_cfg) = serde_json::from_value(cfg) {
            return fs_cfg;
        }
    }
    // 2. 降级读旧 app_config.file_search
    let app_config = get_config(pool).await;
    app_config.file_search
}

/// 更新文件搜索配置（写入 engine:file_search，保留旧 app_config.file_search 兼容）。
pub async fn update_file_search(pool: &SqlitePool, file_search: FileSearchConfig) -> Result<(), String> {
    // 写入新 key（主配置）
    let engine_json = serde_json::to_value(file_search.clone()).map_err(|e| e.to_string())?;
    set_engine_config(pool, "file_search", &engine_json).await?;

    // 兼容：同时更新旧字段（确保 0.4 版本回退也能用）
    let mut config = get_config(pool).await;
    config.file_search = file_search;
    save_config(pool, &config).await?;

    tracing::debug!("文件搜索配置已更新");
    Ok(())
}

// ── 插件配置（0.5.1，见 0.5 设计 §2.4）───────────────────────────────────────────
//
// PluginConfig 只管 enabled + settings。trigger/surface 不在此——继续由 manifest
// 单一来源驱动 RuleRouter（详见 0.5 §2.4 评审修订说明）。

fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

/// 插件独立配置。settings 是 free-form JSON（manifest 声明 schema,core 只存不解释）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settings: serde_json::Value::Null,
        }
    }
}

/// 获取单个插件配置（key=`plugin:{id}`）。不存在返回 None。
pub async fn get_plugin_config(pool: &SqlitePool, plugin_id: &str) -> Option<PluginConfig> {
    let key = format!("plugin:{plugin_id}");
    crate::history::get_config(pool, &key)
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
    crate::history::set_config(pool, &key, &json).await;
    tracing::debug!(plugin_id, enabled = config.enabled, "插件配置已更新");
    Ok(())
}

/// 获取所有插件配置（key 前缀 `plugin:`）。返回 (plugin_id, config) 列表。
pub async fn get_all_plugin_config(pool: &SqlitePool) -> Vec<(String, PluginConfig)> {
    crate::history::get_all_config(pool)
        .await
        .into_iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("plugin:")?;
            let cfg = serde_json::from_str(&v).ok()?;
            Some((id.to_string(), cfg))
        })
        .collect()
}

// ── Context 层配置（0.5.2，见 0.5 设计 §2.5）───────────────────────────────────

/// Context 层配置：控制唤起时的环境采集行为。
/// - enabled: 总开关，关闭后完全不采集
/// - clipboard_enabled: 是否采集剪贴板文本
/// - sensitive_apps: 敏感应用进程名黑名单（如密码管理器），前台为这些应用时不采集（隐私保护）
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub clipboard_enabled: bool,
    #[serde(default)]
    pub sensitive_apps: Vec<String>,
}

impl Default for ContextConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            clipboard_enabled: true,
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
    crate::history::get_config(pool, "context:config")
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
    crate::history::set_config(pool, "context:config", &json).await;
    tracing::debug!(
        enabled = config.enabled,
        clipboard = config.clipboard_enabled,
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