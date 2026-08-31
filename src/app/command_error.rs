//! IPC 错误协议（0.14.7 W3）。
//!
//! **职责**：app/IPC 边界的稳定、可序列化错误 wire schema。
//! 前端能按错误类别展示（code/message/detail/retryable），同时兼容尚未迁移的字符串错误。
//!
//! **设计原则**：
//! - code 使用稳定的 snake_case 值，不把 Rust 类型名或 Debug 文本当协议
//! - message 是用户可读的简短说明（前端可直接展示）
//! - detail 是可选的结构化数据，用于调试或前端特殊处理
//! - retryable 标识错误是否可重试（如超时可重试、参数错误不可重试）

use serde::{Deserialize, Serialize};

/// IPC 错误类型（app/IPC 边界 wire schema）。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub struct CommandError {
    /// 稳定的 snake_case 错误码，前端可据此分类展示（如 `permission_denied`、`timeout`）。
    pub code: String,
    /// 用户可读的简短说明，前端可直接展示。
    pub message: String,
    /// 可选的结构化详情，用于调试或前端特殊处理（如缺失的参数名）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<serde_json::Value>,
    /// 错误是否可重试（如超时可重试、参数错误不可重试）。
    pub retryable: bool,
}

impl CommandError {
    /// 创建新的 CommandError（最小形式）。
    pub fn new(code: &str, message: impl AsRef<str>, retryable: bool) -> Self {
        Self {
            code: code.to_string(),
            message: message.as_ref().to_string(),
            detail: None,
            retryable,
        }
    }

    /// 创建带 detail 的 CommandError。
    pub fn with_detail(
        code: &str,
        message: impl AsRef<str>,
        retryable: bool,
        detail: serde_json::Value,
    ) -> Self {
        Self {
            code: code.to_string(),
            message: message.as_ref().to_string(),
            detail: Some(detail),
            retryable,
        }
    }
}

impl std::fmt::Display for CommandError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::error::Error for CommandError {}

// ── CapabilityError 映射 ─────────────────────────────────────────────────────

/// CapabilityError → CommandError 映射（0.14.7 W3）。
impl From<crate::domain::capability::CapabilityError> for CommandError {
    fn from(e: crate::domain::capability::CapabilityError) -> Self {
        use crate::domain::capability::CapabilityError::*;

        match e {
            InvalidArgs { detail } => {
                Self::new("invalid_args", format!("参数错误: {detail}"), false)
            }
            InvalidState { detail } => {
                Self::new("invalid_state", format!("状态错误: {detail}"), false)
            }
            Conflict { detail } => Self::new("conflict", format!("并发冲突: {detail}"), true),
            InvalidData { reason, detail } => Self::with_detail(
                "invalid_data",
                format!("数据无效: {detail}"),
                false,
                serde_json::json!({ "reason": reason }),
            ),
            Permission { detail } => {
                Self::new("permission_denied", format!("权限不足: {detail}"), false)
            }
            OriginDenied { origin, allowed } => Self::with_detail(
                "origin_denied",
                format!("来源不被允许: {origin} 不在允许集合内 ({allowed})"),
                false,
                serde_json::json!({ "origin": origin, "allowed": allowed }),
            ),
            Unsupported { required, actual } => Self::with_detail(
                "unsupported",
                format!("运行时不支持: 需要 {required}，当前可用 {actual}"),
                false,
                serde_json::json!({ "required": required, "actual": actual }),
            ),
            Timeout { detail } => Self::new("timeout", format!("操作超时: {detail}"), true),
            Cancelled => Self::new("cancelled", "操作已取消", false),
            NotFound { id } => Self::with_detail(
                "not_found",
                format!("能力不存在: {id}"),
                false,
                serde_json::json!({ "id": id }),
            ),
            Backend {
                category,
                message,
                detail,
                retryable,
            } => {
                // 0.22.6.1：后端结构化错误保留 stable category/code、message、
                // detail、retryable——诊断直接读结构化字段，不再解析字符串。
                let mut detail_json = serde_json::json!({ "category": category });
                if let Some(d) = detail {
                    detail_json["detail"] = serde_json::Value::String(d);
                }
                Self::with_detail(&category, message, retryable, detail_json)
            }
            Internal { detail } => {
                Self::new("internal_error", format!("内部错误: {detail}"), false)
            }
        }
    }
}

// ── OcrError 映射（ocr_image command）──────────────────────────────────────

/// OcrError → CommandError 映射（0.14.7 W3）。
impl From<crate::domain::capability::builtins::ocr_engine::OcrError> for CommandError {
    fn from(e: crate::domain::capability::builtins::ocr_engine::OcrError) -> Self {
        use crate::domain::capability::builtins::ocr_engine::OcrError::*;

        match e {
            Engine(msg) => Self::new("ocr_engine_error", format!("OCR 引擎错误: {msg}"), false),
            Decode(msg) => Self::new("image_decode_error", format!("图片解码错误: {msg}"), false),
            Unsupported => Self::new("ocr_unsupported", "当前平台不支持 OCR", false),
        }
    }
}

// ── StickyError / StickyWorkflowError 映射（close_sticky_note command，0.20.7）──

impl From<crate::domain::sticky::StickyError> for CommandError {
    fn from(e: crate::domain::sticky::StickyError) -> Self {
        use crate::domain::sticky::StickyError;

        match e {
            StickyError::Db { detail } => {
                Self::new("internal_error", format!("数据库错误: {detail}"), false)
            }
            StickyError::NotFound { id } => Self::with_detail(
                "not_found",
                format!("便签不存在: {id}"),
                false,
                serde_json::json!({ "id": id }),
            ),
            StickyError::Trashed { id } => Self::with_detail(
                "invalid_state",
                format!("便签已在回收站: {id}"),
                false,
                serde_json::json!({ "id": id }),
            ),
            StickyError::Conflict {
                id,
                expected_updated_at,
                actual_updated_at,
            } => Self::with_detail(
                "conflict",
                format!(
                    "便签已被修改，请重试（期望版本 {expected_updated_at}，当前版本 {actual_updated_at}）"
                ),
                true,
                serde_json::json!({
                    "id": id,
                    "expected_updated_at": expected_updated_at,
                    "actual_updated_at": actual_updated_at,
                }),
            ),
        }
    }
}

impl From<crate::domain::sticky::StickyWorkflowError> for CommandError {
    fn from(e: crate::domain::sticky::StickyWorkflowError) -> Self {
        match e {
            crate::domain::sticky::StickyWorkflowError::Sticky(err) => err.into(),
            crate::domain::sticky::StickyWorkflowError::SideEffect { detail } => Self::new(
                "internal_error",
                format!("便签界面同步失败: {detail}"),
                false,
            ),
        }
    }
}

// ── LocalEngineError 映射（0.22.5）────────────────────────────────────────

/// LocalEngineError → CommandError 映射。
///
/// 稳定 code 保留为 snake_case，detail 投影 phase + 原始 detail。
impl From<crate::domain::local_engine::LocalEngineError> for CommandError {
    fn from(e: crate::domain::local_engine::LocalEngineError) -> Self {
        use crate::domain::local_engine::LocalEngineErrorCode;

        let retryable = matches!(
            e.code,
            LocalEngineErrorCode::Timeout
                | LocalEngineErrorCode::PortConflict
                | LocalEngineErrorCode::Rejected
                | LocalEngineErrorCode::ServiceUnreachable
                | LocalEngineErrorCode::ModelNotReady
        );

        let detail = serde_json::json!({
            "phase": format!("{:?}", e.phase).to_lowercase(),
            "detail": e.detail,
        });

        // code: serde 序列化为 snake_case 字符串（Debug 格式不保留 snake_case）
        let code = serde_json::to_value(e.code)
            .ok()
            .and_then(|v| v.as_str().map(|s| s.to_string()))
            .unwrap_or_else(|| format!("{:?}", e.code).to_lowercase());

        Self::with_detail(&code, e.action_hint, retryable, detail)
    }
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_error_serializes_stably() {
        let e = CommandError::new("invalid_args", "参数错误", false);
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["code"], "invalid_args");
        assert_eq!(json["message"], "参数错误");
        assert_eq!(json["retryable"], false);
        assert!(json.get("detail").is_none());
    }

    #[test]
    fn command_error_with_detail_serializes() {
        let e = CommandError::with_detail(
            "not_found",
            "能力不存在",
            false,
            serde_json::json!({ "id": "test_cap" }),
        );
        let json = serde_json::to_value(&e).unwrap();
        assert_eq!(json["code"], "not_found");
        assert_eq!(json["detail"]["id"], "test_cap");
    }

    #[test]
    fn capability_error_invalid_args_maps_correctly() {
        use crate::domain::capability::CapabilityError;
        let e = CapabilityError::InvalidArgs {
            detail: "缺少 query 参数".into(),
        };
        let ce: CommandError = e.into();
        assert_eq!(ce.code, "invalid_args");
        assert!(ce.message.contains("参数错误"));
        assert!(!ce.retryable);
    }

    #[test]
    fn capability_error_timeout_is_retryable() {
        use crate::domain::capability::CapabilityError;
        let e = CapabilityError::Timeout {
            detail: "5s".into(),
        };
        let ce: CommandError = e.into();
        assert_eq!(ce.code, "timeout");
        assert!(ce.retryable);
    }

    #[test]
    fn capability_error_not_found_includes_id_in_detail() {
        use crate::domain::capability::CapabilityError;
        let e = CapabilityError::NotFound {
            id: "nonexistent".into(),
        };
        let ce: CommandError = e.into();
        assert_eq!(ce.code, "not_found");
        assert_eq!(ce.detail.unwrap()["id"], "nonexistent");
    }

    #[test]
    fn ocr_error_engine_maps_correctly() {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        let e = OcrError::Engine("OCR 引擎初始化失败".into());
        let ce: CommandError = e.into();
        assert_eq!(ce.code, "ocr_engine_error");
        assert!(ce.message.contains("OCR 引擎错误"));
        assert!(!ce.retryable);
    }

    #[test]
    fn ocr_error_unsupported_maps_correctly() {
        use crate::domain::capability::builtins::ocr_engine::OcrError;
        let ce: CommandError = OcrError::Unsupported.into();
        assert_eq!(ce.code, "ocr_unsupported");
        assert!(!ce.retryable);
    }

    #[test]
    fn command_error_round_trip() {
        let e = CommandError::with_detail(
            "not_found",
            "能力不存在",
            false,
            serde_json::json!({ "id": "test" }),
        );
        let json = serde_json::to_value(&e).unwrap();
        let e2: CommandError = serde_json::from_value(json).unwrap();
        assert_eq!(e.code, e2.code);
        assert_eq!(e.message, e2.message);
        assert_eq!(e.detail, e2.detail);
        assert_eq!(e.retryable, e2.retryable);
    }

    /// Capability → CommandError 投影：OCR 各错误分类保留
    /// stable code / message / detail / retryable，诊断不再解析 "[[...]]" 字符串。
    ///
    /// 从 domain/capability/builtins/ocr_image.rs 迁移（0.22 D1）：
    /// app 测试从公开领域错误 CapabilityError 构造输入，不依赖 domain private helper。
    #[test]
    fn capability_to_command_error_projection_preserves_structure() {
        use crate::domain::capability::CapabilityError;

        struct Case {
            name: &'static str,
            cap: CapabilityError,
            expected_code: &'static str,
            expected_retryable: bool,
        }
        let cases = vec![
            Case {
                name: "decode_error",
                cap: CapabilityError::InvalidData {
                    reason: "decode_error".to_string(),
                    detail: "PNG header 非法".to_string(),
                },
                expected_code: "invalid_data",
                expected_retryable: false,
            },
            Case {
                name: "input_too_large",
                cap: CapabilityError::InvalidData {
                    reason: "input_too_large".to_string(),
                    detail: "decoded 像素超出上限".to_string(),
                },
                expected_code: "invalid_data",
                expected_retryable: false,
            },
            Case {
                name: "protocol_error",
                cap: CapabilityError::Backend {
                    category: "protocol_error".to_string(),
                    message: "响应缺少 request_id".to_string(),
                    detail: None,
                    retryable: false,
                },
                expected_code: "protocol_error",
                expected_retryable: false,
            },
            Case {
                name: "timeout",
                cap: CapabilityError::Timeout {
                    detail: "操作超时".to_string(),
                },
                expected_code: "timeout",
                expected_retryable: true,
            },
            Case {
                name: "cancelled",
                cap: CapabilityError::Cancelled,
                expected_code: "cancelled",
                expected_retryable: false,
            },
        ];

        for case in cases {
            let ce: CommandError = case.cap.into();
            assert_eq!(ce.code, case.expected_code, "case={}", case.name);
            assert_eq!(ce.retryable, case.expected_retryable, "case={}", case.name);
            assert!(
                !ce.message.contains("[["),
                "case={} message 不得再携带 [[category]] 括号伪协议",
                case.name
            );
            // detail 保留结构化字段（timeout/cancelled 无载荷 detail）
            let detail = ce.detail;
            match case.name {
                "decode_error" | "input_too_large" => {
                    let detail = detail.expect("invalid_data 投影应保留 detail");
                    assert!(
                        detail.get("reason").is_some(),
                        "case={} detail.reason 应保留",
                        case.name
                    );
                }
                "protocol_error" => {
                    let detail = detail.expect("backend 投影应保留 detail");
                    assert_eq!(detail["category"], "protocol_error");
                }
                _ => {}
            }
        }
    }
}
