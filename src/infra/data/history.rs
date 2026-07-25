//! 历史记录：SQLite 存储执行次数，用于搜索结果频率加权。
//!
//! 配置迁移函数（`migrate_0_4_to_0_5` / `migrate_camelcase_to_snake`）已迁至 `config.rs`，
//! 此处通过 `pub use` 重导出保持向后兼容。

use std::collections::HashMap;

use sqlx::SqlitePool;

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
    rows.into_iter()
        .map(|(path, hit, last)| (path, (hit, last)))
        .collect()
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

// ── 配置访问层（0.12.0 §2.2.3 迁移到 infra/data/config.rs）────────────────────
//
// 向后兼容重导出——现有调用点 `history::get_config` 等无需改动。
pub use crate::infra::data::config::{
    delete_config, get_all_config, get_config, set_config, migrate_0_4_to_0_5,
    migrate_camelcase_to_snake,
};

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

    #[test]
    fn camelcase_to_snake_migrates_general_config() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            // 模拟旧前端写入的 camelCase JSON
            let old_json = r#"{"theme":"gruvbox","searchHistoryEnabled":false,"searchHistoryDays":60,"maxResults":25,"pageSize":7}"#;
            set_config(&pool, "general_config", old_json).await.unwrap();

            migrate_camelcase_to_snake(&pool).await;

            let migrated = get_config(&pool, "general_config").await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&migrated).unwrap();
            assert_eq!(v["search_history_enabled"], false);
            assert_eq!(v["search_history_days"], 60);
            assert_eq!(v["max_results"], 25);
            assert_eq!(v["page_size"], 7);
            assert_eq!(v["theme"], "gruvbox");
            // 旧 camelCase key 应已消失
            assert!(v.get("searchHistoryEnabled").is_none());
            assert!(v.get("maxResults").is_none());
        });
    }

    #[test]
    fn camelcase_to_snake_migrates_start_menu() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            let old_json = r#"{"enabled":true,"scanDepth":5,"includeUwp":false}"#;
            set_config(&pool, "engine:start_menu", old_json)
                .await
                .unwrap();

            migrate_camelcase_to_snake(&pool).await;

            let migrated = get_config(&pool, "engine:start_menu").await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&migrated).unwrap();
            assert_eq!(v["scan_depth"], 5);
            assert_eq!(v["include_uwp"], false);
            assert!(v.get("scanDepth").is_none());
        });
    }

    #[test]
    fn camelcase_to_snake_migrates_file_search() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            let old_json = r#"{"enabled":true,"dataSource":"everything","everythingPort":8080,"maxResults":30}"#;
            set_config(&pool, "engine:file_search", old_json)
                .await
                .unwrap();

            migrate_camelcase_to_snake(&pool).await;

            let migrated = get_config(&pool, "engine:file_search").await.unwrap();
            let v: serde_json::Value = serde_json::from_str(&migrated).unwrap();
            assert_eq!(v["data_source"], "everything");
            assert_eq!(v["everything_port"], 8080);
            assert_eq!(v["max_results"], 30);
            assert!(v.get("dataSource").is_none());
        });
    }

    #[test]
    fn camelcase_to_snake_skips_already_migrated() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            // 已经是 snake_case 的数据不应被改动
            let snake_json = r#"{"theme":"dark","search_history_enabled":true,"search_history_days":30,"max_results":50,"page_size":9}"#;
            set_config(&pool, "general_config", snake_json)
                .await
                .unwrap();

            migrate_camelcase_to_snake(&pool).await;

            let migrated = get_config(&pool, "general_config").await.unwrap();
            assert_eq!(migrated, snake_json, "snake_case 数据不应被改写");
        });
    }

    #[test]
    fn camelcase_to_snake_marker_prevents_rerun() {
        tauri::async_runtime::block_on(async {
            let pool = in_memory_pool().await;
            let old_json = r#"{"theme":"dark","searchHistoryEnabled":false,"searchHistoryDays":10,"maxResults":50,"pageSize":9}"#;
            set_config(&pool, "general_config", old_json).await.unwrap();

            // 第一次迁移
            migrate_camelcase_to_snake(&pool).await;
            let first = get_config(&pool, "general_config").await.unwrap();

            // 再次写入旧数据，第二次迁移应跳过（marker 已存在）
            set_config(&pool, "general_config", old_json).await.unwrap();
            migrate_camelcase_to_snake(&pool).await;
            let second = get_config(&pool, "general_config").await.unwrap();

            // 第二次迁移跳过了，所以 second 仍是手动写入的旧 camelCase
            assert_eq!(second, old_json, "marker 存在时不应再迁移");
            // 而第一次迁移的结果是 snake_case
            let v: serde_json::Value = serde_json::from_str(&first).unwrap();
            assert!(v.get("searchHistoryEnabled").is_none());
        });
    }
}
