//! FunASR 引擎配置投影（0.22.7.4：随旧 Python 启动构造删除，只保留
//! `SttConfig.local_engine` 的 serde 投影形状）。
//!
//! 旧 `build_funasr_launch_descriptor`（venv python + blink_stt_server.py +
//! ModelScope 环境）已删除；GGUF 启动构造见 [`super::gguf`]。

use crate::domain::local_engine::AdapterConfig;

// ── FunasrEngineConfig（从 SttConfig 投影） ────────────────────────────────

/// FunASR 引擎配置（从 `SttConfig.local_engine` 投影）。
///
/// 保持已有配置 key 和 serde 形状，不做配置迁移，不改默认值。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunasrEngineConfig {
    /// 模型标识（GGUF 目录 id，如 "gguf/sensevoice-small-q8"）
    pub funasr_model: String,
    /// 推理设备: "cpu"（GGUF 首版仅 CPU profile；字段保留为兼容形状）
    pub device: String,
    /// CPU 推理线程数（None = 自动；映射 worker 的 BLINK_WORKER_THREADS）
    #[serde(default)]
    pub num_threads: Option<u32>,
    /// 热词列表（**GGUF 实现不支持热词**——字段保留为配置兼容占位，
    /// 不参与启动；能力差异已在模型目录 description 声明）
    #[serde(default)]
    pub hotwords: Option<String>,
    /// ITN 逆文本归一化（SenseVoice 内置 ITN；字段随请求传递保留）
    pub use_itn: bool,
    /// VAD 切句参数（伪流式模式生效）
    #[serde(default)]
    pub vad: VadConfigProjection,
    /// Blink 启动后自动启动服务
    #[serde(default)]
    pub auto_start_server: bool,
}

/// VAD 配置投影（保持与 SttConfig 相同的 serde 形状）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct VadConfigProjection {
    /// RMS 低于此值视为静默。
    #[serde(default)]
    pub silence_threshold: f64,
    /// 静默持续多久判定句尾。
    #[serde(default)]
    pub min_silence_ms: u32,
    /// 最小句子长度。
    #[serde(default)]
    pub min_sentence_ms: u32,
}

impl FunasrEngineConfig {
    /// 从 `SttConfig` 的 `local_engine` 配置投影。
    ///
    /// 保持已有配置 key 和 serde 形状。
    pub fn from_stt_config(local: &crate::domain::config::stt_config::LocalEngineConfig) -> Self {
        Self {
            funasr_model: local.funasr_model.clone(),
            device: local.device.clone(),
            num_threads: local.num_threads,
            hotwords: local.hotwords.clone(),
            use_itn: local.use_itn,
            vad: VadConfigProjection {
                silence_threshold: local.vad.silence_threshold,
                min_silence_ms: local.vad.min_silence_ms,
                min_sentence_ms: local.vad.min_sentence_ms,
            },
            auto_start_server: local.auto_start_server,
        }
    }

    /// 转为 `serde_json::Value` 以注入 `AdapterConfig::engine_config`。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

/// 便捷入口：从 `AdapterConfig.engine_config` 解析配置投影。
pub(crate) fn _adapter_config_marker(_: &AdapterConfig) {}
