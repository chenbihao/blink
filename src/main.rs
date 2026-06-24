#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calc;
mod commands;
mod config;
mod history;
mod hotkey;
mod logging;
mod plugin;
mod search;
mod service;
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
            tracing::debug!(%path, "blink-icon: 收到图标请求");
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

            // 构造 SearchService(多路引擎)。command 层经 app.state 取用,故单独 manage;
            // 引擎后台任务(开始菜单预扫等)由 SearchLifecycle 在下面 services 启动时触发。
            let search_service = std::sync::Arc::new(search::SearchService::new(
                app.handle().clone(),
                pool.clone(),
                search::build_engines(app.handle()),
            ));
            app.manage(search_service.clone());

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
            commands::search_apps,
            commands::launch_app,
            commands::get_storage_info,
            commands::clear_history,
            commands::resize_window,
            commands::get_config,
            commands::update_hotkey,
            commands::update_tap_threshold,
            commands::update_grace_period,
            commands::update_auto_start,
            commands::update_language,
            commands::reset_config,
            commands::record_hotkey,
            commands::update_log_level,
            commands::open_log_file,
            commands::open_log_dir,
            commands::get_log_info
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
