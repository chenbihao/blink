//! 剪贴板图片历史（0.16.4）：SQLite 持久化，放 cache 库（`blink_cache.db`）。
//!
//! **设计**（见 phases/0.16-clipboard-polish.md §5.5）：
//! - 独立建表，不进 `clipboard_history`（history 库），避免大 BLOB 污染文本历史
//! - 存完整 PNG + 缩略图 PNG + sha256 + 元数据
//! - 内容去重"删旧留新"：入库前按 sha256 DELETE 旧记录 + INSERT 新记录
//! - 上限 max_image_items=200（独立配置），超量清理保留最新

use sqlx::SqlitePool;
use std::sync::OnceLock;

/// 全局 cache pool（供 blink-clipimg 协议懒加载缩略图用，与 icon 模块同模式）。
static POOL: OnceLock<SqlitePool> = OnceLock::new();

/// 注册全局 cache pool（main.rs setup 阶段调用）。
pub fn set_pool(pool: SqlitePool) {
    let _ = POOL.set(pool);
}

/// 从全局 pool 查询缩略图（blink-clipimg 协议用）。
pub async fn get_thumb_by_id_global(id: &str) -> Option<Vec<u8>> {
    let pool = POOL.get()?;
    get_thumb_by_id(pool, id).await
}

/// 剪贴板图片条目。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClipboardImage {
    pub id: String,
    /// 完整 PNG 字节（写回系统剪贴板用）。
    pub png_blob: Vec<u8>,
    /// 缩略图 PNG 字节（max 边 256px，列表展示用）。
    pub thumb_blob: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 内容去重用哈希。
    pub sha256: String,
    pub created_at: i64,
    pub source_app: Option<String>,
}

/// 默认图片上限。
#[allow(dead_code)]
pub const DEFAULT_MAX_IMAGE_ITEMS: u32 = 200;

/// 初始化 clipboard_images 表（放 cache 库）。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS clipboard_images (
            id TEXT PRIMARY KEY,
            png_blob BLOB NOT NULL,
            thumb_blob BLOB NOT NULL,
            width INTEGER NOT NULL,
            height INTEGER NOT NULL,
            sha256 TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            source_app TEXT
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_img_created ON clipboard_images(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_img_sha256 ON clipboard_images(sha256)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    tracing::debug!("clipboard_images 表已初始化");
    Ok(())
}

/// 保存剪贴板图片条目。
///
/// **去重语义**（0.16.4）：入库前按 sha256 删除旧记录（删旧留新），
/// 再 INSERT 新记录。保证跨时段重复复制同一图片只保留最新一条。
pub async fn save_image(pool: &SqlitePool, item: &ClipboardImage) -> Result<(), String> {
    // P1-#15 fix: 删旧留新包在事务里，防止 DELETE 成功 INSERT 失败导致数据丢失
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // 删旧留新：先按 sha256 删除同内容旧记录
    sqlx::query("DELETE FROM clipboard_images WHERE sha256 = ?1")
        .bind(&item.sha256)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT OR REPLACE INTO clipboard_images
         (id, png_blob, thumb_blob, width, height, sha256, created_at, source_app)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )
    .bind(&item.id)
    .bind(&item.png_blob)
    .bind(&item.thumb_blob)
    .bind(item.width as i64)
    .bind(item.height as i64)
    .bind(&item.sha256)
    .bind(item.created_at)
    .bind(&item.source_app)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// 查询最近的剪贴板图片条目（不含 BLOB，只元数据 + 缩略图）。
pub async fn query_recent_images(pool: &SqlitePool, limit: i64) -> Vec<ClipboardImageMeta> {
    sqlx::query_as::<_, ClipboardImageMeta>(
        "SELECT id, thumb_blob, width, height, created_at, source_app
         FROM clipboard_images ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 查询近 N 天的剪贴板图片（按时间倒序，最多 limit 条）。
#[allow(dead_code)]
pub async fn query_recent_days_images(
    pool: &SqlitePool,
    days: u32,
    limit: i64,
) -> Vec<ClipboardImageMeta> {
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    sqlx::query_as::<_, ClipboardImageMeta>(
        "SELECT id, thumb_blob, width, height, created_at, source_app
         FROM clipboard_images WHERE created_at > ?1 ORDER BY created_at DESC LIMIT ?2",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 图片元数据（不含完整 PNG，用于列表展示）。
#[derive(Debug, Clone, sqlx::FromRow)]
#[allow(dead_code)]
pub struct ClipboardImageMeta {
    pub id: String,
    pub thumb_blob: Vec<u8>,
    pub width: i64,
    pub height: i64,
    pub created_at: i64,
    pub source_app: Option<String>,
}

/// 按 id 查询完整 PNG（写回剪贴板 / pin 用）。
pub async fn get_png_by_id(pool: &SqlitePool, id: &str) -> Option<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>("SELECT png_blob FROM clipboard_images WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// 按 id 查询缩略图 PNG（前端展示用）。
pub async fn get_thumb_by_id(pool: &SqlitePool, id: &str) -> Option<Vec<u8>> {
    sqlx::query_scalar::<_, Vec<u8>>("SELECT thumb_blob FROM clipboard_images WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// 删除指定图片条目。
pub async fn delete_image(pool: &SqlitePool, id: &str) {
    let _ = sqlx::query("DELETE FROM clipboard_images WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await;
}

/// 清空所有剪贴板图片历史。
pub async fn clear_all_images(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM clipboard_images")
        .execute(pool)
        .await;
}

/// 清理过期图片（按天）。
pub async fn cleanup_old_images(pool: &SqlitePool, days: u32) {
    if days == 0 {
        return;
    }
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    let result = sqlx::query("DELETE FROM clipboard_images WHERE created_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await;
    if let Ok(r) = result {
        let rows = r.rows_affected();
        if rows > 0 {
            tracing::info!(rows, "清理过期剪贴板图片");
        }
    }
}

/// 超量清理：保留最新的 max_items 条。
pub async fn cleanup_excess_images(pool: &SqlitePool, max_items: u32) {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM clipboard_images")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));

    if count.0 > max_items as i64 {
        let excess = count.0 - max_items as i64;
        let _ = sqlx::query(
            "DELETE FROM clipboard_images WHERE id IN (
                SELECT id FROM clipboard_images ORDER BY created_at ASC LIMIT ?1
            )",
        )
        .bind(excess)
        .execute(pool)
        .await;
    }
}

/// 生成唯一 ID。
pub fn generate_image_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("clipimg_{}", timestamp)
}

/// 获取剪贴板图片统计信息。
///
/// 返回图片数量和总 BLOB 大小（png_blob + thumb_blob 字节数）。
pub async fn get_image_stats(pool: &SqlitePool) -> serde_json::Value {
    let row: (i64, Option<i64>) = sqlx::query_as(
        "SELECT COUNT(*), COALESCE(SUM(LENGTH(png_blob) + LENGTH(thumb_blob)), 0) FROM clipboard_images",
    )
    .fetch_one(pool)
    .await
    .unwrap_or((0, Some(0)));

    serde_json::json!({
        "image_count": row.0,
        "total_size_bytes": row.1.unwrap_or(0),
    })
}

/// 获取剪贴板图片数量。
pub async fn count(pool: &SqlitePool) -> i64 {
    let row: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM clipboard_images")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));
    row.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn save_and_query_image() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();

        let item = ClipboardImage {
            id: generate_image_id(),
            png_blob: vec![1, 2, 3],
            thumb_blob: vec![4, 5, 6],
            width: 100,
            height: 200,
            sha256: "abc123".to_string(),
            created_at: 1000,
            source_app: Some("TestApp".to_string()),
        };
        save_image(&pool, &item).await.unwrap();

        let items = query_recent_images(&pool, 10).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].width, 100);
        assert_eq!(items[0].height, 200);
        assert_eq!(items[0].source_app.as_deref(), Some("TestApp"));

        let png = get_png_by_id(&pool, &item.id).await;
        assert_eq!(png, Some(vec![1, 2, 3]));

        let thumb = get_thumb_by_id(&pool, &item.id).await;
        assert_eq!(thumb, Some(vec![4, 5, 6]));
    }

    #[tokio::test]
    async fn dedup_deletes_old_by_sha256() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();

        // 插入第一条
        let item1 = ClipboardImage {
            id: generate_image_id(),
            png_blob: vec![1],
            thumb_blob: vec![2],
            width: 10,
            height: 10,
            sha256: "same_hash".to_string(),
            created_at: 1000,
            source_app: None,
        };
        save_image(&pool, &item1).await.unwrap();

        // 插入相同 sha256 的第二条（应删旧留新）
        let item2 = ClipboardImage {
            id: generate_image_id(),
            png_blob: vec![3],
            thumb_blob: vec![4],
            width: 20,
            height: 20,
            sha256: "same_hash".to_string(),
            created_at: 2000,
            source_app: Some("New".to_string()),
        };
        save_image(&pool, &item2).await.unwrap();

        let items = query_recent_images(&pool, 10).await;
        assert_eq!(items.len(), 1, "同 sha256 应只保留最新一条");
        assert_eq!(items[0].width, 20, "应保留新记录");
        assert_eq!(items[0].created_at, 2000);
    }

    #[tokio::test]
    async fn test_cleanup_excess_images() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();

        for i in 0..5 {
            let item = ClipboardImage {
                id: format!("id_{i}"),
                png_blob: vec![i as u8],
                thumb_blob: vec![i as u8],
                width: i as u32,
                height: i as u32,
                sha256: format!("hash_{i}"),
                created_at: i as i64,
                source_app: None,
            };
            save_image(&pool, &item).await.unwrap();
        }

        cleanup_excess_images(&pool, 3).await;

        let items = query_recent_images(&pool, 10).await;
        assert_eq!(items.len(), 3, "超量清理后应只剩 3 条");
        // 保留最新的（created_at 最大的）
        assert_eq!(items[0].created_at, 4);
        assert_eq!(items[2].created_at, 2);
    }
}
