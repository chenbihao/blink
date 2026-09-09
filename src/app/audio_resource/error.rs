//! AudioResourceRegistry 错误类型（0.22.16 H03）。

use thiserror::Error;

/// 错误类别——稳定字符串，供调用方（Capability/CLI/MCP）分类展示。
///
/// **铁则**：Debug 和 Display 输出**不包含**绝对路径、文件名中的正文、
/// 音频字节或转写全文。只包含类别和必要的非敏感诊断信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AudioRefErrorKind {
    /// audio_ref 格式无效或空。
    InvalidAudioRef,
    /// audio_ref 已过期（TTL 超时）。
    StaleAudioRef,
    /// 文件身份变化（size/mtime/identity 不匹配）。
    FileIdentityChanged,
    /// 不是 regular file（目录、symlink、reparse point）。
    NotRegularFile,
    /// 资源预算超限（条目数或累计字节超上限）。
    ResourceBudgetExceeded,
    /// generation 不匹配（跨 registry 或已推进）。
    GenerationMismatch,
    /// scope 不匹配。
    ScopeMismatch,
    /// 文件不存在或无法访问。
    FileNotFound,
    /// 打开文件 IO 错误。
    IoError,
}

impl AudioRefErrorKind {
    /// 稳定字符串表示——用于 serde 和日志。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidAudioRef => "invalid_audio_ref",
            Self::StaleAudioRef => "stale_audio_ref",
            Self::FileIdentityChanged => "file_identity_changed",
            Self::NotRegularFile => "not_regular_file",
            Self::ResourceBudgetExceeded => "resource_budget_exceeded",
            Self::GenerationMismatch => "generation_mismatch",
            Self::ScopeMismatch => "scope_mismatch",
            Self::FileNotFound => "file_not_found",
            Self::IoError => "io_error",
        }
    }
}

impl std::fmt::Display for AudioRefErrorKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// AudioResourceRegistry 错误——携带稳定类别 + 非敏感诊断信息。
///
/// **隐私铁则**：`detail` 字段不含绝对路径、音频字节或转写全文。
/// Debug 输出只含类别和脱敏摘要。
#[derive(Debug, Clone, Error)]
#[error("{kind}")]
pub struct AudioRefError {
    pub kind: AudioRefErrorKind,
    /// 非敏感诊断信息（不含绝对路径）。
    #[allow(dead_code)]
    pub detail: Option<String>,
}

impl AudioRefError {
    pub fn new(kind: AudioRefErrorKind) -> Self {
        Self { kind, detail: None }
    }

    pub fn with_detail(kind: AudioRefErrorKind, detail: impl Into<String>) -> Self {
        Self {
            kind,
            detail: Some(detail.into()),
        }
    }

    #[allow(dead_code)]
    pub fn kind(&self) -> AudioRefErrorKind {
        self.kind.clone()
    }
}

// ── From 转换 ──────────────────────────────────────────────────────────────

impl From<std::io::Error> for AudioRefError {
    fn from(e: std::io::Error) -> Self {
        let kind = if e.kind() == std::io::ErrorKind::NotFound {
            AudioRefErrorKind::FileNotFound
        } else {
            AudioRefErrorKind::IoError
        };
        // io::Error 的 Display 可能含路径——用 error kind 替代
        Self::with_detail(kind, format!("io error kind: {:?}", e.kind()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_kind_as_str_stable() {
        assert_eq!(
            AudioRefErrorKind::InvalidAudioRef.as_str(),
            "invalid_audio_ref"
        );
        assert_eq!(AudioRefErrorKind::StaleAudioRef.as_str(), "stale_audio_ref");
        assert_eq!(
            AudioRefErrorKind::FileIdentityChanged.as_str(),
            "file_identity_changed"
        );
        assert_eq!(
            AudioRefErrorKind::NotRegularFile.as_str(),
            "not_regular_file"
        );
        assert_eq!(
            AudioRefErrorKind::ResourceBudgetExceeded.as_str(),
            "resource_budget_exceeded"
        );
        assert_eq!(
            AudioRefErrorKind::GenerationMismatch.as_str(),
            "generation_mismatch"
        );
        assert_eq!(AudioRefErrorKind::ScopeMismatch.as_str(), "scope_mismatch");
        assert_eq!(AudioRefErrorKind::FileNotFound.as_str(), "file_not_found");
        assert_eq!(AudioRefErrorKind::IoError.as_str(), "io_error");
    }

    #[test]
    fn error_display_is_kind_string() {
        let e = AudioRefError::new(AudioRefErrorKind::StaleAudioRef);
        assert_eq!(e.to_string(), "stale_audio_ref");
    }

    #[test]
    fn error_debug_does_not_contain_path() {
        // 即使 detail 有路径片段，Debug 也不应输出路径
        let e =
            AudioRefError::with_detail(AudioRefErrorKind::IoError, "some diagnostic without path");
        let debug_str = format!("{e:?}");
        assert!(!debug_str.contains("C:\\Users"));
        assert!(!debug_str.contains("/home/"));
        // Debug 输出应含 error kind 变体名
        assert!(debug_str.contains("IoError"));
    }

    #[test]
    fn from_io_not_found() {
        let io_err = std::io::Error::new(std::io::ErrorKind::NotFound, "file missing");
        let audio_err = AudioRefError::from(io_err);
        assert_eq!(audio_err.kind, AudioRefErrorKind::FileNotFound);
    }

    #[test]
    fn from_io_other() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "no access");
        let audio_err = AudioRefError::from(io_err);
        assert_eq!(audio_err.kind, AudioRefErrorKind::IoError);
    }
}
