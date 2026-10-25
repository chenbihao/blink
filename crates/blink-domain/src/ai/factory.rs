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

use crate::ai::compat_dialect::{resolve_compat_dialect, ResolvedCompatDialect};
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
                // 0.27.14：方言解析单一真源——Auto 按官方 hostname 精确匹配，
                // 未知网关恒通用；显式覆盖供自建代理。RigProvider 与 AgentProvider
                // 两条链共用同一解析结果。
                let compat = resolve_compat_dialect(
                    entry.kind,
                    entry.compat_dialect,
                    entry.base_url.as_deref(),
                );
                build_openai_compatible(&key_str, entry.base_url.as_deref(), model, compat)?
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
///
/// **0.27.14 方言接线**：`compat` 选择 rig 0.45 `Dialect`——DeepSeek/Moonshot 携带
/// 厂商 rewrite quirks（forced tool choice 处理 / reasoning_content 回传 /
/// required 降级等），Generic 走通用 Chat Completions。**方言只改协议行为，不改端点**：
/// 用户 base_url 恒保留（`with_base_url` 覆盖 dialect 默认 URL）、恒显式 `Route::Chat`
/// （OPENAI dialect 的 completion_route 默认 Responses，DeepSeek/Moonshot 默认 Chat，
/// 统一显式覆盖防依赖 dialect 默认值）、统一 `LoggingHttpClient`（日志/代理 transport）。
pub(crate) fn build_openai_client(
    key: &str,
    base_url: Option<&str>,
    compat: ResolvedCompatDialect,
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
    // 0.27.1: 显式 Route::Chat——第三方 OpenAICompatible 网关普遍无 /responses 端点会
    // 404。本产品协议承诺是 /chat/completions，此处显式覆盖 dialect 默认
    // （wire 测试见 mod wire_tests）。
    // 0.27.14: with_key(dialect, key) 选择厂商方言——quirks 随 dialect 生效；
    // with_base_url 之后覆盖 dialect 默认端点，用户填的地址永远生效。
    let config = match compat {
        ResolvedCompatDialect::Generic => openai::OpenAIConfig::new(key),
        ResolvedCompatDialect::DeepSeek => {
            openai::OpenAIConfig::with_key(&openai::wire::DEEPSEEK, key)
        }
        ResolvedCompatDialect::Moonshot => {
            openai::OpenAIConfig::with_key(&openai::wire::MOONSHOT, key)
        }
    };
    Ok(config
        .with_base_url(url)
        .with_route(openai::Route::Chat)
        .connect(blink_infra::utils::http_log::LoggingHttpClient::from_net()))
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
    Ok(config.connect(blink_infra::utils::http_log::LoggingHttpClient::from_net()))
}

/// 构造 Google Gemini 协议的 rig client（0.12.1 抽出）。
/// 0.27.15：rig 0.45 起 GeminiConfig 支持 with_base_url（0.42 曾固定
/// googleapis.com）——用户填的 base_url 现在生效（自建网关/代理可达）。
pub(crate) fn build_gemini_client(
    key: &str,
    base_url: Option<&str>,
) -> Result<rig_core::providers::gemini::Gemini, AIError> {
    use rig_core::providers::gemini;
    let mut config = gemini::GeminiConfig::new(key);
    if let Some(url) = base_url.filter(|s| !s.is_empty()) {
        config = config.with_base_url(url);
    }
    // 0.21.16/0.43: 注入 LoggingHttpClient——与其他协议一致的请求/响应体日志开关。
    Ok(config.connect(blink_infra::utils::http_log::LoggingHttpClient::from_net()))
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
    Ok(config.connect(blink_infra::utils::http_log::LoggingHttpClient::from_net()))
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
    compat: ResolvedCompatDialect,
) -> Result<Arc<dyn AIProvider>, AIError> {
    // **护栏**:base_url 空一律拒绝构造——rig 默认落到 `api.openai.com`,用户拿着
    // 第三方 Key 打去 OpenAI 官方必 401 且极难自诊断(前端已有校验;这里是双重保险,
    // 防止老配置迁移 / 手动编辑 db 绕过前端)。
    let client = build_openai_client(key, base_url, compat)?;
    let rig_model: DynModel<Completion> = client.completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OpenAICompatible,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        crate::ai::thinking::resolve_thinking_capability(
            ProviderKind::OpenAICompatible,
            compat,
            base_url,
            &model.id,
            model.thinking_style,
        ),
        model.reasoning_effort.as_deref(),
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
        crate::ai::thinking::resolve_thinking_capability(
            ProviderKind::AnthropicMessages,
            ResolvedCompatDialect::Generic,
            base_url,
            &model.id,
            model.thinking_style,
        ),
        model.reasoning_effort.as_deref(),
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
        crate::ai::thinking::resolve_thinking_capability(
            ProviderKind::GeminiGenerateContent,
            ResolvedCompatDialect::Generic,
            base_url,
            &model.id,
            model.thinking_style,
        ),
        model.reasoning_effort.as_deref(),
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

    // 0.45：completion() 走 Ollama 的 OpenAI 兼容 /v1/chat/completions（不发
    // 模型 options）；native_completion() 保持原生 /api/chat——主窗口的
    // additional_params 合并进 options、think 布尔等既有语义依赖原生路径。
    let rig_model: DynModel<Completion> = client.native_completion(&model.id).into();
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OllamaHttp,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
        crate::ai::thinking::resolve_thinking_capability(
            ProviderKind::OllamaHttp,
            ResolvedCompatDialect::Generic,
            base_url,
            &model.id,
            model.thinking_style,
        ),
        model.reasoning_effort.as_deref(),
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
            compat_dialect: Default::default(),
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
pub(super) mod wire_tests {
    use super::*;
    use crate::ai::message::{ChatMessage, CompletionRequest};
    use crate::ai::provider::{AIProvider, StreamChunk};
    use crate::schema::ToolSchema;
    use std::io::{BufRead, BufReader, Read, Write};
    use std::net::{SocketAddr, TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// 捕获到的一次请求。
    #[derive(Debug, Clone)]
    pub(crate) struct CapturedRequest {
        method: String,
        path: String,
        pub(crate) body: String,
    }

    /// 单用途 HTTP 捕获桩：同步服务 `responses.len()` 个请求后自行退出。
    /// 每连接读请求行 + headers + Content-Length body，回写 `responses[i]`（完整响应字节）。
    pub(crate) fn spawn_capture_server(
        responses: Vec<Vec<u8>>,
    ) -> (SocketAddr, Arc<Mutex<Vec<CapturedRequest>>>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind 127.0.0.1:0");
        let addr = listener.local_addr().expect("local_addr");
        let captured: Arc<Mutex<Vec<CapturedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = captured.clone();
        std::thread::spawn(move || {
            for response in responses {
                let Ok((stream, _)) = listener.accept() else {
                    break;
                };
                let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(5)));
                let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(5)));
                let mut reader = BufReader::new(stream);
                let Some(req) = read_http_request(&mut reader) else {
                    break;
                };
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
    pub(crate) fn sse_stream_response() -> Vec<u8> {
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
        build_openai_compatible(
            "sk-wire-test",
            Some(base_url),
            &model,
            ResolvedCompatDialect::Generic,
        )
        .expect("构造 wire provider")
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

    /// Ollama 非流式原生响应（/api/chat 的 message/done 形状）。
    fn ollama_chat_response(content: &str) -> Vec<u8> {
        let body = format!(
            r#"{{"model":"wire-ollama","created_at":"2026-10-25T00:00:00Z","message":{{"role":"assistant","content":{content}}},"done":true,"done_reason":"stop"}}"#
        );
        format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes()
    }

    /// 0.27.13：rig 0.45 起 `completion()` 改走 Ollama 的 OpenAI 兼容
    /// `/v1/chat/completions`，`native_completion()` 才是原生 `/api/chat`。
    /// Blink 刻意保持原生语义（additional_params → options、max_tokens →
    /// num_predict、think 布尔），本测试钉住 native 路由不回退。
    #[tokio::test]
    async fn ollama_native_completion_posts_to_api_chat_with_options() {
        let (addr, captured) = spawn_capture_server(vec![ollama_chat_response("\"hello native\"")]);
        let model = super::tests::sample_model();
        let provider =
            build_ollama(Some(&format!("http://{addr}")), &model).expect("构造 ollama provider");

        let resp = provider
            .complete(sample_wire_request())
            .await
            .expect("ollama complete 应成功");
        assert_eq!(resp.text.as_deref(), Some("hello native"));

        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(
            reqs[0].path, "/api/chat",
            "0.45 必须走 native_completion（/api/chat），不得回退 /v1/chat/completions"
        );
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).expect("请求体是 JSON");
        assert!(
            body["messages"].as_array().is_some_and(|m| !m.is_empty()),
            "原生请求携带 messages"
        );
        assert_eq!(
            body["options"]["num_predict"], 64,
            "max_tokens 经 rig 一等参数映射为 options.num_predict（sample 请求 max_tokens=64）"
        );
    }

    // ── 0.27.14 方言接线 wire 测试 ─────────────────────────────────────────
    //
    // 钉住三件事（§6.4）：
    // 1. 方言只改协议行为不改端点——用户 base_url / 显式 Chat 路由 / 统一 transport 恒保留；
    // 2. rig dialect quirks 经本仓接线真实生效——DeepSeek 思考中 forced tool choice 抑制 +
    //    reasoning_content 回传、Moonshot required 降级 / 指定单工具本地拒绝；
    // 3. 未知网关铁律——Auto + 非官方 hostname 一律通用路径，无 rewrite。

    /// DeepSeek/Moonshot 显式方言 + 三种 base_url 形态：保持 Chat 路由、无重复 /v1、
    /// 请求仍打到用户 base_url（捕获桩），不会被 dialect 默认官方端点改写。
    #[tokio::test]
    async fn dialect_keeps_user_base_url_and_chat_route() {
        use crate::ai::compat_dialect::ResolvedCompatDialect as R;
        for (dialect, label) in [(R::DeepSeek, "deepseek"), (R::Moonshot, "moonshot")] {
            for base in ["/v1", "/v1/", ""] {
                let expect_path = if base.is_empty() {
                    "/chat/completions"
                } else {
                    "/v1/chat/completions"
                };
                let (addr, captured) =
                    spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
                let model = super::tests::sample_model();
                let provider = build_openai_compatible(
                    "sk-wire-test",
                    Some(&format!("http://{addr}{base}")),
                    &model,
                    dialect,
                )
                .expect("构造方言 wire provider");

                provider
                    .complete(sample_wire_request())
                    .await
                    .expect("complete 应成功");

                let reqs = captured.lock().unwrap();
                assert_eq!(
                    reqs.len(),
                    1,
                    "{label} base={base}: 请求必须打到捕获桩——base_url 不得被 dialect 默认端点覆盖"
                );
                assert_eq!(
                    reqs[0].path, expect_path,
                    "{label} base={base}: 方言不得改变 Chat 路由或产生重复 /v1"
                );
            }
        }
    }

    /// rig 原生请求 helper——Blink `AIProvider` 接口不表达 tool_choice，
    /// rewrite 断言经 rig `CompletionRequest` 直驱 wire（构造仍走被测的
    /// `build_openai_client` 接线点）。
    async fn post_via_dialect(
        dialect: crate::ai::compat_dialect::ResolvedCompatDialect,
        request: rig_core::completion::CompletionRequest,
        response: Vec<u8>,
    ) -> (Vec<CapturedRequest>, Result<(), String>) {
        let (addr, captured) = spawn_capture_server(vec![response]);
        let client =
            build_openai_client("sk-wire-test", Some(&format!("http://{addr}/v1")), dialect)
                .expect("构造方言 client");
        let model: DynModel<Completion> = client.completion("wire-model").into();
        let result = model
            .call(request)
            .await
            .map(|_| ())
            .map_err(|e| e.to_string());
        (captured.lock().unwrap().clone(), result)
    }

    /// 带 assistant tool-call 历史的 rig 请求 + additional_params 携带 thinking。
    fn rig_request_with_tool_history(
        tool_choice: rig_core::message::ToolChoice,
        thinking_type: &str,
    ) -> rig_core::completion::CompletionRequest {
        use rig_core::message::{
            AssistantContent, AssistantMessage, Message, ToolCall, ToolFunction, ToolName,
        };
        let call = ToolCall::from_wire(
            "call-wire-1",
            ToolFunction::new(
                ToolName::try_from("echo_tool").unwrap(),
                serde_json::json!({"x":1}),
            ),
        );
        let assistant = Message::Assistant(AssistantMessage::new(vec![
            AssistantContent::text("先调工具"),
            AssistantContent::ToolCall(call),
        ]));
        let tool_result = Message::tool_result(
            rig_core::message::CallId::from_wire("call-wire-1"),
            ToolName::try_from("echo_tool").unwrap(),
            "42",
        );
        let tool = rig_core::completion::ToolDefinition::new(
            ToolName::try_from("echo_tool").unwrap(),
            "测试工具",
            serde_json::json!({"type":"object"}),
        );
        let mut request = rig_core::completion::CompletionRequest::new("继续")
            .messages([assistant, tool_result])
            .tool(tool)
            .tool_choice(tool_choice);
        request.additional_params = Some(serde_json::json!({ "thinking": { "type": thinking_type } }));
        request
    }

    fn body_of(req: &CapturedRequest) -> serde_json::Value {
        serde_json::from_str(&req.body).expect("请求体是 JSON")
    }

    /// DeepSeek 方言：思考开启时 forced tool choice 被丢弃（官方拒绝该组合），
    /// assistant 历史（含 tool call）带 `reasoning_content` 回传字段；思考关闭时
    /// tool_choice 保留。工具调用与工具结果消息本身不被 rewrite 丢弃。
    #[tokio::test]
    async fn deepseek_dialect_rewrites_tool_choice_and_replays_reasoning_field() {
        use rig_core::message::ToolChoice;

        // 思考开启 + required → tool_choice 被 drop
        let (reqs, result) = post_via_dialect(
            crate::ai::compat_dialect::ResolvedCompatDialect::DeepSeek,
            rig_request_with_tool_history(ToolChoice::Required, "enabled"),
            chat_completion_response("\"done\""),
        )
        .await;
        result.expect("请求应成功发出");
        let body = body_of(&reqs[0]);
        assert!(
            body.get("tool_choice").is_none(),
            "DeepSeek 思考开启时 forced tool choice 必须被 rewrite 丢弃: {}",
            body["tool_choice"]
        );
        assert_eq!(body["thinking"]["type"], "enabled", "thinking 参数原样随 wire");

        // assistant 历史带 reasoning_content（空串占位——quirk: 每条 assistant 消息恒带）
        let assistant_msgs: Vec<&serde_json::Value> = body["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "assistant")
            .collect();
        assert!(!assistant_msgs.is_empty(), "assistant 历史必须回放");
        for msg in &assistant_msgs {
            assert!(
                msg.get("reasoning_content").is_some(),
                "DeepSeek 方言下 assistant 消息应带 reasoning_content 回传字段: {msg}"
            );
        }
        // 工具调用与结果不丢——带 tools 定义时历史轮次以标准 tool_calls 数组 +
        // tool 消息回放（无 tools 时 rig 才降级为文本注记），rewrite 不得丢弃
        assert!(
            assistant_msgs.iter().any(|m| m
                .get("tool_calls")
                .is_some_and(|t| !t.as_array().unwrap().is_empty())),
            "assistant 工具调用必须完整回放: {}",
            body["messages"]
        );
        assert!(
            body["messages"].as_array().unwrap().iter().any(|m| m["role"] == "tool"
                && m["tool_call_id"] == "call-wire-1"
                && m["content"].as_str().is_some_and(|c| c.contains("42"))),
            "工具结果消息必须完整回放（tool_call_id + 内容）: {}",
            body["messages"]
        );

        // 思考关闭 → tool_choice 保留（不误伤）
        let (reqs, _) = post_via_dialect(
            crate::ai::compat_dialect::ResolvedCompatDialect::DeepSeek,
            rig_request_with_tool_history(ToolChoice::Required, "disabled"),
            chat_completion_response("\"done\""),
        )
        .await;
        let body = body_of(&reqs[0]);
        assert_eq!(
            body["tool_choice"], "required",
            "DeepSeek 思考关闭时 tool_choice 不应被丢弃"
        );
    }

    /// Moonshot 方言：`required` 降级为 `auto` 并追加引导消息；指定单工具本地拒绝
    /// （请求不发出，桩收不到任何请求，错误信息明确）。
    #[tokio::test]
    async fn moonshot_dialect_coerces_required_and_rejects_specific_choice() {
        use rig_core::message::{ToolChoice, ToolName};

        // required → auto + 引导消息
        let (reqs, result) = post_via_dialect(
            crate::ai::compat_dialect::ResolvedCompatDialect::Moonshot,
            rig_request_with_tool_history(ToolChoice::Required, "enabled"),
            chat_completion_response("\"ok\""),
        )
        .await;
        result.expect("required 请求应成功（降级为 auto 后发出）");
        let body = body_of(&reqs[0]);
        assert_eq!(
            body["tool_choice"], "auto",
            "Moonshot 不支持 required，必须降级为 auto"
        );
        let messages = body["messages"].as_array().unwrap();
        assert_eq!(
            messages.last().unwrap()["role"],
            "user",
            "降级时应追加引导 user 消息"
        );
        let steering = match &messages.last().unwrap()["content"] {
            serde_json::Value::String(text) => text.clone(),
            serde_json::Value::Array(parts) => parts
                .iter()
                .filter_map(|p| p["text"].as_str())
                .collect::<Vec<_>>()
                .join("
"),
            other => format!("{other}"),
        };
        assert!(
            steering.contains("tool"),
            "引导消息应提示模型选用工具: {steering}"
        );

        // 指定单工具 → 本地拒绝，请求不发出
        let (reqs, result) = post_via_dialect(
            crate::ai::compat_dialect::ResolvedCompatDialect::Moonshot,
            rig_request_with_tool_history(
                ToolChoice::Specific {
                    function_names: vec![ToolName::try_from("echo_tool").unwrap()],
                },
                "enabled",
            ),
            chat_completion_response("\"never\""),
        )
        .await;
        assert!(
            result.is_err(),
            "Moonshot 指定单工具必须本地拒绝，错误: {:?}",
            result
        );
        assert!(
            result.as_ref().unwrap_err().contains("forcing a specific tool"),
            "拒绝错误信息应指向根因: {:?}",
            result
        );
        assert!(
            reqs.is_empty(),
            "本地拒绝的请求不得发出（桩不应收到请求）"
        );
    }

    /// 未知网关铁律：Auto 解析为 Generic（捕获桩 host 非官方域名）时，
    /// body 不带任何方言 rewrite 痕迹——tool_choice 原样保留、assistant 无
    /// reasoning_content 注入。
    #[tokio::test]
    async fn generic_dialect_sends_unrewritten_body() {
        use rig_core::message::ToolChoice;
        // 显式 Generic 直驱（Auto → Generic 的解析由 compat_dialect 单测钉住）
        let (reqs, result) = post_via_dialect(
            crate::ai::compat_dialect::ResolvedCompatDialect::Generic,
            rig_request_with_tool_history(ToolChoice::Required, "enabled"),
            chat_completion_response("\"ok\""),
        )
        .await;
        result.expect("通用路径请求应成功");
        let body = body_of(&reqs[0]);
        assert_eq!(
            body["tool_choice"], "required",
            "通用路径不做 tool_choice rewrite"
        );
        for msg in body["messages"].as_array().unwrap() {
            if msg["role"] == "assistant" {
                assert!(
                    msg.get("reasoning_content").is_none(),
                    "通用路径不注入 reasoning_content: {msg}"
                );
            }
        }
    }

    /// 共享 usage 解析：DeepSeek `prompt_cache_hit_tokens` 在**通用**方言下同样可读
    /// （rig `normalized_usage` lenient 读取，与方言无关）——证明缓存 token
    /// 不是接入方言才有的能力，不将其包装成方言收益。
    #[tokio::test]
    async fn deepseek_cache_hit_tokens_readable_without_dialect() {
        let body = r#"{"id":"wire-c","object":"chat.completion","created":1,"model":"m","choices":[{"index":0,"message":{"role":"assistant","content":"ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":100,"prompt_cache_hit_tokens":64,"completion_tokens":4,"total_tokens":104}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        let (addr, captured) = spawn_capture_server(vec![response]);
        let provider = build_provider(&format!("http://{addr}/v1"));

        let resp = provider
            .complete(sample_wire_request())
            .await
            .expect("complete 应成功");
        assert_eq!(
            resp.usage.cached_input_tokens, 64,
            "缓存命中 token 经共享解析可读（非方言专属）"
        );
        assert!(resp.usage.reported);
        let reqs = captured.lock().unwrap();
        assert_eq!(reqs.len(), 1);
    }
    // ── 0.27.15 跨 Provider 思考 wire 测试（主窗口 RigProvider 链）──────────
    //
    // 钉住「能力 → 意图 → wire」链路：主窗口默认关策略按能力分派
    // （等级型 omit / 开关型显式关），显式意图剔除冲突私参、保留无关嵌套。

    /// 构造带自定义参数与 per-model effort 的 wire provider（0.27.15 测试辅助）。
    fn build_thinking_provider(
        base_url: &str,
        compat: ResolvedCompatDialect,
        model_id: &str,
        custom_parameters: &[crate::config::ai_config::CustomParam],
        reasoning_effort: Option<&str>,
    ) -> Arc<dyn AIProvider> {
        let model = ModelEntry {
            id: model_id.into(),
            display_name: "T".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: None,
            custom_parameters: custom_parameters.to_vec(),
            reasoning_effort: reasoning_effort.map(Into::into),
            thinking_style: None,
            capabilities: vec![],
        };
        build_openai_compatible("sk-wire-test", Some(base_url), &model, compat)
            .expect("构造 thinking wire provider")
    }

    /// DeepSeek 主窗口默认关：发 thinking.type=disabled（含等级档模型）；
    /// per-model 显式等级叠加 reasoning_effort。
    #[tokio::test]
    async fn main_window_deepseek_thinking_off_and_effort_wire() {
        // 默认（未配 effort）→ 显式关
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_thinking_provider(
            &format!("http://{addr}/v1"),
            ResolvedCompatDialect::DeepSeek,
            "deepseek-v4-flash",
            &[],
            None,
        );
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["thinking"]["type"], "disabled", "主窗口默认关策略：DeepSeek 显式关");

        // per-model effort=high → 开 + reasoning_effort=high（等级叠加）
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_thinking_provider(
            &format!("http://{addr}/v1"),
            ResolvedCompatDialect::DeepSeek,
            "deepseek-v4-flash",
            &[],
            Some("high"),
        );
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["thinking"]["type"], "enabled");
        assert_eq!(body["reasoning_effort"], "high");
    }

    /// 未知网关（Generic + 非 OpenAI 官方 host）：默认 omit（不发 reasoning_effort），
    /// 私参 reasoning_effort 透传保留；显式等级剔除冲突私参、保留无关字段。
    #[tokio::test]
    async fn main_window_generic_gateway_effort_passthrough_and_strip() {
        use crate::config::ai_config::CustomParam;
        // 默认 + 私参 reasoning_effort=low → FollowDefault：私参原样透传（现状语义）
        let custom = vec![
            CustomParam { key: "reasoning_effort".into(), value: serde_json::json!("low") },
            CustomParam { key: "top_p".into(), value: serde_json::json!(0.9) },
        ];
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_thinking_provider(
            &format!("http://{addr}/v1"),
            ResolvedCompatDialect::Generic,
            "some-model",
            &custom,
            None,
        );
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["reasoning_effort"], "low", "默认档不 patch——私参透传");
        assert_eq!(body["top_p"], 0.9);

        // 显式 effort=high → 剔除私参 reasoning_effort，high 胜出，top_p 保留
        let (addr, captured) = spawn_capture_server(vec![chat_completion_response("\"ok\"")]);
        let provider = build_thinking_provider(
            &format!("http://{addr}/v1"),
            ResolvedCompatDialect::Generic,
            "some-model",
            &custom,
            Some("high"),
        );
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["reasoning_effort"], "high", "显式意图剔除并覆盖冲突私参");
        assert_eq!(body["top_p"], 0.9, "无关私参保留");
    }

    /// Ollama 主窗口默认关：think=false 仍随 wire 下发。
    #[tokio::test]
    async fn main_window_ollama_thinking_off_wire() {
        let (addr, captured) = spawn_capture_server(vec![ollama_chat_response("\"ok\"")]);
        let model = super::tests::sample_model();
        let provider =
            build_ollama(Some(&format!("http://{addr}")), &model).expect("构造 ollama provider");
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(body["think"], false, "主窗口默认关策略：Ollama think=false");
    }

    /// Gemini 主窗口默认关 + 能力分派：2.5-flash（可关）→ thinkingBudget=0；
    /// 2.5-pro（不可关）→ 不发思考字段（诚实跟随默认，不报错不出 400）。
    /// 自定义参数 generationConfig.thinkingConfig 在显式关时剔除、其余键保留。
    #[tokio::test]
    async fn main_window_gemini_thinking_wire() {
        use crate::config::ai_config::CustomParam;
        let gemini_response = |text: &str| {
            let body = format!(
                r#"{{"candidates":[{{"content":{{"parts":[{{"text":"{text}"}}],"role":"model"}},"finishReason":"STOP"}}],"usageMetadata":{{"promptTokenCount":1,"candidatesTokenCount":1}}}}"#
            );
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .into_bytes()
        };
        let build = |model_id: &str, custom: Vec<CustomParam>| {
            ModelEntry {
                id: model_id.into(),
                display_name: "G".into(),
                enabled: true,
                context_window: None,
                input_price_per_million: None,
                output_price_per_million: None,
                temperature: None,
                max_tokens: None,
                custom_parameters: custom,
                reasoning_effort: None,
                thinking_style: None,
                capabilities: vec![],
            }
        };

        // 2.5-flash（可关）默认策略：无私参 → thinkingBudget=0
        let (addr, captured) = spawn_capture_server(vec![gemini_response("ok")]);
        let model = build("gemini-2.5-flash", vec![]);
        let provider = build_gemini("g-key", Some(&format!("http://{addr}")), &model)
            .expect("构造 gemini provider");
        let resp = provider.complete(sample_wire_request()).await.expect("gemini complete 应成功");
        assert_eq!(resp.text.as_deref(), Some("ok"), "Gemini 响应解析");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 0,
            "可关模型主窗口默认关：thinkingBudget=0"
        );

        // 2.5-flash + 用户私参 thinkingBudget=512：默认策略是产品默认值而非本轮
        // 意图——or_insert 语义保留用户刻意配置（512），temperature 等无关键同存
        let custom = vec![
            CustomParam {
                key: "generationConfig".into(),
                value: serde_json::json!({"thinkingConfig": {"thinkingBudget": 512}, "temperature": 0.5}),
            },
        ];
        let (addr, captured) = spawn_capture_server(vec![gemini_response("ok")]);
        let model = build("gemini-2.5-flash", custom);
        let provider = build_gemini("g-key", Some(&format!("http://{addr}")), &model)
            .expect("构造 gemini provider");
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 512,
            "默认关策略是产品默认值——用户刻意私参优先（or_insert 语义保留）"
        );
        assert_eq!(
            body["generationConfig"]["temperature"], 0.5,
            "generationConfig 无关键保留"
        );

        // 2.5-pro：不可关 → 不发思考字段（跟随服务默认），私参原样透传
        let custom = vec![
            CustomParam {
                key: "generationConfig".into(),
                value: serde_json::json!({"thinkingConfig": {"thinkingBudget": 512}}),
            },
        ];
        let (addr, captured) = spawn_capture_server(vec![gemini_response("ok2")]);
        let model = build("gemini-2.5-pro", custom);
        let provider = build_gemini("g-key", Some(&format!("http://{addr}")), &model)
            .expect("构造 gemini provider");
        provider.complete(sample_wire_request()).await.expect("complete 应成功");
        let reqs = captured.lock().unwrap();
        let body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(
            body["generationConfig"]["thinkingConfig"]["thinkingBudget"], 512,
            "不可关模型主窗口不 patch——私参透传（FollowDefault）"
        );
    }

    /// Anthropic 主窗口默认关：thinking.type=disabled 随 wire 下发。
    #[tokio::test]
    async fn main_window_anthropic_thinking_off_wire() {
        let body = r#"{"id":"wire-an","type":"message","role":"assistant","content":[{"type":"text","text":"ok"}],"model":"claude-sonnet-4-5","stop_reason":"end_turn","usage":{"input_tokens":1,"output_tokens":1}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        )
        .into_bytes();
        let (addr, captured) = spawn_capture_server(vec![response]);
        let model = ModelEntry {
            id: "claude-sonnet-4-5".into(),
            display_name: "C".into(),
            enabled: true,
            context_window: None,
            input_price_per_million: None,
            output_price_per_million: None,
            temperature: None,
            max_tokens: Some(4096),
            custom_parameters: vec![],
            reasoning_effort: None,
            thinking_style: None,
            capabilities: vec![],
        };
        let provider =
            build_anthropic("sk-wire", Some(&format!("http://{addr}/v1")), &model)
                .expect("构造 anthropic provider");
        let resp = provider.complete(sample_wire_request()).await.expect("complete 应成功");
        assert_eq!(resp.text.as_deref(), Some("ok"));
        let reqs = captured.lock().unwrap();
        let req_body: serde_json::Value = serde_json::from_str(&reqs[0].body).unwrap();
        assert_eq!(
            req_body["thinking"]["type"], "disabled",
            "主窗口默认关策略：Anthropic 显式关"
        );
    }
}
