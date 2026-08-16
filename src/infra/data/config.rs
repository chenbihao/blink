//! 配置库访问层（0.12.0 §2.2.3 分层修复）——从 `history.rs` 迁出。
//!
//! `config` 表的 CRUD 操作独立到此文件，历史库（`history.rs`）不再直持配置访问。
//!
//! **向后兼容**：`history.rs` 通过 `pub use` 重导出本模块的函数，
//! 现有调用点（`history::get_config` 等）无需改动。

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use sqlx::SqlitePool;

// ── 内存缓存 ──────────────────────────────────────────────────────────────
//
// 配置写入频率极低（仅用户改设置时），读频率极高（每次唤起 7+ 次）。
// 缓存 Some(String) 结果；None 不缓存（缺失 key 走 default 兜底，不影响正确性）。
// set 时更新缓存，delete 时移除缓存——保证一致性。
//
// dead_code 说明：缓存仅在 `#[cfg(not(test))]` 路径消费，test 编译不可见。
#[allow(dead_code)]
static CONFIG_CACHE: OnceLock<RwLock<HashMap<String, String>>> = OnceLock::new();

#[allow(dead_code)]
fn config_cache() -> &'static RwLock<HashMap<String, String>> {
    CONFIG_CACHE.get_or_init(|| RwLock::new(HashMap::new()))
}

/// 获取配置值。
pub async fn get_config(pool: &SqlitePool, key: &str) -> Option<String> {
    // 生产环境：先查内存缓存（测试环境跳过——多 pool 并行测试会互相污染）
    #[cfg(not(test))]
    {
        if let Ok(cache) = config_cache().read()
            && let Some(val) = cache.get(key)
        {
            return Some(val.clone());
        }
    }
    // 查 SQLite
    let row: (String,) = sqlx::query_as("SELECT value FROM config WHERE key = ?1")
        .bind(key)
        .fetch_optional(pool)
        .await
        .ok()??;
    // 回填缓存
    #[cfg(not(test))]
    {
        if let Ok(mut cache) = config_cache().write() {
            cache.insert(key.to_string(), row.0.clone());
        }
    }
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
    // 同步更新缓存
    #[cfg(not(test))]
    {
        if let Ok(mut cache) = config_cache().write() {
            cache.insert(key.to_string(), value.to_string());
        }
    }
    Ok(())
}

/// 删除配置值（0.8.8 §8.7：`AppConfig` 分片迁移完毕后清理旧 `app_config` 单 key）。
pub async fn delete_config(pool: &SqlitePool, key: &str) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM config WHERE key = ?1")
        .bind(key)
        .execute(pool)
        .await?;
    // 移除缓存
    #[cfg(not(test))]
    {
        if let Ok(mut cache) = config_cache().write() {
            cache.remove(key);
        }
    }
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

// ── 配置迁移（0.21.14：migrate_0_4_to_0_5 已上移到 app/config.rs）────────────
//
// infra 层不反向依赖 domain::plugin::PluginHandle / domain::config::PluginConfig。
// migrate_0_4_to_0_5 的逻辑现由 app 层负责，调用 infra 的 get/set_config primitive。

/// 0.9.5 前端重构把 camelCase 字段名统一为 snake_case。
/// 存量 DB 中的旧 JSON 仍是 camelCase，此迁移把字段名改写为 snake_case，跑一次后写 marker 跳过。
pub async fn migrate_camelcase_to_snake(pool: &SqlitePool) {
    const MARKER_KEY: &str = "migration_camelcase_to_snake_done";
    if get_config(pool, MARKER_KEY).await.is_some() {
        return;
    }
    tracing::info!("开始执行 camelCase→snake_case 配置迁移");

    let general_map: &[(&str, &str)] = &[
        ("searchHistoryEnabled", "search_history_enabled"),
        ("searchHistoryDays", "search_history_days"),
        ("maxResults", "max_results"),
        ("pageSize", "page_size"),
    ];
    let start_menu_map: &[(&str, &str)] =
        &[("scanDepth", "scan_depth"), ("includeUwp", "include_uwp")];
    let file_search_map: &[(&str, &str)] = &[
        ("dataSource", "data_source"),
        ("everythingPort", "everything_port"),
        ("maxResults", "max_results"),
        ("localDirs", "local_dirs"),
        ("localMaxDepth", "local_max_depth"),
        ("localCacheTtlSec", "local_cache_ttl_sec"),
        ("localMaxResults", "local_max_results"),
    ];

    let tasks: &[(&str, &[(&str, &str)])] = &[
        ("general_config", general_map),
        ("engine:start_menu", start_menu_map),
        ("engine:file_search", file_search_map),
    ];

    for &(key, map) in tasks {
        let Some(json_str) = get_config(pool, key).await else {
            continue;
        };
        let Ok(mut obj) = serde_json::from_str::<serde_json::Value>(&json_str) else {
            tracing::warn!(key, "配置 JSON 解析失败，跳过迁移");
            continue;
        };
        let Some(map_obj) = obj.as_object_mut() else {
            continue;
        };
        let mut changed = false;
        for &(from, to) in map.iter() {
            if let Some(val) = map_obj.remove(from) {
                map_obj.insert(to.to_string(), val);
                changed = true;
            }
        }
        if changed {
            match serde_json::to_string(&obj) {
                Ok(new_json) => {
                    if let Err(e) = set_config(pool, key, &new_json).await {
                        tracing::warn!(key, error = %e, "迁移写入失败");
                    } else {
                        tracing::info!(key, "camelCase→snake_case 迁移完成");
                    }
                }
                Err(e) => tracing::warn!(key, error = %e, "迁移序列化失败"),
            }
        }
    }

    if let Err(e) = set_config(pool, MARKER_KEY, "1").await {
        tracing::warn!(error = %e, "迁移标记写入失败");
    }
    tracing::info!("camelCase→snake_case 配置迁移完成");
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
        set_config(&pool, "test_key", "test_value").await.unwrap();
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
