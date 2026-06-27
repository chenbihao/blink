#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calc;
mod commands;
mod config;
mod context;
mod history;
mod hotkey;
mod intent;
mod locale;
mod logging;
mod plugin;
mod search;
mod service;
mod text;
mod window;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

fn main() {
    // 初始化日志（尽早，默认 error；setup 读配置后 reload 到用户级别）
    logging::init("error");

    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            window::invoke(app);
        }))
        .plugin(tauri_plugin_autostart::init(
            tauri_plugin_autostart::MacosLauncher::LaunchAgent,
            None,
        ))
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
                    search::icon::get_icon_png(&path)
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
            // 初始化历史记录 SQLite
            let pool = tauri::async_runtime::block_on(history::init_db())
                .expect("failed to init history db");

            // 初始化配置
            tauri::async_runtime::block_on(config::init_config(&pool))
                .expect("failed to init config");

            // 读取应用配置(快照)
            let app_config = tauri::async_runtime::block_on(config::get_config(&pool));

            // 日志级别 reload 到配置值（init 时为默认 error）
            logging::update_level(&app_config.log_level);

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
                // 拦截 Alt+Space 系统菜单（防左上角弹移动/最大化菜单）
                if let Ok(hwnd) = w.hwnd() {
                    // hwnd 来自 Tauri(windows 0.61)，转成本项目依赖的 windows 0.62 HWND
                    window::install_sysmenu_blocker(windows::Win32::Foundation::HWND(hwnd.0 as _));
                }
                w.on_window_event(move |event| {
                    if let WindowEvent::Focused(focused) = event {
                        window::on_focused(*focused);
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
            let global_proxy = tauri::async_runtime::block_on(crate::config::get_engine_config(&pool, "_global_proxy"));
            let proxy = global_proxy.and_then(|v| {
                let http = v.get("http").and_then(|s| s.as_str()).map(|s| s.to_string());
                let https = v.get("https").and_then(|s| s.as_str()).map(|s| s.to_string());
                if http.is_some() || https.is_some() { Some((http.unwrap_or_default(), https.unwrap_or_default())) }
                else { None }
            });
            let plugins = plugin::load_builtin_plugins(app.handle(), proxy.clone());
            let plugin_engine = if plugins.is_empty() {
                None
            } else {
                tracing::info!(count = plugins.len(), "PluginEngine 已构造");
                let engine = std::sync::Arc::new(plugin::PluginEngine::new(plugins.clone(), pool.clone(), proxy));
                // 0.4→0.5 自动迁移（首次运行时执行一次，后续 marker 跳过）
                tauri::async_runtime::block_on(crate::history::migrate_0_4_to_0_5(&pool, &plugins));
                // 加载/初始化每个插件配置(不存在则写默认 {enabled, settings:null})。
                tauri::async_runtime::block_on(engine.init_configs());
                Some(engine)
            };

            // 构造意图路由 RuleRouter,从插件 manifest 注入规则。
            let router = std::sync::Arc::new(intent::RuleRouter::new(app_config.surface_takeover_enabled));
            for plugin in &plugins {
                // 跳过启动时已禁用的插件(不注入其 keyword 规则,输其触发词走 Generic)。
                // 注:运行时禁用的插件规则已注入,Takeover 命中后由 query_subset 跳过查询;
                //    该路径 Takeover 空白为已知限制(需 RuleRouter remove API,见 0.5 §3.1 / 0.2 §3.7 B5)。
                if let Some(ref pe) = plugin_engine {
                    if !pe.is_enabled(plugin.id()) {
                        tracing::debug!(plugin = %plugin.id(), "插件已禁用,跳过规则注入");
                        continue;
                    }
                }
                for trigger in &plugin.manifest().triggers {
                    match trigger {
                        plugin::PluginTrigger::Keyword { keyword, exclusive } => {
                            // 向后兼容:旧 exclusive=true→Auto(无参 priority / 带参 takeover),
                            // false→Inline(始终混排)。
                            let surface = if *exclusive {
                                intent::Surface::Auto
                            } else {
                                intent::Surface::Inline
                            };
                            router.add_keyword_rule(
                                plugin.id().to_string(),
                                keyword.clone(),
                                surface,
                                intent::SurfaceView::List,
                            );
                        }
                        plugin::PluginTrigger::Regex { pattern, exclusive } => {
                            let surface = if *exclusive {
                                intent::Surface::Auto
                            } else {
                                intent::Surface::Inline
                            };
                            if let Err(e) = router.add_regex_rule(
                                plugin.id().to_string(),
                                pattern,
                                surface,
                                intent::SurfaceView::List,
                            ) {
                                tracing::warn!(plugin = %plugin.id(), error = %e, "regex trigger 注入失败,跳过");
                            }
                        }
                    }
                }
            }

            // 构造 FileEngine 配置（从 app_config 读取）
            let file_config = config::FileSearchConfig {
                enabled: app_config.file_search.enabled,
                everything_port: app_config.file_search.everything_port,
                local_scan_depth: app_config.file_search.local_scan_depth,
                max_results: app_config.file_search.max_results,
            };

            // 构造 SearchService(多路引擎 + 意图路由)。command 层经 app.state 取用。
            let search_service = std::sync::Arc::new(search::SearchService::new(
                app.handle().clone(),
                pool.clone(),
                search::build_engines(Some(file_config)),
                plugin_engine.clone(),
                router,
            ));
            app.manage(search_service.clone());
            // 初始化 SearchService 的 max_results 内存值（来自 AppConfig，搜索热路径零 IO）
            search_service.update_max_results(app_config.max_results as usize);
            // 启动清理过期搜索历史（后台 spawn，不阻塞启动；enabled=false 或 days=0 跳过）
            {
                let cleanup_pool = pool.clone();
                let days = app_config.search_history_days;
                let enabled = app_config.search_history_enabled;
                tauri::async_runtime::spawn(async move {
                    if enabled {
                        crate::history::cleanup_old(&cleanup_pool, days).await;
                    }
                });
            }
            // 注入 ContextConfig 内存缓存：invoke 热键回调零 IO 读它（热更新见 update_context_config）
            let context_config = tauri::async_runtime::block_on(config::get_context_config(&pool));
            app.manage(std::sync::Arc::new(std::sync::RwLock::new(context_config)));
            // PluginEngine 单独注册供设置页 API 用
            app.manage(plugin_engine);

            // 后台服务编排:按依赖拓扑顺序启动(搜索预扫 / 看门狗 / 热键监听等)。
            // pool 与 config 就绪后才构建 AppContext —— 前置初始化(DB/配置/日志/托盘/窗口)
            // 仍留在 setup 中,它们是构建 ctx 的前提。
            let ctx = service::AppContext {
                app: app.handle().clone(),
                pool,
                config: app_config,
            };
            let services = service::all_services(search_service);
            for svc in &services {
                if let Err(e) = tauri::async_runtime::block_on(svc.start(&ctx)) {
                    tracing::error!(service = svc.name(), error = %e, "service start failed");
                }
            }
            // 持有服务列表,保证其生命周期与 app 一致。
            // 0.2.1 各服务随进程退出即可,不接退出钩子;stop / 逆序清理留到 0.3 插件进程。
            app.manage(services);

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            commands::hide_window,
            commands::hide_settings_window,
            commands::search_apps,
            commands::launch_app,
            commands::get_storage_info,
            commands::clear_history,
            commands::resize_window,
            commands::get_config,
            commands::update_hotkey,
            commands::update_tap_threshold,
            commands::update_grace_period,
            commands::update_general_config,
            commands::update_auto_start,
            commands::update_language,
            commands::reset_config,
            commands::record_hotkey,
            commands::update_log_level,
            commands::open_log_file,
            commands::open_log_dir,
            commands::get_log_info,
            commands::update_file_search,
            commands::probe_everything,
            commands::get_engine_config,
            commands::update_engine_config,
            commands::get_plugins,
            commands::update_plugin_config,
            commands::update_global_proxy,
            commands::get_context_config,
            commands::update_context_config,
            commands::open_containing_folder,
            commands::open_lnk_target,
            commands::copy_to_clipboard,
            commands::reset_item_history,
            commands::list_running_processes,
            commands::show_context_menu,
            commands::hide_context_menu,
            commands::context_menu_action
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

/// 打开设置窗口：已存在则聚焦，否则创建。
fn open_settings(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Blink Settings")
        .inner_size(960.0, 680.0)
        .min_inner_size(760.0, 520.0)
        .center()
        .build();
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
