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

    sqlx::query("CREATE INDEX IF NOT EXISTS idx_messages_conv ON messages(conversation_id, id)")
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

    // 0.13.2: memory_fts 全文检索虚拟表（trigram 分词器，中文友好）
    // 归档滑动窗口截断的旧消息，供 BM25 检索召回
    sqlx::query(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(
            content,
            conversation_id UNINDEXED,
            role UNINDEXED,
            content_hash UNINDEXED,
            tokenize = 'trigram'
        )",
    )
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    tracing::debug!("conversations + messages + conversation_groups + memory_fts 表已初始化");
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
    if result.rows_affected() == 0
        && let Some(t) = title
        && !t.is_empty()
    {
        sqlx::query(
            "UPDATE conversations SET title = ?1 WHERE id = ?2 AND (title IS NULL OR title = '')",
        )
        .bind(t)
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// 更新对话标题。
pub async fn rename_conversation(pool: &SqlitePool, id: &str, title: &str) -> Result<bool, String> {
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

/// `list_conversations` 的 SQL 行（id, title, created_at, last_active_at, group_id, msg_count）。
type ConversationRow = (String, Option<String>, i64, i64, Option<String>, i64);

/// 列出所有对话（按 last_active_at 倒序），含消息条数和 group_id。
pub async fn list_conversations(pool: &SqlitePool) -> Result<Vec<Conversation>, String> {
    let rows: Vec<ConversationRow> = sqlx::query_as(
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

    Ok(rows
        .into_iter()
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

/// 删除对话（级联删除 messages + memory_fts——SQLite 默认不启用 FK，手动 DELETE）。
pub async fn delete_conversation(pool: &SqlitePool, id: &str) -> Result<bool, String> {
    // 先删 messages，再删 memory_fts，最后删 conversation
    sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 0.13.2: 级联删除 FTS5 归档
    let _ = clear_memory_fts(pool, id).await;

    let result = sqlx::query("DELETE FROM conversations WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 插入一条消息（content 为序列化的 rig Message JSON），返回插入的消息 id。
pub async fn append_message(
    pool: &SqlitePool,
    conversation_id: &str,
    role: &str,
    content: &str,
) -> Result<i64, String> {
    let now = chrono::Utc::now().timestamp();
    let result = sqlx::query(
        "INSERT INTO messages (conversation_id, role, content, created_at) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(conversation_id)
    .bind(role)
    .bind(content)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(result.last_insert_rowid())
}

/// 加载对话的最后一条消息（role, content）。无消息时返回 None。
///
/// 供「发出即保存」去重使用：判断尾部是否已是相同 user 消息，避免重发产生重复行。
pub async fn load_last_message(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Option<(String, String)>, String> {
    let row: Option<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY id DESC LIMIT 1",
    )
    .bind(conversation_id)
    .fetch_optional(pool)
    .await
    .map_err(|e| e.to_string())?;
    Ok(row)
}

/// 更新指定消息的 content（流式增量写入部分 assistant 回复用）。返回是否命中。
pub async fn update_message_content(
    pool: &SqlitePool,
    id: i64,
    content: &str,
) -> Result<bool, String> {
    let result = sqlx::query("UPDATE messages SET content = ?2 WHERE id = ?1")
        .bind(id)
        .bind(content)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
}

/// 删除指定消息（append 合并实况回合时清理流式期间写出的部分回复）。
pub async fn delete_message_by_id(pool: &SqlitePool, id: i64) -> Result<bool, String> {
    let result = sqlx::query("DELETE FROM messages WHERE id = ?1")
        .bind(id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(result.rows_affected() > 0)
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
///
/// 0.13.2: 同时清理 memory_fts 中该对话的归档。
pub async fn clear_messages(pool: &SqlitePool, conversation_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM messages WHERE conversation_id = ?1")
        .bind(conversation_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    // 0.13.2: 同步清理 FTS5 归档
    let _ = clear_memory_fts(pool, conversation_id).await;

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
    let ids: Vec<i64> =
        sqlx::query_scalar("SELECT id FROM messages WHERE conversation_id = ?1 ORDER BY id ASC")
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

/// 对话总数。
pub async fn count_conversations(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM conversations")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// 消息总数。
pub async fn count_messages(pool: &SqlitePool) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM messages")
        .fetch_one(pool)
        .await
        .unwrap_or(0)
}

/// 清空全部对话（删除 conversations + messages + memory_fts，保留 conversation_groups）。
///
/// 与 `delete_conversation` 的单条级联不同，这里是全表清理：
/// messages -> memory_fts -> conversations，手动级联（SQLite 默认不启用 FK）。
pub async fn clear_all_conversations(pool: &SqlitePool) -> Result<(), String> {
    sqlx::query("DELETE FROM messages")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("DELETE FROM memory_fts")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;

    sqlx::query("DELETE FROM conversations")
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
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
        Some(pid) => sqlx::query_scalar(
            "SELECT COALESCE(MAX(sort_order), -1) FROM conversation_groups WHERE parent_id = ?1",
        )
        .bind(pid)
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?,
        None => sqlx::query_scalar(
            "SELECT COALESCE(MAX(sort_order), -1) FROM conversation_groups WHERE parent_id IS NULL",
        )
        .fetch_one(pool)
        .await
        .map_err(|e| e.to_string())?,
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
pub async fn rename_group(pool: &SqlitePool, id: &str, name: &str) -> Result<bool, String> {
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
    let parent_id: Option<Option<String>> =
        sqlx::query_scalar("SELECT parent_id FROM conversation_groups WHERE id = ?1")
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

/// `list_groups` 的 SQL 行（id, name, system_prompt, parent_id, sort_order, expanded, created_at）。
type GroupRow = (
    String,
    String,
    Option<String>,
    Option<String>,
    i64,
    i64,
    i64,
);

/// 列出所有分组（按 sort_order 升序，含 parent_id 供前端构建树）。
pub async fn list_groups(pool: &SqlitePool) -> Vec<ConversationGroup> {
    let rows: Vec<GroupRow> = sqlx::query_as(
        "SELECT id, name, system_prompt, parent_id, sort_order, expanded, created_at \
             FROM conversation_groups ORDER BY sort_order ASC, created_at ASC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(
            |(id, name, system_prompt, parent_id, sort_order, expanded, created_at)| {
                ConversationGroup {
                    id,
                    name,
                    system_prompt,
                    parent_id,
                    sort_order,
                    expanded: expanded != 0,
                    created_at,
                }
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
    let result = sqlx::query("UPDATE conversation_groups SET system_prompt = ?1 WHERE id = ?2")
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
pub async fn set_group_expanded(pool: &SqlitePool, id: &str, expanded: bool) -> Result<(), String> {
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
    let prompt: Option<Option<String>> =
        sqlx::query_scalar("SELECT system_prompt FROM conversation_groups WHERE id = ?1")
            .bind(gid)
            .fetch_optional(pool)
            .await
            .map_err(|e| e.to_string())?;

    Ok(prompt.unwrap_or(None))
}

// ── 0.13.2 FTS5 记忆归档与召回 ──────────────────────────────────────────────────

/// FTS5 召回结果。
#[derive(Debug, Clone)]
pub struct MemoryRecall {
    /// 消息文本内容。
    pub content: String,
    /// 消息角色（user/assistant）。
    pub role: String,
}

/// 将被挤出窗口的消息归档到 FTS5（幂等：content_hash 去重）。
///
/// 0.13.2：滑动窗口/token-aware 裁剪挤出的消息归档到 `memory_fts` 虚拟表，
/// 供后续 BM25 全文检索召回。`content_hash` 用于幂等去重——
/// 同一消息文本不重复归档（INSERT OR IGNORE 语义通过先查再插实现）。
///
/// **注意**：FTS5 虚拟表不支持 UNIQUE 约束，需手动查重。
pub async fn archive_to_fts(
    pool: &SqlitePool,
    conversation_id: &str,
    role: &str,
    content: &str,
    content_hash: &str,
) -> Result<(), String> {
    // 幂等检查：同 hash 已存在则跳过
    let exists: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM memory_fts WHERE conversation_id = ?1 AND content_hash = ?2",
    )
    .bind(conversation_id)
    .bind(content_hash)
    .fetch_one(pool)
    .await
    .map_err(|e| e.to_string())?;

    if exists > 0 {
        return Ok(());
    }

    sqlx::query(
        "INSERT INTO memory_fts (content, conversation_id, role, content_hash) VALUES (?1, ?2, ?3, ?4)",
    )
    .bind(content)
    .bind(conversation_id)
    .bind(role)
    .bind(content_hash)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;

    Ok(())
}

/// FTS5 BM25 全文检索召回。
///
/// 从 `memory_fts` 中检索与 `query` 相关的旧消息，按 BM25 相关度排序，返回 Top-K。
/// `conversation_id` 限定检索范围（只召回当前对话的归档消息）。
///
/// **trigram 分词器**：中文友好，支持子串模糊匹配（如搜"搜索"命中含"搜索词"的文本）。
///
/// **OR 语义**（0.13 优化）：将用户消息拆词后以 `OR` 连接，
/// 任一关键词命中即召回——避免长消息 AND 全命中率低的问题。
/// trigram 要求 ≥3 字符，短于 3 的词自动跳过。
pub async fn search_memory_fts(
    pool: &SqlitePool,
    conversation_id: &str,
    query: &str,
    limit: i64,
) -> Result<Vec<MemoryRecall>, String> {
    if query.trim().is_empty() {
        return Ok(Vec::new());
    }

    // 将 query 转为 OR 语义：拆词 → 过滤 <3 字符 → 双引号转义 → OR 连接
    let fts_query = build_fts_or_query(query);
    if fts_query.is_empty() {
        return Ok(Vec::new());
    }

    // BM25：分数越低越相关（FTS5 的 bm25() 返回负值，越负越相关）
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT content, role FROM memory_fts
         WHERE memory_fts MATCH ?1 AND conversation_id = ?2
         ORDER BY bm25(memory_fts)
         LIMIT ?3",
    )
    .bind(&fts_query)
    .bind(conversation_id)
    .bind(limit)
    .fetch_all(pool)
    .await
    .map_err(|e| {
        tracing::warn!(error = %e, %fts_query, "FTS5 检索失败");
        e.to_string()
    })?;

    Ok(rows
        .into_iter()
        .map(|(content, role)| MemoryRecall { content, role })
        .collect())
}

/// 将用户消息转为 FTS5 OR 查询字符串（0.21.18 重写分词策略）。
///
/// ## 背景
///
/// `memory_fts` 表使用 `tokenize='trigram'`，trigram 索引每个 ≥3 字符的子串。
/// 引号短语查询 = 子串包含语义：`"异步编程"` 命中含该 4 字子串的文档。
///
/// 旧实现按**空白拆词**做 OR 检索。英文正常；但中文没有空格——整句变成一个
/// 带引号的长 phrase，trigram 下等价于"要求归档文本包含**整句原样子串**"，
/// 几乎永远不命中。中文用户的"召回本对话较早内容"形同虚设。
///
/// ## 新策略
///
/// 逐字符扫描，把 query 切成两类 run，生成 OR 词项：
///
/// - **ASCII / 非 CJK 词**：保持原行为（≥3 字符、去内嵌 `"`、双引号包裹）。
/// - **CJK 连续 run**（用 `is_cjk_char` 判定）：
///   - 长度 <3 → 跳过（trigram 下限无法命中）；
///   - 长度 3..=6 → 整段作为一个引号短语（子串匹配，精确且便宜）；
///   - 长度 >6 → 生成 3 字符滑动窗口（stride 1），每个窗口一个引号词项。
///
/// **总量上限** 32 个 OR 词项（防超长用户消息生成巨型 FTS 查询）；超限时保留
/// 首尾各半（前 16 + 后 16），丢弃中部。词项去重。
///
/// 超长 query（>4096 字符）先截取首尾各 2048 字符，防无界收集——
/// 1MB CJK query 会产生 ~30 万个滑动窗口词项，导致 Vec + HashSet 瞬时占用
/// 30-40MB。截取后最终只保留 32 个词项，对头尾两段采样与对全量词项保头保尾等价。
///
/// 返回 `"term1" OR "term2" OR ...`；无有效词项返回空串（上层已处理空串）。
fn build_fts_or_query(query: &str) -> String {
    /// OR 词项总量上限，防止超长用户消息生成巨型 FTS 查询拖慢检索。
    const MAX_TERMS: usize = 32;
    /// query 字符数截断阈值——超过此值先截取首尾两段。
    const QUERY_TRUNCATE_THRESHOLD: usize = 4096;
    /// 截断时保留的首/尾字符数。
    const QUERY_TRUNCATE_HALF: usize = 2048;

    let char_count = query.chars().count();

    // 超长 query 截取首尾各 2048 字符，防无界收集
    if char_count > QUERY_TRUNCATE_THRESHOLD {
        let chars: Vec<char> = query.chars().collect();
        let head: String = chars[..QUERY_TRUNCATE_HALF].iter().collect();
        let tail: String = chars[char_count - QUERY_TRUNCATE_HALF..].iter().collect();

        // 对头尾两段分别收集，合并去重
        let mut terms: Vec<String> = Vec::new();
        let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
        collect_terms(&head, &mut terms, &mut seen);
        collect_terms(&tail, &mut terms, &mut seen);

        // 超限时取头部段的词项作前 16、尾部段的词项作后 16
        if terms.len() > MAX_TERMS {
            let half = MAX_TERMS / 2;
            let mut kept: Vec<String> = Vec::with_capacity(MAX_TERMS);
            kept.extend_from_slice(&terms[..half]);
            kept.extend_from_slice(&terms[terms.len() - half..]);
            terms = kept;
        }

        return terms.join(" OR ");
    }

    // 常规路径：收集全部词项
    let mut terms: Vec<String> = Vec::new();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();
    collect_terms(query, &mut terms, &mut seen);

    // 超限时保留首尾各半（前 16 + 后 16），丢弃中部。
    if terms.len() > MAX_TERMS {
        let half = MAX_TERMS / 2;
        let mut kept: Vec<String> = Vec::with_capacity(MAX_TERMS);
        kept.extend_from_slice(&terms[..half]);
        kept.extend_from_slice(&terms[terms.len() - half..]);
        terms = kept;
    }

    terms.join(" OR ")
}

/// 从一段 query 文本收集 FTS OR 词项（含去重），无上限截断。
///
/// 逐字符扫描，把 query 切成 CJK run 与非 CJK run 交替处理：
/// - CJK run：由 `push_cjk_terms` 生成词项
/// - 非 CJK run：按空白拆分，≥3 字符的词生成引号词项
fn collect_terms(
    text: &str,
    terms: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if is_cjk_char(chars[i]) {
            // 收集连续 CJK run
            let start = i;
            while i < chars.len() && is_cjk_char(chars[i]) {
                i += 1;
            }
            let run: String = chars[start..i].iter().collect();
            push_cjk_terms(&run, terms, seen);
        } else {
            // 收集连续非 CJK（ASCII / 空白 / 其他），后续按空白拆分
            let start = i;
            while i < chars.len() && !is_cjk_char(chars[i]) {
                i += 1;
            }
            let segment: String = chars[start..i].iter().collect();
            for word in segment.split_whitespace() {
                // trigram 下限：<3 字符的词无法命中任何内容，跳过
                if word.chars().count() < 3 {
                    continue;
                }
                let cleaned = word.replace('"', "");
                let term = format!("\"{cleaned}\"");
                if seen.insert(cleaned) {
                    terms.push(term);
                }
            }
        }
    }
}

/// 为一段 CJK run 生成 OR 词项并追加到 `terms`（带去重 + 上限截断）。
///
/// - 长度 <3 → 跳过（trigram 下限）；
/// - 长度 3..=6 → 整段作为一个引号短语；
/// - 长度 >6 → 3 字符滑动窗口（stride 1），每个窗口一个引号词项。
fn push_cjk_terms(
    run: &str,
    terms: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
) {
    let chars: Vec<char> = run.chars().collect();
    let len = chars.len();
    if len < 3 {
        return;
    }
    if len <= 6 {
        let cleaned = run.replace('"', "");
        if seen.insert(cleaned.clone()) {
            terms.push(format!("\"{cleaned}\""));
        }
        return;
    }
    // 长度 >6：3 字符滑动窗口（stride 1）
    for window in chars.windows(3) {
        let s: String = window.iter().collect();
        if seen.insert(s.clone()) {
            terms.push(format!("\"{s}\""));
        }
    }
}

/// 判断字符是否为 CJK（中日韩统一表意文字 + 扩展 A/B + 兼容 + 假名 + 韩文 + 全角形式）。
///
/// 与 `domain::ai::token_budget::is_cjk` 镜像，分层禁止跨层引用（infra 不得
/// `use crate::domain::`）。字符范围保持一致以避免同一仓库内两套 CJK 定义
/// 产生行为漂移。
///
/// P1-3: 改为 pub 供 domain 层测试做双源一致性校验（domain 依赖 infra 合法）。
pub fn is_cjk_char(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x3000..=0x33FF    // CJK 符号和标点 + 假名（平假名/片假名）
        | 0x3400..=0x4DBF  // CJK 扩展 A
        | 0x4E00..=0x9FFF  // CJK 统一表意文字
        | 0xAC00..=0xD7AF  // 韩文音节
        | 0xF900..=0xFAFF  // CJK 兼容表意文字
        | 0xFF00..=0xFFEF  // 半角/全角形式
        | 0x20000..=0x2A6DF // CJK 扩展 B
    )
}

/// 清理指定对话的 FTS5 归档（删除对话 / 清空消息时调用）。
pub async fn clear_memory_fts(pool: &SqlitePool, conversation_id: &str) -> Result<(), String> {
    sqlx::query("DELETE FROM memory_fts WHERE conversation_id = ?1")
        .bind(conversation_id)
        .execute(pool)
        .await
        .map_err(|e| e.to_string())?;
    Ok(())
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

        create_conversation(&pool, "c1", Some("Hello"))
            .await
            .unwrap();
        create_conversation(&pool, "c2", Some("World"))
            .await
            .unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(convs.len(), 2);
        // 按 last_active_at 倒序——c2 后创建所以在前
        assert_eq!(convs[0].id, "c2");
        assert_eq!(convs[1].id, "c1");
    }

    #[tokio::test]
    async fn create_conversation_is_idempotent() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Title 1"))
            .await
            .unwrap();
        // 重复创建不报错
        create_conversation(&pool, "c1", Some("Title 2"))
            .await
            .unwrap();

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
        create_conversation(&pool, "c1", Some("Real Title"))
            .await
            .unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(
            convs[0].title.as_deref(),
            Some("Real Title"),
            "空标题应被补写"
        );

        // 已有非空标题时，不覆盖
        create_conversation(&pool, "c1", Some("Other Title"))
            .await
            .unwrap();
        let convs = list_conversations(&pool).await.unwrap();
        assert_eq!(
            convs[0].title.as_deref(),
            Some("Real Title"),
            "非空标题不覆盖"
        );
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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        append_message(
            &pool,
            "c1",
            "user",
            r#"{"role":"user","content":[{"type":"text","text":"hi"}]}"#,
        )
        .await
        .unwrap();
        append_message(
            &pool,
            "c1",
            "assistant",
            r#"{"role":"assistant","content":[{"type":"text","text":"hello"}]}"#,
        )
        .await
        .unwrap();

        assert_eq!(count_messages(&pool).await, 2);

        let deleted = delete_conversation(&pool, "c1").await.unwrap();
        assert!(deleted);

        assert_eq!(count_conversations(&pool).await, 0);
        assert_eq!(count_messages(&pool).await, 0);
    }

    #[tokio::test]
    async fn append_and_load_messages() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();

        for i in 0..5 {
            append_message(
                &pool,
                "c1",
                "user",
                &format!("{{\"role\":\"user\",\"content\":{i}}}"),
            )
            .await
            .unwrap();
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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        append_message(&pool, "c1", "user", "content")
            .await
            .unwrap();

        clear_messages(&pool, "c1").await.unwrap();
        assert_eq!(count_messages(&pool).await, 0);
        // conversation 记录仍在
        assert_eq!(count_conversations(&pool).await, 1);
    }

    // ── 0.12.5 §5.5: truncate_messages ──────────────────────────────────

    #[tokio::test]
    async fn truncate_messages_keeps_first_n() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        for i in 0..5 {
            append_message(&pool, "c1", "user", &format!("msg{i}"))
                .await
                .unwrap();
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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        append_message(&pool, "c1", "user", "msg0").await.unwrap();
        append_message(&pool, "c1", "user", "msg1").await.unwrap();

        // keep_count > 消息数 → 无操作
        truncate_messages(&pool, "c1", 10).await.unwrap();
        assert_eq!(count_messages(&pool).await, 2);
    }

    #[tokio::test]
    async fn load_empty_conversation_returns_empty() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Empty"))
            .await
            .unwrap();
        let msgs = load_recent_messages(&pool, "c1", 20).await.unwrap();
        assert!(msgs.is_empty());
    }

    #[tokio::test]
    async fn load_nonexistent_conversation_returns_empty() {
        let pool = setup_pool().await;
        let msgs = load_recent_messages(&pool, "nonexistent", 20)
            .await
            .unwrap();
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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        append_message(&pool, "c1", "user", "content1")
            .await
            .unwrap();
        append_message(&pool, "c1", "assistant", "content2")
            .await
            .unwrap();

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
        create_group(&pool, "g2", "项目A", Some("g1"))
            .await
            .unwrap();

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
        create_conversation(&pool, "c1", Some("周报"))
            .await
            .unwrap();
        set_conversation_group(&pool, "c1", Some("g1"))
            .await
            .unwrap();

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
        update_group_system_prompt(&pool, "g1", Some("你是翻译助手"))
            .await
            .unwrap();
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
        set_conversation_group(&pool, "c1", Some("g1"))
            .await
            .unwrap();

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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        set_conversation_group(&pool, "c1", Some("g1"))
            .await
            .unwrap();

        // 移到 g2
        set_conversation_group(&pool, "c1", Some("g2"))
            .await
            .unwrap();
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
        update_group_system_prompt(&pool, "g1", Some("你是翻译助手"))
            .await
            .unwrap();
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        set_conversation_group(&pool, "c1", Some("g1"))
            .await
            .unwrap();

        // 有分组的对话 → 返回分组系统提示词
        let prompt = get_effective_system_prompt(&pool, "c1").await.unwrap();
        assert_eq!(prompt.as_deref(), Some("你是翻译助手"));

        // 默认组对话 → None
        create_conversation(&pool, "c2", Some("Default"))
            .await
            .unwrap();
        let prompt = get_effective_system_prompt(&pool, "c2").await.unwrap();
        assert_eq!(prompt, None);

        // 不存在的对话 → None
        let prompt = get_effective_system_prompt(&pool, "nonexistent")
            .await
            .unwrap();
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
        let children: Vec<_> = groups
            .iter()
            .filter(|g| g.parent_id.as_deref() == Some("g1"))
            .collect();
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
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        set_conversation_group(&pool, "c1", None).await.unwrap();
    }

    #[tokio::test]
    async fn list_conversations_includes_group_id() {
        let pool = setup_pool().await;
        create_group(&pool, "g1", "工作", None).await.unwrap();
        create_conversation(&pool, "c1", Some("A")).await.unwrap();
        create_conversation(&pool, "c2", Some("B")).await.unwrap();
        set_conversation_group(&pool, "c1", Some("g1"))
            .await
            .unwrap();

        let convs = list_conversations(&pool).await.unwrap();
        let c1 = convs.iter().find(|c| c.id == "c1").unwrap();
        let c2 = convs.iter().find(|c| c.id == "c2").unwrap();
        assert_eq!(c1.group_id.as_deref(), Some("g1"));
        assert_eq!(c2.group_id, None);
    }

    // ── 0.13.2 FTS5 记忆归档与召回 ──────────────────────────────────────

    #[tokio::test]
    async fn fts_archive_and_search() {
        let pool = setup_pool().await;

        // 归档几条消息
        archive_to_fts(
            &pool,
            "c1",
            "user",
            "如何用 Rust 写一个 HTTP 服务器",
            "hash1",
        )
        .await
        .unwrap();
        archive_to_fts(
            &pool,
            "c1",
            "assistant",
            "可以使用 axum 或 actix-web 框架",
            "hash2",
        )
        .await
        .unwrap();
        archive_to_fts(&pool, "c1", "user", "Rust 的所有权机制是什么", "hash3")
            .await
            .unwrap();

        // 搜索 "Rust" 应返回相关消息
        let recalls = search_memory_fts(&pool, "c1", "Rust", 3).await.unwrap();
        assert!(!recalls.is_empty(), "应能搜到包含 Rust 的消息");
        assert!(recalls.iter().any(|r| r.content.contains("Rust")));
    }

    #[tokio::test]
    async fn fts_archive_is_idempotent() {
        let pool = setup_pool().await;

        // 同一 content_hash 归档两次应幂等
        archive_to_fts(&pool, "c1", "user", "重复的消息内容", "hash_dup")
            .await
            .unwrap();
        archive_to_fts(&pool, "c1", "user", "重复的消息内容", "hash_dup")
            .await
            .unwrap();

        // 搜索应只返回一条（trigram 需要 3+ 字符，用「重复的」搜索）
        let recalls = search_memory_fts(&pool, "c1", "重复的", 10).await.unwrap();
        assert_eq!(recalls.len(), 1, "幂等归档：同 hash 不重复插入");
    }

    #[tokio::test]
    async fn fts_search_isolation_between_conversations() {
        let pool = setup_pool().await;

        archive_to_fts(&pool, "c1", "user", "对话一的话题是 Rust", "h1")
            .await
            .unwrap();
        archive_to_fts(&pool, "c2", "user", "对话二的话题是 Python", "h2")
            .await
            .unwrap();

        // c1 搜索只返回 c1 的归档
        let recalls = search_memory_fts(&pool, "c1", "Rust", 10).await.unwrap();
        assert_eq!(recalls.len(), 1);
        assert!(recalls[0].content.contains("Rust"));

        // c2 搜索只返回 c2 的归档
        let recalls = search_memory_fts(&pool, "c2", "Python", 10).await.unwrap();
        assert_eq!(recalls.len(), 1);
        assert!(recalls[0].content.contains("Python"));

        // c1 搜索 Python 应无结果
        let recalls = search_memory_fts(&pool, "c1", "Python", 10).await.unwrap();
        assert!(recalls.is_empty(), "对话隔离：c1 不应搜到 c2 的归档");
    }

    #[tokio::test]
    async fn fts_search_chinese_trigram() {
        let pool = setup_pool().await;

        archive_to_fts(&pool, "c1", "user", "我想学习 Rust 异步编程", "h1")
            .await
            .unwrap();
        archive_to_fts(
            &pool,
            "c1",
            "assistant",
            "异步编程推荐使用 tokio 运行时",
            "h2",
        )
        .await
        .unwrap();

        // trigram 分词器：搜 "异步编程" 应命中含 "异步编程" 的消息
        let recalls = search_memory_fts(&pool, "c1", "异步编程", 10)
            .await
            .unwrap();
        assert!(!recalls.is_empty(), "trigram 中文子串匹配应生效");

        // 搜 "tokio" 应命中 assistant 消息
        let recalls = search_memory_fts(&pool, "c1", "tokio", 10).await.unwrap();
        assert!(!recalls.is_empty(), "英文关键词也应可搜到");
    }

    #[tokio::test]
    async fn fts_clear_removes_entries() {
        let pool = setup_pool().await;

        archive_to_fts(&pool, "c1", "user", "待清理的消息内容", "h1")
            .await
            .unwrap();
        assert!(
            !search_memory_fts(&pool, "c1", "清理的", 10)
                .await
                .unwrap()
                .is_empty()
        );

        clear_memory_fts(&pool, "c1").await.unwrap();
        assert!(
            search_memory_fts(&pool, "c1", "清理的", 10)
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn fts_delete_conversation_cascades() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        archive_to_fts(&pool, "c1", "user", "级联删除测试", "h1")
            .await
            .unwrap();
        assert!(
            !search_memory_fts(&pool, "c1", "级联删除", 10)
                .await
                .unwrap()
                .is_empty()
        );

        delete_conversation(&pool, "c1").await.unwrap();
        assert!(
            search_memory_fts(&pool, "c1", "级联删除", 10)
                .await
                .unwrap()
                .is_empty(),
            "删除对话应级联清理 FTS5 归档"
        );
    }

    #[tokio::test]
    async fn fts_clear_messages_cleans_fts() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Test"))
            .await
            .unwrap();
        archive_to_fts(&pool, "c1", "user", "清空消息时清理FTS归档", "h1")
            .await
            .unwrap();
        assert!(
            !search_memory_fts(&pool, "c1", "清空消息", 10)
                .await
                .unwrap()
                .is_empty()
        );

        clear_messages(&pool, "c1").await.unwrap();
        assert!(
            search_memory_fts(&pool, "c1", "清空消息", 10)
                .await
                .unwrap()
                .is_empty(),
            "清空消息应同步清理 FTS5 归档"
        );
    }

    #[tokio::test]
    async fn fts_search_empty_query_returns_empty() {
        let pool = setup_pool().await;
        archive_to_fts(&pool, "c1", "user", "一些内容", "h1")
            .await
            .unwrap();

        let recalls = search_memory_fts(&pool, "c1", "", 10).await.unwrap();
        assert!(recalls.is_empty(), "空 query 应返回空结果");

        let recalls = search_memory_fts(&pool, "c1", "   ", 10).await.unwrap();
        assert!(recalls.is_empty(), "纯空格 query 应返回空结果");
    }

    #[tokio::test]
    async fn fts_search_or_semantics() {
        let pool = setup_pool().await;

        // 归档两条消息：一条含 Rust，一条含 Python
        archive_to_fts(&pool, "c1", "user", "如何用 Rust 写 HTTP 服务器", "h1")
            .await
            .unwrap();
        archive_to_fts(&pool, "c1", "user", "Python 数据分析入门", "h2")
            .await
            .unwrap();

        // 多词 query：AND 语义会要求全部命中，OR 语义只需任一命中
        // “Rust 和 Python 区别” — AND 下无命中（无消息同时含两者），OR 下命中两条
        let recalls = search_memory_fts(&pool, "c1", "Rust 和 Python 区别", 10)
            .await
            .unwrap();
        assert_eq!(
            recalls.len(),
            2,
            "OR 语义：含 Rust 或 Python 的消息都应被召回"
        );
    }

    #[tokio::test]
    async fn fts_search_short_words_filtered() {
        // trigram 要求 ≥3 字符，短词自动跳过
        let pool = setup_pool().await;
        archive_to_fts(&pool, "c1", "user", "Go 语言并发模型", "h1")
            .await
            .unwrap();

        // “Go” 只有 2 字符，被过滤；“语言” 只有 2 字符也被过滤
        // 但 “并发模型” 4 字符可命中
        let recalls = search_memory_fts(&pool, "c1", "Go 语言 并发模型", 10)
            .await
            .unwrap();
        assert!(!recalls.is_empty(), "短词过滤后仍有有效词可命中");

        // 全是短词 → 无有效 query → 空结果
        let recalls = search_memory_fts(&pool, "c1", "Go 语言", 10).await.unwrap();
        assert!(recalls.is_empty(), "全部短于 3 字符的词应返回空结果");
    }

    // ── 0.21.18 FTS 中文长句召回修复 ───────────────────────────────────

    /// 核心回归：旧实现（空白拆词）下此用例必失败。
    ///
    /// 归档内容含 "异步编程" 和 "tokio"，但查询是一个自然长句，
    /// 旧逻辑把整句作为一个 phrase → 要求归档文本包含整句原样子串 → 不命中。
    /// 新逻辑（CJK 滑窗）把长句拆成 3 字窗口 OR → 任一窗口命中即召回。
    #[tokio::test]
    async fn fts_search_long_chinese_sentence_recall() {
        let pool = setup_pool().await;

        archive_to_fts(
            &pool,
            "c1",
            "user",
            "我想学习 Rust 异步编程和 tokio 运行时的生态",
            "h_long",
        )
        .await
        .unwrap();

        // 长自然句查询——包含 "异步编程" 等关键词，但不是归档内容的原样子串
        let query = "帮我规划一下异步编程的学习路线应该怎么安排";
        let recalls = search_memory_fts(&pool, "c1", query, 10)
            .await
            .unwrap();
        assert!(
            !recalls.is_empty(),
            "长中文句查询应通过滑窗 OR 召回包含关键词的归档消息"
        );
        assert!(
            recalls
                .iter()
                .any(|r| r.content.contains("异步编程")),
            "召回内容应包含归档的异步编程消息"
        );
    }

    /// `build_fts_or_query` 单测：混合中英 → 两类词项都在结果里。
    #[test]
    fn fts_query_mixed_cjk_ascii() {
        // "Rust" ASCII ≥3 保留，"异步编程" 是 4 字 CJK run（≤6 整段短语），
        // "tokio" ASCII ≥3 保留。
        let q = build_fts_or_query("Rust 异步编程 tokio");
        // ASCII 词 "Rust" 和 "tokio" 应作为引号短语出现
        assert!(q.contains("\"Rust\""), "ASCII 词应保留：{q}");
        assert!(q.contains("\"tokio\""), "ASCII 词应保留：{q}");
        // CJK run "异步编程"（4 字 ≤6）应作为整段短语出现
        assert!(
            q.contains("\"异步编程\""),
            "3..=6 CJK run 应整段短语：{q}"
        );
        // 结果非空
        assert!(!q.is_empty(), "混合中英 query 应生成非空 FTS 串");
    }

    /// `build_fts_or_query` 单测：长 CJK run（20 字）→ 多个 3 字滑窗词项。
    #[test]
    fn fts_query_long_cjk_run_sliding_windows() {
        // 20 字 CJK run → 滑窗 stride 1，窗口数 = 20 - 3 + 1 = 18 个
        let run = "一二三四五六七八九十一二三四五六七八九十";
        assert_eq!(run.chars().count(), 20);
        let q = build_fts_or_query(run);
        // 应有多个 OR 词项（>1），且以 OR 连接
        assert!(
            q.contains(" OR "),
            "长 CJK run 应生成多个 OR 词项：{q}"
        );
        // 词项数 ≤ 32
        let term_count = q.split(" OR ").count();
        assert!(
            term_count <= 32,
            "OR 词项应 ≤ 32 上限，实际 {term_count}"
        );
        // 每个词项应为 3 字（双引号包裹 3 个 CJK 字符）
        let first_term = q.split(" OR ").next().unwrap();
        assert!(
            first_term.starts_with("\""),
            "词项应双引号包裹：{first_term}"
        );
    }

    /// `build_fts_or_query` 单测：2 字 CJK run 被过滤。
    #[test]
    fn fts_query_short_cjk_filtered() {
        // 纯 2 字 CJK → 全被过滤 → 空串
        let q = build_fts_or_query("你好");
        assert!(q.is_empty(), "2 字 CJK run 应被过滤，实际：{q}");

        // 混合：2 字 CJK + 有效词 → 2 字被过滤，有效词保留
        let q = build_fts_or_query("你好 Rust");
        assert!(
            q.contains("\"Rust\"") && !q.contains("你好"),
            "2 字 CJK 过滤，ASCII ≥3 保留：{q}"
        );
    }

    /// `build_fts_or_query` 单测：空 query / 全短词 → 空串。
    #[test]
    fn fts_query_empty_and_all_short() {
        assert_eq!(build_fts_or_query(""), "");
        assert_eq!(build_fts_or_query("   "), "");
        // 全是 <3 字符的 ASCII + CJK → 空串
        assert_eq!(build_fts_or_query("Go 语言"), "");
    }

    /// `build_fts_or_query` 单测：词项内 `"` 被去除。
    #[test]
    fn fts_query_strips_embedded_quotes() {
        // 含内嵌双引号的 ASCII 词 → 去引号后 ≥3 才保留
        let q = build_fts_or_query("Rus\"t tokyo");
        // "Rus\"t" → 清理后 "Rust"（4 字）应保留
        // "tokyo" 应保留
        assert!(q.contains("\"Rust\""), "内嵌引号应去除：{q}");
        assert!(q.contains("\"tokyo\""), "正常词应保留：{q}");
        // 不应出现未转义的裸引号破坏 FTS 语法
        assert!(
            !q.contains("\"\"\""),
            "不应出现连续三引号：{q}"
        );
    }

    /// `build_fts_or_query` 单测：词项去重。
    #[test]
    fn fts_query_dedup_terms() {
        // "Rust Rust Rust" → 去重后只一个 "Rust"
        let q = build_fts_or_query("Rust Rust Rust");
        let terms: Vec<&str> = q.split(" OR ").collect();
        assert_eq!(
            terms.len(),
            1,
            "重复词项应去重，实际：{q}"
        );
        assert_eq!(terms[0], "\"Rust\"");
    }

    /// `build_fts_or_query` 单测：32 词项上限，保留首尾各半。
    #[test]
    fn fts_query_max_32_terms() {
        // 生成 40 个不同的 ≥3 字符 ASCII 词
        let words: Vec<String> = (0..40).map(|i| format!("word{i:02}")).collect();
        let query = words.join(" ");
        let q = build_fts_or_query(&query);
        let terms: Vec<&str> = q.split(" OR ").collect();
        assert_eq!(
            terms.len(), 32,
            "超长 query 应截断到 32 词项，实际 {}", terms.len()
        );
        // P1-4: 包含首词 word00 与末词 word39
        assert!(
            terms.contains(&"\"word00\""),
            "应包含首词 word00"
        );
        assert!(
            terms.contains(&"\"word39\""),
            "应包含末词 word39"
        );
        // P1-4: 不含中部词 word20
        assert!(
            !terms.contains(&"\"word20\""),
            "不应包含中部词 word20"
        );
    }

    /// 超长 CJK query（8000 字符）截取首尾各 2048 字符后收集，
    /// 返回 ≤32 词项，且首 3 字窗口与末 3 字窗口都在结果中。
    #[test]
    fn fts_query_long_cjk_truncated_to_head_tail() {
        // 构造 8000 字符的 CJK query。为确保首/末 3 字窗口不被去重丢弃，
        // 头部和尾部各用一个唯一的"锚点"3 字符序列（不与中部重复）。
        let head_anchor = "\u{4E00}\u{4E01}\u{4E02}"; // 一丁丂
        let tail_anchor = "\u{5E00}\u{5E01}\u{5E02}"; // 廡廢廣
        let filler = "\u{6C34}"; // 水 — 重复填充字符

        let mut query = String::with_capacity(8000);
        query.push_str(head_anchor);
        for _ in 0..(8000 - 6) {
            query.push_str(filler);
        }
        query.push_str(tail_anchor);
        assert_eq!(query.chars().count(), 8000);

        let q = build_fts_or_query(&query);
        let terms: Vec<&str> = q.split(" OR ").collect();

        // 应 ≤32 词项
        assert!(
            terms.len() <= 32,
            "超长 query 应截断到 ≤32 词项，实际 {}", terms.len()
        );

        // 首 3 字窗口应在结果中（head_anchor 本身就是一个 3 字符 run，整段作为一个引号短语）
        assert!(
            q.contains(&format!("\"{head_anchor}\"")),
            "应包含首 3 字窗口 \"{head_anchor}\""
        );

        // 末 3 字窗口应在结果中（tail_anchor 同理）
        assert!(
            q.contains(&format!("\"{tail_anchor}\"")),
            "应包含末 3 字窗口 \"{tail_anchor}\""
        );
    }

    /// 4096 字符以内的 query 走原路径不受截断影响。
    #[test]
    fn fts_query_under_threshold_not_truncated() {
        // 构造恰好 4096 字符的 ASCII query（每词 5 字符 + 空格 = 6 字符/词，~681 词）
        // 实际只需验证不触发 >4096 截断分支即可——用一个短 query 确认走原路径
        let query: String = "hello world test ".repeat(50); // ~800 字符
        assert!(query.chars().count() <= 4096);

        let q = build_fts_or_query(&query);
        // 应正常产生词项（hello / world / test 去重后 3 个）
        assert!(!q.is_empty(), "短 query 应正常产生词项");
        assert!(q.contains("\"hello\""), "应包含 hello");
        assert!(q.contains("\"world\""), "应包含 world");
        assert!(q.contains("\"test\""), "应包含 test");
    }
}
