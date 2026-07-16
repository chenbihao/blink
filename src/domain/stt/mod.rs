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
//! - **本地伪流式**: PseudoStreamingSttEngine (VAD 切句 + 定时 HTTP 预览)
//! - **本地非流式**: LocalSttEngine (HTTP 一次性识别)
//!
//! 0.10.4 起，真流式（WebSocket + Paraformer-streaming）已移除——
//! 伪流式在准确率、标点、体积、CPU 友好度上全面优于真流式，且 Python 侧零改动。

use std::fmt;

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
/// finalize 时一次性 HTTP 请求返回;伪流式引擎在 chunk 中实时返回 partial。
#[async_trait::async_trait]
pub trait SttEngine: Send + Sync {
    /// 接收一段音频 chunk,返回当前累积识别的 partial text。
    ///
    /// 伪流式模式下每次调用返回逐步完善的文本;
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
#[derive(Debug, Clone)]
pub struct ModelDescriptor {
    /// 模型 id(唯一标识,如 "sensevoice-small")
    pub id: &'static str,
    /// 显示名称
    pub display_name: &'static str,
    /// 引擎名("funasr")
    pub engine: &'static str,
    /// FunASR 模型标识(传给 funasr-server --model 参数)
    pub funasr_model_id: &'static str,
    /// 参数量（如 "234M" / "220M"），来自 FunASR 官方文档
    pub params: &'static str,
    /// 模型下载体积(MB,FP32 PyTorch 格式近似值)
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
        params: "234M",
        size_mb: 234,
        languages: &["zh", "en", "ja", "ko", "yue"],
        device: "cpu",
        description: "五语种 ASR（中/英/日/韩/粤），CPU 17 倍实时，带情感与音频事件标签。体积小、速度快，推荐 CPU 首选",
    },
    ModelDescriptor {
        id: "paraformer-zh",
        display_name: "FunASR Paraformer-zh (SeacoParaformer)",
        engine: "funasr",
        funasr_model_id: "paraformer-zh",
        params: "220M",
        size_mb: 880 + 1130,
        languages: &["zh", "en"],
        device: "cpu",
        description: "纯中文非流式 ASR（SeacoParaformer），原生支持热词 boosting 与 ITN。自动配置 VAD(fsmn-vad) + 标点(ct-punc) 子模型。整句准确率高于 SenseVoice，不会幻觉英文语气词",
    },
];

/// 按 id 查找模型描述符。
pub fn find_model(id: &str) -> Option<&'static ModelDescriptor> {
    model_registry().iter().find(|m| m.id == id)
}

// ── STT Engines ──────────────────────────────────────────────────────────

mod cloud;
pub mod funasr;
pub mod local;
#[cfg(test)]
mod mock;
pub mod pseudo_streaming;
pub mod vad;
pub(crate) mod wav;

/// 创建 STT 引擎实例(工厂函数)。
///
/// 根据 SttConfig 选择引擎：
/// - Cloud 模式 + 已配置 cloud_provider → CloudSttEngine
/// - Local 模式 + StreamingMode::Pseudo → PseudoStreamingSttEngine (VAD + 预览) ⭐ 默认
/// - Local 模式 + StreamingMode::Off → LocalSttEngine (HTTP 非流式)
///
/// **不会回退到 Mock 引擎**——未启用 / 未配置 / 服务未就绪时返回 Err，
/// 由调用方（VoiceService）决定如何向用户反馈错误。
pub fn create_engine() -> Result<Box<dyn SttEngine>, String> {
    let config = crate::app::stt_config::get_stt_config();

    if !config.enabled {
        return Err("STT 未启用，请在设置页开启语音输入".to_string());
    }

    match config.mode {
        crate::app::stt_config::SttMode::Cloud => {
            if config.cloud_provider.is_some() {
                tracing::info!("STT 引擎: cloud");
                Ok(Box::new(cloud::CloudSttEngine::new()))
            } else {
                Err("云端 STT 未配置供应商，请在设置页中配置".to_string())
            }
        }
        crate::app::stt_config::SttMode::Local => {
            match config.streaming_mode {
                crate::app::stt_config::StreamingMode::Pseudo => {
                    match pseudo_streaming::PseudoStreamingSttEngine::new(&config) {
                        Ok(engine) => {
                            tracing::info!("STT 引擎: pseudo-streaming (VAD + HTTP 轮询)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => {
                            tracing::warn!(%e, "STT 伪流式引擎创建失败,回退非流式");
                            match local::LocalSttEngine::new(&config) {
                                Ok(engine) => {
                                    tracing::info!("STT 引擎: local (FunASR, 非流式回退)");
                                    Ok(Box::new(engine))
                                }
                                Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                            }
                        }
                    }
                }
                crate::app::stt_config::StreamingMode::Off => {
                    match local::LocalSttEngine::new(&config) {
                        Ok(engine) => {
                            tracing::info!("STT 引擎: local (FunASR)");
                            Ok(Box::new(engine))
                        }
                        Err(e) => Err(format!("STT 引擎创建失败: {e}")),
                    }
                }
            }
        }
    }
}

/// Mock STT 引擎的假文本库(按时间轮换,模拟"边说边出字")。
///
/// 仅供 `#[cfg(test)]` 的 MockSttEngine 使用，不参与生产路径。
#[cfg(test)]
pub fn mock_text_for_elapsed(elapsed: std::time::Duration) -> &'static str {
    match elapsed.as_secs() {
        0..=1 => "",
        2 => "你好",
        3 => "你好世界",
        4 => "你好世界这是一段",
        5 => "你好世界这是一段测试语音",
        _ => "你好世界这是一段测试语音识别的文字结果",
    }
}
