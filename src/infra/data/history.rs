//! 历史记录：SQLite 存储执行次数，用于搜索结果频率加权。

use std::collections::HashMap;
use std::sync::Arc;

use sqlx::SqlitePool;

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

/// 0.9.5 前端重构把 camelCase 字段名统一为 snake_case，同时移除了后端 `serde(rename_all = "camelCase")`。
/// 存量 DB 中的旧 JSON 仍是 camelCase，直接反序列化会静默 fallback 默认值（用户配置丢失）。
/// 此迁移把三个 config key 的字段名从 camelCase 改写为 snake_case，跑一次后写 marker 跳过。
pub async fn migrate_camelcase_to_snake(pool: &SqlitePool) {
    const MARKER_KEY: &str = "migration_camelcase_to_snake_done";
    if get_config(pool, MARKER_KEY).await.is_some() {
        return;
    }
    tracing::info!("开始执行 camelCase→snake_case 配置迁移");

    // 字段映射表：camelCase → snake_case
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

    for &(key, ref map) in tasks {
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
pub use crate::infra::data::config::{delete_config, get_all_config, get_config, set_config};

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
