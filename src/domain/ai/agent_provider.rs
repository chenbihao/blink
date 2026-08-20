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

// 0.42: Agent runtime 迁移到 rig-agent crate
use rig_agent::agent::{Agent, AgentBuilder, MultiTurnStreamItem};
use rig_agent::streaming::{StreamedAssistantContent, StreamedUserContent, StreamingPrompt};
use rig_agent::tool::DynamicTool;
use rig_core::client::CompletionClient;
use rig_core::completion::CompletionModel;
use rig_core::memory::ConversationMemory;
#[cfg(test)]
use rig_core::memory::InMemoryConversationMemory;

use crate::domain::config::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use std::sync::Arc;
use std::time::Duration;

use crate::domain::ai::factory::{
    build_anthropic_client, build_gemini_client, build_ollama_client, build_openai_client,
};
// 0.17.6: memory 改为 trait object，不再需要具体类型 import
use crate::domain::ai::provider::AIError;
use crate::domain::ai::rig_provider::expose_for_rig;
use crate::domain::ai::thinking::thinking_request_patch;
use crate::infra::platform::secret;
use crate::infra::utils::text::single_line;

/// Anthropic API 要求 max_tokens 必填，若 model 层未配置则使用此默认值。
/// 与 `rig_provider::build_rig_request` 中的 `ANTHROPIC_DEFAULT_MAX_TOKENS` 保持一致。
const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;

/// 对话窗口流式输出 chunk(emit 前端 `blink://chat-stream`)。
///
/// `run_stream` 消费 rig `MultiTurnStreamItem` 转成此枚举,前端按 `kind` 渲染。
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
    ToolCall {
        tool: String,
        call_id: String,
        arguments: String,
    },
    /// tool 执行结果(来自 rig `StreamUserItem`)。
    ///
    /// `call_id` 与 `ToolCall.call_id` 配对。`summary` 为结果文本(前 50000 字符);
    /// 图片内容以 `[image]` 占位。`success` 由 `content` 是否为空推断。
    ToolResult {
        call_id: String,
        success: bool,
        summary: String,
    },
    /// 一轮结束（`FinalResponse`），携带 token 用量 + 模型名。
    /// 0.12.3：model_name 供前端在气泡左下角显示。
    /// 0.21.17：使用统一 `message::Usage` 替代散落的独立字段，`serde(flatten)` 保持 JSON 向后兼容。
    Done {
        /// 统一 Usage（七字段 + reported）。`serde(flatten)` 展开到 Done 的 JSON 层级。
        #[serde(flatten)]
        usage: crate::domain::ai::message::Usage,
        /// 当前模型显示名（前端在气泡左下角显示）。
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
///
/// 四种协议统一使用 `LoggingHttpClient` 作为 HTTP 后端（0.21.16）——设置页「AI HTTP
/// 请求/响应体日志」开关打开后打印真实请求/响应体，与 `factory::build_*_client` 的
/// 注入保持一致。
/// 0.42: Agent 不再是泛型——所有 provider 的 Agent 都是同一个具体类型。
/// ChatAgent 枚举简化为只持有一个 Agent（ModelHandle 内部擦除了具体 model 类型）。
enum ChatAgent {
    Agent(Agent),
}

/// 对话窗口 Agent 封装--持有按当前 `ProviderEntry`+`ModelEntry` 构造的 rig `Agent`。
///
/// 构造时挂载 tool 池 + `InMemoryConversationMemory` + preamble,`stream_prompt` 驱动
/// agent loop 并把流式 chunk 经 channel emit 前端。
pub struct AgentProvider {
    agent: ChatAgent,
    /// 当前模型显示名（供 Done chunk 携带，前端在气泡左下角显示）
    model_name: String,
    /// 供应商身份——请求时按 `kind` + `base_url` + `model_id` 计算 thinking 补丁
    /// （见 `thinking_request_patch`）。返回 None 的供应商（仅 Gemini）开关不生效。
    kind: ProviderKind,
    base_url: Option<String>,
    model_id: String,
    /// 0.21.17: 工具定义快照——构造时从 `DynamicTool` + MCP tools 提取，
    /// 供 `compute_context_status` 估算 tools_tokens 使用。
    /// 不随 Agent 运行时变化，构造时一次性固化。
    tool_prompt_infos: Vec<crate::domain::ai::prompt::ToolPromptInfo>,
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
        tools: Vec<DynamicTool>,
        mcp_tools: Vec<(rmcp::model::Tool, rmcp::service::ServerSink)>,
        preamble: &str,
        memory: Arc<dyn ConversationMemory>,
    ) -> Result<Self, AIError> {
        // 0.13.1: context_limit 注入已移至 ChatService::ensure_provider（0.17.6：
        // memory 现在是 trait object，SqliteConversationMemory 的 update_context_limit
        // 在 ChatService 侧调用）。

        // 读密钥--与 RigFactory::build 一致(本地 provider 跳过)
        // 0.17.8: 密钥缺失用 SecretMissing 而非 NotConfigured，区分"档位悬空"与"密钥丢失"，
        // 让前端能给用户更精准的修复引导。
        let key_str: String = if entry.kind.requires_secret() {
            let key = secret::load_secret(&entry.id, "key").map_err(|e| {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    "AgentProvider: {} 密钥未配置 ({e})",
                    entry.display_name,
                );
                AIError::SecretMissing(entry.display_name.clone())
            })?;
            expose_for_rig(&key)
        } else {
            String::new()
        };

        // memory 已是 Arc<dyn ConversationMemory>，直接用于 rig AgentBuilder
        let memory_dyn = memory;

        // 0.21.17: 在 tools 被 move 进 build_agent 之前，提取工具定义快照供 token 预算使用。
        // DynamicTool::definition() 返回 ToolDefinition（name + description + parameters），
        // rmcp::model::Tool 暴露 name + description + input_schema。
        let tool_prompt_infos = build_tool_prompt_infos(&tools, &mcp_tools);

        // 0.13.x: 将 model 层的 temperature / max_tokens 传入 build_agent，
        // 让 rig AgentBuilder 在构造时固化默认值。Anthropic 的 max_tokens 是必填字段，
        // 若 model 层未配置则使用 ANTHROPIC_DEFAULT_MAX_TOKENS 兜底。
        let default_temperature = model.temperature.map(|f| f as f64);
        let default_max_tokens = model.max_tokens.map(|n| n as u64);

        // 供应商身份固化，供请求时计算 thinking 补丁（各供应商格式见 thinking_request_patch）
        let kind = entry.kind;
        let base_url = entry.base_url.clone();
        let model_id = model.id.clone();

        let agent = match entry.kind {
            ProviderKind::OpenAICompatible => {
                let client = build_openai_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
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
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    memory_dyn,
                    default_temperature,
                    Some(anthropic_max_tokens),
                ))
            }
            ProviderKind::GeminiGenerateContent => {
                let client = build_gemini_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    memory_dyn,
                    default_temperature,
                    default_max_tokens,
                ))
            }
            ProviderKind::OllamaHttp => {
                let client = build_ollama_client(entry.base_url.as_deref())?;
                let m = client.completion_model(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    memory_dyn,
                    default_temperature,
                    default_max_tokens,
                ))
            }
        };
        Ok(Self {
            agent,
            model_name: model_display_name(entry, model),
            kind,
            base_url,
            model_id,
            tool_prompt_infos,
        })
    }

    /// 流式 prompt--驱动 agent loop,chunk 经 `tx` emit 前端。
    ///
    /// `conversation_id` 贯穿:rig agent 按 id 自动 load/append memory(0.12.1 进程内,
    /// 0.12.2 持久化)。调用方(commands 层)创建 channel,spawn 此 task,chunk 走
    /// `blink://chat-stream` 事件。
    /// 0.21.17: 返回当前 Agent 挂载的工具定义快照，供 token 预算估算使用。
    ///
    /// 快照在构造时固化，不随运行时变化。纯 Capability tool 有 hint（来自 manifest），
    /// MCP tool 无 hint（rmcp Tool 不携带此字段）。
    pub fn tool_prompt_infos(&self) -> &[crate::domain::ai::prompt::ToolPromptInfo] {
        &self.tool_prompt_infos
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
        thinking_enabled: bool,
        reasoning_effort: Option<String>,
    ) {
        let model_name = if self.model_name.is_empty() {
            None
        } else {
            Some(self.model_name.clone())
        };
        // 按 provider + 开关状态 + 显式等级计算 thinking 补丁（见 thinking_request_patch）
        let thinking_patch = thinking_request_patch(
            self.kind,
            self.base_url.as_deref(),
            &self.model_id,
            thinking_enabled,
            reasoning_effort.as_deref(),
        );
        // 0.21.16: 思考开关状态打 trace——配合"不隐藏真实思考块"，开关失效时便于及时发现
        tracing::trace!(
            conversation_id = %conversation_id,
            thinking_enabled,
            reasoning_effort = ?reasoning_effort,
            thinking_patch = ?thinking_patch,
            "stream_prompt: 思考开关/强度状态"
        );
        match &self.agent {
            ChatAgent::Agent(a) => {
                Self::run_stream(
                    a,
                    conversation_id,
                    user_msg,
                    tx,
                    model_name,
                    thinking_patch.as_ref(),
                )
                .await
            }
        }
    }

    /// 流式 prompt + max_tokens 覆盖（0.21.19 摘要任务用）。
    ///
    /// 与 `stream_prompt` 逻辑一致，额外通过 `merge_additional_params` 注入 `max_tokens`，
    /// 限制输出长度（摘要任务要求 ≤ 600 token）。
    pub async fn stream_prompt_with_max_tokens(
        &self,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        thinking_enabled: bool,
        reasoning_effort: Option<String>,
        max_tokens: u64,
    ) {
        let model_name = if self.model_name.is_empty() {
            None
        } else {
            Some(self.model_name.clone())
        };
        // 思考强制关：reasoning_effort = None
        let thinking_patch = thinking_request_patch(
            self.kind,
            self.base_url.as_deref(),
            &self.model_id,
            thinking_enabled,
            reasoning_effort.as_deref(),
        );

        // 合并 thinking_patch + max_tokens 到 additional_params
        let mut params = serde_json::Map::new();
        if let Some(serde_json::Value::Object(map)) = thinking_patch {
            params.extend(map);
        }
        // max_tokens 字段名因 provider 而异，但 rig 内部会统一映射
        params.insert(
            "max_tokens".to_string(),
            serde_json::Value::Number(serde_json::Number::from(max_tokens)),
        );

        match &self.agent {
            ChatAgent::Agent(a) => {
                Self::run_stream_with_params(
                    a,
                    conversation_id,
                    user_msg,
                    tx,
                    model_name,
                    Some(&params),
                )
                .await
            }
        }
    }

    /// `run_stream` 的变体——接受预构造的 additional_params Map（含 max_tokens 等）。
    ///
    /// 将 params 包装为 `Value::Object` 后复用 `run_stream` 的消费循环。
    async fn run_stream_with_params(
        agent: &Agent,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
        params: Option<&serde_json::Map<String, serde_json::Value>>,
    ) {
        let patch = params.map(|m| serde_json::Value::Object(m.clone()));
        Self::run_stream(
            agent,
            conversation_id,
            user_msg,
            tx,
            model_name,
            patch.as_ref(),
        )
        .await;
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
    async fn run_stream(
        agent: &Agent,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
        thinking_patch: Option<&serde_json::Value>,
    ) {
        let timeout_ms =
            crate::domain::config::ai_config::get_ai_config().effective_hard_timeout_ms();
        let idle_timeout = Duration::from_millis(timeout_ms as u64);
        // 阶段 2：注入按 provider + 开关状态算好的 thinking 补丁（见 thinking_request_patch）
        let stream_builder = {
            let builder = agent.stream_prompt(user_msg).conversation(conversation_id);
            if let Some(serde_json::Value::Object(map)) = thinking_patch {
                // merge_additional_params 需要所有权 Map，模板很小，clone 无成本
                builder.merge_additional_params(map.clone())
            } else {
                builder
            }
        };
        let mut stream = match tokio::time::timeout(idle_timeout, stream_builder).await {
            Ok(stream) => stream,
            Err(_) => {
                tracing::warn!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    conversation = %conversation_id,
                    timeout_ms,
                    "run_stream: 等待模型首个响应超时"
                );
                let _ = tx.send(ChatStreamChunk::Error {
                    message: format!(
                        "AI 请求超时（{timeout_ms} 毫秒），请重试或在设置中调整硬超时"
                    ),
                });
                return;
            }
        };
        let mut done_sent = false;
        // 跟踪是否收到过实质内容（Text/Thinking/ToolCall/ToolResult）。
        // rig 在 SSE 解析失败时可能 yield 一个空 FinalResponse（0 token + 无内容），
        // 需区分"正常空回复"与"请求根本未处理"——后者转为 Error 上报前端。
        let mut has_content = false;
        loop {
            let item = match tokio::time::timeout(idle_timeout, stream.next()).await {
                Ok(Some(item)) => item,
                Ok(None) => break,
                Err(_) => {
                    tracing::warn!(
                        target: crate::infra::utils::perf::ai_slo::TARGET,
                        conversation = %conversation_id,
                        timeout_ms,
                        "run_stream: 等待模型流式响应超时"
                    );
                    let _ = tx.send(ChatStreamChunk::Error {
                        message: format!(
                            "AI 请求超时（{timeout_ms} 毫秒），请重试或在设置中调整硬超时"
                        ),
                    });
                    return;
                }
            };
            let chunk = match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                    StreamedAssistantContent::Text(t) => {
                        has_content = true;
                        tracing::trace!(
                            conversation = %conversation_id,
                            text = single_line(&t.text),
                            "run_stream: text delta"
                        );
                        ChatStreamChunk::Text { text: t.text }
                    }
                    StreamedAssistantContent::Reasoning { reasoning, .. } => {
                        has_content = true;
                        let text = reasoning.display_text();
                        tracing::trace!(
                            conversation = %conversation_id,
                            thinking = single_line(&text),
                            "run_stream: thinking delta"
                        );
                        // 0.21.16: 不按开关隐藏思考块——开关失效时模型仍思考能及时暴露
                        ChatStreamChunk::Thinking { text }
                    }
                    StreamedAssistantContent::ReasoningDelta { reasoning, .. } => {
                        has_content = true;
                        tracing::trace!(
                            conversation = %conversation_id,
                            thinking = single_line(&reasoning),
                            "run_stream: thinking delta"
                        );
                        // 0.21.16: 不按开关隐藏思考块——开关失效时模型仍思考能及时暴露
                        ChatStreamChunk::Thinking { text: reasoning }
                    }
                    StreamedAssistantContent::ToolCall {
                        tool_call,
                        internal_call_id,
                    } => {
                        has_content = true;
                        let tool = tool_call.function.name.clone();
                        let arguments = tool_call.function.arguments.to_string();
                        tracing::debug!(
                            conversation = %conversation_id,
                            tool = %tool,
                            call_id = %internal_call_id,
                            args_chars = arguments.chars().count(),
                            "run_stream: tool call"
                        );
                        ChatStreamChunk::ToolCall {
                            tool,
                            call_id: internal_call_id,
                            arguments,
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
                    tracing::debug!(
                        conversation = %conversation_id,
                        call_id = %internal_call_id,
                        success,
                        summary_chars = summary.chars().count(),
                        "run_stream: tool result"
                    );
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
                            message: "AI 返回了无效响应，可能是服务过载或配额耗尽，请稍后重试"
                                .to_string(),
                        }
                    } else {
                        // 0.21.17: 统一使用 message::Usage::from_rig_usage 映射
                        ChatStreamChunk::Done {
                            usage: crate::domain::ai::message::Usage::from_rig_usage(&usage),
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

/// 0.21.17: 从 `DynamicTool` + MCP tools 提取 `ToolPromptInfo` 快照。
///
/// 在 `AgentProvider::new` 中 `tools` 被 move 进 `build_agent` 之前调用。
/// - `DynamicTool::definition()` 返回 rig `ToolDefinition`（name + description + parameters）
/// - `rmcp::model::Tool` 暴露 `name` + `description` + `input_schema`（JsonObject）
///
/// MCP tool 的 `input_schema` 是 `Map<String, Value>`，转为 `serde_json::Value::Object`
/// 以统一到 `ToolPromptInfo.parameters` 的 `Value` 类型。MCP tool 无 `hint` 字段。
fn build_tool_prompt_infos(
    tools: &[DynamicTool],
    mcp_tools: &[(rmcp::model::Tool, rmcp::service::ServerSink)],
) -> Vec<crate::domain::ai::prompt::ToolPromptInfo> {
    let mut infos = Vec::with_capacity(tools.len() + mcp_tools.len());

    // 1. DynamicTool（Capability tool）
    for dt in tools {
        let def = dt.definition();
        infos.push(crate::domain::ai::prompt::ToolPromptInfo {
            name: def.name,
            description: def.description,
            parameters: def.parameters,
            hint: None, // DynamicTool 不携带 hint；hint 在 system prompt 中已拼接
        });
    }

    // 2. MCP tool
    for (tool, _) in mcp_tools {
        let params = serde_json::Value::Object(tool.input_schema.as_ref().clone());
        infos.push(crate::domain::ai::prompt::ToolPromptInfo {
            name: tool.name.to_string(),
            description: tool.description.as_deref().unwrap_or("").to_string(),
            parameters: params,
            hint: None,
        });
    }

    infos
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
pub(crate) fn summarize_tool_result(
    tool_result: &rig_core::completion::message::ToolResult,
) -> String {
    use rig_core::completion::message::ToolResultContent;
    let mut parts: Vec<String> = Vec::new();
    for content in tool_result.content.iter() {
        match content {
            ToolResultContent::Text(t) => parts.push(t.text.clone()),
            ToolResultContent::Image(_) => parts.push("[image]".to_string()),
            ToolResultContent::Json { value } => parts.push(value.to_string()),
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

/// 构造 `Agent`（0.42: Agent 不再是泛型，ModelHandle 内部擦除具体 model 类型）。
///
/// `default_temperature` / `default_max_tokens` 从 `ModelEntry` 来，
/// 构造时固化到 `AgentBuilder`，rig Agent 内部生成 `CompletionRequest` 时使用。
/// `None` 表示不覆盖（rig 让 provider 自己决定），但 Anthropic 的 `max_tokens` 是
/// 必填字段，调用方需确保传入 `Some`（见 `AgentProvider::new` 中的兜底逻辑）。
fn build_agent<M>(
    model: M,
    preamble: &str,
    tools: Vec<DynamicTool>,
    mcp_tools: Vec<(rmcp::model::Tool, rmcp::service::ServerSink)>,
    memory: Arc<dyn ConversationMemory>,
    default_temperature: Option<f64>,
    default_max_tokens: Option<u64>,
) -> Agent
where
    M: CompletionModel + 'static,
{
    // 0.42: AgentBuilder 使用 typestate 模式（NoToolConfig → WithBuilderTools → Agent）。
    // typestate 不能在 if 中条件性改变类型，所以分两条路径构建：
    // - 有 tools → 先 dynamic_tools 进入 WithBuilderTools，再逐个 rmcp_tool
    // - 无 tools → 直接在 NoToolConfig 上 build
    let has_tools = !tools.is_empty() || !mcp_tools.is_empty();

    if !has_tools {
        // 无 tool：直接在 NoToolConfig 上配置 temperature/max_tokens 然后 build
        let mut builder = AgentBuilder::new(model)
            .preamble(preamble)
            .memory(memory)
            .default_max_turns(MAX_TURNS);
        if let Some(temp) = default_temperature {
            builder = builder.temperature(temp);
        }
        if let Some(max_tok) = default_max_tokens {
            builder = builder.max_tokens(max_tok);
        }
        return builder.build();
    }

    // 有 tool：先 dynamic_tools 进入 WithBuilderTools 状态
    let mut builder = AgentBuilder::new(model)
        .preamble(preamble)
        .memory(memory)
        .default_max_turns(MAX_TURNS)
        .dynamic_tools(tools);

    // 0.42: MCP tools 通过 rmcp_tools 注册（McpTool 是 pub(crate)，只能走此路径）
    // rmcp_tools 接受 (Vec<Tool>, ServerSink)，但每个 server 的 tools 需要分开注册
    // 因为不同 tool 可能来自不同 server（不同 ServerSink）
    for (tool, client) in mcp_tools {
        builder = builder.rmcp_tools(vec![tool], client);
    }

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
    use rig_core::test_utils::{MockCompletionModel, MockStreamEvent};

    /// 验证 `run_stream` 把 `MultiTurnStreamItem` 正确转 `ChatStreamChunk`。
    /// 用 `MockCompletionModel`(不依赖网络/密钥),绕过 `AgentProvider::new`。
    #[tokio::test]
    async fn run_stream_emits_text_then_done() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

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
        use rig_agent::test_utils::MockAddTool;
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::tool_call("call_1", "add", serde_json::json!({"a":1,"b":2})),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        // 挂 MockAddTool(name="add")--agent 接受 model 的 tool_call,否则 UnknownToolCall
        let agent = AgentBuilder::new(model)
            .tool(MockAddTool)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

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
        use rig_agent::test_utils::MockAddTool;
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
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }

        // 应同时存在 ToolCall 和 ToolResult,且 call_id 可配对
        let tool_call_id = chunks.iter().find_map(|c| match c {
            ChatStreamChunk::ToolCall { tool, call_id, .. } if tool == "add" => {
                Some(call_id.clone())
            }
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
        assert!(summary.contains('3'), "摘要应包含 tool 结果 3: {summary}");
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
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        let done = chunks.iter().find_map(|c| match c {
            ChatStreamChunk::Done {
                usage,
                model_name: _,
            } => Some((usage.input_tokens, usage.output_tokens)),
            _ => None,
        });
        let (input_tokens, output_tokens) = done.expect("应 emit Done chunk: {chunks:?}");
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
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        let (input_tokens, _) = chunks
            .iter()
            .find_map(|c| match c {
                ChatStreamChunk::Done { usage, .. } => {
                    Some((usage.input_tokens, usage.output_tokens))
                }
                _ => None,
            })
            .expect("应 emit Done");
        assert_eq!(
            input_tokens,
            u32::MAX,
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
        let model =
            MockCompletionModel::from_stream_turns(vec![vec![MockStreamEvent::final_response(
                zero_usage,
            )]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

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
            DocumentSourceKind, Image, Text, ToolCallId, ToolResult, ToolResultContent,
        };

        // 短文本不截断
        let short = ToolResult {
            call: ToolCallId::new_or_mint("1"),
            provider: None,
            name: "test".into(),
            content: vec![ToolResultContent::Text(Text::new("ok"))],
        };
        assert_eq!(summarize_tool_result(&short), "ok");

        // 长文本截断到 50000 字符 + 省略号
        let long_text = "x".repeat(60000);
        let long = ToolResult {
            call: ToolCallId::new_or_mint("2"),
            provider: None,
            name: "test".into(),
            content: vec![ToolResultContent::Text(Text::new(long_text))],
        };
        let summary = summarize_tool_result(&long);
        assert_eq!(summary.chars().count(), 50001, "50000 字符 + 1 省略号");
        assert!(summary.ends_with('…'));

        // 图片转占位
        let img = ToolResult {
            call: ToolCallId::new_or_mint("3"),
            provider: None,
            name: "test".into(),
            content: vec![ToolResultContent::Image(Image {
                data: DocumentSourceKind::Url("http://example.com/x.png".into()),
                media_type: None,
                detail: None,
                additional_params: None,
            })],
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
            usage: crate::domain::ai::message::Usage {
                input_tokens: 10,
                output_tokens: 20,
                total_tokens: 30,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 5,
                reported: true,
            },
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

    /// 验证模型产出 reasoning 时 run_stream 总是 emit Thinking chunk（0.21.16 起不按
    /// 开关隐藏——开关失效时模型仍思考，前端可见以便及时发现）。
    #[tokio::test]
    async fn run_stream_always_emits_thinking_chunks() {
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::reasoning("secret thinking"),
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let agent = AgentBuilder::new(model)
            .memory(InMemoryConversationMemory::new())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hi", tx, None, None).await;

        let mut chunks = Vec::new();
        while let Some(c) = rx.recv().await {
            chunks.push(c);
        }
        // 应有 Thinking chunk（无论开关状态）
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Thinking { text } if text.contains("secret thinking"))),
            "模型产出 reasoning 时应 emit Thinking chunk: {chunks:?}"
        );
        // 应有 Text chunk
        assert!(
            chunks
                .iter()
                .any(|c| matches!(c, ChatStreamChunk::Text { text } if text == "hello")),
            "应仍 emit Text(hello): {chunks:?}"
        );
    }

    // ── 回归：发出即保存（预写 user）不得让请求上下文重复（0.21.16 bug）──────────

    /// 模拟"发出即保存"：`persist_user_message` 预写当前 user 后走 rig
    /// `stream_prompt(prompt)`。rig 会把 memory 加载的历史 + 当前 prompt 组装成
    /// 请求，若 load 也带上预写 user，同一 user 会在 chat_history 出现两次
    /// （模型看到"用户询问了我两次"）。修复后应恰好 1 次。
    #[tokio::test]
    async fn prewrite_user_appears_once_in_request() {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        crate::infra::data::conversations::init_db(&pool)
            .await
            .expect("init tables");
        let mem = std::sync::Arc::new(crate::domain::ai::memory::SqliteConversationMemory::new(
            pool,
        ));

        // 1. 发出即保存：预写当前 user（与 ChatService::prompt 一致）
        mem.persist_user_message("c1", "hello").await.unwrap();

        // 2. 走 rig agent 流式路径（与 stream_prompt 一致），mock 记录收到的请求
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("reply"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let recorded = model.clone();
        let agent = AgentBuilder::new(model)
            .memory(mem.clone())
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, "c1", "hello", tx, None, None).await;
        while rx.recv().await.is_some() {}

        // 3. 检查模型实际收到的 chat_history：user 应恰好 1 次
        let requests = recorded.requests();
        assert_eq!(requests.len(), 1, "应恰好 1 次请求");
        let history = &requests[0].chat_history;
        let user_texts: Vec<String> = history
            .iter()
            .filter_map(|m| match m {
                rig_core::completion::message::Message::User { content } => Some(
                    content
                        .iter()
                        .filter_map(|c| match c {
                            rig_core::completion::message::UserContent::Text(t) => {
                                Some(t.text.clone())
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>()
                        .join(""),
                ),
                _ => None,
            })
            .collect();
        assert_eq!(
            user_texts.iter().filter(|t| *t == "hello").count(),
            1,
            "预写 user 在请求上下文里应恰好 1 次（rig 只追加一次 prompt）: {history:?}"
        );
    }

    // ── 0.21.17: build_tool_prompt_infos 生产连线测试 ──────────────────────────

    /// 验证 `build_tool_prompt_infos` 从 `DynamicTool` 正确提取 name/description/parameters。
    #[test]
    fn build_tool_prompt_infos_extracts_dynamic_tools() {
        use rig_agent::tool::ToolOutput;
        let tool1 = DynamicTool::new(
            "search_apps",
            "搜索应用",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"}}}),
            |_ctx, _args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );
        let tool2 = DynamicTool::new(
            "open_url",
            "打开网址",
            serde_json::json!({"type":"object","properties":{"url":{"type":"string"}}}),
            |_ctx, _args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );

        let infos = build_tool_prompt_infos(&[tool1, tool2], &[]);
        assert_eq!(infos.len(), 2);
        assert_eq!(infos[0].name, "search_apps");
        assert_eq!(infos[0].description, "搜索应用");
        assert_eq!(infos[0].parameters["type"], "object");
        assert!(infos[0].hint.is_none());

        assert_eq!(infos[1].name, "open_url");
        assert_eq!(infos[1].description, "打开网址");
    }

    /// 验证 `build_tool_prompt_infos` 空输入返回空列表。
    #[test]
    fn build_tool_prompt_infos_empty_returns_empty() {
        let infos = build_tool_prompt_infos(&[], &[]);
        assert!(infos.is_empty());
    }

    /// 验证 `AgentProvider::tool_prompt_infos()` 返回构造时固化的快照。
    #[tokio::test]
    async fn agent_provider_tool_prompt_infos_returns_snapshot() {
        use crate::domain::config::ai_config::{
            ModelCapability, ModelEntry, ProviderEntry, ProviderKind,
        };
        use rig_agent::tool::ToolOutput;

        let tool = DynamicTool::new(
            "test_tool",
            "测试工具",
            serde_json::json!({"type":"object"}),
            |_ctx, _args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );

        let entry = ProviderEntry {
            id: "test".into(),
            display_name: "Test".into(),
            kind: ProviderKind::OllamaHttp,
            base_url: Some("http://localhost:11434".into()),
            secret_ref: String::new(),
            models: Vec::new(),
            enabled: true,
            created_at: 0,
        };
        let model = ModelEntry {
            id: "test-model".into(),
            display_name: "Test Model".into(),
            enabled: true,
            context_window: Some(8192),
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            capabilities: vec![ModelCapability::Chat],
            reasoning_effort: None,
            custom_parameters: Vec::new(),
        };

        let provider = AgentProvider::new(
            &entry,
            &model,
            vec![tool],
            Vec::new(),
            "",
            Arc::new(InMemoryConversationMemory::new()),
        )
        .await
        .expect("AgentProvider 构造成功");

        let infos = provider.tool_prompt_infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "test_tool");
        assert_eq!(infos[0].description, "测试工具");
    }
}
