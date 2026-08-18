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
pub mod permission_memory;
pub mod pools;
pub mod sticky;

pub use pools::{CleanupParams, DbPools};

/// 压缩单个数据库文件（0.16.0 起为 VACUUM；0.21.17 补 `wal_checkpoint(TRUNCATE)`）。
///
/// 在 `clear_all` / `clear` 等 DELETE 操作后调用，实际回收磁盘空间。
///
/// **WAL 模式铁则**：四库均 `journal_mode=WAL`，仅 `VACUUM` 不会收缩磁盘文件——
/// 空闲页与未 checkpoint 的数据都在 `-wal` 里，且 Blink 连接池常驻、连接永不关闭，
/// 必须追加 `PRAGMA wal_checkpoint(TRUNCATE)` 才能真正截断 `-wal` 并把主库缩到实际大小。
///
/// VACUUM 需要独占锁，不能在事务内执行——sqlx 的 `execute` 自动提交，满足此约束。
/// 失败返回 Err（调用方按 best-effort 处理），不中断主操作。
pub async fn compact(pool: &sqlx::SqlitePool) -> Result<(), String> {
    if let Err(e) = sqlx::query("VACUUM").execute(pool).await {
        tracing::warn!(error = %e, "VACUUM 失败（数据库可能被占用）");
        return Err(format!("VACUUM 失败: {e}"));
    }
    if let Err(e) = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "wal_checkpoint(TRUNCATE) 失败（数据库可能被占用）");
        return Err(format!("wal_checkpoint(TRUNCATE) 失败: {e}"));
    }
    Ok(())
}

/// 按需压缩：检测 freelist 占比超阈值时执行 `compact()`（0.17.0）。
///
/// SQLite 的 DELETE 只把页标记为空闲页加入 freelist，文件不缩。
/// 此函数查 `PRAGMA freelist_count` / `PRAGMA page_count`，比值超 threshold
/// 则调 `compact()` 实际回收磁盘空间。返回是否执行了压缩。
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
    let page_count: (i64,) = match sqlx::query_as("PRAGMA page_count").fetch_one(pool).await {
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
            free,
            total,
            ratio,
            threshold,
            "freelist 占比低于阈值，跳过 VACUUM"
        );
        return false;
    }

    let started = std::time::Instant::now();
    tracing::info!(free, total, ratio, "freelist 占比超阈值，执行 compact");
    let _ = compact(pool).await;
    tracing::info!(elapsed_ms = started.elapsed().as_millis(), "compact 完成");
    true
}
