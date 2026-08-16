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

/// display_pages 默认值：3 页（与 phase 0.20.1 契约一致）。
const DEFAULT_DISPLAY_PAGES: u32 = 3;
/// display_pages 范围下限。
const DISPLAY_PAGES_MIN: u32 = 1;
/// display_pages 范围上限。
const DISPLAY_PAGES_MAX: u32 = 20;
/// 旧 display_count 默认值（迁移计算用）。
const LEGACY_DISPLAY_COUNT_DEFAULT: u32 = 30;

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
    /// 剪贴板模式一次加载几页（0.20.1：替代旧 `display_count`）。
    /// `effective_limit = display_pages × page_size`，`page_size` 来自 SearchConfig。
    /// 范围 1..=20，默认 3。
    #[serde(default = "default_display_pages")]
    pub display_pages: u32,
    /// 旧字段：单次展示条数。0.20.1 起废弃，仅用于反序列化迁移。
    /// 新代码不应读写此字段；保存时只写 `display_pages`。
    /// `skip_serializing` 确保保存时不双写旧字段（规划 3.5：只向前迁移）。
    #[serde(default = "default_display_count", skip_serializing)]
    #[allow(dead_code)] // 0.20.1 废弃字段，仅反序列化迁移用
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
/// 单次展示条数默认值（旧字段，迁移用）。30 = 3 页 × 10（旧默认 page_size）。
fn default_display_count() -> u32 {
    LEGACY_DISPLAY_COUNT_DEFAULT
}
/// display_pages 默认值。
fn default_display_pages() -> u32 {
    DEFAULT_DISPLAY_PAGES
}
/// 从旧 `display_count` 和 `page_size` 换算 `display_pages`。
/// `ceil(display_count / page_size)`，钳制到 `[1, 20]`；非法值回退 3。
///
/// 0.20.7：使用饱和/扩大整数计算，避免 `display_count + page_size - 1` 的 `u32` 溢出。
pub fn migrate_display_count_to_pages(display_count: u32, page_size: u32) -> u32 {
    if display_count == 0 || page_size == 0 {
        return DEFAULT_DISPLAY_PAGES;
    }
    // 扩大到 u64 做加法，避免溢出
    let count = display_count as u64;
    let size = page_size as u64;
    let pages = (count + size - 1) / size; // ceil
    // 钳制到 u32 范围再 clamp 到合法区间
    let pages = pages.min(u32::MAX as u64) as u32;
    pages.clamp(DISPLAY_PAGES_MIN, DISPLAY_PAGES_MAX)
}
/// 钳制 `display_pages` 到 `[1, 20]`，非法值回退 3。
pub fn clamp_display_pages(pages: u32) -> u32 {
    if (DISPLAY_PAGES_MIN..=DISPLAY_PAGES_MAX).contains(&pages) {
        pages
    } else {
        DEFAULT_DISPLAY_PAGES
    }
}

/// 0.20.1 启动迁移：从 raw clipboard JSON 判定 `display_pages` 最终值。
///
/// **迁移规则**（规划 §3.5、§5.2 配置契约）：
/// - 新旧字段同时存在：`display_pages` 优先，忽略 `display_count` 并记录一次 warn。
/// - 只有旧字段：`ceil(display_count / page_size)` 钳制到 `[1, 20]`；非法旧值回退 3 页。
/// - 只有新字段或两者都不存在：使用 `display_pages` 原值或默认 3 页。
///
/// 返回 `(display_pages, migrated)`——`migrated=true` 表示从旧字段换算过，
/// 调用方可据此决定是否在首次保存时写回迁移后的值。
pub fn resolve_display_pages_from_json(
    raw_json: &serde_json::Value,
    page_size: u32,
) -> (u32, bool) {
    let has_new = raw_json.get("display_pages").is_some();
    let has_old = raw_json.get("display_count").is_some();

    match (has_new, has_old) {
        // 新旧并存：新字段优先，忽略旧字段。
        // 返回 migrated=true 让调用方写回——`display_count` 有 skip_serializing，
        // 写回即丢弃旧字段，避免旧字段永远留在 DB 导致每次启动重复 warn
        (true, true) => {
            // 0.20.7：从 u64 checked 转换到 u32，不静默截断
            let pages = raw_json
                .get("display_pages")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(DEFAULT_DISPLAY_PAGES);
            tracing::warn!(
                display_pages = pages,
                display_count = raw_json
                    .get("display_count")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0),
                "clipboard: display_count 与 display_pages 并存，display_pages 优先（旧字段将被丢弃）"
            );
            (clamp_display_pages(pages), true)
        }
        // 只有旧字段：换算迁移
        (false, true) => {
            // 0.20.7：从 u64 checked 转换到 u32，不静默截断
            let raw_count = raw_json
                .get("display_count")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(LEGACY_DISPLAY_COUNT_DEFAULT);
            let migrated = migrate_display_count_to_pages(raw_count, page_size);
            tracing::info!(
                raw_count,
                page_size,
                migrated_pages = migrated,
                "clipboard: display_count → display_pages 迁移换算"
            );
            (migrated, true)
        }
        // 只有新字段或都没有：使用 display_pages 原值或默认
        _ => {
            // 0.20.7：从 u64 checked 转换到 u32，不静默截断
            let pages = raw_json
                .get("display_pages")
                .and_then(|v| v.as_u64())
                .and_then(|v| u32::try_from(v).ok())
                .unwrap_or(DEFAULT_DISPLAY_PAGES);
            (clamp_display_pages(pages), false)
        }
    }
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
            display_pages: DEFAULT_DISPLAY_PAGES,
            display_count: LEGACY_DISPLAY_COUNT_DEFAULT,
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
    .map(
        |(id, preview, created_at, source_app, hit_count)| ClipboardMeta {
            id,
            preview,
            created_at,
            source_app,
            hit_count,
        },
    )
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
    .map(
        |(id, preview, created_at, source_app, hit_count)| ClipboardMeta {
            id,
            preview,
            created_at,
            source_app,
            hit_count,
        },
    )
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

/// 0.20.2：按 id 批量查询完整 text（批量原子复制用）。
///
/// 接受 id 列表，返回 `(id, Option<String>)` 元组列表，顺序与输入一致。
/// 未找到的 id 返回 `None`——调用方据此次定是否整体放弃。
///
/// **实现**：逐个 `get_text_by_id` 查询（曾尝试动态 IN 查询但 sqlx 生命周期问题，
/// 且批量通常 < 50 条，逐查性能足够 < 10ms）。
pub async fn get_text_batch_by_ids(
    pool: &SqlitePool,
    ids: &[&str],
) -> Vec<(String, Option<String>)> {
    if ids.is_empty() {
        return Vec::new();
    }
    // 逐个查询，保持输入顺序
    let mut results = Vec::with_capacity(ids.len());
    for id in ids {
        let text = get_text_by_id(pool, id).await;
        results.push((id.to_string(), text));
    }
    results
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

    // ── 0.20.1: display_count → display_pages 迁移单测 ───────────────

    #[test]
    fn migrate_30_count_9_page_size_yields_4_pages() {
        assert_eq!(migrate_display_count_to_pages(30, 9), 4);
    }

    #[test]
    fn migrate_18_count_9_page_size_yields_2_pages() {
        assert_eq!(migrate_display_count_to_pages(18, 9), 2);
    }

    #[test]
    fn migrate_exact_multiple_no_rounding() {
        assert_eq!(migrate_display_count_to_pages(9, 9), 1);
        assert_eq!(migrate_display_count_to_pages(18, 9), 2);
    }

    #[test]
    fn migrate_clamps_to_20_max() {
        assert_eq!(migrate_display_count_to_pages(9999, 1), 20);
    }

    #[test]
    fn migrate_zero_values_fallback_to_default() {
        assert_eq!(migrate_display_count_to_pages(0, 9), DEFAULT_DISPLAY_PAGES);
        assert_eq!(migrate_display_count_to_pages(30, 0), DEFAULT_DISPLAY_PAGES);
    }

    #[test]
    fn clamp_display_pages_valid_range() {
        assert_eq!(clamp_display_pages(1), 1);
        assert_eq!(clamp_display_pages(20), 20);
        assert_eq!(clamp_display_pages(10), 10);
    }

    #[test]
    fn clamp_display_pages_invalid_falls_back() {
        assert_eq!(clamp_display_pages(0), DEFAULT_DISPLAY_PAGES);
        assert_eq!(clamp_display_pages(21), DEFAULT_DISPLAY_PAGES);
        assert_eq!(clamp_display_pages(9999), DEFAULT_DISPLAY_PAGES);
    }

    #[test]
    fn config_default_has_display_pages_3() {
        let cfg = ClipboardConfig::default();
        assert_eq!(cfg.display_pages, 3);
        // 旧字段保留默认值但不参与运行时逻辑
        assert_eq!(cfg.display_count, 30);
    }

    #[test]
    fn config_serde_new_field_takes_priority() {
        let json = serde_json::json!({
            "display_pages": 5,
            "display_count": 30,
            "enabled": true,
            "max_items": 1000,
            "retention_days": 30,
            "search_enabled": true,
            "blacklist_keywords": [],
            "capture_images": true,
            "max_image_items": 200,
            "candidate_limit": 500,
        });
        let cfg: ClipboardConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.display_pages, 5);
    }

    #[test]
    fn config_serde_only_old_field_uses_default_pages() {
        // 只有旧字段、没有新字段时，display_pages 使用 serde default = 3。
        // 迁移换算在应用层 helper 中完成（需要 page_size 上下文）。
        let json = serde_json::json!({
            "display_count": 30,
            "enabled": true,
            "max_items": 1000,
            "retention_days": 30,
            "search_enabled": true,
            "blacklist_keywords": [],
            "capture_images": true,
            "max_image_items": 200,
            "candidate_limit": 500,
        });
        let cfg: ClipboardConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.display_pages, 3); // serde default
        assert_eq!(cfg.display_count, 30); // 旧字段保留原值
    }

    // ── 0.20.1: resolve_display_pages_from_json 迁移单测 ────────────

    #[test]
    fn resolve_both_fields_new_takes_priority() {
        // 新旧并存：display_pages 优先，且要求写回（丢弃旧字段，避免每次启动重复 warn）
        let json = serde_json::json!({"display_pages": 5, "display_count": 30});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, 5);
        assert!(migrated);
    }

    #[test]
    fn resolve_only_old_field_migrates() {
        // 只有旧字段：ceil(30/9) = 4 页
        let json = serde_json::json!({"display_count": 30});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, 4);
        assert!(migrated);
    }

    #[test]
    fn resolve_only_old_field_18_count_9_page() {
        // ceil(18/9) = 2 页
        let json = serde_json::json!({"display_count": 18});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, 2);
        assert!(migrated);
    }

    #[test]
    fn resolve_only_new_field_no_migration() {
        let json = serde_json::json!({"display_pages": 7});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, 7);
        assert!(!migrated);
    }

    #[test]
    fn resolve_neither_field_returns_default() {
        let json = serde_json::json!({"enabled": true});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, DEFAULT_DISPLAY_PAGES);
        assert!(!migrated);
    }

    #[test]
    fn resolve_old_field_zero_falls_back() {
        // display_count=0 是非法值，migrate 函数回退默认 3 页
        let json = serde_json::json!({"display_count": 0});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, DEFAULT_DISPLAY_PAGES);
        assert!(migrated);
    }

    #[test]
    fn resolve_old_field_clamps_to_20() {
        // display_count=9999, page_size=1 → 9999 页，clamp 到 20
        let json = serde_json::json!({"display_count": 9999});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 1);
        assert_eq!(pages, 20);
        assert!(migrated);
    }

    #[test]
    fn resolve_new_field_out_of_range_clamps() {
        // 新字段越界时 clamp
        let json = serde_json::json!({"display_pages": 99});
        let (pages, migrated) = resolve_display_pages_from_json(&json, 9);
        assert_eq!(pages, DEFAULT_DISPLAY_PAGES);
        assert!(!migrated);
    }

    // ── 0.20.1: 序列化不双写单测 ─────────────────────────────────────

    #[test]
    fn config_serialize_omits_display_count() {
        // 保存时不双写 display_count（规划 3.5：只向前迁移）
        let cfg = ClipboardConfig::default();
        let json = serde_json::to_value(&cfg).unwrap();
        assert!(
            json.get("display_count").is_none(),
            "序列化不应包含 display_count"
        );
        assert!(
            json.get("display_pages").is_some(),
            "序列化应包含 display_pages"
        );
    }

    #[test]
    fn config_roundtrip_preserves_display_pages_without_display_count() {
        let cfg = ClipboardConfig {
            display_pages: 7,
            display_count: 99, // 旧值不应影响
            ..ClipboardConfig::default()
        };
        let json_str = serde_json::to_string(&cfg).unwrap();
        // 反序列化回来 display_pages 应保持 7
        let roundtrip: ClipboardConfig = serde_json::from_str(&json_str).unwrap();
        assert_eq!(roundtrip.display_pages, 7);
        // 旧字段不写出，反序列化时走 default = 30
        assert_eq!(roundtrip.display_count, LEGACY_DISPLAY_COUNT_DEFAULT);
    }
}
