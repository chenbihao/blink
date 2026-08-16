//! AI 工具调用审计日志（0.11.4 改进 2 §2.2.7）。
//!
//! 记录 AI 调用的工具名 / 参数 / 结果摘要 / 时间戳 / provider / 轮次，
//! 写入 `ai_tool_audit` 表。用户在设置页可查"AI 最近调了哪些工具"。
//!
//! **设计原则**：
//! - **只记摘要不记全量**：`result_summary` 截断到 500 字符，避免大结果（如截图 Blob）
//!   灌爆数据库。完整结果走前端展示，审计日志只做"回溯查询"用途。
//! - **参数 JSON 原样存**：`arguments` 是 JSON Object，直接 `to_string` 存。参数通常
//!   不大（city / url / text 等），无需截断。若未来出现超大参数再评估。
//! - **写入失败不阻塞主流程**：审计日志是观测层，写入失败只 `tracing::warn!`，
//!   不影响 AI 工具调用的主链路。
//! - **provider_kind 存字符串**：与 `ProviderKind` 的 serde rename 对齐，
//!   0.12 新增 provider 种类时老记录仍可读。
//!
//! **与 SLO 埋点的关系**：0.9.7 `CapabilityRegistry::invoke` 已有 SLO 埋点
//! （tracing 日志），本表是它的**持久化投影**——日志易过期，DB 可查长期历史。

use sqlx::SqlitePool;

/// 一条 AI 工具调用审计记录。
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)] // 0.12.x 设置页"AI 调用历史"面板消费
pub struct AuditLog {
    /// 自增主键。
    pub id: i64,
    /// 工具名（如 `builtin.weather:get_weather` / `open_path`）。
    pub tool_name: String,
    /// 参数 JSON 字符串（`ToolCall.arguments.to_string()`）。
    pub arguments: String,
    /// 结果摘要（截断到 500 字符）。
    pub result_summary: String,
    /// provider 种类字符串（`ProviderKind` serde rename）。
    pub provider_kind: String,
    /// 模型 id（如 `gpt-4o-mini`）。
    pub model_id: String,
    /// 轮次：1 = Turn 1（首次调用），2 = Turn 2（回流后的链式调用）。
    pub turn: u8,
    /// 调用来源（0.13.4）：`internal` = AI tool call / 用户确认执行；`mcp_external` = 外部 MCP client 调用。
    pub caller: String,
    /// UTC 时间戳（秒）。
    pub created_at: i64,
}

/// 结果摘要最大长度（超出截断 + `…`）。
const RESULT_SUMMARY_MAX: usize = 500;

/// 审计日志保留天数（0.12.0 §2.2.4）。
const AUDIT_RETENTION_DAYS: i64 = 30;
/// 审计日志行数上限——超出时删最旧的（0.12.0 §2.2.4）。
const AUDIT_MAX_ROWS: i64 = 10_000;

/// 初始化 `ai_tool_audit` 表。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS ai_tool_audit (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            tool_name TEXT NOT NULL,
            arguments TEXT NOT NULL DEFAULT '{}',
            result_summary TEXT NOT NULL DEFAULT '',
            provider_kind TEXT NOT NULL DEFAULT '',
            model_id TEXT NOT NULL DEFAULT '',
            turn INTEGER NOT NULL DEFAULT 1,
            caller TEXT NOT NULL DEFAULT 'internal',
            created_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 0.13.4 迁移：为已有数据库添加 caller 列
    // SQLite 不支持 IF NOT EXISTS，"duplicate column" 错误是正常的幂等行为，其他错误需记录
    match sqlx::query(
        "ALTER TABLE ai_tool_audit ADD COLUMN caller TEXT NOT NULL DEFAULT 'internal'",
    )
    .execute(pool)
    .await
    {
        Ok(_) => tracing::debug!("ai_tool_audit: caller 列迁移完成"),
        Err(e) => {
            let msg = e.to_string();
            if msg.contains("duplicate column") {
                tracing::debug!("ai_tool_audit: caller 列已存在，跳过迁移");
            } else {
                tracing::warn!(
                    error = %msg,
                    "ai_tool_audit: caller 列迁移失败（非幂等错误，后续 INSERT 可能失败）"
                );
            }
        }
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_audit_created ON ai_tool_audit(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    tracing::debug!("ai_tool_audit 表已初始化");
    Ok(())
}

/// 写入一条审计日志。
///
/// **写入失败不返回 Err**——审计是观测层，失败只 `tracing::warn!`，不阻塞主流程。
/// 调用方无需处理错误。
///
/// `caller` 参数（0.13.4）：`"internal"` = AI tool call / 用户确认执行；`"mcp_external"` = 外部 MCP client 调用。
#[allow(clippy::too_many_arguments)]
pub async fn save_audit_log(
    pool: &SqlitePool,
    tool_name: &str,
    arguments: &serde_json::Value,
    result_summary: &str,
    provider_kind: &str,
    model_id: &str,
    turn: u8,
    caller: &str,
) {
    let arguments_str = arguments.to_string();
    let summary = truncate_summary(result_summary);
    let now = chrono::Utc::now().timestamp();

    let result = sqlx::query(
        "INSERT INTO ai_tool_audit (tool_name, arguments, result_summary, provider_kind, model_id, turn, caller, created_at) \
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(tool_name)
    .bind(&arguments_str)
    .bind(&summary)
    .bind(provider_kind)
    .bind(model_id)
    .bind(turn as i64)
    .bind(caller)
    .bind(now)
    .execute(pool)
    .await;

    match result {
        Ok(_) => {
            tracing::debug!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                tool = %tool_name,
                turn,
                "审计日志写入"
            );
        }
        Err(e) => {
            tracing::warn!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                tool = %tool_name,
                error = %e,
                "审计日志写入失败（不阻塞主流程）"
            );
        }
    }
}

/// 查询最近的审计日志（按时间倒序）。
///
/// 供设置页"AI 调用历史"展示。`limit` 建议 50-200，避免一次性拉太多。
#[allow(dead_code)] // 0.12.x 设置页"AI 调用历史"面板消费
pub async fn query_recent(pool: &SqlitePool, limit: i64) -> Vec<AuditLog> {
    sqlx::query_as::<_, (i64, String, String, String, String, String, i64, String, i64)>(
        "SELECT id, tool_name, arguments, result_summary, provider_kind, model_id, turn, caller, created_at \
         FROM ai_tool_audit ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(id, tool_name, arguments, result_summary, provider_kind, model_id, turn, caller, created_at)| {
            AuditLog {
                id,
                tool_name,
                arguments,
                result_summary,
                provider_kind,
                model_id,
                turn: turn as u8,
                caller,
                created_at,
            }
        },
    )
    .collect()
}

/// 截断结果摘要到 `RESULT_SUMMARY_MAX` 字符（按字符不按字节，避免中文 panic）。
fn truncate_summary(s: &str) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= RESULT_SUMMARY_MAX {
        return s.to_string();
    }
    let truncated: String = chars.iter().take(RESULT_SUMMARY_MAX).collect();
    format!("{truncated}…")
}

/// 审计日志总行数（设置页存储统计用）。
pub async fn count(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM ai_tool_audit")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// 清空全部审计日志（设置页"清除 AI 调用历史"用）。
pub async fn clear_all(pool: &SqlitePool) {
    if let Err(e) = sqlx::query("DELETE FROM ai_tool_audit").execute(pool).await {
        tracing::warn!(error = %e, "清空 ai_tool_audit 失败");
    }
}

/// 清理过期审计日志（0.12.0 §2.2.4）。
///
/// 两级策略（与 `icon_cache::cleanup_old` 同模式）：
/// 1. **按天清理**：删除超过 30 天的记录
/// 2. **行数兜底**：若仍超过 10000 行，删除最旧的超出部分
///
/// 启动时由 `DbPools::spawn_startup_cleanup` 统一调用。
pub async fn cleanup_old(pool: &SqlitePool) {
    // 1. 按天清理
    let cutoff = chrono::Utc::now().timestamp() - AUDIT_RETENTION_DAYS * 86400;
    match sqlx::query("DELETE FROM ai_tool_audit WHERE created_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, cutoff, "清理过期审计日志");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期审计日志失败"),
    }

    // 2. 行数兜底
    let total = count(pool).await;
    if total > AUDIT_MAX_ROWS {
        let excess = total - AUDIT_MAX_ROWS;
        let _ = sqlx::query(
            "DELETE FROM ai_tool_audit WHERE id IN (
                SELECT id FROM ai_tool_audit ORDER BY created_at ASC LIMIT ?1
            )",
        )
        .bind(excess)
        .execute(pool)
        .await;
        tracing::info!(deleted = excess, total, "清理超量审计日志");
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_summary_short_unchanged() {
        assert_eq!(truncate_summary("短文本"), "短文本".to_string());
        // 显式传超长
        let short = "hello";
        assert_eq!(truncate_summary(short), "hello".to_string());
    }

    #[test]
    fn truncate_summary_long_gets_ellipsis() {
        let long: String = "a".repeat(600);
        let result = truncate_summary(&long);
        assert!(result.ends_with('…'));
        // 500 字符 + 1 个 …
        assert_eq!(result.chars().count(), 501);
    }

    #[test]
    fn truncate_summary_cjk_exactly_at_limit() {
        let exact: String = "中".repeat(RESULT_SUMMARY_MAX);
        assert_eq!(truncate_summary(&exact), exact);
    }

    #[test]
    fn truncate_summary_cjk_over_limit() {
        let over: String = "中".repeat(RESULT_SUMMARY_MAX + 10);
        let result = truncate_summary(&over);
        assert!(result.ends_with('…'));
        assert_eq!(result.chars().count(), RESULT_SUMMARY_MAX + 1);
    }

    #[test]
    fn truncate_summary_empty_string() {
        assert_eq!(truncate_summary(""), "");
    }

    // ── DB 集成测试（需 in-memory SQLite，跳过无 sqlx runtime 的环境） ──

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        init_db(&pool).await.expect("failed to init audit table");
        pool
    }

    #[tokio::test]
    async fn save_and_query_audit_log_roundtrip() {
        let pool = setup_pool().await;

        save_audit_log(
            &pool,
            "builtin.weather:get_weather",
            &serde_json::json!({"city": "北京"}),
            "北京 25°C 晴",
            "openai_compatible",
            "gpt-4o-mini",
            1,
            "internal",
        )
        .await;

        save_audit_log(
            &pool,
            "open_path",
            &serde_json::json!({"path": "C:\\\\code.lnk"}),
            "已打开 VSCode",
            "openai_compatible",
            "gpt-4o-mini",
            2,
            "internal",
        )
        .await;

        let logs = query_recent(&pool, 10).await;
        assert_eq!(logs.len(), 2);

        // 倒序：后写入的在前
        assert_eq!(logs[0].tool_name, "open_path");
        assert_eq!(logs[0].turn, 2);
        assert_eq!(logs[0].model_id, "gpt-4o-mini");
        assert!(logs[0].arguments.contains("code.lnk"));

        assert_eq!(logs[1].tool_name, "builtin.weather:get_weather");
        assert_eq!(logs[1].turn, 1);
        assert!(logs[1].arguments.contains("北京"));
        assert_eq!(logs[1].result_summary, "北京 25°C 晴");
    }

    #[tokio::test]
    async fn save_audit_log_truncates_long_summary() {
        let pool = setup_pool().await;
        let long_summary: String = "x".repeat(800);

        save_audit_log(
            &pool,
            "search_files",
            &serde_json::json!({}),
            &long_summary,
            "openai_compatible",
            "gpt-4o-mini",
            1,
            "internal",
        )
        .await;

        let logs = query_recent(&pool, 1).await;
        assert_eq!(logs.len(), 1);
        assert!(logs[0].result_summary.ends_with('…'));
        // 500 + 1
        assert_eq!(logs[0].result_summary.chars().count(), 501);
    }

    #[tokio::test]
    async fn query_recent_returns_empty_when_no_records() {
        let pool = setup_pool().await;
        let logs = query_recent(&pool, 10).await;
        assert!(logs.is_empty());
    }

    #[tokio::test]
    async fn query_recent_respects_limit() {
        let pool = setup_pool().await;
        for i in 0..10 {
            save_audit_log(
                &pool,
                &format!("tool_{i}"),
                &serde_json::json!({}),
                "ok",
                "openai_compatible",
                "m",
                1,
                "internal",
            )
            .await;
        }
        let logs = query_recent(&pool, 3).await;
        assert_eq!(logs.len(), 3);
        // 倒序：最新的在前（tool_9, tool_8, tool_7）
        assert_eq!(logs[0].tool_name, "tool_9");
        assert_eq!(logs[2].tool_name, "tool_7");
    }

    #[tokio::test]
    async fn init_db_is_idempotent() {
        // 多次 init 不报错
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        init_db(&pool).await.expect("first init failed");
        init_db(&pool).await.expect("second init failed");
    }

    // ── cleanup_old 测试（0.12.0 §2.2.4）───────────────────────────────────

    #[tokio::test]
    async fn cleanup_old_deletes_expired_records() {
        let pool = setup_pool().await;
        let now = chrono::Utc::now().timestamp();

        // 写入一条 31 天前的记录（应被清理）
        save_audit_log(
            &pool,
            "old_tool",
            &serde_json::json!({}),
            "old",
            "openai_compatible",
            "m",
            1,
            "internal",
        )
        .await;
        // 手动把 created_at 改到 31 天前
        let cutoff = now - 31 * 86400;
        let _ =
            sqlx::query("UPDATE ai_tool_audit SET created_at = ?1 WHERE tool_name = 'old_tool'")
                .bind(cutoff)
                .execute(&pool)
                .await;

        // 写入一条今天的记录（应保留）
        save_audit_log(
            &pool,
            "new_tool",
            &serde_json::json!({}),
            "new",
            "openai_compatible",
            "m",
            1,
            "internal",
        )
        .await;

        cleanup_old(&pool).await;

        let logs = query_recent(&pool, 10).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].tool_name, "new_tool");
    }

    #[tokio::test]
    async fn cleanup_old_enforces_row_limit() {
        let pool = setup_pool().await;

        // 写入 AUDIT_MAX_ROWS + 5 条记录（全部今天的，不会被按天清理）
        for i in 0..(AUDIT_MAX_ROWS + 5) {
            save_audit_log(
                &pool,
                &format!("tool_{i}"),
                &serde_json::json!({}),
                "ok",
                "openai_compatible",
                "m",
                1,
                "internal",
            )
            .await;
        }

        assert_eq!(count(&pool).await, AUDIT_MAX_ROWS + 5);

        cleanup_old(&pool).await;

        // 行数兜底应删到 AUDIT_MAX_ROWS
        assert_eq!(count(&pool).await, AUDIT_MAX_ROWS);
    }

    #[tokio::test]
    async fn cleanup_old_no_op_when_under_limits() {
        let pool = setup_pool().await;
        // 写入少量记录
        for i in 0..5 {
            save_audit_log(
                &pool,
                &format!("tool_{i}"),
                &serde_json::json!({}),
                "ok",
                "openai_compatible",
                "m",
                1,
                "internal",
            )
            .await;
        }
        cleanup_old(&pool).await;
        // 不超 30 天、不超行数，应全保留
        assert_eq!(count(&pool).await, 5);
    }

    #[tokio::test]
    async fn cleanup_old_empty_table_no_panic() {
        let pool = setup_pool().await;
        cleanup_old(&pool).await;
        assert_eq!(count(&pool).await, 0);
    }

    // ── caller 字段测试（0.13.4）─────────────────────────────────────────

    #[tokio::test]
    async fn caller_field_stored_and_queried() {
        let pool = setup_pool().await;

        // 写入 internal 调用
        save_audit_log(
            &pool,
            "search_files",
            &serde_json::json!({"query": "test"}),
            "found 3 files",
            "openai_compatible",
            "gpt-4o-mini",
            1,
            "internal",
        )
        .await;

        // 写入 mcp_external 调用
        save_audit_log(
            &pool,
            "read_clipboard",
            &serde_json::json!({}),
            "clipboard text",
            "",
            "",
            1,
            "mcp_external",
        )
        .await;

        let logs = query_recent(&pool, 10).await;
        assert_eq!(logs.len(), 2);

        // 倒序：后写入的在前（mcp_external）
        assert_eq!(logs[0].tool_name, "read_clipboard");
        assert_eq!(logs[0].caller, "mcp_external");

        assert_eq!(logs[1].tool_name, "search_files");
        assert_eq!(logs[1].caller, "internal");
    }

    #[tokio::test]
    async fn init_db_migration_adds_caller_to_existing_table() {
        // 模拟已有数据库（无 caller 列）
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");

        // 创建旧版表结构（无 caller 列）
        sqlx::query(
            "CREATE TABLE ai_tool_audit (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                tool_name TEXT NOT NULL,
                arguments TEXT NOT NULL DEFAULT '{}',
                result_summary TEXT NOT NULL DEFAULT '',
                provider_kind TEXT NOT NULL DEFAULT '',
                model_id TEXT NOT NULL DEFAULT '',
                turn INTEGER NOT NULL DEFAULT 1,
                created_at INTEGER NOT NULL DEFAULT 0
            )",
        )
        .execute(&pool)
        .await
        .expect("failed to create old schema table");

        // 插入一条旧记录
        sqlx::query("INSERT INTO ai_tool_audit (tool_name, arguments, result_summary, provider_kind, model_id, turn, created_at) VALUES ('old_tool', '{}', 'old', '', '', 1, 0)")
            .execute(&pool)
            .await
            .expect("failed to insert old record");

        // 运行 init_db（应自动迁移添加 caller 列）
        init_db(&pool).await.expect("init_db migration failed");

        // 验证旧记录的 caller 默认值
        let logs = query_recent(&pool, 10).await;
        assert_eq!(logs.len(), 1);
        assert_eq!(logs[0].tool_name, "old_tool");
        assert_eq!(logs[0].caller, "internal");

        // 验证新写入也能正常工作
        save_audit_log(
            &pool,
            "new_tool",
            &serde_json::json!({}),
            "new",
            "",
            "",
            1,
            "mcp_external",
        )
        .await;

        let logs = query_recent(&pool, 10).await;
        assert_eq!(logs.len(), 2);
        assert_eq!(logs[0].caller, "mcp_external");
        assert_eq!(logs[1].caller, "internal");
    }
}
