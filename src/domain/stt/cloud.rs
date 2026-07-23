//! 云端 STT 引擎：通过 OpenAI 兼容 API 做语音转文字。
//!
//! ## 工作模式
//!
//! hold-to-talk 场景下非流式：
//! - `transcribe_chunk`：累积 PCM 样本，不返回 partial（空字符串）
//! - `finalize`：将累积的 PCM 转为 WAV，发送到云端 API，返回识别文本
//!
//! ## API 兼容性
//!
//! 支持两种协议（按供应商 kind 自动路由）：
//! 1. **标准 Whisper 接口**（openai / groq / custom）：
//!    `POST /v1/audio/transcriptions`，multipart/form-data 上传 WAV。
//! 2. **Chat-Completion ASR**（mimo）：
//!    `POST /v1/chat/completions`，JSON body 中以 base64 data-URI 嵌入音频。
//!
//! ## 0.12 §2.7 Provider 统一
//!
//! 密钥、base_url、kind 全部从 AIConfig::ProviderEntry 继承--
//! 一个 OpenAI key 同时用于 chat(GPT-4)和 STT(whisper)，用户只配一次。
//! SttConfig.cloud 引用 AIConfig 中的 provider_id + model_id。
//!
//! 旧配置（cloud_provider 有值但 cloud 为 None）走 `effective_cloud()` 自动迁移；
//! 迁移失败时回退到旧路径（直接读 cloud_provider），保证向后兼容。
//!
//! `resolve_stt_endpoint` + `send_stt_request` 是 `finalize` 与 `test_cloud_stt`
//! 的共用路径，保证测试按钮与实际识别走同一配置解析与发送逻辑。

use std::sync::Mutex;

use crate::app::ai_config::{self, ProviderKind};
use crate::infra::platform::secret;

use super::{SttEngine, SttError};

/// 云端 STT 引擎。
///
/// 累积 PCM 样本，在 `finalize` 时一次性发送到云端 API。
pub struct CloudSttEngine {
    /// 累积的 PCM 样本（f32, 16kHz, mono）
    samples: Mutex<Vec<f32>>,
    /// 采样率
    sample_rate: u32,
}

impl CloudSttEngine {
    pub fn new() -> Self {
        Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
        }
    }
}

impl Default for CloudSttEngine {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl SttEngine for CloudSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        // 非流式模式：只累积，不返回 partial
        self.samples.lock().unwrap().extend_from_slice(samples);
        Ok(String::new())
    }

    async fn finalize(&self) -> Result<String, SttError> {
        let samples = self.samples.lock().unwrap().clone();
        if samples.is_empty() {
            return Ok(String::new());
        }

        let config = crate::app::stt_config::get_stt_config();
        let ai_config = ai_config::get_ai_config();
        let endpoint = resolve_stt_endpoint(&config, &ai_config)?;

        let wav_bytes = super::wav::pcm_to_wav(&samples, self.sample_rate, 1);
        tracing::info!(
            model = %endpoint.model_id,
            samples = samples.len(),
            duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u64,
            uses_chat_asr = endpoint.uses_chat_completion_asr,
            "云端 STT 请求"
        );
        send_stt_request(&endpoint, &wav_bytes).await
    }

    fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }

    fn name(&self) -> &str {
        "cloud-stt"
    }
}

/// 解析后的云端 STT endpoint--`finalize` 与 `test_cloud_stt` 共用，
/// 保证测试按钮与实际识别走同一配置解析路径（0.12 §2.7）。
pub(crate) struct ResolvedSttEndpoint {
    /// 去尾斜杠的 base_url（如 `https://api.openai.com/v1`）
    pub base_url: String,
    /// API Key（本地 provider 无需密钥时为 None）
    pub api_key: Option<String>,
    /// 模型 id
    pub model_id: String,
    /// 是否走 chat-completion ASR 协议（mimo 等）
    pub uses_chat_completion_asr: bool,
}

/// 解析云端 STT endpoint：优先新路径（`effective_cloud` + AIConfig 供应商，含老配置自动迁移），
/// 回退旧路径（`cloud_provider` 直接读）。`finalize` 与 `test_cloud_stt` 共用。
pub(crate) fn resolve_stt_endpoint(
    config: &crate::app::stt_config::SttConfig,
    ai_config: &ai_config::AIConfig,
) -> Result<ResolvedSttEndpoint, SttError> {
    // 新路径：effective_cloud（cloud 有值直用；cloud_provider 老配置自动迁移）
    if let Some((cloud, _)) = config.effective_cloud(ai_config) {
        let provider = ai_config
            .providers
            .iter()
            .find(|p| p.id == cloud.provider_id)
            .ok_or_else(|| {
                SttError::Engine(format!(
                    "STT 供应商 {} 未在 AI 配置中找到",
                    cloud.provider_id
                ))
            })?;

        // ollama 是 LLM 推理服务，不支持 STT--早返友好错误（防用户误选）
        if matches!(provider.kind, ProviderKind::OllamaHttp) {
            return Err(SttError::Engine(
                "ollama 不支持语音识别（STT），请在 AI 供应商页选择支持 ASR 的模型".into(),
            ));
        }

        let api_key = if provider.kind.requires_secret() {
            Some(
                secret::load_secret(&provider.id, "key")
                    .map_err(|e| SttError::Engine(format!("API key 未配置: {e}")))?
                    .expose()
                    .to_owned(),
            )
        } else {
            None
        };
        let base_url = provider
            .base_url
            .as_deref()
            .unwrap_or_else(|| default_base_url_for_kind(provider.kind))
            .trim_end_matches('/')
            .to_string();
        return Ok(ResolvedSttEndpoint {
            base_url,
            api_key,
            model_id: cloud.model_id.clone(),
            uses_chat_completion_asr: is_chat_completion_asr(&provider.base_url),
        });
    }

    // 旧路径回退（cloud_provider 直接读，向后兼容）
    if let Some(old) = &config.cloud_provider {
        let secret_id = format!("stt:{}", old.kind);
        let api_key = Some(
            secret::load_secret(&secret_id, "key")
                .map_err(|e| SttError::Engine(format!("API key 未配置: {e}")))?
                .expose()
                .to_owned(),
        );
        let base_url = old
            .base_url
            .as_deref()
            .unwrap_or_else(|| default_base_url(&old.kind))
            .trim_end_matches('/')
            .to_string();
        return Ok(ResolvedSttEndpoint {
            base_url,
            api_key,
            model_id: old.model_id.clone(),
            uses_chat_completion_asr: super::wav::uses_chat_completion_asr(&old.kind),
        });
    }

    Err(SttError::NotInitialized)
}

/// 发送 WAV 到云端 STT endpoint（`finalize` 与 `test_cloud_stt` 共用）。
pub(crate) async fn send_stt_request(
    endpoint: &ResolvedSttEndpoint,
    wav_bytes: &[u8],
) -> Result<String, SttError> {
    if endpoint.uses_chat_completion_asr {
        let url = format!("{}/chat/completions", endpoint.base_url);
        tracing::debug!(%url, "chat-completion ASR 路径");
        super::wav::transcribe_via_chat_async(
            &url,
            endpoint.api_key.as_deref().unwrap_or(""),
            &endpoint.model_id,
            wav_bytes,
        )
        .await
    } else {
        let url = format!("{}/audio/transcriptions", endpoint.base_url);
        tracing::debug!(%url, "标准 Whisper 路径");
        super::wav::transcribe_async(
            &url,
            endpoint.api_key.as_deref(),
            &endpoint.model_id,
            wav_bytes,
        )
        .await
    }
}

/// 获取供应商默认 base_url。
fn default_base_url(kind: &str) -> &'static str {
    match kind {
        "openai" => "https://api.openai.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "mimo" => "https://api.xiaomimimo.com/v1",
        "mimo_plan" => "https://token-plan-cn.xiaomimimo.com/v1",
        _ => "https://api.openai.com/v1",
    }
}

/// 按 ProviderKind 获取默认 base_url（0.12 §2.7 新路径用）。
fn default_base_url_for_kind(kind: ProviderKind) -> &'static str {
    match kind {
        ProviderKind::OpenAICompatible => "https://api.openai.com/v1",
        // 其他 kind 理论上不用于 STT，但提供兜底
        ProviderKind::AnthropicMessages => "https://api.anthropic.com",
        ProviderKind::GeminiGenerateContent => "https://generativelanguage.googleapis.com",
        ProviderKind::OllamaHttp => "http://localhost:11434",
    }
}

/// 检测供应商是否使用 chat-completion ASR 协议（0.12 §2.7 新路径用）。
///
/// 旧路径通过 kind 字符串判断（"mimo" / "mimo_plan"）。
/// 新路径统一为 ProviderKind::OpenAICompatible，无法通过 kind 区分--
/// 改为检查 base_url 是否包含 mimo 域名。
fn is_chat_completion_asr(base_url: &Option<String>) -> bool {
    match base_url {
        Some(url) => url.contains("xiaomimimo.com"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn cloud_engine_accumulates_and_resets() {
        let engine = CloudSttEngine::new();

        // 累积样本（不返回 partial）
        let result = engine.transcribe_chunk(&[0.1, 0.2, 0.3]).await.unwrap();
        assert!(result.is_empty());

        engine.transcribe_chunk(&[0.4, 0.5]).await.unwrap();

        // reset 清空
        engine.reset();
        let samples = engine.samples.lock().unwrap();
        assert!(samples.is_empty());
    }

    // ── 0.12 §2.7: 新路径辅助函数测试 ──────────────────────────────────

    #[test]
    fn is_chat_completion_asr_detects_mimo_by_base_url() {
        // mimo 域名 -> true
        assert!(is_chat_completion_asr(&Some(
            "https://api.xiaomimimo.com/v1".into()
        )));
        assert!(is_chat_completion_asr(&Some(
            "https://token-plan-cn.xiaomimimo.com/v1".into()
        )));

        // 非 mimo -> false
        assert!(!is_chat_completion_asr(&Some(
            "https://api.openai.com/v1".into()
        )));
        assert!(!is_chat_completion_asr(&Some(
            "https://api.groq.com/openai/v1".into()
        )));

        // None -> false（默认走标准 Whisper）
        assert!(!is_chat_completion_asr(&None));
    }

    #[test]
    fn default_base_url_for_kind_returns_correct_defaults() {
        assert_eq!(
            default_base_url_for_kind(ProviderKind::OpenAICompatible),
            "https://api.openai.com/v1"
        );
        assert_eq!(
            default_base_url_for_kind(ProviderKind::OllamaHttp),
            "http://localhost:11434"
        );
        // 非 STT 常用 kind 也有兜底
        assert!(!default_base_url_for_kind(ProviderKind::AnthropicMessages).is_empty());
        assert!(!default_base_url_for_kind(ProviderKind::GeminiGenerateContent).is_empty());
    }

    #[test]
    fn default_base_url_legacy_covers_known_kinds() {
        assert_eq!(default_base_url("openai"), "https://api.openai.com/v1");
        assert_eq!(default_base_url("groq"), "https://api.groq.com/openai/v1");
        assert_eq!(default_base_url("mimo"), "https://api.xiaomimimo.com/v1");
        assert_eq!(
            default_base_url("mimo_plan"),
            "https://token-plan-cn.xiaomimimo.com/v1"
        );
        // 未知 kind 兜底 OpenAI
        assert_eq!(default_base_url("unknown"), "https://api.openai.com/v1");
    }

    /// ollama 不支持 STT，`resolve_stt_endpoint` 应早返友好错误（不依赖系统资源）。
    #[test]
    fn resolve_stt_endpoint_rejects_ollama() {
        use crate::app::ai_config::{AIConfig, ModelCapability, ProviderEntry};
        use crate::app::stt_config::{SttCloudConfig, SttConfig};

        let cfg = SttConfig {
            cloud: Some(SttCloudConfig {
                provider_id: "p".into(),
                model_id: "m".into(),
            }),
            ..Default::default()
        };
        let ai = AIConfig {
            providers: vec![ProviderEntry {
                id: "p".into(),
                display_name: "ollama".into(),
                kind: ProviderKind::OllamaHttp,
                base_url: None,
                secret_ref: "x".into(),
                models: vec![crate::app::ai_config::ModelEntry {
                    id: "m".into(),
                    display_name: "M".into(),
                    enabled: true,
                    context_window: None,
                    input_price_per_million: None,
                    output_price_per_million: None,
                    temperature: None,
                    max_tokens: None,
                    custom_parameters: Vec::new(),
                    capabilities: vec![ModelCapability::Chat],
                }],
                created_at: 0,
            }],
            ..Default::default()
        };
        let r = resolve_stt_endpoint(&cfg, &ai);
        assert!(r.is_err(), "ollama 不应作为 STT 源");
    }
}
