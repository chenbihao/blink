//! 数据持久化层：SQLite 表操作
//
//! 0.12.0 §2.2.3 分层修复：schema 统一归此层，领域层不再直持 DB。

pub mod ai_audit;
pub mod clipboard;
pub mod clipboard_images;
pub mod config;
pub mod conversations;
pub mod history;
pub mod icon_cache;
pub mod perf;
pub mod pools;

pub use pools::{CleanupParams, DbPools};

/// 执行 VACUUM 收缩数据库文件（0.16.0）。
///
/// 在 `clear_all` / `clear` 等 DELETE 操作后调用，实际回收磁盘空间。
/// VACUUM 需要独占锁，不能在事务内执行——sqlx 的 `execute` 自动提交，
/// 满足此约束。失败只 warn 不中断（收缩是 best-effort）。
pub async fn vacuum(pool: &sqlx::SqlitePool) {
    if let Err(e) = sqlx::query("VACUUM").execute(pool).await {
        tracing::warn!(error = %e, "VACUUM 失败（数据库可能被占用）");
    }
}
