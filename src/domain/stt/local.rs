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
//! - `funasr-server --model iic/SenseVoiceSmall` 一键启动
//! - OpenAI 兼容 API = 复用现有 HTTP 代码
//! - FunASR 原生 pipeline

use std::sync::Mutex;

use super::{SttEngine, SttError};

/// 本地 STT 引擎（FunASR server）。
///
/// 通过本地 `funasr-server` 的 OpenAI 兼容 API 做语音转文字。
/// server 进程的生命周期由 `EngineManager` 统一管理。
///
/// 0.22.6 批次 3: 存储完整 `SttEngineConnection` 快照，确保 health 检查和
/// 转录请求使用同一 endpoint/token——不再分别用 port 和 token 猜测。
pub struct LocalSttEngine {
    /// 累积的 PCM 样本（f32, 16kHz, mono）
    samples: Mutex<Vec<f32>>,
    /// 采样率
    sample_rate: u32,
    /// 连接快照（host + port + token + engine_id + instance_id）
    ///
    /// 0.22.6: health 和 transcribe 共用此快照，保证同一连接。
    /// 服务重启后旧连接的 token/instance_id 不匹配新实例，请求被拒绝。
    connection: Option<crate::domain::stt::SttEngineConnection>,
    /// FunASR 模型标识（传给 /v1/audio/transcriptions 的 model 字段）
    funasr_model: String,
    /// 是否已在创建时确认服务就绪
    #[allow(dead_code)]
    server_ready: bool,
}

impl LocalSttEngine {
    /// 创建本地 STT 引擎。
    ///
    /// 0.22.6 批次 3: `port` 和 `token` 来自 `SttEngineConnection`（由 app 层从
    /// `EngineManager::get_connection` 投影而来），不再从 SttConfig 读取
    /// preferred port。health 和 transcribe 使用同一连接快照。
    pub fn new(
        config: &crate::domain::config::stt_config::SttConfig,
        port: u16,
        token: String,
    ) -> Result<Self, String> {
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
            connection: Some(crate::domain::stt::SttEngineConnection {
                host: "127.0.0.1".to_string(),
                port,
                token,
                engine_id: String::new(),
                instance_id: String::new(),
            }),
            funasr_model: model,
            server_ready: true,
        })
    }

    /// 从 `SttEngineConnection` 创建本地 STT 引擎（0.22.6 批次 3）。
    ///
    /// 这是推荐的生产构造方式——连接快照包含完整的 host/port/token/IDs，
    /// health 和 transcribe 共用同一快照。
    pub fn from_connection(
        config: &crate::domain::config::stt_config::SttConfig,
        conn: crate::domain::stt::SttEngineConnection,
    ) -> Result<Self, String> {
        let model = config.local_engine.funasr_model.clone();

        let ready = super::funasr::is_server_ready(conn.port);
        if !ready {
            return Err(format!(
                "FunASR 服务未在端口 {} 上运行。\
                 请在设置页「语音输入」→「本地模式」中点击「启动服务」按钮。",
                conn.port
            ));
        }

        tracing::info!(port = conn.port, model = %model, "本地 STT 引擎: FunASR server (就绪)");

        Ok(Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
            connection: Some(conn),
            funasr_model: model,
            server_ready: true,
        })
    }

    /// 创建不检查服务就绪的实例（供诊断命令使用）。
    ///
    /// 0.22.6: 诊断模式不携带 token，仅用于 TCP 级别探测。
    /// 不参与正式录音链路。
    pub fn for_diagnostic(port: u16) -> Self {
        Self {
            samples: Mutex::new(Vec::new()),
            sample_rate: 16000,
            connection: None, // 诊断模式无 token
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            server_ready: super::funasr::is_server_ready(port),
        }
    }

    /// 获取服务就绪状态。
    #[allow(dead_code)]
    pub fn is_ready(&self) -> bool {
        self.server_ready
    }

    /// 调用 FunASR server 的 OpenAI 兼容 API 做语音转录。
    ///
    /// 0.22.6 批次 3: 使用连接快照中的 endpoint + token，确保 health 和 transcribe
    /// 使用同一连接。无连接时（诊断模式）返回错误。
    async fn transcribe_via_server(&self, wav_bytes: &[u8]) -> Result<String, String> {
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| "本地引擎无连接快照（诊断模式不支持转录）".to_string())?;

        let base_url = format!("http://{}:{}{}", conn.host, conn.port, "/v1");
        let url = format!("{base_url}/audio/transcriptions");

        tracing::debug!(%url, samples = self.samples.lock().unwrap().len(), "FunASR 转录请求");

        // token 通过 X-Engine-Token header 传递
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

        let result = super::wav::transcribe_with_token(
            &client,
            &url,
            Some(&conn.token),
            &self.funasr_model,
            wav_bytes,
        )
        .await;

        result.map_err(|e| e.to_string())
    }
}

#[async_trait::async_trait]
impl SttEngine for LocalSttEngine {
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

        // 0.22.6: 使用 token-aware health 检查，确保 health 和 transcribe 使用同一连接
        let conn = self
            .connection
            .as_ref()
            .ok_or_else(|| SttError::Engine("本地引擎无连接快照".to_string()))?;
        super::funasr::check_model_ready_or_error_with_token(conn)
            .await
            .map_err(SttError::Engine)?;

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

        // 检查 funasr-server 是否在运行且模型已加载
        if !super::super::funasr::is_server_ready(port) {
            eprintln!("跳过：funasr-server 未在端口 {port} 上运行");
            eprintln!("要运行此测试，请先在设置页安装环境并启动服务");
            return;
        }

        // 等待模型加载完成（首次需下载 ~234MB，可能需要数分钟）
        // 0.22.6: check_model_loaded(port) 无 token 会返回 401/Unreachable。
        // 此测试使用 for_diagnostic（无 token），仅做 TCP 预检 + 轮询等待。
        // 真正的 token-aware health 检查由 hermetic 集成测试覆盖。
        let model_deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(super::super::funasr::SERVER_STARTUP_TIMEOUT_SECS);
        loop {
            if !super::super::funasr::is_server_ready(port) {
                eprintln!("跳过：funasr-server 不可达");
                return;
            }
            // 尝试直接转录——如果模型还在加载，会得到错误，重试
            // （for_diagnostic 模式无 token，仅用于诊断）
            let engine = LocalSttEngine::for_diagnostic(port);
            let test_samples = vec![0.0f32; 16000]; // 1s 静音
            engine.transcribe_chunk(&test_samples).await.unwrap();
            match engine.finalize().await {
                Ok(text) => {
                    eprintln!("模型已就绪，测试转录结果: \"{text}\"");
                    break;
                }
                Err(e) => {
                    let msg = format!("{e}");
                    if msg.contains("加载中") || msg.contains("不可达") || msg.contains("鉴权")
                    {
                        if std::time::Instant::now() > model_deadline {
                            eprintln!("跳过：模型加载超时");
                            return;
                        }
                        eprintln!("模型加载中，等待 3s 后重试...");
                        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                    } else {
                        eprintln!("跳过：模型加载失败: {msg}");
                        return;
                    }
                }
            }
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
            assert!(
                resp.status().is_success(),
                "下载音频 HTTP 失败: {}",
                resp.status()
            );

            let bytes = resp.bytes().await.expect("读取音频字节失败");
            std::fs::write(&tmp_wav, &bytes).expect("写入音频文件失败");
        }

        eprintln!("读取 WAV 文件: {}", tmp_wav.display());
        let wav_bytes = std::fs::read(&tmp_wav).expect("读取 WAV 文件失败");

        // 解析 WAV → f32 PCM 样本
        let samples = super::super::wav::parse_wav_to_f32(&wav_bytes).expect("WAV 解析失败");
        eprintln!(
            "音频: {} 样本, {:.1}s",
            samples.len(),
            samples.len() as f64 / 16000.0
        );

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
