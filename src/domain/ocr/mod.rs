//! OCR 领域协议（0.22.4）。
//!
//! 把 OCR 相关的领域类型从 `capability/builtins/ocr_engine.rs` 收敛到独立域模块，
//! 使 `OcrBackend`、`OcrRequestContext`、`OcrBackendRouter`、`OcrError`、`OcrConfig`
//! 等协议脱离 capability builtin 巨石，归属于 domain 而非 app/Tauri。
//!
//! ## 分层归属
//!
//! - `domain/ocr`：OCR 领域类型、请求上下文、路由抽象、错误分类和配置枚举。
//!   **不 use tauri / windows**，不发送 Tauri 事件。
//! - `app/local_engine/ocr_backend.rs`：`PaddleOcrBackend` 和 `OcrBackendRouter` 的
//!   具体实现（持有 `EngineManager` 受限依赖）。
//! - `domain/capability/builtins/ocr_engine.rs`：保留 `OcrResult` / `OcrLine` /
//!   `OcrWord` / `OcrRect` 定义和 `WindowsOcrBackendAdapter`，通过 re-export
//!   与本模块共享类型，避免大范围改名。
//!
//! ## 设计决策
//!
//! - **re-export 而非搬移**：`OcrResult` 等类型仍在 `ocr_engine.rs` 定义（已有大量
//!   caller），本模块通过 `pub use` re-export，避免无意义的大范围改名。
//! - **OcrBackend trait 扩展**：新增 `recognize_with_context` 方法，接收受限
//!   `OcrRequestContext`（deadline / cancel / origin）。旧 `recognize` 保留兼容。
//! - **OcrError 重构**：使用 `thiserror` + 稳定分类，至少区分 8 种错误类型。

pub mod config;
pub mod context;
pub mod error;
pub mod input_budget;
pub mod router;

// ── re-export 共享类型（定义仍在 ocr_engine.rs，避免大范围改名）──────────────
// 这些 re-export 构成 domain 公共 API；bin crate 内部不直接引用，
// 但设计意图是让未来外部消费者通过 `domain::ocr::` 路径访问类型。
#[allow(unused_imports)]
pub use crate::domain::capability::builtins::ocr_engine::{
    FakeOcrBackend, OcrBackend, OcrLine, OcrRect, OcrResult, OcrWord, WindowsOcrBackendAdapter,
    install_backend, join_words_smart,
};

// ── 领域层公共类型重导出 ────────────────────────────────────────────────────
#[allow(unused_imports)]
pub use config::{ComputePreference, OcrBackendKind, OcrLifecycle, PaddleModel};
#[allow(unused_imports)]
pub use context::{
    OcrRequestContext, OcrRequestGuard, OcrRequestOrigin, OcrRequestTracker, ScreenshotOrigin,
    ocr_request_tracker,
};
#[allow(unused_imports)]
pub use error::{OcrErrorCategory, StructuredOcrError};
#[allow(unused_imports)]
pub use input_budget::validate_ocr_input;
#[allow(unused_imports)]
pub use router::{OcrBackendRouter, RouteDecision, RouteResult};

// ── 领域层纯逻辑测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod domain_tests {
    use super::*;

    #[test]
    fn ocr_backend_kind_default_is_windows() {
        assert_eq!(OcrBackendKind::default(), OcrBackendKind::Windows);
    }

    #[test]
    fn ocr_lifecycle_default_is_on_demand() {
        assert_eq!(OcrLifecycle::default(), OcrLifecycle::OnDemand);
    }

    #[test]
    fn paddle_model_default_is_tiny() {
        assert_eq!(PaddleModel::default(), PaddleModel::Tiny);
    }

    #[test]
    fn ocr_error_category_distinct_variants() {
        // 至少 8 种错误类型（0.22.6.1 起含 InputTooLarge 共 9 种）
        let categories = [
            OcrErrorCategory::EnvironmentMissing,
            OcrErrorCategory::StartFailed,
            OcrErrorCategory::ModelNotReady,
            OcrErrorCategory::Timeout,
            OcrErrorCategory::Cancelled,
            OcrErrorCategory::ProtocolError,
            OcrErrorCategory::DecodeError,
            OcrErrorCategory::InputTooLarge,
            OcrErrorCategory::BackendUnavailable,
        ];
        // 所有变体互不相同
        for i in 0..categories.len() {
            for j in (i + 1)..categories.len() {
                assert_ne!(categories[i], categories[j], "duplicate at {i},{j}");
            }
        }
    }
}
