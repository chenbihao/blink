//! 性能指标 DB 层（0.12.0 §2.2.3 分层修复）——schema 从 `infra/utils/perf.rs` 迁出。
//!
//! `performance_metrics` 表的 schema + 清理策略在此文件。
//! `infra/utils/perf.rs` 只保留 SLO 埋点 API（record / Timer / query / ai_slo）。

use sqlx::SqlitePool;

/// 性能指标行数上限——防极端膨胀（0.12.0 §2.2.4）。
const PERF_MAX_ROWS: i64 = 50_000;

/// 初始化 schema：建表 + 索引。
pub async fn init_schema(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS performance_metrics (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            category TEXT NOT NULL,
            name TEXT NOT NULL,
            value_ms REAL NOT NULL,
            metadata TEXT,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_perf_created ON performance_metrics(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_perf_cat_name ON performance_metrics(category, name)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    tracing::debug!("performance_metrics 表已初始化");
    Ok(())
}

/// 清理过期指标数据 + 行数上限兜底（0.12.0 §2.2.4）。
///
/// 两级策略：
/// 1. 按天清理：删除超过 30 天的记录
/// 2. 行数兜底：若仍超过 50000 行，删除最旧的超出部分
pub async fn cleanup_old(pool: &SqlitePool) {
    // 1. 按天清理（30 天）
    let cutoff = chrono::Utc::now().timestamp() - 30 * 86400;
    match sqlx::query("DELETE FROM performance_metrics WHERE created_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, cutoff, "清理过期性能指标");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期性能指标失败"),
    }

    // 2. 行数兜底（50000 行）
    let total = count(pool).await;
    if total > PERF_MAX_ROWS {
        let excess = total - PERF_MAX_ROWS;
        let _ = sqlx::query(
            "DELETE FROM performance_metrics WHERE rowid IN (
                SELECT rowid FROM performance_metrics ORDER BY created_at ASC LIMIT ?1
            )",
        )
        .bind(excess)
        .execute(pool)
        .await;
        tracing::info!(deleted = excess, total, "清理超量性能指标");
    }
}

/// 性能指标总行数（设置页存储统计用）。
pub async fn count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM performance_metrics")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// 清除全部性能指标数据。
pub async fn clear_all(pool: &SqlitePool) -> Result<u64, String> {
    let r = sqlx::query("DELETE FROM performance_metrics")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    let rows = r.rows_affected();
    tracing::info!(rows, "清除全部性能指标");
    Ok(rows)
}
