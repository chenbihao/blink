//! 对话持久化数据访问层（0.12.3 Phase A + 0.12.6 分组）。
//!
//! `conversations` / `messages` / `conversation_groups` 表存于 AI 库（`blink_ai.db`）。
//!
//! **设计原则**：
//! - `messages.content` 列存 `serde_json::to_string(&rig_core::completion::Message)`，
//!   完整保留 text / tool_call / tool_result。
//! - `role` 列从 Message 变体提取（system/user/assistant），供查询用。
//! - 删除 conversation 时级联删除 messages（SQLite 默认不启用 FK，手动 DELETE）。
//! - 滑动窗口策略在 `SqliteConversationMemory::load` 中实现（取最近 N 条），
//!   持久化层保存完整历史，不删除旧数据。
//!
//! **0.12.6 分组**：
//! - `conversation_groups` 表支持 `parent_id` 多层嵌套。
//! - `conversations.group_id` 列（NULL = 默认/未分组，无系统提示词）。
//! - 删除分组时：组内对话移至默认（group_id = NULL），子分组 re-parent 到被删分组的父级。

use sqlx::SqlitePool;

/// 一条对话记录（列表展示用）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct Conversation {
    pub id: String,
    pub title: Option<String>,
    pub created_at: i64,
    pub last_active_at: i64,
    /// 消息条数（join 聚合，供列表预览用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<i64>,
    /// 所属分组 ID（NULL = 默认/未分组）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
}

/// 一个对话分组（0.12.6）。
///
/// 支持多层嵌套（`parent_id`），每分组可配独立系统提示词。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConversationGroup {
    pub id: String,
    pub name: String,
    /// 分组级系统提示词（NULL = 无自定义提示词，用默认）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_prompt: Option<String>,
    /// 父分组 ID（NULL = 顶层）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    /// 同级排序权重（小在前）。
    pub sort_order: i64,
    /// 折叠状态（true = 展开，false = 折叠）。
    pub expanded: bool,
    pub created_at: i64,
}

/// 初始化 `conversations` + `messages` + `conversation_groups` 表。
///
/// 0.12.6 新增 `conversation_groups` 表 + `conversations.group_id` 列迁移。
/// 迁移用 `PRAGMA table_info` 检测列是否存在，不存在则 `ALTER TABLE ADD COLUMN`。
pub async fn init_db(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS conversations (
            id TEXT PRIMARY KEY,
            title TEXT,
            created_at INTEGER NOT NULL,
            last_active_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE TABLE IF NOT EXISTS messages (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            conversation_id TEXT NOT NULL,
            role TEXT NOT NULL,
            content TEXT NOT NULL,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 0.12.6: conversation_groups 表
    sqlx::query(
        "CREATE TABLE IF NOT EXISTS conversation_groups (
            id TEXT PRIMARY KEY,
            name TEXT NOT NULL,
            system_prompt TEXT,
            parent_id TEXT,
            sort_order INTEGER NOT NULL DEFAULT 0,
            expanded INTEGER NOT NULL DEFAULT 1,
            created_at INTEGER NOT NULL
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, id)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_conversations_last_active ON conversations(last_active_at DESC)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query(
        "CREATE INDEX IF NOT EXISTS idx_conv_groups_parent ON conversation_groups(parent_id, sort_order)",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    // 0.12.6 迁移：conversations 表加 group_id 列（若不存在）
    migrate_add_group_id_column(pool).await?;

    tracing::debug!("conversations + messages + conversation_groups 表已初始化");
    Ok(())
}

/// 检测 `conversations` 表是否有 `group_id` 列，没有则 `ALTER TABLE ADD COLUMN`。
async fn migrate_add_group_id_column(pool: &SqlitePool) -> Result<(), String> {
    // PRAGMA table_info 返回 6 列（cid, name, type, notnull, dflt_value, pk），
    // 用表值函数只取 name 列避免类型不匹配
    let columns: Vec<(String,)> =
        sqlx::query_as("SELECT name FROM pragma_table_info('conversations')")
            .fetch_all(pool)
            .await
            .map_err(|e| e.to_string())?;

    let has_group_id = columns.iter().any(|(name,)| name == "group_id");
    if !has_group_id {
        sqlx::query(sqlx::AssertSqlSafe(
            "ALTER TABLE conversations ADD COLUMN group_id TEXT".to_string(),
        ))
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
        tracing::info!("conversations 表已迁移：新增 group_id 列");
    }
    Ok(())
}

/// 创建对话记录（INSERT OR IGNORE——已存在时不报错）。
///
/// `title` 为 None 时用空标题（后续可由 rename 更新）。
///
/// 0.12.8: 如果 INSERT 被 IGNORE（记录已存在）且新 title 非空而旧 title 为空，
/// 则额外 UPDATE 补写标题——避免 `set_conversation_group` 先创建空标题记录后，
/// `memory.append` 的 `extract_title` 被静默丢弃。
pub async fn create_conversation(
pool: &SqlitePool,
id: &str,
title: Option<&str>,
) -> Result<(), String> {
let now = chrono::Utc::now().timestamp();
let result = sqlx::query(
"INSERT OR IGNORE INTO conversations (id, title, created_at, last_active_at) VALUES (?1, ?2, ?3, ?3)",
)
.bind(id)
.bind(title)
.bind(now)
.execute(pool)
.await
.map_err(|e| e.to_string())?;

// INSERT 被 IGNORE（rows_affected = 0 表示记录已存在）时，
// 如果新 title 非空，尝试补写到空标题的记录
if result.rows_affected() == 0 {
    if let Some(t) = title {
        if !t.is_empty() {
            sqlx::query(
                "UPDATE conversations SET title = ?1 WHERE id = ?2 AND (title IS NULL OR title = '')",
            )
            .bind(t)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
        }
    }
}
Ok(())
}

/// 更新对话标题。
pub async fn rename_conversation(
    pool: &SqlitePool,
    id: &str,
    title: &str,
) -> Result<bool, String> {
    let result = sqlx::query("UPDATE conversations SET title = ?1 WHERE id = ?2")
        .bind(title)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 更新对话的 last_active_at（每次 append 时调用）。
pub async fn touch_conversation(pool: &SqlitePool, id: &str) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query("UPDATE conversations SET last_active_at = ?1 WHERE id = ?2")
        .bind(now)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 列出所有对话（按 last_active_at 倒序），含消息条数和 group_id。
pub async fn list_conversations(pool: &SqlitePool) -> Result<Vec<Conversation>, String> {
    let rows: Vec<(String, Option<String>, i64, i64, Option<String>, i64)> = sqlx::query_as(
        "SELECT c.id, c.title, c.created_at, c.last_active_at, c.group_id, \
         (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS msg_count \
         FROM conversations c ORDER BY c.last_active_at DESC, c.rowid DESC",
    )
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, "list_conversations 查询失败");
        e.to_string()
    })?;

    Ok(rows.into_iter()
        .map(
            |(id, title, created_at, last_active_at, group_id, msg_count)| Conversation {
                id,
                title,
                created_at,
                last_active_at,
                message_count: Some(msg_count),
                group_id,
            },
        )
        .collect())
}

/// 删除对话（级联删除 messages——SQLite 默认不启用 FK，手动 DELETE）。
pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> Result<bool, String> {
    // 先删 messages，再删 conversation
    sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    let result = sqlx::query("DELETE FROM conversations WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 插入一条消息（content 为序列化的 rig Message JSON）。
pub async fn append_message(
    pool: &SqlitePool,
    conversation_id: &str,
    role: &str,
    content: &str,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT INTO messages (conversation_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 加载对话的最近 N 条消息（按 id 升序返回，即时间顺序）。
///
/// 滑动窗口：只返回最后 `limit` 条，但 DB 保留完整历史。
pub async fn load_recent_messages(
    pool: &SqlitePool,
    conversation_id: &str,
    limit: i64,
) -> Result<Vec<(String, String)>, String> {
    // 子查询取最后 N 条（id DESC LIMIT），外层按 id ASC 排序恢复时间顺序
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM (
            SELECT id, role, content FROM messages
            WHERE conversation_id = ?1 ORDER BY id DESC LIMIT ?2
        ) ORDER BY id ASC",
    )
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// 清空对话的所有消息（不删 conversation 记录本身）。
pub async fn clear_messages(pool: &SqlitePool, conversation_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
        .bind(conversation_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 截断对话消息——保留前 `keep_count` 条，删除其余（0.12.5 §5.5）。
///
/// 用于消息编辑重发：用户编辑第 N 条消息后，保留前 N 条消息（索引 0 到 N-1），
/// 删除第 N 条及之后的所有消息，然后重新调 `chat_prompt`。
pub async fn truncate_messages(
    pool: &SqlitePool,
    conversation_id: &str,
    keep_count: i64,
) -> Result<(), String> {
    let ids: Vec<i64> = sqlx::query_scalar(
        "SELECT id FROM messages WHERE conversation_id = ?1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;

    if keep_count >= ids.len() as i64 {
        return Ok(()); // 无需截断
    }

    let start_id = ids[keep_count as usize];

    sqlx::query("DELETE FROM messages WHERE conversation_id = ?1 AND id >= ?2")
        .bind(conversation_id)
        .bind(start_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    Ok(())
}

/// 加载对话的**全部**消息（按 id 升序，即时间顺序）。
///
/// 供 `get_chat_messages` IPC 加载历史用——展示用全量，agent context 用滑动窗口。
/// 0.12.7：返回 `(role, content, created_at)` 三元组，前端据此插入时间分隔符。
pub async fn load_all_messages(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<(String, String, i64)>, String> {
    let rows: Vec<(String, String, i64)> = sqlx::query_as(
        "SELECT role, content, created_at FROM messages WHERE conversation_id = ?1 ORDER BY id ASC",
    )
    .bind(conversation_id)
    .fetch_all(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(rows)
}

/// 对话总数（存储页统计用）。
pub async fn count_conversations(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// 消息总数（存储页统计用）。
pub async fn count_messages(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

// ── 分组 CRUD（0.12.6）─────────────────────────────────────────────────────────

/// 创建分组。`parent_id` 为 None 表示顶层分组。
///
/// `sort_order` 自动设为同级最大 +1。
pub async fn create_group(
    pool: &SqlitePool,
    id: &str,
    name: &str,
    parent_id: Option<&str>,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    // 同级最大 sort_order + 1（IS ?1 在绑 NULL 时行为不确定，分支查询更可靠）
    // 同级最大 sort_order + 1（COALESCE 将空表 NULL 转为 -1，避免 sqlx NULL 解码歧义）
    let max_order: i64 = match parent_id {
        Some(pid) => {
            sqlx::query_scalar(
                "SELECT COALESCE(MAX(sort_order), -1) FROM conversation_groups WHERE parent_id = ?1",
            )
            .bind(pid)
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?
        }
        None => {
            sqlx::query_scalar(
                "SELECT COALESCE(MAX(sort_order), -1) FROM conversation_groups WHERE parent_id IS NULL",
            )
            .fetch_one(pool)
            .await
            .map_err(|e| e.to_string())?
        }
    };
    let sort_order = max_order + 1;

    sqlx::query(
        "INSERT INTO conversation_groups (id, name, system_prompt, parent_id, sort_order, expanded, created_at) \
         VALUES (?1, ?2, NULL, ?3, ?4, 1, ?5)",
    )
    .bind(id)
    .bind(name)
    .bind(parent_id)
    .bind(sort_order)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(())
}

/// 重命名分组。
pub async fn rename_group(
    pool: &SqlitePool,
    id: &str,
    name: &str,
) -> Result<bool, String> {
    let result = sqlx::query("UPDATE conversation_groups SET name = ?1 WHERE id = ?2")
        .bind(name)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 删除分组。
///
/// **行为**：
/// 1. 组内对话移至默认（group_id = NULL）
/// 2. 子分组 re-parent 到被删分组的父级（保留子树结构）
/// 3. 删除分组记录
pub async fn delete_group(pool: &SqlitePool, id: &str) -> Result<bool, String> {
    // 1. 获取被删分组的 parent_id
    let parent_id: Option<Option<String>> = sqlx::query_scalar(
        "SELECT parent_id FROM conversation_groups WHERE id = ?1",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    let Some(parent_id) = parent_id else {
        return Ok(false); // 分组不存在
    };

    // 2-4 在事务中执行：任一步骤失败则回滚，防止数据不一致
    let mut tx = pool.begin().await.map_err(|e| e.to_string())?;

    // 2. 组内对话移至默认（group_id = NULL）
    sqlx::query("UPDATE conversations SET group_id = NULL WHERE group_id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // 3. 子分组 re-parent 到被删分组的父级
    sqlx::query("UPDATE conversation_groups SET parent_id = ?1 WHERE parent_id = ?2")
        .bind(parent_id.as_deref()) // 被删分组的父级（可能为 NULL）
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    // 4. 删除分组记录
    let result = sqlx::query("DELETE FROM conversation_groups WHERE id = ?1")
        .bind(id)
        .execute(&mut *tx)
        .await
        .map_err(|e| e.to_string())?;

    tx.commit().await.map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 列出所有分组（按 sort_order 升序，含 parent_id 供前端构建树）。
pub async fn list_groups(pool: &SqlitePool) -> Vec<ConversationGroup> {
    let rows: Vec<(String, String, Option<String>, Option<String>, i64, i64, i64)> =
        sqlx::query_as(
            "SELECT id, name, system_prompt, parent_id, sort_order, expanded, created_at \
             FROM conversation_groups ORDER BY sort_order ASC, created_at ASC",
        )
        .fetch_all(pool)
        .await
        .unwrap_or_default();

    rows.into_iter()
        .map(
            |(id, name, system_prompt, parent_id, sort_order, expanded, created_at)| ConversationGroup {
                id,
                name,
                system_prompt,
                parent_id,
                sort_order,
                expanded: expanded != 0,
                created_at,
            },
        )
        .collect()
}

/// 更新分组的系统提示词。`prompt` 为 None 时清除。
pub async fn update_group_system_prompt(
    pool: &SqlitePool,
    id: &str,
    prompt: Option<&str>,
) -> Result<bool, String> {
    let result =
        sqlx::query("UPDATE conversation_groups SET system_prompt = ?1 WHERE id = ?2")
            .bind(prompt)
            .bind(id)
            .execute(pool)
            .await
            .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 设置分组的排序权重。
pub async fn set_group_sort_order(
    pool: &SqlitePool,
    id: &str,
    sort_order: i64,
) -> Result<(), String> {
    sqlx::query("UPDATE conversation_groups SET sort_order = ?1 WHERE id = ?2")
        .bind(sort_order)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 设置分组的折叠状态。
pub async fn set_group_expanded(
    pool: &SqlitePool,
    id: &str,
    expanded: bool,
) -> Result<(), String> {
    sqlx::query("UPDATE conversation_groups SET expanded = ?1 WHERE id = ?2")
        .bind(expanded as i64)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 设置对话所属分组。`group_id` 为 None 移至默认组。
///
/// 同时确保对话记录存在（INSERT OR IGNORE），避免 race condition。
pub async fn set_conversation_group(
    pool: &SqlitePool,
    conversation_id: &str,
    group_id: Option<&str>,
) -> Result<(), String> {
    // 确保对话记录存在（INSERT OR IGNORE，已存在时不报错）
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR IGNORE INTO conversations (id, title, created_at, last_active_at) VALUES (?1, '', ?2, ?2)",
    )
    .bind(conversation_id)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    sqlx::query("UPDATE conversations SET group_id = ?1 WHERE id = ?2")
        .bind(group_id)
        .bind(conversation_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// 根据 conversation_id 查询其所属分组的系统提示词。
///
/// `group_id = NULL`（默认组）→ 返回 None。
/// `group_id` 不存在（被删后遗留）→ 返回 None。
pub async fn get_effective_system_prompt(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<String>, String> {
    let prompt: Option<Option<String>> = sqlx::query_scalar(
        "SELECT g.system_prompt \
         FROM conversations c \
         LEFT JOIN conversation_groups g ON c.group_id = g.id \
         WHERE c.id = ?1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    // prompt = None: 对话不存在或 group_id 为 NULL
    // prompt = Some(None): 分组存在但无 system_prompt
    // prompt = Some(Some(s)): 分组有 system_prompt
    Ok(prompt.unwrap_or(None))
}

/// 按 group_id 直接查询分组的系统提示词（0.12.8）。
///
/// 与 `get_effective_system_prompt`（按 conversation_id 查）不同，此函数直接按
/// group_id 查询，供 `chat_prompt` 在持久化分组之前获取系统提示词——避免副作用先于
/// 校验的问题（set_conversation_group 移到 prompt 成功后才执行）。
///
/// `group_id = None` → 返回 None（默认组无系统提示词）。
/// `group_id` 不存在 → 返回 None。
pub async fn get_group_system_prompt(
    pool: &SqlitePool,
    group_id: Option<&str>,
) -> Result<Option<String>, String> {
    let gid = match group_id {
        Some(g) if !g.is_empty() => g,
        _ => return Ok(None),
    };
    let prompt: Option<Option<String>> = sqlx::query_scalar(
        "SELECT system_prompt FROM conversation_groups WHERE id = ?1",
    )
    .bind(gid)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(prompt.unwrap_or(None))
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        init_db(&pool).await.expect("failed to init tables");
        pool
    }

    #[tokio::test]
    async fn create_and_list_conversation() {
        let pool = setup_pool().await;

        create_conversation(&pool, "c1", Some("Hello")).await.unwrap();
        create_conversation(&pool, "c2", Some("World")).await.unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs.len(), 2);
        // 按 last_active_at 倒序——c2 后创建所以在前
        assert_eq!(convs[0].id, "c2");
        assert_eq!(convs[1].id, "c1");
    }

    #[tokio::test]
    async fn create_conversation_is_idempotent() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Title 1")).await.unwrap();
        // 重复创建不报错
        create_conversation(&pool, "c1", Some("Title 2")).await.unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs.len(), 1);
        // INSERT OR IGNORE 保留原值
        assert_eq!(convs[0].title.as_deref(), Some("Title 1"));
    }

    #[tokio::test]
    async fn create_conversation_backfills_empty_title() {
        // 0.12.8: 如果记录已存在但标题为空，create_conversation 应补写标题
        let pool = setup_pool().await;

        // 先用空标题创建（模拟 set_conversation_group 的行为）
        create_conversation(&pool, "c1", None).await.unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].title, None);

        // 再用非空标题创建（模拟 memory.append 的 extract_title）
        create_conversation(&pool, "c1", Some("Real Title")).await.unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(
            convs[0].title.as_deref(),
            Some("Real Title"),
            "空标题应被补写"
        );

        // 已有非空标题时，不覆盖
        create_conversation(&pool, "c1", Some("Other Title")).await.unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].title.as_deref(), Some("Real Title"), "非空标题不覆盖");
    }

    #[tokio::test]
    async fn rename_updates_title() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Old")).await.unwrap();

        let updated = rename_conversation(&pool, "c1", "New").await.unwrap();
        assert!(updated);

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].title.as_deref(), Some("New"));
    }

    #[tokio::test]
    async fn delete_conversation_cascades_messages() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        append_message(&pool, "c1", "user", r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#).await.unwrap();
        append_message(&pool, "c1", "assistant", r#"{"role":"assistant","content":[{"type":"text","text":"hello"}]}"#).await.unwrap();

        assert_eq!(count_messages(&pool).await, 2);

        let deleted = delete_conversation(&pool, "c1").await.unwrap();
        assert!(deleted);

        assert_eq!(count_conversations(&pool).await, 0);
        assert_eq!(count_messages(&pool).await, 0);
    }

    #[tokio::test]
    async fn append_and_load_messages() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();

        for i in 0..5 {
            append_message(&pool, "c1", "user", &format!("{{\"role\":\"user\",\"content\":{i}}}")).await.unwrap();
        }

        let msgs = load_recent_messages(&pool, "c1", 3).await.unwrap();
        assert_eq!(msgs.len(), 3);
        // 滑动窗口取最后 3 条（id 3,4,5），按 id 升序返回
        assert!(msgs[0].1.contains("2"));
        assert!(msgs[2].1.contains("4"));
    }

    #[tokio::test]
    async fn clear_messages_keeps_conversation() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        append_message(&pool, "c1", "user", "content").await.unwrap();

        clear_messages(&pool, "c1").await.unwrap();
        assert_eq!(count_messages(&pool).await, 0);
        // conversation 记录仍在
        assert_eq!(count_conversations(&pool).await, 1);
    }

    // ── 0.12.5 §5.5: truncate_messages ──────────────────────────────────

    #[tokio::test]
    async fn truncate_messages_keeps_first_n() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        for i in 0..5 {
            append_message(&pool, "c1", "user", &format!("msg{i}")).await.unwrap();
        }
        assert_eq!(count_messages(&pool).await, 5);

        // 保留前 3 条，删除其余
        truncate_messages(&pool, "c1", 3).await.unwrap();
        assert_eq!(count_messages(&pool).await, 3);

        // 验证保留的是前 3 条（按 id 升序）
        let msgs = load_all_messages(&pool, "c1").await.unwrap();
        assert_eq!(msgs.len(), 3);
        assert!(msgs[0].1.contains("msg0"));
        assert!(msgs[2].1.contains("msg2"));
    }

    #[tokio::test]
    async fn truncate_messages_noop_when_keep_exceeds() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        append_message(&pool, "c1", "user", "msg0").await.unwrap();
        append_message(&pool, "c1", "user", "msg1").await.unwrap();

        // keep_count > 消息数 → 无操作
        truncate_messages(&pool, "c1", 10).await.unwrap();
        assert_eq!(count_messages(&pool).await, 2);
    }

    #[tokio::test]
    async fn load_empty_conversation_returns_empty() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Empty")).await.unwrap();
        let msgs = load_recent_messages(&pool, "c1", 20).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn load_nonexistent_conversation_returns_empty() {
        let pool = setup_pool().await;
        let msgs = load_recent_messages(&pool, "nonexistent", 20).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn init_db_is_idempotent() {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        init_db(&pool).await.expect("first init failed");
        init_db(&pool).await.expect("second init failed");
    }

    #[tokio::test]
    async fn message_count_in_list() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        append_message(&pool, "c1", "user", "content1").await.unwrap();
        append_message(&pool, "c1", "assistant", "content2").await.unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].message_count, Some(2));
    }

    // ── 0.12.6 分组 ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn group_create_and_list() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_group(&pool, "g2", "学习", None).await.unwrap();

        let groups = list_groups(&pool).await;
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].name, "工作");
        assert_eq!(groups[1].name, "学习");
        // sort_order 自动递增
        assert_eq!(groups[0].sort_order, 0);
        assert_eq!(groups[1].sort_order, 1);
    }

    #[tokio::test]
    async fn group_create_nested() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_group(&pool, "g2", "项目A", Some("g1")).await.unwrap();

        let groups = list_groups(&pool).await;
        assert_eq!(groups.len(), 2);
        let child = groups.iter().find(|g| g.id == "g2").unwrap();
        assert_eq!(child.parent_id.as_deref(), Some("g1"));
    }

    #[tokio::test]
    async fn group_rename() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "旧名", None).await.unwrap();
        let ok = rename_group(&pool, "g1", "新名").await.unwrap();
        assert!(ok);

        let groups = list_groups(&pool).await;
        assert_eq!(groups[0].name, "新名");
    }

    #[tokio::test]
    async fn group_delete_moves_conversations_to_default() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_conversation(&pool, "c1", Some("周报")).await.unwrap();
        set_conversation_group(&pool, "c1", Some("g1")).await.unwrap();

        // 确认对话在 g1 组
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].group_id.as_deref(), Some("g1"));

        // 删除分组 → 对话移至默认
        let ok = delete_group(&pool, "g1").await.unwrap();
        assert!(ok);

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].group_id, None);
    }

    #[tokio::test]
    async fn group_delete_reparents_children() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_group(&pool, "g2", "子", Some("g1")).await.unwrap();

        // 删除 g1 → g2 re-parent 到顶层
        delete_group(&pool, "g1").await.unwrap();

        let groups = list_groups(&pool).await;
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].id, "g2");
        assert_eq!(groups[0].parent_id, None);
    }

    #[tokio::test]
    async fn group_system_prompt_update() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "翻译", None).await.unwrap();

        // 设置系统提示词
        update_group_system_prompt(&pool, "g1", Some("你是翻译助手")).await.unwrap();
        let groups = list_groups(&pool).await;
        assert_eq!(groups[0].system_prompt.as_deref(), Some("你是翻译助手"));

        // 清除系统提示词
        update_group_system_prompt(&pool, "g1", None).await.unwrap();
        let groups = list_groups(&pool).await;
        assert_eq!(groups[0].system_prompt, None);
    }

    #[tokio::test]
    async fn set_conversation_group_creates_record_if_missing() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();

        // 对话记录不存在时 set_conversation_group 应自动创建
        set_conversation_group(&pool, "c1", Some("g1")).await.unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].id, "c1");
        assert_eq!(convs[0].group_id.as_deref(), Some("g1"));
    }

    #[tokio::test]
    async fn set_conversation_group_updates_existing() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_group(&pool, "g2", "学习", None).await.unwrap();
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        set_conversation_group(&pool, "c1", Some("g1")).await.unwrap();

        // 移到 g2
        set_conversation_group(&pool, "c1", Some("g2")).await.unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].group_id.as_deref(), Some("g2"));

        // 移到默认
        set_conversation_group(&pool, "c1", None).await.unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs[0].group_id, None);
    }

    #[tokio::test]
    async fn effective_system_prompt_query() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "翻译", None).await.unwrap();
        update_group_system_prompt(&pool, "g1", Some("你是翻译助手")).await.unwrap();
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        set_conversation_group(&pool, "c1", Some("g1")).await.unwrap();

        // 有分组的对话 → 返回分组系统提示词
        let prompt = get_effective_system_prompt(&pool, "c1").await.unwrap();
        assert_eq!(prompt.as_deref(), Some("你是翻译助手"));

        // 默认组对话 → None
        create_conversation(&pool, "c2", Some("Default")).await.unwrap();
        let prompt = get_effective_system_prompt(&pool, "c2").await.unwrap();
        assert_eq!(prompt, None);

        // 不存在的对话 → None
        let prompt = get_effective_system_prompt(&pool, "nonexistent").await.unwrap();
        assert_eq!(prompt, None);
    }

    #[tokio::test]
    async fn group_sort_order_auto_increment_per_parent() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "A", None).await.unwrap();
        create_group(&pool, "g2", "B", None).await.unwrap();
        create_group(&pool, "g3", "C", None).await.unwrap();
        create_group(&pool, "g4", "子A", Some("g1")).await.unwrap();
        create_group(&pool, "g5", "子B", Some("g1")).await.unwrap();

        let groups = list_groups(&pool).await;
        // 顶层: g1(0), g2(1), g3(2)
        let top: Vec<_> = groups.iter().filter(|g| g.parent_id.is_none()).collect();
        assert_eq!(top.len(), 3);
        assert_eq!(top[0].sort_order, 0);
        assert_eq!(top[1].sort_order, 1);
        assert_eq!(top[2].sort_order, 2);

        // g1 子级: g4(0), g5(1)
        let children: Vec<_> = groups.iter().filter(|g| g.parent_id.as_deref() == Some("g1")).collect();
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].sort_order, 0);
        assert_eq!(children[1].sort_order, 1);
    }

    #[tokio::test]
    async fn group_expanded_persistence() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();

        // 默认展开
        let groups = list_groups(&pool).await;
        assert!(groups[0].expanded);

        // 折叠
        set_group_expanded(&pool, "g1", false).await.unwrap();
        let groups = list_groups(&pool).await;
        assert!(!groups[0].expanded);

        // 展开
        set_group_expanded(&pool, "g1", true).await.unwrap();
        let groups = list_groups(&pool).await;
        assert!(groups[0].expanded);
    }

    #[tokio::test]
    async fn migrate_group_id_column_idempotent() {
        let pool = setup_pool().await;
        // init_db 已运行（setup_pool），group_id 列应已存在
        // 再次调用迁移函数应无操作、不报错
        migrate_add_group_id_column(&pool).await.unwrap();
        // 验证列存在：能设置 group_id 不报错
        create_conversation(&pool, "c1", Some("Test")).await.unwrap();
        set_conversation_group(&pool, "c1", None).await.unwrap();
    }

    #[tokio::test]
    async fn list_conversations_includes_group_id() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_conversation(&pool, "c1", Some("A")).await.unwrap();
        create_conversation(&pool, "c2", Some("B")).await.unwrap();
        set_conversation_group(&pool, "c1", Some("g1")).await.unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        let c1 = convs.iter().find(|c| c.id == "c1").unwrap();
        let c2 = convs.iter().find(|c| c.id == "c2").unwrap();
        assert_eq!(c1.group_id.as_deref(), Some("g1"));
        assert_eq!(c2.group_id, None);
    }
}
