//! 受控设置的 canonical 应用层更新服务（0.19.8）。
//!
//! 设置页 command 与 AI Capability 都通过这里完成持久化、运行时热更新和通知。

use serde_json::{Value, json};
use tauri::{Emitter, Manager};

use crate::domain::config::{
    AppearanceConfig, ConfigStore, ManagedSetting, ManagedSettingId, ManagedSettingUpdate,
    SearchConfig, SuggestionConfig,
};
use crate::domain::event_names::EventNames;
use crate::infra::data::clipboard::{self, ClipboardConfig};

static SETTING_UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

pub async fn list_managed_settings(app: &tauri::AppHandle) -> Vec<ManagedSetting> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let appearance = ConfigStore::get::<AppearanceConfig>(pool).await;
    let search = ConfigStore::get::<SearchConfig>(pool).await;
    let suggestion = ConfigStore::get::<SuggestionConfig>(pool).await;
    let clipboard = ConfigStore::get::<ClipboardConfig>(pool).await;

    ManagedSettingId::ALL
        .into_iter()
        // 0.20.1: list 不返回旧 clipboard.display_count（已废弃 alias）
        .filter(|id| !matches!(id, ManagedSettingId::ClipboardDisplayCount))
        .map(|id| {
            let current = current_value(id, &appearance, &search, &suggestion, &clipboard);
            id.descriptor(current)
        })
        .collect()
}

pub async fn get_managed_setting(
    app: &tauri::AppHandle,
    id: &str,
) -> Result<ManagedSetting, String> {
    list_managed_settings(app)
        .await
        .into_iter()
        .find(|item| item.id == id)
        .ok_or_else(|| format!("未知或不允许管理的 setting id: {id}"))
}

pub async fn update_managed_setting(
    app: &tauri::AppHandle,
    id: &str,
    expected_old_value: Value,
    value: Value,
) -> Result<ManagedSettingUpdate, String> {
    let _guard = SETTING_UPDATE_LOCK.lock().await;
    let setting_id = ManagedSettingId::parse(id)
        .ok_or_else(|| format!("未知或不允许管理的 setting id: {id}"))?;
    setting_id.validate(&value)?;
    let old_value = get_managed_setting(app, id).await?.current_value;
    if old_value != expected_old_value {
        return Err(format!(
            "设置 {id} 已变化；期望旧值 {expected_old_value}，当前值 {old_value}，请重新查询后再修改"
        ));
    }

    let pool = &app.state::<crate::infra::data::DbPools>().config;
    match setting_id {
        ManagedSettingId::Theme => {
            let mut cfg = ConfigStore::get::<AppearanceConfig>(pool).await;
            cfg.theme = value.as_str().unwrap().to_string();
            save_appearance(app, &cfg).await?;
        }
        ManagedSettingId::WindowOpacity => {
            let mut cfg = ConfigStore::get::<AppearanceConfig>(pool).await;
            cfg.window_opacity = value.as_f64().unwrap();
            save_appearance(app, &cfg).await?;
        }
        ManagedSettingId::SearchHistoryEnabled
        | ManagedSettingId::SearchHistoryDays
        | ManagedSettingId::SearchMaxResults
        | ManagedSettingId::SearchPageSize => {
            let mut cfg = ConfigStore::get::<SearchConfig>(pool).await;
            match setting_id {
                ManagedSettingId::SearchHistoryEnabled => {
                    cfg.search_history_enabled = value.as_bool().unwrap()
                }
                ManagedSettingId::SearchHistoryDays => {
                    cfg.search_history_days = value.as_u64().unwrap() as u32
                }
                ManagedSettingId::SearchMaxResults => {
                    cfg.max_results = value.as_u64().unwrap() as u32
                }
                ManagedSettingId::SearchPageSize => cfg.page_size = value.as_u64().unwrap() as u32,
                _ => unreachable!(),
            }
            save_search(app, &cfg).await?;
        }
        ManagedSettingId::AutosuggestEnabled => {
            let mut cfg = ConfigStore::get::<SuggestionConfig>(pool).await;
            cfg.autosuggest_enabled = value.as_bool().unwrap();
            save_suggestion(app, &cfg).await?;
        }
        ManagedSettingId::ClipboardEnabled
        | ManagedSettingId::ClipboardRetentionDays
        | ManagedSettingId::ClipboardMaxItems
        | ManagedSettingId::ClipboardDisplayCount
        | ManagedSettingId::ClipboardDisplayPages
        | ManagedSettingId::ClipboardCandidateLimit => {
            let mut cfg = ConfigStore::get::<ClipboardConfig>(pool).await;
            match setting_id {
                ManagedSettingId::ClipboardEnabled => cfg.enabled = value.as_bool().unwrap(),
                ManagedSettingId::ClipboardRetentionDays => {
                    cfg.retention_days = value.as_u64().unwrap() as u32
                }
                ManagedSettingId::ClipboardMaxItems => {
                    cfg.max_items = value.as_u64().unwrap() as u32
                }
                ManagedSettingId::ClipboardDisplayCount => {
                    // 0.20.1: 旧 alias 接受写入，换算为 display_pages 存储。
                    // 需要当前 page_size 做换算。
                    let search = ConfigStore::get::<SearchConfig>(pool).await;
                    let page_size = search.page_size.max(1);
                    let raw_count = value.as_u64().unwrap() as u32;
                    cfg.display_pages = clipboard::migrate_display_count_to_pages(raw_count, page_size);
                    tracing::info!(
                        raw_count, page_size,
                        migrated_pages = cfg.display_pages,
                        "clipboard.display_count alias → display_pages 迁移换算"
                    );
                }
                ManagedSettingId::ClipboardDisplayPages => {
                    cfg.display_pages = clipboard::clamp_display_pages(value.as_u64().unwrap() as u32);
                }
                ManagedSettingId::ClipboardCandidateLimit => {
                    cfg.candidate_limit = value.as_u64().unwrap() as u32
                }
                _ => unreachable!(),
            }
            save_clipboard(app, &cfg).await?;
        }
    }

    tracing::info!(setting_id = id, old_value = %old_value, new_value = %value, "受控设置已更新");
    Ok(ManagedSettingUpdate {
        setting_id: id.into(),
        old_value,
        new_value: value,
        immediately_effective: true,
        requires_restart: false,
    })
}

pub async fn apply_general_config(
    app: &tauri::AppHandle,
    general: &crate::domain::config::GeneralConfig,
) -> Result<(), String> {
    let _guard = SETTING_UPDATE_LOCK.lock().await;
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let mut appearance = ConfigStore::get::<AppearanceConfig>(pool).await;
    appearance.theme = general.theme.clone();
    save_appearance(app, &appearance).await?;

    let search = SearchConfig {
        search_history_enabled: general.search_history_enabled,
        search_history_days: general.search_history_days,
        max_results: general.max_results,
        page_size: general.page_size,
        ..ConfigStore::get::<SearchConfig>(pool).await
    };
    save_search(app, &search).await
}

pub async fn apply_window_opacity(app: &tauri::AppHandle, opacity: f64) -> Result<(), String> {
    let _guard = SETTING_UPDATE_LOCK.lock().await;
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let mut cfg = ConfigStore::get::<AppearanceConfig>(pool).await;
    cfg.window_opacity = opacity.clamp(0.2, 1.0);
    save_appearance(app, &cfg).await
}

pub async fn apply_autosuggest(
    app: &tauri::AppHandle,
    update: &crate::domain::config::AutosuggestUpdate,
) -> Result<(), String> {
    let _guard = SETTING_UPDATE_LOCK.lock().await;
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let cfg = SuggestionConfig {
        autosuggest_enabled: update.enabled,
        autosuggest_min_score: update.min_score.clamp(0.0, 1.0),
        autosuggest_tab_key: update.tab_key.clone(),
        ..ConfigStore::get::<SuggestionConfig>(pool).await
    };
    save_suggestion(app, &cfg).await
}

pub async fn apply_clipboard(app: &tauri::AppHandle, cfg: &ClipboardConfig) -> Result<(), String> {
    let _guard = SETTING_UPDATE_LOCK.lock().await;
    save_clipboard(app, cfg).await
}

async fn save_appearance(app: &tauri::AppHandle, cfg: &AppearanceConfig) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    ConfigStore::set(pool, cfg).await?;
    emit_changed(app, "app.appearance");
    Ok(())
}

async fn save_search(app: &tauri::AppHandle, cfg: &SearchConfig) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    ConfigStore::set(pool, cfg).await?;
    if let Some(service) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
        service.update_max_results(cfg.max_results as usize);
        // 0.20.1: page_size 变化时同步到 ClipboardEngine
        service.update_clipboard_page_size(cfg.page_size);
    }
    emit_changed(app, "app.search");
    Ok(())
}

async fn save_suggestion(app: &tauri::AppHandle, cfg: &SuggestionConfig) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    ConfigStore::set(pool, cfg).await?;
    if let Some(service) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
        service.update_autosuggest_config(cfg.autosuggest_enabled, cfg.autosuggest_min_score);
    }
    emit_changed(app, "app.suggestion");
    Ok(())
}

async fn save_clipboard(app: &tauri::AppHandle, cfg: &ClipboardConfig) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    ConfigStore::set(pool, cfg).await?;
    crate::infra::platform::clipboard::set_active(cfg.enabled);
    if let Some(service) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
        // 0.20.1: 转发 display_pages（而非旧 display_count）
        service.update_clipboard_display_pages(cfg.display_pages);
        service.update_clipboard_candidate_limit(cfg.candidate_limit);
    }
    emit_changed(app, "clipboard:config");
    Ok(())
}

fn emit_changed(app: &tauri::AppHandle, key: &str) {
    if let Err(error) = app.emit(EventNames::CONFIG_CHANGED, json!({ "key": key })) {
        tracing::warn!(%key, %error, "配置已持久化，但变更事件发送失败");
    }
}

fn current_value(
    id: ManagedSettingId,
    appearance: &AppearanceConfig,
    search: &SearchConfig,
    suggestion: &SuggestionConfig,
    clipboard: &ClipboardConfig,
) -> Value {
    match id {
        ManagedSettingId::Theme => json!(appearance.theme),
        ManagedSettingId::WindowOpacity => json!(appearance.window_opacity),
        ManagedSettingId::SearchHistoryEnabled => json!(search.search_history_enabled),
        ManagedSettingId::SearchHistoryDays => json!(search.search_history_days),
        ManagedSettingId::SearchMaxResults => json!(search.max_results),
        ManagedSettingId::SearchPageSize => json!(search.page_size),
        ManagedSettingId::AutosuggestEnabled => json!(suggestion.autosuggest_enabled),
        ManagedSettingId::ClipboardEnabled => json!(clipboard.enabled),
        ManagedSettingId::ClipboardRetentionDays => json!(clipboard.retention_days),
        ManagedSettingId::ClipboardMaxItems => json!(clipboard.max_items),
        // 0.20.1: list 中只返回 display_pages（旧 display_count 不在 list 暴露）
        ManagedSettingId::ClipboardDisplayPages => json!(clipboard.display_pages),
        ManagedSettingId::ClipboardCandidateLimit => json!(clipboard.candidate_limit),
        // 旧 alias 的 current_value 不在 list 中出现（filter 掉了）；
        // 但 get_managed_setting 可能单独查询——返回 display_pages 值以避免暴露旧字段。
        ManagedSettingId::ClipboardDisplayCount => json!(clipboard.display_pages),
    }
}
