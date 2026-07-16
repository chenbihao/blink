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

use std::sync::Mutex;

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

        // 读取 STT 配置
        let config = crate::app::stt_config::get_stt_config();
        let provider = config
            .cloud_provider
            .as_ref()
            .ok_or(SttError::NotInitialized)?;

        // 加载 API Key
        let secret_id = format!("stt:{}", provider.kind);
        let api_key = secret::load_secret(&secret_id, "key")
            .map_err(|e| SttError::Engine(format!("API key 未配置: {e}")))?;

        // 构建 base_url
        let base_url = provider
            .base_url
            .as_deref()
            .unwrap_or_else(|| default_base_url(&provider.kind))
            .trim_end_matches('/');

        // PCM → WAV
        let wav_bytes = super::wav::pcm_to_wav(&samples, self.sample_rate, 1);

        tracing::info!(
            provider = %provider.kind,
            model = %provider.model_id,
            samples = samples.len(),
            duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u64,
            "云端 STT 请求"
        );

        // 按供应商协议路由
        if super::wav::uses_chat_completion_asr(&provider.kind) {
            let url = format!("{base_url}/chat/completions");
            tracing::debug!(%url, "chat-completion ASR 路径");
            super::wav::transcribe_via_chat_async(
                &url,
                &api_key.expose(),
                &provider.model_id,
                &wav_bytes,
            )
            .await
        } else {
            let url = format!("{base_url}/audio/transcriptions");
            tracing::debug!(%url, "标准 Whisper 路径");
            super::wav::transcribe_async(
                &url,
                Some(&api_key.expose()),
                &provider.model_id,
                &wav_bytes,
            )
            .await
        }
    }

    fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }

    fn name(&self) -> &str {
        "cloud-stt"
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
}
