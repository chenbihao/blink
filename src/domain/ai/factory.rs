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
//! - `secret::load_secret` 缺 → `AIError::NotConfigured`(不是 Provider,语义"未配置")
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

use rig_core::client::CompletionClient;

use crate::app::ai_config::{ModelEntry, ProviderEntry, ProviderKind};
use crate::domain::ai::provider::{AIError, AIProvider};
use crate::domain::ai::registry::ProviderFactory;
use crate::domain::ai::rig_provider::{RigProvider, expose_for_rig};
use crate::infra::platform::secret;

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
            target: crate::infra::utils::perf::ai_slo::TARGET,
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
/// 1. 从 CM 读密钥(缺 → NotConfigured)
/// 2. 按 `entry.kind` 构造 rig `Client`(base_url 可覆盖)
/// 3. `client.completion_model(&model.id)` 得到具体 `CompletionModel`
/// 4. 包进 `RigProvider<M>` → 擦除 `Arc<dyn AIProvider>`
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
        //    云端 provider 缺密钥 = 未配置,不是错误。
        let key_str: String;
        if entry.kind.requires_secret() {
            let key = secret::load_secret(&entry.id, "key").map_err(|e| {
                tracing::debug!(
                    target: crate::infra::utils::perf::ai_slo::TARGET,
                    "AI factory: {} 密钥未配置 ({e})",
                    entry.display_name,
                );
                AIError::NotConfigured
            })?;
            key_str = expose_for_rig(&key);
        } else {
            // 本地 provider 无需密钥——ollama 等
            tracing::debug!(
                target: crate::infra::utils::perf::ai_slo::TARGET,
                "AI factory: {} 本地 provider,跳过密钥加载",
                entry.display_name,
            );
            key_str = String::new(); // 空字符串,rig ollama client 不使用
        }
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
            target: crate::infra::utils::perf::ai_slo::TARGET,
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
) -> Result<rig_core::providers::openai::CompletionsClient, AIError> {
    use rig_core::providers::openai;
    let url = base_url.filter(|s| !s.is_empty()).ok_or_else(|| {
        AIError::Provider(
            "OpenAI Compatible 协议必须配 base_url(如 https://api.openai.com/v1)".into(),
        )
    })?;
    openai::CompletionsClient::builder()
        .api_key(key)
        .base_url(url)
        .build()
        .map_err(|_| AIError::Provider("openai-compatible client 构造失败".into()))
}

/// 构造 Anthropic Messages 协议的 rig client（0.12.1 抽出）。
pub(crate) fn build_anthropic_client(
    key: &str,
    base_url: Option<&str>,
) -> Result<rig_core::providers::anthropic::Client, AIError> {
    use rig_core::providers::anthropic;
    let mut builder = anthropic::Client::builder().api_key(key);
    if let Some(url) = base_url.filter(|s| !s.is_empty()) {
        builder = builder.base_url(url);
    }
    builder
        .build()
        .map_err(|_| AIError::Provider("anthropic client 构造失败".into()))
}

/// 构造 Google Gemini 协议的 rig client（0.12.1 抽出）。
/// rig 0.39 gemini builder 不支持 base_url（端点固定 googleapis.com），用户填的忽略。
pub(crate) fn build_gemini_client(
    key: &str,
    _base_url: Option<&str>,
) -> Result<rig_core::providers::gemini::Client, AIError> {
    use rig_core::providers::gemini;
    gemini::Client::builder()
        .api_key(key)
        .build()
        .map_err(|_| AIError::Provider("gemini client 构造失败".into()))
}

/// 构造 ollama 本地推理的 rig client（0.12.1 抽出）。
/// 无需 API Key（OllamaApiKey::default()=None），base_url 默认 localhost:11434。
pub(crate) fn build_ollama_client(
    base_url: Option<&str>,
) -> Result<rig_core::providers::ollama::Client, AIError> {
    use rig_core::providers::ollama;
    let mut builder = ollama::Client::builder().api_key(ollama::OllamaApiKey::default());
    if let Some(url) = base_url.filter(|s| !s.is_empty()) {
        builder = builder.base_url(url);
    }
    builder
        .build()
        .map_err(|e| AIError::Provider(format!("ollama client 构造失败: {e}")))
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
    let rig_model = client.completion_model(&model.id);
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OpenAICompatible,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
    )))
}

/// Anthropic Messages 协议——`/v1/messages`,仅 Claude 官方。
fn build_anthropic(
    key: &str,
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    let client = build_anthropic_client(key, base_url)?;
    let rig_model = client.completion_model(&model.id);
    Ok(Arc::new(RigProvider::new(
        ProviderKind::AnthropicMessages,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
    )))
}

/// Google Gemini GenerateContent 协议——`/v1beta/models/*:generateContent`。
///
/// rig 0.39 里 `gemini::Client::builder()` 支持 `api_key` 但不显式支持 `base_url`
/// (Gemini 端点固定在 googleapis.com);如果用户填了 base_url 我们也不报错,
/// 仅忽略——避免"填了没用又不知道"的困惑,统一走 rig 默认。
fn build_gemini(
    key: &str,
    base_url: Option<&str>,
    model: &ModelEntry,
) -> Result<Arc<dyn AIProvider>, AIError> {
    let client = build_gemini_client(key, base_url)?;
    let rig_model = client.completion_model(&model.id);
    Ok(Arc::new(RigProvider::new(
        ProviderKind::GeminiGenerateContent,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
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
        .any(|c| matches!(c, crate::app::ai_config::ModelCapability::Chat))
    {
        tracing::warn!(
            model_id = %model.id,
            capabilities = ?model.capabilities,
            "ollama 模型未标记 Chat 能力，构造为 completion model 可能运行时失败（embedding 模型请在 0.13 RAG 消费）"
        );
    }

    let rig_model = client.completion_model(&model.id);
    Ok(Arc::new(RigProvider::new(
        ProviderKind::OllamaHttp,
        model.id.clone(),
        rig_model,
        None,
        model.temperature,
        model.max_tokens,
        &model.custom_parameters,
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
    use crate::app::ai_config::{ModelCapability, ProviderKind};

    fn sample_entry(kind: ProviderKind, base_url: Option<String>) -> ProviderEntry {
        ProviderEntry {
            id: "test-provider-uuid".into(),
            display_name: "Test".into(),
            kind,
            base_url,
            secret_ref: "blink/test-provider-uuid/key".into(),
            models: Vec::new(),
            created_at: 0,
        }
    }

    fn sample_model() -> ModelEntry {
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
        // 无密钥场景 → load_secret 缺 → NotConfigured(§6.4 兜底铁则,不 panic)
        let f = default_factory();
        let entry = sample_entry(ProviderKind::OpenAICompatible, None);
        let result = f.build(&entry, &sample_model());
        assert!(
            matches!(result, Err(AIError::NotConfigured)),
            "无密钥应返 NotConfigured 而非其他错误(is_ok={})",
            result.is_ok()
        );
    }

    #[test]
    fn all_cloud_kinds_have_factory_arms() {
        // 云端三类协议都能触达 factory dispatch;无密钥场景一律 NotConfigured 早于协议构造。
        // 这是"分派完整性"测试:防止未来新增 kind 时忘了 factory arm。
        // 0.12: OllamaHttp 不需要密钥,不走 NotConfigured 路径,单独测试。
        let f = RigFactory;
        for kind in [
            ProviderKind::OpenAICompatible,
            ProviderKind::AnthropicMessages,
            ProviderKind::GeminiGenerateContent,
        ] {
            let entry = sample_entry(kind, None);
            let result = f.build(&entry, &sample_model());
            assert!(
                matches!(result, Err(AIError::NotConfigured)),
                "kind={kind:?} 无密钥应返 NotConfigured"
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
