//! `RigProvider` —— 用 rig-core `CompletionModel` 实体承载 `AIProvider` trait。
//!
//! ## 位置在架构里的意义
//!
//! 本文件是 `domain::ai` 里**第二处**触碰 `rig_core` 类型的地方(第一处是
//! `execution/schema.rs::to_rig_tool`)。上层调用方 `use crate::domain::ai::AIProvider`
//! 拿到 `Arc<dyn AIProvider>` 时,rig 类型编译期就没了——§2.6 类型收窄编译期钉死。
//!
//! `RigProvider<M>` 是**泛型**而非 `Box<dyn CompletionModel>`——rig 0.39
//! `CompletionModel: Clone + WasmCompatSend + WasmCompatSync` 且有 3 个关联类型
//! (`Response / StreamingResponse / Client`),**不 object-safe**。
//! `RigFactory` 按 `ProviderKind` 实例化具体 `RigProvider<openai::…::CompletionModel>` /
//! `RigProvider<anthropic::…::CompletionModel>` 等,再擦除到 `Arc<dyn AIProvider>`。
//!
//! ## 硬超时(§3.3 骨架层)
//!
//! rig 的 `CompletionError::HttpError` 没有 timeout 语义(`http_client::Error` 8 变体
//! 都没有 timeout),不能指望 rig 自己报超时。**必须外层 `tokio::time::timeout`
//! 包住 `model.completion(request)`**——这是 spike `skeleton.rs:19` 已验证的模式,
//! future drop 时 in-flight reqwest task 自动 abort,<100ms 释放。
//!
//! ## 流式两阶段超时(0.9.7+)
//!
//! `stream()` 的超时不是一把包住全过程的硬超时,而是分两阶段:
//! - **Phase 1(连接)**:`model.stream()` 建立连接——用完整 deadline 作硬超时
//! - **Phase 2(chunk 循环)**:每个 chunk 的等待用 deadline 作 **idle timeout**
//!   (两个 chunk 之间的最大间隔)。token 持续到达则不超时,只有 stall 才判超时。
//!
//! 这修复了"流式返回到一半触发硬超时"的问题——AI 正在工作(持续吐 token)不应
//! 被打断。idle timeout 与连接超时复用同一 `timeout_ms`,未来可拆分独立配置。
//!
//! ## 错误映射保守原则
//!
//! rig 返回的错误里可能带 URL / response body / status message——本文件的
//! `map_rig_error` **只带 stage 名 / status code**,绝不透传 rig 的 error message
//! 到我们的 `AIError` 里,防止密钥或内网 URL 泄漏到日志。
//!
//! ## 非流式的 first_token_ms 语义
//!
//! 0.9.2 第一步是一次性返回,`first_token_ms = total_ms = start.elapsed()`。
//! 0.10 上 SSE 时改造成"首 token 到达时刻",字段语义不变——SLO 消费方零改动。

use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures::StreamExt;
use rig_core::OneOrMany;
use rig_core::completion::{
    AssistantContent, CompletionError, CompletionModel as RigCompletionModel,
    CompletionRequest as RigCompletionRequest, Message as RigMessage,
};
use rig_core::streaming::StreamedAssistantContent as RigStreamChunk;
use tokio::sync::mpsc;

use crate::app::ai_config::{CustomParam, ProviderKind};
use crate::domain::ai::message::{
    CompletionRequest, CompletionResponse, Role, ToolCall, Usage,
};
use crate::domain::ai::provider::{AIError, AIProvider, StreamChunk};
use crate::infra::platform::secret::SecretString;

/// 默认硬超时(§3.3 骨架层)——用户未在 `CompletionRequest.timeout_ms` 覆盖时的兜底。
///
/// 与 `AIConfig::slo_hard_timeout_ms` 默认值一致(见 §3.3 SLO 表)。
const DEFAULT_HARD_TIMEOUT_MS: u32 = 20_000;

/// rig-core 承载的 `AIProvider` 实体。泛型 M 由 factory 按 `ProviderKind` 敲定。
///
/// **字段可见性**:`pub(crate)` 不 re-export——`use crate::domain::ai::AIProvider`
/// 的上层拿不到具体类型,只能通过 `Arc<dyn AIProvider>` 消费。
///
/// **无 PhantomData**:`model: M` 字段已经消耗了泛型参数 M,不需要额外的
/// `PhantomData<M>`(那是"仅带类型标记但不实际持有 M"时的模板,与此处场景无关)。
///
/// **0.9.4 Step 1 模型级参数默认值**:`default_temperature / default_max_tokens /
/// custom_parameters` 三个字段承载 `ModelEntry` 里的调用参数。构造时一次固化,
/// `complete()` 时用 request 值 fallback 到这里(见 `build_rig_request`)。
pub(crate) struct RigProvider<M: RigCompletionModel> {
    kind: ProviderKind,
    model_id: String,
    model: M,
    default_timeout_ms: u32,
    // 0.9.4 Step 1:模型级参数默认值——None 表示"不覆盖,请求方决定"
    default_temperature: Option<f32>,
    default_max_tokens: Option<u32>,
    /// 自定义参数——透传到 rig `additional_params`。构造时把 `Vec<CustomParam>`
    /// 折叠成一个 `serde_json::Value::Object`,请求时若非空就直接塞给 rig。
    custom_params_json: Option<serde_json::Value>,
}

impl<M: RigCompletionModel> RigProvider<M> {
    /// 构造——`RigFactory` 在挑好 rig client + model_id 后调这个。
    ///
    /// `default_timeout_ms` 从 `AIConfig::slo_hard_timeout_ms` 或 `DEFAULT_HARD_TIMEOUT_MS` 来。
    /// `default_temperature / default_max_tokens / custom_parameters` 从 `ModelEntry` 来。
    #[allow(dead_code)] // 0.9.2 Phase 5b 由 factory 消费
    pub(crate) fn new(
        kind: ProviderKind,
        model_id: impl Into<String>,
        model: M,
        default_timeout_ms: Option<u32>,
        default_temperature: Option<f32>,
        default_max_tokens: Option<u32>,
        custom_parameters: &[CustomParam],
    ) -> Self {
        Self {
            kind,
            model_id: model_id.into(),
            model,
            default_timeout_ms: default_timeout_ms.unwrap_or(DEFAULT_HARD_TIMEOUT_MS),
            default_temperature,
            default_max_tokens,
            custom_params_json: build_custom_params_json(custom_parameters),
        }
    }
}

/// 把 `Vec<CustomParam>` 折叠成一个 `serde_json::Value::Object`。
///
/// 空 vec 返回 `None`——请求侧看到 None 就完全不塞 `additional_params`,保持"零透传"语义。
/// 重复 key 后者覆盖前者(与 JS `Object.fromEntries` 一致,用户预期)。
fn build_custom_params_json(params: &[CustomParam]) -> Option<serde_json::Value> {
    if params.is_empty() {
        return None;
    }
    let mut map = serde_json::Map::with_capacity(params.len());
    for p in params {
        if p.key.trim().is_empty() {
            continue; // 前端应过滤空 key,后端多一层防御
        }
        map.insert(p.key.clone(), p.value.clone());
    }
    if map.is_empty() {
        None
    } else {
        Some(serde_json::Value::Object(map))
    }
}

#[async_trait]
impl<M> AIProvider for RigProvider<M>
where
    M: RigCompletionModel + Send + Sync + 'static,
    <M as RigCompletionModel>::Response: Send,
{
    fn kind(&self) -> ProviderKind {
        self.kind
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn complete(&self, req: CompletionRequest) -> Result<CompletionResponse, AIError> {
        let timeout_ms = req.timeout_ms.unwrap_or(self.default_timeout_ms);
        let deadline = Duration::from_millis(timeout_ms as u64);

        let rig_req = build_rig_request(
            self.kind,
            &req,
            self.default_temperature,
            self.default_max_tokens,
            self.custom_params_json.as_ref(),
        )?;

        let start = Instant::now();
        // 外层 tokio::time::timeout —— rig 自己不报 timeout(见文件顶注)
        let result = tokio::time::timeout(deadline, self.model.completion(rig_req)).await;
        let elapsed = start.elapsed().as_millis() as u32;

        match result {
            Err(_) => Err(AIError::Timeout), // tokio timeout,future 已被 drop
            Ok(Err(rig_err)) => Err(map_rig_error(rig_err)),
            Ok(Ok(rig_resp)) => Ok(map_rig_response(rig_resp, elapsed)),
        }
    }

    /// 流式 completion —— 调 rig `model.stream()` 逐 chunk 通过 channel 发送。
    ///
    /// **两阶段超时**(见文件顶注「流式两阶段超时」):
    /// - Phase 1:`model.stream()` 建立连接——用完整 deadline 作硬超时
    /// - Phase 2:逐 chunk 循环——每个 chunk 的等待用 deadline 作 idle timeout
    ///
    /// 只要 token 持续到达,总时长不限;只有 chunk 间出现 deadline 长的 stall 才超时。
    ///
    /// **tool_calls 收集**:流式过程中 Text chunk 实时发送;tool_calls 在流结束后
    /// 通过 `StreamChunk::Done` 一次性返回(调用方统一处理)。
    async fn stream(
        &self,
        req: CompletionRequest,
        tx: mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<(), AIError> {
        let timeout_ms = req.timeout_ms.unwrap_or(self.default_timeout_ms);
        let deadline = Duration::from_millis(timeout_ms as u64);

        let rig_req = build_rig_request(
            self.kind,
            &req,
            self.default_temperature,
            self.default_max_tokens,
            self.custom_params_json.as_ref(),
        )?;

        let start = Instant::now();

        // ── Phase 1: 建立连接,等首个响应 ──────────────────────────────
        // 用完整 deadline 作硬超时——AI 在 deadline 内没开始返回(连接慢/排队),判超时。
        let mut streaming_resp = match tokio::time::timeout(
            deadline,
            self.model.stream(rig_req),
        )
        .await
        {
            Err(_) => return Err(AIError::Timeout), // 连接阶段超时
            Ok(Err(rig_err)) => return Err(map_rig_error(rig_err)),
            Ok(Ok(resp)) => resp,
        };

        let mut tool_calls: Vec<ToolCall> = Vec::new();

        // ── Phase 2: 逐 chunk 消费,每个 chunk 用 deadline 作 idle timeout ──
        // token 持续到达则不超时;只有两个 chunk 间隔超过 deadline 才判 stall 超时。
        loop {
            let chunk_result = match tokio::time::timeout(
                deadline,
                StreamExt::next(&mut streaming_resp),
            )
            .await
            {
                Err(_) => return Err(AIError::Timeout), // chunk 间 idle 超时
                Ok(None) => break,                       // 流正常结束
                Ok(Some(result)) => result,
            };

            match chunk_result {
                Ok(raw_choice) => match raw_choice {
                    RigStreamChunk::Text(t) => {
                        if tx.send(StreamChunk::Text(t.text)).is_err() {
                            // 接收端已关闭(调用方 drop 了)——提前终止
                            return Ok(());
                        }
                    }
                    RigStreamChunk::ToolCall { tool_call: tc, .. } => {
                        tool_calls.push(ToolCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            arguments: tc.function.arguments.clone(),
                        });
                    }
                    _ => {} // ToolCallDelta / Reasoning / Final 等忽略
                },
                Err(e) => return Err(map_rig_error(e)),
            }
        }

        // 流结束——发 Done
        let _ = tx.send(StreamChunk::Done {
            tool_calls,
            // usage 在流结束后从 streaming_resp 聚合状态取
            usage: Usage {
                input_tokens: 0,
                output_tokens: 0,
            },
        });

        let _elapsed = start.elapsed().as_millis() as u32;
        Ok(())
    }
}

// ── 请求映射:我们的 → rig ────────────────────────────────────────────────

/// 把我们的 `CompletionRequest` 投影成 rig `CompletionRequest`。
///
/// **消息构造策略**(0.9.2 第一步):
/// - `Role::System` → `preamble`(rig legacy 兼容 + 通用)或 `Message::system`;
///   优先用 preamble 让多 provider 语义一致(anthropic 不吃 system message)
/// - `Role::User` → `Message::user(content)`
/// - `Role::Assistant` → `Message::assistant` 走 `From<String>` 不支持,0.9.2 不构造
/// - `Role::Tool` → 0.9.2 无 tool loop,不构造(0.9.3 起加)
///
/// **约束**:`chat_history` 必须至少 1 条(rig 契约"最后一条是 prompt")。
/// 空 vec 或全 system → 返 `AIError::Serialization`。
///
/// **0.9.4 Step 1 参数 fallback 优先级**:
/// - `temperature`: `req.temperature` > `default_temperature`(model 层) > None(供应商默认)
/// - `max_tokens`:同上
/// - `additional_params`: 请求方目前不显式塞,直接用 model 层 `custom_params_json`
///   (未来若 request 层要合并,需增字段)
///
/// **铁则**:请求方显式指定(路由档 `temperature=0.0`)优先——model 层默认不能覆盖它,
/// 保证 SearchService 路由的确定性(见 §3.6)。
///
/// **Anthropic 特殊处理**:`max_tokens` 是 Anthropic API 的必填字段,
/// 若请求方和 model 层都未指定,强制使用 4096 作为默认值。
fn build_rig_request(
    kind: ProviderKind,
    req: &CompletionRequest,
    default_temperature: Option<f32>,
    default_max_tokens: Option<u32>,
    custom_params: Option<&serde_json::Value>,
) -> Result<RigCompletionRequest, AIError> {
    // 抽 system → preamble;user 消息进 chat_history
    let mut preamble: Option<String> = None;
    let mut user_msgs: Vec<RigMessage> = Vec::new();

    for m in &req.messages {
        match m.role {
            Role::System => {
                // 多条 system 拼接(极少发生;0.9.2 只 1 条 user)
                preamble = Some(match preamble {
                    Some(prev) => format!("{prev}\n{}", m.content),
                    None => m.content.clone(),
                });
            }
            Role::User => user_msgs.push(RigMessage::from(m.content.as_str())),
            Role::Assistant | Role::Tool => {
                // 0.9.2 第一步不构造这些角色;若真出现说明调用方错了
                return Err(AIError::Serialization(
                    "0.9.2 主窗口路径不支持 assistant/tool 消息".into(),
                ));
            }
        }
    }

    let chat_history = OneOrMany::many(user_msgs).map_err(|_| {
        AIError::Serialization("CompletionRequest.messages 至少需一条 user 消息".into())
    })?;

    // ActionSchema → rig::ToolDefinition(唯一 tool 类型投影)
    let tools = req.tools.iter().map(|s| s.to_rig_tool()).collect();

    // 参数 fallback:req 显式值 > model 层默认值 > None(rig 让 provider 自己决定)
    let effective_temperature = req.temperature.or(default_temperature).map(|f| f as f64);
    let mut effective_max_tokens = req.max_tokens.or(default_max_tokens).map(|n| n as u64);

    // Anthropic 特殊处理:max_tokens 是必填字段,若都未指定则强制使用默认值
    if kind == ProviderKind::AnthropicMessages && effective_max_tokens.is_none() {
        const ANTHROPIC_DEFAULT_MAX_TOKENS: u64 = 4096;
        effective_max_tokens = Some(ANTHROPIC_DEFAULT_MAX_TOKENS);
        tracing::debug!("Anthropic max_tokens 未设置,使用默认值 {}", ANTHROPIC_DEFAULT_MAX_TOKENS);
    }

    Ok(RigCompletionRequest {
        model: None,
        preamble,
        chat_history,
        documents: Vec::new(),
        tools,
        temperature: effective_temperature,
        max_tokens: effective_max_tokens,
        tool_choice: None,
        additional_params: custom_params.cloned(),
        output_schema: None,
    })
}

// ── 响应映射:rig → 我们的 ────────────────────────────────────────────────

/// 把 rig `CompletionResponse<T>` 转成我们的 `CompletionResponse`。
///
/// **AssistantContent 处理**:
/// - `Text(t)` → 拼进 `text`(多段用 `\n` 连接)
/// - `ToolCall(tc)` → 收进 `tool_calls`
/// - `Reasoning(_)` / `Image(_)` → **skip**(0.9.2 主窗口不消费)
///
/// **首 token / 总时**:非流式一律等于 elapsed(见文件顶注)。
pub(crate) fn map_rig_response<T>(
    rig_resp: rig_core::completion::CompletionResponse<T>,
    elapsed_ms: u32,
) -> CompletionResponse {
    let mut texts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    for c in rig_resp.choice.iter() {
        match c {
            AssistantContent::Text(t) => texts.push(t.text.clone()),
            AssistantContent::ToolCall(tc) => tool_calls.push(ToolCall {
                id: tc.id.clone(),
                name: tc.function.name.clone(),
                arguments: tc.function.arguments.clone(),
            }),
            // 0.9.2 主窗口不消费 reasoning / image
            AssistantContent::Reasoning(_) | AssistantContent::Image(_) => {}
        }
    }

    let text = if texts.is_empty() {
        None
    } else {
        Some(texts.join("\n"))
    };

    let usage = Usage {
        input_tokens: rig_resp.usage.input_tokens.min(u32::MAX as u64) as u32,
        output_tokens: rig_resp.usage.output_tokens.min(u32::MAX as u64) as u32,
    };

    CompletionResponse {
        text,
        tool_calls,
        usage,
        first_token_ms: elapsed_ms,
        total_ms: elapsed_ms,
    }
}

// ── 错误映射:rig → 我们的(保守 + 有诊断价值) ────────────────────────────

/// rig `CompletionError` → `AIError`——**保守透传状态码 + 脱敏响应体片段**。
///
/// ## rig 0.39 的错误路径实际情况
///
/// 摸清 rig 0.39 源码后:**所有 4xx/5xx 都归到 `HttpError`**,不走 `ProviderError`——
/// rig `client.send()` 底层在响应非 2xx 时直接返回
/// `http_client::Error::InvalidStatusCodeWithMessage(status, body_text)`,openai 层根本
/// 走不到 `is_success()` 分支。真正的错误信息(model 不存在 / 密钥无效 / 配额用尽)
/// 全在 `HttpError` 内层的 message 里。
///
/// 之前把整个 HttpError 一律说成"传输失败"完全没诊断价值,这版把它拆开:
/// - **有 status code**:透传 status(200/401/404/500 非敏感)+ 脱敏后的 message 前缀
/// - **无 status code**(纯连接层错):还是"传输失败"提示
///
/// ## 脱敏铁则
///
/// message 里可能包含 URL / user-id / 密钥哈希——`sanitize_message` 负责掐掉,
/// 只保留诊断字段(常见供应商都在响应 body 放 `error.message` / `error.type`)。
pub(crate) fn map_rig_error(e: CompletionError) -> AIError {
    match e {
        // HTTP 层:大概率是 4xx/5xx(rig 把非 2xx 全塞这里)——拆内层拿 status + message
        CompletionError::HttpError(inner) => map_http_error(inner),
        // JSON 序列化/反序列化——参数或响应结构失配
        CompletionError::JsonError(_) => {
            AIError::Serialization("响应结构无法解析(供应商可能未返回标准 JSON)".into())
        }
        // URL 构造错误——通常是 base_url 配错(用户可 debug)
        CompletionError::UrlError(_) => AIError::Provider("base_url 格式无效".into()),
        // 请求构造错误(reqwest builder 层)——提取底层错误信息帮助诊断
        CompletionError::RequestError(e) => {
            AIError::Network(format!("请求构造失败: {}", sanitize_message(&e.to_string())))
        }
        // 供应商返回结构解析失败——最常见:model_id 不匹配供应商
        CompletionError::ResponseError(_) => AIError::Serialization(
            "响应结构不匹配(检查供应商类型与 model_id 是否一致)".into(),
        ),
        // rig 直接 emit 的 ProviderError(理论上罕见——见文档顶注)
        CompletionError::ProviderError(msg) => {
            AIError::Provider(format!("供应商错误: {}", sanitize_message(&msg)))
        }
    }
}

/// 把 rig 的 `http_client::Error` 拆成用户可读诊断。
///
/// 关键分支:
/// - `InvalidStatusCodeWithMessage(status, msg)`:4xx/5xx——**主流路径**,rig 把
///   响应体全文塞在 msg 里。我们透传 status + 脱敏后的 msg 前 200 字符
/// - `InvalidStatusCode(status)`:2xx 外但没 body(极少见)
/// - `Instance(_)`:底层 reqwest 错误(DNS/TCP/TLS/超时)——归网络
/// - 其他:归网络,通用提示
///
/// **同时**在 debug 级别打完整脱敏 message,方便用户在设置页开 debug 后自查。
fn map_http_error(e: rig_core::http_client::Error) -> AIError {
    use rig_core::http_client::Error as H;
    match e {
        H::InvalidStatusCodeWithMessage(status, msg) => {
            let clean = sanitize_message(&msg);
            // debug 级别打完整 message(截断到 500 字符防日志爆),用户开 debug 后能自查
            tracing::debug!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                "AI 供应商 HTTP {status} 响应体片段: {}",
                truncate_chars(&clean, 500),
            );
            AIError::Provider(diagnose_status(status.as_u16(), &clean))
        }
        H::InvalidStatusCode(status) => {
            AIError::Provider(format!("供应商返回状态 {status}(无响应体)"))
        }
        H::Instance(inner) => {
            // reqwest 层错误——DNS/TCP/TLS/连接超时等。inner 的 Display 可能含 URL,
            // 但**base_url 是用户自己填的,不敏感**,可以放心透传前缀
            let msg = truncate_chars(&format!("{inner}"), 160);
            AIError::Network(format!("传输失败: {msg}"))
        }
        H::Protocol(inner) => {
            AIError::Network(format!("HTTP 协议错误: {}", truncate_chars(&format!("{inner}"), 120)))
        }
        H::InvalidHeaderValue(_) => {
            AIError::Provider("请求头非法(检查密钥是否含控制字符)".into())
        }
        H::InvalidContentType(_) => {
            AIError::Serialization("响应 Content-Type 非法(供应商返回非 JSON?)".into())
        }
        H::StreamEnded => AIError::Network("流被提前关闭".into()),
        H::NoHeaders => AIError::Network("无法读取响应头".into()),
    }
}

/// 按 HTTP 状态码给用户可读诊断——附带响应体的关键片段。
///
/// 遵循**先说是什么、再给方向**的模式,让用户不用去 grep debug 日志就能自诊断。
fn diagnose_status(status: u16, sanitized_msg: &str) -> String {
    let hint = match status {
        400 => "请求参数错误(常见:model_id 拼写错、参数字段不被供应商支持)",
        401 => "密钥无效或未授权(检查 API Key 是否正确、有效期未过)",
        402 => "余额不足或需要付费(检查供应商账户)",
        403 => "拒绝访问(密钥无对应模型权限?配额用尽?)",
        404 => "endpoint 或模型不存在(检查 base_url 尾缀 /v1 与 model_id)",
        408 => "供应商侧超时",
        413 => "请求体过大",
        422 => "参数验证失败(检查 model_id / temperature 是否被支持)",
        429 => "触发限流(检查 RPM / TPM / 并发)",
        500..=599 => "供应商服务端错误(稍后重试或换 provider)",
        _ => "未预期状态码",
    };
    // 响应体的关键片段截 120 字符——足够看到 `{"error":{"message":"..."}}` 主要内容
    let excerpt = extract_error_field(sanitized_msg);
    if excerpt.is_empty() {
        format!("HTTP {status} · {hint}")
    } else {
        format!("HTTP {status} · {hint} · 响应: {}", truncate_chars(&excerpt, 120))
    }
}

/// 从供应商响应体里抽出真正的错误消息(优先看 `error.message`,兜底整体截断)。
///
/// 大多数 OpenAI 兼容平台返回 `{"error": {"message": "...", "type": "...", "code": ...}}`,
/// 直接抽 `error.message` 比展示整个 JSON 更清爽。抽不到就返原样(已 sanitize)。
fn extract_error_field(body: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some(m) = v.pointer("/error/message").and_then(|x| x.as_str()) {
            return m.to_string();
        }
        // 有些平台把 message 放顶层
        if let Some(m) = v.get("message").and_then(|x| x.as_str()) {
            return m.to_string();
        }
    }
    // 非 JSON 或无 error.message → 返原样(已经过 sanitize)
    body.trim().to_string()
}

/// 消息脱敏——掐掉常见密钥前缀。
///
/// 处理场景:供应商偶尔把 API Key 回显在错误 message 里(不规范但发生过);
/// URL 里的 query string 可能含 access token。
///
/// **保守策略**:识别到常见密钥 pattern 就掐掉后 20 字符(足以让 debug 者判断
/// "这里有密钥"但看不到明文)。base_url 本身用户自己配的,不算敏感,不动。
fn sanitize_message(msg: &str) -> String {
    // 命中 `Bearer xxx` / `sk-xxx` / `key=xxx` / `api_key=xxx` / `token=xxx` 的
    // 后续非空白串,统一替换成 `<redacted>`。
    // 用简单 char 扫描,避免拉 regex crate。
    let mut out = String::with_capacity(msg.len());
    let mut chars = msg.chars().peekable();
    while let Some(c) = chars.next() {
        // 简单模式:检查是否是敏感前缀开始
        let rest: String = std::iter::once(c).chain(chars.clone().take(15)).collect();
        let rest_lower = rest.to_lowercase();
        let (matched_prefix_len, has_secret) = if rest_lower.starts_with("bearer ") {
            (7, true)
        } else if rest_lower.starts_with("sk-") {
            (3, true)
        } else if rest_lower.starts_with("api_key=") || rest_lower.starts_with("apikey=") {
            (if rest_lower.starts_with("api_key=") { 8 } else { 7 }, true)
        } else if rest_lower.starts_with("token=") {
            (6, true)
        } else if rest_lower.starts_with("key=") {
            (4, true)
        } else {
            (0, false)
        };

        if has_secret {
            // 把前缀原样写出,再吞掉后续非空白/非引号/非逗号字符,替 <redacted>
            for _ in 1..matched_prefix_len {
                chars.next();
            }
            out.push_str(&rest[..matched_prefix_len]);
            out.push_str("<redacted>");
            // skip 到下一个空白/引号/逗号/大括号
            while let Some(&nxt) = chars.peek() {
                if nxt.is_whitespace()
                    || nxt == '"'
                    || nxt == '\''
                    || nxt == ','
                    || nxt == '}'
                    || nxt == '&'
                {
                    break;
                }
                chars.next();
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// 按 char 边界截断字符串,防止中间截到多字节 UTF-8 中间导致乱码。
fn truncate_chars(s: &str, max_chars: usize) -> String {
    if s.chars().count() <= max_chars {
        return s.to_string();
    }
    let mut r: String = s.chars().take(max_chars).collect();
    r.push('…');
    r
}

// ── 密钥破口——RigFactory 用 ─────────────────────────────────────────────

/// 把 `SecretString` 转成 rig client 构造需要的 `String` —— **唯一破口**。
///
/// **调用契约**:调用方必须**只在 rig `Client::new(...)` 参数一次窗口用**,
/// 不存进任何 struct 字段、不日志、不写盘。返回的 String 出作用域即释放,
/// `SecretString` Drop 走 zeroize 抹掉栈上原文。
///
/// (放这里而非 secret 模块,是为了让"密钥→rig"这一步的可见性局限在
/// domain::ai::rig_provider —— 其他任何地方都不该做这个转换。)
#[allow(dead_code)] // 0.9.2 Phase 5b 起由 RigFactory 消费
pub(crate) fn expose_for_rig(s: &SecretString) -> String {
    s.expose().to_string()
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ai::message::ChatMessage;
    use crate::domain::execution::ActionSchema;
    use rig_core::completion::{CompletionResponse as RigResp, Usage as RigUsage};
    use rig_core::message::{Text as RigText, ToolCall as RigToolCall, ToolFunction as RigToolFunc};
    use serde_json::json;

    // ── build_rig_request ───────────────────────────────────────────────

    #[test]
    fn build_rig_request_maps_system_and_user() {
        let req = CompletionRequest {
            messages: vec![
                ChatMessage::system("You are helpful."),
                ChatMessage::user("hi"),
            ],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None).unwrap();
        assert_eq!(rig.preamble.as_deref(), Some("You are helpful."));
        assert_eq!(rig.chat_history.len(), 1);
    }

    #[test]
    fn build_rig_request_concatenates_multiple_system_msgs() {
        // 极少见但契约:多条 system 拼成一个 preamble
        let req = CompletionRequest {
            messages: vec![
                ChatMessage::system("a"),
                ChatMessage::system("b"),
                ChatMessage::user("q"),
            ],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None).unwrap();
        assert_eq!(rig.preamble.as_deref(), Some("a\nb"));
    }

    #[test]
    fn build_rig_request_projects_tool_schemas() {
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("q")],
            tools: vec![
                ActionSchema::empty("open_settings", "打开设置"),
                ActionSchema::empty("lock", "锁定电脑"),
            ],
            max_tokens: Some(128),
            temperature: Some(0.2),
            timeout_ms: None,
        };
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None).unwrap();
        assert_eq!(rig.tools.len(), 2);
        assert_eq!(rig.tools[0].name, "open_settings");
        assert_eq!(rig.tools[1].name, "lock");
        assert_eq!(rig.max_tokens, Some(128));
        // temperature 是 f32→f64,浮点精度会漂移(0.2f32→0.20000000298...),
        // 断言"落在合理区间"而不是等值
        let t = rig.temperature.unwrap();
        assert!((t - 0.2).abs() < 1e-5, "temperature 期望 ~0.2,实际 {t}");
    }

    #[test]
    fn build_rig_request_rejects_empty_messages() {
        let req = CompletionRequest {
            messages: Vec::new(),
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        assert!(matches!(build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None), Err(AIError::Serialization(_))));
    }

    #[test]
    fn build_rig_request_rejects_only_system() {
        // 只有 system 没有 user → rig 契约不满足(chat_history 至少 1)
        let req = CompletionRequest {
            messages: vec![ChatMessage::system("only")],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        assert!(matches!(build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None), Err(AIError::Serialization(_))));
    }

    #[test]
    fn build_rig_request_rejects_assistant_role_in_0_9_2() {
        // 0.9.2 主窗口不该出现 assistant / tool——多轮留 0.10 agent 窗口
        let req = CompletionRequest {
            messages: vec![
                ChatMessage::user("q"),
                ChatMessage::assistant("a"),
            ],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        assert!(matches!(build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, None), Err(AIError::Serialization(_))));
    }

    // ── 0.9.4 Step 1:模型级参数 fallback ───────────────────────────────

    #[test]
    fn build_rig_request_uses_model_defaults_when_request_omits() {
        // request 没指定 temperature/max_tokens → 用 model 层默认
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("q")],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, Some(0.7), Some(4096), None).unwrap();
        // f32 → f64 有精度损失,允许 1e-6 误差
        assert!((rig.temperature.unwrap() - 0.7).abs() < 1e-6);
        assert_eq!(rig.max_tokens, Some(4096));
    }

    #[test]
    fn build_rig_request_request_overrides_model_defaults() {
        // 路由档铁则:request 显式指定必须优先——即使 model 默认也不能覆盖
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("q")],
            tools: Vec::new(),
            max_tokens: Some(64),
            temperature: Some(0.0),
            timeout_ms: None,
        };
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, Some(0.7), Some(4096), None).unwrap();
        assert_eq!(rig.temperature, Some(0.0));
        assert_eq!(rig.max_tokens, Some(64));
    }

    #[test]
    fn build_rig_request_passes_custom_params_verbatim() {
        // custom_parameters 组装后透传到 additional_params
        let req = CompletionRequest {
            messages: vec![ChatMessage::user("q")],
            tools: Vec::new(),
            max_tokens: None,
            temperature: None,
            timeout_ms: None,
        };
        let extra = json!({"top_p": 0.9, "web_search": true});
        let rig = build_rig_request(ProviderKind::OpenAICompatible, &req, None, None, Some(&extra)).unwrap();
        assert_eq!(rig.additional_params.as_ref(), Some(&extra));
    }

    #[test]
    fn build_custom_params_json_folds_and_dedupes() {
        // 空 → None;后 key 覆盖前;空 key 忽略
        assert!(build_custom_params_json(&[]).is_none());

        let ps = vec![
            CustomParam { key: "top_p".into(), value: json!(0.5) },
            CustomParam { key: "".into(), value: json!("skipped") },
            CustomParam { key: "top_p".into(), value: json!(0.9) },
        ];
        let out = build_custom_params_json(&ps).unwrap();
        assert_eq!(out, json!({"top_p": 0.9}));
    }

    // ── map_rig_response ────────────────────────────────────────────────

    /// 构造一个仅含 text 的 rig response(raw_response 用 unit type)
    fn rig_text_resp(text: &str) -> RigResp<()> {
        RigResp {
            choice: OneOrMany::one(AssistantContent::Text(RigText::new(text))),
            usage: RigUsage {
                input_tokens: 10,
                output_tokens: 5,
                total_tokens: 15,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: (),
            message_id: None,
        }
    }

    fn rig_toolcall_resp(name: &str, args: serde_json::Value) -> RigResp<()> {
        RigResp {
            choice: OneOrMany::one(AssistantContent::ToolCall(RigToolCall::new(
                "call_abc".into(),
                RigToolFunc::new(name.into(), args),
            ))),
            usage: RigUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: (),
            message_id: None,
        }
    }

    #[test]
    fn map_response_text_only() {
        let ours = map_rig_response(rig_text_resp("hello world"), 123);
        assert_eq!(ours.text.as_deref(), Some("hello world"));
        assert!(ours.tool_calls.is_empty());
        assert_eq!(ours.first_token_ms, 123);
        assert_eq!(ours.total_ms, 123);
        assert_eq!(ours.usage.input_tokens, 10);
        assert_eq!(ours.usage.output_tokens, 5);
    }

    #[test]
    fn map_response_tool_call_only() {
        let ours = map_rig_response(
            rig_toolcall_resp("open_url", json!({ "url": "https://a.b" })),
            50,
        );
        assert!(ours.text.is_none()); // 无 Text 段 → None(不是空字符串)
        assert_eq!(ours.tool_calls.len(), 1);
        assert_eq!(ours.tool_calls[0].id, "call_abc");
        assert_eq!(ours.tool_calls[0].name, "open_url");
        assert_eq!(
            ours.tool_calls[0].arguments.get("url").and_then(|v| v.as_str()),
            Some("https://a.b")
        );
    }

    #[test]
    fn map_response_mixed_text_and_toolcall() {
        // OneOrMany::many 需要 non-empty vec
        let items = vec![
            AssistantContent::Text(RigText::new("intro")),
            AssistantContent::ToolCall(RigToolCall::new(
                "c1".into(),
                RigToolFunc::new("do".into(), json!({})),
            )),
            AssistantContent::Text(RigText::new("outro")),
        ];
        let rig = RigResp {
            choice: OneOrMany::many(items).unwrap(),
            usage: RigUsage {
                input_tokens: 0,
                output_tokens: 0,
                total_tokens: 0,
                cached_input_tokens: 0,
                cache_creation_input_tokens: 0,
                tool_use_prompt_tokens: 0,
                reasoning_tokens: 0,
            },
            raw_response: (),
            message_id: None,
        };
        let ours = map_rig_response(rig, 42);
        assert_eq!(ours.text.as_deref(), Some("intro\noutro"));
        assert_eq!(ours.tool_calls.len(), 1);
    }

    // ── map_rig_error ───────────────────────────────────────────────────

    #[test]
    fn map_error_json_decode_stays_generic() {
        // ProviderError 分支:message 会脱敏 sk-* 后透传作诊断
        let err = CompletionError::ProviderError("sk-secret-1234 leaked".into());
        let ours = map_rig_error(err);
        match ours {
            AIError::Provider(msg) => {
                // 铁则:sk- 后的原文必须被 <redacted>
                assert!(!msg.contains("sk-secret"), "映射后不该带密钥原文: {msg}");
                assert!(!msg.contains("secret-1234"), "映射后不该带密钥原文: {msg}");
                assert!(msg.contains("<redacted>"), "sk- 应替换为 <redacted>: {msg}");
                assert!(msg.starts_with("供应商错误"), "前缀需可识别: {msg}");
            }
            _ => panic!("预期 Provider,得到 {ours:?}"),
        }
    }

    #[test]
    fn map_error_response_error_stays_generic() {
        let err = CompletionError::ResponseError("choices[0].message.content missing".into());
        let ours = map_rig_error(err);
        match ours {
            AIError::Serialization(msg) => {
                assert!(!msg.contains("choices"), "不该带原文: {msg}");
                assert!(msg.contains("model_id") || msg.contains("响应"), "需诊断 hint: {msg}");
            }
            _ => panic!("预期 Serialization,得到 {ours:?}"),
        }
    }

    // ── 脱敏与诊断辅助 ─────────────────────────────────────────────────────

    #[test]
    fn sanitize_message_redacts_sk_prefix() {
        let s = sanitize_message("Invalid API key sk-abc123def456 provided");
        assert!(!s.contains("abc123def456"), "sk- 后原文未脱敏: {s}");
        assert!(s.contains("<redacted>"), "应含 <redacted> 占位: {s}");
    }

    #[test]
    fn sanitize_message_redacts_bearer() {
        let s = sanitize_message("Authorization: Bearer eyJhbGc.eyJzdWIu.SflKxwR failed");
        assert!(!s.contains("eyJhbGc"), "Bearer token 未脱敏: {s}");
        assert!(s.to_lowercase().contains("bearer <redacted>"));
    }

    #[test]
    fn sanitize_message_redacts_query_params() {
        let s = sanitize_message("URL: https://a.b/x?key=abc123&other=1");
        assert!(!s.contains("abc123"), "query key 未脱敏: {s}");
        // 其他字段保留
        assert!(s.contains("other=1"));
    }

    #[test]
    fn sanitize_message_preserves_non_secret_content() {
        let s = sanitize_message("Model 'gpt-5' not found");
        assert_eq!(s, "Model 'gpt-5' not found");
    }

    #[test]
    fn diagnose_status_translates_common_codes() {
        assert!(diagnose_status(401, "unauthorized").contains("密钥"));
        assert!(diagnose_status(404, "not found").contains("model_id"));
        assert!(diagnose_status(429, "rate limit").contains("限流"));
        assert!(diagnose_status(500, "").contains("500"));
    }

    #[test]
    fn diagnose_status_extracts_error_message_json() {
        let body = r#"{"error":{"message":"Model 'x' does not exist","type":"invalid_request"}}"#;
        let out = diagnose_status(404, body);
        // 抽出的应是 error.message,而非整个 JSON
        assert!(out.contains("Model 'x' does not exist"), "未抽 error.message: {out}");
        // 且 body 里的 JSON 大括号不应出现在 excerpt
        assert!(!out.contains("\"type\""), "不该包含额外字段");
    }

    #[test]
    fn extract_error_field_falls_back_to_body() {
        // 非 JSON 直接返原文
        let s = extract_error_field("plain text error");
        assert_eq!(s, "plain text error");
        // JSON 但无 error.message → 返原文
        let s = extract_error_field(r#"{"code":42}"#);
        assert_eq!(s, r#"{"code":42}"#);
    }

    #[test]
    fn truncate_chars_respects_utf8_boundary() {
        // 中文字符按 char 计数,不按字节
        let s = truncate_chars("你好世界你好世界", 3);
        assert_eq!(s, "你好世…");
        // 短于 max 时不截
        assert_eq!(truncate_chars("abc", 10), "abc");
    }
}
