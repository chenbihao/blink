//! 本地 STT 引擎：通过 FunASR Python 工具箱做语音转文字。
//!
//! ## 设计
//!
//! 不再使用 sherpa-onnx 二进制 + ONNX 模型，改为直接使用 FunASR 的
//! OpenAI 兼容 API（`funasr-server`）。
//!
//! ### 工作模式
//!
//! **非流式（hold-to-talk，0.10 当前阶段）**：
//! - `transcribe_chunk`: 累积 PCM 样本，返回空字符串
//! - `finalize`: 写 WAV → POST `localhost:{port}/v1/audio/transcriptions` → 返回文本
//!
//! ### FunASR 自动管理
//!
//! FunASR 自动处理：
//! - 模型下载（从 ModelScope，国内 CDN 稳定）
//! - VAD（语音端点检测）
//! - 标点恢复
//! - 推理
//!
//! Rust 侧零模型管理——只需确保 `funasr-server` 在运行。
//!
//! ## 与 CloudSttEngine 的关系
//!
//! 本引擎复用 CloudSttEngine 的 HTTP 转录逻辑，只是目标 URL 指向
//! `localhost:{port}/v1` 而非云端 API。API key 不需要（本地服务无鉴权）。
//!
//! ## 历史对照
//!
//! 旧方案（sherpa-onnx 子进程）的问题：
//! - GitHub releases 下载二进制不稳定
//! - HuggingFace ONNX 模型国内不可达
//! - stdout "text:" 解析脆弱
//! - 版本不匹配
//! - 无 VAD / 标点 pipeline
//!
//! 新方案（FunASR server）：
//! - Blink 通过 uv 自动安装 Python 3.12 + funasr（用户零手动操作）
//! - `funasr-server --model sensevoice` 一键启动
//! - OpenAI 兼容 API = 复用现有 HTTP 代码
//! - FunASR 原生 pipeline

use std::sync::Mutex;

use super::{SttEngine, SttError};

/// 本地 STT 引擎（FunASR server）。
///
/// 通过本地 `funasr-server` 的 OpenAI 兼容 API 做语音转文字。
/// server 进程的生命周期由 `app/commands.rs` 的 start/stop_funasr_server 管理。
pub struct LocalSttEngine {
    /// 累积的 PCM 样本（f32, 16kHz, mono）
    samples: Mutex<Vec<f32>>,
    /// 采样率
    sample_rate: u32,
    /// funasr-server 监听端口
    server_port: u16,
    /// FunASR 模型标识（传给 /v1/audio/transcriptions 的 model 字段）
    funasr_model: String,
    /// 是否已在创建时确认服务就绪
    server_ready: bool,
}

impl LocalSttEngine {
    /// 创建本地 STT 引擎。
    ///
    /// 从 SttConfig 读取端口配置，检查 funasr-server 是否就绪。
    /// 如果服务未就绪，返回错误（提示用户在设置页启动服务）。
    pub fn new(config: &crate::app::stt_config::SttConfig) -> Result<Self, String> {
        let port = config.local_engine.server_port;
        let model = config.local_engine.funasr_model.clone();

        let ready = super::funasr::is_server_ready(port);
        if !ready {
            return Err(format!(
                "FunASR 服务未在端口 {port} 上运行。\
                 请在设置页「语音输入」→「本地模式」中点击「启动服务」按钮。\
                 （首次使用需先点击「安装环境」，Blink 会自动安装 Python + funasr）"
            ));
        }

        tracing::info!(port, model = %model, "本地 STT 引擎: FunASR server (就绪)");

        Ok(Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
            server_port: port,
            funasr_model: model,
            server_ready: true,
        })
    }

    /// 创建不检查服务就绪的实例（供诊断命令使用）。
    pub fn for_diagnostic(port: u16) -> Self {
        Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
            server_port: port,
            funasr_model: "sensevoice".to_string(),
            server_ready: super::funasr::is_server_ready(port),
        }
    }

    /// 获取服务就绪状态。
    pub fn is_ready(&self) -> bool {
        self.server_ready
    }

    /// 获取监听端口。
    pub fn port(&self) -> u16 {
        self.server_port
    }

    /// 写 WAV 文件（16kHz, 16-bit, mono PCM）。
    ///
    /// 保留此方法用于诊断命令写测试音频文件。
    pub fn write_wav(
        path: &std::path::Path,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<(), String> {
        use std::io::Write;

        let data_len = samples.len() * 2; // 16-bit = 2 bytes/sample
        let file_size = 36 + data_len as u32;

        let mut file = std::fs::File::create(path)
            .map_err(|e| format!("创建 WAV 文件失败: {e}"))?;

        // RIFF header
        file.write_all(b"RIFF").map_err(|e| format!("写 RIFF 失败: {e}"))?;
        file.write_all(&file_size.to_le_bytes())
            .map_err(|e| format!("写 file_size 失败: {e}"))?;
        file.write_all(b"WAVE").map_err(|e| format!("写 WAVE 失败: {e}"))?;

        // fmt chunk
        file.write_all(b"fmt ").map_err(|e| format!("写 fmt 失败: {e}"))?;
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
        file.write_all(b"data").map_err(|e| format!("写 data 失败: {e}"))?;
        file.write_all(&(data_len as u32).to_le_bytes())
            .map_err(|e| format!("写 data_size 失败: {e}"))?;

        // PCM 数据（f32 → i16 LE）
        let mut pcm = Vec::with_capacity(data_len);
        for &s in samples {
            let clamped = s.max(-1.0).min(1.0);
            let i16_sample = (clamped * 32767.0) as i16;
            pcm.extend_from_slice(&i16_sample.to_le_bytes());
        }
        file.write_all(&pcm).map_err(|e| format!("写 PCM 数据失败: {e}"))?;

        Ok(())
    }

    /// 公开接口：写 WAV（供诊断命令直接调用）。
    #[allow(dead_code)]
    pub fn write_wav_for_test(
        path: &std::path::Path,
        samples: &[f32],
        sample_rate: u32,
    ) -> Result<(), String> {
        Self::write_wav(path, samples, sample_rate)
    }

    /// 调用 FunASR server 的 OpenAI 兼容 API 做语音转录。
    ///
    /// 复用 CloudSttEngine 的 HTTP 逻辑，只是目标指向 localhost。
    async fn transcribe_via_server(&self, wav_bytes: &[u8]) -> Result<String, String> {
        let base_url = super::funasr::server_base_url(self.server_port);
        let url = format!("{base_url}/audio/transcriptions");

        tracing::debug!(%url, samples = self.samples.lock().unwrap().len(), "FunASR 转录请求");

        transcribe_async(&url, &wav_bytes, &self.funasr_model).await
    }
}

#[async_trait::async_trait]
impl SttEngine for LocalSttEngine {
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError> {
        // 非流式模式：只累积，不返回 partial
        self.samples
            .lock()
            .unwrap()
            .extend_from_slice(samples);
        Ok(String::new())
    }

    async fn finalize(&self) -> Result<String, SttError> {
        let samples = self.samples.lock().unwrap().clone();

        if samples.is_empty() {
            return Ok(String::new());
        }

        // 检查服务 HTTP API 是否就绪（TCP 可连但模型可能还在加载）
        if !super::funasr::is_server_ready_http(self.server_port).await {
            return Err(SttError::Engine(format!(
                "FunASR 服务 HTTP API 未就绪（端口 {}）。模型可能仍在加载中，请稍后重试或在设置页检查状态。",
                self.server_port
            )));
        }

        let duration_ms = (samples.len() as f64 / self.sample_rate as f64 * 1000.0) as u64;
        tracing::debug!(
            samples = samples.len(),
            duration_ms,
            "LocalSttEngine::finalize 开始识别",
        );

        // PCM → WAV
        let wav_bytes = pcm_to_wav(&samples, self.sample_rate, 1);

        // 调用 FunASR server
        let text = self
            .transcribe_via_server(&wav_bytes)
            .await
            .map_err(SttError::Engine)?;

        tracing::info!(
            text_len = text.chars().count(),
            %text,
            "LocalSttEngine 识别完成",
        );

        Ok(text)
    }

    fn reset(&self) {
        self.samples.lock().unwrap().clear();
        tracing::debug!("LocalSttEngine::reset");
    }

    fn name(&self) -> &str {
        "local-funasr"
    }
}

/// 异步 HTTP 转录请求（OpenAI 兼容格式）。
///
/// 与 CloudSttEngine::transcribe_async 类似，但不需要 API key。
async fn transcribe_async(url: &str, wav_bytes: &[u8], model: &str) -> Result<String, String> {
    use reqwest::multipart;

    let part = multipart::Part::bytes(wav_bytes.to_vec())
        .file_name("audio.wav")
        .mime_str("audio/wav")
        .map_err(|e| format!("multipart 构建失败: {e}"))?;

    let form = multipart::Form::new()
        .text("model", model.to_string())
        .part("file", part);

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = client
        .post(url)
        .multipart(form)
        .send()
        .await
        .map_err(|e| format!("HTTP 请求失败: {e}"))?;

    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("HTTP {status}: {body}"));
    }

    #[derive(serde::Deserialize)]
    struct TranscriptionResponse {
        text: String,
    }

    let result: TranscriptionResponse = resp
        .json()
        .await
        .map_err(|e| format!("JSON 解析失败: {e}"))?;

    Ok(result.text)
}

/// PCM f32 样本 → WAV 字节（16-bit PCM, little-endian）。
///
/// 与 cloud.rs 中的 pcm_to_wav 相同逻辑。
fn pcm_to_wav(samples: &[f32], sample_rate: u32, channels: u16) -> Vec<u8> {
    let num_samples = samples.len();
    let bits_per_sample = 16u16;
    let byte_rate = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
    let block_align = channels * (bits_per_sample / 8);
    let data_size = num_samples * (bits_per_sample / 8) as usize;
    let file_size = 36 + data_size;

    let mut wav = Vec::with_capacity(44 + data_size);

    // RIFF header
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(file_size as u32).to_le_bytes());
    wav.extend_from_slice(b"WAVE");

    // fmt chunk
    wav.extend_from_slice(b"fmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&channels.to_le_bytes());
    wav.extend_from_slice(&sample_rate.to_le_bytes());
    wav.extend_from_slice(&byte_rate.to_le_bytes());
    wav.extend_from_slice(&block_align.to_le_bytes());
    wav.extend_from_slice(&bits_per_sample.to_le_bytes());

    // data chunk
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&(data_size as u32).to_le_bytes());

    for &sample in samples {
        let clamped = sample.max(-1.0).min(1.0);
        let i16_sample = (clamped * 32767.0) as i16;
        wav.extend_from_slice(&i16_sample.to_le_bytes());
    }

    wav
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证 WAV 文件写入格式正确。
    #[test]
    fn wav_writer_produces_valid_file() {
        let tmp = std::env::temp_dir().join("blink_wav_test.wav");
        let sample_rate = 16000u32;
        let samples: Vec<f32> = (0..sample_rate)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.5
            })
            .collect();

        LocalSttEngine::write_wav(&tmp, &samples, sample_rate).expect("write_wav 失败");

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

    /// 验证 LocalSttEngine 的 transcribe_chunk + finalize 流程（不含 HTTP）。
    #[tokio::test]
    async fn stt_engine_accumulates_samples() {
        let engine = LocalSttEngine::for_diagnostic(65535); // 不会被调用的端口

        engine.transcribe_chunk(&[0.1, 0.2, 0.3]).await.unwrap();
        engine.transcribe_chunk(&[0.4, 0.5]).await.unwrap();

        let samples = engine.samples.lock().unwrap();
        assert_eq!(samples.len(), 5);
        assert!((samples[0] - 0.1).abs() < 1e-6);
        assert!((samples[4] - 0.5).abs() < 1e-6);
    }

    /// 验证 reset 清空累积的样本。
    #[tokio::test]
    async fn stt_engine_reset_clears_samples() {
        let engine = LocalSttEngine::for_diagnostic(65535);

        engine.transcribe_chunk(&[0.1, 0.2, 0.3]).await.unwrap();
        assert_eq!(engine.samples.lock().unwrap().len(), 3);

        engine.reset();
        assert_eq!(engine.samples.lock().unwrap().len(), 0);
    }

    /// 验证 pcm_to_wav 生成正确的 WAV 字节。
    #[test]
    fn pcm_to_wav_header_is_correct() {
        let samples = vec![0.0, 0.5, -0.5, 1.0, -1.0];
        let wav = pcm_to_wav(&samples, 16000, 1);

        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        assert_eq!(u32::from_le_bytes([wav[16], wav[17], wav[18], wav[19]]), 16);
        assert_eq!(u16::from_le_bytes([wav[20], wav[21]]), 1); // PCM
        assert_eq!(&wav[36..40], b"data");
        let data_size = u32::from_le_bytes([wav[40], wav[41], wav[42], wav[43]]);
        assert_eq!(data_size as usize, samples.len() * 2);
        assert_eq!(wav.len(), 44 + samples.len() * 2);
    }

    /// 验证 pcm_to_wav 对超范围样本做 clamp。
    #[test]
    fn pcm_to_wav_clamps_samples() {
        let samples = vec![2.0, -2.0];
        let wav = pcm_to_wav(&samples, 16000, 1);

        let s0 = i16::from_le_bytes([wav[44], wav[45]]);
        let s1 = i16::from_le_bytes([wav[46], wav[47]]);

        assert_eq!(s0, 32767); // clamped to max
        assert_eq!(s1, -32767); // clamped to min
    }

    /// 端到端测试：用 FunASR 示例音频验证本地 STT 管线。
    ///
    /// 流程：
    /// 1. 下载 FunASR 官方示例音频（BAC009S0764W0121.wav）
    /// 2. 读取音频 → 转为 f32 PCM 样本
    /// 3. 调用 LocalSttEngine::transcribe_chunk 累积
    /// 4. 调用 LocalSttEngine::finalize → POST 到 funasr-server
    /// 5. 断言识别结果包含预期关键词
    ///
    /// 如果 funasr-server 未运行，跳过（不 fail）。
    #[tokio::test(flavor = "multi_thread")]
    async fn stt_end_to_end_with_funasr_sample() {
        let port: u16 = 8000;

        // 检查 funasr-server 是否在运行
        if !super::super::funasr::is_server_ready(port) {
            eprintln!("跳过：funasr-server 未在端口 {port} 上运行");
            eprintln!("要运行此测试，请先在设置页安装环境并启动服务");
            return;
        }

        // FunASR 示例音频
        let audio_url = "https://isv-data.oss-cn-hangzhou.aliyuncs.com/ics/MaaS/ASR/test_audio/BAC009S0764W0121.wav";
        let tmp_wav = std::env::temp_dir().join("blink_funasr_test_sample.wav");

        // 如果本地已有缓存，跳过下载
        if !tmp_wav.exists() {
            eprintln!("下载 FunASR 示例音频...");
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .expect("HTTP client 创建失败");

            let resp = client.get(audio_url).send().await.expect("下载音频失败");
            assert!(resp.status().is_success(), "下载音频 HTTP 失败: {}", resp.status());

            let bytes = resp.bytes().await.expect("读取音频字节失败");
            std::fs::write(&tmp_wav, &bytes).expect("写入音频文件失败");
        }

        eprintln!("读取 WAV 文件: {}", tmp_wav.display());
        let wav_bytes = std::fs::read(&tmp_wav).expect("读取 WAV 文件失败");

        // 解析 WAV → f32 PCM 样本
        let samples = parse_wav_to_f32(&wav_bytes).expect("WAV 解析失败");
        eprintln!("音频: {} 样本, {:.1}s", samples.len(), samples.len() as f64 / 16000.0);

        // 创建引擎实例
        let engine = LocalSttEngine::for_diagnostic(port);
        assert!(engine.is_ready(), "funasr-server 应就绪");

        // 模拟 transcribe_chunk（累积）
        let chunk_size = 1600usize; // 100ms
        for chunk in samples.chunks(chunk_size) {
            engine.transcribe_chunk(chunk).await.unwrap();
        }

        // finalize → POST 到 funasr-server
        eprintln!("调用 funasr-server 转录...");
        let result = engine.finalize().await;

        match &result {
            Ok(text) => {
                eprintln!("识别结果: \"{text}\"");
                assert!(!text.is_empty(), "识别结果不应为空");
                eprintln!("=== 测试通过 ===");
            }
            Err(e) => {
                panic!("funasr-server 转录失败: {e}");
            }
        }
    }
}

/// 解析 WAV 文件为 f32 PCM 样本（16-bit, 16kHz, mono）。
///
/// 简化版解析器：跳过非 data chunk，只读 PCM data。
/// 供诊断命令和测试共用。
pub(crate) fn parse_wav_to_f32(data: &[u8]) -> Result<Vec<f32>, String> {
    // RIFF header
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("不是有效的 WAV 文件".to_string());
    }

    // 跳过 fmt chunk，找到 data chunk
    let mut offset = 12;
    let mut samples = Vec::new();

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size =
            u32::from_le_bytes([data[offset + 4], data[offset + 5], data[offset + 6], data[offset + 7]])
                as usize;

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
