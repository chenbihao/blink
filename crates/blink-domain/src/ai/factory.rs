//! Provider 工厂——按 `ProviderKind` 构造对应 rig client 并封装成 `AIProvider`。
//!
//! ## 阶段与状态(0.9.2 Phase 5b)
//!
//! **RigFactory 已上线**——`default_factory()` 返 `RigFactory`,真跑 rig completion。
//!
//! `NoopFactory` 保留:
//! - 作为 registry baseline 单测的固定入参(证明"我没接 rig 时也能证明 registry 骨架正确")
//! - 为 0.11 增本地模型 factory 时提供参考实现模板
//!
//! ## §6.4 兜底铁则
//!
//! AI 配置错误绝不破坏主链路。RigFactory::build 失败链:
//! - `secret::load_secret` 缺 → `AIError::SecretMissing`(0.17.8: 区分"档位悬空"与"密钥丢失")
//! - `openai_compat` 缺 base_url → `AIError::Provider`(用户配错,需感知)
//! - rig `Client::builder().build()` 失败 → `AIError::Provider("client build failed")`
//!
//! 所有失败都被 registry.reload 的 skip + warn 消化——单个 provider 挂不影响其他。
//!
//! ## 类型收窄
//!
//! 每个 arm 构造出不同具体类型的 `RigProvider<M>`(泛型 M 由 ProviderKind 敲定),
//! 全部擦除到 `Arc<dyn AIProvider>` 回给 registry。上层拿不到 rig 类型——§2.6。

use std::sync::Arc;

use rig_core::driver::DynModel;
use rig_core::operation::Completion;

use crate::ai::provider::{AIError, AIProvider};
use crate::ai::registry::ProviderFactory;
use crate::ai::rig_provider::{RigProvider, expose_for_rig};
use crate::config::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use blink_infra::platform::secret;

/// **占位 factory** —— 所有 `build` 请求返 `NotConfigured`。
///
/// **保留原因**:registry baseline 单测入参、未来本地模型 factory 模板。
/// 生产用 `RigFactory`。
pub struct NoopFactory;

impl ProviderFactory for NoopFactory {
    fn build(
        &self,
        entry: &ProviderEntry,
        model: &ModelEntry,
    ) -> Result<Arc<dyn AIProvider>, AIError> {
        tracing::warn!(
            target: blink_infra::utils::perf::ai_slo::TARGET,
            "NoopFactory 拒绝构造 {} · {}——仅用作单测 baseline",
            entry.display_name,
            model.id,
        );
        Err(AIError::NotConfigured)
    }
}

/// **生产 factory** —— 接 rig-core 各 provider client,真跑 LLM。
///
/// 每次 `build` 都:
/// 1. 从 CM 读密钥(缺 → SecretMissing)
/// 2. 按 `entry.kind` 构造 rig `Client`(base_url 可覆盖)
/// 3. `client.completion(&model.id)` 得到 `Model<W>`，擦除为 `DynModel<Completion>`
/// 4. 包进 `RigProvider` → 擦除 `Arc<dyn AIProvider>`
///
/// **密钥生命周期**:`load_secret` → `expose_for_rig(&s)` **只一次** →
/// 传给 rig `.api_key(k)`。返回后 `SecretString` Drop 走 zeroize。
pub struct RigFactory;

impl ProviderFactory for RigFactory {
    fn build(
        &self,
        entry: &ProviderEntry,
        model: &ModelEntry,
    ) -> Result<Arc<dyn AIProvider>, AIError> {
        // 1. 读密钥——本地 provider (ollama) 不需要密钥,跳过。
        //    云端 provider 缺密钥 = SecretMissing（区别于 NotConfigured 档位悬空）。
        let key_str: String = if entry.kind.requires_secret() {
            let key = secret::load_secret(&secret::SecretRef::ai(&entry.id)).map_err(|e| {
                tracing::debug!(
                    target: blink_infra::utils::perf::ai_slo::TARGET,
                    "AI factory: {} 密钥未配置 ({e})",
                    entry.display_name,
                );
                AIError::SecretMissing(entry.display_name.clone())
            })?;
            expose_for_rig(&key)
        } else {
            // 本地 provider 无需密钥——ollama 等
            tracing::debug!(
                target: blink_infra::utils::perf::ai_slo::TARGET,
                "AI factory: {} 本地 provider,跳过密钥加载",
                entry.display_name,
            );
            String::new() // 空字符串,rig ollama client 不使用
        };
        // key 在此作用域内保留,rig builder 需要 &str;函数返回时 key 出栈 → zeroize

        // 2. 按协议分派构造 —— 每 arm 独立返回 Arc<dyn AIProvider>
        //    (0.9.2 第二步:3 类协议,老 kind 已通过 serde alias 迁移到 OpenAICompatible)
        //    0.9.4 Step 1:model 参数默认值(temperature/max_tokens/custom_parameters)
        //    随 model 引用一路传到 RigProvider::new,由 complete() 做 fallback。
        let provider: Arc<dyn AIProvider> = match entry.kind {
            ProviderKind::OpenAICompatible => {
                build_openai_compatible(&key_str, entry.base_url.as_deref(), model)?
            }
            ProviderKind::AnthropicMessages => {
                build_anthropic(&key_str, entry.base_url.as_deref(), model)?
            }
            ProviderKind::GeminiGenerateContent => {
                build_gemini(&key_str, entry.base_url.as_deref(), model)?
            }
            ProviderKind::OllamaHttp => {
                // 0.12 §2.3: ollama 本地推理 provider
                build_ollama(entry.base_url.as_deref(), model)?
            }
        };

        tracing::info!(
            target: blink_infra::utils::perf::ai_slo::TARGET,
            "AI factory: 构造 {} · kind={:?} · model={}",
            entry.display_name,
            entry.kind,
            model.id,
        );
        Ok(provider)
    }
}

// ── 各 Provider 构造 ─────────────────────────────────────────────────────
/// 构造 OpenAI Compatible 协议的 rig client（0.12.1 抽出，AgentProvider 复用拿裸 model）。
///
/// **护栏**：base_url 空一律拒绝--rig 默认落 `api.openai.com`，用户拿着第三方 Key 打去
/// OpenAI 官方必 401 且极难自诊断（前端已有校验；这里是双重保险，防手动编辑 db 绕过）。
pub(crate) fn build_openai_client(
    key: &str,
    base_url: Option<&str>,
) -> Result<rig_core::providers::openai::OpenAI, AIError> {
    use rig_core::providers::openai;
    let url = base_url.filter(|s| !s.is_empty()).ok_or_else(|| {
        AIError::Provider(
            "OpenAI Compatible 协议必须配 base_url(如 https://api.openai.com/v1)".into(),
        )
    })?;
    // 0.21.16/0.43: 注入 LoggingHttpClient——trace 级别打印真实请求/响应体（wire JSON），
    // 排查 provider 兼容问题（如本地 qwen 思考块）。平时零开销透传。
    // 0.43: OpenAIConfig + connect(http)——client 不再携带 HTTP 泛型参数。
    // 0.27.1: 显式 Route::Chat——OpenAIConfig::new 默认 OPENAI dialect（completion_route
    // = Responses），第三方 OpenAICompatible 网关普遍无 /responses 端点会 404。本产品
    // 协议承诺是 /chat/completions，此处显式覆盖 dialect 默认（wire 测试见 mod wire_tests）。
    Ok(openai::OpenAIConfig::new(key)
        .with_base_url(url)
        .with_route(openai::Route::Chat)
        .connect(blink_infra::utils::http_log::LoggingHttpClient::default()))
}

/// 构造 Anthropic Messages 协议的 rig client（0.12.1 抽出）。
pub(crate) fn build_anthropic_client(
    key: &str,
    base_url: Option<&str>,
) -> Result<rig_core::providers::anthropic::Anthropic, AIError> {
    use rig_core::providers::anthropic;
    let mut config = anthropic::AnthropicConfig::new(key);
    if let Some(url) = base_url.filter(|s| !s.is_empty()) {
        config = config.with_base_url(url);
    }
    // 0.21.16/0.43: 注入 LoggingHttpClient——与其他协议一致的请求/响应体日志开关。
    Ok(config.connect(blink_infra::utils::http_log::LoggingHttpClient::default()))
}

/// 构造 Google Gemini 协议的 rig client（0.12.1 抽出）。
/// rig 0.42+ gemini 不支持 base_url（端点固定 googleapis.com），用户填的忽略。
pub(crate) fn build_gemini_client(
    key: &str,
    _base_url: Option<&str>,
) -> Result<rig_core::providers::gemini::Gemini, AIError> {
    use rig_core::providers::gemini;
    // 0.21.16/0.43: 注入 LoggingHttpClient——与其他协议一致的请求/响应体日志开关。
    Ok(gemini::GeminiConfig::new(key)
        .connect(blink_infra::utils::http_log::LoggingHttpClient::default()))
}

/// 构造 ollama 本地推理的 rig client（0.12.1 抽出）。
/// 无需 API Key（本地服务），base_url 默认 localhost:11434。
pub(crate) fn build_ollama_client(
    base_url: Option<&str>,
) -> Result<rig_core::providers::ollama::Ollama, AIError> {
    use rig_core::providers::ollama;
    let mut config = ollama::OllamaConfig::new();
    if let Some(url) = base_url.filter(|s| !s.is_empty()) {
        config = config.with_base_url(url);
    }
    // 0.21.16/0.43: 注入 LoggingHttpClient——与其他协议一致的请求/响应体日志开关。
    Ok(config.connect(blink_infra::utils::http_log::LoggingHttpClient::default()))
}

/// OpenAI Chat Completions 协议——**通用兼容层**(0.9.2 第二步)。
///
/// **覆盖范围**:OpenAI 官方 / DeepSeek / 硅基流动 / Moonshot / Groq / OpenRouter /
/// xAI / 自建代理——所有走 `/v1/chat/completions` 的端点。
///
/// **base_url 政策**:
/// - `Some(url)` → 用用户填的 url(前端预设下拉可一键填 preset)
/// - `None` → 走 rig `CompletionsClient` 默认 base(`api.openai.com`);多数用户会
///   通过前端 preset 填,None 极少见,但保持不 panic 兼容手动 JSON 编辑场景
///
/// **为什么不再区分 `deepseek::Client` / `openai::Client`**:
/// rig 里 `deepseek::Client` 内部就是 Chat Completions 协议 + 预置 base_url,
/// 我们把预置放到前端 preset 下拉,后端只留一条协议路径,更干净。
fn build_openai_compatible(
    key: &str,
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    // **护栏**:base_url 空一律拒绝构造——rig 默认落到 `api.openai.com`,用户拿着
    // 第三方 Key 打去 OpenAI 官方必 401 且极难自诊断(前端已有校验;这里是双重保险,
    // 防止老配置迁移 / 手动编辑 db 绕过前端)。
    let client = build_openai_client(key, base_url)?;
    let rig_model: DynModel<Completion> = client.completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OpenAICompatible,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        base_url,
        model.reasoning_effort.as_deref(),
        model.thinking_style,
    )))
}

/// Anthropic Messages 协议——`/v1/messages`,仅 Claude 官方。
fn build_anthropic(
    key: &str,
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    let client = build_anthropic_client(key, base_url)?;
    let rig_model: DynModel<Completion> = client.completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::AnthropicMessages,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        base_url,
        model.reasoning_effort.as_deref(),
        model.thinking_style,
    )))
}

/// Google Gemini GenerateContent 协议——`/v1beta/models/*:generateContent`。
///
/// rig 0.42 里 `gemini::Client::builder()` 支持 `api_key` 但不显式支持 `base_url`
/// (Gemini 端点固定在 googleapis.com);如果用户填了 base_url 我们也不报错,
/// 仅忽略——避免"填了没用又不知道"的困惑,统一走 rig 默认。
fn build_gemini(
    key: &str,
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    let client = build_gemini_client(key, base_url)?;
    let rig_model: DynModel<Completion> = client.completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::GeminiGenerateContent,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        base_url,
        model.reasoning_effort.as_deref(),
        model.thinking_style,
    )))
}

/// ollama HTTP API——本地推理,走 rig::providers::ollama（0.12 §2.3）。
///
/// **无需 API Key**——ollama 是本地服务,`requires_secret()` 已返回 false。
/// base_url 默认 `http://localhost:11434`,用户可改（如远程 ollama 实例）。
///
/// 同时支持 chat 模型和 embedding 模型——factory 根据 `ModelEntry.capabilities`
/// 决定构造哪种。当前只构造 completion model（embedding 留给 0.13 RAG）。
fn build_ollama(
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    // ollama 无需认证——OllamaApiKey::default() = None,不发送 Authorization header。
    // 但 builder 类型系统要求 Key: ApiKey,必须显式设置。
    let client = build_ollama_client(base_url)?;

    // 0.12 §2.7: 校验模型能力--纯 embedding 模型不应构造为 completion model。
    // 0.12 不构造 embedding model（留给 0.13 RAG），此处仅 warn 不 block。
    if !model
        .capabilities
        .iter()
        .any(|c| matches!(c, crate::config::ai_config::ModelCapability::Chat))
    {
        tracing::warn!(
            model_id = %model.id,
            capabilities = ?model.capabilities,
            "ollama 模型未标记 Chat 能力，构造为 completion model 可能运行时失败（embedding 模型请在 0.13 RAG 消费）"
        );
    }

    let rig_model: DynModel<Completion> = client.completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OllamaHttp,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        base_url,
        model.reasoning_effort.as_deref(),
        model.thinking_style,
    )))
}

/// 默认 factory 构造——**0.9.2 Phase 5b 起返回 `RigFactory`**。
///
/// **调用位置**:`main.rs::setup`。挂进 `AIProviderRegistry`。
#[allow(dead_code)] // 由 main.rs 消费
pub fn default_factory() -> Arc<dyn ProviderFactory> {
    Arc::new(RigFactory)
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ai_config::{ModelCapability, ProviderKind};

    fn sample_entry(kind: ProviderKind, base_url: Option<String>) -> ProviderEntry {
        ProviderEntry {
            id: "test-provider-uuid".into(),
            display_name: "Test".into(),
            kind,
            base_url,
            secret_ref: "blink/test-provider-uuid/key".into(),
            models: Vec::new(),
            enabled: true,
            created_at: 0,
        }
    }

    pub(super) fn sample_model() -> ModelEntry {
        ModelEntry {
            id: "gpt-4o-mini".into(),
            display_name: "M1".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            custom_parameters: Vec::new(),
            reasoning_effort: None,
            thinking_style: None,
            capabilities: vec![ModelCapability::Chat],
        }
    }

    #[test]
    fn noop_factory_always_returns_not_configured() {
        let f = NoopFactory;
        let result = f.build(
            &sample_entry(ProviderKind::OpenAICompatible, None),
            &sample_model(),
        );
        assert!(matches!(result, Err(AIError::NotConfigured)));
    }

    #[test]
    fn default_factory_is_rig_factory_in_phase_5b() {
        // 0.9.2 第二步:default 是 RigFactory。
        // 无密钥场景 → load_secret 缺 → SecretMissing(0.17.8: 区分"档位悬空"与"密钥丢失")
        let f = default_factory();
        let entry = sample_entry(ProviderKind::OpenAICompatible, None);
        let result = f.build(&entry, &sample_model());
        assert!(
            matches!(result, Err(AIError::SecretMissing(_))),
            "无密钥应返 SecretMissing 而非其他错误(is_ok={})",
            result.is_ok()
        );
    }

    #[test]
    fn all_cloud_kinds_have_factory_arms() {
        // 云端三类协议都能触达 factory dispatch;无密钥场景一律 SecretMissing 早于协议构造。
        // 这是"分派完整性"测试:防止未来新增 kind 时忘了 factory arm。
        // 0.12: OllamaHttp 不需要密钥,不走 SecretMissing 路径,单独测试。
        let f = RigFactory;
        for kind in [
            ProviderKind::OpenAICompatible,
            ProviderKind::AnthropicMessages,
            ProviderKind::GeminiGenerateContent,
        ] {
            let entry = sample_entry(kind, None);
            let result = f.build(&entry, &sample_model());
            assert!(
                matches!(result, Err(AIError::SecretMissing(_))),
                "kind={kind:?} 无密钥应返 SecretMissing"
            );
        }
    }

    #[test]
    fn ollama_skips_secret_loading() {
        // 0.12 §2.3: OllamaHttp 是本地 provider,不需要密钥——build 应跳过 load_secret,
        // 直接进入 match arm 并成功构造 RigProvider（client 构造不连接服务器,只创建 HTTP 客户端）。
        let f = RigFactory;
        let entry = sample_entry(ProviderKind::OllamaHttp, None);
        let result = f.build(&entry, &sample_model());
        assert!(
            result.is_ok(),
            "OllamaHttp 应成功构造 provider(无需密钥,client 构造不连接服务器)"
        );
    }
}

// ── 0.27.1 wire 测试：本地 TcpListener 捕获真实 HTTP method/path/body ─────────
//
// 静态结论（rig-core 0.43.0 源码核验）：`OpenAIConfig::new` 默认 OPENAI dialect，
// `Quirks::completion_route = Route::Responses`——factory 不显式覆盖路由时，
// OpenAICompatible 请求会打 `/responses` 而非 `/chat/completions`。本组测试钉住
// `build_openai_client` 显式 `Route::Chat` 的实际 wire 行为（不走 mock client，
// 真实走 rig 编码 + LoggingHttpClient + reqwest 传输，仅端点换成本地捕获桩）。
#[cfg(test)]
mod wire_tests {
    use super::*;
    use crate::ai::message::{ChatMessage, CompletionRequest};
    use crate::ai::provider::{AIProvider, StreamChunk};
    use crate::schema::ToolSchema;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// 捕获到的一次请求。
    #[derive(Debug, Clone)]
    struct CapturedRequest {
        method: String,
        path: String,
        body: String,
    }

    /// 单用途 HTTP 捕获桩：同步服务 `responses.len()` 个请求后自行退出。
    /// 每连接读请求行 + headers + Content-Length body，回写 `responses[i]`（完整响应字节）。
    fn spawn_capture_server(responses: Vec<Vec<u8>>) -> (SocketAddr, Arc<Mutex<Vec<CapturedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else { break };
                let mut reader = BufReader::new(stream);
                let Some(req) = read_http_request(&mut reader) else { break };
                sink.lock().unwrap().push(req);
                let _ = reader.get_mut().write_all(&response);
                let _ = reader.get_mut().flush();
            }
        });
        (addr, captured)
    }

    /// 读单个 HTTP/1.1 请求（请求行 + headers + Content-Length body）。
    fn read_http_request(reader: &mut BufReader<TcpStream>) -> Option<CapturedRequest> {
        let mut request_line = String::new();
        reader.read_line(&mut request_line).ok()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();
        let mut content_length = 0usize;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line).ok()?;
            if line == "\r\n" || line.is_empty() {
                break;
            }
            if let Some((name, value)) = line.split_once(':')
                && name.trim().eq_ignore_ascii_case("content-length")
            {
                content_length = value.trim().parse().unwrap_or(0);
            }
        }
        let mut body = vec![0u8; content_length];
        if content_length > 0 {
            reader.read_exact(&mut body).expect("读请求体");
        }
        Some(CapturedRequest {
            method,
            path,
            body: String::from_utf8_lossy(&body).into_owned(),
        })
    }

    /// 标准非流式 chat completion 响应（200 + JSON + Connection: close）。
    fn chat_completion_response(content: &str) -> Vec<u8> {
        let body = format!(
            r#"{{"id":"wire-1","object":"chat.completion","created":1,"model":"wire-model","choices":[{{"index":0,"message":{{"role":"assistant","content":{content}}},"finish_reason":"stop"}}],"usage":{{"prompt_tokens":5,"completion_tokens":4,"total_tokens":9}}}}"#
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    /// 标准 SSE 流式响应（3 个内容 chunk + [DONE]）。
    fn sse_stream_response() -> Vec<u8> {
        // frame() 只产 JSON 载荷，"data: " 前缀由拼接层统一加。
        let frame = |delta: &str, finish: &str| {
            format!(
                r#"{{"id":"wire-s","object":"chat.completion.chunk","created":1,"model":"wire-model","choices":[{{"index":0,"delta":{delta},"finish_reason":{finish}}}]}}"#
            )
        };
        let body = format!(
            "data: {}\n\ndata: {}\n\ndata: {}\n\ndata: [DONE]\n\n",
            frame(r#"{"role":"assistant","content":"Hel"}"#, "null"),
            frame(r#"{"content":"lo"}"#, "null"),
            frame("{}", "\"stop\""),
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    fn sample_wire_request() -> CompletionRequest {
        CompletionRequest {
            messages: vec![
                ChatMessage::system("你是测试助手"),
                ChatMessage::user("say hi"),
            ],
            tools: Vec::new(),
            max_tokens: Some(64),
            temperature: Some(0.0),
            timeout_ms: Some(5_000),
        }
    }

    fn build_provider(base_url: &str) -> Arc<dyn AIProvider> {
        let model = super::tests::sample_model();
        build_openai_compatible("sk-wire-test", Some(base_url), &model).expect("构造 wire provider")
    }

    /// 普通生成走 POST /chat/completions（不是 /responses），body 携带 model 与消息。
    #[tokio::test]
    async fn normal_completion_posts_to_chat_completions() {
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"hello from wire\"")]);
        let provider = build_provider(&format!("http://{addr}/v1"));

        let resp = provider.complete(sample_wire_request()).await.expect("complete 应成功");
        assert_eq!(resp.text.as_deref(), Some("hello from wire"));

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].path, "/v1/chat/completions", "必须显式 Chat 路由，禁止落 dialect 默认 Responses");
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).expect("请求体是 JSON");
        assert_eq!(body["model"], "gpt-4o-mini");
        assert!(body["messages"].as_array().is_some_and(|m| !m.is_empty()));
    }

    /// 工具定义随请求体下发（tools 数组真实编码进 wire）。
    #[tokio::test]
    async fn tool_definitions_ride_the_request_body() {
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_provider(&format!("http://{addr}/v1"));

        let mut req = sample_wire_request();
        req.tools = vec![ToolSchema::empty("open_settings", "打开设置")];
        provider.complete(req).await.expect("带工具 complete 应成功");

        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).expect("请求体是 JSON");
        let tools = body["tools"].as_array().expect("tools 应随 wire 下发");
        assert!(tools.iter().any(|t| t["function"]["name"] == "open_settings"));
    }

    /// 流式生成同样走 Chat 路由，SSE 帧被正确解析聚合。
    #[tokio::test]
    async fn streaming_posts_to_chat_completions_and_parses_sse() {
        let (addr, captured) = spawn_capture_server(vec![sse_stream_response()]);
        let provider = build_provider(&format!("http://{addr}/v1"));

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        provider
            .stream(sample_wire_request(), tx)
            .await
            .expect("stream 应成功");
        let mut text = String::new();
        while let Some(chunk) = rx.recv().await {
            match chunk {
                StreamChunk::Text(t) => text.push_str(&t),
                StreamChunk::Done { .. } => break,
            }
        }
        assert_eq!(text, "Hello");

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs[0].path, "/v1/chat/completions", "流式同样必须 Chat 路由");
        // 流式请求 body 声明 stream=true
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).expect("请求体是 JSON");
        assert_eq!(body["stream"], true, "流式请求应带 stream=true");
    }

    /// base_url 拼接不产生重复 /v1：带 /v1、带尾斜杠 /v1/ 均只出现一次 v1 段。
    #[tokio::test]
    async fn base_url_join_never_duplicates_v1() {
        for base in ["/v1", "/v1/"] {
            let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
            let provider = build_provider(&format!("http://{addr}{base}"));
            provider.complete(sample_wire_request()).await.expect("complete 应成功");

            let reqs = captured.lock().unwrap();
            let v1_count = reqs[0].path.matches("v1").count();
            assert_eq!(
                v1_count, 1,
                "base={base} 时路径 {path} 不应出现重复 v1 段",
                path = reqs[0].path
            );
            assert_eq!(reqs[0].path, "/v1/chat/completions", "base={base} 应归一为 /v1/chat/completions");
        }
    }

    /// 无 /v1 前缀的 base（部分网关根路径部署）直接拼 /chat/completions。
    #[tokio::test]
    async fn base_url_without_v1_suffix_appends_chat_path() {
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_provider(&format!("http://{addr}"));
        provider.complete(sample_wire_request()).await.expect("complete 应成功");

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs[0].path, "/chat/completions");
    }
}
