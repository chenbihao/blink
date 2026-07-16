//! WAV 编码 / 解码 + OpenAI 兼容 HTTP 转录的共享工具。
//!
//! 供 `cloud.rs`（云端 STT）和 `local.rs`（本地 FunASR STT）共用，
//! 消除原先三处重复的 `pcm_to_wav` / `transcribe_async` 实现。
//!
//! ## 云端 STT 的两种 API 协议
//!
//! 1. **标准 Whisper 接口**（OpenAI / Groq 等）：
//!    `POST /v1/audio/transcriptions`，multipart/form-data 上传 WAV。
//! 2. **Chat-Completion ASR**（Mimo 等）：
//!    `POST /v1/chat/completions`，JSON body 中以 base64 data-URI 嵌入音频。

use std::io::Write;
use std::path::Path;

// ── WAV 编码 ─────────────────────────────────────────────────────────────

/// PCM f32 样本 → WAV 字节（16-bit PCM, little-endian）。
///
/// f32 范围 [-1.0, 1.0] → i16，超出范围的值做 clamp。
pub fn pcm_to_wav(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
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

/// 将 PCM f32 样本写入 WAV 文件（16-bit, mono PCM）。
///
/// 与 [`pcm_to_wav`] 使用相同的编码逻辑，但直接写入文件而非返回字节。
/// 供诊断命令写测试音频文件使用。
#[allow(dead_code)]
pub fn write_wav_file(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    let data_len = samples.len() * 2; // 16-bit = 2 bytes/sample
    let file_size = 36 + data_len as u32;

    let mut file = std::fs::File::create(path).map_err(|e| format!("创建 WAV 文件失败: {e}"))?;

    // RIFF header
    file.write_all(b"RIFF")
        .map_err(|e| format!("写 RIFF 失败: {e}"))?;
    file.write_all(&file_size.to_le_bytes())
        .map_err(|e| format!("写 file_size 失败: {e}"))?;
    file.write_all(b"WAVE")
        .map_err(|e| format!("写 WAVE 失败: {e}"))?;

    // fmt chunk
    file.write_all(b"fmt ")
        .map_err(|e| format!("写 fmt 失败: {e}"))?;
    file.write_all(&16u32.to_le_bytes())
        .map_err(|e| format!("写 chunk_size 失败: {e}"))?;
    file.write_all(&1u16.to_le_bytes())
        .map_err(|e| format!("写 audio_format 失败: {e}"))?; // PCM
    file.write_all(&1u16.to_le_bytes())
        .map_err(|e| format!("写 num_channels 失败: {e}"))?; // mono
    file.write_all(&sample_rate.to_le_bytes())
        .map_err(|e| format!("写 sample_rate 失败: {e}"))?;
    let byte_rate = sample_rate * 2; // 16-bit mono
    file.write_all(&byte_rate.to_le_bytes())
        .map_err(|e| format!("写 byte_rate 失败: {e}"))?;
    file.write_all(&2u16.to_le_bytes())
        .map_err(|e| format!("写 block_align 失败: {e}"))?;
    file.write_all(&16u16.to_le_bytes())
        .map_err(|e| format!("写 bits_per_sample 失败: {e}"))?;

    // data chunk
    file.write_all(b"data")
        .map_err(|e| format!("写 data 失败: {e}"))?;
    file.write_all(&(data_len as u32).to_le_bytes())
        .map_err(|e| format!("写 data_size 失败: {e}"))?;

    // PCM 数据（f32 → i16 LE）
    let mut pcm = Vec::with_capacity(data_len);
    for &s in samples {
        let clamped = s.max(-1.0).min(1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        pcm.extend_from_slice(&i16_sample.to_le_bytes());
    }
    file.write_all(&pcm)
        .map_err(|e| format!("写 PCM 数据失败: {e}"))?;

    Ok(())
}

// ── WAV 解码 ─────────────────────────────────────────────────────────────

/// 解析 WAV 文件为 f32 PCM 样本（16-bit, 16kHz, mono）。
///
/// 简化版解析器：跳过非 data chunk，只读 PCM data。
/// 供诊断命令和测试共用。
pub fn parse_wav_to_f32(data: &[u8]) -> Result<Vec<f32>, String> {
    // RIFF header
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("不是有效的 WAV 文件".to_string());
    }

    // 跳过 fmt chunk，找到 data chunk
    let mut offset = 12;
    let mut samples = Vec::new();

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        if chunk_id == b"data" {
            // 16-bit PCM samples
            let data_start = offset + 8;
            let data_end = (data_start + chunk_size).min(data.len());
            let pcm_bytes = &data[data_start..data_end];

            samples = pcm_bytes
                .chunks_exact(2)
                .map(|chunk| {
                    let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    sample as f32 / 32768.0
                })
                .collect();
            break;
        }

        offset += 8 + chunk_size;
        // chunks are word-aligned
        if chunk_size % 2 == 1 {
            offset += 1;
        }
    }

    if samples.is_empty() {
        return Err("WAV 中未找到 PCM data".to_string());
    }

    Ok(samples)
}

// ── 供应商协议判定 ───────────────────────────────────────────────────────

/// 判断云端 STT 供应商是否使用 chat-completion ASR 协议。
///
/// - `mimo` / `mimo_plan`：使用 `POST /v1/chat/completions`，base64 音频嵌入 messages
/// - 其他（openai / groq / custom）：使用标准 `POST /v1/audio/transcriptions`
pub fn uses_chat_completion_asr(provider_kind: &str) -> bool {
    matches!(provider_kind, "mimo" | "mimo_plan")
}

// ── HTTP 转录（标准 Whisper 接口）────────────────────────────────────────

/// 异步 HTTP 转录请求（OpenAI 兼容格式）。
///
/// - `url`：完整 URL（如 `https://api.openai.com/v1/audio/transcriptions`）
/// - `api_key`：Bearer token；本地服务传 `None`
/// - `model_id`：模型标识（如 `whisper-large-v3` / `sensevoice`）
/// - `wav_bytes`：WAV 格式音频字节
///
/// 返回识别文本。HTTP 错误或 JSON 解析失败转为 [`super::SttError`]。
pub async fn transcribe_async(
    url: &str,
    api_key: Option<&str>,
    model_id: &str,
    wav_bytes: &[u8],
) -> Result<String, super::SttError> {
    use reqwest::multipart;

    let part = multipart::Part::bytes(wav_bytes.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| super::SttError::Engine(format!("multipart 构建失败: {e}")))?;

    let form = multipart::Form::new()
        .text("model", model_id.to_string())
        .part("file", part);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| super::SttError::Engine(format!("HTTP client 创建失败: {e}")))?;

    let mut req = client.post(url).multipart(form);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| super::SttError::Engine(format!("HTTP 请求失败: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(super::SttError::Engine(format!("HTTP {status}: {body}")));
    }

    #[derive(serde::Deserialize)]
    struct TranscriptionResponse {
        text: String,
    }

    let result: TranscriptionResponse = resp
        .json()
        .await
        .map_err(|e| super::SttError::Engine(format!("JSON 解析失败: {e}")))?;

    Ok(result.text)
}

// ── HTTP 转录（Chat-Completion ASR 接口）─────────────────────────────────

/// 通过 chat completions API 做语音转文字（Mimo 等供应商）。
///
/// 请求格式：`POST /v1/chat/completions`，JSON body，音频以 base64 data-URI
/// 嵌入 `messages[0].content[0].input_audio.data`。
///
/// - `url`：完整 URL（如 `https://api.xiaomimimo.com/v1/chat/completions`）
/// - `api_key`：Bearer token
/// - `model_id`：模型标识（如 `mimo-v2.5-asr`）
/// - `wav_bytes`：WAV 格式音频字节
///
/// 返回 `choices[0].message.content` 文本。
pub async fn transcribe_via_chat_async(
    url: &str,
    api_key: &str,
    model_id: &str,
    wav_bytes: &[u8],
) -> Result<String, super::SttError> {
    use base64::Engine as _;

    let audio_b64 = base64::engine::general_purpose::STANDARD.encode(wav_bytes);
    let data_uri = format!("data:audio/wav;base64,{audio_b64}");

    let body = serde_json::json!({
        "model": model_id,
        "messages": [{
            "role": "user",
            "content": [{
                "type": "input_audio",
                "input_audio": {
                    "data": data_uri
                }
            }]
        }],
        "asr_options": {
            "language": "auto"
        }
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| super::SttError::Engine(format!("HTTP client 创建失败: {e}")))?;

    let resp = client
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .map_err(|e| super::SttError::Engine(format!("HTTP 请求失败: {e}")))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let resp_body = resp.text().await.unwrap_or_default();
        return Err(super::SttError::Engine(format!("HTTP {status}: {resp_body}")));
    }

    // Chat completion 响应: { "choices": [{ "message": { "content": "..." } }] }
    #[derive(serde::Deserialize)]
    struct ChatResponse {
        choices: Vec<ChatChoice>,
    }

    #[derive(serde::Deserialize)]
    struct ChatChoice {
        message: ChatMessage,
    }

    #[derive(serde::Deserialize)]
    struct ChatMessage {
        content: String,
    }

    let result: ChatResponse = resp
        .json()
        .await
        .map_err(|e| super::SttError::Engine(format!("JSON 解析失败: {e}")))?;

    result
        .choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .ok_or_else(|| super::SttError::Engine("响应中无 choices".to_string()))
}

// ── 测试 ──────────────────────────────────────────────────────────────────

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
    fn write_wav_file_produces_valid_file() {
        let tmp = std::env::temp_dir().join("blink_wav_shared_test.wav");
        let sample_rate = 16000u32;
        let samples: Vec<f32> = (0..sample_rate)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();

        write_wav_file(&tmp, &samples, sample_rate).expect("write_wav_file 失败");

        let data = std::fs::read(&tmp).expect("读取 WAV 文件失败");
        let _ = std::fs::remove_file(&tmp);

        assert_eq!(&data[0..4], b"RIFF", "RIFF 魔数不匹配");
        assert_eq!(&data[8..12], b"WAVE", "WAVE 魔数不匹配");
        assert_eq!(&data[12..16], b"fmt ", "fmt chunk 标识不匹配");
        assert_eq!(&data[36..40], b"data", "data chunk 标识不匹配");

        let audio_format = u16::from_le_bytes([data[20], data[21]]);
        assert_eq!(audio_format, 1, "音频格式应为 PCM(1)");
        let num_channels = u16::from_le_bytes([data[22], data[23]]);
        assert_eq!(num_channels, 1, "声道数应为 1(mono)");
        let sr = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
        assert_eq!(sr, sample_rate, "采样率不匹配");
        let bits = u16::from_le_bytes([data[34], data[35]]);
        assert_eq!(bits, 16, "位深应为 16");

        let data_size = u32::from_le_bytes([data[40], data[41], data[42], data[43]]);
        assert_eq!(data_size as usize, samples.len() * 2, "PCM 数据长度不匹配");
    }

    #[test]
    fn parse_wav_roundtrip() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -0.3, 0.7];
        let wav = pcm_to_wav(&samples, 16000, 1);
        let parsed = parse_wav_to_f32(&wav).expect("解析失败");

        assert_eq!(parsed.len(), samples.len());
        for (i, (a, b)) in samples.iter().zip(parsed.iter()).enumerate() {
            assert!((a - b).abs() < 1e-4, "样本 {i} 不匹配: {a} vs {b}");
        }
    }
}
