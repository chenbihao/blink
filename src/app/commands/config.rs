//! config 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use crate::domain::event_names::EventNames;
use tauri::{Emitter, Manager};

static APP_SEARCH_UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

static INPUT_BINDING_UPDATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// 获取完整配置。
#[tauri::command]
pub async fn get_config(app: tauri::AppHandle) -> Result<serde_json::Value, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    // IPC 刷新不能将损坏/读取失败伪装成默认偏好。
    use crate::app::config::{
        AppearanceConfig, ChordConfig, ConfigStore, DisableConfig, HotkeyConfig,
    };
    ConfigStore::get_checked::<AppearanceConfig>(pool).await?;
    ConfigStore::get_checked::<HotkeyConfig>(pool).await?;
    ConfigStore::get_checked::<ChordConfig>(pool).await?;
    ConfigStore::get_checked::<DisableConfig>(pool).await?;
    ConfigStore::get_checked::<crate::app::config::SearchConfig>(pool).await?;
    ConfigStore::get_checked::<crate::infra::data::clipboard::ClipboardConfig>(pool).await?;
    let cfg = crate::app::config::get_config(pool).await;
    let mut value = serde_json::to_value(cfg).map_err(|e| e.to_string())?;
    value["auto_start_registration_skipped"] = serde_json::json!(cfg!(debug_assertions));
    Ok(value)
}

/// 泛型配置写入（0.8.6 P1-C 前端泛型化）。
///
/// 前端统一调用 `invoke('set_config', { key, value })`，后端按 key 路由到
/// 对应分片持久化 + 副作用（SearchService 热更新 / 平台 API / emit 事件）。
///
/// # 支持的 key
///
/// **AppConfig 分片**：`language` / `log_level` / `auto_start` / `onboarding_version` / `hotkey` /
/// `tap_threshold` / `grace_period` / `general_config` / `wheel_mode` / `autosuggest` /
/// `chord_toggles` / `clipboard_enabled` / `disabled_builtin_actions` /
/// `disabled_context_bindings` / `disabled_chord_actions` / `window_opacity`
///
/// **引擎配置**：`file_search` / `start_menu_config` / `calc_config` / `global_proxy`
///
/// **快捷超链（0.26.12）**：`weblinks` —— WebLinksConfig 分片（保存时 keyword 冲突校验）
///
/// **浏览器书签（0.26.13）**：`bookmarks` —— BookmarksConfig 分片（保存时 keyword 冲突校验）
///
/// **插件配置**：`plugin_config`
///
/// **Context 配置**：`context_config`
///
/// **截图配置**（0.11.10-b）：`screenshot_config` —— ScreenshotConfig 分片（prewarm_ocr 等）
#[tauri::command]
pub async fn set_config(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
    expected: Option<serde_json::Value>,
) -> Result<(), crate::app::command_error::CommandError> {
    set_config_value(app, key, value, expected)
        .await
        .map_err(|message| {
        let code = [
            "config_conflict",
            "autostart_failed",
            "autostart_restore_failed",
            "secret_read_failed",
            "secret_write_failed",
            "secret_restore_failed",
            "keyword_conflict",
        ]
            .into_iter()
            .find(|code| message == *code || message.starts_with(&format!("{code}:")))
            .unwrap_or("config_update_failed");
            crate::app::command_error::CommandError::new(code, &message, true)
        })
}

async fn set_config_value(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
    expected: Option<serde_json::Value>,
) -> Result<(), String> {
    // 跨分片冲突检查与写入同序，避免主热键和 Chord 并发检查通过后撞键。
    let _bindings_guard = if matches!(key.as_str(), "hotkey" | "chord_bindings") {
        Some(INPUT_BINDING_UPDATE_LOCK.lock().await)
    } else {
        None
    };
    let pool = &app.state::<crate::infra::data::DbPools>().config;

    match key.as_str() {
        // ── 单值字段（直接解析） ──────────────────────────────────────────
        "language" => {
            let language: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_language(pool, language.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_language(language.clone());
            }
            // 托盘菜单是 Rust 侧静态构建的（不走前端 i18n），切语言后需主动重建。
            // on_menu_event 挂在 TrayIcon 上，set_menu 不影响 id 路由。
            // 0.26.20：重建时保留「检查更新」项的版本提示（不受语言切换影响）。
            let update_hint = app
                .try_state::<std::sync::Arc<std::sync::RwLock<crate::app::update_reminder::UpdateReminderState>>>()
                .map(|c| {
                    let state = c.read().unwrap_or_else(|e| e.into_inner()).clone();
                    crate::app::update_reminder::tray_version_hint(&state).map(|v| v.to_string())
                })
                .flatten();
            crate::app::tray::rebuild_menu(&app, &language, update_hint.as_deref());
            tracing::info!(%language, "语言已更新");
        }
        "log_level" => {
            let level: String = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_log_level(pool, level.clone()).await?;
            crate::infra::utils::logging::update_level(&level);
            tracing::info!(%level, "日志级别已切换");
        }
        "ai_http_body_log" => {
            let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_ai_http_body_log(pool, enabled).await?;
            crate::infra::utils::http_log::set_body_log_enabled(enabled);
            tracing::info!(enabled, "AI HTTP 请求/响应体日志开关已切换");
        }
        "auto_start" => {
            let auto_start: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let expected = expected
                .map(serde_json::from_value::<bool>)
                .transpose()
                .map_err(|e| e.to_string())?;
            crate::app::autostart::update(&app, auto_start, expected).await?;
        }
        // 0.22.13：引导向导版本标记。storage 页「重新显示引导」置 0；
        // 完成/跳过向导走 complete_onboarding 命令（版本常量唯一真源在后端）。
        "onboarding_version" => {
            let version: u32 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_onboarding_version(pool, version).await?;
            tracing::info!(version, "引导向导版本标记已更新");
        }
        // 0.26.20：自动检查更新总开关（设置页关于 tab）。关闭后后台静默检查停跑、
        // 建议槽与托盘版本提示不再出现；手动检查（按钮/内置动作）不拦（用户主动行为）。
        "auto_check_update" => {
            let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::update_reminder::set_auto_check_enabled(&app, enabled).await;
        }
        "tap_threshold" => {
            let threshold: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_tap_threshold(pool, threshold).await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::debug!(threshold, "tap 阈值已更新");
        }
        "grace_period" => {
            let period: u64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_grace_period(pool, period).await?;
            crate::infra::platform::window::update_grace_period(period);
            tracing::debug!(period, "grace period 已更新");
        }
        "clipboard_enabled" => {
            let enabled: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::setting_service::apply_clipboard(
                &app,
                serde_json::json!({"enabled": enabled}),
                expected.map(|v| serde_json::json!({"enabled": v})),
            )
            .await?;
            tracing::info!(enabled, "剪贴板监听开关已更新");
        }

        // ── 结构体字段（按 key 对应 serde 解析） ──────────────────────────
        "hotkey" => {
            let hotkey: crate::app::config::HotkeyConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            // 0.22.12：新主热键 vs chord Tap 键 / 已设全局键 跨检查（缺口补齐）
            let chord_cfg = crate::app::config::get_chord_config(pool).await;
            let main_hotkey_conflicts: Vec<_> = app
                .state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
                .global_binding_conflicts(
                    &chord_cfg.bindings,
                    Some((&hotkey.modifiers, &hotkey.key)),
                )
                .into_iter()
                .filter(|c| {
                    matches!(
                        c.kind,
                        crate::domain::chord::BindingConflictKind::GlobalVsMainHotkey
                            | crate::domain::chord::BindingConflictKind::ChordKeyVsMainHotkey
                    )
                })
                .collect();
            if let Some(conflict) = main_hotkey_conflicts.first() {
                let language = crate::app::config::get_config(pool).await.language;
                return Err(app
                    .state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>()
                    .describe_binding_conflict(conflict, &language));
            }
            crate::app::config::update_hotkey(pool, hotkey.clone()).await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::info!(display = %hotkey.display, "全局热键已更新");
        }
        "wheel_mode" => {
            let mode = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let expected = expected
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| e.to_string())?;
            crate::app::setting_service::apply_wheel_mode(&app, mode, expected).await?;
            tracing::info!(?mode, "主窗口滚轮行为已更新");
        }
        "general_config" => {
            let general: crate::app::config::GeneralConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::setting_service::apply_general_config(&app, &general).await?;
            tracing::info!(
                theme = %general.theme,
                search_history_enabled = general.search_history_enabled,
                search_history_days = general.search_history_days,
                max_results = general.max_results,
                page_size = general.page_size,
                "通用配置已更新"
            );
        }
        "autosuggest" => {
            let v: crate::app::config::AutosuggestUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::setting_service::apply_autosuggest(&app, &v).await?;
            tracing::info!(v.enabled, v.min_score, tab_key = %v.tab_key, "Autosuggest 配置已更新");
        }
        "chord_toggles" => {
            let v: crate::app::config::ChordTogglesUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let expected: Option<crate::app::config::ChordTogglesUpdate> = expected
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::update::<crate::app::config::ChordConfig>(
                pool,
                |cfg| {
                    if expected.as_ref().is_some_and(|old| {
                        old.chord_enabled != cfg.chord_enabled
                            || old.chord_hint_visible != cfg.chord_hint_visible
                    }) {
                        return Err(
                            "config_conflict: Chord 开关已被其他窗口修改，请重新读取后重试".into(),
                        );
                    }
                    cfg.chord_enabled = v.chord_enabled;
                    cfg.chord_hint_visible = v.chord_hint_visible;
                    Ok(())
                },
            )
            .await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::info!(v.chord_enabled, v.chord_hint_visible, "Chord 开关已更新");
        }
        "chord_bindings" => {
            // chord 键位绑定（设置页改键用）
            let bindings: crate::domain::chord::ChordBindings =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let registry = app.state::<std::sync::Arc<crate::domain::chord::ChordRegistry>>();
            let language = crate::app::config::get_config(pool).await.language;
            // 0.25.5：IPC 校验与系统注册层共用白名单，不能绕过 UI 全局占用裸输入键。
            for (id, combo) in registry.global_hotkey_combos(&bindings) {
                if !crate::infra::platform::hotkey::is_supported_global_hotkey(
                    &combo.modifiers(),
                    combo.key(),
                ) {
                    return Err(format!(
                        "全局快捷键 {}（{}）不受支持：请使用单独 F1–F11，或 Ctrl/Alt/Win + 字母/数字/空格/标点/方向键/导航键；F12 为系统保留",
                        combo.display(),
                        registry.action_label(&id, &language)
                    ));
                }
            }
            // 既有检查：chord 生效键互相重复
            if let Some(conflict) = registry.binding_conflicts(&bindings).first() {
                return Err(registry.describe_binding_conflict(conflict, &language));
            }
            // 0.22.12：全局快捷键冲突 + 主热键跨检查（0.26.18：旧方向键长名归一后再比）
            let hotkey_cfg =
                crate::app::config::ConfigStore::get::<crate::app::config::HotkeyConfig>(pool)
                    .await;
            let hotkey_key = crate::app::config::normalize_legacy_hotkey_key(&hotkey_cfg.key);
            if let Some(conflict) = registry
                .global_binding_conflicts(&bindings, Some((&hotkey_cfg.modifiers, &hotkey_key)))
                .first()
            {
                return Err(registry.describe_binding_conflict(conflict, &language));
            }
            let expected = expected
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| e.to_string())?;
            crate::app::config::update_chord_bindings_checked(pool, bindings.clone(), expected)
                .await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::info!("Chord 键位绑定已更新");
        }
        "clipboard_config" => {
            let cfg = crate::app::setting_service::apply_clipboard(&app, value, expected).await?;
            tracing::info!(
                enabled = cfg.enabled,
                max_items = cfg.max_items,
                retention_days = cfg.retention_days,
                max_image_items = cfg.max_image_items,
                candidate_limit = cfg.candidate_limit,
                "剪贴板配置已更新"
            );
        }

        // ── Disable 列表 ──────────────────────────────────────────────────
        "disabled_builtin_actions" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_builtin_actions(pool, disabled.clone()).await?;
            let search_service =
                app.state::<std::sync::Arc<crate::domain::search::SearchService>>();
            search_service.update_disabled_builtin_actions(disabled.clone());
            tracing::info!(count = disabled.len(), ?disabled, "内置动作禁用列表已更新");
        }
        "disabled_context_bindings" => {
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_context_bindings(pool, disabled.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_disabled_context_bindings(disabled.clone());
            }
            tracing::info!(
                count = disabled.len(),
                ?disabled,
                "Context binding 禁用列表已更新"
            );
        }
        "disabled_chord_actions" => {
            let mut disabled: Vec<String> =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            disabled.sort();
            disabled.dedup();
            let expected: Option<Vec<String>> = expected
                .map(serde_json::from_value)
                .transpose()
                .map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::update::<crate::app::config::DisableConfig>(
                pool,
                |cfg| {
                    if expected
                        .as_ref()
                        .is_some_and(|old| old != &cfg.disabled_chord_actions)
                    {
                        return Err("config_conflict: 功能启用状态已变化，请重新读取后重试".into());
                    }
                    cfg.disabled_chord_actions = disabled.clone();
                    Ok(())
                },
            )
            .await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::info!(
                count = disabled.len(),
                ?disabled,
                "Chord 动作禁用列表已更新"
            );
        }
        "window_opacity" => {
            let opacity: f64 = serde_json::from_value(value).map_err(|e| e.to_string())?;
            let opacity = opacity.clamp(0.2, 1.0);
            crate::app::setting_service::apply_window_opacity(&app, opacity).await?;
            tracing::info!(opacity, "主窗口透明度已更新");
        }

        // ── 引擎配置 ──────────────────────────────────────────────────────
        "file_search" => {
            // 补丁合并（0.26.14）：前端只提交 enabled/data_source/everything_port/
            // max_results 四个字段，local_* 与 managed_* 不在 UI 内管理——
            // 整结构覆盖会把它们重置为默认值，改为按 key 覆盖存储对象。
            let stored: serde_json::Value =
                crate::app::config::get_engine_config(pool, "file_search")
                    .await
                    .or_else(|| serde_json::to_value(crate::app::config::FileSearchConfig::default()).ok())
                    .unwrap_or(serde_json::Value::Null);
            let mut merged = stored;
            if let (Some(dst), Some(src)) = (merged.as_object_mut(), value.as_object()) {
                for (key, field) in src {
                    dst.insert(key.clone(), field.clone());
                }
            }
            let fs: crate::app::config::FileSearchConfig =
                serde_json::from_value(merged).map_err(|e| e.to_string())?;
            crate::app::config::update_file_search(pool, fs.clone()).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "file",
                    crate::domain::search::EngineConfigUpdate::File(fs.clone()),
                )
                .await;
            }
            tracing::info!(enabled = fs.enabled, data_source = %fs.data_source, "文件搜索配置已更新");
        }
        "start_menu_config" => {
            let _guard = APP_SEARCH_UPDATE_LOCK.lock().await;
            let sm = crate::app::config::patch_start_menu_config(pool, value, expected).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "start_menu",
                    crate::domain::search::EngineConfigUpdate::StartMenu(sm.clone()),
                )
                .await;
            }
            tracing::info!(
                enabled = sm.enabled,
                scan_depth = sm.scan_depth,
                "应用搜索配置已更新"
            );
        }
        "calc_config" => {
            let cc: crate::app::config::CalcConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_calc_config(pool, &cc).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "calc",
                    crate::domain::search::EngineConfigUpdate::Calc(cc.clone()),
                )
                .await;
            }
            tracing::info!(enabled = cc.enabled, "计算器配置已更新");
        }
        "global_proxy" => {
            let v: crate::app::config::GlobalProxyUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let config = serde_json::json!({ "http": v.http, "https": v.https });
            crate::app::config::set_engine_config(pool, "_global_proxy", &config).await?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let has_http = !v.http.is_empty();
            let has_https = !v.https.is_empty();
            let proxy = if !has_http && !has_https {
                None
            } else {
                Some((v.http, v.https))
            };
            engine.update_global_proxy(proxy).await;
            tracing::info!(has_http, has_https, "全局代理配置已更新");
        }
        // ── 快捷超链（0.26.12 WebEngine）─────────────────────────────────
        "weblinks" => {
            let cfg: crate::app::config::WebLinksConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            if cfg.entries.iter().any(|entry| !crate::domain::search::validate_weblink_urls(entry)) {
                return Err("weblink_url_invalid".to_string());
            }
            // 保存时冲突校验：表内重复/保留前缀/既有 engine 与插件触发词。
            // 校验失败整体拒绝并回传全量冲突（前端逐条标红），不落库。
            let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
            if let Err(conflicts) = crate::domain::search::validate_keywords(&cfg.entries, &|kw| {
                router.keyword_owner(
                    kw,
                    Some(crate::domain::search::WEBLINKS_ENGINE_ID),
                )
            }) {
                let detail =
                    serde_json::to_string(&conflicts).unwrap_or_else(|_| "[]".to_string());
                return Err(format!("keyword_conflict: {detail}"));
            }
            crate::app::config::ConfigStore::set(pool, &cfg).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "weblinks",
                    crate::domain::search::EngineConfigUpdate::WebLinks(cfg.clone()),
                )
                .await;
            }
            // 重注册覆盖旧规则（含清空场景注入空表）；保存时已校验，此处冲突仅剩
            // 运行期插件 trigger 变化的窗口，warn 跳过。
            let skipped = crate::domain::search::register_weblinks_rules(&router, &cfg);
            if !skipped.is_empty() {
                tracing::warn!(?skipped, "weblinks keyword 与现有触发词冲突，已跳过注册");
            }
            tracing::info!(
                entries = cfg.entries.len(),
                enabled = cfg.entries.iter().filter(|e| e.enabled).count(),
                "快捷超链配置已更新"
            );
        }

        // ── 浏览器书签（0.26.13 BookmarkEngine）───────────────────────────
        "bookmarks" => {
            let cfg: crate::app::config::BookmarksConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            // 保存时冲突校验：空/保留前缀/既有 engine 与插件触发词。
            // 校验失败整体拒绝并回传冲突（前端标红 keyword 输入框），不落库。
            let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
            if let Err(conflict) = crate::domain::search::validate_bookmark_keyword(&cfg.keyword, &|kw| {
                router.keyword_owner(kw, Some(crate::domain::search::BOOKMARKS_ENGINE_ID))
            }) {
                let detail =
                    serde_json::to_string(std::slice::from_ref(&conflict)).unwrap_or_else(|_| "[]".to_string());
                return Err(format!("keyword_conflict: {detail}"));
            }
            crate::app::config::ConfigStore::set(pool, &cfg).await?;
            if let Some(ss) =
                app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            {
                ss.update_engine_config(
                    "bookmarks",
                    crate::domain::search::EngineConfigUpdate::Bookmarks(cfg.clone()),
                )
                .await;
            }
            // 重注册覆盖旧规则（含禁用/清空场景注入空表）。
            let skipped = crate::domain::search::register_bookmark_rules(&router, &cfg);
            if !skipped.is_empty() {
                tracing::warn!(?skipped, "bookmarks keyword 与现有触发词冲突，已跳过注册");
            }
            tracing::info!(
                enabled = cfg.enabled,
                keyword = %cfg.keyword,
                mix_into_global = cfg.mix_into_global,
                browsers = cfg.browsers.len(),
                "浏览器书签配置已更新"
            );
        }

        // ── 插件配置 ──────────────────────────────────────────────────────
        "plugin_config" => {
            let v: crate::app::config::PluginConfigUpdate =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            let engine = app.state::<std::sync::Arc<crate::domain::plugin::PluginEngine>>();
            let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
            let mut config = engine.get_config(&v.plugin_id).unwrap_or_default();
            config.enabled = v.enabled;
            config.settings = v.settings;
            if let Some(revision) = v.expected_revision {
                config.revision = revision;
            } else if v.plugin_id == "builtin.weather" {
                return Err("config_conflict: 天气设置缺少版本，请刷新后重试".into());
            }
            let result = engine
                .update_config(&v.plugin_id, config, Some(&router))
                .await;
            match &result {
                Ok(_) => {
                    tracing::info!(plugin_id = %v.plugin_id, enabled = v.enabled, "插件配置已更新");
                    // 0.17.8: 插件禁用时清理该插件的权限记忆行
                    if !v.enabled {
                        use tauri::Manager;
                        if let Some(pc) = app.try_state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>() {
                            let prefix = format!("plugin_{}", v.plugin_id);
                            pc.clear_plugin_trusted_db(&prefix).await;
                        }
                    }
                }
                Err(err) => {
                    tracing::warn!(plugin_id = %v.plugin_id, error = %err, "插件配置更新失败")
                }
            }
            result?;
        }

        // ── Context 配置 ──────────────────────────────────────────────────
        "context_config" => {
            let ctx: crate::app::config::ContextConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::set_context_config(pool, &ctx).await?;
            crate::infra::platform::selection::set_active(ctx.selection_enabled);
            crate::infra::platform::selection::set_sensitive_apps(ctx.sensitive_apps.clone());
            if let Some(mem) = app
                .try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>()
            {
                *mem.write().unwrap() = ctx;
            }
            tracing::debug!("Context 配置已更新");
        }

        // ── AI 配置(0.9.1 Phase 3-6) ──────────────────────────────────────
        //
        // 完整 AIConfig 分片写入(第 7 分片,独立于 AppConfig 门面);写完
        // 通知 registry reload —— 骨架条 #7(切换零重启)在此触发。
        //
        // **注意**:AIConfig 结构里不含密钥,只含 `secret_ref` CM 别名。
        // 密钥独立走 `save_ai_secret / delete_ai_secret` 两个命令,永不进 SQLite。
        "ai_config" => {
            let ai: crate::app::ai_config::AIConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(pool, &ai).await?;

            // registry 热更新——空档降级 / factory 失败静默跳过 / 复用未变动实例
            if let Some(reg) =
                app.try_state::<std::sync::Arc<crate::domain::ai::AIProviderRegistry>>()
            {
                reg.reload(&ai);
            }
            // 对话 Agent 按需重建；memory 归 ChatService 所有，不随配置失效。
            // 0.13.1: 先同步 memory 策略配置（保留运行时 context_limit），再失效缓存。
            if let Some(chat) =
                app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
            {
                chat.update_chat_config(&ai.chat_config);
                chat.update_memory_config(ai.chat_config.memory_config.clone())
                    .await;
                // 0.19.10: Skill 总开关、重扫与单 Skill 禁用清单统一热更新。
                chat.apply_skill_config(&ai.chat_config.skill_config);
                chat.notify_config_changed();
            }

            // 0.12 §2.7: 同步更新 AIConfig 内存缓存（供 CloudSttEngine 等非 async 上下文读取）
            crate::app::ai_config::update_ai_cache(&ai);

            let _ = app.emit(
                EventNames::CONFIG_CHANGED,
                serde_json::json!({ "key": "ai_config" }),
            );
            tracing::info!(
                enabled = ai.enabled,
                providers = ai.providers.len(),
                tier_ultra_light = ai.tier_ultra_light.is_some(),
                tier_light = ai.tier_light.is_some(),
                tier_main = ai.tier_main.is_some(),
                direct_execute_safe_actions = ai.direct_execute_safe_actions,
                slo_hard_timeout_ms = ?ai.slo_hard_timeout_ms,
                "AI 配置已更新"
            );
        }

        // ── AI 权限记忆配置（0.17.8）─────────────────────────────────────
        //
        // AiPermissionConfig 独立分片（第 9 KV），控制跨会话权限记忆行为。
        // 写完同步更新 PendingConfirms 的运行时配置副本。
        "ai_permission" => {
            let perm: crate::app::config::AiPermissionConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(pool, &perm).await?;

            // 同步更新 PendingConfirms 运行时配置
            {
                use tauri::Manager;
                if let Some(pc) = app
                    .try_state::<std::sync::Arc<crate::domain::ai::tool_adapter::PendingConfirms>>()
                {
                    pc.update_memory_config(perm.clone()).await;
                }
            }

            let _ = app.emit(
                EventNames::CONFIG_CHANGED,
                serde_json::json!({ "key": "ai_permission" }),
            );
            tracing::info!(
                memory_enabled = perm.memory_enabled,
                memory_days = perm.memory_days,
                "AI 权限记忆配置已更新"
            );
        }

        // ── Screenshot 配置(0.11.10-b)───────────────────────────────────
        //
        // 截图 overlay 行为分片。目前只承载 prewarm_ocr;写完不需要热更新任何
        // 内存副本,前端每次 overlay 显示时按需读取(读路径:`get_config_section`)。
        "screenshot_config" => {
            let sc: crate::app::config::ScreenshotConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::ConfigStore::set(pool, &sc).await?;
            tracing::info!(
                prewarm_ocr = sc.prewarm_ocr,
                scroll_debug = sc.scroll_debug,
                window_edge_snap = sc.window_edge_snap,
                preselection_cutout = sc.preselection_cutout,
                "截图配置已更新"
            );
        }

        _ => {
            return Err(format!("未知的配置 key: {key}"));
        }
    }

    let shard = match key.as_str() {
        "language" | "log_level" | "ai_http_body_log" | "onboarding_version" => {
            Some("app.appearance")
        }
        "start_menu_config" => Some("engine:start_menu"),
        "weblinks" => Some("app.weblinks"),
        "bookmarks" => Some("app.bookmarks"),
        "hotkey" | "tap_threshold" | "grace_period" => Some("app.hotkey"),
        "chord_toggles" | "chord_bindings" => Some("app.chord"),
        "disabled_builtin_actions" | "disabled_context_bindings" | "disabled_chord_actions" => {
            Some("app.disable")
        }
        _ => None,
    };
    if let Some(shard) = shard {
        crate::app::setting_service::emit_changed(&app, shard);
    }
    Ok(())
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let _bindings_guard = INPUT_BINDING_UPDATE_LOCK.lock().await;
    let config = crate::app::config::AppConfig::default();
    crate::app::autostart::reset(&app, &config).await?;
    crate::app::config::refresh_input_config(&app).await;
    for key in [
        "app.appearance",
        "app.hotkey",
        "app.chord",
        "app.disable",
        "app.search",
        "app.suggestion",
    ] {
        crate::app::setting_service::emit_changed(&app, key);
    }
    Ok(())
}

/// 完成（或跳过）当前引导向导（0.22.13）。
///
/// 写入后端持有的当前引导版本号 `ONBOARDING_VERSION`——版本常量唯一真源在
/// domain，前端不传版本号，避免两处硬编码漂移（漂移会导致向导每次启动重复弹出）。
#[tauri::command]
pub async fn complete_onboarding(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let version = crate::domain::config::ONBOARDING_VERSION;
    crate::app::config::update_onboarding_version(pool, version).await?;
    crate::app::setting_service::emit_changed(&app, "app.appearance");
    tracing::info!(version, "引导向导已完成/跳过");
    Ok(())
}

/// 泛型配置读取（0.8.6 §8.1.3）。
///
/// 前端 `invoke("get_config_section", { key: "app_config" })` → 返回该 key 的 JSON 值。
/// 不存在返回 `null`（前端自行 fallback 到默认值）。
///
/// **key 命名空间**：
/// - `app_config`：完整 AppConfig（兼容旧 key）
/// - `engine:{id}`：引擎配置（start_menu / calc / file_search）
/// - `plugin:{id}`：插件配置
/// - `context:config`：Context 层配置
///
/// 0.9 扩展：`ai.provider` / `ai.chat` 等直接加 key，零脚手架。
#[tauri::command]
pub async fn get_config_section(
    app: tauri::AppHandle,
    key: String,
) -> Result<serde_json::Value, String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let json_str = crate::infra::data::history::get_config(pool, &key).await;
    match json_str {
        Some(s) => serde_json::from_str(&s).map_err(|e| format!("配置解析失败: {e}")),
        None => Ok(serde_json::Value::Null),
    }
}

/// 泛型配置写入（0.8.6 §8.1.3）。
///
/// 前端 `invoke("set_config_section", { key: "engine:custom", value: {...} })` → 写入 SQLite。
/// 写入成功后 emit `blink://config-changed` 事件，前端各模块按需订阅。
///
/// **幂等性**：直接覆盖写，不需要先读后写。
#[tauri::command]
pub async fn set_config_section(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    // 共享 AppConfig 的运行期写入必须经过字段更新、冲突校验和系统副作用。
    if matches!(
        key.as_str(),
        "app_config" | "app.appearance" | "app.hotkey" | "app.chord" | "app.disable"
    ) {
        return Err("共享配置请使用 set_config 字段更新；恢复默认请使用 reset_config".into());
    }
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let json = serde_json::to_string(&value).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json)
        .await
        .map_err(|e| format!("配置写入失败: {e}"))?;

    // 广播配置变更事件（前端各模块按 key 订阅）
    if let Err(e) = app.emit(
        EventNames::CONFIG_CHANGED,
        serde_json::json!({ "key": key }),
    ) {
        tracing::debug!(error = %e, "emit blink://config-changed failed");
    }

    Ok(())
}

// ── 浏览器搜索引擎导入（0.26.21）───────────────────────────────────────────
//
// 导入=只读扫描+既有保存链路合并：`scan_browser_search_engines` 只读返回候选，
// 确认导入由前端并入 `app.weblinks` 走 `set_config` 的既有校验/CAS/广播——
// 本组命令不产生任何写路径。数据源是 Chromium 系每 profile 的 `Web Data`
// SQLite `keywords` 表；profile 发现复用 0.26.13 书签引擎的同一套代码。

/// 可导入浏览器与其 profile 列表（仅列检测到 `Web Data` 的组合）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct BrowserSearchSource {
    pub browser: String,
    pub profiles: Vec<String>,
}

/// 扫描结果：实际使用的 profile + 候选（含跳过原因，前端预览标灰）。
#[derive(Debug, serde::Serialize)]
pub struct BrowserSearchScan {
    pub profile: String,
    pub candidates: Vec<crate::domain::search::ImportCandidate>,
}

/// 枚举某浏览器下存在 `Web Data` 的 profile（`Local State` 缺失回落 Default，
/// 与书签扫描同口径）。
fn web_data_profiles(base: &std::path::Path, browser: &str) -> Vec<String> {
    let Some(user_data) =
        crate::domain::search::bookmark_engine::browser_user_data_dir(base, browser)
    else {
        return Vec::new();
    };
    let mut profiles = std::fs::read_to_string(user_data.join("Local State"))
        .map(|s| crate::domain::search::bookmark_engine::parse_local_state_profiles(&s))
        .unwrap_or_default();
    if profiles.is_empty() {
        profiles.push("Default".to_string());
    }
    profiles
        .into_iter()
        .filter(|p| user_data.join(p).join("Web Data").is_file())
        .collect()
}

/// 临时副本守卫：Drop 时尽力清理（正常路径读取后显式 drop，守卫兜底异常分支）。
struct TempDbGuard(Vec<std::path::PathBuf>);

impl Drop for TempDbGuard {
    fn drop(&mut self) {
        for path in &self.0 {
            let _ = std::fs::remove_file(path);
        }
    }
}

/// 复制 `Web Data`（及非空 `-journal`）到临时目录，返回副本路径守卫。
///
/// Chrome 运行中持有原文件句柄，Windows 上直接打开原文件不可靠；副本归本
/// 进程所有，SQLite 打开副本时可按需回滚热 journal（Web Data 为 journal 模式
/// 非本机 WAL，实测单文件复制即可安全读取）。
fn copy_web_data_to_temp(src: &std::path::Path) -> Result<TempDbGuard, String> {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let unique = format!(
        "blink-webdata-{}-{}",
        std::process::id(),
        SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temp_main = std::env::temp_dir().join(format!("{unique}.db"));
    std::fs::copy(src, &temp_main).map_err(|e| format!("复制 Web Data 失败: {e}"))?;
    let mut paths = vec![temp_main];
    // 非空 journal 一并复制（撞上写入瞬间时让副本可回滚；0 字节 journal 无热事务）。
    let journal = src.with_file_name("Web Data-journal");
    if journal.is_file()
        && std::fs::metadata(&journal).map(|m| m.len() > 0).unwrap_or(false)
    {
        let temp_journal = std::env::temp_dir().join(format!("{unique}.db-journal"));
        match std::fs::copy(&journal, &temp_journal) {
            Ok(_) => paths.push(temp_journal),
            Err(e) => tracing::debug!(error = %e, "复制 Web Data-journal 失败（忽略，主副本可读则继续）"),
        }
    }
    Ok(TempDbGuard(paths))
}

/// 只读查询副本的 keywords 表（SELECT 只取需要的列；usage_count 降序最常用在前）。
async fn read_keyword_rows(
    db: &std::path::Path,
) -> Result<Vec<crate::domain::search::KeywordRow>, String> {
    use sqlx::{Connection, Row};
    let options = sqlx::sqlite::SqliteConnectOptions::new()
        .filename(db)
        .create_if_missing(false);
    let mut conn = sqlx::sqlite::SqliteConnection::connect_with(&options)
        .await
        .map_err(|e| format!("搜索引擎数据库打开失败: {e}"))?;
    let rows = sqlx::query(
        "SELECT short_name, keyword, url FROM keywords ORDER BY usage_count DESC, id",
    )
    .fetch_all(&mut conn)
    .await
    .map_err(|e| format!("keywords 表读取失败（数据库结构可能已变化）: {e}"))?;
    Ok(rows
        .iter()
        .map(|row| crate::domain::search::KeywordRow {
            // 三列均 NOT NULL；失败按空串防御（跳过判定会兜住）
            short_name: row.try_get(0).unwrap_or_default(),
            keyword: row.try_get(1).unwrap_or_default(),
            url: row.try_get(2).unwrap_or_default(),
        })
        .collect())
}

/// 枚举可导入浏览器（0.26.21）：仅返回检测到 `Web Data` 的浏览器与 profile。
#[tauri::command]
pub async fn list_browser_search_sources() -> Result<Vec<BrowserSearchSource>, String> {
    let base = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    let sources = tokio::task::spawn_blocking(move || {
        ["chrome", "edge", "brave"]
            .iter()
            .map(|b| (*b, web_data_profiles(&base, b)))
            .filter(|(_, profiles)| !profiles.is_empty())
            .map(|(browser, profiles)| BrowserSearchSource {
                browser: browser.to_string(),
                profiles,
            })
            .collect::<Vec<_>>()
    })
    .await
    .map_err(|e| format!("枚举浏览器数据失败: {e}"))?;
    Ok(sources)
}

/// 只读扫描浏览器自定义搜索引擎（0.26.21）。
///
/// 复制 `Web Data` 到临时目录避开浏览器文件锁 → 只读 SELECT keywords 表 →
/// domain 纯函数转换/过滤/判重（对既有超链表 + engine/插件触发词）。
#[tauri::command]
pub async fn scan_browser_search_engines(
    app: tauri::AppHandle,
    browser: String,
    profile: Option<String>,
) -> Result<BrowserSearchScan, String> {
    let base = std::env::var("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_default();
    // profile 解析与文件复制是同步 IO，挪出工作线程（阻塞操作隔离铁则）。
    let browser_for_task = browser.clone();
    let (src, profile) =
        tokio::task::spawn_blocking(move || -> Result<(std::path::PathBuf, String), String> {
            let profiles = web_data_profiles(&base, &browser_for_task);
            let Some(profile) = profile
                .filter(|p| profiles.iter().any(|x| x == p))
                .or_else(|| profiles.first().cloned())
            else {
                return Err(format!(
                    "未找到 {browser_for_task} 的搜索引擎数据（可能未安装或尚未添加自定义搜索引擎）"
                ));
            };
            let src = crate::domain::search::bookmark_engine::browser_user_data_dir(
                &base,
                &browser_for_task,
            )
            .ok_or_else(|| format!("不支持的浏览器: {browser_for_task}"))?
            .join(&profile)
            .join("Web Data");
            Ok((src, profile))
        })
        .await
        .map_err(|e| format!("扫描任务失败: {e}"))??;

    let guard = tokio::task::spawn_blocking(move || copy_web_data_to_temp(&src))
        .await
        .map_err(|e| format!("复制任务失败: {e}"))??;
    let rows = read_keyword_rows(&guard.0[0]).await;
    drop(guard); // 显式清理临时副本（Drop 兜底异常分支）
    let rows = rows?;

    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let existing =
        crate::app::config::ConfigStore::get::<crate::app::config::WebLinksConfig>(pool).await;
    let router = app.state::<std::sync::Arc<crate::domain::intent::RuleRouter>>();
    let candidates = crate::domain::search::import_candidates(
        &rows,
        &existing.entries,
        &|kw| router.keyword_owner(kw, Some(crate::domain::search::WEBLINKS_ENGINE_ID)),
    );
    let importable = candidates.iter().filter(|c| c.skip.is_none()).count();
    tracing::info!(
        %browser,
        %profile,
        total = candidates.len(),
        importable,
        "扫描浏览器搜索引擎"
    );
    Ok(BrowserSearchScan { profile, candidates })
}

#[cfg(test)]
mod weblinks_import_tests {
    use super::*;

    fn temp_dir(tag: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blink-webdata-{}-{}-{tag}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn web_data_profiles_filters_by_file_and_falls_back_to_default() {
        let dir = temp_dir("profiles");
        // chrome：两个 profile，仅 Default 有 Web Data；无 Local State → 回落 Default
        let chrome = dir.join("Google/Chrome/User Data");
        std::fs::create_dir_all(chrome.join("Default")).unwrap();
        std::fs::create_dir_all(chrome.join("Profile 1")).unwrap();
        std::fs::write(chrome.join("Default/Web Data"), b"fake").unwrap();
        assert_eq!(web_data_profiles(&dir, "chrome"), vec!["Default".to_string()]);

        // edge：Local State 声明两个 profile，仅 Profile 1 有文件
        let edge = dir.join("Microsoft/Edge/User Data");
        std::fs::create_dir_all(edge.join("Default")).unwrap();
        std::fs::create_dir_all(edge.join("Profile 1")).unwrap();
        std::fs::write(
            edge.join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{},"Profile 1":{}}}}"#,
        )
        .unwrap();
        std::fs::write(edge.join("Profile 1/Web Data"), b"fake").unwrap();
        assert_eq!(web_data_profiles(&dir, "edge"), vec!["Profile 1".to_string()]);

        // brave：未安装 / 未知浏览器 → 空
        assert!(web_data_profiles(&dir, "brave").is_empty());
        assert!(web_data_profiles(&dir, "netscape").is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn copy_web_data_copies_and_cleans_up() {
        let dir = temp_dir("copy");
        let src = dir.join("Web Data");
        std::fs::write(&src, b"payload-42").unwrap();
        std::fs::write(dir.join("Web Data-journal"), b"hot").unwrap();
        let guard = copy_web_data_to_temp(&src).unwrap();
        assert_eq!(guard.0.len(), 2, "非空 journal 一并复制");
        assert_eq!(std::fs::read(&guard.0[0]).unwrap(), b"payload-42");
        let temp_paths: Vec<_> = guard.0.iter().map(|p| p.to_path_buf()).collect();
        drop(guard);
        for path in temp_paths {
            assert!(!path.exists(), "Drop 后临时副本应被清理: {path:?}");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn read_keyword_rows_orders_by_usage_and_maps_columns() {
        let dir = temp_dir("rows");
        let db = dir.join("fixture.db");
        let options =
            sqlx::sqlite::SqliteConnectOptions::new().filename(&db).create_if_missing(true);
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        sqlx::query(
            "CREATE TABLE keywords (id INTEGER PRIMARY KEY, short_name VARCHAR NOT NULL, \
             keyword VARCHAR NOT NULL, url VARCHAR NOT NULL, usage_count INTEGER DEFAULT 0)",
        )
        .execute(&pool)
        .await
        .unwrap();
        for (id, name, kw, usage) in [
            (1, "Bing", "bing.com", 3i64),
            (2, "百度", "baidu.com", 9),
            (3, "书签", "@书签", 0),
        ] {
            sqlx::query("INSERT INTO keywords (id, short_name, keyword, url, usage_count) VALUES (?, ?, ?, ?, ?)")
                .bind(id)
                .bind(name)
                .bind(kw)
                .bind("https://example.test/?q={searchTerms}")
                .bind(usage)
                .execute(&pool)
                .await
                .unwrap();
        }
        pool.close().await;

        let rows = read_keyword_rows(&db).await.unwrap();
        assert_eq!(
            rows.iter().map(|r| r.keyword.as_str()).collect::<Vec<_>>(),
            vec!["baidu.com", "bing.com", "@书签"],
            "usage_count 降序（最常用在前）"
        );
        assert_eq!(rows[0].short_name, "百度");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
