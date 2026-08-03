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
pub mod sticky;

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

/// 按需 VACUUM：检测 freelist 占比超阈值时执行 VACUUM（0.17.0）。
///
/// SQLite 的 DELETE 只把页标记为空闲页加入 freelist，文件不缩。
/// 此函数查 `PRAGMA freelist_count` / `PRAGMA page_count`，比值超 threshold
/// 则调 `vacuum()` 实际回收磁盘空间。返回是否执行了 VACUUM。
///
/// **调用时机**：启动后台清理之后（`spawn_startup_cleanup` 末尾）。
/// `max_connections(1)` 的连接池在 VACUUM 期间独占连接，启动时无用户交互查询，影响可忽略。
pub async fn vacuum_if_needed(pool: &sqlx::SqlitePool, threshold: f64) -> bool {
    let freelist: (i64,) = match sqlx::query_as("PRAGMA freelist_count")
        .fetch_one(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "查 freelist_count 失败，跳过 VACUUM");
            return false;
        }
    };
    let page_count: (i64,) = match sqlx::query_as("PRAGMA page_count")
        .fetch_one(pool)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            tracing::warn!(error = %e, "查 page_count 失败，跳过 VACUUM");
            return false;
        }
    };

    let free = freelist.0;
    let total = page_count.0;
    if total == 0 {
        return false;
    }
    let ratio = free as f64 / total as f64;
    if ratio < threshold {
        tracing::debug!(
            free, total, ratio, threshold,
            "freelist 占比低于阈值，跳过 VACUUM"
        );
        return false;
    }

    let started = std::time::Instant::now();
    tracing::info!(free, total, ratio, "freelist 占比超阈值，执行 VACUUM");
    vacuum(pool).await;
    tracing::info!(elapsed_ms = started.elapsed().as_millis(), "VACUUM 完成");
    true
}
