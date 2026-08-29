//! 结构化 OCR 错误（0.22.4）。
//!
//! 使用 `thiserror` + 稳定分类，至少区分 8 种错误类型。
//! `ocr_image` Capability 将 `StructuredOcrError` 映射为对应 `CapabilityError`，
//! 禁止全部拍平成 `Internal(String)`。
//!
//! ## 错误分类
//!
//! | 分类 | 含义 | 可行动提示 |
//! |---|---|---|
//! | `EnvironmentMissing` | PaddleOCR 环境未安装 | "请在设置页安装 PP-OCRv6 环境" |
//! | `StartFailed` | 服务启动失败 | 检查 Python/venv/端口 |
//! | `ModelNotReady` | 模型未就绪 | 等待模型加载完成 |
//! | `Timeout` | 请求超时 | 重试或减小图片 |
//! | `Cancelled` | 请求被取消 | 正常行为，不提示 |
//! | `ProtocolError` | HTTP 协议错误 | 检查服务身份/版本 |
//! | `DecodeError` | 图片解码失败 | 检查图片格式 |
//! | `BackendUnavailable` | 后端不可用 | 回退 WinRT 或检查配置 |

use serde::{Deserialize, Serialize};

/// OCR 错误分类（稳定，serde 序列化）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OcrErrorCategory {
    /// PaddleOCR 环境未安装。
    EnvironmentMissing,
    /// 服务启动失败。
    StartFailed,
    /// 模型未就绪（NotLoaded/Loading/Failed）。
    ModelNotReady,
    /// 请求超时。
    Timeout,
    /// 请求被取消。
    Cancelled,
    /// HTTP 协议错误（状态码/JSON shape/身份不匹配）。
    ProtocolError,
    /// 图片解码失败。
    DecodeError,
    /// 输入超出资源预算（compressed bytes / 单边尺寸 / decoded 像素预算）。
    InputTooLarge,
    /// 后端不可用（无 WinRT、无 PaddleOCR 连接）。
    BackendUnavailable,
}

impl std::fmt::Display for OcrErrorCategory {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrErrorCategory::EnvironmentMissing => write!(f, "environment_missing"),
            OcrErrorCategory::StartFailed => write!(f, "start_failed"),
            OcrErrorCategory::ModelNotReady => write!(f, "model_not_ready"),
            OcrErrorCategory::Timeout => write!(f, "timeout"),
            OcrErrorCategory::Cancelled => write!(f, "cancelled"),
            OcrErrorCategory::ProtocolError => write!(f, "protocol_error"),
            OcrErrorCategory::DecodeError => write!(f, "decode_error"),
            OcrErrorCategory::InputTooLarge => write!(f, "input_too_large"),
            OcrErrorCategory::BackendUnavailable => write!(f, "backend_unavailable"),
        }
    }
}

/// 结构化 OCR 错误（可序列化，IPC 边界保留分类字段）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StructuredOcrError {
    /// 稳定错误分类。
    pub category: OcrErrorCategory,
    /// 用户可读的简短说明。
    pub message: String,
    /// 结构化详情（用于调试或前端特殊处理）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// 错误是否可重试。
    pub retryable: bool,
}

impl StructuredOcrError {
    /// 创建新错误。
    pub fn new(category: OcrErrorCategory, message: impl Into<String>, retryable: bool) -> Self {
        Self {
            category,
            message: message.into(),
            detail: None,
            retryable,
        }
    }

    /// 带详情创建。
    pub fn with_detail(
        category: OcrErrorCategory,
        message: impl Into<String>,
        retryable: bool,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            category,
            message: message.into(),
            detail: Some(detail),
            retryable,
        }
    }

    /// 环境未安装错误（附安装入口提示）。
    #[allow(dead_code)] // 预留：供未来 OCR 后端切换时使用
    pub fn environment_missing() -> Self {
        Self::new(
            OcrErrorCategory::EnvironmentMissing,
            "PP-OCRv6 环境未安装。请在设置页点击「安装环境」按钮。",
            false,
        )
    }

    /// 启动失败错误。
    pub fn start_failed(msg: impl Into<String>) -> Self {
        Self::new(OcrErrorCategory::StartFailed, msg, true)
    }

    /// 模型未就绪错误。
    pub fn model_not_ready(state: impl Into<String>) -> Self {
        Self::with_detail(
            OcrErrorCategory::ModelNotReady,
            "PaddleOCR 模型未就绪",
            false,
            serde_json::json!({ "model_state": state.into() }),
        )
    }

    /// 超时错误。
    pub fn timeout() -> Self {
        Self::new(OcrErrorCategory::Timeout, "OCR 请求超时", true)
    }

    /// 取消错误。
    pub fn cancelled() -> Self {
        Self::new(OcrErrorCategory::Cancelled, "OCR 请求已取消", false)
    }

    /// 协议错误。
    pub fn protocol_error(msg: impl Into<String>) -> Self {
        Self::new(OcrErrorCategory::ProtocolError, msg, false)
    }

    /// 解码错误。
    pub fn decode_error(msg: impl Into<String>) -> Self {
        Self::new(OcrErrorCategory::DecodeError, msg, false)
    }

    /// 输入超出资源预算（带实际值与允许上限，不记录图片内容）。
    pub fn input_too_large(message: impl Into<String>, detail: serde_json::Value) -> Self {
        Self::with_detail(OcrErrorCategory::InputTooLarge, message, false, detail)
    }

    /// 后端不可用错误。
    pub fn backend_unavailable(msg: impl Into<String>) -> Self {
        Self::new(OcrErrorCategory::BackendUnavailable, msg, false)
    }
}

impl std::fmt::Display for StructuredOcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.category, self.message)
    }
}

impl std::error::Error for StructuredOcrError {}

/// 从旧的 `OcrError`（capability builtin）转换到 `StructuredOcrError`。
///
/// 旧 `OcrError` 只有 3 个变体（Engine/Decode/Unsupported），
/// 映射到新分类时保守地归入 `ProtocolError` / `DecodeError` / `BackendUnavailable`。
impl From<&crate::domain::capability::builtins::ocr_engine::OcrError> for StructuredOcrError {
    fn from(err: &crate::domain::capability::builtins::ocr_engine::OcrError) -> Self {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        match err {
            OcrError::Engine(msg) => StructuredOcrError::protocol_error(msg.clone()),
            OcrError::Decode(msg) => StructuredOcrError::decode_error(msg.clone()),
            OcrError::Unsupported => StructuredOcrError::backend_unavailable("当前平台不支持 OCR"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environment_missing_has_install_hint() {
        let err = StructuredOcrError::environment_missing();
        assert_eq!(err.category, OcrErrorCategory::EnvironmentMissing);
        assert!(err.message.contains("安装"));
        assert!(!err.retryable);
    }

    #[test]
    fn timeout_is_retryable() {
        let err = StructuredOcrError::timeout();
        assert_eq!(err.category, OcrErrorCategory::Timeout);
        assert!(err.retryable);
    }

    #[test]
    fn cancelled_is_not_retryable() {
        let err = StructuredOcrError::cancelled();
        assert_eq!(err.category, OcrErrorCategory::Cancelled);
        assert!(!err.retryable);
    }

    #[test]
    fn model_not_ready_carries_state() {
        let err = StructuredOcrError::model_not_ready("Loading");
        assert_eq!(err.category, OcrErrorCategory::ModelNotReady);
        let detail = err.detail.as_ref().unwrap();
        assert_eq!(detail["model_state"], "Loading");
    }

    #[test]
    fn serde_roundtrip() {
        let err = StructuredOcrError::start_failed("端口被占用");
        let json = serde_json::to_string(&err).unwrap();
        let back: StructuredOcrError = serde_json::from_str(&json).unwrap();
        assert_eq!(err.category, back.category);
        assert_eq!(err.message, back.message);
        assert_eq!(err.retryable, back.retryable);
    }

    #[test]
    fn from_legacy_ocr_error_engine() {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        let legacy = OcrError::Engine("server crashed".to_string());
        let structured = StructuredOcrError::from(&legacy);
        assert_eq!(structured.category, OcrErrorCategory::ProtocolError);
    }

    #[test]
    fn from_legacy_ocr_error_decode() {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        let legacy = OcrError::Decode("bad png".to_string());
        let structured = StructuredOcrError::from(&legacy);
        assert_eq!(structured.category, OcrErrorCategory::DecodeError);
    }

    #[test]
    fn from_legacy_ocr_error_unsupported() {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        let legacy = OcrError::Unsupported;
        let structured = StructuredOcrError::from(&legacy);
        assert_eq!(structured.category, OcrErrorCategory::BackendUnavailable);
    }
}
