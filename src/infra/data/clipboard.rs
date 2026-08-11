//! 剪贴板历史（0.7.3）：SQLite 持久化，模糊搜索。
//!
//! 设计（见 phases/0.7-plugin-ecosystem-local-search.md §三）：
//! - 默认关闭，用户手动启用后才生效
//! - 去重：10s 防连发 + 跨时段内容去重删旧留新（0.16.4）
//! - SQLite 持久化存储，可配置保留天数
//! - 敏感应用黑名单（密码管理器等）
//!
//! ⚠️ 当前状态：基础存储和查询功能已实现，但缺少实时监听功能。
//! 用户需要手动触发记录（如通过命令），无法自动捕获剪贴板变化。
//!
//! TODO（0.8+）：实现实时监听 `AddClipboardFormatListener`：
//! 1. 创建隐藏窗口（用于接收 Windows 消息）
//! 2. 调用 `AddClipboardFormatListener` 注册监听
//! 3. 处理 `WM_CLIPBOARDUPDATE` 消息
//! 4. 在消息处理中读取剪贴板内容，检查黑名单，保存到数据库
//! 5. 需要与主线程消息循环集成（可能需要 `tauri::async_runtime::spawn_blocking`）
//!
//! 参考实现思路：
//! ```rust
//! // 在某个合适的时机（如应用启动后）
//! let hwnd = create_hidden_window()?;
//! unsafe { AddClipboardFormatListener(hwnd)?; }
//! // 在窗口过程中处理 WM_CLIPBOARDUPDATE
//! ```

use sqlx::SqlitePool;

/// 剪贴板条目。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipboardItem {
    pub id: String,
    pub text: String,
    pub preview: String,
    pub created_at: i64,
    pub source_app: Option<String>,
    pub hit_count: u32,
}

/// 剪贴板配置。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ClipboardConfig {
    /// 是否启用剪贴板历史
    #[serde(default)]
    pub enabled: bool,
    /// 最大保留条数（存储上限）
    #[serde(default = "default_max_items")]
    pub max_items: u32,
    /// 保留天数（0=永久）
    #[serde(default = "default_retention_days")]
    pub retention_days: u32,
    /// 单次展示条数（Alt+C / 搜索"剪贴板"时一次显示多少条）。
    /// 与 `max_items`（存储上限）语义不同：这只控制一次召回展示多少。
    #[serde(default = "default_display_count")]
    pub display_count: u32,
    /// 是否允许搜索剪贴板内容
    #[serde(default = "default_true")]
    pub search_enabled: bool,
    /// 敏感窗口标题黑名单
    #[serde(default = "default_blacklist")]
    pub blacklist_keywords: Vec<String>,
    /// 是否采集剪贴板图片（0.16.4）。false 时跳过 CF_DIB 采集。
    #[serde(default = "default_true")]
    pub capture_images: bool,
    /// 图片最大保留条数（0.16.4）。独立于文本 max_items。
    #[serde(default = "default_max_image_items")]
    pub max_image_items: u32,
    /// 搜索候选池上限（0.19.15：性能可配置化）。
    ///
    /// 搜索时拉近 N 天的元数据做 fuzzy 匹配，N 由 `retention_days` 控制，
    /// 此值控制候选池最大条数。默认 500——对大多数用户足够，
    /// 历史记录特别多的用户可调高（消耗更多内存/IO），或调低（更快响应）。
    /// 范围 [50, 5000]。
    #[serde(default = "default_candidate_limit")]
    pub candidate_limit: u32,
}

fn default_max_items() -> u32 {
    10000 // 兜底防无限增长；主要靠 retention_days 按天清理
}
fn default_retention_days() -> u32 {
    30
}
/// 单次展示条数默认值。30 对齐 AI Capability `search_clipboard_history` 默认值。
fn default_display_count() -> u32 {
    30
}
/// 搜索候选池上限默认值。500 条 × preview(80 chars) ≈ 40KB JSON，足够 fuzzy 匹配。
fn default_candidate_limit() -> u32 {
    500
}
fn default_true() -> bool {
    true
}
fn default_blacklist() -> Vec<String> {
    vec![
        "密码".to_string(),
        "Password".to_string(),
        "Bitwarden".to_string(),
        "1Password".to_string(),
        "KeePass".to_string(),
    ]
}
fn default_max_image_items() -> u32 {
    200
}

impl Default for ClipboardConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            max_items: 10000, // 兜底；主要靠 retention_days=30 按天清理
            retention_days: 30,
            display_count: 30,
            search_enabled: true,
            blacklist_keywords: default_blacklist(),
            capture_images: true,
            max_image_items: 200,
            candidate_limit: 500,
        }
    }
}

/// 初始化剪贴板历史表。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS clipboard_history (
            id TEXT PRIMARY KEY,
            text TEXT NOT NULL,
            preview TEXT NOT NULL,
            created_at INTEGER NOT NULL,
            source_app TEXT,
            hit_count INTEGER NOT NULL DEFAULT 0
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_clip_created ON clipboard_history(created_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 覆盖索引：query_recent_meta / query_recent_days_meta 只 SELECT
    // id, preview, created_at, source_app, hit_count——此索引让查询完全不碰
    // 表 B-tree（text 列的 overflow page），对 1000+ 行 + 大 text 场景提速显著。
    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_clip_meta_covering \
         ON clipboard_history(created_at DESC, id, preview, source_app, hit_count)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    tracing::debug!("clipboard_history 表已初始化");
    Ok(())
}

/// 保存剪贴板条目到数据库。
///
/// **去重语义**（0.16.4）：入库前按文本内容删除旧记录（删旧留新），
/// 再 INSERT 新记录。保证跨时段重复复制同一文本只保留最新一条。
/// 短窗口去重（10s 防连发）在监听器侧已处理，此处做跨时段内容去重。
#[allow(dead_code)] // 预留给剪贴板监听器
pub async fn save_item(pool: &SqlitePool, item: &ClipboardItem) -> Result<(), String> {
    // P1-#15 fix: 删旧留新包在事务里，防止 DELETE 成功 INSERT 失败导致数据丢失
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // 删旧留新：先按文本内容删除同内容旧记录
    sqlx::query("DELETE FROM clipboard_history WHERE text = ?1 AND id != ?2")
        .bind(&item.text)
        .bind(&item.id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query(
        "INSERT OR REPLACE INTO clipboard_history (id, text, preview, created_at, source_app, hit_count) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
    )
    .bind(&item.id)
    .bind(&item.text)
    .bind(&item.preview)
    .bind(item.created_at)
    .bind(&item.source_app)
    .bind(item.hit_count)
    .execute(&mut *tx)
    .await
    .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(())
}

/// 查询最近的剪贴板条目。
pub async fn query_recent(pool: &SqlitePool, limit: i64) -> Vec<ClipboardItem> {
    sqlx::query_as::<_, (String, String, String, i64, Option<String>, u32)>(
        "SELECT id, text, preview, created_at, source_app, hit_count FROM clipboard_history ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, text, preview, created_at, source_app, hit_count)| ClipboardItem {
        id,
        text,
        preview,
        created_at,
        source_app,
        hit_count,
    })
    .collect()
}

/// 查询近 N 天的剪贴板记录（按时间倒序，最多 limit 条）。
///
/// 供 `search` 做 fuzzy 候选池——查近 30 天而非固定 200 条，
/// 确保 `retention_days` 内的记录都能被搜到。
pub async fn query_recent_days(pool: &SqlitePool, days: u32, limit: i64) -> Vec<ClipboardItem> {
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    sqlx::query_as::<_, (String, String, String, i64, Option<String>, u32)>(
        "SELECT id, text, preview, created_at, source_app, hit_count \
         FROM clipboard_history WHERE created_at > ?1 ORDER BY created_at DESC LIMIT ?2",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(
        |(id, text, preview, created_at, source_app, hit_count)| ClipboardItem {
            id,
            text,
            preview,
            created_at,
            source_app,
            hit_count,
        },
    )
    .collect()
}

/// 剪贴板条目元数据（不含完整 text，供搜索路径用）。
///
/// 搜索路径只需 preview（80 字符截断）做 fuzzy 匹配 + 展示，
/// 完整 text 在用户激活时通过 [`get_text_by_id`] 按需拉取，避免搜索路径
/// 加载 500 条完整 text 导致 MB 级 JSON 序列化开销。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct ClipboardMeta {
    pub id: String,
    pub preview: String,
    pub created_at: i64,
    pub source_app: Option<String>,
    pub hit_count: u32,
}

/// 查询近 N 天的剪贴板元数据（不含 text 列）。
///
/// 供搜索路径用——`query_recent_days` 的轻量版，不加载完整 `text` 列
/// （长文本可达数十 KB/条，500 条 × text = MB 级数据传输 + 序列化开销）。
/// 仅 SELECT `id, preview, created_at, source_app, hit_count`，足够 fuzzy
/// 匹配 + 副行展示用。
pub async fn query_recent_days_meta(
    pool: &SqlitePool,
    days: u32,
    limit: i64,
) -> Vec<ClipboardMeta> {
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    sqlx::query_as::<_, (String, String, i64, Option<String>, u32)>(
        "SELECT id, preview, created_at, source_app, hit_count \
         FROM clipboard_history WHERE created_at > ?1 ORDER BY created_at DESC LIMIT ?2",
    )
    .bind(cutoff)
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, preview, created_at, source_app, hit_count)| ClipboardMeta {
        id,
        preview,
        created_at,
        source_app,
        hit_count,
    })
    .collect()
}

/// 查询最近的剪贴板元数据（不含 text 列）。
///
/// 供空 query 场景（Alt+C 刚进入剪贴板模式）用——只需最近 N 条的 preview + 元数据
/// 做展示，不需要完整 text。`query_recent` 的轻量版。
pub async fn query_recent_meta(pool: &SqlitePool, limit: i64) -> Vec<ClipboardMeta> {
    sqlx::query_as::<_, (String, String, i64, Option<String>, u32)>(
        "SELECT id, preview, created_at, source_app, hit_count \
         FROM clipboard_history ORDER BY created_at DESC LIMIT ?1",
    )
    .bind(limit)
    .fetch_all(pool)
    .await
    .unwrap_or_default()
    .into_iter()
    .map(|(id, preview, created_at, source_app, hit_count)| ClipboardMeta {
        id,
        preview,
        created_at,
        source_app,
        hit_count,
    })
    .collect()
}

/// 模糊搜索剪贴板内容（限定近 30 天，与默认 retention_days 对齐）。
///
/// **0.11.5 改动**：原先查最近 200 条做 fuzzy 候选池，现改为查近 30 天的记录——
/// 这样即使保留期内的记录超过 200 条也能被搜到。30 天与 `retention_days` 默认值对齐。
pub async fn search(pool: &SqlitePool, query: &str, limit: i64) -> Vec<ClipboardItem> {
    let items = query_recent_days(pool, 30, 500).await;

    // 使用 nucleo 模糊匹配
    use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32Str};

    let query_lower = query.to_ascii_lowercase();
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        &query_lower,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();

    let mut scored: Vec<(u32, ClipboardItem)> = items
        .into_iter()
        .filter_map(|item| {
            let haystack = Utf32Str::new(&item.preview, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((score, item))
        })
        .collect();

    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit as usize)
        .map(|(_, item)| item)
        .collect()
}

/// 按 id 查询单条剪贴板记录（0.16.3：编辑器保存时继承 hit_count 用）。
///
/// 返回完整 `ClipboardItem`（含 text），供编辑器等需要完整文本的场景用。
pub async fn query_by_id(pool: &SqlitePool, id: &str) -> Option<ClipboardItem> {
    sqlx::query_as::<_, (String, String, String, i64, Option<String>, u32)>(
        "SELECT id, text, preview, created_at, source_app, hit_count FROM clipboard_history WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .ok()
    .flatten()
    .map(|(id, text, preview, created_at, source_app, hit_count)| ClipboardItem {
        id,
        text,
        preview,
        created_at,
        source_app,
        hit_count,
    })
}

/// 按 id 查询完整 text（激活时按需加载）。
///
/// 搜索路径只携带 `id` + `preview`，用户选中某条历史时调此函数拉取完整 `text`。
/// 相比 `query_by_id` 只取 `text` 列，不加载其他字段——单列 scalar 查询最快。
pub async fn get_text_by_id(pool: &SqlitePool, id: &str) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT text FROM clipboard_history WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await
        .ok()
        .flatten()
}

/// 记录剪贴板命中（用户选择粘贴某条历史）。
pub async fn record_hit(pool: &SqlitePool, id: &str) {
    let _ = sqlx::query("UPDATE clipboard_history SET hit_count = hit_count + 1 WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await;
}

/// 删除指定条目。
pub async fn delete_item(pool: &SqlitePool, id: &str) {
    let _ = sqlx::query("DELETE FROM clipboard_history WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await;
}

/// 清空所有剪贴板历史。
pub async fn clear_all(pool: &SqlitePool) {
    let _ = sqlx::query("DELETE FROM clipboard_history")
        .execute(pool)
        .await;
}

/// 清理过期条目（按天）。启动时由 main.rs 调用。
pub async fn cleanup_old(pool: &SqlitePool, days: u32) {
    if days == 0 {
        return;
    }
    let cutoff = chrono::Utc::now().timestamp() - (days as i64 * 86400);
    let result = sqlx::query("DELETE FROM clipboard_history WHERE created_at < ?1")
        .bind(cutoff)
        .execute(pool)
        .await;
    match result {
        Ok(r) => {
            let rows = r.rows_affected();
            if rows > 0 {
                tracing::info!(rows, "清理过期剪贴板历史");
            }
        }
        Err(e) => tracing::warn!(error = %e, "清理过期剪贴板历史失败"),
    }
}

/// 超量清理：保留最新的 max_items 条（兑底，主要靠 cleanup_old 按天清理）。
pub async fn cleanup_excess(pool: &SqlitePool, max_items: u32) {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM clipboard_history")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));

    if count.0 > max_items as i64 {
        let excess = count.0 - max_items as i64;
        let _ = sqlx::query(
            "DELETE FROM clipboard_history WHERE id IN (SELECT id FROM clipboard_history ORDER BY created_at ASC LIMIT ?1)",
        )
        .bind(excess)
        .execute(pool)
        .await;
        // tracing::info!(deleted = excess, "清理超量剪贴板历史");
    }
}

/// 检查窗口标题是否在黑名单中。
#[allow(dead_code)] // 预留给剪贴板监听器
pub fn is_blacklisted(title: &str, blacklist: &[String]) -> bool {
    let title_lower = title.to_ascii_lowercase();
    blacklist
        .iter()
        .any(|keyword| title_lower.contains(&keyword.to_ascii_lowercase()))
}

/// 生成预览文本（截断前 80 字符）。
#[allow(dead_code)] // 预留给剪贴板监听器
pub fn make_preview(text: &str) -> String {
    if text.chars().count() <= 80 {
        text.to_string()
    } else {
        let preview: String = text.chars().take(80).collect();
        format!("{}...", preview)
    }
}

/// 生成唯一 ID。
#[allow(dead_code)] // 预留给剪贴板监听器
pub fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("clip_{}", timestamp)
}

/// 获取剪贴板统计信息。
pub async fn get_stats(pool: &SqlitePool) -> serde_json::Value {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM clipboard_history")
        .fetch_one(pool)
        .await
        .unwrap_or((0,));

    let oldest: (Option<i64>,) = sqlx::query_as("SELECT MIN(created_at) FROM clipboard_history")
        .fetch_one(pool)
        .await
        .unwrap_or((None,));

    serde_json::json!({
        "count": count.0,
        "oldest_at": oldest.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn make_preview_truncates_long_text() {
        let text = "a".repeat(100);
        let preview = make_preview(&text);
        assert_eq!(preview.len(), 83); // 80 + "..."
        assert!(preview.ends_with("..."));
    }

    #[test]
    fn make_preview_keeps_short_text() {
        let text = "hello";
        let preview = make_preview(text);
        assert_eq!(preview, "hello");
    }

    #[test]
    fn is_blacklisted_matches_keywords() {
        let blacklist = vec!["密码".to_string(), "Password".to_string()];
        assert!(is_blacklisted("输入密码", &blacklist));
        assert!(is_blacklisted("Enter Password", &blacklist));
        assert!(!is_blacklisted("普通窗口", &blacklist));
    }

    #[test]
    fn is_blacklisted_case_insensitive() {
        let blacklist = vec!["password".to_string()];
        assert!(is_blacklisted("PASSWORD", &blacklist));
        assert!(is_blacklisted("Password", &blacklist));
    }
}
