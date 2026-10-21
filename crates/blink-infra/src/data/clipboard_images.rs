//! 剪贴板图片历史（0.16.4）：SQLite 持久化，放 cache 库（`blink_cache.db`）。
//!
//! **设计**（见 phases/0.16-clipboard-polish.md §5.5）：
//! - 独立建表，不进 `clipboard_history`（history 库），避免大 BLOB 污染文本历史
//! - 存完整 PNG + 缩略图 PNG + sha256 + 元数据
//! - 内容去重"删旧留新"：入库前按 sha256 DELETE 旧记录 + INSERT 新记录
//! - 上限 max_image_items=200（独立配置），超量清理保留最新

use sqlx::SqlitePool;
use std::sync::OnceLock;

mod preview;
pub use preview::get_preview_by_id_global;
pub(crate) use preview::try_acquire_image_decode_slot;

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
    /// 0.17.9：源文件名（从 CF_HDROP 提取，文件管理器复制图片文件时有值）。
    pub source_path: Option<String>,
    /// 0.26.16：置顶到期时刻（Unix 秒，0 = 未置顶）。`> now` 即置顶，惰性过期。
    pub pinned_until: i64,
}

/// 默认图片上限。
#[allow(dead_code)]
pub const DEFAULT_MAX_IMAGE_ITEMS: u32 = 1000;

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
            source_app TEXT,
            source_path TEXT,
            pinned_until INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 0.17.9：旧表迁移——已有 clipboard_images 表加 source_path 列
    // ALTER TABLE ADD COLUMN 是幂等的（IF NOT EXISTS 不支持，用 try-catch 模式）
    let migrate_result = sqlx::query("ALTER TABLE clipboard_images ADD COLUMN source_path TEXT")
        .execute(pool)
        .await;
    if let Err(e) = migrate_result {
        // "duplicate column name" = 已有此列，正常情况，静默跳过
        let msg = e.to_string();
        if !msg.contains("duplicate column") {
            tracing::warn!(error = %msg, "clipboard_images 加 source_path 列失败");
        }
    }

    // 0.26.16：同模式加 pinned_until 列
    let migrate_result = sqlx::query(
        "ALTER TABLE clipboard_images ADD COLUMN pinned_until INTEGER NOT NULL DEFAULT 0",
    )
    .execute(pool)
    .await;
    if let Err(e) = migrate_result {
        let msg = e.to_string();
        if !msg.contains("duplicate column") {
            tracing::warn!(error = %msg, "clipboard_images 加 pinned_until 列失败");
        }
    }

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_img_created ON clipboard_images(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_img_sha256 ON clipboard_images(sha256)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 覆盖索引：query_recent_image_list / search_image_list 只 SELECT
    // id, width, height, created_at, source_app, source_path——此索引让查询
    // 完全不碰表 B-tree（png_blob/thumb_blob 的 overflow page）。
    //
    // **性能关键**：clipboard_images 表的 png_blob 可达数十 MB/条（截图），
    // 表 B-tree 页与 BLOB overflow 页交错分布在 130MB+ 的 cache DB 文件中。
    // 没有覆盖索引时，即使只查元数据列，SQLite 也需要读取 leaf page 来解析行头，
    // 而 leaf page 可能远离文件头部，导致冷缓存下多次磁盘 I/O。
    // 覆盖索引将元数据集中在 ~2 个 index page（60 行 × ~100B），读取量从
    // 数十 KB 降至 ~12KB。
    //
    // 0.26.16：v2 纳入 pinned_until（置顶排序表达式读它），旧 v1 索引随即删除。
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_clip_img_meta_covering_v2 \
         ON clipboard_images(created_at DESC, id, width, height, source_app, source_path, pinned_until)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    let _ = sqlx::query("DROP INDEX IF EXISTS idx_clip_img_meta_covering")
        .execute(pool)
        .await;

    tracing::debug!("clipboard_images 表已初始化");
    Ok(())
}

/// 保存剪贴板图片条目。
///
/// **去重语义**（0.16.4）：入库前按 sha256 删除旧记录（删旧留新），
/// 再 INSERT 新记录。保证跨时段重复复制同一图片只保留最新一条。
/// 0.26.16：删旧留新时继承置顶状态（同文本表语义，用户钉住的是内容）。
pub async fn save_image(pool: &SqlitePool, item: &ClipboardImage) -> Result<(), String> {
    // P1-#15 fix: 删旧留新包在事务里，防止 DELETE 成功 INSERT 失败导致数据丢失
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // 删旧留新：先按 sha256 删除同内容旧记录（置顶状态随后继承）
    let previous_pinned_until: i64 = sqlx::query_scalar(
        "SELECT COALESCE(MAX(pinned_until), 0) FROM clipboard_images WHERE sha256 = ?1",
    )
    .bind(&item.sha256)
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;
    sqlx::query("DELETE FROM clipboard_images WHERE sha256 = ?1")
        .bind(&item.sha256)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT OR REPLACE INTO clipboard_images
         (id, png_blob, thumb_blob, width, height, sha256, created_at, source_app, source_path, pinned_until)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )
    .bind(&item.id)
    .bind(&item.png_blob)
    .bind(&item.thumb_blob)
    .bind(item.width as i64)
    .bind(item.height as i64)
    .bind(&item.sha256)
    .bind(item.created_at)
    .bind(&item.source_app)
    .bind(&item.source_path)
    .bind(item.pinned_until.max(previous_pinned_until))
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// 历史图片复制成功后置顶：保留内容/来源，只更新复制时间和同秒排序身份。
/// 与重复采集的“删旧留新”顺序一致，但不重新解码、写入 BLOB 或覆盖原始来源。
/// 新 id 使同一秒内再次复制也排在该秒的新图片前面；旧引用由列表重查失效。
pub async fn promote_image(pool: &SqlitePool, id: &str) -> Result<bool, String> {
    let result = sqlx::query("UPDATE clipboard_images SET id = ?1, created_at = ?2 WHERE id = ?3")
        .bind(generate_image_id())
        .bind(chrono::Utc::now().timestamp())
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 查询最近的剪贴板图片条目（不含 BLOB，只元数据 + 缩略图）。
///
/// 0.19.15：生产路径已改用 `query_recent_image_list`（不含 thumb_blob），
/// 此函数仅在测试中使用（验证 save/get 闭环）。
#[allow(dead_code)]
pub async fn query_recent_images(pool: &SqlitePool, limit: i64) -> Vec<ClipboardImageMeta> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query_as::<_, ClipboardImageMeta>(
        "SELECT id, thumb_blob, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images
         ORDER BY (pinned_until > ?2) DESC, created_at DESC, id DESC LIMIT ?1",
    )
    .bind(limit)
    .bind(now)
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
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - (days as i64 * 86400);
    sqlx::query_as::<_, ClipboardImageMeta>(
        "SELECT id, thumb_blob, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images WHERE created_at > ?1
         ORDER BY (pinned_until > ?2) DESC, created_at DESC, id DESC LIMIT ?3",
    )
    .bind(cutoff)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 图片元数据（不含任何 BLOB，用于 AI Capability 等纯元数据场景）。
///
/// 0.19.1：`list_clipboard_images` Capability 只需 id/尺寸/来源/时间，
/// 不需要缩略图 BLOB（~50KB/条），避免无谓的数据加载。
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ClipboardImageListItem {
    pub id: String,
    pub width: i64,
    pub height: i64,
    pub created_at: i64,
    pub source_app: Option<String>,
    pub source_path: Option<String>,
    /// 0.26.16：置顶到期时刻（Unix 秒，0 或已过期 = 未置顶）。
    pub pinned_until: i64,
}

/// 查询最近的剪贴板图片元数据（不含任何 BLOB，仅 id/尺寸/来源/时间）。
///
/// 0.19.1：供 `list_clipboard_images` Capability 用，避免加载缩略图 BLOB。
pub async fn query_recent_image_list(pool: &SqlitePool, limit: i64) -> Vec<ClipboardImageListItem> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query_as::<_, ClipboardImageListItem>(
        "SELECT id, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images
         ORDER BY (pinned_until > ?2) DESC, created_at DESC, id DESC LIMIT ?1",
    )
    .bind(limit)
    .bind(now)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 0.25.29：keyset 游标分页——查询严格早于 `cursor` 的图片元数据。
/// 0.26.16：排序键头部加置顶位（与文本表同构），游标条件为三元组比较。
pub async fn query_recent_image_list_after(
    pool: &SqlitePool,
    cursor: &super::clipboard::ClipboardCursor,
    limit: i64,
) -> Vec<ClipboardImageListItem> {
    let now = chrono::Utc::now().timestamp();
    let cursor_pinned = i64::from(cursor.pinned);
    sqlx::query_as::<_, ClipboardImageListItem>(
        "SELECT id, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images
         WHERE ((pinned_until > ?3) < ?4
                OR ((pinned_until > ?3) = ?4
                    AND (created_at < ?1 OR (created_at = ?1 AND id < ?2))))
         ORDER BY (pinned_until > ?3) DESC, created_at DESC, id DESC LIMIT ?5",
    )
    .bind(cursor.created_at)
    .bind(&cursor.id)
    .bind(now)
    .bind(cursor_pinned)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// SQL 级搜索图片元数据（按 source_app / source_path LIKE 过滤）。
///
/// 替代旧方案（加载 200 条全量元数据 → Rust 层 `String::contains` 过滤），
/// 将过滤下推到 SQLite，减少 IPC 数据量。`query` 已为原始用户输入，
/// SQL 用 `%query%` 做 contains 匹配（大小写不敏感——SQLite LIKE 默认 ASCII 不敏感）。
///
/// **注意**：不搜索 width/height（"图片 800x600" 这种标题是前端渲染时拼的，
/// 用户极少按尺寸搜索；即使需要，SQL LIKE 也无法匹配拼接字符串）。
pub async fn search_image_list(
    pool: &SqlitePool,
    query: &str,
    limit: i64,
) -> Vec<ClipboardImageListItem> {
    let now = chrono::Utc::now().timestamp();
    let pattern = format!("%{query}%");
    sqlx::query_as::<_, ClipboardImageListItem>(
        "SELECT id, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images
         WHERE source_app LIKE ?1 OR source_path LIKE ?1
         ORDER BY (pinned_until > ?2) DESC, created_at DESC, id DESC LIMIT ?3",
    )
    .bind(&pattern)
    .bind(now)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
}

/// 0.25.29：keyset 游标分页的图片 SQL 搜索——LIKE 过滤 + 严格早于 cursor。
/// 0.26.16：游标条件含置顶位（三元组比较，与文本表同构）。
pub async fn search_image_list_after(
    pool: &SqlitePool,
    query: &str,
    cursor: &super::clipboard::ClipboardCursor,
    limit: i64,
) -> Vec<ClipboardImageListItem> {
    let now = chrono::Utc::now().timestamp();
    let cursor_pinned = i64::from(cursor.pinned);
    let pattern = format!("%{query}%");
    sqlx::query_as::<_, ClipboardImageListItem>(
        "SELECT id, width, height, created_at, source_app, source_path, pinned_until
         FROM clipboard_images
         WHERE (source_app LIKE ?1 OR source_path LIKE ?1)
           AND ((pinned_until > ?4) < ?5
                OR ((pinned_until > ?4) = ?5
                    AND (created_at < ?2 OR (created_at = ?2 AND id < ?3))))
         ORDER BY (pinned_until > ?4) DESC, created_at DESC, id DESC LIMIT ?6",
    )
    .bind(&pattern)
    .bind(cursor.created_at)
    .bind(&cursor.id)
    .bind(now)
    .bind(cursor_pinned)
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
    /// 0.17.9：源文件名（从 CF_HDROP 提取）。
    pub source_path: Option<String>,
    /// 0.26.16：置顶到期时刻（Unix 秒，0 或已过期 = 未置顶）。
    pub pinned_until: i64,
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
pub async fn delete_image(pool: &SqlitePool, id: &str) -> Result<(), sqlx::Error> {
    delete_images_by_ids(pool, &[id]).await
}

/// 每库单事务；不存在的 ID 幂等成功，任何 SQL 错误均回滚并返回调用方。
pub async fn delete_images_by_ids(pool: &SqlitePool, ids: &[&str]) -> Result<(), sqlx::Error> {
    if ids.is_empty() {
        return Ok(());
    }
    let mut tx = pool.begin().await?;
    for id in ids {
        sqlx::query("DELETE FROM clipboard_images WHERE id = ?1")
            .bind(id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}

/// 清空所有剪贴板图片历史。
pub async fn clear_all_images(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM clipboard_images")
        .execute(pool)
        .await;
}

/// 0.26.16：设置/取消置顶。`until` 为到期 Unix 秒（0 = 取消置顶）。
/// 返回是否有行被更新（false = 条目不存在）。再次置顶即重置到期时间。
/// 置顶同时刷新 `created_at`（后置顶排置顶组最前 + 延长保留期起点，
/// 与 promote_image 同模式）；取消置顶不动时间。
pub async fn set_image_pinned_until(
    pool: &SqlitePool,
    id: &str,
    until: i64,
) -> Result<bool, String> {
    let now = chrono::Utc::now().timestamp();
    let result = if until > 0 {
        sqlx::query("UPDATE clipboard_images SET pinned_until = ?2, created_at = ?3 WHERE id = ?1")
            .bind(id)
            .bind(until)
            .bind(now)
            .execute(pool)
            .await
    } else {
        sqlx::query("UPDATE clipboard_images SET pinned_until = ?2 WHERE id = ?1")
            .bind(id)
            .bind(until)
            .execute(pool)
            .await
    }
    .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 清理过期图片（按天）。0.26.16：未过期置顶条目豁免。
pub async fn cleanup_old_images(pool: &SqlitePool, days: u32) {
    if days == 0 {
        return;
    }
    let now = chrono::Utc::now().timestamp();
    let cutoff = now - (days as i64 * 86400);
    let result =
        sqlx::query("DELETE FROM clipboard_images WHERE created_at < ?1 AND pinned_until <= ?2")
            .bind(cutoff)
            .bind(now)
            .execute(pool)
            .await;
    if let Ok(r) = result {
        let rows = r.rows_affected();
        if rows > 0 {
            tracing::info!(rows, "清理过期剪贴板图片");
        }
    }
}

/// 超量清理：保留最新的 max_items 条。0.26.16：置顶条目不参与容量计算与淘汰。
pub async fn cleanup_excess_images(pool: &SqlitePool, max_items: u32) {
    let now = chrono::Utc::now().timestamp();
    let count: (i64,) = sqlx::query_as(
        "SELECT COUNT(*) FROM clipboard_images WHERE pinned_until <= ?1",
    )
    .bind(now)
    .fetch_one(pool)
    .await
    .unwrap_or((0,));

    if count.0 > max_items as i64 {
        let excess = count.0 - max_items as i64;
        let _ = sqlx::query(
            "DELETE FROM clipboard_images WHERE id IN (
                SELECT id FROM clipboard_images WHERE pinned_until <= ?2
                ORDER BY created_at ASC, id ASC LIMIT ?1
            )",
        )
        .bind(excess)
        .bind(now)
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

    /// 0.25.29：构造带唯一 sha256 的图片条目（save_image 按 sha256 删旧留新，
    /// 测试要多条共存必须 sha 各异）。
    fn img(id: &str, created_at: i64, source_app: Option<&str>) -> ClipboardImage {
        ClipboardImage {
            id: id.to_string(),
            png_blob: vec![1, 2, 3],
            thumb_blob: vec![4, 5, 6],
            width: 100,
            height: 200,
            sha256: format!("sha_{id}"),
            created_at,
            source_app: source_app.map(|s| s.to_string()),
            source_path: None,
            pinned_until: 0,
        }
    }

    #[tokio::test]
    async fn history_image_copy_promotes_without_losing_content_or_source() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        let mut original = img("clipimg_0", now - 60, Some("mspaint.exe"));
        original.source_path = Some("original.png".into());
        save_image(&pool, &original).await.unwrap();
        save_image(&pool, &img("clipimg_1", now, Some("newer.exe")))
            .await
            .unwrap();

        assert!(promote_image(&pool, &original.id).await.unwrap());
        let images = query_recent_images(&pool, 10).await;
        assert_eq!(images.len(), 2, "复制不增加历史条数");
        let promoted = &images[0];
        assert_eq!(promoted.source_app.as_deref(), Some("mspaint.exe"));
        assert_eq!(promoted.source_path, original.source_path);
        assert_eq!(promoted.thumb_blob, original.thumb_blob);
        assert_eq!(
            (promoted.width, promoted.height),
            (original.width as i64, original.height as i64)
        );
        assert_eq!(
            get_png_by_id(&pool, &promoted.id).await.unwrap(),
            original.png_blob
        );
        let sha: String = sqlx::query_scalar("SELECT sha256 FROM clipboard_images WHERE id = ?1")
            .bind(&promoted.id)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(sha, original.sha256);

        // 不靠睡眠跨秒：每次复制也更新同秒 tie-break，重复复制仍只保留一项。
        let previous_id = promoted.id.clone();
        assert!(promote_image(&pool, "clipimg_1").await.unwrap());
        let intervening = query_recent_image_list(&pool, 10).await;
        assert_eq!(intervening[0].source_app.as_deref(), Some("newer.exe"));
        assert!(promote_image(&pool, &previous_id).await.unwrap());
        let repeated = query_recent_image_list(&pool, 10).await;
        assert_eq!(repeated.len(), 2);
        assert!(repeated[0].id > previous_id);
        assert_eq!(repeated[0].source_app.as_deref(), Some("mspaint.exe"));
        assert!(
            !promote_image(&pool, "deleted").await.unwrap(),
            "已删历史不重新创建"
        );
    }

    /// 0.25.29：keyset 游标分页不重不漏 + LIKE 场景的游标下推。
    #[tokio::test]
    async fn image_list_after_pages_without_gaps_or_dupes() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();
        for (id, at) in [("img_a", 100), ("img_b", 200), ("img_c", 300)] {
            save_image(&pool, &img(id, at, Some("TestApp")))
                .await
                .unwrap();
        }

        let mut collected = Vec::new();
        let mut cursor: Option<crate::data::clipboard::ClipboardCursor> = None;
        loop {
            let batch = match &cursor {
                Some(c) => query_recent_image_list_after(&pool, c, 2).await,
                None => query_recent_image_list(&pool, 2).await,
            };
            if batch.is_empty() {
                break;
            }
            let last = batch.last().unwrap();
            collected.extend(batch.iter().map(|m| m.id.clone()));
            cursor = Some(crate::data::clipboard::ClipboardCursor {
                created_at: last.created_at,
                id: last.id.clone(),
                pinned: false,
            });
        }
        assert_eq!(collected, vec!["img_c", "img_b", "img_a"]);

        // LIKE 搜索的游标下推：取第一条做游标，剩余应恰好是更旧的两条
        let first_search = search_image_list(&pool, "TestApp", 1).await;
        assert_eq!(first_search.len(), 1);
        let rest = search_image_list_after(
            &pool,
            "TestApp",
            &crate::data::clipboard::ClipboardCursor {
                created_at: first_search[0].created_at,
                id: first_search[0].id.clone(),
                pinned: false,
            },
            10,
        )
        .await;
        let ids: Vec<&str> = rest.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, vec!["img_b", "img_a"]);
    }

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
            source_path: None,
            pinned_until: 0,
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
            source_path: None,
            pinned_until: 0,
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
            source_path: None,
            pinned_until: 0,
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
                source_path: None,
                pinned_until: 0,
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

    /// 0.26.16：置顶组排序 + 清理豁免 + set 闭环。
    #[tokio::test]
    async fn pin_orders_first_and_exempts_cleanup() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();
        let now = chrono::Utc::now().timestamp();
        save_image(&pool, &img("old_pinned", 100, Some("old.exe")))
            .await
            .unwrap();
        save_image(&pool, &img("new", now, Some("new.exe")))
            .await
            .unwrap();
        assert!(set_image_pinned_until(&pool, "old_pinned", now + 3600).await.unwrap());
        assert!(!set_image_pinned_until(&pool, "ghost", 1).await.unwrap());
        assert!(
            query_recent_images(&pool, 10).await.iter().any(|m| m.id == "old_pinned" && m.created_at >= now),
            "置顶刷新 created_at"
        );

        let ids: Vec<String> = query_recent_image_list(&pool, 10)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["old_pinned", "new"], "置顶组在前");

        cleanup_old_images(&pool, 1).await;
        cleanup_excess_images(&pool, 1).await;
        let ids: Vec<String> = query_recent_image_list(&pool, 10)
            .await
            .into_iter()
            .map(|m| m.id)
            .collect();
        assert_eq!(ids, vec!["old_pinned", "new"], "过期置顶图片豁免按天与超量清理");
    }

    #[tokio::test]
    async fn save_and_query_source_path() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        init_db(&pool).await.unwrap();

        let item = ClipboardImage {
            id: generate_image_id(),
            png_blob: vec![1, 2, 3],
            thumb_blob: vec![4, 5, 6],
            width: 800,
            height: 600,
            sha256: "hash_sp".to_string(),
            created_at: 1000,
            source_app: Some("explorer.exe".to_string()),
            source_path: Some("photo.jpg".to_string()),
            pinned_until: 0,
        };
        save_image(&pool, &item).await.unwrap();

        let items = query_recent_images(&pool, 10).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].source_app.as_deref(), Some("explorer.exe"));
        assert_eq!(items[0].source_path.as_deref(), Some("photo.jpg"));
    }
}
