//! AppConfig 门面 + 配置操作函数（0.14.6 §2.1 从 `app/config.rs` 迁入）。
//!
//! `AppConfig` 是门面 struct——内部组合 6 分片 + clipboard 独立 KV。
//! init/get/save/update 操作函数供 app 层 commands 调用。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::shards::{
    AppearanceConfig, CalcConfig, ChordConfig, ContextConfig, DisableConfig, FileSearchConfig,
    HotkeyConfig, SearchConfig, StartMenuConfig, SuggestionConfig,
};
use super::store::ConfigStore;

// ── AppConfig 门面 ─────────────────────────────────────────────────────────────

/// 应用配置门面（内部组合 6 分片 + clipboard 独立 KV）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub hotkey: HotkeyConfig,
    pub tap_threshold: u64,
    pub grace_period: u64,
    pub auto_start: bool,
    pub language: String,
    #[serde(default = "default_log_level")]
    pub log_level: String,
    #[serde(default = "default_surface_takeover_enabled")]
    pub surface_takeover_enabled: bool,
    #[serde(default = "default_theme")]
    pub theme: String,
    #[serde(default = "default_true")]
    pub search_history_enabled: bool,
    #[serde(default = "default_30")]
    pub search_history_days: u32,
    #[serde(default = "default_50")]
    pub max_results: u32,
    #[serde(default = "default_page_size")]
    pub page_size: u32,
    #[serde(default = "default_false")]
    pub proactive_enabled: bool,
    #[serde(default = "default_5")]
    pub empty_query_topn: u32,
    #[serde(default)]
    pub clipboard: crate::infra::data::clipboard::ClipboardConfig,
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
    #[serde(default)]
    pub disabled_context_bindings: Vec<String>,
    #[serde(default = "default_false")]
    pub chord_enabled: bool,
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    #[serde(default)]
    pub chord_bindings: crate::domain::chord::ChordBindings,
    #[serde(default)]
    pub disabled_chord_actions: Vec<String>,
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
    #[serde(default = "default_false")]
    pub ai_verbose_log: bool,
    #[serde(default = "default_true")]
    pub first_run: bool,
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
            disabled_context_bindings: Vec::new(),
            chord_enabled: false,
            chord_hint_visible: true,
            chord_bindings: crate::domain::chord::ChordBindings::default(),
            disabled_chord_actions: Vec::new(),
            window_opacity: default_window_opacity(),
            ai_verbose_log: false,
            first_run: true,
        }
    }
}

/// 通用配置（0.5）：用户可调的外观与行为项。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GeneralConfig {
    pub theme: String,
    pub search_history_enabled: bool,
    pub search_history_days: u32,
    pub max_results: u32,
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

// ── set_config 命令辅助结构体（0.8.6 P1-C 前端泛型化）─────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AutosuggestUpdate {
    pub enabled: bool,
    pub min_score: f64,
    pub tab_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChordTogglesUpdate {
    pub chord_enabled: bool,
    pub chord_hint_visible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GlobalProxyUpdate {
    pub http: String,
    pub https: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PluginConfigUpdate {
    pub plugin_id: String,
    pub enabled: bool,
    pub settings: serde_json::Value,
}

// ── 默认值函数（AppConfig 字段用）──────────────────────────────────────────────

fn default_log_level() -> String {
    "error".to_string()
}
fn default_surface_takeover_enabled() -> bool {
    true
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
fn default_5() -> u32 {
    5
}
fn default_autosuggest_min_score() -> f64 {
    0.7
}
fn default_autosuggest_tab_key() -> String {
    "Tab".to_string()
}
fn default_window_opacity() -> f64 {
    1.0
}

// ── 配置操作函数 ────────────────────────────────────────────────────────────────

/// 初始化配置：首次运行写默认值 + 检测旧 `app_config` 单 key 触发迁移。
pub async fn init_config(pool: &SqlitePool) -> Result<(), String> {
    // Step 1: 检测旧 KV 迁移
    if let Some(json) = crate::infra::data::history::get_config(pool, "app_config").await {
        tracing::info!("检测到旧 app_config 单 key,开始迁移到分片 KV");
        let legacy: AppConfig = serde_json::from_str(&json).unwrap_or_default();
        save_config(pool, &legacy).await?;
        crate::infra::data::history::delete_config(pool, "app_config")
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("app_config 单 key 已拆分到 6 分片 + clipboard 独立 KV,旧 key 删除");
    }

    // Step 2: 首次运行
    let existing = crate::infra::data::history::get_all_config(pool).await;
    let has_any_shard = existing.contains_key("app.hotkey")
        || existing.contains_key("app.appearance")
        || existing.contains_key("app.search")
        || existing.contains_key("app.suggestion")
        || existing.contains_key("app.chord")
        || existing.contains_key("app.disable");
    if !has_any_shard {
        let mut config = AppConfig::default();
        config.language = crate::infra::platform::locale::detect_system_language();
        tracing::info!(language = %config.language, "首次运行,按系统语言设置默认语言");
        save_config(pool, &config).await?;
    }

    // Step 3: 一次性数据修正——旧版热键 key:"space" → " "
    {
        let mut config = get_config(pool).await;
        if config.hotkey.key == "space" && config.hotkey.display.contains("Space") {
            config.hotkey.key = " ".to_string();
            save_config(pool, &config).await?;
            tracing::info!("迁移:修正热键 key 'space' → ' '");
        }
    }

    Ok(())
}

/// 获取完整配置（门面 view,内部组合 6 分片 + clipboard 独立 KV）。
pub async fn get_config(pool: &SqlitePool) -> AppConfig {
    let hotkey = ConfigStore::get::<HotkeyConfig>(pool).await;
    let appearance = ConfigStore::get::<AppearanceConfig>(pool).await;
    let search = ConfigStore::get::<SearchConfig>(pool).await;
    let suggestion = ConfigStore::get::<SuggestionConfig>(pool).await;
    let chord = ConfigStore::get::<ChordConfig>(pool).await;
    let disable = ConfigStore::get::<DisableConfig>(pool).await;
    let clipboard = ConfigStore::get::<crate::infra::data::clipboard::ClipboardConfig>(pool).await;

    AppConfig {
        hotkey: HotkeyConfig {
            modifiers: hotkey.modifiers.clone(),
            key: hotkey.key.clone(),
            display: hotkey.display.clone(),
            tap_threshold: hotkey.tap_threshold,
            grace_period: hotkey.grace_period,
        },
        tap_threshold: hotkey.tap_threshold,
        grace_period: hotkey.grace_period,
        theme: appearance.theme,
        language: appearance.language,
        auto_start: appearance.auto_start,
        log_level: appearance.log_level,
        window_opacity: appearance.window_opacity,
        ai_verbose_log: appearance.ai_verbose_log,
        first_run: appearance.first_run,
        surface_takeover_enabled: search.surface_takeover_enabled,
        search_history_enabled: search.search_history_enabled,
        search_history_days: search.search_history_days,
        max_results: search.max_results,
        page_size: search.page_size,
        autosuggest_enabled: suggestion.autosuggest_enabled,
        autosuggest_min_score: suggestion.autosuggest_min_score,
        autosuggest_tab_key: suggestion.autosuggest_tab_key,
        proactive_enabled: suggestion.proactive_enabled,
        empty_query_topn: suggestion.empty_query_topn,
        chord_enabled: chord.chord_enabled,
        chord_hint_visible: chord.chord_hint_visible,
        chord_bindings: chord.bindings.clone(),
        disabled_builtin_actions: disable.disabled_builtin_actions,
        disabled_context_bindings: disable.disabled_context_bindings,
        disabled_chord_actions: disable.disabled_chord_actions,
        clipboard,
    }
}

/// 保存完整配置（拆分回 6 分片 + clipboard 独立 KV）。
pub async fn save_config(pool: &SqlitePool, config: &AppConfig) -> Result<(), String> {
    let hotkey_shard = HotkeyConfig {
        modifiers: config.hotkey.modifiers.clone(),
        key: config.hotkey.key.clone(),
        display: config.hotkey.display.clone(),
        tap_threshold: config.tap_threshold,
        grace_period: config.grace_period,
    };
    ConfigStore::set(pool, &hotkey_shard).await?;

    ConfigStore::set(
        pool,
        &AppearanceConfig {
            theme: config.theme.clone(),
            language: config.language.clone(),
            auto_start: config.auto_start,
            log_level: config.log_level.clone(),
            window_opacity: config.window_opacity,
            ai_verbose_log: config.ai_verbose_log,
            first_run: config.first_run,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &SearchConfig {
            search_history_enabled: config.search_history_enabled,
            search_history_days: config.search_history_days,
            max_results: config.max_results,
            page_size: config.page_size,
            surface_takeover_enabled: config.surface_takeover_enabled,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &SuggestionConfig {
            autosuggest_enabled: config.autosuggest_enabled,
            autosuggest_min_score: config.autosuggest_min_score,
            autosuggest_tab_key: config.autosuggest_tab_key.clone(),
            proactive_enabled: config.proactive_enabled,
            empty_query_topn: config.empty_query_topn,
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &ChordConfig {
            chord_enabled: config.chord_enabled,
            chord_hint_visible: config.chord_hint_visible,
            bindings: config.chord_bindings.clone(),
        },
    )
    .await?;

    ConfigStore::set(
        pool,
        &DisableConfig {
            disabled_builtin_actions: config.disabled_builtin_actions.clone(),
            disabled_context_bindings: config.disabled_context_bindings.clone(),
            disabled_chord_actions: config.disabled_chord_actions.clone(),
        },
    )
    .await?;

    ConfigStore::set(pool, &config.clipboard).await?;

    Ok(())
}

// ── 分项更新函数 ────────────────────────────────────────────────────────────────

pub async fn update_hotkey(pool: &SqlitePool, hotkey: HotkeyConfig) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.hotkey.modifiers = hotkey.modifiers;
    config.hotkey.key = hotkey.key;
    config.hotkey.display = hotkey.display;
    save_config(pool, &config).await
}

pub async fn update_tap_threshold(pool: &SqlitePool, threshold: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.tap_threshold = threshold;
    save_config(pool, &config).await
}

pub async fn update_grace_period(pool: &SqlitePool, period: u64) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.grace_period = period;
    save_config(pool, &config).await
}

pub async fn update_auto_start(pool: &SqlitePool, auto_start: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.auto_start = auto_start;
    save_config(pool, &config).await
}

/// 0.17.3：更新首次启动标记。镜像 update_auto_start 模式。
pub async fn update_first_run(pool: &SqlitePool, first_run: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.first_run = first_run;
    save_config(pool, &config).await
}

pub async fn update_language(pool: &SqlitePool, language: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.language = language;
    save_config(pool, &config).await
}

pub async fn update_log_level(pool: &SqlitePool, level: String) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.log_level = level;
    save_config(pool, &config).await
}

pub async fn update_ai_verbose_log(pool: &SqlitePool, verbose: bool) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.ai_verbose_log = verbose;
    save_config(pool, &config).await
}

pub async fn get_disabled_builtin_actions(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_builtin_actions
}

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

pub async fn update_autosuggest_config(
    pool: &SqlitePool,
    enabled: bool,
    min_score: f64,
    tab_key: String,
) -> Result<(), String> {
    let min_score = min_score.clamp(0.0, 1.0);
    let mut config = get_config(pool).await;
    config.autosuggest_enabled = enabled;
    config.autosuggest_min_score = min_score;
    config.autosuggest_tab_key = tab_key;
    save_config(pool, &config).await
}

#[allow(dead_code)]
pub async fn get_disabled_context_bindings(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_context_bindings
}

pub async fn update_disabled_context_bindings(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_context_bindings = normalized;
    save_config(pool, &config).await
}

pub async fn get_disabled_chord_actions(pool: &SqlitePool) -> Vec<String> {
    get_config(pool).await.disabled_chord_actions
}

pub async fn get_chord_config(pool: &SqlitePool) -> ChordConfig {
    ConfigStore::get::<ChordConfig>(pool).await
}

pub async fn update_disabled_chord_actions(
    pool: &SqlitePool,
    disabled: Vec<String>,
) -> Result<(), String> {
    let mut normalized = disabled;
    normalized.sort();
    normalized.dedup();
    let mut config = get_config(pool).await;
    config.disabled_chord_actions = normalized;
    save_config(pool, &config).await
}

#[allow(dead_code)]
pub async fn get_chord_toggles(pool: &SqlitePool) -> (bool, bool) {
    let cfg = get_config(pool).await;
    (cfg.chord_enabled, cfg.chord_hint_visible)
}

pub async fn update_chord_toggles(
    pool: &SqlitePool,
    chord_enabled: bool,
    chord_hint_visible: bool,
) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.chord_enabled = chord_enabled;
    config.chord_hint_visible = chord_hint_visible;
    save_config(pool, &config).await
}

pub async fn update_chord_bindings(
    pool: &SqlitePool,
    bindings: crate::domain::chord::ChordBindings,
) -> Result<(), String> {
    let mut chord = get_chord_config(pool).await;
    chord.bindings = bindings;
    ConfigStore::set(pool, &chord).await
}

pub async fn update_general_config(
    pool: &SqlitePool,
    general: &GeneralConfig,
) -> Result<(), String> {
    let mut config = get_config(pool).await;
    config.theme = general.theme.clone();
    config.search_history_enabled = general.search_history_enabled;
    config.search_history_days = general.search_history_days;
    config.max_results = general.max_results;
    config.page_size = general.page_size;
    save_config(pool, &config).await
}

// ── 引擎配置（通用 API）─────────────────────────────────────────────────────────

pub async fn get_engine_config(pool: &SqlitePool, engine_id: &str) -> Option<serde_json::Value> {
    let key = format!("engine:{}", engine_id);
    crate::infra::data::history::get_config(pool, &key)
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
}

pub async fn set_engine_config(
    pool: &SqlitePool,
    engine_id: &str,
    config: &serde_json::Value,
) -> Result<(), String> {
    let key = format!("engine:{}", engine_id);
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub async fn get_file_search_config(pool: &SqlitePool) -> FileSearchConfig {
    get_engine_config(pool, "file_search")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

pub async fn update_file_search(
    pool: &SqlitePool,
    file_search: FileSearchConfig,
) -> Result<(), String> {
    let engine_json = serde_json::to_value(file_search).map_err(|e| e.to_string())?;
    set_engine_config(pool, "file_search", &engine_json).await?;
    tracing::debug!("文件搜索配置已更新");
    Ok(())
}

pub async fn get_start_menu_config(pool: &SqlitePool) -> StartMenuConfig {
    get_engine_config(pool, "start_menu")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

pub async fn update_start_menu_config(
    pool: &SqlitePool,
    config: &StartMenuConfig,
) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "start_menu", &json).await?;
    tracing::debug!(
        enabled = config.enabled,
        scan_depth = config.scan_depth,
        "应用搜索配置已更新"
    );
    Ok(())
}

pub async fn get_calc_config(pool: &SqlitePool) -> CalcConfig {
    get_engine_config(pool, "calc")
        .await
        .and_then(|cfg| serde_json::from_value(cfg).ok())
        .unwrap_or_default()
}

pub async fn update_calc_config(pool: &SqlitePool, config: &CalcConfig) -> Result<(), String> {
    let json = serde_json::to_value(config).map_err(|e| e.to_string())?;
    set_engine_config(pool, "calc", &json).await?;
    tracing::debug!(enabled = config.enabled, "计算器配置已更新");
    Ok(())
}

// ── Context 配置操作 ───────────────────────────────────────────────────────────

pub async fn get_context_config(pool: &SqlitePool) -> ContextConfig {
    crate::infra::data::history::get_config(pool, "context:config")
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub async fn set_context_config(pool: &SqlitePool, config: &ContextConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, "context:config", &json)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(
        enabled = config.enabled,
        clipboard = config.clipboard_enabled,
        selection = config.selection_enabled,
        sensitive_count = config.sensitive_apps.len(),
        "Context 配置已更新"
    );
    Ok(())
}

// ── 测试 ────────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn app_config_default_serde_roundtrip() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.language, config.language);
        assert_eq!(parsed.theme, config.theme);
        assert_eq!(parsed.hotkey.key, config.hotkey.key);
    }

    #[tokio::test]
    async fn app_config_from_default_json() {
        let config = AppConfig::default();
        let json = serde_json::to_string(&config).unwrap();
        let parsed: AppConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.language, "zh");
        assert_eq!(parsed.theme, "auto");
        assert!(parsed.autosuggest_enabled);
        assert!((parsed.autosuggest_min_score - 0.7).abs() < 1e-9);
        assert_eq!(parsed.autosuggest_tab_key, "Tab");
    }

    #[tokio::test]
    async fn shard_defaults_match_appconfig_default() {
        let app = AppConfig::default();
        let hotkey = HotkeyConfig::default();
        let app_shard = AppearanceConfig::default();
        let search = SearchConfig::default();
        let suggestion = SuggestionConfig::default();
        let chord = ChordConfig::default();
        let disable = DisableConfig::default();

        assert_eq!(hotkey.key, app.hotkey.key);
        assert_eq!(hotkey.tap_threshold, app.tap_threshold);
        assert_eq!(hotkey.grace_period, app.grace_period);
        assert_eq!(app_shard.theme, app.theme);
        assert_eq!(app_shard.language, app.language);
        assert_eq!(app_shard.log_level, app.log_level);
        assert_eq!(app_shard.auto_start, app.auto_start);
        assert_eq!(search.max_results, app.max_results);
        assert_eq!(
            search.surface_takeover_enabled,
            app.surface_takeover_enabled
        );
        assert_eq!(suggestion.autosuggest_enabled, app.autosuggest_enabled);
        assert_eq!(suggestion.proactive_enabled, app.proactive_enabled);
        assert_eq!(chord.chord_enabled, app.chord_enabled);
        assert_eq!(chord.chord_hint_visible, app.chord_hint_visible);
        assert_eq!(
            disable.disabled_builtin_actions,
            app.disabled_builtin_actions
        );
    }

    async fn in_memory_pool() -> SqlitePool {
        use sqlx::sqlite::SqlitePoolOptions;
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create config table");
        pool
    }

    #[tokio::test]
    async fn get_config_on_empty_db_returns_defaults() {
                    let pool = in_memory_pool().await;
            let cfg = get_config(&pool).await;
            assert_eq!(cfg.theme, AppConfig::default().theme);
            assert_eq!(cfg.hotkey.key, AppConfig::default().hotkey.key);
            assert_eq!(cfg.tap_threshold, 300);
            assert_eq!(cfg.grace_period, 500);
    }

    #[tokio::test]
    async fn save_and_get_config_roundtrip() {
                    let pool = in_memory_pool().await;
            let mut cfg = AppConfig::default();
            cfg.theme = "light".to_string();
            cfg.language = "en".to_string();
            cfg.max_results = 42;
            cfg.autosuggest_min_score = 0.85;
            cfg.chord_enabled = true;
            cfg.disabled_builtin_actions = vec!["shutdown".to_string()];
            cfg.tap_threshold = 250;
            cfg.hotkey.key = "F1".to_string();
            save_config(&pool, &cfg).await.unwrap();

            let loaded = get_config(&pool).await;
            assert_eq!(loaded.theme, "light");
            assert_eq!(loaded.language, "en");
            assert_eq!(loaded.max_results, 42);
            assert!((loaded.autosuggest_min_score - 0.85).abs() < 1e-9);
            assert!(loaded.chord_enabled);
            assert_eq!(loaded.disabled_builtin_actions, vec!["shutdown"]);
            assert_eq!(loaded.tap_threshold, 250);
            assert_eq!(loaded.hotkey.key, "F1");
            assert_eq!(loaded.hotkey.tap_threshold, 250);
    }

    #[tokio::test]
    async fn shards_persist_to_distinct_kv_keys() {
                    let pool = in_memory_pool().await;
            save_config(&pool, &AppConfig::default()).await.unwrap();

            let all = crate::infra::data::history::get_all_config(&pool).await;
            assert!(all.contains_key("app.hotkey"), "app.hotkey 分片应存在");
            assert!(
                all.contains_key("app.appearance"),
                "app.appearance 分片应存在"
            );
            assert!(all.contains_key("app.search"), "app.search 分片应存在");
            assert!(
                all.contains_key("app.suggestion"),
                "app.suggestion 分片应存在"
            );
            assert!(all.contains_key("app.chord"), "app.chord 分片应存在");
            assert!(all.contains_key("app.disable"), "app.disable 分片应存在");
            assert!(
                all.contains_key("clipboard:config"),
                "clipboard 独立 KV 应存在"
            );
            assert!(!all.contains_key("app_config"), "旧单 key 不应重现");
    }

    #[tokio::test]
    async fn legacy_app_config_migrates_to_shards() {
                    let pool = in_memory_pool().await;
            let legacy_json = r#"{
                "hotkey": {"modifiers": ["ctrl", "alt"], "key": "k", "display": "Ctrl+Alt+K"},
                "tap_threshold": 250,
                "grace_period": 400,
                "auto_start": true,
                "language": "en",
                "log_level": "debug",
                "surface_takeover_enabled": false,
                "theme": "gruvbox",
                "search_history_enabled": false,
                "search_history_days": 60,
                "max_results": 25,
                "page_size": 7,
                "proactive_enabled": true,
                "empty_query_topn": 3,
                "clipboard": {"enabled": false, "max_items": 100, "retention_days": 3, "search_enabled": false, "blacklist_keywords": []},
                "disabled_builtin_actions": ["shutdown", "restart"],
                "autosuggest_enabled": false,
                "autosuggest_min_score": 0.9,
                "autosuggest_tab_key": "ArrowRight",
                "disabled_context_bindings": ["builtin.translate::text_is_non_target_lang"],
                "chord_enabled": true,
                "chord_hint_visible": false,
                "disabled_chord_actions": ["screenshot"]
            }"#;
            crate::infra::data::history::set_config(&pool, "app_config", legacy_json)
                .await
                .unwrap();

            init_config(&pool).await.unwrap();

            assert!(
                crate::infra::data::history::get_config(&pool, "app_config")
                    .await
                    .is_none(),
                "app_config 单 key 应在迁移后删除"
            );

            let all = crate::infra::data::history::get_all_config(&pool).await;
            assert!(all.contains_key("app.hotkey"));
            assert!(all.contains_key("clipboard:config"));

            let cfg = get_config(&pool).await;
            assert_eq!(cfg.hotkey.modifiers, vec!["ctrl", "alt"]);
            assert_eq!(cfg.hotkey.key, "k");
            assert_eq!(cfg.tap_threshold, 250);
            assert_eq!(cfg.grace_period, 400);
            assert!(cfg.auto_start);
            assert_eq!(cfg.language, "en");
            assert_eq!(cfg.log_level, "debug");
            assert!(!cfg.surface_takeover_enabled);
            assert_eq!(cfg.theme, "gruvbox");
            assert!(!cfg.search_history_enabled);
            assert_eq!(cfg.max_results, 25);
            assert!(cfg.proactive_enabled);
            assert_eq!(cfg.empty_query_topn, 3);
            assert!(!cfg.clipboard.enabled);
            assert_eq!(cfg.clipboard.max_items, 100);
            assert_eq!(cfg.disabled_builtin_actions, vec!["shutdown", "restart"]);
            assert!(!cfg.autosuggest_enabled);
            assert!((cfg.autosuggest_min_score - 0.9).abs() < 1e-9);
            assert_eq!(cfg.autosuggest_tab_key, "ArrowRight");
            assert_eq!(
                cfg.disabled_context_bindings,
                vec!["builtin.translate::text_is_non_target_lang"]
            );
            assert!(cfg.chord_enabled);
            assert!(!cfg.chord_hint_visible);
            assert_eq!(cfg.disabled_chord_actions, vec!["screenshot"]);
    }

    #[tokio::test]
    async fn update_hotkey_preserves_tap_grace() {
                    let pool = in_memory_pool().await;
            let mut cfg = AppConfig::default();
            cfg.tap_threshold = 250;
            cfg.grace_period = 700;
            save_config(&pool, &cfg).await.unwrap();

            let new_hotkey = HotkeyConfig {
                modifiers: vec!["ctrl".to_string()],
                key: "F2".to_string(),
                display: "Ctrl+F2".to_string(),
                ..Default::default()
            };
            update_hotkey(&pool, new_hotkey).await.unwrap();

            let loaded = get_config(&pool).await;
            assert_eq!(loaded.hotkey.key, "F2");
            assert_eq!(
                loaded.tap_threshold, 250,
                "update_hotkey 不该覆盖 tap_threshold"
            );
            assert_eq!(
                loaded.grace_period, 700,
                "update_hotkey 不该覆盖 grace_period"
            );
    }
}
