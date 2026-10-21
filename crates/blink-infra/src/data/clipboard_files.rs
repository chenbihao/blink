//! 剪贴板文件路径历史（0.26.9）：SQLite 持久化，放 history 库
//! （`blink_history.db`，与 `clipboard_history` 同库——纯文本路径，无 BLOB）。
//!
//! **设计**（0.26.9 §5.10）：
//! - 路径是历史条目，文件内容只在悬浮预览或显式入库时读取；本表不存内容
//! - 去重键 = 排序后路径列表的无歧义序列化哈希；哈希命中后核对列表本体，
//!   同组合重复制删旧留新（更新时间/来源/顺序），不堆新行
//! - 存储保留原始顺序；排序仅用于比较
//! - 容量淘汰：条数（`max_file_items`）与序列化载荷合计（32MiB）双上限，
//!   保存事务外按最旧顺序淘汰；时间淘汰复用 `retention_days`（0=不按时间）

use sqlx::SqlitePool;

use crate::platform::clipboard::dropfiles;

mod preview;
pub use preview::{
    DecodedFileImage, FileImagePreview, FilePreviewError, encode_full_png, encode_thumbnail_png,
    get_file_preview_by_id, get_file_size_by_id, make_thumbnail_from_png_bytes, probe_file_entry_status,
    read_file_image_by_id,
};

/// 默认文件条目上限。
pub const DEFAULT_MAX_FILE_ITEMS: u32 = 1000;
/// 条数合法范围下限。
pub const MAX_FILE_ITEMS_MIN: u32 = 1;
/// 条数合法范围上限。
pub const MAX_FILE_ITEMS_MAX: u32 = 10_000;

/// 剪贴板文件条目（写入口径：原始有序路径列表）。
#[derive(Debug, Clone)]
pub struct ClipboardFileItem {
    pub id: String,
    /// 原始采集顺序的完整路径列表。
    pub paths: Vec<String>,
    pub created_at: i64,
    pub source_app: Option<String>,
}

/// 文件条目列表元数据（不含 paths_json，浏览/分页用）。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClipboardFileMeta {
    pub id: String,
    pub file_count: i64,
    pub first_name: String,
    pub created_at: i64,
    pub source_app: Option<String>,
    /// 0.26.16：置顶到期时刻（Unix 秒，0 或已过期 = 未置顶）。
    pub pinned_until: i64,
}

/// 文件条目搜索行（含 paths_json，供引擎对各路径文件名做模糊匹配）。
///
/// 时间窗口有界（`LIMIT candidate_limit`），引擎逐条解析即弃，不用 LIKE 排除模糊命中。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClipboardFileSearchRow {
    pub id: String,
    pub file_count: i64,
    pub first_name: String,
    pub created_at: i64,
    pub source_app: Option<String>,
    pub paths_json: String,
    /// 0.26.16：置顶到期时刻（Unix 秒，0 或已过期 = 未置顶）。
    pub pinned_until: i64,
}

/// 初始化 clipboard_files 表（history 库）。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS clipboard_files (
            id TEXT PRIMARY KEY,
            paths_json TEXT NOT NULL,
            dedup_hash TEXT NOT NULL,
            file_count INTEGER NOT NULL,
            first_name TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            source_app TEXT,
            payload_bytes INTEGER NOT NULL,
            pinned_until INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 0.26.16：旧表迁移——加 pinned_until 列（duplicate column 静默跳过，幂等）。
    let migrate_result = sqlx::query(
        "ALTER TABLE clipboard_files ADD COLUMN pinned_until INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await;
    if let Err(e) = migrate_result {
        let msg = e.to_string();
        if !msg.contains("duplicate column") {
            tracing::warn!(error = %msg, "clipboard_files 加 pinned_until 列失败");
        }
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_files_created ON clipboard_files(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_files_hash ON clipboard_files(dedup_hash)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 覆盖索引：query_recent_file_list(_after) 只 SELECT
    // id, file_count, first_name, created_at, source_app——不碰 paths_json 的
    // overflow page（与 clipboard_history / clipboard_images 同范式）。
    // 0.26.16：v2 纳入 pinned_until（置顶排序表达式读它），旧 v1 索引随即删除。
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_clip_files_meta_covering_v2 \
         ON clipboard_files(created_at DESC, id, file_count, first_name, source_app, pinned_until)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    let _ = sqlx::query("DROP INDEX IF EXISTS idx_clip_files_meta_covering")
        .execute(pool)
        .await;

    tracing::debug!("clipboard_files 表已初始化");
    Ok(())
}

/// 保存文件条目：同组合（哈希 + 列表核对双确认）删旧留新。
///
/// 哈希碰撞（键同列表不同）不覆盖，作为独立条目共存。
/// 0.26.16：删旧留新时继承置顶状态（同文本/图片表语义）。
pub async fn save_file(pool: &SqlitePool, item: &ClipboardFileItem) -> Result<(), String> {
    let paths_json = dropfiles::serialize_paths(&item.paths)?;
    let dedup_hash = dropfiles::dedup_key(&item.paths);
    let first_name = item
        .paths
        .first()
        .and_then(|p| dropfiles::first_file_name(p))
        .unwrap_or_default()
        .to_string();

    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // 哈希命中后核对列表本体：排序比较，避免仅凭哈希覆盖（碰撞防御）
    let existing = sqlx::query_as::<_, (String, String, i64)>(
        "SELECT id, paths_json, pinned_until FROM clipboard_files WHERE dedup_hash = ?1",
    )
    .bind(&dedup_hash)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    let mut inherited_pinned_until = 0i64;
    if let Some((old_id, old_json, old_pinned_until)) = existing {
        let same_set = serde_json::from_str::<Vec<String>>(&old_json)
            .map(|old_paths| {
                let mut a = old_paths;
                let mut b = item.paths.clone();
                a.sort();
                b.sort();
                a == b
            })
            .unwrap_or(false);
        if same_set {
            inherited_pinned_until = old_pinned_until.max(0);
            sqlx::query("DELETE FROM clipboard_files WHERE id = ?1")
                .bind(&old_id)
                .execute(&mut *tx)
                .await
                .map_err(|e| e.to_string())?;
        }
    }

    sqlx::query(
        "INSERT OR REPLACE INTO clipboard_files
         (id, paths_json, dedup_hash, file_count, first_name, created_at, source_app, payload_bytes, pinned_until)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
    )
    .bind(&item.id)
    .bind(&paths_json)
    .bind(&dedup_hash)
    .bind(item.paths.len() as i64)
    .bind(&first_name)
    .bind(item.created_at)
    .bind(&item.source_app)
    .bind(paths_json.len() as i64)
    .bind(inherited_pinned_until)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

const META_COLUMNS: &str = "id, file_count, first_name, created_at, source_app, pinned_until";

/// 查询最近的文件条目元数据（keyset 排序与文本/图片表同构）。
/// 0.26.16：排序键头部加置顶位（未过期置顶条目排普通条目之前）。
pub async fn query_recent_file_list(pool: &SqlitePool, limit: i64) -> Vec<ClipboardFileMeta> {
    let now = chrono::Utc::now().timestamp();
    let sql = format!(
        "SELECT {META_COLUMNS} FROM clipboard_files \
         ORDER BY (pinned_until > ?2) DESC, created_at DESC, id DESC LIMIT ?1"
    );
    sqlx::query_as::<_, ClipboardFileMeta>(sqlx::AssertSqlSafe(sql))
        .bind(limit)
        .bind(now)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
}

/// keyset 游标分页：严格早于 `cursor` 的文件条目元数据。
/// 0.26.16：游标条件为含置顶位的三元组比较（与文本/图片表同构）。
pub async fn query_recent_file_list_after(
    pool: &SqlitePool,
    cursor: &super::clipboard::ClipboardCursor,
    limit: i64,
) -> Vec<ClipboardFileMeta> {
    let now = chrono::Utc::now().timestamp();
    let cursor_pinned = i64::from(cursor.pinned);
    let sql = format!(
        "SELECT {META_COLUMNS} FROM clipboard_files \
         WHERE ((pinned_until > ?3) < ?4 \
                OR ((pinned_until > ?3) = ?4 \
                    AND (created_at < ?1 OR (created_at = ?1 AND id < ?2)))) \
         ORDER BY (pinned_until > ?3) DESC, created_at DESC, id DESC LIMIT ?5"
    );
    sqlx::query_as::<_, ClipboardFileMeta>(sqlx::AssertSqlSafe(sql))
        .bind(cursor.created_at)
        .bind(&cursor.id)
        .bind(now)
        .bind(cursor_pinned)
        .bind(limit)
        .fetch_all(pool)
        .await
        .unwrap_or_default()
}

/// 有界文件搜索候选；SQL 仅约束保留期/游标，模糊匹配交给引擎。
/// 0.26.16：游标条件含置顶位；无游标（?6 IS NULL）从头扫描。
pub async fn query_file_search_candidates(
    pool: &SqlitePool,
    cursor: Option<&super::clipboard::ClipboardCursor>,
    limit: i64,
    retention_days: u32,
) -> Vec<ClipboardFileSearchRow> {
    let now = chrono::Utc::now().timestamp();
    let cutoff = (retention_days > 0)
        .then(|| now - i64::from(retention_days) * 86400);
    sqlx::query_as::<_, ClipboardFileSearchRow>(
        "SELECT id, file_count, first_name, created_at, source_app, paths_json, pinned_until
         FROM clipboard_files
         WHERE (?1 IS NULL OR created_at < ?1 OR (created_at = ?1 AND id < ?2))
           AND (?3 IS NULL OR created_at >= ?3)
           AND (?6 IS NULL
                OR (pinned_until > ?5) < ?6
                OR ((pinned_until > ?5) = ?6
                    AND (created_at < ?1 OR (created_at = ?1 AND id < ?2))))
         ORDER BY (pinned_until > ?5) DESC, created_at DESC, id DESC LIMIT ?4",
    )
    .bind(cursor.map(|c| c.created_at))
    .bind(cursor.map(|c| c.id.as_str()))
    .bind(cutoff)
    .bind(limit)
    .bind(now)
    .bind(cursor.map(|c| i64::from(c.pinned)))
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 按 id 查完整路径列表（写回/预览/入库入口；不存在返回 `None`）。
pub async fn get_paths_by_id(pool: &SqlitePool, id: &str) -> Option<ClipboardFileItem> {
    let row = sqlx::query_as::<_, (String, i64, Option<String>)>(
        "SELECT paths_json, created_at, source_app FROM clipboard_files WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()?;
    let (paths_json, created_at, source_app) = row;
    let paths: Vec<String> = serde_json::from_str(&paths_json).ok()?;
    Some(ClipboardFileItem {
        id: id.to_string(),
        paths,
        created_at,
        source_app,
    })
}

/// 按 id 查序列化载荷字节（存储统计校验等轻量场景）。
pub async fn get_payload_bytes_by_id(pool: &SqlitePool, id: &str) -> Option<i64> {
    sqlx::query_scalar::<_, i64>("SELECT payload_bytes FROM clipboard_files WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// 批量删除文件条目；每库单事务，不存在的 ID 幂等成功。
pub async fn delete_files_by_ids(pool: &SqlitePool, ids: &[&str]) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for id in ids {
        sqlx::query("DELETE FROM clipboard_files WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// 清空所有文件路径历史（不触碰磁盘上的原文件）。
pub async fn clear_all_files(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM clipboard_files")
        .execute(pool)
        .await;
}

/// 按天清理过期文件条目（0=不按时间清理，直接返回）。
/// 0.26.16：未过期置顶条目豁免。
pub async fn cleanup_old_files(pool: &SqlitePool, days: u32) {
    if days == 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - (days as i64 * 86400);
    let result =
        sqlx::query("DELETE FROM clipboard_files WHERE created_at < ?1 AND pinned_until <= ?2")
            .bind(cutoff)
            .bind(now)
            .execute(pool)
            .await;
    if let Ok(r) = result {
        let rows = r.rows_affected();
        if rows > 0 {
            tracing::info!(rows, "清理过期剪贴板文件条目");
        }
    }
}

/// 容量淘汰：条数与序列化载荷合计双上限，按最旧顺序淘汰。
///
/// 条数超限一次删足超额数；载荷超限从最旧起累积删除直至达标。
/// 与保存同调用序（保存后清理），不进保存事务。
/// 0.26.16：未过期置顶条目豁免淘汰——不参与条数基数，也不进载荷 victim；
/// 置顶期间允许总量暂时超出（到期后自动回归）。
pub async fn cleanup_excess_files(pool: &SqlitePool, max_items: u32) {
    let now = chrono::Utc::now().timestamp();
    let count: i64 = sqlx::query_as(
        "SELECT COUNT(*) FROM clipboard_files WHERE pinned_until <= ?1",
    )
    .bind(now)
    .fetch_one(pool)
    .await
    .map(|(c,): (i64,)| c)
    .unwrap_or(0);
    if count > max_items as i64 {
        let excess = count - max_items as i64;
        let _ = sqlx::query(
            "DELETE FROM clipboard_files WHERE id IN (
                SELECT id FROM clipboard_files WHERE pinned_until <= ?2
                ORDER BY created_at ASC, id ASC LIMIT ?1
            )",
        )
        .bind(excess)
        .bind(now)
        .execute(pool)
        .await;
        tracing::debug!(removed = excess, "文件条目超条数上限,淘汰最旧");
    }

    let total: i64 = sqlx::query_as("SELECT COALESCE(SUM(payload_bytes), 0) FROM clipboard_files")
        .fetch_one(pool)
        .await
        .map(|(t,): (i64,)| t)
        .unwrap_or(0);
    if total <= dropfiles::MAX_TOTAL_PAYLOAD_BYTES {
        return;
    }
    // 从最旧起累积删除，直到总载荷回到上限内（置顶条目不进 victim）
    let rows: Vec<(String, i64)> = sqlx::query_as(
        "SELECT id, payload_bytes FROM clipboard_files \
         WHERE pinned_until <= ?1 ORDER BY created_at ASC, id ASC",
    )
    .bind(now)
    .fetch_all(pool)
    .await
    .unwrap_or_default();
    let mut acc = 0i64;
    let mut victims: Vec<String> = Vec::new();
    for (id, payload) in rows {
        if total - acc <= dropfiles::MAX_TOTAL_PAYLOAD_BYTES {
            break;
        }
        acc += payload;
        victims.push(id);
    }
    let removed = victims.len();
    for chunk in victims.chunks(500) {
        let placeholders = chunk
            .iter()
            .enumerate()
            .map(|(i, _)| format!("?{}", i + 1))
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!("DELETE FROM clipboard_files WHERE id IN ({placeholders})");
        let mut q = sqlx::query(sqlx::AssertSqlSafe(sql));
        for id in chunk {
            q = q.bind(id);
        }
        let _ = q.execute(pool).await;
    }
    if removed > 0 {
        tracing::debug!(removed, freed_bytes = acc, "文件条目超载荷上限,淘汰最旧");
    }
}

/// 0.26.16：设置/取消置顶。`until` 为到期 Unix 秒（0 = 取消置顶）。
/// 返回是否有行被更新（false = 条目不存在）。再次置顶即重置到期时间。
/// 置顶同时刷新 `created_at`（后置顶排置顶组最前 + 延长保留期起点，
/// 与 promote_image 同模式）；取消置顶不动时间。
pub async fn set_file_pinned_until(pool: &SqlitePool, id: &str, until: i64) -> Result<bool, String> {
    let now = chrono::Utc::now().timestamp();
    let result = if until > 0 {
        sqlx::query("UPDATE clipboard_files SET pinned_until = ?2, created_at = ?3 WHERE id = ?1")
            .bind(id)
            .bind(until)
            .bind(now)
            .execute(pool)
            .await
    } else {
        sqlx::query("UPDATE clipboard_files SET pinned_until = ?2 WHERE id = ?1")
            .bind(id)
            .bind(until)
            .execute(pool)
            .await
    }
    .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 生成唯一 ID。
pub fn generate_file_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("clipfile_{timestamp}")
}

/// 文件条目统计：条数与序列化载荷字节数。
pub async fn get_file_stats(pool: &SqlitePool) -> serde_json::Value {
    let row: (i64, i64) =
        sqlx::query_as("SELECT COUNT(*), COALESCE(SUM(payload_bytes), 0) FROM clipboard_files")
            .fetch_one(pool)
            .await
            .unwrap_or((0, 0));
    serde_json::json!({
        "file_count": row.0,
        "file_payload_bytes": row.1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(id: &str, paths: &[&str], created_at: i64, source: Option<&str>) -> ClipboardFileItem {
        ClipboardFileItem {
            id: id.to_string(),
            paths: paths.iter().map(|s| s.to_string()).collect(),
            created_at,
            source_app: source.map(str::to_string),
        }
    }

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();
        pool
    }

    #[tokio::test]
    async fn save_and_query_roundtrip() {
        let pool = pool().await;
        save_file(
            &pool,
            &item(
                "f1",
                &[r"C:\a.txt", r"C:\b.txt"],
                1000,
                Some("explorer.exe"),
            ),
        )
        .await
        .unwrap();
        let metas = query_recent_file_list(&pool, 10).await;
        assert_eq!(metas.len(), 1);
        assert_eq!(metas[0].file_count, 2);
        assert_eq!(metas[0].first_name, "a.txt");
        assert_eq!(metas[0].source_app.as_deref(), Some("explorer.exe"));

        let full = get_paths_by_id(&pool, "f1").await.unwrap();
        assert_eq!(full.paths, vec![r"C:\a.txt", r"C:\b.txt"], "保留原始顺序");
        assert!(get_paths_by_id(&pool, "missing").await.is_none());
    }

    #[tokio::test]
    async fn same_set_repost_updates_in_place_not_stacking() {
        let pool = pool().await;
        save_file(
            &pool,
            &item("f1", &[r"C:\a.txt", r"C:\b.txt"], 1000, Some("old")),
        )
        .await
        .unwrap();
        // 乱序同组合重复制：删旧留新，顺序取最近一次
        save_file(
            &pool,
            &item("f2", &[r"C:\b.txt", r"C:\a.txt"], 2000, Some("new")),
        )
        .await
        .unwrap();
        let metas = query_recent_file_list(&pool, 10).await;
        assert_eq!(metas.len(), 1, "同组合不堆新行");
        assert_eq!(metas[0].id, "f2");
        assert_eq!(metas[0].created_at, 2000);
        assert_eq!(metas[0].source_app.as_deref(), Some("new"));
        let full = get_paths_by_id(&pool, "f2").await.unwrap();
        assert_eq!(full.paths, vec![r"C:\b.txt", r"C:\a.txt"], "存储最近顺序");
    }

    #[tokio::test]
    async fn hash_collision_keeps_both_entries() {
        let pool = pool().await;
        // 手工构造同 dedup_hash 不同列表的碰撞行（绕过 save_file 的核对删除）
        sqlx::query(
            "INSERT INTO clipboard_files (id, paths_json, dedup_hash, file_count, first_name, created_at, source_app, payload_bytes)
             VALUES ('c1', ?1, 'collide', 1, 'a', 1000, NULL, 10),
                    ('c2', ?2, 'collide', 1, 'b', 2000, NULL, 10)",
        )
        .bind(r#"["C:\\a.txt"]"#)
        .bind(r#"["C:\\b.txt"]"#)
        .execute(&pool)
        .await
        .unwrap();
        // save_file 哈希命中但列表不同 → 不覆盖，共存三行
        save_file(&pool, &item("c3", &[r"C:\c.txt"], 3000, None))
            .await
            .unwrap();
        let metas = query_recent_file_list(&pool, 10).await;
        assert_eq!(metas.len(), 3, "碰撞不凭哈希覆盖");
    }

    #[tokio::test]
    async fn keyset_pagination_no_gaps_or_dupes() {
        let pool = pool().await;
        for (id, at) in [("fa", 100), ("fb", 100), ("fc", 200)] {
            save_file(
                &pool,
                &item(id, &[format!(r"C:\{id}.txt").as_str()], at, None),
            )
            .await
            .unwrap();
        }
        let first = query_recent_file_list(&pool, 2).await;
        assert_eq!(first.len(), 2);
        let cursor = super::super::clipboard::ClipboardCursor {
            created_at: first[1].created_at,
            id: first[1].id.clone(), pinned: false,
        };
        let rest = query_recent_file_list_after(&pool, &cursor, 10).await;
        let ids: Vec<&str> = rest.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["fa"], "同秒 id 字符串序 tie-break");
        // 全量翻页覆盖
        let mut collected = Vec::new();
        let mut cur: Option<super::super::clipboard::ClipboardCursor> = None;
        loop {
            let batch = match &cur {
                Some(c) => query_recent_file_list_after(&pool, c, 2).await,
                None => query_recent_file_list(&pool, 2).await,
            };
            if batch.is_empty() {
                break;
            }
            let last = batch.last().unwrap();
            collected.extend(batch.iter().map(|m| m.id.clone()));
            cur = Some(super::super::clipboard::ClipboardCursor {
                created_at: last.created_at,
                id: last.id.clone(), pinned: false,
            });
        }
        assert_eq!(collected, vec!["fc", "fb", "fa"]);
    }

    #[tokio::test]
    async fn search_candidates_are_bounded_and_cursor_scoped() {
        let pool = pool().await;
        save_file(
            &pool,
            &item(
                "f1",
                &[r"C:\docs\report.pdf", r"C:\pics\photo.jpg"],
                1000,
                None,
            ),
        )
        .await
        .unwrap();
        save_file(&pool, &item("f2", &[r"C:\music\song.mp3"], 2000, None))
            .await
            .unwrap();
        let hits = query_file_search_candidates(&pool, None, 1, 0).await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, "f2");
        // 游标下推
        let cursor = super::super::clipboard::ClipboardCursor {
            created_at: hits[0].created_at,
            id: hits[0].id.clone(), pinned: false,
        };
        let rest = query_file_search_candidates(&pool, Some(&cursor), 10, 0).await;
        assert_eq!(rest.len(), 1);
        assert_eq!(rest[0].id, "f1");
        assert!(rest[0].paths_json.contains("photo"), "完整候选保留非首路径");
        assert!(
            query_file_search_candidates(&pool, None, 10, 1)
                .await
                .is_empty(),
            "保留期下推"
        );
    }

    #[tokio::test]
    async fn cleanup_excess_respects_count_and_payload_caps() {
        let pool = pool().await;
        for i in 0..5 {
            save_file(
                &pool,
                &item(
                    &format!("id{i}"),
                    &[format!(r"C:\f{i}.txt").as_str()],
                    i as i64,
                    None,
                ),
            )
            .await
            .unwrap();
        }
        cleanup_excess_files(&pool, 3).await;
        let metas = query_recent_file_list(&pool, 10).await;
        assert_eq!(metas.len(), 3);
        assert_eq!(metas[0].id, "id4", "保留最新");

        // 载荷上限：手工放大 payload_bytes 触发按载荷淘汰（删到 ≤32MiB）
        let _ = sqlx::query("UPDATE clipboard_files SET payload_bytes = 20000000")
            .execute(&pool)
            .await;
        cleanup_excess_files(&pool, 100).await;
        let stats = get_file_stats(&pool).await;
        assert!(
            stats["file_payload_bytes"].as_i64().unwrap() <= dropfiles::MAX_TOTAL_PAYLOAD_BYTES,
            "载荷淘汰至上限内: {}",
            stats["file_payload_bytes"]
        );
    }

    #[tokio::test]
    async fn cleanup_old_and_clear_all() {
        let pool = pool().await;
        save_file(&pool, &item("old", &[r"C:\old.txt"], 1000, None))
            .await
            .unwrap();
        save_file(&pool, &item("new", &[r"C:\new.txt"], 4_000_000_000, None))
            .await
            .unwrap();
        cleanup_old_files(&pool, 1).await;
        let metas = query_recent_file_list(&pool, 10).await;
        assert_eq!(metas.len(), 1, "按天清理旧条目");
        assert_eq!(metas[0].id, "new");

        clear_all_files(&pool).await;
        assert!(query_recent_file_list(&pool, 10).await.is_empty());
    }

    /// 0.26.16：置顶组排序 + 清理豁免 + set 闭环 + 重采集继承。
    #[tokio::test]
    async fn pin_orders_first_exempts_cleanup_and_inherits_on_repost() {
        let pool = pool().await;
        let now = chrono::Utc::now().timestamp();
        save_file(&pool, &item("old", &[r"C:\old.txt"], 100, None))
            .await
            .unwrap();
        save_file(&pool, &item("fresh", &[r"C:resh.txt"], now, None))
            .await
            .unwrap();
        let until = now + 6 * 3600;
        assert!(set_file_pinned_until(&pool, "old", until).await.unwrap());
        assert!(!set_file_pinned_until(&pool, "ghost", until).await.unwrap());
        let pinned_row = query_recent_file_list(&pool, 10)
            .await
            .into_iter()
            .find(|m| m.id == "old")
            .unwrap();
        assert!(pinned_row.created_at >= now, "置顶刷新 created_at");

        let ids: Vec<String> = query_recent_file_list(&pool, 10)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["old", "fresh"], "置顶组在前");

        cleanup_old_files(&pool, 1).await;
        cleanup_excess_files(&pool, 1).await;
        let ids: Vec<String> = query_recent_file_list(&pool, 10)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["old", "fresh"], "过期置顶文件豁免清理");

        // 同组合重采集（删旧留新）继承置顶
        save_file(&pool, &item("repost", &[r"C:\old.txt"], now + 10, None))
            .await
            .unwrap();
        let rows = query_recent_file_list(&pool, 10).await;
        assert_eq!(rows[0].id, "repost");
        assert_eq!(rows[0].pinned_until, until, "置顶状态跨重采集继承");
    }

    #[tokio::test]
    async fn delete_by_ids_idempotent() {
        let pool = pool().await;
        save_file(&pool, &item("f1", &[r"C:\a.txt"], 1000, None))
            .await
            .unwrap();
        delete_files_by_ids(&pool, &["f1", "missing"])
            .await
            .unwrap();
        assert!(query_recent_file_list(&pool, 10).await.is_empty());
    }
}
