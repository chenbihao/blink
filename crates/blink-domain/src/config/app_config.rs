//! AppConfig 门面 + 配置操作函数（0.14.6 §2.1 从 `app/config.rs` 迁入）。
//!
//! `AppConfig` 是门面 struct——内部组合 6 分片 + clipboard 独立 KV。
//! init/get/save/update 操作函数供 app 层 commands 调用。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use super::shards::{
    AiPermissionConfig, AppearanceConfig, CalcConfig, ChordConfig, ContextConfig, DisableConfig,
    FileSearchConfig, HotkeyConfig, SearchConfig, StartMenuConfig, SuggestionConfig,
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
    pub clipboard: blink_infra::data::clipboard::ClipboardConfig,
    #[serde(default)]
    pub disabled_builtin_actions: Vec<String>,
    #[serde(default = "default_true")]
    pub autosuggest_enabled: bool,
    #[serde(default = "default_autosuggest_min_score")]
    pub autosuggest_min_score: f64,
    #[serde(default = "default_autosuggest_tab_key")]
    pub autosuggest_tab_key: String,
    /// 0.24.5 §5.6 建议展示策略开关组 + 降频（设置页 Smart Tab 建议区投影）。
    #[serde(default = "default_true")]
    pub completion_enabled: bool,
    #[serde(default = "default_true")]
    pub context_suggestion_enabled: bool,
    #[serde(default = "default_true")]
    pub ai_suggestion_enabled: bool,
    #[serde(default = "default_true")]
    pub secondary_enabled: bool,
    #[serde(default = "default_true")]
    pub suppress_repeated: bool,
    #[serde(default)]
    pub disabled_context_bindings: Vec<String>,
    #[serde(default = "default_true")]
    pub chord_enabled: bool,
    #[serde(default = "default_true")]
    pub chord_hint_visible: bool,
    #[serde(default)]
    pub chord_bindings: crate::chord::ChordBindings,
    #[serde(default)]
    pub disabled_chord_actions: Vec<String>,
    #[serde(default = "default_window_opacity")]
    pub window_opacity: f64,
    /// AI HTTP 请求/响应体日志开关（0.21.16，默认关）。
    #[serde(default = "default_false")]
    pub ai_http_body_log: bool,
    /// 0.22.13：已完成/看过的最新引导版本（0 = 从未看过）。启动时
    /// `< ONBOARDING_VERSION` 即弹引导窗口。
    #[serde(default)]
    pub onboarding_version: u32,
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
            clipboard: blink_infra::data::clipboard::ClipboardConfig::default(),
            disabled_builtin_actions: Vec::new(),
            autosuggest_enabled: true,
            autosuggest_min_score: 0.7,
            autosuggest_tab_key: "Tab".to_string(),
            completion_enabled: true,
            context_suggestion_enabled: true,
            ai_suggestion_enabled: true,
            secondary_enabled: true,
            suppress_repeated: true,
            disabled_context_bindings: Vec::new(),
            chord_enabled: true,
            chord_hint_visible: true,
            chord_bindings: crate::chord::ChordBindings::default(),
            disabled_chord_actions: Vec::new(),
            window_opacity: default_window_opacity(),
            ai_http_body_log: false,
            onboarding_version: 0,
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
    /// 0.24.5 §5.6 展示策略开关组 + 降频。serde 默认全开：旧格式 payload
    /// {enabled, minScore, tabKey} 不误关新特性（设置页始终发送完整字段）。
    #[serde(default = "default_true")]
    pub completion_enabled: bool,
    #[serde(default = "default_true")]
    pub context_suggestion_enabled: bool,
    #[serde(default = "default_true")]
    pub ai_suggestion_enabled: bool,
    #[serde(default = "default_true")]
    pub secondary_enabled: bool,
    #[serde(default = "default_true")]
    pub suppress_repeated: bool,
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
    // P1-3：typed deserialize 失败时不写回默认值（会覆盖有效用户配置）。
    // 旧 JSON 损坏时只删除旧 key，让 Step 2 的首次运行逻辑补写默认分片。
    if let Some(json) = blink_infra::data::history::get_config(pool, "app_config").await {
        tracing::info!("检测到旧 app_config 单 key,开始迁移到分片 KV");
        match serde_json::from_str::<AppConfig>(&json) {
            Ok(legacy) => {
                save_config(pool, &legacy).await?;
                blink_infra::data::history::delete_config(pool, "app_config")
                    .await
                    .map_err(|e| e.to_string())?;
                tracing::info!("app_config 单 key 已拆分到 6 分片 + clipboard 独立 KV,旧 key 删除");
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "旧 app_config JSON 反序列化失败，跳过迁移写回（不覆盖），仅删除旧 key"
                );
                blink_infra::data::history::delete_config(pool, "app_config")
                    .await
                    .map_err(|e| e.to_string())?;
            }
        }
    }

    // Step 2: 首次运行
    let existing = blink_infra::data::history::get_all_config(pool).await;
    let has_any_shard = existing.contains_key("app.hotkey")
        || existing.contains_key("app.appearance")
        || existing.contains_key("app.search")
        || existing.contains_key("app.suggestion")
        || existing.contains_key("app.chord")
        || existing.contains_key("app.disable");
    if !has_any_shard {
        let config = AppConfig {
            language: blink_infra::platform::locale::detect_system_language(),
            ..Default::default()
        };
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
    let mut appearance = ConfigStore::get::<AppearanceConfig>(pool).await;

    // 0.22.13：onboarding_version(u32) 一次性迁移。None = 0.22.13 前的存量记录，
    // 一律迁为 0（未完成当前版引导）——老用户升级后补看一次新版向导，
    // 完成/跳过后写回 ONBOARDING_VERSION。迁移即持久化。
    if appearance.onboarding_version.is_none() {
        match ConfigStore::update::<AppearanceConfig>(pool, |cfg| {
            if cfg.onboarding_version.is_none() {
                cfg.onboarding_version = Some(0);
            }
            Ok(())
        })
        .await
        {
            Ok(current) => appearance = current,
            Err(error) => {
                appearance.onboarding_version = Some(0);
                tracing::warn!(%error, "onboarding_version 迁移写回失败（下次读取重试）");
            }
        }
        tracing::info!(
            version = appearance.onboarding_version,
            "appearance 分片一次性迁移：存量记录 → onboarding_version=0"
        );
    }

    let search = ConfigStore::get::<SearchConfig>(pool).await;
    let suggestion = ConfigStore::get::<SuggestionConfig>(pool).await;
    let chord = ConfigStore::get::<ChordConfig>(pool).await;
    let disable = ConfigStore::get::<DisableConfig>(pool).await;

    // 0.20.1：clipboard display_count → display_pages 启动迁移。
    // 0.20.7 修订：typed deserialize 失败时绝不写回默认值（会覆盖有效用户配置）。
    // 在原始 JSON object 上操作：只更新 display_pages、删除 display_count，
    // 保留未知字段和其他合法字段。成功验证后才写回。
    let clipboard = {
        let raw = blink_infra::data::history::get_config(
            pool,
            <blink_infra::data::clipboard::ClipboardConfig as super::store::ConfigKey>::KEY,
        )
        .await;

        let cfg: blink_infra::data::clipboard::ClipboardConfig = match raw.as_deref() {
            Some(json_str) => match serde_json::from_str::<serde_json::Value>(json_str) {
                Ok(raw_val) => {
                    // 迁移换算（并存时新字段优先；结果需要写回时 migrated=true）
                    let (resolved_pages, migrated) =
                        blink_infra::data::clipboard::resolve_display_pages_from_json(
                            &raw_val,
                            search.page_size.max(1),
                        );

                    // typed deserialize 失败时不写回默认值（spec-backend §7.5）
                    // ——unwrap_or_default 会掩盖字段类型损坏，导致整份默认配置被写回
                    let parsed: blink_infra::data::clipboard::ClipboardConfig =
                        match serde_json::from_value::<blink_infra::data::clipboard::ClipboardConfig>(
                            raw_val.clone(),
                        ) {
                            Ok(mut p) => {
                                p.display_pages = resolved_pages;
                                p
                            }
                            Err(e) => {
                                // typed deserialize 失败：不写回，返回默认值
                                tracing::warn!(
                                    error = %e,
                                    "clipboard 配置反序列化失败，使用默认值（不写回 DB）"
                                );
                                blink_infra::data::clipboard::ClipboardConfig {
                                    display_pages: resolved_pages,
                                    ..Default::default()
                                }
                            }
                        };

                    // 迁移/清理后立即写回，确保下次读取时 DB 中已是新字段
                    if migrated {
                        // 0.20.7：在原始 JSON object 上操作，保留未知字段
                        if let serde_json::Value::Object(mut obj) = raw_val {
                            obj.insert(
                                "display_pages".to_string(),
                                serde_json::Value::Number(resolved_pages.into()),
                            );
                            obj.remove("display_count");
                            let migrated_val = serde_json::Value::Object(obj);
                            match serde_json::to_string(&migrated_val) {
                                Ok(serialized) => {
                                    if let Err(e) =
                                        blink_infra::data::history::set_config(
                                            pool,
                                            <blink_infra::data::clipboard::ClipboardConfig as super::store::ConfigKey>::KEY,
                                            &serialized,
                                        )
                                        .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "clipboard display_pages 迁移写回失败"
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "clipboard display_pages 迁移序列化失败，跳过写回"
                                ),
                            }
                        } else {
                            // 非 object（数组/标量）→ 用 typed struct 序列化
                            match serde_json::to_string(&parsed) {
                                Ok(serialized) => {
                                    if let Err(e) = blink_infra::data::history::set_config(
                                        pool,
                                        <blink_infra::data::clipboard::ClipboardConfig as super::store::ConfigKey>::KEY,
                                        &serialized,
                                    )
                                    .await
                                    {
                                        tracing::warn!(
                                            error = %e,
                                            "clipboard display_pages 迁移写回失败"
                                        );
                                    }
                                }
                                Err(e) => tracing::warn!(
                                    error = %e,
                                    "clipboard display_pages 迁移序列化失败，跳过写回"
                                ),
                            }
                        }
                    }
                    parsed
                }
                Err(_) => blink_infra::data::clipboard::ClipboardConfig::default(),
            },
            None => blink_infra::data::clipboard::ClipboardConfig::default(),
        };
        cfg
    };

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
        ai_http_body_log: appearance.ai_http_body_log,
        onboarding_version: appearance.onboarding_version.unwrap_or(0),
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
        completion_enabled: suggestion.completion_enabled,
        context_suggestion_enabled: suggestion.context_suggestion_enabled,
        ai_suggestion_enabled: suggestion.ai_suggestion_enabled,
        secondary_enabled: suggestion.secondary_enabled,
        suppress_repeated: suggestion.suppress_repeated,
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
    let mut shards = Vec::new();
    let hotkey_shard = HotkeyConfig {
        modifiers: config.hotkey.modifiers.clone(),
        key: config.hotkey.key.clone(),
        display: config.hotkey.display.clone(),
        tap_threshold: config.tap_threshold,
        grace_period: config.grace_period,
    };
    shards.push(encoded(&hotkey_shard)?);

    shards.push(encoded(&AppearanceConfig {
        theme: config.theme.clone(),
        language: config.language.clone(),
        auto_start: config.auto_start,
        log_level: config.log_level.clone(),
        window_opacity: config.window_opacity,
        ai_http_body_log: config.ai_http_body_log,
        onboarding_version: Some(config.onboarding_version),
    })?);

    shards.push(encoded(&SearchConfig {
        search_history_enabled: config.search_history_enabled,
        search_history_days: config.search_history_days,
        max_results: config.max_results,
        page_size: config.page_size,
        surface_takeover_enabled: config.surface_takeover_enabled,
    })?);

    shards.push(encoded(&SuggestionConfig {
        autosuggest_enabled: config.autosuggest_enabled,
        autosuggest_min_score: config.autosuggest_min_score,
        autosuggest_tab_key: config.autosuggest_tab_key.clone(),
        proactive_enabled: config.proactive_enabled,
        empty_query_topn: config.empty_query_topn,
        completion_enabled: config.completion_enabled,
        context_suggestion_enabled: config.context_suggestion_enabled,
        ai_suggestion_enabled: config.ai_suggestion_enabled,
        secondary_enabled: config.secondary_enabled,
        suppress_repeated: config.suppress_repeated,
        // 未映射进 AppConfig 门面的字段（0.24 secondary_min_rank 等）保留 KV 现值
        ..ConfigStore::get::<SuggestionConfig>(pool).await
    })?);

    shards.push(encoded(&ChordConfig {
        chord_enabled: config.chord_enabled,
        chord_hint_visible: config.chord_hint_visible,
        bindings: config.chord_bindings.clone(),
    })?);

    shards.push(encoded(&DisableConfig {
        disabled_builtin_actions: config.disabled_builtin_actions.clone(),
        disabled_context_bindings: config.disabled_context_bindings.clone(),
        disabled_chord_actions: config.disabled_chord_actions.clone(),
    })?);

    shards.push(encoded(&config.clipboard)?);

    blink_infra::data::config::set_configs(pool, &shards)
        .await
        .map_err(|e| e.to_string())
}

fn encoded<T: super::store::ConfigKey>(value: &T) -> Result<(String, String), String> {
    Ok((
        T::KEY.to_string(),
        serde_json::to_string(value).map_err(|e| e.to_string())?,
    ))
}

// ── 分项更新函数 ────────────────────────────────────────────────────────────────

pub async fn update_hotkey(pool: &SqlitePool, hotkey: HotkeyConfig) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut HotkeyConfig| {
        config.modifiers = hotkey.modifiers.clone();
        config.key = hotkey.key.clone();
        config.display = hotkey.display.clone();
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_tap_threshold(pool: &SqlitePool, threshold: u64) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut HotkeyConfig| {
        config.tap_threshold = threshold;
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_grace_period(pool: &SqlitePool, period: u64) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut HotkeyConfig| {
        config.grace_period = period;
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_auto_start(pool: &SqlitePool, auto_start: bool) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut AppearanceConfig| {
        config.auto_start = auto_start;
        Ok(())
    })
    .await
    .map(|_| ())
}

/// 0.22.13：更新引导向导已完成版本（完成/跳过向导写 `ONBOARDING_VERSION`，
/// storage 页「重新显示引导」写 0）。镜像 update_auto_start 模式。
pub async fn update_onboarding_version(pool: &SqlitePool, version: u32) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut AppearanceConfig| {
        config.onboarding_version = Some(version);
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_language(pool: &SqlitePool, language: String) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut AppearanceConfig| {
        config.language = language.clone();
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_log_level(pool: &SqlitePool, level: String) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut AppearanceConfig| {
        config.log_level = level.clone();
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_ai_http_body_log(pool: &SqlitePool, enabled: bool) -> Result<(), String> {
    ConfigStore::update(pool, |config: &mut AppearanceConfig| {
        config.ai_http_body_log = enabled;
        Ok(())
    })
    .await
    .map(|_| ())
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
    ConfigStore::update(pool, |config: &mut DisableConfig| {
        config.disabled_builtin_actions = normalized.clone();
        Ok(())
    })
    .await
    .map(|_| ())
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
    ConfigStore::update(pool, |config: &mut DisableConfig| {
        config.disabled_context_bindings = normalized.clone();
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn get_disabled_chord_actions(pool: &SqlitePool) -> Vec<String> {
    // 直接查 DisableConfig 分片（1 次 DB），不走 get_config 全量门面（7 次 DB）。
    // 此函数在 HoldStarted effect 热路径上调用，全量查询会阻塞串行事件循环。
    ConfigStore::get::<DisableConfig>(pool)
        .await
        .disabled_chord_actions
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
    ConfigStore::update(pool, |config: &mut DisableConfig| {
        config.disabled_chord_actions = normalized.clone();
        Ok(())
    })
    .await
    .map(|_| ())
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
    ConfigStore::update(pool, |config: &mut ChordConfig| {
        config.chord_enabled = chord_enabled;
        config.chord_hint_visible = chord_hint_visible;
        Ok(())
    })
    .await
    .map(|_| ())
}

pub async fn update_chord_bindings(
    pool: &SqlitePool,
    bindings: crate::chord::ChordBindings,
) -> Result<(), String> {
    update_chord_bindings_checked(pool, bindings, None).await
}

pub async fn update_chord_bindings_checked(
    pool: &SqlitePool,
    bindings: crate::chord::ChordBindings,
    expected: Option<crate::chord::ChordBindings>,
) -> Result<(), String> {
    ConfigStore::update(pool, |chord: &mut ChordConfig| {
        if expected
            .as_ref()
            .is_some_and(|previous| previous != &chord.bindings)
        {
            return Err("config_conflict: 快捷键配置已被其他窗口修改，请重新读取后重试".into());
        }
        chord.bindings = bindings.clone();
        Ok(())
    })
    .await
    .map(|_| ())
}

// ── 引擎配置（通用 API）─────────────────────────────────────────────────────────

pub async fn get_engine_config(pool: &SqlitePool, engine_id: &str) -> Option<serde_json::Value> {
    let key = format!("engine:{}", engine_id);
    blink_infra::data::history::get_config(pool, &key)
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
    blink_infra::data::history::set_config(pool, &key, &json)
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
    blink_infra::data::history::get_config(pool, "context:config")
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
        .unwrap_or_default()
}

pub async fn set_context_config(pool: &SqlitePool, config: &ContextConfig) -> Result<(), String> {
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    blink_infra::data::history::set_config(pool, "context:config", &json)
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

// ── AI 权限记忆配置操作（0.17.8）──────────────────────────────────────────────

/// 获取 AI 权限记忆配置。
pub async fn get_ai_permission_config(pool: &SqlitePool) -> AiPermissionConfig {
    ConfigStore::get::<AiPermissionConfig>(pool).await
}

/// 更新 AI 权限记忆配置。
#[allow(dead_code)] // set_config 命令直接用 ConfigStore::set + 同步 PendingConfirms，此函数为备选 API
pub async fn update_ai_permission_config(
    pool: &SqlitePool,
    memory_enabled: bool,
    memory_days: u64,
) -> Result<(), String> {
    let config = AiPermissionConfig {
        memory_enabled,
        memory_days: memory_days.clamp(1, 180),
    };
    ConfigStore::set(pool, &config).await?;
    tracing::info!(
        memory_enabled,
        memory_days = config.memory_days,
        "AI 权限记忆配置已更新"
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

    /// 0.24.5 §5.6：`AutosuggestUpdate` 旧格式 payload（只有 enabled/minScore/tabKey）
    /// 反序列化后五个新开关默认 true——防旧前端/异常路径误关展示策略。
    #[test]
    fn autosuggest_update_legacy_payload_defaults_new_toggles() {
        let legacy = r#"{"enabled":true,"minScore":0.8,"tabKey":"Tab"}"#;
        let u: AutosuggestUpdate = serde_json::from_str(legacy).unwrap();
        assert!(u.enabled);
        assert!((u.min_score - 0.8).abs() < 1e-9);
        assert_eq!(u.tab_key, "Tab");
        assert!(u.completion_enabled);
        assert!(u.context_suggestion_enabled);
        assert!(u.ai_suggestion_enabled);
        assert!(u.secondary_enabled);
        assert!(u.suppress_repeated);
    }

    /// 完整 payload 的显式 false 必须被尊重（serde default 只兜缺失字段）。
    #[test]
    fn autosuggest_update_full_payload_respects_explicit_false() {
        let full = r#"{"enabled":false,"minScore":0.7,"tabKey":"Tab","completionEnabled":false,"contextSuggestionEnabled":false,"aiSuggestionEnabled":false,"secondaryEnabled":false,"suppressRepeated":false}"#;
        let u: AutosuggestUpdate = serde_json::from_str(full).unwrap();
        assert!(!u.enabled);
        assert!(!u.completion_enabled);
        assert!(!u.context_suggestion_enabled);
        assert!(!u.ai_suggestion_enabled);
        assert!(!u.secondary_enabled);
        assert!(!u.suppress_repeated);
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
    async fn concurrent_field_updates_preserve_other_shards_and_fields() {
        let pool = in_memory_pool().await;
        save_config(&pool, &AppConfig::default()).await.unwrap();
        let (auto_start, onboarding, chord, language, theme) = tokio::join!(
            update_auto_start(&pool, true),
            update_onboarding_version(&pool, 99),
            update_chord_toggles(&pool, false, false),
            update_language(&pool, "en".into()),
            ConfigStore::update::<AppearanceConfig>(&pool, |cfg| {
                cfg.theme = "dark".into();
                Ok(())
            }),
        );
        auto_start.unwrap();
        onboarding.unwrap();
        chord.unwrap();
        language.unwrap();
        theme.unwrap();
        let loaded = get_config(&pool).await;
        assert!(loaded.auto_start);
        assert_eq!(loaded.onboarding_version, 99);
        assert_eq!(loaded.theme, "dark");
        assert_eq!(loaded.language, "en");
        assert!(!loaded.chord_enabled);
        assert!(!loaded.chord_hint_visible);
        assert_eq!(loaded.hotkey.display, "Alt+Space");
    }

    #[tokio::test]
    async fn update_preserves_unknown_fields_and_rejects_corrupt_shard() {
        let pool = in_memory_pool().await;
        blink_infra::data::config::set_config(
            &pool,
            "app.appearance",
            r#"{"auto_start":false,"future_option":42}"#,
        )
        .await
        .unwrap();
        update_auto_start(&pool, true).await.unwrap();
        let raw = blink_infra::data::config::config_value(&pool, "app.appearance")
            .await
            .unwrap()
            .unwrap();
        let value: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(value["future_option"], 42);
        blink_infra::data::config::set_config(&pool, "app.appearance", "invalid")
            .await
            .unwrap();
        assert!(update_auto_start(&pool, false).await.is_err());
        assert_eq!(
            blink_infra::data::config::config_value(&pool, "app.appearance")
                .await
                .unwrap()
                .as_deref(),
            Some("invalid")
        );
    }

    #[tokio::test]
    async fn binding_compare_and_set_rejects_other_window_update() {
        let pool = in_memory_pool().await;
        let original = crate::chord::ChordBindings::default();
        let mut first = original.clone();
        first.chat = crate::chord::ChordBinding {
            key: "b".into(),
            ..Default::default()
        };
        update_chord_bindings_checked(&pool, first.clone(), Some(original.clone()))
            .await
            .unwrap();
        let error = update_chord_bindings_checked(&pool, original.clone(), Some(original))
            .await
            .unwrap_err();
        assert!(error.starts_with("config_conflict:"));
        assert_eq!(get_chord_config(&pool).await.bindings, first);
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
        // 0.24.5 展示策略开关组 + 降频：分片与门面默认值一致（全开）
        assert_eq!(suggestion.suppress_repeated, app.suppress_repeated);
        assert_eq!(suggestion.completion_enabled, app.completion_enabled);
        assert_eq!(
            suggestion.context_suggestion_enabled,
            app.context_suggestion_enabled
        );
        assert_eq!(suggestion.ai_suggestion_enabled, app.ai_suggestion_enabled);
        assert_eq!(suggestion.secondary_enabled, app.secondary_enabled);
        // secondary_min_rank 刻意不进门面（§5.6 注记：KV 原值保留），此处钉分片默认
        assert!((suggestion.secondary_min_rank - 0.60).abs() < 1e-9);
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
        let mut cfg = AppConfig {
            theme: "light".to_string(),
            language: "en".to_string(),
            max_results: 42,
            autosuggest_min_score: 0.85,
            chord_enabled: true,
            disabled_builtin_actions: vec!["shutdown".to_string()],
            tap_threshold: 250,
            ..Default::default()
        };
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

        let all = blink_infra::data::history::get_all_config(&pool).await;
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
        blink_infra::data::history::set_config(&pool, "app_config", legacy_json)
            .await
            .unwrap();

        init_config(&pool).await.unwrap();

        assert!(
            blink_infra::data::history::get_config(&pool, "app_config")
                .await
                .is_none(),
            "app_config 单 key 应在迁移后删除"
        );

        let all = blink_infra::data::history::get_all_config(&pool).await;
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
        // 旧格式不含 0.24.5 新字段 → 门面 serde 默认全开，不误关新特性
        assert!(cfg.suppress_repeated);
        assert!(cfg.completion_enabled);
        assert!(cfg.context_suggestion_enabled);
        assert!(cfg.ai_suggestion_enabled);
        assert!(cfg.secondary_enabled);
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
        let cfg = AppConfig {
            tap_threshold: 250,
            grace_period: 700,
            ..Default::default()
        };
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

    /// P1-3：旧 app_config JSON 损坏时，init_config 不应写回默认值覆盖已有分片。
    #[tokio::test]
    async fn init_config_corrupt_legacy_does_not_overwrite_with_defaults() {
        let pool = in_memory_pool().await;

        // 预先保存一份有效配置到分片
        let cfg = AppConfig {
            theme: "dark".to_string(),
            language: "en".to_string(),
            max_results: 42,
            ..Default::default()
        };
        save_config(&pool, &cfg).await.unwrap();

        // 写入损坏的旧 app_config 单 key
        blink_infra::data::history::set_config(&pool, "app_config", "{not valid json")
            .await
            .unwrap();

        init_config(&pool).await.unwrap();

        // 旧 key 应被删除
        assert!(
            blink_infra::data::history::get_config(&pool, "app_config")
                .await
                .is_none(),
            "损坏的旧 app_config key 应被删除"
        );

        // 已有分片配置不应被覆盖
        let loaded = get_config(&pool).await;
        assert_eq!(loaded.theme, "dark", "已有 theme 不应被默认值覆盖");
        assert_eq!(loaded.language, "en", "已有 language 不应被默认值覆盖");
        assert_eq!(loaded.max_results, 42, "已有 max_results 不应被默认值覆盖");
    }

    /// P1-3：旧 app_config JSON 缺少部分字段（合法缺失），init_config 应正常迁移。
    /// serde #[serde(default)] 使得缺字段时仍能反序列化成功。
    #[tokio::test]
    async fn init_config_partial_legacy_migrates_successfully() {
        let pool = in_memory_pool().await;

        // 部分字段缺失的旧 JSON（有 hotkey 但没有 chord 等新功能字段）
        let partial_json = r#"{
            "hotkey": {"modifiers": ["alt"], "key": "space", "display": "Alt+Space"},
            "tap_threshold": 300,
            "grace_period": 500,
            "auto_start": false,
            "language": "zh",
            "theme": "auto"
        }"#;
        blink_infra::data::history::set_config(&pool, "app_config", partial_json)
            .await
            .unwrap();

        init_config(&pool).await.unwrap();

        assert!(
            blink_infra::data::history::get_config(&pool, "app_config")
                .await
                .is_none(),
            "旧 app_config key 应在成功迁移后删除"
        );

        // 迁移后的配置应保留旧 JSON 中的值
        // 注意：init_config Step 3 会把 hotkey.key "space" → " "（一次性修正）
        let loaded = get_config(&pool).await;
        assert_eq!(loaded.hotkey.key, " ");
        assert_eq!(loaded.language, "zh");
        assert_eq!(loaded.theme, "auto");
        // 缺失字段应使用默认值
        assert!(loaded.chord_enabled);
    }
}
