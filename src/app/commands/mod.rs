//! Tauri command 层：前端 invoke 入口，组合 core/search/history 能力。
//!
//! 命令保持轻量——编排逻辑，不含业务实现。
//! 0.14.6 §2.4：按域拆分到子模块，mod.rs 聚合 re-export。

mod ai;
mod clipboard;
mod config;
mod content_editor;
mod diagnostic;
mod mcp;
mod plugin;
mod search;
mod shared;
mod stt;

pub use ai::*;
pub use clipboard::*;
pub use config::*;
pub use content_editor::*;
pub use diagnostic::*;
pub use mcp::*;
pub use plugin::*;
pub use search::*;
pub use stt::*;
