//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。
//! 0.14.6 §2.4：按域拆分到子模块，mod.rs 聚合 re-export。

mod ai;
mod attachments;
mod clipboard;
pub(crate) mod config; // set_config_internal 为同步应用服务复用
mod config_sync; // 0.28.6：配置同步命令（WebDAV 设置/测试/同步/初始化）
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
mod plugin_ui;
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
