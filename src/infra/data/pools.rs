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

use crate::infra::utils::paths;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use std::path::PathBuf;
use std::time::Duration;

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
        // ── 缓存库：剪贴板图片清理（7 天，与文本剪贴板默认保留天数对齐） ──
        // 0.17.0：cleanup_old_images 之前是 dead code，现在接通。
        {
            let pool = self.cache.clone();
            tauri::async_runtime::spawn(async move {
                crate::infra::data::clipboard_images::cleanup_old_images(&pool, 7).await;
            });
        }
        // 缓存库（performance_metrics / icon_cache）的清理已在各自 init_db 时 spawn，此处不重复。

        // ── 0.17.7: 回收站过期便签清理（30 天） ──
        {
            let pool = self.history.clone();
            tauri::async_runtime::spawn(async move {
                crate::infra::data::sticky::cleanup_trashed(&pool, 30).await;
            });
        }

        // ── 0.17.8: 过期权限记忆清理（启动时批量删除过期行） ──
        {
            let pool = self.config.clone();
            tauri::async_runtime::spawn(async move {
                crate::infra::data::permission_memory::cleanup_expired(&pool).await;
            });
        }

        // ── 0.17.0: 按需 VACUUM（freelist 占比超 20% 则收缩） ──
        // 在所有清理之后执行，启动时无用户交互查询，VACUUM 独占连接影响可忽略。
        {
            let history = self.history.clone();
            let ai = self.ai.clone();
            let cache = self.cache.clone();
            tauri::async_runtime::spawn(async move {
                crate::infra::data::vacuum_if_needed(&history, 0.2).await;
                crate::infra::data::vacuum_if_needed(&ai, 0.2).await;
                crate::infra::data::vacuum_if_needed(&cache, 0.2).await;
            });
        }
    }
}

/// 数据目录：%APPDATA%\blink\
/// 统一走 `paths::app_data_dir()`，见 `infra::utils::paths`。
fn data_dir() -> PathBuf {
    paths::app_data_dir()
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
    let config = create_pool(&paths::db_path("blink_config.db")).await?;
    let history = create_pool(&paths::db_path("blink_history.db")).await?;
    let ai = create_pool(&paths::db_path("blink_ai.db")).await?;
    let cache = create_pool(&paths::db_path("blink_cache.db")).await?;

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

/// 创建单个 SQLite pool。
///
/// **SQLite 优化**（0.19.15 性能修复）：
/// - `journal_mode=WAL`：读写不互斥（WAL 允许并发读）
/// - `synchronous=NORMAL`：WAL 模式下安全，减少 fsync 频率
/// - `busy_timeout=5s`：锁冲突时等待而非立即失败
/// - `mmap_size=256MB`：内存映射 I/O，避免 read() 系统调用开销
/// - `cache_size=20MB`：SQLite 内部页面缓存，减少磁盘读取
///
/// 用 `SqliteConnectOptions` 确保 pool 中**每个连接**都设置 PRAGMA
/// （原先 `.execute("PRAGMA ...")` 只设置第一个连接）。
///
/// **背景**：原先 `max_connections(1)` + 默认 DELETE journal + 无 mmap/cache 导致
/// Alt+C 搜索延迟 ~315ms——即使加了 WAL 也不改善，因为查询本身需要从磁盘读取页面。
/// 覆盖索引 + mmap + cache 三管齐下后降至个位数毫秒。
async fn create_pool(path: &PathBuf) -> Result<SqlitePool, String> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true)
        .journal_mode(SqliteJournalMode::Wal)
        .synchronous(SqliteSynchronous::Normal)
        .busy_timeout(Duration::from_secs(5))
        .pragma("mmap_size", "268435456") // 256MB mmap
        .pragma("cache_size", "-20000") // 20MB page cache
        .pragma("temp_store", "MEMORY"); // 临时表用内存

    SqlitePoolOptions::new()
        .max_connections(4)
        .connect_with(options)
        .await
        .map_err(|e| format!("connect {}: {e}", path.display()))
}

// ── 各库建表 ─────────────────────────────────────────────────────────────────

/// 配置库：config 表 + ai_permission_memory 表（0.17.8）
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

    // 0.17.8: AI 权限记忆表（跨会话持久化用户对危险 tool 的信任授权）
    crate::infra::data::permission_memory::init_db(pool)
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

    // sticky_notes 表（0.16.7：桌面便签持久化）
    crate::infra::data::sticky::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// AI 库：ai_tool_audit 表 + conversations/messages 表（0.12.3）
async fn init_ai_schema(pool: &SqlitePool) -> Result<(), String> {
    crate::infra::data::ai_audit::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;
    crate::infra::data::conversations::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 缓存库：performance_metrics + icon_cache + clipboard_images 表
async fn init_cache_schema(pool: &SqlitePool) -> Result<(), String> {
    // performance_metrics 由 perf::init 创建
    crate::infra::utils::perf::init(pool)
        .await
        .map_err(|e| e.to_string())?;

    // icon_cache 由 icon::init 创建
    crate::infra::platform::icon::init(pool)
        .await
        .map_err(|e| e.to_string())?;

    // clipboard_images 由 clipboard_images::init_db 创建（0.16.4）
    crate::infra::data::clipboard_images::init_db(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 缓存库纯建表（迁移路径用）--只建 performance_metrics + icon_cache + clipboard_images 表，
/// 不注册全局 pool、不 spawn 清理（迁移 pool 临时使用后即 close）。
async fn init_cache_schema_only(pool: &SqlitePool) -> Result<(), String> {
    crate::infra::data::perf::init_schema(pool).await?;
    crate::infra::data::icon_cache::init_schema(pool).await?;
    crate::infra::data::clipboard_images::init_db(pool).await?;
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

    // 先检查配置库是否已有迁移标记（避免重复迁移）
    let config_path = dir.join("blink_config.db");
    if config_path.exists() {
        if let Ok(pool) = SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite:{}", config_path.display()))
            .await
        {
            if let Ok(row) = sqlx::query_scalar::<_, Option<String>>(
                "SELECT value FROM config WHERE key = 'db_split_done'",
            )
            .fetch_one(&pool)
            .await
            {
                if row.as_deref() == Some("true") {
                    // 迁移已完成，清理可能残留的失败标记（上次失败→本次成功的场景）
                    let _ = sqlx::query("DELETE FROM config WHERE key = 'migration_failed'")
                        .execute(&pool)
                        .await;
                    pool.close().await;
                    tracing::info!("DB 四层拆分已迁移过，跳过");
                    return Ok(());
                }
            }
            pool.close().await;
        }
    }

    if !legacy_path.exists() {
        tracing::debug!("无旧 blink.db，跳过迁移");
        return Ok(());
    }

    tracing::info!("开始旧 blink.db → 四库迁移");

    // 创建四库 pool（此时已确保表结构）
    let config = create_pool(&config_path).await?;
    let history = create_pool(&dir.join("blink_history.db")).await?;
    let ai = create_pool(&dir.join("blink_ai.db")).await?;
    let cache = create_pool(&dir.join("blink_cache.db")).await?;

    // 先建表（缓存库用纯建表函数，不注册全局 pool--迁移 pool 是临时的，
    // 占用 OnceCell 会导致 init_all 的最终 pool 注册失败 -> perf/icon_cache 写入静默失效）
    init_config_schema(&config).await?;
    init_history_schema(&history).await?;
    init_ai_schema(&ai).await?;
    init_cache_schema_only(&cache).await?;

    // ATTACH 旧库用纯文件路径（反斜杠替换为正斜杠，SQLite 跨平台兼容）。
    // 不加 `sqlite:` 前缀——那是 sqlx 连接字符串格式，ATTACH 不认。
    let legacy_url = legacy_path.display().to_string().replace('\\', "/");

    // 各库 ATTACH 旧库 + 迁移对应表。
    // 若任一迁移失败，仍需 close 已创建的 pool 避免泄漏。
    let migrate_result: Result<(), String> = async {
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
        Ok(())
    }
    .await;

    if let Err(e) = migrate_result {
        // 迁移失败：保留旧库原样（不删），下次启动重试。
        // 不 return Err--main.rs 用 expect 初始化 pool，return Err 会 panic 阻塞启动。
        // 旧库保留 = 数据不丢（用户可手动恢复）；新库缺部分表数据但应用仍可用。
        // P2.7: 写 migration_failed 标记到配置库，设置页存储面板显示警告（避免静默丢数据）。
        tracing::error!(error = %e, "迁移中途失败，旧 blink.db 保留，下次启动重试");
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let reason = format!("migration_failed: {e}");
        let _ = sqlx::query(
            "INSERT OR REPLACE INTO config (key, value, updated_at) VALUES ('migration_failed', ?1, ?2)",
        )
        .bind(&reason)
        .bind(now)
        .execute(&config)
        .await;
        config.close().await;
        history.close().await;
        ai.close().await;
        cache.close().await;
        return Ok(());
    }

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
    // P2.7: 清除可能残留的 migration_failed 标记（前次失败本次成功）
    let _ = sqlx::query("DELETE FROM config WHERE key = 'migration_failed'")
        .execute(&config)
        .await;

    config.close().await;
    history.close().await;
    ai.close().await;
    cache.close().await;

    // 删除旧库
    match std::fs::remove_file(&legacy_path) {
        Ok(()) => tracing::info!("旧 blink.db 已删除，迁移完成"),
        Err(e) => {
            tracing::warn!(error = %e, "删除旧 blink.db 失败（不影响运行，下次启动会跳过迁移）")
        }
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
async fn migrate_via_attach(dst: &SqlitePool, legacy_url: &str, table: &str) -> Result<(), String> {
    // ATTACH 旧库（legacy_url 是内部文件路径，非用户输入，安全）
    sqlx::query(sqlx::AssertSqlSafe(format!(
        "ATTACH DATABASE '{legacy_url}' AS legacy"
    )))
    .execute(dst)
    .await
    .map_err(|e| format!("ATTACH 旧库失败: {e}"))?;

    // 迁移逻辑包在块内，确保无论成功失败都 DETACH 旧库（避免连接残留）
    let result: Result<(), String> = async {
        // 检查旧库是否有此表
        let exists: (i64,) = sqlx::query_as(
            "SELECT COUNT(*) FROM legacy.sqlite_master WHERE type='table' AND name=?1",
        )
        .bind(table)
        .fetch_one(dst)
        .await
        .map_err(|e| format!("检查 legacy.{table} 存在性: {e}"))?;

        if exists.0 > 0 {
            // 迁移数据（INSERT OR REPLACE 防主键冲突；table 名由调用方硬编码字面量传入）
            let r = sqlx::query(sqlx::AssertSqlSafe(format!(
                "INSERT OR REPLACE INTO main.{table} SELECT * FROM legacy.{table}"
            )))
            .execute(dst)
            .await
            .map_err(|e| format!("迁移表 {table} 失败（schema 差异?）: {e}"))?;
            tracing::info!(table, rows = r.rows_affected(), "表迁移完成");
        } else {
            tracing::debug!(table, "旧库无此表，跳过");
        }
        Ok(())
    }
    .await;

    // DETACH 旧库（无论迁移成功失败都执行）
    let _ = sqlx::query("DETACH DATABASE legacy").execute(dst).await;

    result
}
