//! 配置库访问层（0.12.0 §2.2.3 分层修复）——从 `history.rs` 迁出。
//!
//! `config` 表的 CRUD 操作独立到此文件，历史库（`history.rs`）不再直持配置访问。
//!
//! **向后兼容**：`history.rs` 通过 `pub use` 重导出本模块的函数，
//! 现有调用点（`history::get_config` 等）无需改动。

use std::collections::HashMap;

use sqlx::SqlitePool;

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

/// 删除配置值（0.8.8 §8.7：`AppConfig` 分片迁移完毕后清理旧 `app_config` 单 key）。
pub async fn delete_config(pool: &SqlitePool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM config WHERE key = ?1")
        .bind(key)
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

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    async fn in_memory_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (
                key TEXT PRIMARY KEY,
                value TEXT NOT NULL,
                updated_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("create config table");
        pool
    }

    #[tokio::test]
    async fn get_config_returns_none_for_missing_key() {
        let pool = in_memory_pool().await;
        assert!(get_config(&pool, "nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn set_then_get_roundtrip() {
        let pool = in_memory_pool().await;
        set_config(&pool, "test_key", "test_value")
            .await
            .unwrap();
        assert_eq!(
            get_config(&pool, "test_key").await,
            Some("test_value".to_string())
        );
    }

    #[tokio::test]
    async fn set_config_updates_existing() {
        let pool = in_memory_pool().await;
        set_config(&pool, "key", "v1").await.unwrap();
        set_config(&pool, "key", "v2").await.unwrap();
        assert_eq!(get_config(&pool, "key").await, Some("v2".to_string()));
    }

    #[tokio::test]
    async fn delete_config_removes_key() {
        let pool = in_memory_pool().await;
        set_config(&pool, "key", "val").await.unwrap();
        delete_config(&pool, "key").await.unwrap();
        assert!(get_config(&pool, "key").await.is_none());
    }

    #[tokio::test]
    async fn get_all_config_returns_all() {
        let pool = in_memory_pool().await;
        set_config(&pool, "a", "1").await.unwrap();
        set_config(&pool, "b", "2").await.unwrap();
        let all = get_all_config(&pool).await;
        assert_eq!(all.len(), 2);
        assert_eq!(all.get("a"), Some(&"1".to_string()));
        assert_eq!(all.get("b"), Some(&"2".to_string()));
    }
}
