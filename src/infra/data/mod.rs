//! 数据持久化层：SQLite 表操作
//
//! 0.12.0 §2.2.3 分层修复：schema 统一归此层，领域层不再直持 DB。

pub mod ai_audit;
pub mod clipboard;
pub mod config;
pub mod history;
pub mod icon_cache;
pub mod perf;
pub mod pools;

pub use pools::{CleanupParams, DbPools};
