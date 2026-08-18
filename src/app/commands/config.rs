//! config 域命令（0.14.6 §2.4 从 commands.rs 拆分）。

use crate::domain::event_names::EventNames;
use tauri::{Emitter, Manager};

/// 获取完整配置。
#[tauri::command]
pub async fn get_config(app: tauri::AppHandle) -> crate::app::config::AppConfig {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    crate::app::config::get_config(pool).await
}

/// 泛型配置写入（0.8.6 P1-C 前端泛型化）。
///
/// 前端统一调用 `invoke('set_config', { key, value })`，后端按 key 路由到
/// 对应分片持久化 + 副作用（SearchService 热更新 / 平台 API / emit 事件）。
///
/// # 支持的 key
///
/// **AppConfig 分片**：`language` / `log_level` / `auto_start` / `first_run` / `hotkey` /
/// `tap_threshold` / `grace_period` / `general_config` / `autosuggest` /
/// `chord_toggles` / `clipboard_enabled` / `disabled_builtin_actions` /
/// `disabled_context_bindings` / `disabled_chord_actions` / `window_opacity`
///
/// **引擎配置**：`file_search` / `start_menu_config` / `calc_config` / `global_proxy` / `interpreter_paths`
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
) -> Result<(), String> {
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
            crate::app::tray::rebuild_menu(&app, &language);
            let _ = app.emit(EventNames::CONFIG_CHANGED, ());
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
            crate::app::config::update_auto_start(pool, auto_start).await?;
            // dev 模式跳过注册表写入（原因同 main.rs 启动同步逻辑）：
            // current_exe() 是 debug 构建（console 子系统），写入 Run 键后开机弹控制台。
            // 配置 DB 仍正常记录偏好，release 下 set_config 才真正写注册表。
            if !cfg!(debug_assertions) {
                use tauri_plugin_autostart::ManagerExt;
                let manager = app.autolaunch();
                if auto_start {
                    manager.enable().map_err(|e| e.to_string())?;
                } else {
                    manager.disable().map_err(|e| e.to_string())?;
                }
            }
            tracing::info!(auto_start, "开机自启配置已更新");
        }
        // 0.17.3：首次启动标记（引导窗口"开始使用"或关闭时设为 false）
        "first_run" => {
            let first_run: bool = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_first_run(pool, first_run).await?;
            tracing::info!(first_run, "首次启动标记已更新");
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
            let mut clip_cfg = crate::app::config::ConfigStore::get::<
                crate::infra::data::clipboard::ClipboardConfig,
            >(pool)
            .await;
            clip_cfg.enabled = enabled;
            crate::app::setting_service::apply_clipboard(&app, &clip_cfg).await?;
            tracing::info!(enabled, "剪贴板监听开关已更新");
        }

        // ── 结构体字段（按 key 对应 serde 解析） ──────────────────────────
        "hotkey" => {
            let hotkey: crate::app::config::HotkeyConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_hotkey(pool, hotkey.clone()).await?;
            crate::app::config::refresh_input_config(&app).await;
            tracing::info!(display = %hotkey.display, "全局热键已更新");
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
            crate::app::config::update_chord_toggles(pool, v.chord_enabled, v.chord_hint_visible)
                .await?;
            crate::app::config::refresh_input_config(&app).await;
            let _ = app.emit(EventNames::CONFIG_CHANGED, ());
            tracing::info!(v.chord_enabled, v.chord_hint_visible, "Chord 开关已更新");
        }
        "chord_bindings" => {
            // chord 键位绑定（设置页改键用）
            let bindings: crate::domain::chord::ChordBindings =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_chord_bindings(pool, bindings.clone()).await?;
            crate::app::config::refresh_input_config(&app).await;
            let _ = app.emit(EventNames::CONFIG_CHANGED, ());
            tracing::info!("Chord 键位绑定已更新");
        }
        "clipboard_config" => {
            // 0.20.1：剪贴板历史详细配置（retention_days / max_items / blacklist_keywords / display_pages）
            let cfg: crate::infra::data::clipboard::ClipboardConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::setting_service::apply_clipboard(&app, &cfg).await?;
            tracing::info!(
                enabled = cfg.enabled,
                max_items = cfg.max_items,
                retention_days = cfg.retention_days,
                display_pages = cfg.display_pages,
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
            let disabled: Vec<String> = serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_disabled_chord_actions(pool, disabled.clone()).await?;
            crate::app::config::refresh_input_config(&app).await;
            let _ = app.emit(EventNames::CONFIG_CHANGED, ());
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
            let fs: crate::app::config::FileSearchConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
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
            let sm: crate::app::config::StartMenuConfig =
                serde_json::from_value(value).map_err(|e| e.to_string())?;
            crate::app::config::update_start_menu_config(pool, &sm).await?;
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

        // ── 解释器路径配置 ────────────────────────────────────────────────
        "interpreter_paths" => {
            let json_str = serde_json::to_string(&value).map_err(|e| e.to_string())?;
            crate::infra::data::history::set_config(pool, "interpreter_paths", &json_str)
                .await
                .map_err(|e| e.to_string())?;
            tracing::info!("解释器路径配置已更新");
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
                "截图配置已更新"
            );
        }

        _ => {
            return Err(format!("未知的配置 key: {key}"));
        }
    }

    Ok(())
}

/// 恢复默认配置。
#[tauri::command]
pub async fn reset_config(app: tauri::AppHandle) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let config = crate::app::config::AppConfig::default();
    crate::app::config::save_config(pool, &config).await
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
/// 前端 `invoke("set_config_section", { key: "app_config", value: {...} })` → 写入 SQLite。
/// 写入成功后 emit `blink://config-changed` 事件，前端各模块按需订阅。
///
/// **幂等性**：直接覆盖写，不需要先读后写。
#[tauri::command]
pub async fn set_config_section(
    app: tauri::AppHandle,
    key: String,
    value: serde_json::Value,
) -> Result<(), String> {
    let pool = &app.state::<crate::infra::data::DbPools>().config;
    let json = serde_json::to_string(&value).map_err(|e| format!("序列化失败: {e}"))?;
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
