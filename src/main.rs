#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod domain;
mod infra;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WindowEvent,
};

fn main() {
    // 开启 Per-Monitor V2 DPI 感知：混合 DPI（例如主屏 100% + 副屏 150%）跨屏时
    // 由系统按目标显示器的 scale 自动重算尺寸，避免文字虚化 / 位置漂移。
    // 必须在任何窗口创建前调用；调用失败静默忽略（Win10 早期版本走 System DPI 也可用）。
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::HiDpi::{
            SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        };
        let _ = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
    }

    // 初始化日志（尽早，默认 error；setup 读配置后 reload 到用户级别）
    infra::utils::logging::init("error");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            infra::platform::window::invoke(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
        .plugin(tauri_plugin_dialog::init())
        // 自定义协议：http://blink-icon.localhost/<url-encoded-path>（Windows）—— 按需懒加载应用图标 PNG。
        // 前端 <img src> 直接引用，图标提取移出搜索热路径（见 search/icon.rs）。
        .register_asynchronous_uri_scheme_protocol("blink-icon", |_ctx, request, responder| {
            // path 段形如 "/C%3A%5C..."，去掉前导 '/' 后做 percent-decode 还原真实路径。
            let raw = request.uri().path().trim_start_matches('/').to_string();
            let path = percent_decode(&raw);
            // tracing::debug!(%path, "blink-icon: 收到图标请求");
            tauri::async_runtime::spawn(async move {
                let path_for_log = path.clone();
                let icon = tauri::async_runtime::spawn_blocking(move || {
                    crate::domain::search::icon::get_icon_png(&path)
                })
                .await
                .ok()
                .flatten();
                let response = match icon {
                    Some(bytes) => tauri::http::Response::builder()
                        .status(200)
                        .header("Content-Type", "image/png")
                        .body(bytes)
                        .unwrap(),
                    None => {
                        tracing::debug!(path = %path_for_log, "blink-icon: 未取到图标，返回 404");
                        tauri::http::Response::builder()
                            .status(404)
                            .body(Vec::new())
                            .unwrap()
                    }
                };
                responder.respond(response);
            });
        })
        .setup(|app| {
            // 启动总耗时（setup 结束时手动记录，因为 setup 在同步上下文，没有 runtime 句柄）
            let startup_start = std::time::Instant::now();

            // 初始化历史记录 SQLite
            let pool = tauri::async_runtime::block_on(infra::data::history::init_db())
                .expect("failed to init history db");

            // 初始化性能统计（0.7.0）
            tauri::async_runtime::block_on(infra::utils::perf::init(&pool))
                .expect("failed to init perf metrics");

            // 初始化剪贴板历史表（0.7.3）
            tauri::async_runtime::block_on(infra::data::clipboard::init_db(&pool))
                .expect("failed to init clipboard db");

            // 初始化图标缓存持久化（0.7.4）
            tauri::async_runtime::block_on(domain::search::icon::init(&pool))
                .expect("failed to init icon cache");

            // 初始化配置
            let config_start = std::time::Instant::now();
            tauri::async_runtime::block_on(app::config::init_config(&pool))
                .expect("failed to init config");
            // 记录配置加载耗时（setup 在同步上下文，需用 block_on）
            let config_elapsed = config_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "config_load",
                config_elapsed,
                None,
            ));

            // 读取应用配置(快照)
            let app_config = tauri::async_runtime::block_on(app::config::get_config(&pool));

            // 日志级别 reload 到配置值（init 时为默认 error）
            infra::utils::logging::update_level(&app_config.log_level);

            // pool 交给 Tauri 管理(command 层用 app.state 取);AppContext 再留一份 clone
            app.manage(pool.clone());

            // 同步开机自启（确保注册表 Run 项与配置一致，覆盖用户在 app 外改动的情况）
            {
                use tauri_plugin_autostart::ManagerExt;
                let manager = app.autolaunch();
                let _ = if app_config.auto_start {
                    manager.enable()
                } else {
                    manager.disable()
                };
            }

            // 主窗口启动即隐藏；注册焦点事件
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
                // 拦截 Alt+Space 系统菜单 + 启用系统级圆角（Win11+ DWM）
                if let Ok(hwnd) = w.hwnd() {
                    // hwnd 来自 Tauri(windows 0.61)，转成本项目依赖的 windows 0.62 HWND
                    let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
                    infra::platform::window::install_sysmenu_blocker(hwnd);
                    infra::platform::window::enable_rounded_corners(hwnd);
                }
                w.on_window_event(move |event| {
                    if let WindowEvent::Focused(focused) = event {
                        infra::platform::window::on_focused(*focused);
                    }
                });
            }

            // 托盘菜单：设置 + 退出
            let settings = MenuItem::with_id(app, "settings", "Settings", true, None::<&str>)?;
            let quit = MenuItem::with_id(app, "quit", "Quit Blink", true, None::<&str>)?;
            let menu = Menu::with_items(app, &[&settings, &quit])?;
            TrayIconBuilder::new()
                .icon(app.default_window_icon().unwrap().clone())
                .tooltip("Blink")
                .menu(&menu)
                .on_menu_event(|app, event| match event.id().as_ref() {
                    "settings" => open_settings(app),
                    "quit" => app.exit(0),
                    _ => {}
                })
                .build(app)?;

            // 加载 builtin 插件
            // 全局代理配置(engine:_global_proxy → {http,https})，进程启动时 env 注入，ureq/reqwest 原生读取
            let global_proxy = tauri::async_runtime::block_on(app::config::get_engine_config(&pool, "_global_proxy"));
            let proxy = global_proxy.and_then(|v| {
                let http = v.get("http").and_then(|s| s.as_str()).map(|s| s.to_string());
                let https = v.get("https").and_then(|s| s.as_str()).map(|s| s.to_string());
                if http.is_some() || https.is_some() { Some((http.unwrap_or_default(), https.unwrap_or_default())) }
                else { None }
            });
            let plugins = domain::plugin::load_builtin_plugins(app.handle(), proxy.clone());
            // 构造意图路由 RuleRouter,从插件 manifest 注入规则(合并用户自定义 triggers)。
            let router = std::sync::Arc::new(domain::intent::RuleRouter::new(app_config.surface_takeover_enabled));

            let plugin_engine = if plugins.is_empty() {
                None
            } else {
                tracing::info!(count = plugins.len(), "PluginEngine 已构造");
                let engine = std::sync::Arc::new(domain::plugin::PluginEngine::new(plugins.clone(), pool.clone(), proxy));
                // 0.4→0.5 自动迁移（首次运行时执行一次，后续 marker 跳过）
                tauri::async_runtime::block_on(infra::data::history::migrate_0_4_to_0_5(&pool, &plugins));
                // 加载/初始化每个插件配置(不存在则写默认 {enabled, settings:null})。
                tauri::async_runtime::block_on(engine.init_configs());

                // 0.8.2 §3.4：把 PluginEngine 作为 PluginSettingResolver 后置注入 RuleRouter,
                // 让 Context 触发能读插件 target_lang(如翻译)。同步 app_language 快照。
                router.set_setting_resolver(engine.clone() as std::sync::Arc<dyn domain::plugin::PluginSettingResolver>);
                router.set_app_language(app_config.language.clone());

                // 注入规则到 RuleRouter（合并 manifest triggers + 用户自定义 triggers）
                for plugin in &plugins {
                    if !engine.is_enabled(plugin.id()) {
                        tracing::debug!(plugin = %plugin.id(), "插件已禁用,跳过规则注入");
                        continue;
                    }
                    // 读取配置（含自定义 triggers）
                    let config = engine.get_config(plugin.id()).unwrap_or_default();
                    let effective_triggers = config.effective_triggers(&plugin.manifest().triggers);
                    router.reload_plugin_triggers(plugin.id(), &effective_triggers);
                }

                Some(engine)
            };

            // 构造三层搜索引擎配置（应用搜索 / 文件搜索 / 计算器）
            let start_menu_config = tauri::async_runtime::block_on(app::config::get_start_menu_config(&pool));
            let file_config = tauri::async_runtime::block_on(app::config::get_file_search_config(&pool));
            let calc_config = tauri::async_runtime::block_on(app::config::get_calc_config(&pool));
            tracing::info!(
                app_search = start_menu_config.enabled,
                file_search = file_config.enabled,
                data_source = %file_config.data_source,
                calc = calc_config.enabled,
                "搜索引擎配置"
            );

            // 构造 SearchService(多路引擎 + 意图路由)。command 层经 app.state 取用。
            let engine_configs = domain::search::EngineConfigs {
                start_menu: start_menu_config,
                file: file_config,
                calc: calc_config,
            };
            let search_service = std::sync::Arc::new(domain::search::SearchService::new(
                app.handle().clone(),
                pool.clone(),
                domain::search::build_engines(engine_configs),
                plugin_engine.clone(),
                router.clone(),
            ));
            app.manage(search_service.clone());
            // 初始化 SearchService 的 max_results 内存值（来自 AppConfig，搜索热路径零 IO）
            search_service.update_max_results(app_config.max_results as usize);
            // 初始化内置动作 disable 列表（0.8.0 §1.3）
            search_service.update_disabled_builtin_actions(app_config.disabled_builtin_actions.clone());
            // 初始化 Autosuggestion 配置（0.8.1 §2.5）
            search_service.update_autosuggest_config(
                app_config.autosuggest_enabled,
                app_config.autosuggest_min_score,
            );
            // 初始化 context binding 禁用列表（0.8.3 §4.6）
            search_service.update_disabled_context_bindings(
                app_config.disabled_context_bindings.clone(),
            );
            // 初始化界面语言快照（0.8.1）— 供 empty_arg_hint 等 LocalizableText 解析用
            search_service.update_language(app_config.language.clone());
            // 启动清理过期搜索历史（后台 spawn，不阻塞启动；enabled=false 或 days=0 跳过）
            {
                let cleanup_pool = pool.clone();
                let days = app_config.search_history_days;
                let enabled = app_config.search_history_enabled;
                tauri::async_runtime::spawn(async move {
                    if enabled {
                        infra::data::history::cleanup_old(&cleanup_pool, days).await;
                    }
                });
            }
            // 注入 ContextConfig 内存缓存：invoke 热键回调零 IO 读它（热更新见 update_context_config）
            let context_config = tauri::async_runtime::block_on(app::config::get_context_config(&pool));
            app.manage(std::sync::Arc::new(std::sync::RwLock::new(context_config)));
            // RuleRouter 单独注册供设置页 API 用（triggers 热更新）
            app.manage(router.clone());
            // PluginEngine 单独注册供设置页 API 用
            app.manage(plugin_engine);

            // 后台服务编排:按依赖拓扑顺序启动(搜索预扫 / 看门狗 / 热键监听等)。
            // pool 与 config 就绪后才构建 AppContext —— 前置初始化(DB/配置/日志/托盘/窗口)
            // 仍留在 setup 中,它们是构建 ctx 的前提。
            let ctx = app::service::AppContext {
                app: app.handle().clone(),
                pool: pool.clone(),
                config: app_config,
            };
            let services = app::service::all_services(search_service);
            let svc_start = std::time::Instant::now();
            for svc in &services {
                if let Err(e) = tauri::async_runtime::block_on(svc.start(&ctx)) {
                    tracing::error!(service = svc.name(), error = %e, "service start failed");
                }
            }
            // 记录服务初始化耗时
            let svc_elapsed = svc_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "services_init",
                svc_elapsed,
                None,
            ));
            // 记录启动总耗时（setup 在同步上下文，需用 block_on）
            // 注意：必须在 app.manage(pool) 之前记录，否则 pool 已被 move
            let startup_total_ms = startup_start.elapsed().as_secs_f64() * 1000.0;
            tauri::async_runtime::block_on(infra::utils::perf::record_blocking(
                &pool,
                infra::utils::perf::MetricCategory::Startup,
                "total",
                startup_total_ms,
                None,
            ));

            // 0.8.5 Chord：构建 registry（注册 stub 动作）+ 注册 app state（command 层 try_state 取）
            let chord_registry = std::sync::Arc::new(crate::domain::chord::build_default_registry());
            app.manage(chord_registry);

            // 持有服务列表,保证其生命周期与 app 一致。
            // 0.2.1 各服务随进程退出即可,不接退出钩子;stop / 逆序清理留到 0.3 插件进程。
            app.manage(services);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            app::commands::hide_window,
            app::commands::hide_settings_window,
            app::commands::search_apps,
            app::commands::launch_app,
            app::commands::run_builtin_action,
            app::commands::list_builtin_actions,
            app::commands::set_disabled_builtin_actions,
            app::commands::list_context_bindings,
            app::commands::set_disabled_context_bindings,
            app::commands::trigger_chord,
            app::commands::list_chord_actions,
            app::commands::is_alt_down,
            app::commands::hide_chord_ball,
            app::commands::confirm_chord_selection,
            app::commands::get_storage_info,
            app::commands::clear_history,
            app::commands::get_app_info,
            app::commands::resize_window,
            app::commands::get_config,
            app::commands::update_hotkey,
            app::commands::update_tap_threshold,
            app::commands::update_grace_period,
            app::commands::update_general_config,
            app::commands::update_auto_start,
            app::commands::update_language,
            app::commands::reset_config,
            app::commands::record_hotkey,
            app::commands::update_log_level,
            app::commands::open_log_file,
            app::commands::open_log_dir,
            app::commands::get_log_info,
            app::commands::update_file_search,
            app::commands::get_start_menu_config,
            app::commands::update_start_menu_config,
            app::commands::get_calc_config,
            app::commands::update_calc_config,
            app::commands::probe_everything,
            app::commands::get_engine_config,
            app::commands::update_engine_config,
            app::commands::get_plugins,
            app::commands::update_plugin_config,
            app::commands::update_global_proxy,
            app::commands::get_context_config,
            app::commands::update_context_config,
            app::commands::open_containing_folder,
            app::commands::open_lnk_target,
            app::commands::copy_to_clipboard,
            app::commands::reset_item_history,
            app::commands::list_running_processes,
            app::commands::show_context_menu,
            app::commands::hide_context_menu,
            app::commands::context_menu_action,
            app::commands::probe_interpreters,
            app::commands::update_interpreter_config,
            app::commands::open_file_dialog,
            app::commands::get_clipboard_history,
            app::commands::search_clipboard_history,
            app::commands::record_clipboard_hit,
            app::commands::delete_clipboard_item,
            app::commands::clear_clipboard_history,
            app::commands::get_clipboard_stats,
            app::commands::get_perf_overview,
            app::commands::get_perf_percentiles,
            app::commands::get_perf_slow_queries,
            app::commands::get_perf_recent,
            app::commands::export_perf_report,
            app::commands::clear_perf_data,
            app::commands::open_url,
            app::commands::toggle_default_trigger,
            app::commands::add_custom_trigger,
            app::commands::delete_custom_trigger,
            app::commands::update_autosuggest_config
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 打开设置窗口（委托给 window 模块统一实现）。
fn open_settings(app: &tauri::AppHandle) {
    infra::platform::window::open_settings(app);
}

/// 最小 percent-decode：还原前端 `encodeURIComponent` 编码的图标路径。
/// 仅处理 `%XX`，非法序列原样保留。
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hi = (bytes[i + 1] as char).to_digit(16);
            let lo = (bytes[i + 2] as char).to_digit(16);
            if let (Some(h), Some(l)) = (hi, lo) {
                out.push((h * 16 + l) as u8);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}
