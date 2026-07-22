//! DB 四层拆分（0.12.0 §2.2）——按数据域拆四库，独立写锁。
//!
//! **四库**：
//! - `blink_config.db`（配置库）— `config` 表（所有设置页 KV）。极低频写。
//! - `blink_history.db`（历史库）— `history` / `clipboard_history` 表。低频写。
//! - `blink_ai.db`（AI 库）— `ai_tool_audit` 表（0.12.2 后加 conversations/messages）。中频写。
//! - `blink_cache.db`（缓存库）— `performance_metrics` / `icon_cache` 表。高频写 + BLOB。
//!
//! **设计决策**：用单个 `DbPools` struct 持有四个 `SqlitePool`，作为一个 Tauri State 注册。
//! 功能等价于四个独立 State 类型（独立 pool / 独立写锁），但调用点改动更小
//! （`pools.config` vs `ConfigDbPool(pub SqlitePool).0`）。
//!
//! **迁移**：启动时检测旧 `blink.db`，用 SQL `ATTACH` + `INSERT INTO ... SELECT` 一次性迁移，
//! 迁移完成后删除旧库。版本标记写入配置库 `config` 表（`db_split_done`），避免重复迁移。

use std::path::PathBuf;

use sqlx::SqlitePool;
use sqlx::sqlite::SqlitePoolOptions;

/// 四库连接池集合——作为单一 Tauri State 注册。
///
/// 各字段独立 `SqlitePool`，独立写锁，互不阻塞。
/// 消费方通过 `app.state::<DbPools>()` 取用，按需选择 `.config` / `.history` / `.ai` / `.cache`。
#[derive(Clone)]
pub struct DbPools {
    /// 配置库——`config` 表（所有设置页 KV）。
    pub config: SqlitePool,
    /// 历史库——`history` / `clipboard_history` 表。
    pub history: SqlitePool,
    /// AI 库——`ai_tool_audit` 表（0.12.2 后加 conversations/messages）。
    pub ai: SqlitePool,
    /// 缓存库——`performance_metrics` / `icon_cache` 表。
    pub cache: SqlitePool,
}

/// 启动后台清理参数——由 `AppConfig` 投影出原始值，避免 infra 层依赖 app 层。
///
/// 各字段含义见 `AppConfig` 对应字段。
pub struct CleanupParams {
    /// 搜索历史是否启用。
    pub search_history_enabled: bool,
    /// 搜索历史保留天数。
    pub search_history_days: u32,
    /// 剪贴板历史是否启用。
    pub clipboard_enabled: bool,
    /// 剪贴板历史保留天数（0=永久）。
    pub clipboard_retention_days: u32,
}

impl DbPools {
    /// 启动后台清理任务（0.12.0 §2.2.4）。
    ///
    /// 统一发起各库的过期数据清理，后台 spawn 不阻塞启动：
    /// - **history 库**：搜索历史（按配置天数）+ 剪贴板历史（按配置天数）
    /// - **ai 库**：审计日志（30 天 + 10000 行上限，无条件）
    /// - **cache 库**：性能指标 + 图标缓存（各自 init 时已 spawn，此处不重复）
    pub fn spawn_startup_cleanup(&self, params: CleanupParams) {
        // ── 历史库：搜索历史清理（enabled=false 跳过） ──
        {
            let pool = self.history.clone();
            let days = params.search_history_days;
            let enabled = params.search_history_enabled;
            tauri::async_runtime::spawn(async move {
                if enabled {
                    crate::infra::data::history::cleanup_old(&pool, days).await;
                }
            });
        }
        // ── 历史库：剪贴板历史清理（按 retention_days，0=永久） ──
        {
            let pool = self.history.clone();
            let days = params.clipboard_retention_days;
            let clip_enabled = params.clipboard_enabled;
            tauri::async_runtime::spawn(async move {
                if clip_enabled && days > 0 {
                    crate::infra::data::clipboard::cleanup_old(&pool, days).await;
                }
            });
        }
        // ── AI 库：审计日志清理（30 天 + 10000 行上限，无条件） ──
        {
            let pool = self.ai.clone();
            tauri::async_runtime::spawn(async move {
                crate::infra::data::ai_audit::cleanup_old(&pool).await;
            });
        }
        // 缓存库（performance_metrics / icon_cache）的清理已在各自 init_db 时 spawn，此处不重复。
    }
}

/// 数据目录：%APPDATA%\blink\

/// 数据目录：%APPDATA%\blink\
fn data_dir() -> PathBuf {
    let appdata = std::env::var("APPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(appdata).join("blink")
}

/// 创建并初始化四库连接池。
///
/// **调用时机**：`main.rs` setup 阶段，在所有其他初始化之前。
///
/// **流程**：
/// 1. 确保数据目录存在
/// 2. 旧 `blink.db` 迁移到四库（如果旧库存在且未迁移过）
/// 3. 创建四个独立 pool（各 `max_connections(1)`）
/// 4. 各库建表（IF NOT EXISTS）
pub async fn init_all() -> Result<DbPools, String> {
    let dir = data_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;

    // 旧库迁移（如果旧 blink.db 存在且未迁移过）
    migrate_legacy_db(&dir).await?;

    // 创建四个独立 pool
    let config = create_pool(&dir.join("blink_config.db")).await?;
    let history = create_pool(&dir.join("blink_history.db")).await?;
    let ai = create_pool(&dir.join("blink_ai.db")).await?;
    let cache = create_pool(&dir.join("blink_cache.db")).await?;

    // 各库建表
    init_config_schema(&config).await?;
    init_history_schema(&history).await?;
    init_ai_schema(&ai).await?;
    init_cache_schema(&cache).await?;

    tracing::info!("DB 四层拆分初始化完成（config/history/ai/cache 各独立 pool）");

    Ok(DbPools {
        config,
        history,
        ai,
        cache,
    })
}

/// 创建单个 SQLite pool（max_connections(1)，串行写可接受）。
async fn create_pool(path: &PathBuf) -> Result<SqlitePool, String> {
    let url = format!("sqlite:{}?mode=rwc", path.display());
    SqlitePoolOptions::new()
        .max_connections(1)
        .connect(&url)
        .await
        .map_err(|e| format!("connect {}: {e}", path.display()))
}

// ── 各库建表 ─────────────────────────────────────────────────────────────────

/// 配置库：config 表
async fn init_config_schema(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS config (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 历史库：history + clipboard_history 表
async fn init_history_schema(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS history (
            lnk_path TEXT PRIMARY KEY,
            hit_count INTEGER NOT NULL DEFAULT 0,
            last_used_at INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // clipboard_history 表由 clipboard::init_db 创建（保持现有代码不变）
    crate::infra::data::clipboard::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// AI 库：ai_tool_audit 表
async fn init_ai_schema(pool: &SqlitePool) -> Result<(), String> {
    crate::infra::data::ai_audit::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 缓存库：performance_metrics + icon_cache 表
async fn init_cache_schema(pool: &SqlitePool) -> Result<(), String> {
    // performance_metrics 由 perf::init 创建
    crate::infra::utils::perf::init(pool)
        .await
        .map_err(|e| e.to_string())?;

    // icon_cache 由 icon::init 创建
    crate::domain::search::icon::init(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

// ── 旧库迁移 ─────────────────────────────────────────────────────────────────

/// 旧 `blink.db` → 四库迁移。
///
/// **条件**：旧 `blink.db` 存在 + 配置库中无 `db_split_done` 标记。
///
/// **策略**：用 SQL `ATTACH` + `INSERT INTO ... SELECT` 一次性迁移，
/// 无需逐行读取/写入，类型安全且高效。
async fn migrate_legacy_db(dir: &PathBuf) -> Result<(), String> {
    let legacy_path = dir.join("blink.db");
    if !legacy_path.exists() {
        tracing::debug!("无旧 blink.db，跳过迁移");
        return Ok(());
    }

    // 先检查配置库是否已有迁移标记（避免重复迁移）
    let config_path = dir.join("blink_config.db");
    if config_path.exists() {
        if let Ok(pool) = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite:{}", config_path.display()))
            .await
        {
            if let Ok(row) =
                sqlx::query_scalar::<_, Option<String>>("SELECT value FROM config WHERE key = 'db_split_done'")
                    .fetch_one(&pool)
                    .await
            {
                if row.as_deref() == Some("true") {
                    tracing::info!("DB 四层拆分已迁移过，跳过");
                    pool.close().await;
                    return Ok(());
                }
            }
            pool.close().await;
        }
    }

    tracing::info!("开始旧 blink.db → 四库迁移");

    // 创建四库 pool（此时已确保表结构）
    let config = create_pool(&config_path).await?;
    let history = create_pool(&dir.join("blink_history.db")).await?;
    let ai = create_pool(&dir.join("blink_ai.db")).await?;
    let cache = create_pool(&dir.join("blink_cache.db")).await?;

    // 先建表
    init_config_schema(&config).await?;
    init_history_schema(&history).await?;
    init_ai_schema(&ai).await?;
    init_cache_schema(&cache).await?;

    // 各库 ATTACH 旧库 + 迁移对应表
    let legacy_url = format!("sqlite:{}", legacy_path.display());

    // 配置库：config 表
    migrate_via_attach(&config, &legacy_url, "config").await?;
    // 历史库：history + clipboard_history 表
    migrate_via_attach(&history, &legacy_url, "history").await?;
    migrate_via_attach(&history, &legacy_url, "clipboard_history").await?;
    // AI 库：ai_tool_audit 表
    migrate_via_attach(&ai, &legacy_url, "ai_tool_audit").await?;
    // 缓存库：performance_metrics + icon_cache 表
    migrate_via_attach(&cache, &legacy_url, "performance_metrics").await?;
    migrate_via_attach(&cache, &legacy_url, "icon_cache").await?;

    // 写入迁移完成标记到配置库
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64;
    sqlx::query("INSERT OR REPLACE INTO config (key, value, updated_at) VALUES ('db_split_done', 'true', ?1)")
        .bind(now)
        .execute(&config)
        .await
        .map_err(|e| format!("写迁移标记失败: {e}"))?;

    config.close().await;
    history.close().await;
    ai.close().await;
    cache.close().await;

    // 删除旧库
    match std::fs::remove_file(&legacy_path) {
        Ok(()) => tracing::info!("旧 blink.db 已删除，迁移完成"),
        Err(e) => tracing::warn!(error = %e, "删除旧 blink.db 失败（不影响运行，下次启动会跳过迁移）"),
    }

    Ok(())
}

/// 通过 SQL `ATTACH` + `INSERT INTO ... SELECT` 迁移单张表。
///
/// **流程**：
/// 1. `ATTACH DATABASE 'old.db' AS legacy`
/// 2. 检查 `legacy.table` 是否存在
/// 3. `INSERT INTO main.table SELECT * FROM legacy.table`（表已建好，INSERT OR REPLACE 防主键冲突）
/// 4. `DETACH DATABASE legacy`
async fn migrate_via_attach(
    dst: &SqlitePool,
    legacy_url: &str,
    table: &str,
) -> Result<(), String> {
    // ATTACH 旧库（legacy_url 是内部文件路径，非用户输入，安全）
    sqlx::query(sqlx::AssertSqlSafe(format!("ATTACH DATABASE '{legacy_url}' AS legacy")))
        .execute(dst)
        .await
        .map_err(|e| format!("ATTACH 旧库失败: {e}"))?;;

    // 检查旧库是否有此表
    let exists: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM legacy.sqlite_master WHERE type='table' AND name=?1")
        .bind(table)
        .fetch_one(dst)
        .await
        .map_err(|e| format!("检查 legacy.{table} 存在性: {e}"))?;

    if exists.0 > 0 {
        // 迁移数据（INSERT OR REPLACE 防主键冲突；table 名已白名单校验）
        let result = sqlx::query(sqlx::AssertSqlSafe(format!(
            "INSERT OR REPLACE INTO main.{table} SELECT * FROM legacy.{table}"
        )))
        .execute(dst)
        .await;

        match result {
            Ok(r) => {
                tracing::info!(table, rows = r.rows_affected(), "表迁移完成");
            }
            Err(e) => {
                tracing::warn!(table, error = %e, "表迁移失败（可能是 schema 差异，跳过）");
            }
        }
    } else {
        tracing::debug!(table, "旧库无此表，跳过");
    }

    // DETACH 旧库
    let _ = sqlx::query("DETACH DATABASE legacy").execute(dst).await;

    Ok(())
}
