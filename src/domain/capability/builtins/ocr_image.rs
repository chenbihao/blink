//! `ocr_image` Capability（0.11.7-c，0.11.7-f 走 backend 注入，0.22.4 走 router）。
//!
//! 接收 PNG 字节，返回 OCR 识别结果（文本 + 行级坐标）。
//! 通过 `OcrBackendRouter` 路由到 windows/paddleocr/auto 后端。
//!
//! **0.22.4**：
//! - 优先使用 `OcrBackendRouter`（支持 windows/paddleocr/auto 路由）。
//! - 可选参数 `screenshot_session` / `screenshot_revision` 携带截图来源信息。
//! - `StructuredOcrError` 正确映射到 `CapabilityError`（不全部拍平为 Internal）。

use std::sync::Arc;

use serde_json::{Value, json};

use super::image_input::resolve_png_input;
use super::ocr_engine::{OcrResult, backend};
use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use crate::domain::ocr::error::OcrErrorCategory;

/// `ocr_image` — 识别图片中的文字，返回文本 + 行级坐标。
pub struct OcrImage;

#[async_trait::async_trait]
impl Capability for OcrImage {
    fn id(&self) -> &str {
        "ocr_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "ocr_image".into(),
            description: "识别图片中的文字，返回识别文本和每行文字的位置坐标。支持中文和英文。图片来源：image_ref（来自截图/剪贴板等能力返回的引用）或 png（原始 PNG 字节数组），二选一。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "image_ref": {
                        "type": "string",
                        "description": "图片引用（来自 read_clipboard/screenshot 等能力返回的 image_ref，与 png 二选一）"
                    },
                    "png": {
                        "type": "array",
                        "items": {"type": "integer"},
                        "description": "PNG 图片字节数组（与 image_ref 二选一）"
                    },
                    "screenshot_session": {
                        "type": "integer",
                        "description": "截图 session epoch（可选，来自截图 overlay 时传入）"
                    },
                    "screenshot_revision": {
                        "type": "integer",
                        "description": "截图选区 revision（可选，同一 session 内重选递增）"
                    }
                }
            }),
            sensitive: true, // 0.21.1 §4.1b：识图输出用户内容，对齐 analyze_image_palette
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::NONE,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::sensitive(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 提取 PNG 字节：image_ref 或 png 二选一（0.19.4）
        // Task 10: resolve_png_input 返回 Bytes（Arc-backed），零拷贝
        let stash = ctx.env.image_stash();
        let png_bytes = resolve_png_input(&args, stash.map(|s| s.as_ref()), "png")?;

        // 0.22.4：优先使用 OcrBackendRouter（支持 windows/paddleocr/auto 路由）
        // 如果未安装 router（测试/旧环境），回退到直接调用 backend()
        if let Some(router) = crate::domain::ocr::router::router() {
            // 0.22.4：优先使用前端传入的 request_id（用于 cancel tracker），
            // 如果没有则自动生成
            let request_id = args
                .get("request_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| {
                    format!(
                        "ocr-cap-{}-{:x}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_millis(),
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .subsec_nanos()
                    )
                });

            // 从 args 中提取截图来源信息（可选）
            let screenshot_session = args.get("screenshot_session").and_then(|v| v.as_u64());
            let screenshot_revision = args.get("screenshot_revision").and_then(|v| v.as_u64());

            // 根据是否有截图来源信息选择 context 构造方式
            let ocr_ctx = if let (Some(epoch), Some(rev)) =
                (screenshot_session, screenshot_revision)
            {
                crate::domain::ocr::context::OcrRequestContext::for_screenshot(
                    &request_id,
                    None,
                    epoch,
                    rev,
                )
            } else {
                crate::domain::ocr::context::OcrRequestContext::for_capability(&request_id, None)
            };

            // 注册到全局 OcrRequestTracker，使前端可通过 cancel_ocr_request 取消
            // Task 6: 使用 RAII guard，drop 时自动 unregister
            let tracker = crate::domain::ocr::context::ocr_request_tracker();
            let _tracker_guard = tracker.register(&request_id, ocr_ctx.cancellation.clone());

            // Task 10: png_bytes 是 Bytes（Arc-backed），直接传给 router（零拷贝）
            let route_result = router.recognize(png_bytes, &ocr_ctx).await;

            // _tracker_guard drop 时自动注销

            // 将 StructuredOcrError 正确映射到 CapabilityError
            if let Some(err) = &route_result.error {
                return Err(map_structured_error_to_capability(err));
            }

            // 0.22.10: 注入实际引擎与回退原因（capability 层专属，不污染底层 backend 结果；
            // 直连 backend() 的 fallback 路径不注入，保持 None）
            let mut result = route_result.result.unwrap();
            result.backend_used = Some(route_result.decision.selected_backend);
            result.backend_fallback_reason = route_result.decision.fallback_reason.clone();
            return Ok(CapabilityResult::Text {
                content: serde_json::to_string(&result as &OcrResult)
                    .unwrap_or_else(|_| result.text.clone()),
                desc: None,
            });
        }

        // 回退：直接调用全局 backend()（测试/旧环境）
        let b = backend();
        let result = b
            .recognize(&png_bytes)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Text {
            content: serde_json::to_string(&result as &OcrResult)
                .unwrap_or_else(|_| result.text.clone()),
            desc: None,
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OcrImage) as Arc<dyn Capability>,
});

// ── StructuredOcrError → CapabilityError 映射（0.22.4，0.22.6.1 结构化收敛） ──

/// 将 `StructuredOcrError` 映射到 `CapabilityError`。
///
/// **不全部拍平为 `Internal(String)`**：
/// - `EnvironmentMissing` → `InvalidState`（环境未就绪，状态不对）
/// - `DecodeError` → `InvalidData { reason: "decode_error" }`
/// - `InputTooLarge` → `InvalidData { reason: "input_too_large" }`
/// - `Timeout` → `Timeout`
/// - `Cancelled` → `Cancelled`
/// - `ProtocolError` / `StartFailed` / `ModelNotReady` / `BackendUnavailable` →
///   `Backend { category, message, detail, retryable }`——稳定分类码在
///   Capability → CommandError 投影中原样保留，诊断不再依靠解析
///   字符串中的 "[protocol_error]"。
fn map_structured_error_to_capability(
    err: &crate::domain::ocr::error::StructuredOcrError,
) -> CapabilityError {
    match err.category {
        OcrErrorCategory::EnvironmentMissing => CapabilityError::InvalidState {
            detail: err.message.clone(),
        },
        OcrErrorCategory::DecodeError => CapabilityError::InvalidData {
            reason: "decode_error".to_string(),
            detail: err.message.clone(),
        },
        OcrErrorCategory::InputTooLarge => CapabilityError::InvalidData {
            reason: "input_too_large".to_string(),
            detail: err.message.clone(),
        },
        OcrErrorCategory::Timeout => CapabilityError::Timeout {
            detail: err.message.clone(),
        },
        OcrErrorCategory::Cancelled => CapabilityError::Cancelled,
        // 后端基础设施错误——保留稳定分类码的 Backend 变体
        OcrErrorCategory::StartFailed
        | OcrErrorCategory::ModelNotReady
        | OcrErrorCategory::ProtocolError
        | OcrErrorCategory::BackendUnavailable => CapabilityError::Backend {
            category: err.category.to_string(),
            message: err.message.clone(),
            detail: err.detail.as_ref().map(|d| d.to_string()),
            retryable: err.retryable,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::super::ocr_engine::{FakeOcrBackend, install_backend};
    use super::*;

    #[test]
    fn id_is_ocr_image() {
        assert_eq!(OcrImage.id(), "ocr_image");
    }

    #[test]
    fn schema_has_png_and_image_ref_params() {
        let s = OcrImage.schema();
        assert_eq!(s.name, "ocr_image");
        assert_eq!(s.parameters["properties"]["png"]["type"], "array");
        assert_eq!(s.parameters["properties"]["image_ref"]["type"], "string");
        // 0.22.4: screenshot_session / screenshot_revision 可选参数
        assert_eq!(
            s.parameters["properties"]["screenshot_session"]["type"],
            "integer"
        );
        assert_eq!(
            s.parameters["properties"]["screenshot_revision"]["type"],
            "integer"
        );
        // 0.19.4: png 不再 required，与 image_ref 二选一
        let required = s.parameters.get("required");
        assert!(required.is_none() || required.unwrap().as_array().unwrap().is_empty());
    }

    // ── StructuredOcrError → CapabilityError 映射测试（0.22.4） ───────────

    #[test]
    fn map_environment_missing_to_invalid_state() {
        let err = crate::domain::ocr::error::StructuredOcrError::environment_missing();
        let cap_err = map_structured_error_to_capability(&err);
        assert!(matches!(cap_err, CapabilityError::InvalidState { .. }));
    }

    #[test]
    fn map_decode_error_to_invalid_data() {
        let err = crate::domain::ocr::error::StructuredOcrError::decode_error("bad png");
        let cap_err = map_structured_error_to_capability(&err);
        assert!(matches!(cap_err, CapabilityError::InvalidData { .. }));
    }

    #[test]
    fn map_timeout_to_timeout() {
        let err = crate::domain::ocr::error::StructuredOcrError::timeout();
        let cap_err = map_structured_error_to_capability(&err);
        assert!(matches!(cap_err, CapabilityError::Timeout { .. }));
    }

    #[test]
    fn map_cancelled_to_cancelled() {
        let err = crate::domain::ocr::error::StructuredOcrError::cancelled();
        let cap_err = map_structured_error_to_capability(&err);
        assert!(matches!(cap_err, CapabilityError::Cancelled));
    }

    #[test]
    fn map_start_failed_to_backend_with_category() {
        let err = crate::domain::ocr::error::StructuredOcrError::start_failed("port in use");
        let cap_err = map_structured_error_to_capability(&err);
        match &cap_err {
            CapabilityError::Backend {
                category,
                message,
                retryable,
                ..
            } => {
                assert_eq!(category, "start_failed");
                assert_eq!(message, "port in use");
                assert!(*retryable, "start_failed 的 retryable 语义必须透传");
            }
            other => panic!("start_failed 应映射为 Backend，实际 {other:?}"),
        }
    }

    #[test]
    fn map_protocol_error_to_backend_with_category() {
        let err = crate::domain::ocr::error::StructuredOcrError::protocol_error("HTTP 500");
        let cap_err = map_structured_error_to_capability(&err);
        match &cap_err {
            CapabilityError::Backend { category, .. } => {
                assert_eq!(category, "protocol_error");
            }
            other => panic!("protocol_error 应映射为 Backend，实际 {other:?}"),
        }
    }

    #[test]
    fn map_input_too_large_to_invalid_data() {
        let err = crate::domain::ocr::error::StructuredOcrError::input_too_large(
            "OCR 输入 30000000 字节超出上限 33554432",
            serde_json::json!({"field": "compressed_bytes", "actual": 30000000u64, "max": 33554432u64}),
        );
        let cap_err = map_structured_error_to_capability(&err);
        match &cap_err {
            CapabilityError::InvalidData { reason, detail } => {
                assert_eq!(reason, "input_too_large");
                assert!(detail.contains("30000000"));
            }
            other => panic!("input_too_large 应映射为 InvalidData，实际 {other:?}"),
        }
    }

    // ── 0.22.6.1 序列化/投影测试：decode_error / input_too_large /
    //    protocol_error / timeout / cancelled ─────────────────────────────

    /// StructuredOcrError 序列化保留稳定 category（snake_case）。
    #[test]
    fn structured_ocr_error_serializes_stable_category() {
        use crate::domain::ocr::error::StructuredOcrError;
        let cases = [
            (StructuredOcrError::decode_error("bad png"), "decode_error"),
            (
                StructuredOcrError::input_too_large("too big", serde_json::json!({})),
                "input_too_large",
            ),
            (
                StructuredOcrError::protocol_error("shape"),
                "protocol_error",
            ),
            (StructuredOcrError::timeout(), "timeout"),
            (StructuredOcrError::cancelled(), "cancelled"),
        ];
        for (err, expected) in &cases {
            let v = serde_json::to_value(err).unwrap();
            assert_eq!(v["category"], *expected, "{err:?} category 序列化漂移");
        }
    }

    /// CapabilityError::Backend 序列化保留 kind=backend + category + retryable。
    #[test]
    fn capability_backend_error_serializes_stably() {
        let e = CapabilityError::Backend {
            category: "protocol_error".to_string(),
            message: "响应 request_id 不匹配".to_string(),
            detail: Some("expected=a got=b".to_string()),
            retryable: false,
        };
        let v = serde_json::to_value(&e).unwrap();
        assert_eq!(v["kind"], "backend");
        assert_eq!(v["category"], "protocol_error");
        assert_eq!(v["retryable"], false);
        assert_eq!(v["detail"], "expected=a got=b");
    }

    /// Capability → CommandError 投影测试已迁移到 app/command_error.rs（0.22 D1）。
    /// domain 层不再引用 crate::app。
    /// Capability 通过 backend() 拿注入的 FakeOcrBackend。
    /// 用一个 minimal PNG 字节序列（PNG 魔数 + 简单 header）绕过 empty check。
    #[tokio::test]
    async fn uses_injected_backend_for_recognition() {
        install_backend(Arc::new(FakeOcrBackend::returning("injected-fake-text")));

        // minimal 8x8 PNG magic + IHDR + IEND（不严格合法但足够绕过 empty check；
        // FakeBackend 不解码只返回预设，PNG 内容不重要）
        let fake_png = vec![0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, 0x00];
        let _args = json!({
            "png": fake_png.iter().map(|b| json!(b)).collect::<Vec<_>>()
        });

        // 无法直接构造 InvokeContext（需要 AppHandle），跳过 Capability::invoke 完整链路，
        // 直接测 backend 注入语义（本文件核心逻辑就是 backend() 调用 + 参数解析）。
        let b = super::super::ocr_engine::backend();
        let result = b.recognize(&fake_png).await.unwrap();
        assert_eq!(result.text, "injected-fake-text");
    }
}
