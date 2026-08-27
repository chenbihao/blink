//! OCR 后端路由器（domain 侧抽象，0.22.4）。
//!
//! `OcrBackendRouter` 是 domain 层的路由 trait：
//! - 每次 recognize 开始时快照 `Arc<dyn OcrBackend>` 或等价不可变 route decision。
//! - 中途修改配置不能改变在途请求。
//!
//! 具体实现（`OcrBackendRouterImpl`）在 `app/local_engine/ocr_backend.rs`，
//! 持有 `LocalEngineService` 和 `OcrConfig` 受限依赖。
//!
//! ## 路由语义（§3.9）
//!
//! | backend | 行为 |
//! |---|---|
//! | `windows` | 始终使用 WinRT backend |
//! | `paddleocr` | 明确选择 PaddleOCR；未安装/启动失败返回可行动错误，不静默回退 |
//! | `auto` | 仅 PaddleOCR 热态 Ready 时使用它；否则立即 WinRT |

use std::sync::Arc;

use bytes::Bytes;

use crate::domain::capability::builtins::ocr_engine::OcrResult;
use crate::domain::ocr::config::OcrBackendKind;
use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::StructuredOcrError;

/// 路由决策（recognize 开始时的不可变快照）。
#[derive(Debug, Clone)]
pub struct RouteDecision {
    /// 配置的 backend 种类。
    /// 预留：当前实现只使用 `selected_backend`，保留 `configured_backend` 供诊断/日志。
    #[allow(dead_code)]
    pub configured_backend: OcrBackendKind,
    /// 本次实际选择的 backend。
    pub selected_backend: OcrBackendKind,
    /// 如果发生了 fallback（auto 模式 PaddleOCR 不可用 → WinRT），
    /// 记录 fallback 原因。
    pub fallback_reason: Option<String>,
}

/// 路由结果。
#[derive(Debug)]
pub struct RouteResult {
    /// 路由决策。
    pub decision: RouteDecision,
    /// 识别结果（成功时）。
    pub result: Option<OcrResult>,
    /// 错误（失败时）。
    pub error: Option<StructuredOcrError>,
    /// 总耗时（毫秒）。
    pub total_elapsed_ms: u64,
    /// 启动等待耗时（毫秒，paddleocr on-demand start 等待时间）。
    pub start_wait_ms: u64,
    /// 识别耗时（毫秒）。
    pub recognize_ms: u64,
    /// fallback 识别耗时（毫秒，auto 模式 PaddleOCR 崩溃后 WinRT fallback 耗时）。
    pub fallback_ms: u64,
}

impl RouteResult {
    /// 成功结果。
    pub fn success(
        decision: RouteDecision,
        result: OcrResult,
        total_elapsed_ms: u64,
        start_wait_ms: u64,
        recognize_ms: u64,
    ) -> Self {
        Self {
            decision,
            result: Some(result),
            error: None,
            total_elapsed_ms,
            start_wait_ms,
            recognize_ms,
            fallback_ms: 0,
        }
    }

    /// 失败结果。
    pub fn error(
        decision: RouteDecision,
        error: StructuredOcrError,
        total_elapsed_ms: u64,
        start_wait_ms: u64,
    ) -> Self {
        Self {
            decision,
            result: None,
            error: Some(error),
            total_elapsed_ms,
            start_wait_ms,
            recognize_ms: 0,
            fallback_ms: 0,
        }
    }

    /// fallback 结果（auto 模式 PaddleOCR 崩溃后 WinRT 成功）。
    pub fn fallback_success(
        decision: RouteDecision,
        result: OcrResult,
        total_elapsed_ms: u64,
        start_wait_ms: u64,
        recognize_ms: u64,
        fallback_ms: u64,
    ) -> Self {
        Self {
            decision,
            result: Some(result),
            error: None,
            total_elapsed_ms,
            start_wait_ms,
            recognize_ms,
            fallback_ms,
        }
    }
}

/// OCR 后端路由器 trait（domain 侧抽象）。
///
/// 每次 `recognize` 开始时快照不可变 route decision，
/// 中途修改配置不改变在途请求。
#[async_trait::async_trait]
pub trait OcrBackendRouter: Send + Sync {
    /// 路由并执行 OCR 识别。
    ///
    /// 1. 读取本次 OcrConfig 快照。
    /// 2. 根据 configured_backend 选择实际后端。
    /// 3. 执行识别，返回 `RouteResult`。
    async fn recognize(&self, png_data: Bytes, ctx: &OcrRequestContext) -> RouteResult;

    /// 返回当前路由诊断快照（只读，无副作用）。
    ///
    /// 不启动/修复引擎，只返回当前状态。
    async fn diagnose(&self) -> OcrRouteDiagnosis;
}

/// OCR 路由诊断快照（只读）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct OcrRouteDiagnosis {
    /// 配置的 backend。
    pub configured_backend: OcrBackendKind,
    /// 上次实际选择的 backend。
    pub last_selected_backend: Option<OcrBackendKind>,
    /// 上次 fallback 原因。
    pub last_fallback_reason: Option<String>,
    /// PaddleOCR 环境是否已安装。
    pub paddleocr_installed: bool,
    /// PaddleOCR 服务状态。
    pub paddleocr_service_state: String,
    /// PaddleOCR 模型状态。
    pub paddleocr_model_state: String,
    /// PaddleOCR 模型 id。
    pub paddleocr_model_id: Option<String>,
    /// PaddleOCR 模型 revision。
    pub paddleocr_model_revision: Option<String>,
    /// PaddleOCR instance id（非敏感值）。
    pub paddleocr_instance_id: Option<String>,
    /// 实际计算后端。
    pub paddleocr_actual_backend: Option<String>,
    /// 当前 in-flight 请求数。
    pub in_flight_count: usize,
    /// 生命周期策略。
    pub lifecycle: String,
    /// 空闲 TTL（秒）。
    pub idle_ttl_seconds: u32,
    /// 上次结构化错误。
    pub last_error: Option<StructuredOcrError>,
    /// WinRT available_languages。
    pub winrt_available_languages: Vec<String>,
    /// WinRT engine_language。
    pub winrt_engine_language: Option<String>,
    /// 上次总耗时（毫秒）。
    pub last_total_elapsed_ms: Option<u64>,
    /// 上次启动等待耗时（毫秒）。
    pub last_start_wait_ms: Option<u64>,
    /// 上次识别耗时（毫秒）。
    pub last_recognize_ms: Option<u64>,
    /// 上次 fallback 耗时（毫秒）。
    pub last_fallback_ms: Option<u64>,
}

impl Default for OcrRouteDiagnosis {
    fn default() -> Self {
        Self {
            configured_backend: OcrBackendKind::default(),
            last_selected_backend: None,
            last_fallback_reason: None,
            paddleocr_installed: false,
            paddleocr_service_state: "Unknown".to_string(),
            paddleocr_model_state: "Unknown".to_string(),
            paddleocr_model_id: None,
            paddleocr_model_revision: None,
            paddleocr_instance_id: None,
            paddleocr_actual_backend: None,
            in_flight_count: 0,
            lifecycle: "on_demand".to_string(),
            idle_ttl_seconds: 300,
            last_error: None,
            winrt_available_languages: Vec::new(),
            winrt_engine_language: None,
            last_total_elapsed_ms: None,
            last_start_wait_ms: None,
            last_recognize_ms: None,
            last_fallback_ms: None,
        }
    }
}

/// 全局 router 注入（对齐 `ocr_engine::install_backend` 模式）。
///
/// 0.22.4：`main.rs` 启动时注入 `OcrBackendRouterImpl`，
/// `ocr_image` Capability 和截图 OCR 通过 `router()` 获取实例。
static ROUTER: std::sync::OnceLock<std::sync::RwLock<Arc<dyn OcrBackendRouter>>> =
    std::sync::OnceLock::new();

/// 安装/替换 OCR router。
#[allow(dead_code)]
pub fn install_router(router: Arc<dyn OcrBackendRouter>) {
    match ROUTER.get() {
        Some(lock) => {
            if let Ok(mut w) = lock.write() {
                *w = router;
            }
        }
        None => {
            let _ = ROUTER.set(std::sync::RwLock::new(router));
        }
    }
}

/// 获取当前 OCR router。
///
/// **首次调用兜底**：如果没有注入 router，返回 `None`，
/// 调用方回退到 `ocr_engine::backend()`。
pub fn router() -> Option<Arc<dyn OcrBackendRouter>> {
    ROUTER
        .get()
        .and_then(|lock| lock.read().ok().map(|r| r.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ocr::context::OcrRequestContext;

    /// 测试用 fake router（返回预定义结果）。
    pub struct FakeRouter {
        result_text: String,
        decision: RouteDecision,
    }

    impl FakeRouter {
        pub fn returning(text: impl Into<String>) -> Self {
            Self {
                result_text: text.into(),
                decision: RouteDecision {
                    configured_backend: OcrBackendKind::Windows,
                    selected_backend: OcrBackendKind::Windows,
                    fallback_reason: None,
                },
            }
        }
    }

    #[async_trait::async_trait]
    impl OcrBackendRouter for FakeRouter {
        async fn recognize(&self, _png_data: Bytes, _ctx: &OcrRequestContext) -> RouteResult {
            use crate::domain::capability::builtins::ocr_engine::OcrResult;
            RouteResult::success(
                self.decision.clone(),
                OcrResult {
                    text: self.result_text.clone(),
                    lines: vec![],
                    words: vec![],
                    text_angle: None,
                },
                10,
                0,
                10,
            )
        }

        async fn diagnose(&self) -> OcrRouteDiagnosis {
            OcrRouteDiagnosis::default()
        }
    }

    #[tokio::test]
    async fn fake_router_returns_configured_text() {
        let router = FakeRouter::returning("test-text");
        let ctx = OcrRequestContext::for_capability("test", None);
        let result = router.recognize(Bytes::new(), &ctx).await;
        assert_eq!(result.result.unwrap().text, "test-text");
        assert_eq!(result.decision.selected_backend, OcrBackendKind::Windows);
    }

    #[test]
    fn route_result_success_has_result() {
        let decision = RouteDecision {
            configured_backend: OcrBackendKind::Windows,
            selected_backend: OcrBackendKind::Windows,
            fallback_reason: None,
        };
        use crate::domain::capability::builtins::ocr_engine::OcrResult;
        let result = RouteResult::success(
            decision,
            OcrResult {
                text: "hello".into(),
                lines: vec![],
                words: vec![],
                text_angle: None,
            },
            100,
            0,
            100,
        );
        assert!(result.result.is_some());
        assert!(result.error.is_none());
        assert_eq!(result.total_elapsed_ms, 100);
    }

    #[test]
    fn route_result_error_has_error() {
        let decision = RouteDecision {
            configured_backend: OcrBackendKind::PaddleOcr,
            selected_backend: OcrBackendKind::PaddleOcr,
            fallback_reason: None,
        };
        let result = RouteResult::error(decision, StructuredOcrError::environment_missing(), 50, 0);
        assert!(result.result.is_none());
        assert!(result.error.is_some());
        assert_eq!(
            result.error.unwrap().category,
            crate::domain::ocr::error::OcrErrorCategory::EnvironmentMissing
        );
    }

    #[test]
    fn install_and_get_router() {
        let r: Arc<dyn OcrBackendRouter> = Arc::new(FakeRouter::returning("injected"));
        install_router(r);
        let got = router();
        assert!(got.is_some());
    }
}
