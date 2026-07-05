//! 历史记录：SQLite 存储执行次数，用于搜索结果频率加权。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

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

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(&pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(pool)
}

/// 0.4→0.5 自动迁移：
/// 1. ~~`app_config.file_search` → `engine:file_search`~~（已移除，未发版不需要）
/// 2. 为每个插件初始化默认配置（`plugin:{id}` 不存在则写入默认）
/// 3. 迁移完成后写 marker，下次不再执行
pub async fn migrate_0_4_to_0_5(
    pool: &SqlitePool,
    plugins: &[Arc<crate::domain::plugin::PluginHandle>],
) {
    const MARKER_KEY: &str = "migration_0_5_done";

    // 检查是否已迁移过
    if get_config(pool, MARKER_KEY).await.is_some() {
        return;
    }
    tracing::info!("开始执行 0.4→0.5 配置迁移");

    // ── 1. 初始化插件默认配置 ──
    for plugin in plugins {
        let plugin_id = plugin.id();
        let key = format!("plugin:{plugin_id}");
        if get_config(pool, &key).await.is_none() {
            let mut default_config = crate::app::config::PluginConfig::default();
            default_config.settings = plugin.manifest().default_settings();
            match serde_json::to_string(&default_config) {
                Ok(json) => {
                    if let Err(e) = set_config(pool, &key, &json).await {
                        tracing::warn!(plugin = %plugin_id, error = %e, "插件配置写入失败");
                    } else {
                        tracing::info!(plugin = %plugin_id, "初始化插件默认配置");
                    }
                }
                Err(e) => tracing::warn!(plugin = %plugin_id, error = %e, "插件配置初始化失败"),
            }
        }
    }

    // 标记迁移完成
    if let Err(e) = set_config(pool, MARKER_KEY, "1").await {
        tracing::warn!(error = %e, "迁移标记写入失败");
    }
    tracing::info!("0.4→0.5 配置迁移完成");
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

/// 获取所有历史权重：lnk_path → (hit_count, last_used_at)。
///
/// 0.7.5: 返回 last_used_at 用于时间衰减计算。
pub async fn get_weights(pool: &SqlitePool) -> HashMap<String, (i64, i64)> {
    let rows: Vec<(String, i64, i64)> =
        sqlx::query_as("SELECT lnk_path, hit_count, last_used_at FROM history")
            .fetch_all(pool)
            .await
            .unwrap_or_default();
    rows.into_iter().map(|(path, hit, last)| (path, (hit, last))).collect()
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

/// 重置某项的历史权重（删除该行，权重归零，不影响其他项）。
/// 用于右键菜单「重置该项记录」（0.5.3）。
pub async fn reset_weight(pool: &SqlitePool, lnk_path: &str) {
    let _ = sqlx::query("DELETE FROM history WHERE lnk_path = ?1")
        .bind(lnk_path)
        .execute(pool)
        .await;
}

/// 清理过期历史：删除 last_used_at 早于 `now - days*86400` 的记录。
/// `days=0` 视为永久保留（直接返回）。启动时按 search_history_days 调用。
pub async fn cleanup_old(pool: &SqlitePool, days: u32) {
    if days == 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - (days as i64 * 86400);
    match sqlx::query("DELETE FROM history WHERE last_used_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, cutoff, days, "清理过期搜索历史");
            } else {
                tracing::debug!(days, "无过期历史需清理");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期历史失败"),
    }
}

// ── 配置相关函数 ────────────────────────────────────────────────────────────────

/// 获取配置值。
pub async fn get_config(pool: &SqlitePool, key: &str) -> Option<String> {
    let row: (String,) = sqlx::query_as("SELECT value FROM config WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()??;
    Some(row.0)
}

/// 设置配置值（存在则更新，不存在则插入）。
pub async fn set_config(pool: &SqlitePool, key: &str, value: &str) -> Result<(), sqlx::Error> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO config (key, value, updated_at) VALUES (?1, ?2, ?3)
         ON CONFLICT(key) DO UPDATE SET value = ?2, updated_at = ?3",
    )
    .bind(key)
    .bind(value)
    .bind(now)
    .execute(pool)
    .await?;
    Ok(())
}

/// 获取所有配置。
pub async fn get_all_config(pool: &SqlitePool) -> HashMap<String, String> {
    let rows: Vec<(String, String)> = sqlx::query_as("SELECT key, value FROM config")
        .fetch_all(pool)
        .await
        .unwrap_or_default();
    rows.into_iter().collect()
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
