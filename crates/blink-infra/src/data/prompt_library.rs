//! 提示词库行存储（0.27.5，phase §3.7 / §8.1）。
//!
//! **分层**：本模块只管行存储（DDL/CRUD/revision CAS/种子落库）；稳定 builtin
//! ID、默认内容目录与「有效内容解析」在 `blink_domain::prompt_library`——
//! infra 不反向依赖 domain，种子由 app 启动时携带 domain 目录调
//! [`seed_builtins`]。
//!
//! **一致性**：`revision` 乐观并发（写入带 expected_revision，冲突返回
//! [`PromptStoreError::Conflict`]，禁止静默覆盖）；内置条目删除由本层强制
//! 拒绝（[`PromptStoreError::BuiltinLocked`]），不只依赖上层校验。

use sqlx::{Row, SqlitePool};

/// 提示词作用域（§8.1：chat 供 Chat 插入，editor 供编辑器预设）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PromptScope {
    Chat,
    Editor,
}

impl PromptScope {
    pub fn as_str(self) -> &'static str {
        match self {
            PromptScope::Chat => "chat",
            PromptScope::Editor => "editor",
        }
    }

    /// 从存储字符串解析（未知值报错，不静默落默认）。
    pub fn parse(value: &str) -> Result<Self, String> {
        match value {
            "chat" => Ok(Self::Chat),
            "editor" => Ok(Self::Editor),
            other => Err(format!("未知的提示词作用域: {other}")),
        }
    }
}

/// 行投影（含有效内容解析所需的原样列；解析规则在 domain）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct PromptLibraryRow {
    pub id: String,
    pub name: String,
    /// 内置=随版本默认内容；自建=用户正文。
    pub content: String,
    /// 仅内置条目：用户覆盖；None=未覆盖。
    pub override_content: Option<String>,
    pub scope: PromptScope,
    pub builtin: bool,
    pub sort_order: i64,
    pub hidden: bool,
    pub revision: i64,
    pub created_at: i64,
    pub updated_at: i64,
}

/// 存储层错误（上层投影为结构化 CommandError code）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PromptStoreError {
    #[error("条目不存在")]
    NotFound,
    #[error("版本冲突：条目已被其他窗口修改，请重新载入")]
    Conflict,
    #[error("内置条目不允许删除")]
    BuiltinLocked,
    #[error("仅内置条目支持恢复默认")]
    NotBuiltin,
    #[error("{0}")]
    Db(String),
}

impl From<sqlx::Error> for PromptStoreError {
    fn from(e: sqlx::Error) -> Self {
        Self::Db(e.to_string())
    }
}

/// 建表（`CREATE TABLE IF NOT EXISTS`，§8.1 DDL 原样）。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS prompt_library (
          id               TEXT PRIMARY KEY,
          name             TEXT NOT NULL,
          content          TEXT NOT NULL,
          override_content TEXT,
          scope            TEXT NOT NULL DEFAULT 'chat' CHECK (scope IN ('chat','editor')),
          builtin          INTEGER NOT NULL DEFAULT 0,
          sort_order       INTEGER NOT NULL DEFAULT 0,
          hidden           INTEGER NOT NULL DEFAULT 0,
          revision         INTEGER NOT NULL DEFAULT 1,
          created_at       INTEGER NOT NULL,
          updated_at       INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

fn map_row(row: &sqlx::sqlite::SqliteRow) -> Result<PromptLibraryRow, PromptStoreError> {
    Ok(PromptLibraryRow {
        id: row.try_get("id").unwrap_or_default(),
        name: row.try_get("name").unwrap_or_default(),
        content: row.try_get("content").unwrap_or_default(),
        override_content: row.try_get("override_content").unwrap_or(None),
        scope: PromptScope::parse(
            row.try_get::<String, _>("scope")
                .unwrap_or_default()
                .as_str(),
        )
        .map_err(PromptStoreError::Db)?,
        builtin: row.try_get::<i64, _>("builtin").unwrap_or(0) != 0,
        sort_order: row.try_get("sort_order").unwrap_or(0),
        hidden: row.try_get::<i64, _>("hidden").unwrap_or(0) != 0,
        revision: row.try_get("revision").unwrap_or(1),
        created_at: row.try_get("created_at").unwrap_or(0),
        updated_at: row.try_get("updated_at").unwrap_or(0),
    })
}

/// 同步落地：按 id upsert 展示字段（revision 自增、时间戳刷新；不触发
/// CAS——冲突语义由同步引擎的冻结计划承担）。内置条目的 `content`
/// 随版本提供，不接受同步覆盖；`builtin` 标志以本机行为准不改动。
/// 返回是否实际写入。
pub async fn upsert_sync(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    content: Option<&str>,
    override_content: Option<&str>,
    scope: &str,
    sort_order: i64,
    hidden: bool,
) -> Result<bool, PromptStoreError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let result = sqlx::query(
        "INSERT INTO prompt_library (id, name, content, override_content, scope, builtin,          sort_order, hidden, revision, created_at, updated_at)          VALUES (?1, ?2, COALESCE(?3, ''), ?4, ?5, 0, ?6, ?7, 1, ?8, ?8)          ON CONFLICT(id) DO UPDATE SET            name = ?2,            content = CASE WHEN ?3 IS NULL THEN prompt_library.content ELSE ?3 END,            override_content = ?4,            sort_order = ?6,            hidden = ?7,            revision = prompt_library.revision + 1,            updated_at = ?8",
    )
    .bind(id)
    .bind(name)
    .bind(content)
    .bind(override_content)
    .bind(scope)
    .bind(sort_order)
    .bind(hidden)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| PromptStoreError::Db(e.to_string()))?;
    Ok(result.rows_affected() > 0)
}

/// 列出条目（sort_order 升序 → name 稳定次序）；`scope` 为 None 时全量。
pub async fn list(
    pool: &SqlitePool,
    scope: Option<PromptScope>,
) -> Result<Vec<PromptLibraryRow>, PromptStoreError> {
    let rows = match scope {
        None => {
            sqlx::query(
                "SELECT * FROM prompt_library ORDER BY sort_order ASC, name ASC, id ASC",
            )
            .fetch_all(pool)
            .await?
        }
        Some(scope) => {
            sqlx::query(
                "SELECT * FROM prompt_library WHERE scope = ?1
                 ORDER BY sort_order ASC, name ASC, id ASC",
            )
            .bind(scope.as_str())
            .fetch_all(pool)
            .await?
        }
    };
    rows.iter().map(map_row).collect()
}

/// 按 id 取单条。
pub async fn get(pool: &SqlitePool, id: &str) -> Result<PromptLibraryRow, PromptStoreError> {
    let row = sqlx::query("SELECT * FROM prompt_library WHERE id = ?1")
        .bind(id)
        .fetch_optional(pool)
        .await?
        .ok_or(PromptStoreError::NotFound)?;
    map_row(&row)
}

/// 新建自建条目（UUID id 由上层生成）。
pub async fn insert(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    content: &str,
    scope: PromptScope,
    sort_order: i64,
) -> Result<PromptLibraryRow, PromptStoreError> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO prompt_library
           (id, name, content, override_content, scope, builtin, sort_order, hidden, revision, created_at, updated_at)
         VALUES (?1, ?2, ?3, NULL, ?4, 0, ?5, 0, 1, ?6, ?6)",
    )
    .bind(id)
    .bind(name)
    .bind(content)
    .bind(scope.as_str())
    .bind(sort_order)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| {
        // 主键冲突 = 复制到已存在 id / 重放，按冲突语义报
        if e.as_database_error().is_some_and(|db| db.is_unique_violation()) {
            PromptStoreError::Conflict
        } else {
            PromptStoreError::Db(e.to_string())
        }
    })?;
    get(pool, id).await
}

/// 更新条目（乐观并发）。
///
/// - 自建：可改 name/content/scope；
/// - 内置：只允许 name/override_content/hidden/sort_order，content 由种子
///   随版本升级（§3.7），本入口拒绝改内置 content，防绕过目录升级语义。
/// - `expected_revision` 为 None 时按当前版本无条件写（新建流程用）；
///   Some 且不匹配 → [`PromptStoreError::Conflict`]。
#[allow(clippy::too_many_arguments)]
pub async fn update(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    content: Option<&str>,
    override_content: Option<Option<&str>>,
    hidden: Option<bool>,
    sort_order: Option<i64>,
    expected_revision: Option<i64>,
) -> Result<PromptLibraryRow, PromptStoreError> {
    let existing = get(pool, id).await?;
    if existing.builtin && content.is_some_and(|c| c != existing.content) {
        return Err(PromptStoreError::BuiltinLocked);
    }

    let now = chrono::Utc::now().timestamp();
    let new_name = name;
    let new_content = content.unwrap_or(existing.content.as_str());
    let new_override = match override_content {
        Some(value) => value.map(str::to_string),
        None => existing.override_content.clone(),
    };
    let new_hidden = hidden.unwrap_or(existing.hidden) as i64;
    let new_sort = sort_order.unwrap_or(existing.sort_order);

    let rows_affected = sqlx::query(
        "UPDATE prompt_library
         SET name = ?2, content = ?3, override_content = ?4, hidden = ?5,
             sort_order = ?6, revision = revision + 1, updated_at = ?7
         WHERE id = ?1 AND (?8 IS NULL OR revision = ?8)",
    )
    .bind(id)
    .bind(new_name)
    .bind(new_content)
    .bind(new_override)
    .bind(new_hidden)
    .bind(new_sort)
    .bind(now)
    .bind(expected_revision)
    .execute(pool)
    .await?
    .rows_affected();

    if rows_affected == 0 {
        // 条目在 get 后被并发删除，或 revision 不匹配
        if get(pool, id).await.is_ok() {
            return Err(PromptStoreError::Conflict);
        }
        return Err(PromptStoreError::NotFound);
    }
    get(pool, id).await
}

/// 删除条目（内置强制拒绝，后端铁则 §3.7）。
pub async fn delete(pool: &SqlitePool, id: &str) -> Result<(), PromptStoreError> {
    let existing = get(pool, id).await?;
    if existing.builtin {
        return Err(PromptStoreError::BuiltinLocked);
    }
    sqlx::query("DELETE FROM prompt_library WHERE id = ?1 AND builtin = 0")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(())
}

/// 恢复默认：清除 override_content（仅内置条目），revision +1。
///
/// `expected_revision` 语义与 [`update`] 一致：None 无条件写，Some 且不匹配
/// → [`PromptStoreError::Conflict`]（与并发编辑的 A 窗口互斥，spec §7.5）。
pub async fn reset_override(
    pool: &SqlitePool,
    id: &str,
    expected_revision: Option<i64>,
) -> Result<PromptLibraryRow, PromptStoreError> {
    let existing = get(pool, id).await?;
    if !existing.builtin {
        return Err(PromptStoreError::NotBuiltin);
    }
    let now = chrono::Utc::now().timestamp();
    let rows_affected = sqlx::query(
        "UPDATE prompt_library
         SET override_content = NULL, revision = revision + 1, updated_at = ?2
         WHERE id = ?1 AND (?3 IS NULL OR revision = ?3)",
    )
    .bind(id)
    .bind(now)
    .bind(expected_revision)
    .execute(pool)
    .await?
    .rows_affected();
    if rows_affected == 0 {
        if get(pool, id).await.is_ok() {
            return Err(PromptStoreError::Conflict);
        }
        return Err(PromptStoreError::NotFound);
    }
    get(pool, id).await
}

/// 按给定 id 顺序重排（全量 CAS 无关操作；只更新变化的行，受影响行 revision +1）。
///
/// 未出现在列表中的条目 sort_order 不变（防误清）；顺序值 = 数组下标。
pub async fn reorder(
    pool: &SqlitePool,
    ordered_ids: &[String],
) -> Result<(), PromptStoreError> {
    let mut tx = pool.begin().await?;
    for (index, id) in ordered_ids.iter().enumerate() {
        sqlx::query(
            "UPDATE prompt_library
             SET sort_order = ?2, revision = revision + 1, updated_at = ?3
             WHERE id = ?1 AND sort_order != ?2",
        )
        .bind(id)
        .bind(index as i64)
        .bind(chrono::Utc::now().timestamp())
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// 种子条目（app 启动携带 domain 目录调用；幂等）。
pub struct SeedEntry {
    pub id: &'static str,
    pub name: &'static str,
    pub content: &'static str,
    pub scope: PromptScope,
    pub sort_order: i64,
}

/// 幂等种子（§3.7：补元数据、不覆盖修改）：
/// - 不存在 → 插入完整行；
/// - 已存在 → 只把 content 升级到目录默认值（内置 content=随版本默认内容），
///   **不动** name/override_content/hidden/sort_order/revision 之外的任何用户
///   可编辑字段；content 变化才推进 revision/updated_at。
pub async fn seed_builtins(
    pool: &SqlitePool,
    entries: &[SeedEntry],
) -> Result<(), PromptStoreError> {
    let now = chrono::Utc::now().timestamp();
    for entry in entries {
        sqlx::query(
            "INSERT INTO prompt_library
               (id, name, content, override_content, scope, builtin, sort_order, hidden, revision, created_at, updated_at)
             VALUES (?1, ?2, ?3, NULL, ?4, 1, ?5, 0, 1, ?6, ?6)
             ON CONFLICT(id) DO UPDATE SET
               content = excluded.content,
               revision = CASE WHEN prompt_library.content != excluded.content
                          THEN prompt_library.revision + 1 ELSE prompt_library.revision END,
               updated_at = CASE WHEN prompt_library.content != excluded.content
                             THEN ?6 ELSE prompt_library.updated_at END",
        )
        .bind(entry.id)
        .bind(entry.name)
        .bind(entry.content)
        .bind(entry.scope.as_str())
        .bind(entry.sort_order)
        .bind(now)
        .execute(pool)
        .await?;
    }
    Ok(())
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn temp_pool() -> SqlitePool {
        let dir = std::env::temp_dir().join(format!(
            "blink-promptlib-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .subsec_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let options = sqlx::sqlite::SqliteConnectOptions::new()
            .filename(dir.join("test.db"))
            .create_if_missing(true);
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await
            .unwrap();
        init_db(&pool).await.unwrap();
        pool
    }

    fn seed_fixture() -> Vec<SeedEntry> {
        vec![SeedEntry {
            id: "builtin.editor.tidy",
            name: "整理",
            content: "v1 默认内容",
            scope: PromptScope::Editor,
            sort_order: 0,
        }]
    }

    use sqlx::sqlite::SqlitePoolOptions;

    #[tokio::test]
    async fn seed_is_idempotent_and_upgrades_unoverridden_content() {
        let pool = temp_pool().await;
        seed_builtins(&pool, &seed_fixture()).await.unwrap();
        let row = get(&pool, "builtin.editor.tidy").await.unwrap();
        assert_eq!(row.content, "v1 默认内容");
        assert_eq!(row.revision, 1);

        // 覆盖后再种子：content 升级、override 保留、hidden/sort 不动
        update(
            &pool,
            "builtin.editor.tidy",
            "我的整理",
            None,
            Some(Some("用户覆盖")),
            Some(true),
            Some(7),
            Some(1),
        )
        .await
        .unwrap();
        let mut v2 = seed_fixture();
        v2[0].content = "v2 默认内容";
        seed_builtins(&pool, &v2).await.unwrap();
        let row = get(&pool, "builtin.editor.tidy").await.unwrap();
        assert_eq!(row.content, "v2 默认内容", "未覆盖内置项随目录升级（content 列本身升级）");
        assert_eq!(row.override_content.as_deref(), Some("用户覆盖"), "覆盖不丢");
        assert_eq!(row.name, "我的整理", "名字不被种子覆盖");
        assert!(row.hidden && row.sort_order == 7, "hidden/sort 不被种子覆盖");
        assert_eq!(row.revision, 3, "content 变化推进 revision（1→2→3）");

        // 再跑一次 v2：内容不变 → revision 不推进
        seed_builtins(&pool, &v2).await.unwrap();
        let again = get(&pool, "builtin.editor.tidy").await.unwrap();
        assert_eq!(again.revision, 3);
    }

    #[tokio::test]
    async fn update_uses_revision_cas_and_protects_builtin_content() {
        let pool = temp_pool().await;
        seed_builtins(&pool, &seed_fixture()).await.unwrap();

        // 旧 revision 写入 → Conflict
        let err = update(
            &pool,
            "builtin.editor.tidy",
            "改名",
            None,
            Some(Some("覆盖")),
            None,
            None,
            Some(99),
        )
        .await
        .unwrap_err();
        assert_eq!(err, PromptStoreError::Conflict);

        // 正确 revision 写入成功且推进
        let row = update(
            &pool,
            "builtin.editor.tidy",
            "改名",
            None,
            Some(Some("覆盖")),
            None,
            None,
            Some(1),
        )
        .await
        .unwrap();
        assert_eq!(row.revision, 2);
        assert_eq!(row.name, "改名");

        // 内置 content 直改被拒（content 与目录不同）
        let err = update(
            &pool,
            "builtin.editor.tidy",
            "改名",
            Some("篡改默认"),
            None,
            None,
            None,
            Some(2),
        )
        .await
        .unwrap_err();
        assert_eq!(err, PromptStoreError::BuiltinLocked);
    }

    #[tokio::test]
    async fn delete_and_reset_enforce_builtin_rules() {
        let pool = temp_pool().await;
        seed_builtins(&pool, &seed_fixture()).await.unwrap();
        assert_eq!(
            delete(&pool, "builtin.editor.tidy").await.unwrap_err(),
            PromptStoreError::BuiltinLocked
        );

        update(
            &pool,
            "builtin.editor.tidy",
            "整理",
            None,
            Some(Some("覆盖")),
            None,
            None,
            Some(1),
        )
        .await
        .unwrap();
        let updated = get(&pool, "builtin.editor.tidy").await.unwrap();
        let row = reset_override(&pool, "builtin.editor.tidy", Some(updated.revision))
            .await
            .unwrap();
        assert_eq!(row.override_content, None, "恢复默认清除覆盖");
        assert!(row.revision >= 3);
        // revision CAS：陈旧版本号拒绝
        assert_eq!(
            reset_override(&pool, "builtin.editor.tidy", Some(1))
                .await
                .unwrap_err(),
            PromptStoreError::Conflict
        );

        // 自建：可删；reset 报 NotBuiltin
        insert(&pool, "uuid-1", "自定义", "正文", PromptScope::Chat, 10)
            .await
            .unwrap();
        assert_eq!(
            reset_override(&pool, "uuid-1", None).await.unwrap_err(),
            PromptStoreError::NotBuiltin
        );
        delete(&pool, "uuid-1").await.unwrap();
        assert_eq!(
            get(&pool, "uuid-1").await.unwrap_err(),
            PromptStoreError::NotFound
        );
    }

    #[tokio::test]
    async fn list_orders_by_sort_then_name_and_filters_scope() {
        let pool = temp_pool().await;
        seed_builtins(&pool, &seed_fixture()).await.unwrap();
        insert(&pool, "a", "甲", "x", PromptScope::Chat, 5)
            .await
            .unwrap();
        insert(&pool, "b", "乙", "y", PromptScope::Editor, 0)
            .await
            .unwrap();

        let all = list(&pool, None).await.unwrap();
        assert_eq!(all.len(), 3);
        // sort_order 0（tidy、乙）→ 5（甲）；同序按 name 二进制稳定次序
        //（UTF-8 字节序「乙」<「整」，b 在前）
        assert_eq!(all[0].id, "b");
        assert_eq!(all[1].id, "builtin.editor.tidy");
        assert_eq!(all[2].id, "a");

        let editor_only = list(&pool, Some(PromptScope::Editor)).await.unwrap();
        assert_eq!(editor_only.len(), 2);
        assert!(editor_only.iter().all(|r| r.scope == PromptScope::Editor));
    }

    #[tokio::test]
    async fn reorder_updates_changed_rows_only() {
        let pool = temp_pool().await;
        insert(&pool, "a", "甲", "x", PromptScope::Chat, 0)
            .await
            .unwrap();
        insert(&pool, "b", "乙", "y", PromptScope::Chat, 1)
            .await
            .unwrap();
        // 交换顺序
        reorder(&pool, &["b".into(), "a".into()]).await.unwrap();
        let rows = list(&pool, None).await.unwrap();
        assert_eq!(rows[0].id, "b");
        assert_eq!(rows[1].id, "a");
        assert_eq!(rows[0].revision, 2, "变化的行推进 revision");
    }
}
