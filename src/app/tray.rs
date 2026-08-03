//! 系统托盘菜单构建 + 文案 i18n。
//!
//! 托盘菜单文字构建在 Rust 侧（`main.rs` setup），不走前端 i18n 字典——前端字典是 JS
//! 对象 Rust 无法 import，且托盘文字仅 3 条，本地 match 比维护一份共享字典更直接。
//!
//! **运行时热切换**：`set_config` 的 `"language"` 分支调用 [`rebuild_menu`] 重建菜单。
//! `on_menu_event` 挂在 `TrayIcon` 上而非 `Menu` 上，`set_menu` 替换菜单不影响 id 路由。
//!
//! 托盘默认 id 为 `"main"`（`TrayIconBuilder::new()` 未显式 `.id()`），`rebuild_menu`
//! 通过 `app.tray_by_id("main")` 取回。

use tauri::Manager;
use tauri::menu::{Menu, MenuItem};

/// 托盘菜单项 key（与菜单 item id 一一对应）。
#[derive(Clone, Copy)]
pub enum TrayText {
    Settings,
    StickyManager,
    About,
    Quit,
}

/// 托盘菜单项文案（按语言解析）。
///
/// `lang` 走 BCP47 前缀（`"zh"` / `"en"`），与 `AppConfig.language` 一致。
/// 未识别语言降级为英文。
pub fn text(lang: &str, key: TrayText) -> &'static str {
    match (lang.starts_with("zh"), key) {
        (true, TrayText::Settings) => "设置",
        (true, TrayText::StickyManager) => "便签管理",
        (true, TrayText::About) => "关于 Blink",
        (true, TrayText::Quit) => "退出 Blink",
        (false, TrayText::Settings) => "Settings",
        (false, TrayText::StickyManager) => "Sticky Manager",
        (false, TrayText::About) => "About Blink",
        (false, TrayText::Quit) => "Quit Blink",
    }
}

/// 构建托盘菜单（不挂事件——事件由 `TrayIconBuilder::on_menu_event` 统一挂）。
///
/// 菜单 item id（`"settings"` / `"about"` / `"quit"`）是稳定的，重建菜单后 id 不变，
/// `on_menu_event` 路由依然有效。
pub fn build_menu(app: &impl Manager<tauri::Wry>, lang: &str) -> tauri::Result<Menu<tauri::Wry>> {
    let settings = MenuItem::with_id(
        app,
        "settings",
        text(lang, TrayText::Settings),
        true,
        None::<&str>,
    )?;
    let sticky_manager = MenuItem::with_id(
        app,
        "sticky_manager",
        text(lang, TrayText::StickyManager),
        true,
        None::<&str>,
    )?;
    let about = MenuItem::with_id(
        app,
        "about",
        text(lang, TrayText::About),
        true,
        None::<&str>,
    )?;
    let sep = tauri::menu::PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", text(lang, TrayText::Quit), true, None::<&str>)?;
    Menu::with_items(app, &[&settings, &sticky_manager, &sep, &about, &sep, &quit])
}

/// 重建托盘菜单（运行时语言切换时调用）。
///
/// 托盘默认 id `"main"`（见模块级注释）。重建失败仅 warn，不阻断语言切换主流程。
///
/// 接收 `AppHandle` 而非泛型 `impl Manager`——`tray_by_id` 是 `App`/`AppHandle` 的
/// inherent method，不在 `Manager` trait 上。`build_menu` 仍走泛型，setup（`&mut App`）
/// 与此处都能调用。
pub fn rebuild_menu(app: &tauri::AppHandle, lang: &str) {
    let Some(tray) = app.tray_by_id("main") else {
        tracing::warn!("rebuild_menu: tray_by_id(\"main\") 未找到，跳过托盘重建");
        return;
    };
    match build_menu(app, lang) {
        Ok(menu) => {
            if let Err(e) = tray.set_menu(Some(menu)) {
                tracing::warn!(%e, "rebuild_menu: set_menu 失败");
            }
        }
        Err(e) => tracing::warn!(%e, "rebuild_menu: build_menu 失败"),
    }
}
