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

// 0.45: Agent runtime 仍在 rig-agent；流式事件模型为 rig-core `StreamEvent`/`Item`，
// model 句柄为 `DynModel<Completion>`。0.27.13：`ConversationMemory` trait 已移除，
// 记忆改由本仓 `ChatMemory` 显式驱动（load → runner.history，FinalResponse → append_turn）。
use rig_agent::agent::{Agent, AgentBuilder, MultiTurnStreamItem, StreamingResult};
use rig_agent::tool::DynamicTool;
use rig_core::driver::DynModel;
use rig_core::operation::Completion;
use rig_core::streaming::{Item as RigStreamItem, StreamEvent as RigStreamEvent};

use crate::ai::memory::ChatMemory;


/// 构造本轮 user 消息（0.27.10）：无图 = `Message::user(text)`（与既有行为
/// 完全一致）；有图 = 非空正文 Text part 在前 + 逐图 base64 Image part。
/// 0.27.12：image_raw → image_base64——rig 的 OpenAICompatible/Gemini wire
/// 转换对 Raw 变体直接报错（"Raw files not supported, encode as base64
/// first"），Base64 是三家 provider 的通行路径（OpenAI image_url data URL /
/// Gemini inline_data / Anthropic base64 source）；归一化产物本就要编码
/// 发送，编码前置到此。图片不落持久化历史——memory append 侧剥离
/// （phase §3.4 单次注入语义）。
fn build_prompt_message(user_msg: &str, image_parts: &[VisionImagePart]) -> rig_core::completion::Message {
    use rig_core::completion::message::{ImageMediaType, UserContent};

    if image_parts.is_empty() {
        return rig_core::completion::Message::user(user_msg);
    }
    let mut content: Vec<UserContent> = Vec::with_capacity(image_parts.len() + 1);
    if !user_msg.is_empty() {
        content.push(UserContent::text(user_msg));
    }
    for part in image_parts {
        let media_type = match part.mime.as_str() {
            "image/jpeg" => Some(ImageMediaType::JPEG),
            _ => Some(ImageMediaType::PNG),
        };
        content.push(UserContent::image_base64(
            base64_encode(&part.bytes),
            media_type,
            None,
        ));
    }
    rig_core::completion::Message::User { content }
}

/// 标准 base64 编码（无换行填充差异；provider wire 侧要求 base64 变体）。
fn base64_encode(bytes: &[u8]) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

#[cfg(test)]
use crate::ai::memory::EphemeralConversationMemory;

use crate::config::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use std::sync::Arc;
use std::time::Duration;

use crate::ai::compat_dialect::{resolve_compat_dialect, ResolvedCompatDialect};
use crate::ai::factory::{
    build_anthropic_client, build_gemini_client, build_ollama_client, build_openai_client,
};
// 0.17.6: memory 改为 trait object，不再需要具体类型 import
use crate::ai::provider::AIError;
use crate::ai::rig_provider::expose_for_rig;
use crate::ai::thinking::thinking_request_patch;
use crate::config::ai_config::ThinkingStyle;
use blink_infra::platform::secret;
use blink_infra::utils::text::single_line;

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
        usage: crate::ai::message::Usage,
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

/// 本轮 user 消息的图片 part（0.27.10 Vision 输入）。
///
/// 内容为归一化后的最终编码字节（`ai::vision::normalize_image` 产物）；
/// 进入 rig `UserContent::image_raw` 多模态 part，不进持久化历史
///（memory append 侧剥离，phase §3.4）。
#[derive(Debug, Clone)]
pub struct VisionImagePart {
    /// "image/png" | "image/jpeg"（归一化后只有两种 wire 路径）。
    pub mime: String,
    pub bytes: Vec<u8>,
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
/// 构造时挂载 tool 池 + preamble,`stream_prompt` 驱动 agent loop 并把流式 chunk
/// 经 channel emit 前端。0.27.13：memory 不再挂进 rig AgentBuilder（0.45 已移除
/// 该接缝），由本结构持有，每轮 load → `runner.history(...)` 注入、FinalResponse
/// 到达时 `append_turn` 回写。
pub struct AgentProvider {
    agent: ChatAgent,
    /// 0.27.13：Blink 自有记忆服务（Sqlite / Ephemeral），显式驱动 load/回写。
    memory: Arc<dyn ChatMemory>,
    /// 当前模型显示名（供 Done chunk 携带，前端在气泡左下角显示）
    model_name: String,
    /// 供应商身份——请求时按 `kind` + `compat` + `model_id` 计算 thinking 补丁
    /// （见 `thinking_request_patch`）。返回 None 的供应商（仅 Gemini）开关不生效。
    kind: ProviderKind,
    /// 0.27.14：构造时解析的兼容方言（单一真源 `resolve_compat_dialect`）——
    /// thinking 判定与 wire 方言接线共享同一结论（原 base_url 子串启发式由其取代）。
    compat: ResolvedCompatDialect,
    model_id: String,
    /// 0.21.22: per-model 思考控件形态逃生口——构造时从 `ModelEntry.thinking_style` 固化
    thinking_style: Option<ThinkingStyle>,
    custom_params: Option<serde_json::Value>,
    default_max_tokens: Option<u64>,
    /// 0.21.17: 工具定义快照——构造时从 `DynamicTool` + MCP tools 提取，
    /// 供 `compute_context_status` 估算 tools_tokens 使用。
    /// 不随 Agent 运行时变化，构造时一次性固化。
    tool_prompt_infos: Vec<crate::ai::prompt::ToolPromptInfo>,
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
        mcp_tools: Vec<rig_rmcp::McpTool>,
        preamble: &str,
        memory: Arc<dyn ChatMemory>,
    ) -> Result<Self, AIError> {
        // 0.13.1: context_limit 注入已移至 ChatService::ensure_provider（0.17.6：
        // memory 现在是 trait object，SqliteConversationMemory 的 update_context_limit
        // 在 ChatService 侧调用）。

        // 读密钥--与 RigFactory::build 一致(本地 provider 跳过)
        // 0.17.8: 密钥缺失用 SecretMissing 而非 NotConfigured，区分"档位悬空"与"密钥丢失"，
        // 让前端能给用户更精准的修复引导。
        let key_str: String = if entry.kind.requires_secret() {
            let key = secret::load_secret(&secret::SecretRef::ai(&entry.id)).map_err(|e| {
                tracing::debug!(
                    target: blink_infra::utils::perf::ai_slo::TARGET,
                    "AgentProvider: {} 密钥未配置 ({e})",
                    entry.display_name,
                );
                AIError::SecretMissing(entry.display_name.clone())
            })?;
            expose_for_rig(&key)
        } else {
            String::new()
        };

        Self::with_key(entry, model, tools, mcp_tools, preamble, memory, &key_str)
    }

    /// 密钥由可信入口注入；测试可用本地 HTTP 桩而不触碰系统凭据。
    #[allow(clippy::too_many_arguments)]
    fn with_key(
        entry: &ProviderEntry,
        model: &ModelEntry,
        tools: Vec<DynamicTool>,
        mcp_tools: Vec<rig_rmcp::McpTool>,
        preamble: &str,
        memory: Arc<dyn ChatMemory>,
        key_str: &str,
    ) -> Result<Self, AIError> {
        // 0.21.17: 在 tools 被 move 进 build_agent 之前，提取工具定义快照供 token 预算使用。
        // DynamicTool::definition() 返回 ToolDefinition（name + description + parameters），
        // rig_rmcp::McpTool 经 definition() 暴露原始 rmcp Tool（name + description + input_schema）。
        let tool_prompt_infos = build_tool_prompt_infos(&tools, &mcp_tools);

        // 0.13.x: 将 model 层的 temperature / max_tokens 传入 build_agent，
        // 让 rig AgentBuilder 在构造时固化默认值。Anthropic 的 max_tokens 是必填字段，
        // 若 model 层未配置则使用 ANTHROPIC_DEFAULT_MAX_TOKENS 兜底。
        let default_temperature = model.temperature.map(|f| f as f64);
        let default_max_tokens = model.max_tokens.map(|n| n as u64);

        // 供应商身份固化，供请求时计算 thinking 补丁（各供应商格式见 thinking_request_patch）
        let kind = entry.kind;
        let model_id = model.id.clone();
        // 0.27.14：方言在构造期解析一次——wire 构造与请求期 thinking 判定共享。
        let compat =
            resolve_compat_dialect(entry.kind, entry.compat_dialect, entry.base_url.as_deref());
        // 0.21.22: per-model thinking_style 逃生口
        let thinking_style = model.thinking_style;

        let agent = match entry.kind {
            ProviderKind::OpenAICompatible => {
                let client = build_openai_client(&key_str, entry.base_url.as_deref(), compat)?;
                let m = client.completion(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    default_temperature,
                    default_max_tokens,
                ))
            }
            ProviderKind::AnthropicMessages => {
                let client = build_anthropic_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion(&model.id);
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
                    default_temperature,
                    Some(anthropic_max_tokens),
                ))
            }
            ProviderKind::GeminiGenerateContent => {
                let client = build_gemini_client(&key_str, entry.base_url.as_deref())?;
                let m = client.completion(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    default_temperature,
                    default_max_tokens,
                ))
            }
            ProviderKind::OllamaHttp => {
                let client = build_ollama_client(entry.base_url.as_deref())?;
                // 0.45：completion() 走 Ollama 的 OpenAI 兼容 /v1/chat/completions，
                // 不再发送模型 options（num_ctx/num_predict 等）；native_completion()
                // 才是原生 /api/chat——保留既有 additional_params → options、
                // max_tokens → num_predict、think 布尔的语义。
                let m = client.native_completion(&model.id);
                ChatAgent::Agent(build_agent(
                    m,
                    preamble,
                    tools,
                    mcp_tools,
                    default_temperature,
                    default_max_tokens,
                ))
            }
        };
        Ok(Self {
            agent,
            memory,
            model_name: model_display_name(entry, model),
            kind,
            compat,
            model_id,
            thinking_style,
            custom_params: super::rig_provider::build_custom_params_json(&model.custom_parameters),
            default_max_tokens: default_max_tokens.or_else(|| {
                (kind == ProviderKind::AnthropicMessages).then_some(ANTHROPIC_DEFAULT_MAX_TOKENS)
            }),
            tool_prompt_infos,
        })
    }

    /// 当前 Agent 挂载的工具定义快照（0.21.17）。
    ///
    /// 构造时从 `DynamicTool` + MCP tools 提取并固化，不随运行时变化——
    /// 供 `compute_context_status` 估算 tools_tokens 使用。
    /// 纯 Capability tool 有 hint（来自 manifest），MCP tool 无 hint（rmcp Tool 不携带此字段）。
    pub fn tool_prompt_infos(&self) -> &[crate::ai::prompt::ToolPromptInfo] {
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
        image_parts: &[VisionImagePart],
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        thinking_enabled: bool,
        reasoning_effort: Option<String>,
    ) {
        let model_name = if self.model_name.is_empty() {
            None
        } else {
            Some(self.model_name.clone())
        };
        // 按 provider + 开关状态 + 显式等级 + per-model style 计算 thinking 补丁
        let thinking_patch = thinking_request_patch(
            self.kind,
            self.compat,
            &self.model_id,
            thinking_enabled,
            reasoning_effort.as_deref(),
            self.thinking_style,
        );
        let mut thinking_patch =
            super::thinking::merge_chat_parameters(self.custom_params.as_ref(), thinking_patch);
        if let Err(error) = super::thinking::validate_thinking_budget(
            self.kind,
            &mut thinking_patch,
            self.default_max_tokens,
        ) {
            let _ = tx.send(ChatStreamChunk::Error {
                message: error.to_string(),
            });
            return;
        }
        // 0.21.16: 思考开关状态打 trace——配合"不隐藏真实思考块"，开关失效时便于及时发现
        tracing::trace!(
            conversation_id = %conversation_id,
            thinking_enabled,
            reasoning_effort = ?reasoning_effort,
            has_custom_params = self.custom_params.is_some(),
            "stream_prompt: 思考开关/强度状态"
        );
        match &self.agent {
            ChatAgent::Agent(a) => {
                Self::run_stream(
                    a,
                    self.memory.clone(),
                    conversation_id,
                    user_msg,
                    image_parts,
                    tx,
                    model_name,
                    thinking_patch.as_ref(),
                )
                .await
            }
        }
    }

    /// 流式 prompt + max_tokens 覆盖（0.21.19 摘要任务用 / 0.21.20 rig 一等参数修复）。
    ///
    /// 与 `stream_prompt` 逻辑一致，额外用 rig builder 的一等 `.max_tokens()` 方法
    /// 限制输出长度（摘要任务要求 ≤ 600 token）。
    ///
    /// **0.21.20 修复**：旧实现把 `"max_tokens": 600` 塞进 `additional_params` 顶层，
    /// 但 rig 0.42 对不同供应商的 `AdditionalParameters` 解析路径不同：
    /// - Gemini：`AdditionalParameters` 只解析 `generationConfig`，其余键 flatten 到请求体
    ///   顶层被 API 忽略——裸 `max_tokens` 永远到不了 `maxOutputTokens`；
    /// - Ollama：`additional_params` 会被合入 `options`，但键名应为 `num_predict`，
    ///   `max_tokens` 键无效；
    /// - OpenAI 兼容 / Anthropic 恰好顶层 `max_tokens` 正确。
    ///
    /// 改用 rig builder 的 `.max_tokens(u64)` 一等参数后，rig 对各协议原生映射
    /// （Gemini → `generationConfig.maxOutputTokens`、Ollama → `options.num_predict`、
    /// OpenAI/Anthropic → `max_tokens`），不再静默失效。
    ///
    /// **thinking_patch 的 `merge_additional_params` 逻辑保留不动**——thinking 字段
    /// 各供应商格式不同（DeepSeek `thinking.type` / Anthropic `thinking.budget_tokens`
    /// 等），走 `merge_additional_params` 是各供应商文档约定的标准注入路径，
    /// 不受 `max_tokens` 映射问题影响。
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
            self.compat,
            &self.model_id,
            thinking_enabled,
            reasoning_effort.as_deref(),
            self.thinking_style,
        );

        let mut thinking_patch =
            super::thinking::merge_chat_parameters(self.custom_params.as_ref(), thinking_patch);
        // 显式输出上限走 rig 一等字段，不能被模型的同名额外参数覆盖。
        if let Some(params) = thinking_patch.as_mut().and_then(|p| p.as_object_mut()) {
            params.remove("max_tokens");
        }
        if let Err(error) = super::thinking::validate_thinking_budget(
            self.kind,
            &mut thinking_patch,
            Some(max_tokens),
        ) {
            let _ = tx.send(ChatStreamChunk::Error {
                message: error.to_string(),
            });
            return;
        }
        match &self.agent {
            ChatAgent::Agent(a) => {
                Self::run_stream_with_max_tokens(
                    a,
                    self.memory.clone(),
                    conversation_id,
                    user_msg,
                    tx,
                    model_name,
                    thinking_patch.as_ref(),
                    max_tokens,
                )
                .await
            }
        }
    }

    /// `run_stream` 的变体——接受 thinking_patch + rig 一等 `max_tokens`。
    ///
    /// thinking_patch 走 `merge_additional_params`（各供应商 thinking 字段的标准注入路径），
    /// `max_tokens` 走 runner 的 `.max_tokens(u64)` 一等参数（rig 原生映射各协议字段）。
    async fn run_stream_with_max_tokens(
        agent: &Agent,
        memory: Arc<dyn ChatMemory>,
        conversation_id: &str,
        user_msg: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
        thinking_patch: Option<&serde_json::Value>,
        max_tokens: u64,
    ) {
        let timeout_ms =
            crate::config::ai_config::get_ai_config().effective_hard_timeout_ms();
        let idle_timeout = Duration::from_millis(timeout_ms as u64);
        // max_tokens 走 rig 一等参数——rig 对各协议原生映射：
        // Gemini → generationConfig.maxOutputTokens、Ollama → options.num_predict、
        // OpenAI/Anthropic → max_tokens
        let history = match memory.load(conversation_id).await {
            Ok(history) => history,
            Err(e) => {
                tracing::error!(
                    conversation_id,
                    error = %e,
                    "run_stream_with_max_tokens: 加载对话历史失败"
                );
                let _ = tx.send(ChatStreamChunk::Error {
                    message: format!("加载对话历史失败: {e}"),
                });
                return;
            }
        };
        let mut runner = agent
            .prompt(rig_core::completion::Message::user(user_msg))
            .history(history)
            .max_tokens(max_tokens);
        if let Some(serde_json::Value::Object(map)) = thinking_patch {
            runner = runner.merge_additional_params(map.clone());
        }
        let stream = runner.stream();
        // 复用 run_stream 的消费循环——传入已构造的 stream
        Self::consume_stream(stream, memory, conversation_id, tx, model_name, idle_timeout).await;
    }

    /// 泛型 stream 消费--4 个 `ChatAgent` arm 共用,每个 arm 具体化 M。
    ///
    /// 消费 `MultiTurnStreamItem` 的完整映射见 `consume_stream` 文档。
    ///
    /// **中断**:调用方 drop `tx`(或 task 被 abort)即中断,stream 被 drop 后 rig 内部
    /// reqwest task 自动 abort(与主窗口 `RigProvider::stream` 一致)。
    async fn run_stream(
        agent: &Agent,
        memory: Arc<dyn ChatMemory>,
        conversation_id: &str,
        user_msg: &str,
        image_parts: &[VisionImagePart],
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
        thinking_patch: Option<&serde_json::Value>,
    ) {
        let timeout_ms =
            crate::config::ai_config::get_ai_config().effective_hard_timeout_ms();
        let idle_timeout = Duration::from_millis(timeout_ms as u64);
        // 0.27.13：0.45 移除了 rig 侧记忆挂载——这里显式 load 后经
        // `runner.history(...)` 注入（不读写 rig store）；thinking 补丁注入
        // merge_additional_params（各供应商 thinking 字段的标准注入路径）。
        // 0.27.10：图片走 typed 多模态 part（image_base64）；无图保持纯文本路径。
        let history = match memory.load(conversation_id).await {
            Ok(history) => history,
            Err(e) => {
                tracing::error!(
                    conversation_id,
                    error = %e,
                    "run_stream: 加载对话历史失败"
                );
                let _ = tx.send(ChatStreamChunk::Error {
                    message: format!("加载对话历史失败: {e}"),
                });
                return;
            }
        };
        let prompt_message = build_prompt_message(user_msg, image_parts);
        let mut runner = agent.prompt(prompt_message).history(history);
        if let Some(serde_json::Value::Object(map)) = thinking_patch {
            runner = runner.merge_additional_params(map.clone());
        }
        let stream = runner.stream();
        Self::consume_stream(stream, memory, conversation_id, tx, model_name, idle_timeout).await;
    }

    /// 消费已构造的 `MultiTurnStream`——`run_stream` 与 `run_stream_with_max_tokens` 共用。
    ///
    /// 消费 `MultiTurnStreamItem`（0.45 事件模型，未知变体兜底 continue）：
    /// - `StreamAssistantItem(Event(Text))` -> `ChatStreamChunk::Text`
    /// - `StreamAssistantItem(Event(Reasoning))` -> `ChatStreamChunk::Thinking`
    /// - `ToolCall`（turn 提交时报告，rig 定型的完整调用）-> `ChatStreamChunk::ToolCall`
    /// - `ToolResult { tool_result }`（提交历史的工具结果投影）->
    ///   `ChatStreamChunk::ToolResult { call_id, summary }`（0.45 起从专用变体取，
    ///   不再经 `StreamUserItem` 包装）
    /// - `FinalResponse(resp)` -> 先 `memory.append_turn`（`resp.messages` 为本轮
    ///   transcript，不含输入历史——与旧 rig 自动 append 语义一致），再 emit
    ///   `ChatStreamChunk::Done`（usage 为整轮聚合，计数 Option 化）
    /// - `Err(PromptError)` -> `ChatStreamChunk::Error`（`MaxTurns` 变体精确映射）
    ///
    /// **中断**：调用方 drop `tx`（或 task 被 abort）即中断，stream 被 drop 后 rig 内部
    /// reqwest task 自动 abort（与主窗口 `RigProvider::stream` 一致）。
    async fn consume_stream(
        mut stream: StreamingResult,
        memory: Arc<dyn ChatMemory>,
        conversation_id: &str,
        tx: mpsc::UnboundedSender<ChatStreamChunk>,
        model_name: Option<String>,
        idle_timeout: Duration,
    ) {
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
                        target: blink_infra::utils::perf::ai_slo::TARGET,
                        conversation = %conversation_id,
                        "consume_stream: 等待模型流式响应超时"
                    );
                    let _ = tx.send(ChatStreamChunk::Error {
                        message: "AI 请求超时，请重试或在设置中调整硬超时".to_string(),
                    });
                    return;
                }
            };
            let chunk = match item {
                Ok(MultiTurnStreamItem::StreamAssistantItem(content)) => match content {
                    RigStreamItem::Event(RigStreamEvent::Text { text, .. }) => {
                        has_content = true;
                        tracing::trace!(
                            conversation = %conversation_id,
                            text = single_line(&text),
                            "consume_stream: text delta"
                        );
                        ChatStreamChunk::Text { text }
                    }
                    RigStreamItem::Event(RigStreamEvent::Reasoning { text, .. }) => {
                        has_content = true;
                        tracing::trace!(
                            conversation = %conversation_id,
                            thinking = single_line(&text),
                            "consume_stream: thinking delta"
                        );
                        ChatStreamChunk::Thinking { text }
                    }
                    // Start / Arguments / Unknown——正文由专用变体或 FinalResponse 携带；
                    // End 中定型的 Reasoning 整块（部分 provider/mock 走整块而非增量）
                    // 转为一条完整 Thinking，ToolCall 的定型由专用 ToolCall 变体携带
                    RigStreamItem::Event(RigStreamEvent::End { content, .. }) => {
                        if let rig_core::completion::AssistantContent::Reasoning(reasoning) =
                            &content
                        {
                            has_content = true;
                            let text = reasoning.text.clone();
                            ChatStreamChunk::Thinking { text }
                        } else {
                            continue;
                        }
                    }
                    _ => continue,
                },
                Ok(MultiTurnStreamItem::ToolCall { tool_call }) => {
                    has_content = true;
                    let tool = tool_call.function.name.to_string();
                    let call_id = tool_call.id.to_string();
                    let arguments = tool_call.function.arguments_value().to_string();
                    tracing::debug!(
                        conversation = %conversation_id,
                        tool = %tool,
                        call_id = %call_id,
                        args_chars = arguments.chars().count(),
                        "consume_stream: tool call"
                    );
                    ChatStreamChunk::ToolCall {
                        tool,
                        call_id,
                        arguments,
                    }
                }
                Ok(MultiTurnStreamItem::ToolExecutionCommitted { .. }) => continue,
                Ok(MultiTurnStreamItem::ToolResult { tool_result }) => {
                    has_content = true;
                    let call_id = tool_result.call.to_string();
                    let summary = summarize_tool_result(&tool_result);
                    let success = !summary.is_empty();
                    tracing::debug!(
                        conversation = %conversation_id,
                        call_id = %call_id,
                        success,
                        summary_chars = summary.chars().count(),
                        "consume_stream: tool result"
                    );
                    ChatStreamChunk::ToolResult {
                        call_id,
                        success,
                        summary,
                    }
                }
                Ok(MultiTurnStreamItem::CompletionCall(_)) => continue,
                Ok(MultiTurnStreamItem::ModelTurnRetried { turn }) => {
                    tracing::debug!(
                        conversation = %conversation_id,
                        turn,
                        "consume_stream: 模型回合被 hook 驳回重试，临时输出将被重置"
                    );
                    continue;
                }
                Ok(MultiTurnStreamItem::FinalResponse(resp)) => {
                    done_sent = true;
                    // 0.27.13：回写本轮 transcript。resp.messages 含本轮 prompt user +
                    // assistant 轮 + 工具结果、不含输入历史——与 0.43 rig 自动 append
                    // 收到的消息一致（预写 user 跳过/图片剥离/实况合并都在 append_turn 内）。
                    // 回写失败不打断已完成的回复：Done 照发，错误落日志（下一轮 load
                    // 读到的是缺本轮的历史，属可接受的降级，与旧版 append 失败同界）。
                    if let Err(e) = memory.append_turn(conversation_id, resp.messages).await {
                        tracing::error!(
                            conversation = %conversation_id,
                            error = %e,
                            "consume_stream: 本轮消息回写失败（回复已送达，历史缺本轮）"
                        );
                    }
                    // 0.45: usage 为整轮聚合，计数 Option<u64>（None = 未报告，视同 0）
                    let usage = resp.usage;
                    if !has_content
                        && usage.input_tokens.unwrap_or(0) == 0
                        && usage.output_tokens.unwrap_or(0) == 0
                    {
                        tracing::warn!(
                            conversation = %conversation_id,
                            "consume_stream: 收到空 FinalResponse（0 token + 无内容），\
                             可能是服务过载 / SSE 解析失败 / 配额耗尽"
                        );
                        ChatStreamChunk::Error {
                            message: "AI 返回了无效响应，可能是服务过载或配额耗尽，请稍后重试"
                                .to_string(),
                        }
                    } else {
                        ChatStreamChunk::Done {
                            usage: crate::ai::message::Usage::from_rig_usage(&usage),
                            model_name: model_name.clone(),
                        }
                    }
                }
                // 0.45 起 MultiTurnStreamItem 标记 non_exhaustive——未知变体
                // （未来新增的生命周期标记等）静默跳过，不误报错误
                Ok(_) => continue,
                Err(e) => {
                    match &e {
                        rig_agent::run::PromptError::MaxTurns { max_turns, .. } => {
                            ChatStreamChunk::MaxTurnsReached {
                                max_turns: *max_turns,
                            }
                        }
                        _ => {
                            let msg = format!("{e}");
                            tracing::warn!(
                                target: blink_infra::utils::perf::ai_slo::TARGET,
                                conversation = %conversation_id,
                                error = %msg,
                                error_debug = ?e,
                                "consume_stream: 流式生成错误"
                            );
                            ChatStreamChunk::Error { message: msg }
                        }
                    }
                }
            };
            if tx.send(chunk).is_err() {
                return;
            }
        }
        if !done_sent {
            tracing::warn!(
                target: blink_infra::utils::perf::ai_slo::TARGET,
                conversation = %conversation_id,
                "consume_stream: 流结束但未收到 Done chunk（SSE 解析异常 / 连接被服务端关闭）"
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
/// 在 `AgentProvider::new` 中 `tools` 被 move 进 build_agent 之前调用。
/// - `DynamicTool::definition()` 返回 rig `ToolDefinition`（name + description + parameters）
/// - `rig_rmcp::McpTool::definition()` 暴露原始 rmcp Tool（name + description + input_schema）
///
/// MCP tool 的 `input_schema` 是 `Map<String, Value>`，转为 `serde_json::Value::Object`
/// 以统一到 `ToolPromptInfo.parameters` 的 `Value` 类型。MCP tool 无 `hint` 字段。
fn build_tool_prompt_infos(
    tools: &[DynamicTool],
    mcp_tools: &[rig_rmcp::McpTool],
) -> Vec<crate::ai::prompt::ToolPromptInfo> {
    let mut infos = Vec::with_capacity(tools.len() + mcp_tools.len());

    // 1. DynamicTool（Capability tool）
    for dt in tools {
        let def = dt.definition();
        infos.push(crate::ai::prompt::ToolPromptInfo {
            // 0.45: ToolDefinition.name 为 ToolName
            name: def.name.to_string(),
            description: def.description,
            parameters: def.parameters,
            hint: None, // DynamicTool 不携带 hint；hint 在 system prompt 中已拼接
        });
    }

    // 2. MCP tool
    for tool in mcp_tools {
        let def = tool.definition();
        let params = serde_json::Value::Object(def.input_schema.as_ref().clone());
        infos.push(crate::ai::prompt::ToolPromptInfo {
            name: def.name.to_string(),
            description: def.description.as_deref().unwrap_or("").to_string(),
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
pub fn summarize_tool_result( // 0.25.4 crate 化：pub(crate) → pub（bin 侧跨 crate 使用）
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

/// 构造 `Agent`（0.45: Agent 非泛型、Builder 非 typestate；记忆不在此挂载——
/// 0.27.13 起由 AgentProvider 每轮 `runner.history` 显式注入）。
///
/// `default_temperature` / `default_max_tokens` 从 `ModelEntry` 来，
/// 构造时固化到 `AgentBuilder`，rig Agent 内部生成 `CompletionRequest` 时使用。
/// `None` 表示不覆盖（rig 让 provider 自己决定），但 Anthropic 的 `max_tokens` 是
/// 必填字段，调用方需确保传入 `Some`（见 `AgentProvider::new` 中的兜底逻辑）。
fn build_agent(
    model: impl Into<DynModel<Completion>>,
    preamble: &str,
    tools: Vec<DynamicTool>,
    mcp_tools: Vec<rig_rmcp::McpTool>,
    default_temperature: Option<f64>,
    default_max_tokens: Option<u64>,
) -> Agent {
    let has_tools = !tools.is_empty() || !mcp_tools.is_empty();

    let mut builder = AgentBuilder::new(model)
        .preamble(preamble)
        .default_max_turns(MAX_TURNS);
    if has_tools {
        builder = builder.dynamic_tools(tools);
        // 0.45：MCP 工具经 rig-rmcp 的 `McpTool`（持有 McpConnection 保活）注册，
        // `DynamicTool::try_from` 转换（名称校验可失败——失败的跳过并告警，
        // 不阻断其余工具）；默认带 rig-rmcp 的 DEFAULT_MCP_TOOL_TIMEOUT（300s）。
        for tool in mcp_tools {
            match DynamicTool::try_from(tool) {
                Ok(dynamic) => builder = builder.dynamic_tool(dynamic),
                Err(e) => {
                    tracing::warn!(error = %e, "build_agent: MCP tool 注册失败，已跳过");
                }
            }
        }
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
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
        // 0.45: Usage 标记 non_exhaustive——经 new() + pub 字段构造
        let mut usage = RigUsage::new();
        usage.input_tokens = Some(150);
        usage.output_tokens = Some(80);
        usage.total_tokens = Some(230);
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hello"),
            MockStreamEvent::final_response(usage),
        ]]);
        let agent = AgentBuilder::new(model)
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
        let mut usage = RigUsage::new();
        usage.input_tokens = Some(u64::from(u32::MAX) + 1000); // 超 u32 范围
        usage.output_tokens = Some(50);
        usage.total_tokens = Some(0);
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("hi"),
            MockStreamEvent::final_response(usage),
        ]]);
        let agent = AgentBuilder::new(model)
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
    /// 0.27.10：typed 多模态消息构造——无图保持纯文本；有图 Text 在前 +
    /// base64 Image parts（0.27.12：raw 变体被 OpenAICompatible/Gemini wire 拒绝）。
    #[test]
    fn build_prompt_message_with_and_without_images() {
        use rig_core::completion::message::UserContent;

        // 无图：与 Message::user 等价
        let plain = build_prompt_message("你好", &[]);
        match &plain {
            rig_core::completion::Message::User { content } => {
                assert_eq!(content.len(), 1);
                assert!(matches!(content[0], UserContent::Text(_)));
            }
            other => panic!("应为 User 消息: {other:?}"),
        }

        // 有图：正文 Text part + PNG/JPEG base64 Image parts
        let parts = vec![
            VisionImagePart { mime: "image/png".into(), bytes: vec![1, 2, 3] },
            VisionImagePart { mime: "image/jpeg".into(), bytes: vec![4, 5] },
        ];
        let msg = build_prompt_message("看图", &parts);
        match &msg {
            rig_core::completion::Message::User { content } => {
                assert_eq!(content.len(), 3, "Text + 2 Image");
                assert!(matches!(content[0], UserContent::Text(_)));
                assert!(matches!(content[1], UserContent::Image(_)));
                assert!(matches!(content[2], UserContent::Image(_)));
                // 序列化含 media type 与 base64 数据（wire 侧可识别格式，
                // 非 raw 字节数组——OpenAICompatible/Gemini 拒绝 Raw 变体）
                let json = serde_json::to_string(&msg).unwrap();
                assert!(json.contains("\"media_type\":\"png\""), "{json}");
                assert!(json.contains("\"media_type\":\"jpeg\""), "{json}");
                assert!(
                    json.contains(&base64_encode(&[1, 2, 3])),
                    "图片数据为 base64 编码: {json}"
                );
            }
            other => panic!("应为 User 消息: {other:?}"),
        }

        // 空正文纯图：只有 Image parts
        let only = build_prompt_message("", &parts);
        match &only {
            rig_core::completion::Message::User { content } => {
                assert_eq!(content.len(), 2, "空正文不产生空 Text part");
            }
            other => panic!("应为 User 消息: {other:?}"),
        }
    }

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
            CallId, DocumentSourceKind, Image, Text, ToolResult, ToolResultContent,
        };
        use rig_core::message::ToolName;

        // 短文本不截断
        let short = ToolResult {
            call: CallId::from_wire("1"),
            name: ToolName::try_from("test").unwrap(),
            content: vec![ToolResultContent::Text(Text::new("ok"))],
            is_error: false,
        };
        assert_eq!(summarize_tool_result(&short), "ok");

        // 长文本截断到 50000 字符 + 省略号
        let long_text = "x".repeat(60000);
        let long = ToolResult {
            call: CallId::from_wire("2"),
            name: ToolName::try_from("test").unwrap(),
            content: vec![ToolResultContent::Text(Text::new(long_text))],
            is_error: false,
        };
        let summary = summarize_tool_result(&long);
        assert_eq!(summary.chars().count(), 50001, "50000 字符 + 1 省略号");
        assert!(summary.ends_with('…'));

        // 图片转占位
        let img = ToolResult {
            call: CallId::from_wire("3"),
            name: ToolName::try_from("test").unwrap(),
            content: vec![ToolResultContent::Image(Image {
                data: DocumentSourceKind::Url("http://example.com/x.png".into()),
                media_type: None,
                detail: None,
                native: None,
            })],
            is_error: false,
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
            usage: crate::ai::message::Usage {
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
            .default_max_turns(5)
            .build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, Arc::new(EphemeralConversationMemory::new()), "c1", "hi", &[], tx, None, None).await;

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
        blink_infra::data::conversations::init_db(&pool)
            .await
            .expect("init tables");
        let mem = std::sync::Arc::new(crate::ai::memory::SqliteConversationMemory::new(
            pool,
        ));

        // 1. 发出即保存：预写当前 user（与 ChatService::prompt 一致）
        mem.persist_user_message("c1", "hello", &[]).await.unwrap();

        // 2. 走 rig agent 流式路径（与 stream_prompt 一致），mock 记录收到的请求
        let model = MockCompletionModel::from_stream_turns(vec![vec![
            MockStreamEvent::text("reply"),
            MockStreamEvent::final_response_with_default_usage(),
        ]]);
        let recorded = model.clone();
        // 0.27.13：记忆不再挂 builder——run_stream 显式 load 注入；传 SQLite
        // memory 与 ChatService::prompt 的真实路径一致（预写 user 不进上下文）。
        let agent = AgentBuilder::new(model).default_max_turns(5).build();
        let (tx, mut rx) = mpsc::unbounded_channel();
        AgentProvider::run_stream(&agent, mem.clone(), "c1", "hello", &[], tx, None, None).await;
        while rx.recv().await.is_some() {}

        // 3. 检查模型实际收到的 chat_history：user 应恰好 1 次
        let requests = recorded.requests();
        assert_eq!(requests.len(), 1, "应恰好 1 次请求");
        let history = &requests[0].chat_history;
        let user_texts: Vec<String> = history
            .iter()
            .filter_map(|m| match m.as_ref() {
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
        use rig_core::message::ToolName;
        let tool1 = DynamicTool::new(
            ToolName::try_from("search_apps").unwrap(),
            "搜索应用",
            serde_json::json!({"type":"object","properties":{"query":{"type":"string"}}}),
            |_args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );
        let tool2 = DynamicTool::new(
            ToolName::try_from("open_url").unwrap(),
            "打开网址",
            serde_json::json!({"type":"object","properties":{"url":{"type":"string"}}}),
            |_args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
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
        use crate::config::ai_config::{
            ModelCapability, ModelEntry, ProviderEntry, ProviderKind,
        };
        use rig_agent::tool::ToolOutput;

        let tool = DynamicTool::new(
            rig_core::message::ToolName::try_from("test_tool").unwrap(),
            "测试工具",
            serde_json::json!({"type":"object"}),
            |_args| Box::pin(async { Ok(ToolOutput::text("ok")) }),
        );

        let entry = ProviderEntry {
            id: "test".into(),
            display_name: "Test".into(),
            kind: ProviderKind::OllamaHttp,
            compat_dialect: Default::default(),
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
            thinking_style: None,
            custom_parameters: Vec::new(),
        };

        let provider = AgentProvider::new(
            &entry,
            &model,
            vec![tool],
            Vec::new(),
            "",
            Arc::new(EphemeralConversationMemory::new()),
        )
        .await
        .expect("AgentProvider 构造成功");

        let infos = provider.tool_prompt_infos();
        assert_eq!(infos.len(), 1);
        assert_eq!(infos[0].name, "test_tool");
        assert_eq!(infos[0].description, "测试工具");
    }
    #[tokio::test]
    async fn agent_custom_parameters_and_request_effort_reach_the_wire() {
        use crate::ai::factory::wire_tests::{spawn_capture_server, sse_stream_response};
        use crate::config::ai_config::CustomParam;
        let (addr, captured) =
            spawn_capture_server(vec![sse_stream_response(), sse_stream_response()]);
        let entry = ProviderEntry {
            id: "wire".into(),
            display_name: "wire".into(),
            kind: ProviderKind::OpenAICompatible,
            compat_dialect: Default::default(),
            base_url: Some(format!("http://{addr}/v1")),
            secret_ref: String::new(),
            models: vec![],
            enabled: true,
            created_at: 0,
        };
        let model = ModelEntry {
            id: "wire-model".into(),
            display_name: "wire".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: Some(2048),
            capabilities: vec![],
            reasoning_effort: None,
            thinking_style: None,
            custom_parameters: vec![
                CustomParam {
                    key: "top_p".into(),
                    value: serde_json::json!(0.7),
                },
                CustomParam {
                    key: "reasoning_effort".into(),
                    value: serde_json::json!("low"),
                },
                CustomParam {
                    key: "max_tokens".into(),
                    value: serde_json::json!(8192),
                },
                CustomParam {
                    key: "vendor_extension".into(),
                    value: serde_json::json!({"nested":true}),
                },
            ],
        };
        let provider = AgentProvider::with_key(
            &entry,
            &model,
            vec![],
            vec![],
            "",
            Arc::new(EphemeralConversationMemory::new()),
            "wire-test-key",
        )
        .unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            provider.stream_prompt("wire1", "hi", &[], tx, true, Some("high".into())),
        )
        .await
        .expect("bounded agent request");
        let mut done = false;
        while let Some(chunk) = rx.recv().await {
            assert!(!matches!(chunk, ChatStreamChunk::Error { .. }), "{chunk:?}");
            done |= matches!(chunk, ChatStreamChunk::Done { .. });
        }
        assert!(done);
        let (tx, _rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(5),
            provider.stream_prompt_with_max_tokens("wire2", "summary", tx, false, None, 600),
        )
        .await
        .expect("bounded summary request");
        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2);
        let body: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        assert_eq!(body["top_p"], 0.7);
        assert_eq!(body["reasoning_effort"], "high");
        assert_eq!(body["vendor_extension"]["nested"], true);
        let summary: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
        assert_eq!(summary["max_tokens"], 600);
        assert_eq!(summary["top_p"], 0.7);
    }

    /// 0.27.14：对话窗口链（AgentProvider）消费显式 DeepSeek 方言——自建代理场景
    /// （host 非官方 + compat_dialect=DeepSeek）。多轮工具回放：
    /// 请求1 携带 thinking.type=enabled（thinking 判定与方言对齐）与 tools；
    /// 工具执行后请求2 的 assistant 历史带 tool_calls + reasoning_content 回传字段，
    /// tool 结果以 tool 消息回放——不丢 reasoning、不丢工具结果。
    #[tokio::test]
    async fn agent_deepseek_dialect_tool_roundtrip() {
        use crate::ai::factory::wire_tests::spawn_capture_server;
        use crate::config::ai_config::CompatDialect;
        use rig_agent::tool::ToolOutput;

        // SSE 桩1：tool_calls delta + tool_calls finish；桩2：文本 + stop
        let tool_call_sse = {
            let frame = |delta: &str, finish: &str| {
                format!(
                    r#"{{"id":"wire-tc","object":"chat.completion.chunk","created":1,"model":"m","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]}}"#
                )
            };
            let body = format!(
                "data: {}

data: {}

data: [DONE]

",
                frame(
                    r#"{"role":"assistant","tool_calls":[{"index":0,"id":"call-ds-1","type":"function","function":{"name":"echo_tool","arguments":"{\"x\":1}"}}]}"#,
                    "null"
                ),
                frame("{}", "\"tool_calls\""),
            );
            format!(
                "HTTP/1.1 200 OK
Content-Type: text/event-stream
Connection: close
Content-Length: {}

{}",
                body.len(),
                body
            )
            .into_bytes()
        };
        let (addr, captured) =
            spawn_capture_server(vec![tool_call_sse, crate::ai::factory::wire_tests::sse_stream_response()]);

        let entry = ProviderEntry {
            id: "ds-wire".into(),
            display_name: "DS proxy".into(),
            kind: ProviderKind::OpenAICompatible,
            compat_dialect: CompatDialect::DeepSeek,
            base_url: Some(format!("http://{addr}/v1")),
            secret_ref: String::new(),
            models: vec![],
            enabled: true,
            created_at: 0,
        };
        let model = ModelEntry {
            id: "deepseek-v4-flash".into(),
            display_name: "DS".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            capabilities: vec![],
            reasoning_effort: None,
            thinking_style: None,
            custom_parameters: vec![],
        };
        let tool = DynamicTool::new(
            rig_core::message::ToolName::try_from("echo_tool").unwrap(),
            "回显工具",
            serde_json::json!({"type":"object"}),
            |_args| Box::pin(async { Ok(ToolOutput::text("echoed")) }),
        );
        let provider = AgentProvider::with_key(
            &entry,
            &model,
            vec![tool],
            vec![],
            "",
            Arc::new(EphemeralConversationMemory::new()),
            "ds-wire-key",
        )
        .unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        tokio::time::timeout(
            Duration::from_secs(10),
            provider.stream_prompt("ds1", "hi", &[], tx, true, None),
        )
        .await
        .expect("bounded agent loop");
        let mut done = false;
        while let Some(chunk) = rx.recv().await {
            assert!(!matches!(chunk, ChatStreamChunk::Error { .. }), "{chunk:?}");
            done |= matches!(chunk, ChatStreamChunk::Done { .. });
        }
        assert!(done, "agent loop 应走完工具轮 + 回答轮");

        let requests = captured.lock().unwrap();
        assert_eq!(requests.len(), 2, "工具调用 → 工具结果 → 再回答共两轮请求");
        let first: serde_json::Value = serde_json::from_str(&requests[0].body).unwrap();
        // thinking 与方言对齐：显式 DeepSeek 方言 → thinking.type（而非 reasoning_effort）
        assert_eq!(
            first["thinking"]["type"], "enabled",
            "DeepSeek 方言链路的思考开关应发 thinking.type: {first}"
        );
        assert!(
            first["tools"].as_array().is_some_and(|t| !t.is_empty()),
            "工具定义随首轮请求下发"
        );
        let second: serde_json::Value = serde_json::from_str(&requests[1].body).unwrap();
        let assistant: Vec<&serde_json::Value> = second["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .collect();
        assert!(
            assistant
                .iter()
                .any(|m| m.get("reasoning_content").is_some()),
            "DeepSeek 方言下 assistant 历史必须带 reasoning_content 回传: {second}"
        );
        assert!(
            assistant.iter().any(|m| m
                .get("tool_calls")
                .is_some_and(|t| !t.as_array().unwrap().is_empty())),
            "assistant 工具调用必须完整回放: {second}"
        );
        assert!(
            second["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool"
                && m["tool_call_id"] == "call-ds-1"),
            "工具结果必须以 tool 消息回放（不丢结果）: {second}"
        );
    }
}

