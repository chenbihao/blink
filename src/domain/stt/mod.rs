//! STT(语音转文字)领域层。
//!
//! 0.10 语音输入:hold-to-talk 期间采集音频 → STT 识别 → 文字上屏。
//!
//! ## 架构
//!
//! ```text
//! AudioCapture (infra)          SttEngine (domain)
//!    ↓ audio chunks                ↑ audio chunks
//!    │                             │
//!    └─── VoiceService ────────────┘
//!              ↓ partial text
//!              ↓ final text
//!         G1: emit chord-fill-query
//!         G2: inject_text()
//! ```
//!
//! ## SttEngine trait
//!
//! - `transcribe_chunk`: 接收一段音频,返回当前累积的 partial text
//! - `finalize`: 录音结束,返回最终 text
//! - `reset`: 新录音会话前重置状态
//!
//! ## 引擎选型
//!
//! - **云端**: CloudSttEngine (OpenAI 兼容 API,走 reqwest)
//! - **本地**: LocalSttEngine (FunASR Python 工具箱, funasr-server OpenAI 兼容 API)
//!
//! 旧方案使用 sherpa-onnx（C++ ONNX 子进程），因下载不稳定、模型格式
//! 不匹配等问题已废弃。新方案直接使用 FunASR Python 工具箱：
//! - Blink 通过 uv 自动管理 Python 3.12 + funasr 安装（用户零手动操作）
//! - `funasr-server --model iic/SenseVoiceSmall` 启动
//! - 通过 OpenAI 兼容 API (localhost:8000) 做转录
//! - FunASR 自动管理模型下载（ModelScope CDN）、VAD、标点
//!
//! 0.10.3 真流式：自定义 `blink_stt_server.py`，同时支持 HTTP 非流式 + WebSocket 流式。

use std::fmt;
use std::time::Duration;

// ── 错误类型 ─────────────────────────────────────────────────────────────

/// STT 识别错误。
#[derive(Debug)]
#[allow(dead_code)]
pub enum SttError {
    /// 引擎未初始化
    NotInitialized,
    /// 识别过程中出错
    Engine(String),
    /// 音频格式不匹配
    FormatMismatch(String),
}

impl fmt::Display for SttError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SttError::NotInitialized => write!(f, "STT engine not initialized"),
            SttError::Engine(msg) => write!(f, "STT engine error: {msg}"),
            SttError::FormatMismatch(msg) => write!(f, "audio format mismatch: {msg}"),
        }
    }
}

impl std::error::Error for SttError {}

// ── STT Engine trait ─────────────────────────────────────────────────────

/// STT 引擎 trait。
///
/// 生命周期: `reset` → 多次 `transcribe_chunk` → `finalize` → (下次) `reset`。
///
/// `transcribe_chunk` 和 `finalize` 为 async——非流式引擎在 chunk 中累积音频,
/// finalize 时一次性 HTTP 请求返回;流式引擎(0.10.3+)在 chunk 中实时返回 partial。
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 接收一段音频 chunk,返回当前累积识别的 partial text。
    ///
    /// 流式模式下每次调用返回逐步完善的文本;
    /// 非流式模式下可以累积音频,在 `finalize` 时一次性返回。
    async fn transcribe_chunk(&self, samples: &[f32]) -> Result<String, SttError>;

    /// 录音结束,返回最终识别文本。
    async fn finalize(&self) -> Result<String, SttError>;

    /// 重置引擎状态(新录音会话前调用)。
    fn reset(&self);

    /// 引擎显示名称(日志/调试用)。
    #[allow(dead_code)]
    fn name(&self) -> &str;
}

// ── 模型注册表 ───────────────────────────────────────────────────────────

/// FunASR 模型描述符。
///
/// 描述 FunASR 工具箱支持的模型，用于前端展示和配置引导。
/// 与旧方案不同：不再涉及 ONNX 文件下载，模型由 FunASR 自动管理。
#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    /// 模型 id(唯一标识,如 "sensevoice-small")
    pub id: &'static str,
    /// 显示名称
    pub display_name: &'static str,
    /// 引擎名("funasr")
    pub engine: &'static str,
    /// FunASR 模型标识(传给 funasr-server --model 参数)
    /// 如 "iic/SenseVoiceSmall" / "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online"
    pub funasr_model_id: &'static str,
    /// 是否支持流式（指模型本身的能力，非当前 funasr-server HTTP 模式是否流式）
    pub streaming: bool,
    /// 参数量（如 "234M" / "220M"），来自 FunASR 官方文档
    /// 注意：这是模型参数数量，不是文件下载大小
    pub params: &'static str,
    /// 模型下载体积(MB,FP32 PyTorch 格式近似值)
    /// 实际下载由 FunASR 自动完成，含 ASR + VAD + 标点模型
    pub size_mb: u32,
    /// 支持的语言
    pub languages: &'static [&'static str],
    /// 设备要求("cpu" 或 "cuda")
    pub device: &'static str,
    /// 简短描述
    pub description: &'static str,
}

/// 预置模型注册表。
///
/// 模型来自 FunASR（modelscope/阿里达摩院）。
/// FunASR 自动从 ModelScope 下载模型（国内 CDN，稳定）。
pub fn model_registry() -> &'static [ModelDescriptor] {
    &MODELS
}

static MODELS: [ModelDescriptor; 2] = [
    ModelDescriptor {
        id: "sensevoice-small",
        display_name: "FunASR SenseVoice-Small",
        engine: "funasr",
        funasr_model_id: "iic/SenseVoiceSmall",
        streaming: false,
        params: "234M",
        size_mb: 234,
        languages: &["zh", "en", "ja", "ko", "yue"],
        device: "cpu",
        description: "五语种 ASR（中/英/日/韩/粤），CPU 17 倍实时，带情感与音频事件标签。体积小、速度快，推荐 CPU 首选",
    },
    ModelDescriptor {
        id: "paraformer-zh-streaming",
        display_name: "FunASR Paraformer-zh-streaming",
        engine: "funasr",
        funasr_model_id: "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online",
        streaming: true,
        params: "220M",
        size_mb: 880,
        languages: &["zh", "en"],
        device: "cuda",
        description: "中英双语流式 ASR，chunk_size=[0,10,5]（600ms 块），延迟 ~860ms。GPU 推荐。同样支持非流式调用（is_final=True）",
    },
];

/// 按 id 查找模型描述符。
pub fn find_model(id: &str) -> Option<&'static ModelDescriptor> {
    model_registry().iter().find(|m| m.id == id)
}

// ── 模型下载状态（保留用于前端兼容，实际由 FunASR 管理）──────────────

/// 模型下载状态(运行时,不持久化)。
///
/// 注：新方案中模型由 FunASR 自动管理，此枚举仅用于前端 API 兼容。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum DownloadStatus {
    /// 未下载
    NotDownloaded,
    /// 下载中
    Downloading,
    /// 已下载(可用)
    Downloaded,
    /// 下载失败
    Failed,
}

/// 模型下载进度(通过 Tauri event 通知前端)。
///
/// 注：新方案中模型由 FunASR 自动管理，此结构仅用于前端 API 兼容。
#[derive(Debug, Clone, serde::Serialize)]
#[allow(dead_code)]
pub struct DownloadProgress {
    /// 模型 id
    pub model_id: String,
    /// 已下载字节数
    pub downloaded_bytes: u64,
    /// 总字节数(0 = 未知)
    pub total_bytes: u64,
    /// 下载速度(bytes/sec)
    pub speed: f64,
}

impl DownloadProgress {
    /// 进度百分比(0.0 ~ 1.0)。total=0 时返回 0。
    #[allow(dead_code)]
    pub fn percent(&self) -> f64 {
        if self.total_bytes == 0 {
            0.0
        } else {
            self.downloaded_bytes as f64 / self.total_bytes as f64
        }
    }
}

// ── STT Engines ──────────────────────────────────────────────────────────

mod cloud;
pub mod funasr;
pub mod local;
mod mock;
pub mod streaming;
pub(crate) mod wav;

/// 创建 STT 引擎实例(工厂函数)。
///
/// 根据 SttConfig 选择引擎：
/// - Cloud 模式 + 已配置 cloud_provider → CloudSttEngine
/// - Local 模式 + streaming=true + streaming_model 已配置 → StreamingSttEngine (WebSocket)
/// - Local 模式 + streaming=false → LocalSttEngine (HTTP 非流式)
/// - 未配置/未启用/服务未就绪 → MockSttEngine
pub fn create_engine() -> Box<dyn SttEngine> {
    let config = crate::app::stt_config::get_stt_config();

    if !config.enabled {
        return Box::new(mock::MockSttEngine::new());
    }

    match config.mode {
        crate::app::stt_config::SttMode::Cloud => {
            if config.cloud_provider.is_some() {
                tracing::info!("STT 引擎: cloud");
                Box::new(cloud::CloudSttEngine::new())
            } else {
                tracing::warn!("STT cloud 模式但未配置 provider,回退 mock");
                Box::new(mock::MockSttEngine::new())
            }
        }
        crate::app::stt_config::SttMode::Local => {
            // 0.10.3: streaming=true 且配置了 streaming_model → 流式引擎
            if config.streaming && config.local_engine.streaming_model.is_some() {
                match streaming::StreamingSttEngine::new(config.local_engine.server_port) {
                    Ok(engine) => {
                        tracing::info!("STT 引擎: streaming (WebSocket)");
                        Box::new(engine)
                    }
                    Err(e) => {
                        tracing::warn!(%e, "STT streaming 引擎创建失败,回退非流式");
                        // 回退到非流式
                        match local::LocalSttEngine::new(&config) {
                            Ok(engine) => Box::new(engine),
                            Err(e) => {
                                tracing::warn!(%e, "STT local 引擎创建失败,回退 mock");
                                Box::new(mock::MockSttEngine::new())
                            }
                        }
                    }
                }
            } else {
                match local::LocalSttEngine::new(&config) {
                    Ok(engine) => {
                        tracing::info!("STT 引擎: local (FunASR)");
                        Box::new(engine)
                    }
                    Err(e) => {
                        tracing::warn!(%e, "STT local 引擎创建失败,回退 mock");
                        Box::new(mock::MockSttEngine::new())
                    }
                }
            }
        }
    }
}

/// Mock STT 引擎的假文本库(按时间轮换,模拟"边说边出字")。
pub fn mock_text_for_elapsed(elapsed: Duration) -> &'static str {
    match elapsed.as_secs() {
        0..=1 => "",
        2 => "你好",
        3 => "你好世界",
        4 => "你好世界这是一段",
        5 => "你好世界这是一段测试语音",
        _ => "你好世界这是一段测试语音识别的文字结果",
    }
}
