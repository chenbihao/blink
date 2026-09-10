//! 一次性文件转写领域契约（0.22.16 Handoff 04）。
//!
//! 定义 `AudioTranscriptionRequest`、`AudioTranscriptionResult`、
//! `AudioTranscriptionError` 和窄 `AudioTranscriptionPort`。
//!
//! **设计约束**：
//! - domain 层框架无关，不依赖 Tauri/文件系统。
//! - 只定义闭合 schema、稳定错误分类和窄 port。
//! - app 层负责 audio_ref 解析、预算、取消、冻结 engine/model identity、
//!   executor 调用和出口投影。
//! - 不包含路径、音频 hash、原始 bytes、worker endpoint/token。

use serde::{Deserialize, Serialize};

// ── 请求 ───────────────────────────────────────────────────────────────────

/// 一次性文件转写请求。
///
/// `audio_ref` 是 app 层签发的短期有效 opaque 引用——
/// AI/MCP 等外部出口不得使用裸路径或 URL 绕过。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioTranscriptionRequest {
    /// opaque audio_ref——由 `AudioResourceRegistry::issue` 签发。
    pub audio_ref: String,
}

// ── 结果 ───────────────────────────────────────────────────────────────────

/// 一次性文件转写结果。
///
/// 至少包含 text、duration_ms、engine_id、model_id、格式规范化和 no_speech 标记。
/// **不得包含路径、音频 hash、原始 bytes、worker endpoint/token。**
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AudioTranscriptionResult {
    /// 识别文本。语义为空时为空串（同时 `no_speech` 为 true）。
    pub text: String,
    /// 音频时长（毫秒）。
    pub duration_ms: u64,
    /// 引擎 id（如 "funasr" 或 "cloud"）。
    pub engine_id: String,
    /// 模型 id（如 "sensevoice-small"）。
    pub model_id: String,
    /// 引擎实例 id（执行时冻结的）。
    pub engine_instance_id: String,
    /// 来源格式摘要（人类可读，如 "2ch 48000Hz 16-bit PCM"）。
    pub source_format: String,
    /// 规范化后格式摘要（如 "1ch 16000Hz mono"）。
    pub normalized_format: String,
    /// 规范化策略摘要（如 "2ch 48000Hz → 16000Hz mono strategy=SimpleAverage"）。
    pub normalization: String,
    /// 语义为空（无语音内容被检测到）。
    pub no_speech: bool,
}

// ── 错误 ───────────────────────────────────────────────────────────────────

/// 一次性文件转写错误——稳定外部分类。
///
/// 可映射到 `CapabilityError::InvalidData` / `Backend` / `Timeout` / `Cancelled` 等，
/// 但不复用语义明确为窗口的 `StaleRef` 文案来描述音频。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioTranscriptionError {
    /// 不支持的音频格式（如压缩 WAV、8-bit PCM、未知 codec）。
    #[error("unsupported audio format: {detail}")]
    UnsupportedAudioFormat { detail: String },

    /// 音频数据损坏（RIFF 魔数错误、chunk 截断、帧不对齐等）。
    #[error("malformed audio: {detail}")]
    MalformedAudio { detail: String },

    /// 解码后样本数或文件大小超出预算。
    #[error("audio budget exceeded: {detail}")]
    AudioBudgetExceeded { detail: String },

    /// audio_ref 已过期（TTL 超时）。
    #[error("stale audio reference")]
    StaleAudioRef,

    /// audio_ref 无效或不可用（不存在、generation 不匹配、scope 不匹配）。
    #[error("invalid audio reference: {detail}")]
    InvalidAudioRef { detail: String },

    /// STT 未配置（未启用或未选择引擎/模型）。
    #[error("stt not configured")]
    SttNotConfigured,

    /// STT 后端不可用（引擎未安装、未启动或实例不可用）。
    #[error("stt backend unavailable: {detail}")]
    SttBackendUnavailable { detail: String },

    /// STT 引擎忙（队列满或推理积压）。
    #[error("stt busy")]
    SttBusy,

    /// 执行期间 STT 身份发生变化（切模或重启后旧结果不得投影）。
    #[error("stt identity changed: {detail}")]
    SttIdentityChanged { detail: String },

    /// 当前构建不支持该转写路线（例如尚未实现的云端文件转写）。
    #[error("unsupported transcription operation: {detail}")]
    Unsupported { detail: String },

    /// 超时——deadline 触发。
    #[error("timeout: {detail}")]
    Timeout { detail: String },

    /// 用户取消。
    #[error("cancelled")]
    Cancelled,

    /// 内部错误（IO 失败等）。
    #[error("internal error: {detail}")]
    Internal { detail: String },
}

impl AudioTranscriptionError {
    /// 稳定分类字符串——用于日志和 CapabilityError::Backend.category。
    pub fn category(&self) -> &'static str {
        match self {
            Self::UnsupportedAudioFormat { .. } => "unsupported_audio_format",
            Self::MalformedAudio { .. } => "malformed_audio",
            Self::AudioBudgetExceeded { .. } => "audio_budget_exceeded",
            Self::StaleAudioRef => "stale_audio_ref",
            Self::InvalidAudioRef { .. } => "invalid_audio_ref",
            Self::SttNotConfigured => "stt_not_configured",
            Self::SttBackendUnavailable { .. } => "stt_backend_unavailable",
            Self::SttBusy => "stt_busy",
            Self::SttIdentityChanged { .. } => "stt_identity_changed",
            Self::Unsupported { .. } => "unsupported",
            Self::Timeout { .. } => "timeout",
            Self::Cancelled => "cancelled",
            Self::Internal { .. } => "internal",
        }
    }

    /// 是否可重试。
    #[allow(dead_code)] // 0.22.16: 保留稳定 API 供未来消费方使用
    pub fn retryable(&self) -> bool {
        match self {
            Self::UnsupportedAudioFormat { .. } => false,
            Self::MalformedAudio { .. } => false,
            Self::AudioBudgetExceeded { .. } => false,
            Self::StaleAudioRef => true,
            Self::InvalidAudioRef { .. } => true,
            Self::SttNotConfigured => false,
            Self::SttBackendUnavailable { .. } => true,
            Self::SttBusy => true,
            Self::SttIdentityChanged { .. } => false,
            Self::Unsupported { .. } => false,
            Self::Timeout { .. } => true,
            Self::Cancelled => false,
            Self::Internal { .. } => true,
        }
    }

    /// 映射到 `CapabilityError`。
    pub fn to_capability_error(&self) -> crate::domain::capability::CapabilityError {
        use crate::domain::capability::CapabilityError;
        match self {
            Self::UnsupportedAudioFormat { detail } => CapabilityError::InvalidData {
                reason: self.category().to_string(),
                detail: detail.clone(),
            },
            Self::MalformedAudio { detail } => CapabilityError::InvalidData {
                reason: self.category().to_string(),
                detail: detail.clone(),
            },
            Self::AudioBudgetExceeded { detail } => CapabilityError::InvalidData {
                reason: self.category().to_string(),
                detail: detail.clone(),
            },
            Self::StaleAudioRef => CapabilityError::InvalidData {
                reason: self.category().to_string(),
                detail: "audio reference expired".into(),
            },
            Self::InvalidAudioRef { detail } => CapabilityError::InvalidData {
                reason: self.category().to_string(),
                detail: detail.clone(),
            },
            Self::SttNotConfigured => CapabilityError::InvalidState {
                detail: "stt not configured".into(),
            },
            Self::SttBackendUnavailable { detail } => CapabilityError::Backend {
                category: self.category().to_string(),
                message: detail.clone(),
                detail: None,
                retryable: true,
            },
            Self::SttBusy => CapabilityError::Conflict {
                detail: "stt engine busy".into(),
            },
            Self::SttIdentityChanged { detail } => CapabilityError::Conflict {
                detail: detail.clone(),
            },
            Self::Unsupported { detail } => CapabilityError::Unsupported {
                required: "implemented file transcription backend".into(),
                actual: detail.clone(),
            },
            Self::Timeout { detail } => CapabilityError::Timeout {
                detail: detail.clone(),
            },
            Self::Cancelled => CapabilityError::Cancelled,
            Self::Internal { detail } => CapabilityError::Internal {
                detail: detail.clone(),
            },
        }
    }
}

// ── 窄 Port ────────────────────────────────────────────────────────────────

/// 一次性文件转写 Port——domain 层窄接口。
///
/// app 层实现此 port，消费 `AudioTranscriptionRequest`，
/// 返回 `AudioTranscriptionResult` 或 `AudioTranscriptionError`。
///
/// **domain 不接触文件系统、Tauri 或 infra 细节**——
/// 只定义闭合请求/结果/错误协议。
#[async_trait::async_trait]
pub trait AudioTranscriptionPort: Send + Sync {
    /// 执行一次性文件转写。
    ///
    /// 实现方负责：
    /// 1. 检查 deadline
    /// 2. 解析 audio_ref 并持有已打开资源
    /// 3. 冻结 STT config、engine/model/instance 身份
    /// 4. 验证已配置、已安装且当前可用
    /// 5. 在 blocking pool 中有界读取、decode、normalize
    /// 6. 再检查 deadline 和被冻结身份
    /// 7. 编码成 canonical 16k mono PCM16 WAV，调用现有唯一 transport
    /// 8. 返回前再次验证 model/instance
    async fn transcribe(
        &self,
        request: AudioTranscriptionRequest,
        deadline: Option<std::time::Instant>,
    ) -> Result<AudioTranscriptionResult, AudioTranscriptionError>;
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_category_stable() {
        assert_eq!(
            AudioTranscriptionError::UnsupportedAudioFormat { detail: "x".into() }.category(),
            "unsupported_audio_format"
        );
        assert_eq!(
            AudioTranscriptionError::MalformedAudio { detail: "x".into() }.category(),
            "malformed_audio"
        );
        assert_eq!(
            AudioTranscriptionError::AudioBudgetExceeded { detail: "x".into() }.category(),
            "audio_budget_exceeded"
        );
        assert_eq!(
            AudioTranscriptionError::StaleAudioRef.category(),
            "stale_audio_ref"
        );
        assert_eq!(
            AudioTranscriptionError::InvalidAudioRef { detail: "x".into() }.category(),
            "invalid_audio_ref"
        );
        assert_eq!(
            AudioTranscriptionError::SttNotConfigured.category(),
            "stt_not_configured"
        );
        assert_eq!(
            AudioTranscriptionError::SttBackendUnavailable { detail: "x".into() }.category(),
            "stt_backend_unavailable"
        );
        assert_eq!(AudioTranscriptionError::SttBusy.category(), "stt_busy");
        assert_eq!(
            AudioTranscriptionError::SttIdentityChanged { detail: "x".into() }.category(),
            "stt_identity_changed"
        );
        assert_eq!(
            AudioTranscriptionError::Unsupported { detail: "x".into() }.category(),
            "unsupported"
        );
        assert_eq!(
            AudioTranscriptionError::Timeout { detail: "x".into() }.category(),
            "timeout"
        );
        assert_eq!(AudioTranscriptionError::Cancelled.category(), "cancelled");
        assert_eq!(
            AudioTranscriptionError::Internal { detail: "x".into() }.category(),
            "internal"
        );
    }

    #[test]
    fn retryable_logic() {
        assert!(
            !AudioTranscriptionError::UnsupportedAudioFormat { detail: "x".into() }.retryable()
        );
        assert!(!AudioTranscriptionError::MalformedAudio { detail: "x".into() }.retryable());
        assert!(AudioTranscriptionError::StaleAudioRef.retryable());
        assert!(AudioTranscriptionError::SttBackendUnavailable { detail: "x".into() }.retryable());
        assert!(AudioTranscriptionError::SttBusy.retryable());
        assert!(!AudioTranscriptionError::SttIdentityChanged { detail: "x".into() }.retryable());
        assert!(!AudioTranscriptionError::Unsupported { detail: "x".into() }.retryable());
        assert!(AudioTranscriptionError::Timeout { detail: "x".into() }.retryable());
        assert!(!AudioTranscriptionError::Cancelled.retryable());
    }

    #[test]
    fn to_capability_error_mapping() {
        use crate::domain::capability::CapabilityError;

        let e = AudioTranscriptionError::UnsupportedAudioFormat {
            detail: "mp3".into(),
        };
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::InvalidData { .. }));

        let e = AudioTranscriptionError::SttNotConfigured;
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::InvalidState { .. }));

        let e = AudioTranscriptionError::SttBackendUnavailable { detail: "x".into() };
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Backend { .. }));

        let e = AudioTranscriptionError::SttBusy;
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Conflict { .. }));

        let e = AudioTranscriptionError::Unsupported {
            detail: "not implemented".into(),
        };
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Unsupported { .. }));

        let e = AudioTranscriptionError::Timeout {
            detail: "30s".into(),
        };
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Timeout { .. }));

        let e = AudioTranscriptionError::Cancelled;
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Cancelled));

        let e = AudioTranscriptionError::Internal {
            detail: "io".into(),
        };
        let cap = e.to_capability_error();
        assert!(matches!(cap, CapabilityError::Internal { .. }));
    }

    #[test]
    fn result_serializes_without_secrets() {
        let result = AudioTranscriptionResult {
            text: "你好世界".into(),
            duration_ms: 1000,
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            engine_instance_id: "inst-abc".into(),
            source_format: "2ch 48000Hz 16-bit PCM".into(),
            normalized_format: "1ch 16000Hz mono".into(),
            normalization: "2ch 48000Hz → 16000Hz mono strategy=SimpleAverage".into(),
            no_speech: false,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("你好世界"));
        assert!(!json.contains("C:\\"));
        assert!(!json.contains("http://"));
        assert!(!json.contains("127.0.0.1"));
        assert!(!json.contains("aref_"));
    }

    #[test]
    fn result_no_speech_uses_empty_text() {
        let result = AudioTranscriptionResult {
            text: String::new(),
            duration_ms: 5000,
            engine_id: "funasr".into(),
            model_id: "sensevoice-small".into(),
            engine_instance_id: "inst-1".into(),
            source_format: "1ch 16000Hz 16-bit PCM".into(),
            normalized_format: "1ch 16000Hz mono".into(),
            normalization: "1ch 16000Hz → 16000Hz mono strategy=Identity".into(),
            no_speech: true,
        };
        assert!(result.text.is_empty());
        assert!(result.no_speech);
    }
}
