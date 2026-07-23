//! `AgentProvider` -- 对话窗口的 rig Agent 封装（0.12.1）。
//!
//! 与主窗口 `AIProvider`（类型收窄，仅 `complete`）对立--`AgentProvider` 开放 rig
//! `AgentBuilder` + memory + tool loop,只在对话窗口上下文可用。两者共享底层 rig
//! Client + Provider 配置,但能力面不同(§3.2 不破主窗口类型收窄铁则)。
//!
//! ## 泛型矛盾与枚举方案(Phase 0 spike 验证,见 `spike/agent.rs`)
//!
//! rig `Agent<M, ()>` 持 `Arc<M>`,`CompletionModel` 非 object-safe(3 关联类型),
//! 无法 `dyn Agent`。用 `ChatAgent` 枚举包 4 种 `ProviderKind` 的具体 `Agent<M>`,
//! `stream_prompt` 用泛型 `run_stream<M>` 分派 4 arm,消费 stream 转 `ChatStreamChunk`
//! emit 前端,外部不暴露 M / R。
//!
//! ## memory
//!
//! 0.12.1 用 `InMemoryConversationMemory`(进程内,重启丢),agent 自动 load/append
//! per `conversation_id`。0.12.2 换 `SqliteConversationMemory`(impl `ConversationMemory`)。
//!
//! ## tool 池
//!
//! 挂 `build_agent_tools()` 产出(0.12.0 Tool 适配层),含危险确认闭环骨架(`PendingConfirms`)。
//! tool loop 由 rig `Agent` 内部驱动,`default_max_turns(50)` 上限内无限轮。

use futures::StreamExt;
use tokio::sync::mpsc;

use rig_core::agent::{Agent, AgentBuilder, MultiTurnStreamItem};
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, GetTokenUsage};
use rig_core::memory::InMemoryConversationMemory;
use rig_core::providers::{anthropic, gemini, ollama, openai};
use rig_core::streaming::{StreamedAssistantContent, StreamingPrompt};
use rig_core::tool::ToolDyn;
use rig_core::wasm_compat::WasmCompatSend;

use crate::app::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use crate::domain::ai::factory::{
    build_anthropic_client, build_gemini_client, build_ollama_client, build_openai_client,
};
use crate::domain::ai::provider::AIError;
use crate::domain::ai::rig_provider::expose_for_rig;
use crate::infra::platform::secret;

/// 对话窗口流式输出 chunk(emit 前端 `blink://chat-stream`)。
///
/// `run_stream` 消费 rig `MultiTurnStreamItem` 转成此枚举,前端按 `kind` 渲染。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatStreamChunk {
    /// assistant 文本 delta(逐字流式)。
    Text { text: String },
    /// tool 调用(前端显示"正在调用 XXX")。
    ToolCall { tool: String },
    /// 一轮结束(`FinalResponse`)。
    Done,
    /// 流错误。
    Error { message: String },
}

/// 4 种 `ProviderKind` 的具体 `Agent<M>`(`CompletionModel` 非 object-safe,枚举包)。
enum ChatAgent {
    OpenAI(Agent<openai::completion::CompletionModel>),
    Anthropic(Agent<anthropic::completion::CompletionModel>),
    Gemini(Agent<gemini::completion::CompletionModel>),
    Ollama(Agent<ollama::CompletionModel>),
}

/// 对话窗口 Agent 封装--持有按当前 `ProviderEntry`+`ModelEntry` 构造的 rig `Agent`。
///
/// 构造时挂载 tool 池 + `InMemoryConversationMemory` + preamble,`stream_prompt` 驱动
/// agent loop 并把流式 chunk 经 channel emit 前端。
pub struct AgentProvider {
    agent: ChatAgent,
}

/// tool loop 上限(§4.4--0.11 主窗口固定 2 次,对话窗口放宽)。
const MAX_TURNS: usize = 50;

impl AgentProvider {
    /// 从 `ProviderEntry`+`ModelEntry` 构造 Agent。
    ///
    /// - 读密钥(本地 ollama 跳过,与 `RigFactory::build` 一致)
    /// - 按 `entry.kind` 构造 rig client + `completion_model` 得裸 model(复用 factory 的 `build_*_client`)
    /// - `AgentBuilder` 挂 preamble + tools + memory + `default_max_turns` -> `Agent<M>` -> `ChatAgent`
    ///
    /// **tools 被 move 进唯一命中的 match arm**(match 只走一个 arm,无需 Clone)。
    #[allow(dead_code)] // 0.12.1 Phase 4 commands 消费
    pub fn new(
        entry: &ProviderEntry,
        model: &ModelEntry,
        tools: Vec<Box<dyn ToolDyn>>,
        preamble: &str,
    ) -> Result<Self, AIError> {
        // 读密钥--与 RigFactory::build 一致(本地 provider 跳过)
        let key_str: String = if entry.kind.requires_secret() {
            let key = secret::load_secret(&entry.id, "key").map_err(|e| {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    "AgentProvider: {} 密钥未配置 ({e})",
                    entry.display_name,
                );
                AIError::NotConfigured
            })?;
            expose_for_rig(&key)
        } else {
            String::new()
        };

        let memory = InMemoryConversationMemory::new();
        let agent = match entry.kind {
            ProviderKind::OpenAICompatible => {
                let client = build_openai_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::OpenAI(build_agent(m, preamble, tools, memory))
            }
            ProviderKind::AnthropicMessages => {
                let client = build_anthropic_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Anthropic(build_agent(m, preamble, tools, memory))
            }
            ProviderKind::GeminiGenerateContent => {
                let client = build_gemini_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Gemini(build_agent(m, preamble, tools, memory))
            }
            ProviderKind::OllamaHttp => {
                let client = build_ollama_client(entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Ollama(build_agent(m, preamble, tools, memory))
            }
        };
        Ok(Self { agent })
    }

    /// 流式 prompt--驱动 agent loop,chunk 经 `tx` emit 前端。
    ///
    /// `conversation_id` 贯穿:rig agent 按 id 自动 load/append memory(0.12.1 进程内,
    /// 0.12.2 持久化)。调用方(commands 层)创建 channel,spawn 此 task,chunk 走
    /// `blink://chat-stream` 事件。
    #[allow(dead_code)] // 0.12.1 Phase 4 commands 消费
    pub async fn stream_prompt(
        &self,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
    ) {
        match &self.agent {
            ChatAgent::OpenAI(a) => Self::run_stream(a, conversation_id, user_msg, tx).await,
            ChatAgent::Anthropic(a) => Self::run_stream(a, conversation_id, user_msg, tx).await,
            ChatAgent::Gemini(a) => Self::run_stream(a, conversation_id, user_msg, tx).await,
            ChatAgent::Ollama(a) => Self::run_stream(a, conversation_id, user_msg, tx).await,
        }
    }

    /// 泛型 stream 消费--4 个 `ChatAgent` arm 共用,每个 arm 具体化 M。
    ///
    /// 消费 `MultiTurnStreamItem`(`#[non_exhaustive]`,须 `Ok(_)` 兜底):
    /// - `StreamAssistantItem(Text)` -> `ChatStreamChunk::Text`
    /// - `StreamAssistantItem(ToolCall)` -> `ChatStreamChunk::ToolCall`
    /// - `FinalResponse` -> `ChatStreamChunk::Done`
    /// - `Err` -> `ChatStreamChunk::Error`
    ///
    /// **中断**:调用方 drop `tx`(或 task 被 abort)即中断,stream 被 drop 后 rig 内部
    /// reqwest task 自动 abort(与主窗口 `RigProvider::stream` 一致)。
    async fn run_stream<M>(
        agent: &Agent<M>,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
    ) where
        M: CompletionModel + 'static,
        <M as CompletionModel>::StreamingResponse: WasmCompatSend + Clone + Unpin + GetTokenUsage,
    {
        let mut stream = agent.stream_prompt(user_msg).conversation(conversation_id).await;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                    StreamedAssistantContent::Text(t) => ChatStreamChunk::Text { text: t.text },
                    StreamedAssistantContent::ToolCall { tool_call, .. } => ChatStreamChunk::ToolCall {
                        tool: tool_call.function.name.clone(),
                    },
                    _ => continue,
                },
                Ok(MultiTurnStreamItem::FinalResponse(_)) => ChatStreamChunk::Done,
                Ok(_) => continue,
                Err(e) => ChatStreamChunk::Error {
                    message: format!("{e}"),
                },
            };
            if tx.send(chunk).is_err() {
                // 接收端关闭(用户中断/窗口关)--提前终止
                return;
            }
        }
    }
}

/// 构造 `Agent<M>`(4 arm 共用,泛型 M 由各 arm 具体化)。
#[allow(dead_code)] // 由 AgentProvider::new 调用;0.12.1 Phase 4 commands 消费 new
fn build_agent<M>(
    model: M,
    preamble: &str,
    tools: Vec<Box<dyn ToolDyn>>,
    memory: InMemoryConversationMemory,
) -> Agent<M>
where
    M: CompletionModel + 'static,
{
    AgentBuilder::new(model)
        .preamble(preamble)
        .tools(tools)
        .memory(memory)
        .default_max_turns(MAX_TURNS)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;
    use rig_core::test_utils::{MockCompletionModel, MockResponse, MockStreamEvent};

    /// 验证 `run_stream` 把 `MultiTurnStreamItem` 正确转 `ChatStreamChunk`。
    /// 用 `MockCompletionModel`(不依赖网络/密钥),绕过 `AgentProvider::new`。
    #[tokio::test]
    async fn run_stream_emits_text_then_done() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::FinalResponse(MockResponse::new()),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Text { text } if text == "hello")),
            "应 emit Text(hello): {chunks:?}"
        );
        assert!(
            chunks.iter().any(|c| matches!(c, ChatStreamChunk::Done)),
            "应以 Done 收尾: {chunks:?}"
        );
    }

    /// 验证 tool 调用 emit `ToolCall` chunk。
    #[tokio::test]
    async fn run_stream_emits_toolcall() {
        use rig_core::test_utils::MockAddTool;
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::tool_call("call_1", "add", serde_json::json!({"a":1,"b":2})),
            MockStreamEvent::FinalResponse(MockResponse::new()),
        ]]);
        // 挂 MockAddTool(name="add")--agent 接受 model 的 tool_call,否则 UnknownToolCall
        let agent = AgentBuilder::new(model)
            .tool(MockAddTool)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::ToolCall { tool } if tool == "add")),
            "应 emit ToolCall(add): {chunks:?}"
        );
    }

    /// 验证 `ChatStreamChunk` 序列化带 `kind` tag(前端按 kind 分派)。
    #[test]
    fn chat_stream_chunk_serializes_with_kind_tag() {
        let text = ChatStreamChunk::Text {
            text: "hi".into(),
        };
        let v = serde_json::to_value(&text).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "hi");

        let done = ChatStreamChunk::Done;
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["kind"], "done");
    }
}
