//! 对话持久化数据访问层（0.12.3 Phase A）。
//!
//! `conversations` 和 `messages` 表存于 AI 库（`blink_ai.db`）。
//!
//! **设计原则**：
//! - `messages.content` 列存 `serde_json::to_string(&rig_core::completion::Message)`，
//!   完整保留 text / tool_call / tool_result。
//! - `role` 列从 Message 变体提取（system/user/assistant），供查询用。
//! - 删除 conversation 时级联删除 messages（SQLite 默认不启用 FK，手动 DELETE）。
//! - 滑动窗口策略在 `SqliteConversationMemory::load` 中实现（取最近 N 条），
//!   持久化层保存完整历史，不删除旧数据。

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
}

/// 初始化 `conversations` + `messages` 表。
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

    tracing::debug!("conversations + messages 表已初始化");
    Ok(())
}

/// 创建对话记录（INSERT OR IGNORE——已存在时不报错）。
///
/// `title` 为 None 时用空标题（后续可由 rename 更新）。
pub async fn create_conversation(
    pool: &SqlitePool,
    id: &str,
    title: Option<&str>,
) -> Result<(), String> {
    let now = chrono::Utc::now().timestamp();
    sqlx::query(
        "INSERT OR IGNORE INTO conversations (id, title, created_at, last_active_at) VALUES (?1, ?2, ?3, ?3)",
    )
    .bind(id)
    .bind(title)
    .bind(now)
    .execute(pool)
    .await
    .map_err(|e| e.to_string())?;
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

/// 列出所有对话（按 last_active_at 倒序），含消息条数。
pub async fn list_conversations(pool: &SqlitePool) -> Vec<Conversation> {
    let rows: Vec<(String, Option<String>, i64, i64, i64)> = sqlx::query_as(
        "SELECT c.id, c.title, c.created_at, c.last_active_at, \
         (SELECT COUNT(*) FROM messages m WHERE m.conversation_id = c.id) AS msg_count \
         FROM conversations c ORDER BY c.last_active_at DESC, c.rowid DESC",
    )
    .fetch_all(pool)
    .await
    .unwrap_or_default();

    rows.into_iter()
        .map(|(id, title, created_at, last_active_at, msg_count)| Conversation {
            id,
            title,
            created_at,
            last_active_at,
            message_count: Some(msg_count),
        })
        .collect()
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
pub async fn load_all_messages(
    pool: &SqlitePool,
    conversation_id: &str,
) -> Result<Vec<(String, String)>, String> {
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT role, content FROM messages WHERE conversation_id = ?1 ORDER BY id ASC",
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

        let convs = list_conversations(&pool).await;
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

        let convs = list_conversations(&pool).await;
        assert_eq!(convs.len(), 1);
        // INSERT OR IGNORE 保留原值
        assert_eq!(convs[0].title.as_deref(), Some("Title 1"));
    }

    #[tokio::test]
    async fn rename_updates_title() {
        let pool = setup_pool().await;
        create_conversation(&pool, "c1", Some("Old")).await.unwrap();

        let updated = rename_conversation(&pool, "c1", "New").await.unwrap();
        assert!(updated);

        let convs = list_conversations(&pool).await;
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

        let convs = list_conversations(&pool).await;
        assert_eq!(convs[0].message_count, Some(2));
    }
}
