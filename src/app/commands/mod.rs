//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。
//! 0.14.6 §2.4：按域拆分到子模块，mod.rs 聚合 re-export。

mod ai;
mod attachments;
mod clipboard;
pub(crate) mod config; // set_config_internal 为同步应用服务复用
mod config_sync; // 0.28.6：配置同步命令（WebDAV 设置/测试/同步/初始化）
mod gif; // 0.28.7：GIF 录制/编辑/导出命令
mod content_editor;
mod diagnostic;
mod feature_catalog;
mod guide;
mod input;
mod local_engine;
mod managed_runtime;
mod mcp;
mod plugin;
mod plugin_center;
pub(crate) mod plugin_ui;
mod prompt_library;
mod script_interpreter;
mod search;
mod shared;
mod sticky;
mod stt;
mod terminal;

pub use ai::*;
pub use attachments::*;
pub use clipboard::*;
pub use config::*;
pub use config_sync::*;
pub use gif::*;
pub use content_editor::*;
pub use diagnostic::*;
pub use feature_catalog::*;
pub use guide::*;
pub use input::*;
pub use local_engine::*;
pub use managed_runtime::*;
pub use mcp::*;
pub use plugin::*;
pub use plugin_center::*;
pub use plugin_ui::*;
pub use prompt_library::*;
pub use script_interpreter::*;
pub use search::*;
pub use sticky::*;
pub use stt::*;
pub use terminal::*;

/// 解析配置主题值为实际主题名：`auto` 跟随系统深浅色（Windows 注册表
/// `AppsUseLightTheme`，读取失败回落 dark），其余原样返回。
///
/// 供需要**具体主题资产**的宿主面消费（插件页 `/@host/theme.css?name=X`
/// 注入、右键菜单配色）——`themes/` 目录没有 `auto.css`，回传原始 `auto`
/// 会导致注入恒 404（0.28.2 起插件窗口无主题的根因之一）。前端主窗的
/// auto 实时监听（prefers-color-scheme）不经过本函数。
pub(crate) fn resolve_effective_theme(raw: &str) -> String {
    if raw != "auto" {
        return raw.to_string();
    }
    let is_light = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER)
        .open_subkey("SOFTWARE\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize")
        .and_then(|k| k.get_value::<u32, _>("AppsUseLightTheme"))
        .map(|v| v == 1)
        .unwrap_or(false);
    if is_light { "light".into() } else { "dark".into() }
}
