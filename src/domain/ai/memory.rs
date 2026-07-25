//! SQLite 持久化对话记忆（0.12.3 Phase A）。
//!
//! 实现 rig `ConversationMemory` trait，将对话历史持久化到 `blink_ai.db`。
//! 替代 0.12.1 的 `InMemoryConversationMemory`（进程内，重启丢）。
//!
//! ## 滑动窗口
//!
//! `load()` 只返回最近 `SLIDING_WINDOW_SIZE` 条消息，但 DB 保留完整历史。
//! TokenWindow、摘要压缩和语义召回留后续版本（0.13 记忆向量召回）。
//!
//! ## 自动创建对话
//!
//! `append()` 时若 conversation 记录不存在，自动创建（`INSERT OR IGNORE`）。
//! 标题从首条 User 消息提取（前 50 字符），无 User 消息时为空。
//!
//! ## content 序列化
//!
//! `messages.content` 列存 `serde_json::to_string(&Message)`，完整保留
//! text / tool_call / tool_result。`Message` 有 `Serialize/Deserialize`。

use rig_core::completion::message::UserContent;
use rig_core::completion::Message;
use rig_core::memory::{ConversationMemory, MemoryError};
use sqlx::SqlitePool;

/// 滑动窗口大小（0.12.3 §5.3：最近 20 条消息）。
const SLIDING_WINDOW_SIZE: i64 = 20;

/// 标题最大字符数（从首条 User 消息提取）。
const TITLE_MAX_CHARS: usize = 50;

/// SQLite 持久化对话记忆。
///
/// 持有 `SqlitePool`（AI 库），实现 rig `ConversationMemory` trait。
/// Agent 构造时注入，rig 自动按 `conversation_id` load/append/clear。
pub struct SqliteConversationMemory {
    pool: SqlitePool,
}

impl SqliteConversationMemory {
    pub fn new(pool: SqlitePool) -> Self {
        Self { pool }
    }

    /// 从 `Message` 变体提取 role 字符串（与 serde tag naming 一致）。
    fn message_role(msg: &Message) -> &'static str {
        match msg {
            Message::System { .. } => "system",
            Message::User { .. } => "user",
            Message::Assistant { .. } => "assistant",
        }
    }

    /// 从消息列表中提取首条 User 消息的文本作为标题。
    fn extract_title(messages: &[Message]) -> String {
        for msg in messages {
            if let Message::User { content } = msg {
                for item in content.iter() {
                    if let UserContent::Text(t) = item {
                        return t.text.chars().take(TITLE_MAX_CHARS).collect();
                    }
                }
            }
        }
        String::new()
    }
}

impl ConversationMemory for SqliteConversationMemory {
    fn load<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<Vec<Message>, MemoryError>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            let rows = crate::infra::data::conversations::load_recent_messages(
                &pool,
                conversation_id,
                SLIDING_WINDOW_SIZE,
            )
            .await
            .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            let mut messages = Vec::with_capacity(rows.len());
            for (_role, content) in rows {
                let msg: Message = serde_json::from_str(&content)
                    .map_err(|e| MemoryError::Backend(Box::from(e)))?;
                messages.push(msg);
            }
            Ok(messages)
        })
    }

    fn append<'a>(
        &'a self,
        conversation_id: &'a str,
        messages: Vec<Message>,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        let pool = self.pool.clone();

        Box::pin(async move {
            // 自动创建 conversation 记录（已存在则 IGNORE）
            let title = Self::extract_title(&messages);
            crate::infra::data::conversations::create_conversation(
                &pool,
                conversation_id,
                if title.is_empty() { None } else { Some(&title) },
            )
            .await
            .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            // 逐条插入消息
            for msg in &messages {
                let role = Self::message_role(msg);
                let content = serde_json::to_string(msg)
                    .map_err(|e| MemoryError::Backend(Box::from(e)))?;
                crate::infra::data::conversations::append_message(
                    &pool,
                    conversation_id,
                    role,
                    &content,
                )
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            }

            // 更新 last_active_at
            crate::infra::data::conversations::touch_conversation(&pool, conversation_id)
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;

            Ok(())
        })
    }

    fn clear<'a>(
        &'a self,
        conversation_id: &'a str,
    ) -> rig_core::wasm_compat::WasmBoxedFuture<'a, Result<(), MemoryError>> {
        let pool = self.pool.clone();
        Box::pin(async move {
            crate::infra::data::conversations::clear_messages(&pool, conversation_id)
                .await
                .map_err(|e| MemoryError::Backend(Box::from(e)))?;
            Ok(())
        })
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::completion::message::Text;
    use rig_core::one_or_many::OneOrMany;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        crate::infra::data::conversations::init_db(&pool)
            .await
            .expect("failed to init tables");
        pool
    }

    fn user_msg(text: &str) -> Message {
        Message::User {
            content: OneOrMany::one(UserContent::Text(Text::new(text))),
        }
    }

    fn assistant_msg(text: &str) -> Message {
        Message::Assistant {
            id: None,
            content: OneOrMany::one(rig_core::completion::message::AssistantContent::Text(
                Text::new(text),
            )),
        }
    }

    #[tokio::test]
    async fn append_and_load_roundtrip() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 空对话 load 返回空
        assert!(mem.load("c1").await.unwrap().is_empty());

        // append 两条
        mem.append("c1", vec![user_msg("hello"), assistant_msg("hi")])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);
    }

    #[tokio::test]
    async fn isolation_between_conversations() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("a", vec![user_msg("hi a")]).await.unwrap();
        mem.append("b", vec![user_msg("hi b")]).await.unwrap();

        assert_eq!(mem.load("a").await.unwrap().len(), 1);
        assert_eq!(mem.load("b").await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn clear_removes_history() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("c", vec![user_msg("x")]).await.unwrap();
        mem.clear("c").await.unwrap();
        assert!(mem.load("c").await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn sliding_window_truncates_old_messages() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 写入 30 条消息
        for i in 0..30 {
            mem.append("c1", vec![user_msg(&format!("msg {i}"))])
                .await
                .unwrap();
        }

        let loaded = mem.load("c1").await.unwrap();
        // 滑动窗口只返回最近 20 条
        assert_eq!(loaded.len(), 20);

        // 最近 20 条是 msg 10 ~ msg 29
        let first = match &loaded[0] {
            Message::User { content } => {
                match &content.first() {
                    UserContent::Text(t) => t.text.clone(),
                    _ => String::new(),
                }
            }
            _ => String::new(),
        };
        assert!(first.contains("msg 10"), "first should be msg 10, got: {first}");
    }

    #[tokio::test]
    async fn auto_creates_conversation_with_title() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        mem.append("c1", vec![user_msg("Hello world this is a test")])
            .await
            .unwrap();

        let convs = crate::infra::data::conversations::list_conversations(&mem.pool).await;
        assert_eq!(convs.len(), 1);
        assert_eq!(convs[0].title.as_deref(), Some("Hello world this is a test"));
    }

    #[tokio::test]
    async fn title_truncated_to_max_chars() {
        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        let long_text = "x".repeat(100);
        mem.append("c1", vec![user_msg(&long_text)])
            .await
            .unwrap();

        let convs = crate::infra::data::conversations::list_conversations(&mem.pool).await;
        assert_eq!(convs[0].title.as_deref().unwrap().chars().count(), TITLE_MAX_CHARS);
    }

    #[tokio::test]
    async fn message_preserves_tool_call_and_result() {
        use rig_core::completion::message::{ToolCall, ToolFunction};

        let pool = setup_pool().await;
        let mem = SqliteConversationMemory::new(pool);

        // 写入带 tool_call 的 assistant 消息
        let assistant_with_tool = Message::Assistant {
            id: None,
            content: OneOrMany::one(
                rig_core::completion::message::AssistantContent::ToolCall(ToolCall::new(
                    "call_1".to_string(),
                    ToolFunction::new("search".to_string(), serde_json::json!({"q": "test"})),
                )),
            ),
        };

        mem.append("c1", vec![user_msg("search for test"), assistant_with_tool])
            .await
            .unwrap();

        let loaded = mem.load("c1").await.unwrap();
        assert_eq!(loaded.len(), 2);

        // 第二条是 assistant 消息，应包含 ToolCall
        match &loaded[1] {
            Message::Assistant { content, .. } => {
                match &content.first() {
                    rig_core::completion::message::AssistantContent::ToolCall(tc) => {
                        assert_eq!(tc.function.name, "search");
                    }
                    _ => panic!("expected ToolCall in assistant message"),
                }
            }
            _ => panic!("expected Assistant message"),
        }
    }

    #[tokio::test]
    async fn arc_conversation_memory_forwards_to_inner() {
        let pool = setup_pool().await;
        let inner = std::sync::Arc::new(SqliteConversationMemory::new(pool));
        let mem: std::sync::Arc<dyn ConversationMemory> = inner.clone();

        mem.append("c", vec![user_msg("hello")]).await.unwrap();
        assert_eq!(mem.load("c").await.unwrap().len(), 1);
        mem.clear("c").await.unwrap();
        assert!(mem.load("c").await.unwrap().is_empty());
    }
}
