//! 历史记录：SQLite 存储执行次数，用于搜索结果频率加权。

use std::collections::HashMap;
use std::path::PathBuf;

use sqlx::sqlite::SqlitePoolOptions;
use sqlx::SqlitePool;

/// 初始化 SQLite 连接 + 建表。返回连接池。
pub async fn init_db() -> Result<SqlitePool, String> {
    let db_path = db_path();
    let db_dir = db_path.parent().ok_or("invalid db path")?;
    std::fs::create_dir_all(db_dir).map_err(|e| e.to_string())?;

    let url = format!("sqlite:{}?mode=rwc", db_path.display());
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS history (
            lnk_path TEXT PRIMARY KEY,
            hit_count INTEGER NOT NULL DEFAULT 0,
            last_used_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(pool)
}

/// 记录一次执行：存在则 hit_count+1，不存在则插入。
pub async fn record_launch(pool: &SqlitePool, lnk_path: &str) {
    let now = chrono::Utc::now().timestamp();
    let _ = sqlx::query(
        "INSERT INTO history (lnk_path, hit_count, last_used_at) VALUES (?1, 1, ?2)
         ON CONFLICT(lnk_path) DO UPDATE SET hit_count = hit_count + 1, last_used_at = ?2",
    )
    .bind(lnk_path)
    .bind(now)
    .execute(pool)
    .await;
}

/// 获取所有历史权重：lnk_path → hit_count。
pub async fn get_weights(pool: &SqlitePool) -> HashMap<String, i64> {
    let rows: Vec<(String, i64)> = sqlx::query_as("SELECT lnk_path, hit_count FROM history")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    rows.into_iter().collect()
}

/// 获取历史记录总条数。
pub async fn count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM history")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));
    row.0
}

/// 清空历史记录。
pub async fn clear(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM history").execute(pool).await;
}

/// SQLite 文件路径字符串（供前端显示）。
pub fn db_path_str() -> String {
    db_path().to_string_lossy().to_string()
}

/// SQLite 文件路径：%APPDATA%\blink\blink.db
fn db_path() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("blink").join("blink.db")
}
