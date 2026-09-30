//! 图标缓存 DB 层（0.12.0 §2.2.3 分层修复）——从 `domain/search/icon.rs` 迁出。
//!
//! `icon_cache` 表的 schema + CRUD 在此文件，领域层不再直持 DB。
//! `domain/search/icon.rs` 调本模块的 `init` / `load` / `save` 进行 DB 操作。

use std::time::SystemTime;

use once_cell::sync::OnceCell;
use sqlx::SqlitePool;

/// 全局 pool 单例——用于 SQLite 持久化。
static POOL: OnceCell<SqlitePool> = OnceCell::new();

/// 默认提取尺寸（物理像素）。32 足够列表项显示，高 DPI 下 GetImage 会按需给更大位图。
const ICON_SIZE: i32 = 32;

/// 初始化 schema：建表 + 索引（纯建表，不注册全局 pool、不 spawn 清理）。
///
/// 供 DB 迁移路径用--迁移 pool 是临时的（迁移后即 close），不应占用全局 `OnceCell`，
/// 否则 `init_all` 的最终 pool 注册会被 `OnceCell` 拒绝（已占用），导致 `perf`/`icon_cache`
/// 写入静默失效（落到已 close 的迁移 pool 上）。
pub async fn init_schema(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS icon_cache (
            path_hash TEXT PRIMARY KEY,
            png_blob BLOB NOT NULL,
            file_mtime INTEGER NOT NULL,
            icon_size INTEGER NOT NULL,
            accessed_at INTEGER NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_icon_accessed ON icon_cache(accessed_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 初始化图标缓存：建表 + 注册全局 pool + 后台清理。
pub async fn init(pool: &SqlitePool) -> Result<(), String> {
    init_schema(pool).await?;

    let _ = POOL.set(pool.clone());

    // 后台清理过期数据
    let cleanup_pool = pool.clone();
    tokio::spawn(async move {
        cleanup_old(&cleanup_pool).await;
    });

    tracing::debug!("icon_cache 表已初始化");
    Ok(())
}

/// 从 SQLite 读取缓存的图标。
///
/// 返回 `None` = 未命中（DB 无记录或 POOL 未初始化）；
/// `Some(None)` = 命中"提取过但无图标"的负缓存；
/// `Some(Some(bytes))` = 命中有效缓存。
pub fn load(path: &str) -> Option<Option<Vec<u8>>> {
    let pool = POOL.get()?;
    let hash = path_hash(path);
    let mtime = file_mtime(path);

    let result = tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            sqlx::query_as::<_, (Vec<u8>, i64)>(
                "SELECT png_blob, file_mtime FROM icon_cache WHERE path_hash = ?1",
            )
            .bind(&hash)
            .fetch_optional(pool)
            .await
        })
    });

    match result {
        Ok(Some((blob, cached_mtime))) => {
            if cached_mtime != mtime {
                tracing::trace!(path, "图标缓存失效(mtime 变化)");
                let pool = pool.clone();
                let hash = hash.clone();
                tokio::spawn(async move {
                    let _ = sqlx::query("DELETE FROM icon_cache WHERE path_hash = ?1")
                        .bind(&hash)
                        .execute(&pool)
                        .await;
                });
                return None;
            }
            let pool = pool.clone();
            tokio::spawn(async move {
                let now = chrono::Utc::now().timestamp();
                let _ = sqlx::query("UPDATE icon_cache SET accessed_at = ?1 WHERE path_hash = ?2")
                    .bind(now)
                    .bind(&hash)
                    .execute(&pool)
                    .await;
            });
            Some(Some(blob))
        }
        Ok(None) => None,
        Err(e) => {
            tracing::debug!(error = %e, "读取图标缓存失败");
            None
        }
    }
}

/// 保存图标到 SQLite。
pub fn save(path: &str, png: &[u8]) {
    let Some(pool) = POOL.get() else {
        return;
    };
    let hash = path_hash(path);
    let mtime = file_mtime(path);
    let now = chrono::Utc::now().timestamp();
    let pool = pool.clone();
    let png = png.to_vec();

    tokio::spawn(async move {
        let _ = sqlx::query(
            "INSERT OR REPLACE INTO icon_cache (path_hash, png_blob, file_mtime, icon_size, accessed_at, created_at) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        )
        .bind(&hash)
        .bind(&png)
        .bind(mtime)
        .bind(ICON_SIZE)
        .bind(now)
        .bind(now)
        .execute(&pool)
        .await;
    });
}

/// 清理超过 30 天未访问的图标缓存。
pub async fn cleanup_old(pool: &SqlitePool) {
    let cutoff = chrono::Utc::now().timestamp() - 30 * 86400;
    match sqlx::query("DELETE FROM icon_cache WHERE accessed_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await
    {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, "清理过期图标缓存");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期图标缓存失败"),
    }

    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM icon_cache")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));

    if count.0 > 2000 {
        let excess = count.0 - 2000;
        let _ = sqlx::query(
            "DELETE FROM icon_cache WHERE path_hash IN (SELECT path_hash FROM icon_cache ORDER BY accessed_at ASC LIMIT ?1)",
        )
        .bind(excess)
        .execute(pool)
        .await;
        tracing::info!(deleted = excess, "清理超量图标缓存");
    }
}

/// 图标缓存总行数（设置页存储统计用）。
pub async fn count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM icon_cache")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));
    row.0
}

/// 清空全部图标缓存。
pub async fn clear_all(pool: &SqlitePool) {
    if let Err(e) = sqlx::query("DELETE FROM icon_cache").execute(pool).await {
        tracing::warn!(error = %e, "清空 icon_cache 失败");
    }
}

// ── 内部辅助函数 ──────────────────────────────────────────────────────────────

/// 计算路径的哈希（用于 SQLite key，避免长路径问题）。
fn path_hash(path: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    path.hash(&mut hasher);
    format!("{:016x}", hasher.finish())
}

/// 获取文件 mtime（秒级时间戳）。
fn file_mtime(path: &str) -> i64 {
    std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
