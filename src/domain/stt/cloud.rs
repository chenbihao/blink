//! 云端 STT 引擎：通过 OpenAI 兼容 API 做语音转文字。
//!
//! ## 工作模式
//!
//! hold-to-talk 场景下非流式：
//! - `transcribe_chunk`：累积 PCM 样本，不返回 partial（空字符串）
//! - `finalize`：将累积的 PCM 转为 WAV，POST 到 `/v1/audio/transcriptions`，返回识别文本
//!
//! ## API 兼容性
//!
//! 支持 OpenAI / Groq / Azure OpenAI 等兼容 `/v1/audio/transcriptions` 的供应商。
//! 请求格式：multipart/form-data，字段 `file`(audio.wav) + `model`(model_id)。
//! 响应格式：`{"text": "..."}`。

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

impl SttEngine for CloudSttEngine {
    fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        // 非流式模式：只累积，不返回 partial
        self.samples
            .lock()
            .unwrap()
            .extend_from_slice(samples);
        Ok(String::new())
    }

    fn finalize(&self) -> Result<String, SttError> {
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

        let url = format!("{base_url}/audio/transcriptions");

        // PCM → WAV
        let wav_bytes = pcm_to_wav(&samples, self.sample_rate, 1);

        tracing::info!(
            url = %url,
            model = %provider.model_id,
            samples = samples.len(),
            duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u64,
            "云端 STT 请求"
        );

        // 使用 block_in_place 在 tokio 多线程运行时中执行 async HTTP
        let result = tokio::task::block_in_place(|| {
            tokio::runtime::Handle::current().block_on(async {
                transcribe_async(
                    &url,
                    &api_key.expose(),
                    &provider.model_id,
                    &wav_bytes,
                )
                .await
            })
        });

        result
    }

    fn reset(&self) {
        self.samples.lock().unwrap().clear();
    }

    fn name(&self) -> &str {
        "cloud-stt"
    }
}

/// 异步 HTTP 转录请求。
async fn transcribe_async(
    url: &str,
    api_key: &str,
    model_id: &str,
    wav_bytes: &[u8],
) -> Result<String, SttError> {
    use reqwest::multipart;

    let part = multipart::Part::bytes(wav_bytes.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| SttError::Engine(format!("multipart 构建失败: {e}")))?;

    let form = multipart::Form::new()
        .text("model", model_id.to_string())
        .part("file", part);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| SttError::Engine(format!("HTTP client 创建失败: {e}")))?;

    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .multipart(form)
        .send()
        .await
        .map_err(|e| SttError::Engine(format!("HTTP 请求失败: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(SttError::Engine(format!(
            "HTTP {status}: {body}"
        )));
    }

    #[derive(serde::Deserialize)]
    struct TranscriptionResponse {
        text: String,
    }

    let result: TranscriptionResponse = resp
        .json()
        .await
        .map_err(|e| SttError::Engine(format!("JSON 解析失败: {e}")))?;

    Ok(result.text)
}

/// 获取供应商默认 base_url。
fn default_base_url(kind: &str) -> &'static str {
    match kind {
        "openai" => "https://api.openai.com/v1",
        "groq" => "https://api.groq.com/openai/v1",
        _ => "https://api.openai.com/v1",
    }
}

/// PCM f32 样本 → WAV 字节（16-bit PCM, little-endian）。
fn pcm_to_wav(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let num_samples = samples.len();
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
    let block_align = channels * (bits_per_sample / 8);
    let data_size = num_samples * (bits_per_sample / 8) as usize;
    let file_size = 36 + data_size; // RIFF header (12) + fmt chunk (24) + data

    let mut wav = Vec::with_capacity(44 + data_size);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(file_size as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes()); // chunk size
    wav.extend_from_slice(&1u16.to_le_bytes()); // audio format = PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_size as u32).to_le_bytes());

    // PCM samples: f32 → i16
    for &sample in samples {
        let clamped = sample.max(-1.0).min(1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        wav.extend_from_slice(&i16_sample.to_le_bytes());
    }

    wav
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wav_header_is_correct() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let wav = pcm_to_wav(&samples, 16000, 1);

        // RIFF header
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");

        // fmt chunk
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes([wav[16], wav[17], wav[18], wav[19]]), 16);
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1); // PCM

        // data chunk
        assert_eq!(&wav[36..40], b"data");
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_size as usize, samples.len() * 2);

        // Total size = 44 header + data
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    #[test]
    fn wav_samples_are_clamped() {
        let samples = vec![2.0, -2.0]; // 超出范围
        let wav = pcm_to_wav(&samples, 16000, 1);

        // 第一个样本（data 从 offset 44 开始）
        let s0 = i16::from_le_bytes([wav[44], wav[45]]);
        let s1 = i16::from_le_bytes([wav[46], wav[47]]);

        assert_eq!(s0, 32767); // clamped to max
        assert_eq!(s1, -32767); // clamped to min
    }

    #[test]
    fn cloud_engine_accumulates_and_resets() {
        let engine = CloudSttEngine::new();

        // 累积样本（不返回 partial）
        let result = engine.transcribe_chunk(&[0.1, 0.2, 0.3]).unwrap();
        assert!(result.is_empty());

        engine.transcribe_chunk(&[0.4, 0.5]).unwrap();

        // reset 清空
        engine.reset();
        let samples = engine.samples.lock().unwrap();
        assert!(samples.is_empty());
    }
}
