#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calc;
mod commands;
mod config;
mod history;
mod hotkey;
mod logging;
mod search;
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
        .setup(|app| {
            // 初始化历史记录 SQLite
            let pool = tauri::async_runtime::block_on(history::init_db())
                .expect("failed to init history db");

            // 初始化配置
            tauri::async_runtime::block_on(config::init_config(&pool))
                .expect("failed to init config");

            // 读取热键配置
            let app_config = tauri::async_runtime::block_on(config::get_config(&pool));
            let hotkey_config = app_config.hotkey.clone();

            // 日志级别 reload 到配置值（init 时为默认 error）
            logging::update_level(&app_config.log_level);

            app.manage(pool);

            // 搜索缓存：后台预扫开始菜单 + 定时增量刷新（避免每次输入重扫）
            search::init();

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

            // 失焦隐藏看门狗
            window::start_watchdog(app.handle().clone());

            // 启动热键监听（使用配置的快捷键）
            let app_handle = app.handle().clone();
            let mut hotkey_rx = hotkey::start(hotkey_config, app_config.tap_threshold);
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = hotkey_rx.recv().await {
                    match ev {
                        hotkey::HotkeyEvent::Tap(_) => {
                            // toggle：已可见则隐藏（仅快捷键；单实例重复运行仍走 invoke 总是显示）
                            if window::is_visible() {
                                window::hide(&app_handle, "toggle");
                            } else {
                                window::invoke(&app_handle);
                            }
                        }
                    }
                }
            });

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
