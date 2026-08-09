//! 桌面便签持久化（0.16.7）。
//!
//! 独立表 `sticky_notes`，放在 `blink_history.db`（与 history / clipboard_history 同库不同表）。
//! "清除历史"只 DELETE history / clipboard_history，不触碰 sticky_notes——
//! 只有便签删除和卸载全量清理可以移除。
//!
//! 设计见 phases/0.16-clipboard-polish.md §3.8/§3.9。

use sqlx::SqlitePool;

/// 带状态判定的便签写入结果。
///
/// 0.19.5：写入不能再把 `rows_affected == 0` 当成功。领域层据此区分不存在、
/// 已回收和乐观并发冲突，避免 AI 静默覆盖或对不存在记录谎报成功。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StickyWriteOutcome {
    Applied { updated_at: i64 },
    NotFound,
    Trashed,
    Conflict { actual_updated_at: i64 },
}

/// 便签颜色（有限色板，§3.11）。
///
/// 序列化值与前端 CSS class 后缀对应：`sticky-color-yellow` 等。
/// 默认黄色（§3.11「默认黄色」）。Theme 变体跟随 accent 色供用户选择，但不作默认。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StickyColor {
    Theme,
    Yellow,
    Pink,
    Purple,
    Blue,
    Green,
    Gray,
}

impl Default for StickyColor {
    fn default() -> Self {
        Self::Yellow
    }
}

impl StickyColor {
    /// 数据库存储用字符串。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Theme => "theme",
            Self::Yellow => "yellow",
            Self::Pink => "pink",
            Self::Purple => "purple",
            Self::Blue => "blue",
            Self::Green => "green",
            Self::Gray => "gray",
        }
    }

    /// 从数据库字符串解析。
    pub fn from_str(s: &str) -> Self {
        match s {
            "yellow" => Self::Yellow,
            "pink" => Self::Pink,
            "purple" => Self::Purple,
            "blue" => Self::Blue,
            "green" => Self::Green,
            "gray" => Self::Gray,
            "theme" => Self::Theme,
            _ => Self::Yellow, // 兜底：未知值回退默认黄色
        }
    }
}

/// 内容格式（§3.8）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StickyFormat {
    Plain,
    Markdown,
}

impl Default for StickyFormat {
    fn default() -> Self {
        Self::Plain
    }
}

impl StickyFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Plain => "plain",
            Self::Markdown => "markdown",
        }
    }

    pub fn from_str(s: &str) -> Self {
        match s {
            "markdown" => Self::Markdown,
            _ => Self::Plain,
        }
    }
}

/// 便签实体（§3.8 字段定义）。
///
/// 0.17.7 新增 `trashed` / `deleted_at` 字段：关闭=软删除进回收站，
/// 30 天后自动物理删除。`visible` 保留原有语义（控制桌面窗口显示），
/// 与 `trashed` 正交——回收站里的便签 `visible` 无意义。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StickyNote {
    /// 唯一 id（`sticky_{timestamp_nanos}`）
    pub id: String,
    /// 正文内容
    pub content: String,
    /// 格式：plain | markdown
    #[serde(default)]
    pub format: StickyFormat,
    /// 颜色
    #[serde(default)]
    pub color: StickyColor,
    /// 是否桌面可见
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 窗口 x（物理像素）
    #[serde(default)]
    pub x: i32,
    /// 窗口 y（物理像素）
    #[serde(default)]
    pub y: i32,
    /// 窗口宽度（物理像素）
    #[serde(default = "default_width")]
    pub width: i32,
    /// 窗口高度（物理像素）
    #[serde(default = "default_height")]
    pub height: i32,
    /// 是否置顶
    #[serde(default = "default_true")]
    pub always_on_top: bool,
    /// 创建时间（Unix 秒）
    pub created_at: i64,
    /// 更新时间（Unix 秒）
    pub updated_at: i64,
    /// 是否在回收站中（0.17.7）
    #[serde(default)]
    pub trashed: bool,
    /// 进入回收站的时间（Unix 秒），`trashed=false` 时为 None（0.17.7）
    #[serde(default)]
    pub deleted_at: Option<i64>,
}

fn default_true() -> bool {
    true
}
fn default_width() -> i32 {
    280
}
fn default_height() -> i32 {
    320
}

/// 默认窗口尺寸（逻辑像素，前端创建时用）。
/// 0.18.3：偏窄偏高，更符合便签直觉。
pub const DEFAULT_WIDTH: i32 = 280;
pub const DEFAULT_HEIGHT: i32 = 320;

/// 初始化 sticky_notes 表。
///
/// 0.17.7：新增 `trashed` / `deleted_at` 列（迁移）+ 旧数据清理
/// （`visible=false` 的便签直接删除——0.16 未发版，无线上数据）。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS sticky_notes (
            id TEXT PRIMARY KEY,
            content TEXT NOT NULL DEFAULT '',
            format TEXT NOT NULL DEFAULT 'plain',
            color TEXT NOT NULL DEFAULT 'yellow',
            visible INTEGER NOT NULL DEFAULT 1,
            x INTEGER NOT NULL DEFAULT 0,
            y INTEGER NOT NULL DEFAULT 0,
            width INTEGER NOT NULL DEFAULT 280,
            height INTEGER NOT NULL DEFAULT 320,
            always_on_top INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL,
            updated_at INTEGER NOT NULL,
            trashed INTEGER NOT NULL DEFAULT 0,
            deleted_at INTEGER
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sticky_updated ON sticky_notes(updated_at)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 0.17.7 迁移：为已有数据库添加 trashed / deleted_at 列
    // 必须在创建 idx_sticky_trashed 索引之前执行——旧表没有 trashed 列，先建索引会崩溃
    migrate_add_trashed_columns(pool).await?;

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_sticky_trashed ON sticky_notes(trashed)")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 0.17.7 旧数据清理：visible=false 的便签直接删除
    // （0.16 未正式发版，无线上用户数据需保留）
    let result = sqlx::query("DELETE FROM sticky_notes WHERE visible = 0 AND trashed = 0")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    if result.rows_affected() > 0 {
        tracing::info!(
            deleted = result.rows_affected(),
            "sticky 旧数据清理：删除 visible=false 的便签"
        );
    }

    tracing::debug!("sticky_notes 表已初始化");
    Ok(())
}

/// 检测并添加 `trashed` / `deleted_at` 列（0.17.7 迁移）。
async fn migrate_add_trashed_columns(pool: &SqlitePool) -> Result<(), String> {
    let columns: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info('sticky_notes')")
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;

    let has_trashed = columns.iter().any(|(name,)| name == "trashed");
    let has_deleted_at = columns.iter().any(|(name,)| name == "deleted_at");

    if !has_trashed {
        sqlx::query("ALTER TABLE sticky_notes ADD COLUMN trashed INTEGER NOT NULL DEFAULT 0")
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("sticky 迁移：已添加 trashed 列");
    }

    if !has_deleted_at {
        sqlx::query("ALTER TABLE sticky_notes ADD COLUMN deleted_at INTEGER")
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        tracing::info!("sticky 迁移：已添加 deleted_at 列");
    }

    Ok(())
}

/// 生成唯一便签 ID。
pub fn generate_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("sticky_{timestamp}")
}

/// 从数据库行构造 StickyNote。
fn row_to_note(
    id: String,
    content: String,
    format: String,
    color: String,
    visible: i64,
    x: i64,
    y: i64,
    width: i64,
    height: i64,
    always_on_top: i64,
    created_at: i64,
    updated_at: i64,
    trashed: i64,
    deleted_at: Option<i64>,
) -> StickyNote {
    StickyNote {
        id,
        content,
        format: StickyFormat::from_str(&format),
        color: StickyColor::from_str(&color),
        visible: visible != 0,
        x: x as i32,
        y: y as i32,
        width: width as i32,
        height: height as i32,
        always_on_top: always_on_top != 0,
        created_at,
        updated_at,
        trashed: trashed != 0,
        deleted_at,
    }
}

/// 创建新便签。
pub async fn create(pool: &SqlitePool, note: &StickyNote) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO sticky_notes (id, content, format, color, visible, x, y, width, height, always_on_top, created_at, updated_at, trashed, deleted_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, 0, NULL)",
    )
    .bind(&note.id)
    .bind(&note.content)
    .bind(note.format.as_str())
    .bind(note.color.as_str())
    .bind(note.visible as i64)
    .bind(note.x as i64)
    .bind(note.y as i64)
    .bind(note.width as i64)
    .bind(note.height as i64)
    .bind(note.always_on_top as i64)
    .bind(if note.created_at != 0 { note.created_at } else { now })
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 按 id 查询单条便签。
///
/// DB 错误时记录 warn 并返回 None（与空结果不可区分，但至少有日志可查）。
pub async fn get(pool: &SqlitePool, id: &str) -> Option<StickyNote> {
    get_result(pool, id)
        .await
        .map_err(|e| {
            tracing::warn!(sticky_id = %id, error = %e, "sticky get 查询失败");
            e
        })
        .ok()
        .flatten()
}

/// 按 id 查询单条便签并保留 DB 错误，供需要精确错误语义的领域路径使用。
pub async fn get_result(pool: &SqlitePool, id: &str) -> Result<Option<StickyNote>, String> {
    let row = sqlx::query_as::<_, (String, String, String, String, i64, i64, i64, i64, i64, i64, i64, i64, i64, Option<i64>)>(
        "SELECT id, content, format, color, visible, x, y, width, height, always_on_top, created_at, updated_at, trashed, deleted_at FROM sticky_notes WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(row.map(|row| {
        row_to_note(
            row.0, row.1, row.2, row.3, row.4, row.5, row.6, row.7, row.8, row.9, row.10, row.11,
            row.12, row.13,
        )
    }))
}

/// 列出全部活跃便签（`trashed=false`，按 updated_at 倒序）。
///
/// 0.17.7：不再返回回收站中的便签。回收站用 `list_trashed()`。
/// DB 错误时记录 warn 并返回空 Vec。
pub async fn list(pool: &SqlitePool) -> Vec<StickyNote> {
    sqlx::query_as::<_, (String, String, String, String, i64, i64, i64, i64, i64, i64, i64, i64, i64, Option<i64>)>(
        "SELECT id, content, format, color, visible, x, y, width, height, always_on_top, created_at, updated_at, trashed, deleted_at FROM sticky_notes WHERE trashed = 0 ORDER BY updated_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "sticky list 查询失败，返回空列表");
        e
    })
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        row_to_note(
            r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12, r.13,
        )
    })
    .collect()
}

/// 列出回收站中的便签（`trashed=true`，按 deleted_at 倒序）。
///
/// 0.17.7 新增。
pub async fn list_trashed(pool: &SqlitePool) -> Vec<StickyNote> {
    sqlx::query_as::<_, (String, String, String, String, i64, i64, i64, i64, i64, i64, i64, i64, i64, Option<i64>)>(
        "SELECT id, content, format, color, visible, x, y, width, height, always_on_top, created_at, updated_at, trashed, deleted_at FROM sticky_notes WHERE trashed = 1 ORDER BY deleted_at DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "sticky list_trashed 查询失败，返回空列表");
        e
    })
    .unwrap_or_default()
    .into_iter()
    .map(|r| {
        row_to_note(
            r.0, r.1, r.2, r.3, r.4, r.5, r.6, r.7, r.8, r.9, r.10, r.11, r.12, r.13,
        )
    })
    .collect()
}

/// 更新便签正文内容（自动更新 updated_at）。
///
/// `expected_updated_at` 为 `Some` 时启用乐观并发；更新时间保证单调递增，
/// 即使两次写入发生在同一秒也会产生不同 revision。
pub async fn update_content(
    pool: &SqlitePool,
    id: &str,
    content: &str,
    expected_updated_at: Option<i64>,
) -> Result<StickyWriteOutcome, String> {
    let now = chrono::Utc::now().timestamp();
    let updated_at = match expected_updated_at {
        Some(expected) => sqlx::query_scalar::<_, i64>(
            "UPDATE sticky_notes
                 SET content = ?1,
                     updated_at = CASE WHEN updated_at >= ?2 THEN updated_at + 1 ELSE ?2 END
                 WHERE id = ?3 AND trashed = 0 AND updated_at = ?4
                 RETURNING updated_at",
        )
        .bind(content)
        .bind(now)
        .bind(id)
        .bind(expected)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?,
        None => sqlx::query_scalar::<_, i64>(
            "UPDATE sticky_notes
                 SET content = ?1,
                     updated_at = CASE WHEN updated_at >= ?2 THEN updated_at + 1 ELSE ?2 END
                 WHERE id = ?3 AND trashed = 0
                 RETURNING updated_at",
        )
        .bind(content)
        .bind(now)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?,
    };

    if let Some(updated_at) = updated_at {
        return Ok(StickyWriteOutcome::Applied { updated_at });
    }

    classify_failed_write(pool, id, expected_updated_at).await
}

async fn classify_failed_write(
    pool: &SqlitePool,
    id: &str,
    expected_updated_at: Option<i64>,
) -> Result<StickyWriteOutcome, String> {
    let row = sqlx::query_as::<_, (i64, i64)>(
        "SELECT trashed, updated_at FROM sticky_notes WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(match row {
        None => StickyWriteOutcome::NotFound,
        Some((1, _)) => StickyWriteOutcome::Trashed,
        Some((_, actual_updated_at)) if expected_updated_at.is_some() => {
            StickyWriteOutcome::Conflict { actual_updated_at }
        }
        Some((_, actual_updated_at)) => StickyWriteOutcome::Conflict { actual_updated_at },
    })
}

/// 更新便签外观（颜色 + 格式）。
pub async fn update_appearance(
    pool: &SqlitePool,
    id: &str,
    color: &StickyColor,
    format: Option<&StickyFormat>,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    if let Some(fmt) = format {
        sqlx::query(
            "UPDATE sticky_notes SET color = ?1, format = ?2, updated_at = ?3 WHERE id = ?4",
        )
        .bind(color.as_str())
        .bind(fmt.as_str())
        .bind(now)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    } else {
        sqlx::query("UPDATE sticky_notes SET color = ?1, updated_at = ?2 WHERE id = ?3")
            .bind(color.as_str())
            .bind(now)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 更新便签窗口几何（位置 + 尺寸）。
pub async fn update_geometry(
    pool: &SqlitePool,
    id: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
) -> Result<StickyWriteOutcome, String> {
    let now = chrono::Utc::now().timestamp();
    let updated_at = sqlx::query_scalar::<_, i64>(
        "UPDATE sticky_notes
         SET x = ?1, y = ?2, width = ?3, height = ?4,
             updated_at = CASE WHEN updated_at >= ?5 THEN updated_at + 1 ELSE ?5 END
         WHERE id = ?6 AND trashed = 0
         RETURNING updated_at",
    )
    .bind(x as i64)
    .bind(y as i64)
    .bind(width as i64)
    .bind(height as i64)
    .bind(now)
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    match updated_at {
        Some(updated_at) => Ok(StickyWriteOutcome::Applied { updated_at }),
        None => classify_failed_write(pool, id, None).await,
    }
}

/// 设置便签可见性（关闭 = 隐藏，不删除）。
pub async fn set_visible(pool: &SqlitePool, id: &str, visible: bool) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE sticky_notes SET visible = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(visible as i64)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 将便签移入回收站（软删除）。
///
/// 0.17.7 新增。`trashed=true` + `deleted_at=now`，不删除数据。
/// 调用后窗口应 hide。恢复用 `restore_from_trash()`。
pub async fn set_trashed(
    pool: &SqlitePool,
    id: &str,
    trashed: bool,
) -> Result<StickyWriteOutcome, String> {
    let now = chrono::Utc::now().timestamp();
    let updated_at = if trashed {
        sqlx::query_scalar::<_, i64>(
            "UPDATE sticky_notes
             SET trashed = 1, deleted_at = ?1,
                 updated_at = CASE WHEN updated_at >= ?1 THEN updated_at + 1 ELSE ?1 END
             WHERE id = ?2 AND trashed = 0
             RETURNING updated_at",
        )
        .bind(now)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?
    } else {
        sqlx::query_scalar::<_, i64>(
            "UPDATE sticky_notes
             SET trashed = 0, deleted_at = NULL,
                 updated_at = CASE WHEN updated_at >= ?1 THEN updated_at + 1 ELSE ?1 END
             WHERE id = ?2 AND trashed = 1
             RETURNING updated_at",
        )
        .bind(now)
        .bind(id)
        .fetch_optional(pool)
        .await
        .map_err(|e| e.to_string())?
    };

    if let Some(updated_at) = updated_at {
        return Ok(StickyWriteOutcome::Applied { updated_at });
    }

    classify_failed_write(pool, id, None).await
}

/// 清空回收站：物理删除所有 `trashed=true` 的便签。
///
/// 0.17.7 新增。返回删除的行数。
pub async fn clear_all_trashed(pool: &SqlitePool) -> Result<u64, String> {
    let result = sqlx::query("DELETE FROM sticky_notes WHERE trashed = 1")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected())
}

/// 清理过期回收站便签：`trashed=true` 且 `deleted_at` 超过指定天数的物理删除。
///
/// 0.17.7 新增。启动时调用，默认 30 天。
pub async fn cleanup_trashed(pool: &SqlitePool, retention_days: i64) -> u64 {
    let cutoff = chrono::Utc::now().timestamp() - retention_days * 86400;
    let result = match sqlx::query(
        "DELETE FROM sticky_notes WHERE trashed = 1 AND deleted_at IS NOT NULL AND deleted_at < ?1",
    )
    .bind(cutoff)
    .execute(pool)
    .await
    {
        Ok(r) => r.rows_affected(),
        Err(e) => {
            tracing::warn!(error = %e, "sticky cleanup_trashed 清理失败");
            return 0;
        }
    };
    if result > 0 {
        tracing::info!(deleted = result, retention_days, "回收站过期便签已清理");
    }
    result
}

/// 设置便签置顶状态。
pub async fn set_always_on_top(
    pool: &SqlitePool,
    id: &str,
    always_on_top: bool,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE sticky_notes SET always_on_top = ?1, updated_at = ?2 WHERE id = ?3")
        .bind(always_on_top as i64)
        .bind(now)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 删除便签（永久删除，不可恢复）。
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM sticky_notes WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 获取便签统计信息。
///
/// DB 错误时记录 warn 并返回 0。
pub async fn get_stats(pool: &SqlitePool) -> serde_json::Value {
    let count: (i64,) = sqlx::query_as("SELECT COUNT(*) FROM sticky_notes WHERE trashed = 0")
        .fetch_one(pool)
        .await
        .map_err(|e| {
            tracing::warn!(error = %e, "sticky get_stats count 查询失败");
            e
        })
        .unwrap_or((0,));

    let visible_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sticky_notes WHERE visible = 1 AND trashed = 0")
            .fetch_one(pool)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "sticky get_stats visible_count 查询失败");
                e
            })
            .unwrap_or((0,));

    let trashed_count: (i64,) =
        sqlx::query_as("SELECT COUNT(*) FROM sticky_notes WHERE trashed = 1")
            .fetch_one(pool)
            .await
            .map_err(|e| {
                tracing::warn!(error = %e, "sticky get_stats trashed_count 查询失败");
                e
            })
            .unwrap_or((0,));

    serde_json::json!({
        "count": count.0,
        "visible": visible_count.0,
        "trashed": trashed_count.0,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::sqlite::SqlitePoolOptions;

    #[test]
    fn color_roundtrip() {
        for c in [
            StickyColor::Theme,
            StickyColor::Yellow,
            StickyColor::Pink,
            StickyColor::Purple,
            StickyColor::Blue,
            StickyColor::Green,
            StickyColor::Gray,
        ] {
            assert_eq!(StickyColor::from_str(c.as_str()), c);
        }
    }

    #[test]
    fn color_default_is_yellow() {
        assert_eq!(StickyColor::default(), StickyColor::Yellow);
    }

    #[test]
    fn format_roundtrip() {
        assert_eq!(
            StickyFormat::from_str(StickyFormat::Plain.as_str()),
            StickyFormat::Plain
        );
        assert_eq!(
            StickyFormat::from_str(StickyFormat::Markdown.as_str()),
            StickyFormat::Markdown
        );
    }

    #[test]
    fn generate_id_has_prefix() {
        let id = generate_id();
        assert!(id.starts_with("sticky_"));
    }

    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        init_db(&pool).await.unwrap();
        pool
    }

    async fn insert_note(pool: &SqlitePool, id: &str) -> StickyNote {
        let note = StickyNote {
            id: id.into(),
            content: "before".into(),
            format: StickyFormat::Plain,
            color: StickyColor::Yellow,
            visible: true,
            x: 0,
            y: 0,
            width: DEFAULT_WIDTH,
            height: DEFAULT_HEIGHT,
            always_on_top: true,
            created_at: 0,
            updated_at: 0,
            trashed: false,
            deleted_at: None,
        };
        create(pool, &note).await.unwrap();
        get(pool, id).await.unwrap()
    }

    #[tokio::test]
    async fn optimistic_update_is_monotonic_and_detects_conflict() {
        let pool = test_pool().await;
        let note = insert_note(&pool, "s1").await;

        let first = update_content(&pool, "s1", "first", Some(note.updated_at))
            .await
            .unwrap();
        let StickyWriteOutcome::Applied { updated_at } = first else {
            panic!("expected applied")
        };
        assert!(updated_at > note.updated_at);

        let stale = update_content(&pool, "s1", "stale", Some(note.updated_at))
            .await
            .unwrap();
        assert_eq!(
            stale,
            StickyWriteOutcome::Conflict {
                actual_updated_at: updated_at
            }
        );
        assert_eq!(get(&pool, "s1").await.unwrap().content, "first");
    }

    #[tokio::test]
    async fn writes_distinguish_missing_and_trashed() {
        let pool = test_pool().await;
        assert_eq!(
            update_content(&pool, "missing", "x", None).await.unwrap(),
            StickyWriteOutcome::NotFound
        );
        assert_eq!(
            update_geometry(&pool, "missing", 1, 2, 300, 400)
                .await
                .unwrap(),
            StickyWriteOutcome::NotFound
        );

        insert_note(&pool, "s2").await;
        assert!(matches!(
            set_trashed(&pool, "s2", true).await.unwrap(),
            StickyWriteOutcome::Applied { .. }
        ));
        assert_eq!(
            update_content(&pool, "s2", "x", None).await.unwrap(),
            StickyWriteOutcome::Trashed
        );
        assert_eq!(
            update_geometry(&pool, "s2", 1, 2, 300, 400).await.unwrap(),
            StickyWriteOutcome::Trashed
        );
        assert_eq!(
            set_trashed(&pool, "s2", true).await.unwrap(),
            StickyWriteOutcome::Trashed
        );
    }

    #[tokio::test]
    async fn geometry_update_changes_revision_and_values() {
        let pool = test_pool().await;
        let note = insert_note(&pool, "geometry").await;
        let outcome = update_geometry(&pool, &note.id, 10, 20, 320, 440)
            .await
            .unwrap();
        let StickyWriteOutcome::Applied { updated_at } = outcome else {
            panic!("expected applied")
        };
        assert!(updated_at > note.updated_at);
        let updated = get(&pool, &note.id).await.unwrap();
        assert_eq!((updated.x, updated.y), (10, 20));
        assert_eq!((updated.width, updated.height), (320, 440));
    }
}
