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
//! 2. **Chat-Completion ASR**（mimo / mimo_plan）：
//!    `POST /v1/chat/completions`，JSON body 中以 base64 data-URI 嵌入音频。
//!
//! ## 独立配置模式
//!
//! STT 云端配置完全独立于 AIConfig——`SttCloudProvider` 直接存储 kind/base_url/model_id。
//! API Key 用 `stt:cloud` 前缀存在 Credential Manager 里，不与 AI 供应商共用。
//!
//! `resolve_stt_endpoint` + `send_stt_request` 是 `finalize` 与 `test_cloud_stt`
//! 的共用路径，保证测试按钮与实际识别走同一配置解析与发送逻辑。

use std::sync::Mutex;

use crate::infra::platform::secret;

use super::{SttEngine, SttError};

/// Credential Manager 中 STT 密钥的 target_id。
///
/// 独立于 AI 供应商的密钥（AI 用 `{provider_id}` 作 target_id，STT 用 `stt:cloud`）。
/// 复用 Credential Manager 体系但不交叉引用。
const STT_SECRET_ID: &str = "stt:cloud";

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

        let config = crate::domain::config::stt_config::get_stt_config();
        let endpoint = resolve_stt_endpoint(&config)?;

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
/// 保证测试按钮与实际识别走同一配置解析路径。
pub(crate) struct ResolvedSttEndpoint {
    /// 去尾斜杠的 base_url（如 `https://api.openai.com/v1`）
    pub base_url: String,
    /// API Key
    pub api_key: Option<String>,
    /// 模型 id
    pub model_id: String,
    /// 是否走 chat-completion ASR 协议（mimo 等）
    pub uses_chat_completion_asr: bool,
}

/// 解析云端 STT endpoint：直接从 `SttCloudProvider` 读取 kind/base_url/model_id，
/// API Key 从 Credential Manager 的 `stt:cloud` 加载。
///
/// `finalize` 与 `test_cloud_stt` 共用此函数。
pub(crate) fn resolve_stt_endpoint(
    config: &crate::domain::config::stt_config::SttConfig,
) -> Result<ResolvedSttEndpoint, SttError> {
    let provider = config.cloud_provider.as_ref().ok_or(SttError::NotInitialized)?;

    let api_key = Some(
        secret::load_secret(STT_SECRET_ID, "key")
            .map_err(|e| SttError::Engine(format!("STT API key 未配置: {e}")))?
            .expose()
            .to_owned(),
    );

    let base_url = provider
        .base_url
        .as_deref()
        .unwrap_or_else(|| default_base_url(&provider.kind))
        .trim_end_matches('/')
        .to_string();

    Ok(ResolvedSttEndpoint {
        base_url,
        api_key,
        model_id: provider.model_id.clone(),
        uses_chat_completion_asr: super::wav::uses_chat_completion_asr(&provider.kind),
    })
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

    #[test]
    fn default_base_url_covers_known_kinds() {
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

    /// resolve_stt_endpoint 在 cloud_provider 为 None 时返回 NotInitialized。
    #[test]
    fn resolve_stt_endpoint_returns_err_when_not_configured() {
        let cfg = crate::domain::config::stt_config::SttConfig::default();
        let r = resolve_stt_endpoint(&cfg);
        assert!(r.is_err());
    }
}
