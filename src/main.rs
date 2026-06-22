#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod calc;
mod commands;
mod history;
mod hotkey;
mod search;
mod window_ctl;

use tauri::{
    menu::{Menu, MenuItem},
    tray::TrayIconBuilder,
    Manager, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            window_ctl::invoke(app);
        }))
        .setup(|app| {
            // 初始化历史记录 SQLite
            let pool = tauri::async_runtime::block_on(history::init_db())
                .expect("failed to init history db");
            app.manage(pool);

            // 主窗口启动即隐藏；注册焦点事件
            if let Some(w) = app.get_webview_window("main") {
                let _ = w.hide();
                w.on_window_event(move |event| {
                    if let WindowEvent::Focused(focused) = event {
                        window_ctl::on_focused(*focused);
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
            window_ctl::start_watchdog(app.handle().clone());

            // 右 Alt tap → 唤起窗口
            let app_handle = app.handle().clone();
            let mut hotkey_rx = hotkey::start();
            tauri::async_runtime::spawn(async move {
                while let Some(ev) = hotkey_rx.recv().await {
                    match ev {
                        hotkey::HotkeyEvent::Tap(_) => window_ctl::invoke(&app_handle),
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
            commands::resize_window
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
        .inner_size(800.0, 600.0)
        .min_inner_size(600.0, 400.0)
        .center()
        .build();
}
