//! AI 权限记忆持久化（0.17.8）：SQLite 持久化用户对危险 tool 的信任授权。
//!
//! **设计**（见 phases/0.17-enhancement-polish.md §3.10）：
//! - 放 `blink_config.db`（配置类数据，不可清理——权限记忆是用户偏好）
//! - 记忆粒度 = `tool_name`（不纳入 args，内置 dangerous action 全无参，粒度安全）
//! - 过期策略：查询时判断 + 启动时批量清理；过期后重新弹确认卡片
//! - `memory_enabled = false` 时不查 DB，DB 数据保留
//!
//! **双层 trusted 设计**：
//! - 会话级 `HashSet<(conversation_id, tool_name)>`（进程内，重启即失）
//! - 持久化 DB 层（本模块），跨会话保留
//! - `is_trusted` 检查顺序：会话级命中 -> 跳过 DB；未命中 -> 查 DB -> 命中且未过期 -> 加入会话级

use sqlx::SqlitePool;

/// 初始化 `ai_permission_memory` 表（放 config 库）。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ai_permission_memory (
            tool_name TEXT PRIMARY KEY,
            trusted_at INTEGER NOT NULL,
            expires_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    tracing::debug!("ai_permission_memory 表已初始化");
    Ok(())
}

/// 记忆一个 tool 的用户信任授权（用户确认后调）。
///
/// 写入/更新 `trusted_at` + `expires_at`（trusted_at + memory_days 天）。
/// `INSERT OR REPLACE` 保证幂等——同一 tool 再次确认时刷新时间。
pub async fn trust_tool(pool: &SqlitePool, tool_name: &str, memory_days: u64) {
    let now = now_ts();
    let expires = now + (memory_days as i64) * 86_400;
    if let Err(e) = sqlx::query(
        "INSERT OR REPLACE INTO ai_permission_memory (tool_name, trusted_at, expires_at) VALUES (?1, ?2, ?3)",
    )
    .bind(tool_name)
    .bind(now)
    .bind(expires)
    .execute(pool)
    .await
    {
        tracing::warn!(error = %e, tool_name, "写入权限记忆失败");
    }
}

/// 查询 tool 是否已被用户信任且未过期。
///
/// 返回 `true` = 命中且未过期；`false` = 未命中或已过期。
/// **过期行实时删除**：查到过期行时同步 DELETE，保持表干净。
pub async fn is_tool_trusted(pool: &SqlitePool, tool_name: &str) -> bool {
    let now = now_ts();
    let row: (i64,) =
        match sqlx::query_as("SELECT expires_at FROM ai_permission_memory WHERE tool_name = ?1")
            .bind(tool_name)
            .fetch_optional(pool)
            .await
        {
            Ok(Some(r)) => r,
            Ok(None) => return false,
            Err(e) => {
                tracing::warn!(error = %e, tool_name, "查询权限记忆失败，视为不信任");
                return false;
            }
        };

    if row.0 > now {
        true
    } else {
        // 已过期——实时删除该行
        let _ = sqlx::query("DELETE FROM ai_permission_memory WHERE tool_name = ?1")
            .bind(tool_name)
            .execute(pool)
            .await;
        tracing::debug!(tool_name, "权限记忆已过期，删除行并重新询问");
        false
    }
}

/// 清空所有权限记忆（设置页"清除所有记忆"按钮调）。
///
/// 只清 DB 持久化层，不影响会话级 `HashSet`。
pub async fn clear_all_trusted(pool: &SqlitePool) {
    if let Err(e) = sqlx::query("DELETE FROM ai_permission_memory")
        .execute(pool)
        .await
    {
        tracing::warn!(error = %e, "清空权限记忆失败");
    }
    tracing::info!("所有权限记忆已清除");
}

/// 批量清理过期行（启动时调）。
pub async fn cleanup_expired(pool: &SqlitePool) {
    let now = now_ts();
    match sqlx::query("DELETE FROM ai_permission_memory WHERE expires_at < ?1")
        .bind(now)
        .execute(pool)
        .await
    {
        Ok(r) => {
            if r.rows_affected() > 0 {
                tracing::info!(count = r.rows_affected(), "清理过期权限记忆行");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期权限记忆失败"),
    }
}

/// 清理指定插件前缀的所有权限记忆行（插件禁用/卸载时调）。
///
/// 插件 tool_name 格式为 `plugin_{id}:tool_{name}`，按 `plugin_{id}:%` 前缀匹配。
pub async fn clear_plugin_trusted(pool: &SqlitePool, plugin_prefix: &str) {
    let pattern = format!("{plugin_prefix}:%");
    match sqlx::query("DELETE FROM ai_permission_memory WHERE tool_name LIKE ?1")
        .bind(&pattern)
        .execute(pool)
        .await
    {
        Ok(r) => {
            if r.rows_affected() > 0 {
                tracing::info!(
                    prefix = plugin_prefix,
                    count = r.rows_affected(),
                    "清理插件权限记忆行"
                );
            }
        }
        Err(e) => tracing::warn!(error = %e, prefix = plugin_prefix, "清理插件权限记忆失败"),
    }
}

/// 当前 unix 时间戳（秒）。
pub fn now_ts() -> i64 {
    chrono::Utc::now().timestamp()
}

// ── 测试 ────────────────────────────────────────────────────────────────────

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
        init_db(&pool).await.expect("init table");
        pool
    }

    #[tokio::test]
    async fn trust_tool_then_is_trusted_returns_true() {
        let pool = in_memory_pool().await;
        trust_tool(&pool, "shutdown", 7).await;
        assert!(is_tool_trusted(&pool, "shutdown").await);
    }

    #[tokio::test]
    async fn is_trusted_returns_false_for_unknown_tool() {
        let pool = in_memory_pool().await;
        assert!(!is_tool_trusted(&pool, "nonexistent").await);
    }

    #[tokio::test]
    async fn expired_trust_is_deleted_and_returns_false() {
        let pool = in_memory_pool().await;
        // 手动写入一条已过期的记录
        let now = now_ts();
        sqlx::query(
            "INSERT INTO ai_permission_memory (tool_name, trusted_at, expires_at) VALUES (?1, ?2, ?3)",
        )
        .bind("lock")
        .bind(now - 86_400 * 10) // 10 天前确认
        .bind(now - 1)            // 1 秒前过期
        .execute(&pool)
        .await
        .unwrap();

        // 查询时应返回 false 并删除行
        assert!(!is_tool_trusted(&pool, "lock").await);

        // 行应已被删除
        let row: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM ai_permission_memory WHERE tool_name = 'lock'")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(row.0, 0, "过期行应已被删除");
    }

    #[tokio::test]
    async fn trust_tool_updates_existing() {
        let pool = in_memory_pool().await;
        trust_tool(&pool, "shutdown", 7).await;
        trust_tool(&pool, "shutdown", 30).await; // 重新确认，延长到 30 天

        let row: (i64,) = sqlx::query_as(
            "SELECT expires_at FROM ai_permission_memory WHERE tool_name = 'shutdown'",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        let now = now_ts();
        assert!(row.0 > now + 86_400 * 25, "过期时间应更新为 30 天后");
    }

    #[tokio::test]
    async fn clear_all_trusted_empties_table() {
        let pool = in_memory_pool().await;
        trust_tool(&pool, "shutdown", 7).await;
        trust_tool(&pool, "lock", 7).await;
        trust_tool(&pool, "sleep", 7).await;

        clear_all_trusted(&pool).await;

        let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM ai_permission_memory")
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(row.0, 0, "表应已清空");
    }

    #[tokio::test]
    async fn cleanup_expired_removes_only_expired() {
        let pool = in_memory_pool().await;
        let now = now_ts();

        // 未过期
        trust_tool(&pool, "shutdown", 7).await;

        // 手动写入过期行
        sqlx::query(
            "INSERT INTO ai_permission_memory (tool_name, trusted_at, expires_at) VALUES (?1, ?2, ?3)",
        )
        .bind("lock_expired")
        .bind(now - 86_400 * 10)
        .bind(now - 1)
        .execute(&pool)
        .await
        .unwrap();

        cleanup_expired(&pool).await;

        // shutdown 保留，lock_expired 删除
        assert!(is_tool_trusted(&pool, "shutdown").await);
        assert!(!is_tool_trusted(&pool, "lock_expired").await);
    }

    #[tokio::test]
    async fn clear_plugin_trusted_removes_by_prefix() {
        let pool = in_memory_pool().await;
        trust_tool(&pool, "shutdown", 7).await; // 内置 tool，不受影响
        trust_tool(&pool, "plugin_weather:tool_forecast", 7).await;
        trust_tool(&pool, "plugin_weather:tool_alert", 7).await;
        trust_tool(&pool, "plugin_translate:tool_translate", 7).await;

        clear_plugin_trusted(&pool, "plugin_weather").await;

        // weather 插件的记忆已清
        assert!(!is_tool_trusted(&pool, "plugin_weather:tool_forecast").await);
        assert!(!is_tool_trusted(&pool, "plugin_weather:tool_alert").await);
        // 其他不受影响
        assert!(is_tool_trusted(&pool, "shutdown").await);
        assert!(is_tool_trusted(&pool, "plugin_translate:tool_translate").await);
    }
}
