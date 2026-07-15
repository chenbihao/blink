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
//! 本引擎复用 [`super::wav`] 的 HTTP 转录逻辑，只是目标 URL 指向
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
    #[allow(dead_code)]
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
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.server_ready
    }

    /// 获取监听端口。
    #[allow(dead_code)]
    pub fn port(&self) -> u16 {
        self.server_port
    }

    /// 调用 FunASR server 的 OpenAI 兼容 API 做语音转录。
    ///
    /// 复用 [`super::wav::transcribe_async`]，只是目标指向 localhost 且无需 API key。
    async fn transcribe_via_server(&self, wav_bytes: &[u8]) -> Result<String, String> {
        let base_url = super::funasr::server_base_url(self.server_port);
        let url = format!("{base_url}/audio/transcriptions");

        tracing::debug!(%url, samples = self.samples.lock().unwrap().len(), "FunASR 转录请求");

        super::wav::transcribe_async(&url, None, &self.funasr_model, wav_bytes)
            .await
            .map_err(|e| e.to_string())
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
        let wav_bytes = super::wav::pcm_to_wav(&samples, self.sample_rate, 1);

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

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
        let samples = super::super::wav::parse_wav_to_f32(&wav_bytes).expect("WAV 解析失败");
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
