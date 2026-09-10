//! WAV 编码 / 解码 + OpenAI 兼容 HTTP 转录的共享工具。
//!
//! 供 `cloud.rs`（云端 STT）和 `local.rs`（本地 FunASR STT）共用，
//! 消除原先三处重复的 `pcm_to_wav` / `transcribe_async` 实现。
//!
//! ## 0.22.16 变更
//!
//! 正式 WAV decoder 已迁移至 `infra/platform/audio/wav.rs`，
//! 支持完整 RIFF/WAVE 格式验证（PCM 16/24/32, IEEE float32, extensible）。
//! 本模块保留 canonical WAV encoder（`pcm_to_wav` / `write_wav_file`）
//! 和 HTTP 转录工具，解码部分 re-export infra 层正式 decoder。
//!
//! ## 云端 STT 的两种 API 协议
//!
//! 1. **标准 Whisper 接口**（OpenAI / Groq 等）：
//!    `POST /v1/audio/transcriptions`，multipart/form-data 上传 WAV。
//! 2. **Chat-Completion ASR**（Mimo 等）：
//!    `POST /v1/chat/completions`，JSON body 中以 base64 data-URI 嵌入音频。

#[cfg(test)]
use std::io::Write;
#[cfg(test)]
use std::path::Path;

// ── 正式 WAV decoder re-export（0.22.16）──────────────────────────────────
//
// infra 层的 `decode_wav` 支持完整 RIFF/WAVE 格式验证，替代了旧的
// `parse_wav_to_f32`（后者只在测试构建中按 16-bit/16k/mono 假设读取 data chunk）。
// 以下 re-export 供测试和未来 transcribe_audio Capability 使用。
#[allow(unused_imports)]
pub use crate::infra::platform::audio::format::{AudioDecodeError, SampleKind, SourceFormat};
#[allow(unused_imports)]
pub use crate::infra::platform::audio::wav::{decode_wav, decode_wav_with_budget, DecodedWav};

// ── WAV 编码 ─────────────────────────────────────────────────────────────

/// PCM f32 样本 → WAV 字节（16-bit PCM, little-endian）。
///
/// f32 范围 [-1.0, 1.0] → i16，超出范围的值做 clamp。
///
/// **0.22.16 加固**：`data_size` 和 `file_size` 使用 checked arithmetic，
/// 超出 u32::MAX 的 RIFF 上限时返回空 Vec（而非 panic 或 wrap）。
/// 单声道 16kHz 1 小时 ≈ 115MB，远在 u32 范围内。
pub fn pcm_to_wav(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let bits_per_sample = 16u16;
    let bytes_per_sample = (bits_per_sample / 8) as usize;
    // checked: data_size = samples.len() * 2
    let data_size = samples.len().checked_mul(bytes_per_sample);
    let data_size = match data_size {
        Some(ds) if ds <= u32::MAX as usize => ds,
        _ => {
            // 超出 RIFF 上限（~4GB），拒绝编码
            return Vec::new();
        }
    };
    // file_size = 36 + data_size (checked)
    let file_size = 36u32.checked_add(data_size as u32);
    let file_size = match file_size {
        Some(fs) => fs,
        None => return Vec::new(),
    };
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
    let block_align = channels * (bits_per_sample / 8);

    let mut wav = Vec::with_capacity(44usize.saturating_add(data_size));

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_size.to_le_bytes());
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
        let clamped = sample.clamp(-1.0, 1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        wav.extend_from_slice(&i16_sample.to_le_bytes());
    }

    wav
}

/// 将 PCM f32 样本写入 WAV 文件（16-bit, mono PCM）。
///
/// 与 [`pcm_to_wav`] 使用相同的编码逻辑，但直接写入文件而非返回字节。
/// 供诊断命令写测试音频文件使用。
#[cfg(test)]
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
        let clamped = s.clamp(-1.0, 1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        pcm.extend_from_slice(&i16_sample.to_le_bytes());
    }
    file.write_all(&pcm)
        .map_err(|e| format!("写 PCM 数据失败: {e}"))?;

    Ok(())
}

// ── WAV 解码（re-export infra 层正式 decoder，0.22.16）──────────────────
//
// 旧 `parse_wav_to_f32` 已删除——它只在测试构建中按 16-bit/16k/mono
// 假设读取 data chunk，不验证 fmt、声道、采样率或帧对齐，48kHz stereo
// PCM16 会被错误解释为 16kHz mono，时长放大六倍。
//
// 正式 decoder 位于 `infra/platform/audio/wav.rs`，通过上方 re-export 暴露。
// 需要解析 WAV 的代码请使用 `wav::decode_wav(data) -> Result<DecodedWav, AudioDecodeError>`。

// ── 供应商协议判定 ───────────────────────────────────────────────────────

/// 判断云端 STT 供应商是否使用 chat-completion ASR 协议。
///
/// - `mimo` / `mimo_plan`：使用 `POST /v1/chat/completions`，base64 音频嵌入 messages
/// - 其他（openai / groq / custom）：使用标准 `POST /v1/audio/transcriptions`
pub fn uses_chat_completion_asr(provider_kind: &str) -> bool {
    matches!(provider_kind, "mimo" | "mimo_plan")
}

// ── HTTP 转录（标准 Whisper 接口）────────────────────────────────────────

/// 异步 HTTP 转录请求（复用外部 Client）。
///
/// 与 [`transcribe_async`] 逻辑相同，但接受外部 `reqwest::Client`，
/// 适用于伪流式引擎等需要复用连接池的场景。
///
/// - `client`：复用的 HTTP 客户端
/// - `url`：完整 URL（如 `https://api.openai.com/v1/audio/transcriptions`）
/// - `api_key`：Bearer token；本地服务传 `None`
/// - `model_id`：模型标识（如 `whisper-large-v3` / `sensevoice`）
/// - `wav_bytes`：WAV 格式音频字节
///
/// 返回识别文本。HTTP 错误或 JSON 解析失败转为 [`super::SttError`]。
pub async fn transcribe_with_client(
    client: &reqwest::Client,
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

/// 异步 HTTP 转录请求（OpenAI 兼容格式，内部创建 Client）。
///
/// 便捷封装：内部创建 30s 超时的 `reqwest::Client` 后委托给 [`transcribe_with_client`]。
/// 需要复用连接池的场景（如伪流式引擎）请直接使用 [`transcribe_with_client`]。
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
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| super::SttError::Engine(format!("HTTP client 创建失败: {e}")))?;

    transcribe_with_client(&client, url, api_key, model_id, wav_bytes).await
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
        return Err(super::SttError::Engine(format!(
            "HTTP {status}: {resp_body}"
        )));
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

// ── 旧 parser 复现 48kHz stereo 误读测试 ─────────────────────────────
//
// 明确验证：旧的 `parse_wav_to_f32` 对 48kHz 双声道 PCM16 的时长误读（6 倍）
// 已被新 decoder 修复。新 `decode_wav` 正确解析声道和采样率。

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

    /// 新 decoder 的往返测试：encode → decode → 对比样本值。
    #[test]
    fn decode_wav_roundtrip_pcm16_mono() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -0.3, 0.7];
        let wav = pcm_to_wav(&samples, 16000, 1);
        let decoded = decode_wav(&wav).expect("decode 失败");

        assert_eq!(decoded.format.channels, 1);
        assert_eq!(decoded.format.sample_rate, 16000);
        assert_eq!(decoded.format.bits_per_sample, 16);
        assert_eq!(decoded.samples.len(), samples.len());
        assert!((decoded.duration_secs - 6.0 / 16000.0).abs() < 1e-10);
        for (i, (a, b)) in samples.iter().zip(decoded.samples.iter()).enumerate() {
            assert!((a - b).abs() < 1e-3, "样本 {i} 不匹配: {a} vs {b}");
        }
    }

    /// 明确复现旧 parser 对 48kHz stereo 的误读。
    ///
    /// 旧 `parse_wav_to_f32` 会把 48kHz stereo 480 帧 ×2 声道 = 960 样本
    /// × 2 bytes = 1920 bytes 当作 16kHz mono 处理，得到 960 个样本
    /// （而非 480 帧 ×2 声道），时长 = 960/16000 = 60ms 而非真实的 10ms。
    #[test]
    fn old_parser_misread_48k_stereo_reproduced() {
        use crate::infra::platform::audio::test_fixtures::*;

        // 构建 48kHz stereo PCM16, 480 帧
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 480,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);

        // 新 decoder 正确解析
        let decoded = decode_wav(&wav).expect("decode 失败");
        assert_eq!(decoded.format.channels, 2);
        assert_eq!(decoded.format.sample_rate, 48000);
        assert_eq!(decoded.num_frames, 480);
        // 时长 = 480 / 48000 = 0.01s = 10ms（旧 parser 会得到 60ms）
        assert!((decoded.duration_secs - 480.0 / 48000.0).abs() < 1e-10);
    }
}
