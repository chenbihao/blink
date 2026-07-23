//! Phase 0 spike--rig 0.39 Agent API 验证（0.12.1 `AgentProvider` 前置）。
//!
//! 验证 `AgentProvider` 设计依赖的 rig API 链路（不依赖网络/密钥，用 rig `test_utils`）：
//! 1. `AgentBuilder` 构造链：`.tools(Vec<Box<dyn ToolDyn>>) + .memory() + .default_max_turns() + .build()`
//! 2. `Agent::stream_prompt(msg).conversation(id).await` -> `Stream<MultiTurnStreamItem>`
//! 3. `MultiTurnStreamItem` 4 变体消费（`StreamAssistantItem` / `StreamUserItem` / `CompletionCall` / `FinalResponse`）
//! 4. `InMemoryConversationMemory` 自动 load/append per `conversation_id`
//! 5. 4 种 `ProviderKind` 的 `CompletionModel` 类型路径正确（`ChatModel`/`ChatAgent` 枚举编译）
//!
//! **结论**：spike 走通后，Phase 1 `AgentProvider` 照此链路实现--枚举包 4 种 `Agent<M>`，
//! `stream_prompt` 用泛型 `run_stream<M>` 分派，memory 用 `InMemoryConversationMemory`（0.12.2 换 SQLite）。
//!
//! **`#[cfg(test)]` 门槛**：继承自 `spike/mod.rs`，release 零残留。

use futures::StreamExt;

use rig_core::agent::{Agent, AgentBuilder, MultiTurnStreamItem, StreamingResult};
use rig_core::memory::{ConversationMemory, InMemoryConversationMemory};
use rig_core::streaming::{StreamedAssistantContent, StreamingPrompt};
use rig_core::test_utils::{MockCompletionModel, MockResponse, MockStreamEvent};

// ── 验证 5: 4 种 ProviderKind CompletionModel 类型路径（枚举编译即验证）──
//
// `CompletionModel` 非 object-safe（3 个关联类型），用枚举包 4 种具体类型。
// 类型路径错则枚举定义编译失败。Phase 1 `AgentProvider` 的 `ChatModel`/`ChatAgent` 照此定义。
#[allow(dead_code)]
enum ChatModel {
    OpenAI(rig_core::providers::openai::completion::CompletionModel),
    Anthropic(rig_core::providers::anthropic::completion::CompletionModel),
    Gemini(rig_core::providers::gemini::completion::CompletionModel),
    Ollama(rig_core::providers::ollama::CompletionModel),
}

#[allow(dead_code)]
enum ChatAgent {
    OpenAI(Agent<rig_core::providers::openai::completion::CompletionModel>),
    Anthropic(Agent<rig_core::providers::anthropic::completion::CompletionModel>),
    Gemini(Agent<rig_core::providers::gemini::completion::CompletionModel>),
    Ollama(Agent<rig_core::providers::ollama::CompletionModel>),
}

// ── 验证 1: AgentBuilder 构造链 ──────────────────────────────────────────

/// `AgentBuilder::tools(Vec<Box<dyn ToolDyn>>)` 接受 `build_agent_tools()` 输出，
/// `.memory()` 挂 `ConversationMemory`，`.default_max_turns()` 设 tool loop 上限，`.build()` 出 `Agent`。
#[tokio::test]
async fn agent_builder_accepts_tooldyn_pool_and_memory() {
    let model = MockCompletionModel::from_stream_turns(vec![vec![
        MockStreamEvent::text("hi"),
        MockStreamEvent::FinalResponse(MockResponse::new()),
    ]]);

    // `build_agent_tools()` 的返回类型--验证 AgentBuilder.tools 签名接受
    let tools: Vec<Box<dyn rig_core::tool::ToolDyn>> = Vec::new();

    let agent = AgentBuilder::new(model)
        .preamble("test preamble")
        .tools(tools)
        .memory(InMemoryConversationMemory::new())
        .default_max_turns(50)
        .build();

    // build 成功即证明构造链编译通过
    drop(agent);
}

// ── 验证 2 + 3: stream_prompt 链路 + MultiTurnStreamItem 消费 ──────────────

/// `agent.stream_prompt(msg).conversation(id).await` 得到 `Stream<MultiTurnStreamItem>`，
/// 逐 item 消费：`StreamAssistantItem(Text)` 收文本 delta，`FinalResponse` 收尾。
#[tokio::test]
async fn stream_prompt_yields_assistant_text_then_final() {
    let model = MockCompletionModel::from_stream_turns(vec![vec![
        MockStreamEvent::text("hello"),
        MockStreamEvent::text(" world"),
        MockStreamEvent::FinalResponse(MockResponse::new()),
    ]]);
    let agent = AgentBuilder::new(model)
        .memory(InMemoryConversationMemory::new())
        .default_max_turns(5)
        .build();

    let mut stream = agent.stream_prompt("hi").conversation("c1").await;
    let mut text = String::new();
    let mut got_final = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => {
                if let StreamedAssistantContent::Text(t) = content {
                    text.push_str(&t.text);
                }
            }
            Ok(MultiTurnStreamItem::FinalResponse(_)) => {
                got_final = true;
            }
            Ok(_) => {} // CompletionCall / StreamUserItem / 其他（non_exhaustive 兜底）
            Err(e) => panic!("stream error: {e:?}"),
        }
    }
    assert_eq!(text, "hello world", "应拼出完整文本");
    assert!(got_final, "应以 FinalResponse 收尾");
}

// ── 验证 4: memory 自动 load/append per conversation_id ────────────────────

/// agent prompt 后，`InMemoryConversationMemory` 自动 append（user + assistant），
/// 同 conversation 跨轮历史增长，不同 conversation 隔离。
#[tokio::test]
async fn memory_persists_across_turns_per_conversation() {
    let memory = InMemoryConversationMemory::new();
    let memory_probe = memory.clone(); // 共享 inner（Arc<Mutex<HashMap>>）

    let model = MockCompletionModel::from_stream_turns(vec![
        vec![
            MockStreamEvent::text("first"),
            MockStreamEvent::FinalResponse(MockResponse::new()),
        ],
        vec![
            MockStreamEvent::text("second"),
            MockStreamEvent::FinalResponse(MockResponse::new()),
        ],
    ]);
    let agent = AgentBuilder::new(model)
        .memory(memory)
        .default_max_turns(5)
        .build();

    // 第一轮
    consume_stream(agent.stream_prompt("msg1").conversation("c1").await).await;
    let history_after_1 = memory_probe.load("c1").await.unwrap();
    assert!(
        !history_after_1.is_empty(),
        "第一轮后 memory 应有历史（agent 自动 append）"
    );

    // 第二轮（同 conversation）--agent 应 load 上一轮 + append 本轮
    consume_stream(agent.stream_prompt("msg2").conversation("c1").await).await;
    let history_after_2 = memory_probe.load("c1").await.unwrap();
    assert!(
        history_after_2.len() > history_after_1.len(),
        "第二轮后历史应增长（跨轮 memory 生效）"
    );

    // 不同 conversation 隔离
    let history_c2 = memory_probe.load("c2").await.unwrap();
    assert!(history_c2.is_empty(), "c2 无历史（conversation 隔离）");
}

/// 消费 stream 到结束（丢弃 item 内容）--复用于多轮 memory 验证。
async fn consume_stream<R>(mut stream: StreamingResult<R>) {
    while stream.next().await.is_some() {}
}
