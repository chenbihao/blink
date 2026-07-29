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
//! 0.13.1: `new()` 接收 `Arc<SqliteConversationMemory>`（具体类型），构造时注入
//! `ModelEntry.context_window` 到 memory 配置，驱动 token-aware 窗口裁剪。
//!
//! ## tool 池
//!
//! 挂 `build_agent_tools()` 产出(0.12.0 Tool 适配层),含危险确认闭环骨架(`PendingConfirms`)。
//! tool loop 由 rig `Agent` 内部驱动,`default_max_turns(50)` 上限内无限轮。

use futures::StreamExt;
use tokio::sync::mpsc;

use rig_core::agent::{Agent, AgentBuilder, MultiTurnStreamItem};
use rig_core::client::CompletionClient;
use rig_core::completion::{CompletionModel, GetTokenUsage, message::ToolResult};
use rig_core::memory::ConversationMemory;
#[cfg(test)]
use rig_core::memory::InMemoryConversationMemory;
use rig_core::providers::{anthropic, gemini, ollama, openai};
use rig_core::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use rig_core::tool::ToolDyn;
use rig_core::wasm_compat::WasmCompatSend;

use crate::app::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use std::sync::Arc;

/// Anthropic API 要求 max_tokens 必填，若 model 层未配置则使用此默认值。
/// 与 `rig_provider::build_rig_request` 中的 `ANTHROPIC_DEFAULT_MAX_TOKENS` 保持一致。
const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;

use crate::domain::ai::factory::{
    build_anthropic_client, build_gemini_client, build_ollama_client, build_openai_client,
};
use crate::domain::ai::memory::SqliteConversationMemory;
use crate::domain::ai::provider::AIError;
use crate::domain::ai::rig_provider::expose_for_rig;
use crate::infra::platform::secret;

/// 对话窗口流式输出 chunk(emit 前端 `blink://chat-stream`)。
///
/// `run_stream` 消费 rig `MultiTurnStreamItem` 转成此枚举,前端按 `kind` 渲染。
///
/// 0.12.2 扩展:
/// - `ToolCall` 加 `call_id`(`rig internal_call_id`),供与 `ToolResult` 配对。
/// - 新增 `ToolResult`(来自 `StreamUserItem`),携带摘要(前 50000 字符,图片转 `[image]`)。
/// - `Done` 携带 `input_tokens`/`output_tokens`(从 `FinalResponse.usage()` 提取)。
#[derive(Clone, Debug, serde::Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ChatStreamChunk {
    /// assistant 文本 delta(逐字流式)。
    Text { text: String },
    /// 思考/reasoning delta(逐字流式,前端折叠展示)。
    Thinking { text: String },
    /// tool 调用(前端显示"正在调用 XXX")。
    ///
    /// `call_id` 是 rig 生成的 `internal_call_id`,用于与后续 `ToolResult` 配对,
    /// 前端据此把结果摘要挂到对应 ToolCall 卡片。
    /// 0.12.7：`arguments` 携带工具参数 JSON 字符串，前端折叠展示。
    ToolCall { tool: String, call_id: String, arguments: String },
    /// tool 执行结果(来自 rig `StreamUserItem`)。
    ///
    /// `call_id` 与 `ToolCall.call_id` 配对。`summary` 为结果文本(前 50000 字符);
    /// 图片内容以 `[image]` 占位。`success` 由 `content` 是否为空推断。
    ToolResult {
        call_id: String,
        success: bool,
        summary: String,
    },
    /// 一轮结束(`FinalResponse`),携带 token 用量 + 模型名。
    /// 0.12.3：model_name 供前端在气泡左下角显示。
    Done {
        input_tokens: u32,
        output_tokens: u32,
        #[serde(skip_serializing_if = "Option::is_none")]
        model_name: Option<String>,
    },
    /// 已达 tool loop 上限（0.12.3 Phase C）。
    ///
    /// rig `default_max_turns(50)` 耗尽时 emit。前端显示"已达工具调用上限"提示。
    MaxTurnsReached { max_turns: usize },
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
    /// 当前模型显示名（供 Done chunk 携带，前端在气泡左下角显示）
    model_name: String,
}

/// tool loop 上限(§4.4--0.11 主窗口固定 2 次,对话窗口放宽)。
const MAX_TURNS: usize = 50;

impl AgentProvider {
    /// 从 `ProviderEntry`+`ModelEntry` 构造 Agent。
    ///
    /// - `memory` 由 ChatService 持有，切换 Provider/Model 时复用同一份 memory。
    /// - 0.13.1: 构造时注入 `model.context_window` 到 memory 配置，驱动 token-aware 裁剪。
    /// - 读密钥(本地 ollama 跳过,与 `RigFactory::build` 一致)
    /// - 按 `entry.kind` 构造 rig client + `completion_model` 得裸 model(复用 factory 的 `build_*_client`)
    /// - `AgentBuilder` 挂 preamble + tools + memory + `default_max_turns` -> `Agent<M>` -> `ChatAgent`
    ///
    /// **tools 被 move 进唯一命中的 match arm**(match 只走一个 arm,无需 Clone)。
    pub async fn new(
        entry: &ProviderEntry,
        model: &ModelEntry,
        tools: Vec<Box<dyn ToolDyn>>,
        preamble: &str,
        memory: Arc<SqliteConversationMemory>,
    ) -> Result<Self, AIError> {
        // 0.13.1: 注入模型 context_window 到 memory 配置
        let context_limit = model.context_window.map(|u| u as usize);
        memory.update_context_limit(context_limit).await;
        tracing::debug!(
            model = %model.id,
            context_limit = ?context_limit,
            "AgentProvider: 已注入 context_limit 到 memory"
        );

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

        // SqliteConversationMemory -> Arc<dyn ConversationMemory>（rig AgentBuilder 需要 trait object）
        let memory_dyn: Arc<dyn ConversationMemory> = memory;

        // 0.13.x: 将 model 层的 temperature / max_tokens 传入 build_agent，
        // 让 rig AgentBuilder 在构造时固化默认值。Anthropic 的 max_tokens 是必填字段，
        // 若 model 层未配置则使用 ANTHROPIC_DEFAULT_MAX_TOKENS 兜底。
        let default_temperature = model.temperature.map(|f| f as f64);
        let default_max_tokens = model.max_tokens.map(|n| n as u64);

        let agent = match entry.kind {
            ProviderKind::OpenAICompatible => {
                let client = build_openai_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::OpenAI(build_agent(
                    m,
                    preamble,
                    tools,
                    memory_dyn,
                    default_temperature,
                    default_max_tokens,
                ))
            }
            ProviderKind::AnthropicMessages => {
                let client = build_anthropic_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                // Anthropic max_tokens 必填，兜底 4096
                let anthropic_max_tokens =
                    default_max_tokens.unwrap_or(ANTHROPIC_DEFAULT_MAX_TOKENS);
                if default_max_tokens.is_none() {
                    tracing::debug!(
                        "AgentProvider: Anthropic max_tokens 未配置，使用默认值 {}",
                        ANTHROPIC_DEFAULT_MAX_TOKENS
                    );
                }
                ChatAgent::Anthropic(build_agent(
                    m,
                    preamble,
                    tools,
                    memory_dyn,
                    default_temperature,
                    Some(anthropic_max_tokens),
                ))
            }
            ProviderKind::GeminiGenerateContent => {
                let client = build_gemini_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Gemini(build_agent(
                    m,
                    preamble,
                    tools,
                    memory_dyn,
                    default_temperature,
                    default_max_tokens,
                ))
            }
            ProviderKind::OllamaHttp => {
                let client = build_ollama_client(entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Ollama(build_agent(
                    m,
                    preamble,
                    tools,
                    memory_dyn,
                    default_temperature,
                    default_max_tokens,
                ))
            }
        };
        Ok(Self { agent, model_name: model_display_name(entry, model) })
    }

    /// 流式 prompt--驱动 agent loop,chunk 经 `tx` emit 前端。
    ///
    /// `conversation_id` 贯穿:rig agent 按 id 自动 load/append memory(0.12.1 进程内,
    /// 0.12.2 持久化)。调用方(commands 层)创建 channel,spawn 此 task,chunk 走
    /// `blink://chat-stream` 事件。
    pub async fn stream_prompt(
        &self,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
    ) {
        let model_name = if self.model_name.is_empty() { None } else { Some(self.model_name.clone()) };
        match &self.agent {
            ChatAgent::OpenAI(a) => Self::run_stream(a, conversation_id, user_msg, tx, model_name).await,
            ChatAgent::Anthropic(a) => Self::run_stream(a, conversation_id, user_msg, tx, model_name).await,
            ChatAgent::Gemini(a) => Self::run_stream(a, conversation_id, user_msg, tx, model_name).await,
            ChatAgent::Ollama(a) => Self::run_stream(a, conversation_id, user_msg, tx, model_name).await,
        }
    }

    /// 泛型 stream 消费--4 个 `ChatAgent` arm 共用,每个 arm 具体化 M。
    ///
    /// 消费 `MultiTurnStreamItem`(`#[non_exhaustive]`,须 `Ok(_)` 兜底):
    /// - `StreamAssistantItem(Text)` -> `ChatStreamChunk::Text`
    /// - `StreamAssistantItem(Reasoning)` -> `ChatStreamChunk::Thinking`
    /// - `StreamAssistantItem(ReasoningDelta)` -> `ChatStreamChunk::Thinking`
    /// - `StreamAssistantItem(ToolCall)` -> `ChatStreamChunk::ToolCall { tool, call_id }`
    ///   (保留 `internal_call_id` 供与 ToolResult 配对)
    /// - `StreamUserItem(ToolResult)` -> `ChatStreamChunk::ToolResult { call_id, summary }`
    ///   (0.12.2 新增:rig tool loop 内部 tool 执行结果,摘要前 200 字符)
    /// - `FinalResponse(resp)` -> `ChatStreamChunk::Done { input_tokens, output_tokens }`
    ///   (0.12.2: 从 `resp.usage()` 提取,`u64` 截断到 `u32`,与 `map_rig_response` 一致)
    /// - `Err` -> `ChatStreamChunk::Error`
    ///
    /// **中断**:调用方 drop `tx`(或 task 被 abort)即中断,stream 被 drop 后 rig 内部
    /// reqwest task 自动 abort(与主窗口 `RigProvider::stream` 一致)。
    async fn run_stream<M>(
        agent: &Agent<M>,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
    ) where
        M: CompletionModel + 'static,
        <M as CompletionModel>::StreamingResponse: WasmCompatSend + Clone + Unpin + GetTokenUsage,
    {
        let mut stream = agent
            .stream_prompt(user_msg)
            .conversation(conversation_id)
            .await;
        let mut done_sent = false;
        // 跟踪是否收到过实质内容（Text/Thinking/ToolCall/ToolResult）。
        // rig 在 SSE 解析失败时可能 yield 一个空 FinalResponse（0 token + 无内容），
        // 需区分"正常空回复"与"请求根本未处理"——后者转为 Error 上报前端。
        let mut has_content = false;
        while let Some(item) = stream.next().await {
            let chunk = match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                    StreamedAssistantContent::Text(t) => {
                        has_content = true;
                        ChatStreamChunk::Text { text: t.text }
                    }
                    StreamedAssistantContent::Reasoning(r) => {
                        has_content = true;
                        ChatStreamChunk::Thinking { text: r.display_text() }
                    }
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                        has_content = true;
                        ChatStreamChunk::Thinking { text: reasoning }
                    }
                    StreamedAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id,
                    } => {
                        has_content = true;
                        ChatStreamChunk::ToolCall {
                            tool: tool_call.function.name.clone(),
                            call_id: internal_call_id,
                            arguments: tool_call.function.arguments.to_string(),
                        }
                    }
                    _ => continue,
                },
                Ok(MultiTurnStreamItem::StreamUserItem(StreamedUserContent::ToolResult {
                    tool_result,
                    internal_call_id,
                })) => {
                    has_content = true;
                    let summary = summarize_tool_result(&tool_result);
                    // 空内容视为失败（如被拒绝的危险 tool / tool 报错）
                    let success = !summary.is_empty();
                    ChatStreamChunk::ToolResult {
                        call_id: internal_call_id,
                        success,
                        summary,
                    }
                }
                Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                    done_sent = true;
                    let usage = resp.usage();
                    // 空响应检测：0 token + 无任何内容 → SSE 解析失败 / 服务过载 / 配额耗尽
                    // rig 在这些场景下不 yield Err，而是 yield 一个空 FinalResponse，
                    // 若直接发 Done 前端只显示空气泡无任何提示。
                    if !has_content && usage.input_tokens == 0 && usage.output_tokens == 0 {
                        tracing::warn!(
                            conversation = %conversation_id,
                            "run_stream: 收到空 FinalResponse（0 token + 无内容），\
                             可能是服务过载 / SSE 解析失败 / 配额耗尽"
                        );
                        ChatStreamChunk::Error {
                            message:
                                "AI 返回了无效响应，可能是服务过载或配额耗尽，请稍后重试"
                                    .to_string(),
                        }
                    } else {
                        ChatStreamChunk::Done {
                            // rig Usage 是 u64,截断到 u32(与 map_rig_response 一致)
                            input_tokens: usage.input_tokens.min(u32::MAX as u64) as u32,
                            output_tokens: usage.output_tokens.min(u32::MAX as u64) as u32,
                            model_name: model_name.clone(),
                        }
                    }
                }
                Ok(_) => {
                    tracing::trace!("run_stream: unknown MultiTurnStreamItem variant, skipped");
                    continue;
                }
                Err(e) => {
                    // 0.12.3 Phase C: 检测 MaxTurnsError 并 emit MaxTurnsReached
                    // 注意：这里用字符串匹配检测 MaxTurnsError，如果 rig 升级后错误消息
                    // 格式变化，此检测会失效。若 rig 后续暴露类型化错误，应改用 downcast_ref。
                    let msg = format!("{e}");
                    if msg.contains("MaxTurnsError") || msg.contains("max turns") {
                        ChatStreamChunk::MaxTurnsReached {
                            max_turns: MAX_TURNS,
                        }
                    } else {
                        // 0.12.9：后端日志记录 SSE / provider 错误详情，便于排查 400 等问题
                        tracing::warn!(
                            target: crate::infra::utils::perf::ai_slo::TARGET,
                            conversation = %conversation_id,
                            error = %msg,
                            error_debug = ?e,
                            "run_stream: 流式生成错误"
                        );
                        ChatStreamChunk::Error { message: msg }
                    }
                }
            };
            if tx.send(chunk).is_err() {
                // 接收端关闭(用户中断/窗口关)--提前终止
                return;
            }
        }
        // 0.12.5：stream 结束但未发送 Done（rig-core 跳过了 SSE 解析错误后连接关闭）
        // → 发送 Error chunk，避免前端永远收不到结束事件而卡死
        if !done_sent {
            tracing::warn!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                conversation = %conversation_id,
                "run_stream: 流结束但未收到 Done chunk（SSE 解析异常 / 连接被服务端关闭）"
            );
            let _ = tx.send(ChatStreamChunk::Error {
                message: "AI 返回了无效响应，可能是服务过载或配额耗尽，请稍后重试".to_string(),
            });
        }
    }
}

/// 提取模型显示名（优先 display_name，回退 model id）。
fn model_display_name(_entry: &ProviderEntry, model: &ModelEntry) -> String {
    if !model.display_name.is_empty() {
        model.display_name.clone()
    } else {
        model.id.clone()
    }
}

/// 从 rig `ToolResult` 提取前端展示摘要(0.12.2 §4.7)。
///
/// - 文本内容拼接,截前 200 字符。
/// - 图片内容转 `[image]` 占位(前端暂不展示图片)。
/// - 多个 content item 用换行分隔。
const TOOL_RESULT_SUMMARY_MAX: usize = 50000;

/// 从 rig `ToolResult` 提取前端展示摘要(0.12.2 §4.7)。
///
/// - 文本内容拼接,截前 50000 字符。
/// - 图片内容转 `[image]` 占位(前端暂不展示图片)。
/// - 多个 content item 用换行分隔。
///
/// 0.14.1: 提升为 `pub(crate)`，供 `app/commands.rs` 对话历史加载复用，
/// 消除内联 match 重复。
pub(crate) fn summarize_tool_result(tool_result: &ToolResult) -> String {
    use rig_core::completion::message::ToolResultContent;
    let mut parts: Vec<String> = Vec::new();
    for content in tool_result.content.iter() {
        match content {
            ToolResultContent::Text(t) => parts.push(t.text.clone()),
            ToolResultContent::Image(_) => parts.push("[image]".to_string()),
        }
    }
    let joined = parts.join("\n");
    // 按 char 截断(避免切坏 UTF-8),超长加省略号
    if joined.chars().count() <= TOOL_RESULT_SUMMARY_MAX {
        joined
    } else {
        let truncated: String = joined.chars().take(TOOL_RESULT_SUMMARY_MAX).collect();
        format!("{truncated}…")
    }
}

/// 构造 `Agent<M>`(4 arm 共用,泛型 M 由各 arm 具体化)。
///
/// `default_temperature` / `default_max_tokens` 从 `ModelEntry` 来，
/// 构造时固化到 `AgentBuilder`，rig Agent 内部生成 `CompletionRequest` 时使用。
/// `None` 表示不覆盖（rig 让 provider 自己决定），但 Anthropic 的 `max_tokens` 是
/// 必填字段，调用方需确保传入 `Some`（见 `AgentProvider::new` 中的兜底逻辑）。
fn build_agent<M>(
    model: M,
    preamble: &str,
    tools: Vec<Box<dyn ToolDyn>>,
    memory: Arc<dyn ConversationMemory>,
    default_temperature: Option<f64>,
    default_max_tokens: Option<u64>,
) -> Agent<M>
where
    M: CompletionModel + 'static,
{
    let mut builder = AgentBuilder::new(model)
        .preamble(preamble)
        .tools(tools)
        .memory(memory)
        .default_max_turns(MAX_TURNS);

    if let Some(temp) = default_temperature {
        builder = builder.temperature(temp);
    }
    if let Some(max_tok) = default_max_tokens {
        builder = builder.max_tokens(max_tok);
    }

    builder.build()
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
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

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
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Done { .. })),
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
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::ToolCall { tool, .. } if tool == "add")),
            "应 emit ToolCall(add): {chunks:?}"
        );
        // 0.12.2: ToolCall 必须带 call_id 供 ToolResult 配对
        assert!(
            chunks.iter().any(|c| matches!(
                c,
                ChatStreamChunk::ToolCall { call_id, .. } if !call_id.is_empty()
            )),
            "ToolCall 必须带非空 call_id: {chunks:?}"
        );
    }

    /// 验证 tool 执行后 emit `ToolResult` chunk,且 `call_id` 与对应 ToolCall 配对(0.12.2 §4.7)。
    #[tokio::test]
    async fn run_stream_emits_tool_result_paired_with_tool_call() {
        use rig_core::test_utils::MockAddTool;
        let model = MockCompletionModel::from_stream_turns(vec![
            // 第 1 轮:模型发起 tool_call
            vec![
                MockStreamEvent::tool_call("call_1", "add", serde_json::json!({"x":1,"y":2})),
                MockStreamEvent::final_response_with_default_usage(),
            ],
            // 第 2 轮:模型拿到结果后给出文本回复 + 收尾
            vec![
                MockStreamEvent::text("result is 3"),
                MockStreamEvent::final_response_with_default_usage(),
            ],
        ]);
        let agent = AgentBuilder::new(model)
            .tool(MockAddTool)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }

        // 应同时存在 ToolCall 和 ToolResult,且 call_id 可配对
        let tool_call_id = chunks.iter().find_map(|c| match c {
            ChatStreamChunk::ToolCall { tool, call_id, .. } if tool == "add" => Some(call_id.clone()),
            _ => None,
        });
        assert!(tool_call_id.is_some(), "应有 ToolCall(add): {chunks:?}");

        let paired_result = chunks.iter().find_map(|c| match c {
            ChatStreamChunk::ToolResult {
                call_id,
                success,
                summary,
            } if call_id == tool_call_id.as_deref().unwrap_or("") => {
                Some((*success, summary.clone()))
            }
            _ => None,
        });
        assert!(
            paired_result.is_some(),
            "应有与 ToolCall 同 call_id 的 ToolResult: {chunks:?}"
        );
        let (success, summary) = paired_result.unwrap();
        assert!(success, "成功 tool 的 success 应为 true");
        assert!(
            summary.contains('3'),
            "摘要应包含 tool 结果 3: {summary}"
        );
    }

    /// 验证 `Done` chunk 携带从 `FinalResponse.usage()` 提取的 token 用量(0.12.2 §4.8)。
    #[tokio::test]
    async fn run_stream_done_carries_usage() {
        use rig_core::completion::Usage as RigUsage;
        let usage = RigUsage {
            input_tokens: 150,
            output_tokens: 80,
            total_tokens: 230,
            ..Default::default()
        };
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response(usage),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        let done = chunks.iter().find_map(|c| match c {
            ChatStreamChunk::Done {
                input_tokens,
                output_tokens,
                model_name: _,
            } => Some((*input_tokens, *output_tokens)),
            _ => None,
        });
        let (input_tokens, output_tokens) =
            done.expect("应 emit Done chunk: {chunks:?}");
        assert_eq!(input_tokens, 150, "Done.input_tokens 应为 150");
        assert_eq!(output_tokens, 80, "Done.output_tokens 应为 80");
    }

    /// 验证 `Done` 的 u64→u32 截断(0.12.2 §4.8,与 map_rig_response 一致)。
    #[tokio::test]
    async fn run_stream_done_truncates_oversized_usage() {
        use rig_core::completion::Usage as RigUsage;
        let usage = RigUsage {
            input_tokens: u64::from(u32::MAX) + 1000, // 超 u32 范围
            output_tokens: 50,
            total_tokens: 0,
            ..Default::default()
        };
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hi"),
            MockStreamEvent::final_response(usage),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        let (input_tokens, _) = chunks
            .iter()
            .find_map(|c| match c {
                ChatStreamChunk::Done {
                    input_tokens,
                    output_tokens,
                    model_name: _,
                } => Some((*input_tokens, *output_tokens)),
                _ => None,
            })
            .expect("应 emit Done");
        assert_eq!(
            input_tokens, u32::MAX,
            "超 u32 的 input_tokens 应截断到 u32::MAX"
        );
    }

    /// 验证空 FinalResponse（0 token + 无内容）转为 Error chunk（0.12.6 修复）。
    ///
    /// rig 在 SSE 解析失败 / 服务过载时可能 yield 一个空 FinalResponse 而非 Err，
    /// 若直接发 Done 前端只显示空气泡无任何错误提示。此测试确保该场景被正确检测。
    #[tokio::test]
    async fn run_stream_empty_final_response_becomes_error() {
        use rig_core::completion::Usage as RigUsage;
        let zero_usage = RigUsage::default();
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::final_response(zero_usage),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        // 应 emit Error，而非 Done
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Error { .. })),
            "空 FinalResponse 应转为 Error: {chunks:?}"
        );
        assert!(
            !chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Done { .. })),
            "空 FinalResponse 不应 emit Done: {chunks:?}"
        );
    }

    /// 验证 `MaxTurnsReached` chunk 序列化（0.12.3 Phase C）。
    #[test]
    fn chat_stream_chunk_max_turns_reached_serializes() {
        let chunk = ChatStreamChunk::MaxTurnsReached { max_turns: 50 };
        let v = serde_json::to_value(&chunk).unwrap();
        assert_eq!(v["kind"], "max_turns_reached");
        assert_eq!(v["max_turns"], 50);
    }

    /// 验证 `summarize_tool_result` 文本截断与图片占位(纯函数测试)。
    #[test]
    fn summarize_tool_result_truncates_and_handles_image() {
        use rig_core::completion::message::{
            DocumentSourceKind, Image, Text, ToolResultContent,
        };
        use rig_core::one_or_many::OneOrMany;

        // 短文本不截断
        let short = ToolResult {
            id: "1".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Text(Text::new("ok"))),
        };
        assert_eq!(summarize_tool_result(&short), "ok");

        // 长文本截断到 50000 字符 + 省略号
        let long_text = "x".repeat(60000);
        let long = ToolResult {
            id: "2".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Text(Text::new(long_text))),
        };
        let summary = summarize_tool_result(&long);
        assert_eq!(summary.chars().count(), 50001, "50000 字符 + 1 省略号");
        assert!(summary.ends_with('…'));

        // 图片转占位
        let img = ToolResult {
            id: "3".into(),
            call_id: None,
            content: OneOrMany::one(ToolResultContent::Image(Image {
                data: DocumentSourceKind::Url("http://example.com/x.png".into()),
                media_type: None,
                detail: None,
                additional_params: None,
            })),
        };
        assert_eq!(summarize_tool_result(&img), "[image]");
    }

    /// 验证 thinking chunk 序列化为 `kind: "thinking"`。
    #[test]
    fn chat_stream_chunk_thinking_serializes() {
        let chunk = ChatStreamChunk::Thinking {
            text: "let me think...".into(),
        };
        let v = serde_json::to_value(&chunk).unwrap();
        assert_eq!(v["kind"], "thinking");
        assert_eq!(v["text"], "let me think...");
    }

    /// 验证 `ChatStreamChunk` 序列化带 `kind` tag(前端按 kind 分派)。
    #[test]
    fn chat_stream_chunk_serializes_with_kind_tag() {
        let text = ChatStreamChunk::Text { text: "hi".into() };
        let v = serde_json::to_value(&text).unwrap();
        assert_eq!(v["kind"], "text");
        assert_eq!(v["text"], "hi");

        let thinking = ChatStreamChunk::Thinking {
            text: "reasoning...".into(),
        };
        let v = serde_json::to_value(&thinking).unwrap();
        assert_eq!(v["kind"], "thinking");

        let done = ChatStreamChunk::Done {
            input_tokens: 10,
            output_tokens: 20,
            model_name: None,
        };
        let v = serde_json::to_value(&done).unwrap();
        assert_eq!(v["kind"], "done");
        assert_eq!(v["input_tokens"], 10);
        assert_eq!(v["output_tokens"], 20);
    }

    /// 验证 `ToolResult` 和 `ToolCall` 序列化字段(0.12.2 §4.7)。
    #[test]
    fn chat_stream_chunk_tool_result_and_call_serialize() {
        let tool_call = ChatStreamChunk::ToolCall {
            tool: "search_apps".into(),
            call_id: "cid_1".into(),
            arguments: "{\"query\":\"test\"}".into(),
        };
        let v = serde_json::to_value(&tool_call).unwrap();
        assert_eq!(v["kind"], "tool_call");
        assert_eq!(v["tool"], "search_apps");
        assert_eq!(v["call_id"], "cid_1");

        let tool_result = ChatStreamChunk::ToolResult {
            call_id: "cid_1".into(),
            success: true,
            summary: "found 3 apps".into(),
        };
        let v = serde_json::to_value(&tool_result).unwrap();
        assert_eq!(v["kind"], "tool_result");
        assert_eq!(v["call_id"], "cid_1");
        assert_eq!(v["success"], true);
        assert_eq!(v["summary"], "found 3 apps");
    }
}
