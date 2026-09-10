//! 路由决策与结果组装（0.22.8-D）。
//!
//! 从 `OcrBackendRouter::recognize` 中拆出的核心路由逻辑：
//! - backend selection（Windows / PaddleOcr / Auto 三路分支）
//! - fallback decision（auto 模式 PaddleOCR 失败 → WinRT）
//! - RouteResult 组装（success / error / fallback_success）
//! - 诊断投影（OcrRouteDiagnosis 构建）
//!
//! **不变量**：
//! - 路由决策在请求开始时快照配置，在途请求不受运行时配置变更影响。
//! - auto 模式的 fallback 逻辑：输入本身的问题（取消/解码失败/超预算）不回退，
//!   只有后端基础设施问题才 fallback WinRT。
//! - deadline/cancel 后不得继续 WinRT fallback。

use std::sync::atomic::Ordering;

use bytes::Bytes;
use tokio::time::Instant;

use crate::domain::capability::builtins::ocr_engine::OcrResult;
use crate::domain::config::ocr_config::OcrRuntimeSnapshot;
use crate::domain::ocr::config::OcrBackendKind;
use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::{OcrErrorCategory, StructuredOcrError};
use crate::domain::ocr::router::{OcrRouteDiagnosis, RouteDecision, RouteResult};

use super::OcrCoordinator;

/// 路由内部的中间结果——backend 分支执行完毕后的产物。
///
/// `decision` 记录路由选择和 fallback 原因；
/// `result` / `start_wait_ms` / `recognize_ms` / `fallback_ms` 记录耗时和结果。
pub(super) struct RouteOutcome {
    decision: RouteDecision,
    result: Result<OcrResult, StructuredOcrError>,
    start_wait_ms: u64,
    recognize_ms: u64,
    fallback_ms: u64,
}

impl OcrCoordinator {
    /// 路由核心：根据 `snapshot.backend` 分发到对应后端分支。
    ///
    /// 三路分支各自返回 `RouteOutcome`，由调用方组装为 `RouteResult`。
    pub(super) async fn route_by_backend(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        snapshot: &OcrRuntimeSnapshot,
        request_png_size: (u32, u32),
    ) -> RouteOutcome {
        match snapshot.backend {
            OcrBackendKind::Windows => self.route_windows(png_data, ctx, snapshot).await,
            OcrBackendKind::PaddleOcr => {
                self.route_paddleocr(png_data, ctx, snapshot, request_png_size)
                    .await
            }
            OcrBackendKind::Auto => {
                self.route_auto(png_data, ctx, snapshot, request_png_size)
                    .await
            }
        }
    }

    /// Windows 后端路由：始终使用 WinRT。
    async fn route_windows(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        snapshot: &OcrRuntimeSnapshot,
    ) -> RouteOutcome {
        let decision = RouteDecision {
            configured_backend: snapshot.backend,
            selected_backend: OcrBackendKind::Windows,
            fallback_reason: None,
        };

        if ctx.should_stop() {
            let err = stop_error(ctx);
            return RouteOutcome {
                decision,
                result: Err(err),
                start_wait_ms: 0,
                recognize_ms: 0,
                fallback_ms: 0,
            };
        }

        let (res, ms) = self.do_winrt_recognize(&png_data, ctx).await;
        RouteOutcome {
            decision,
            result: res,
            start_wait_ms: 0,
            recognize_ms: ms,
            fallback_ms: 0,
        }
    }

    /// PaddleOcr 后端路由：显式选择 PaddleOCR，未安装时降级 WinRT。
    async fn route_paddleocr(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        snapshot: &OcrRuntimeSnapshot,
        request_png_size: (u32, u32),
    ) -> RouteOutcome {
        let installed = self.is_paddleocr_installed().await;
        if !installed {
            tracing::info!("paddleocr 显式模式环境未安装，降级 WinRT");
            let (res, ms) = self.do_winrt_recognize(&png_data, ctx).await;
            return RouteOutcome {
                decision: RouteDecision {
                    configured_backend: snapshot.backend,
                    selected_backend: OcrBackendKind::Windows,
                    fallback_reason: Some("PaddleOCR 环境未安装，已降级 Windows OCR".to_string()),
                },
                result: res,
                start_wait_ms: 0,
                recognize_ms: ms,
                fallback_ms: 0,
            };
        }

        // 已安装：执行 PaddleOCR 识别
        let (res, start_wait, recog_ms) = {
            self.idle_cancel.notify_waiters();
            self.do_paddleocr_recognize(png_data.clone(), ctx, false, request_png_size)
                .await
        };
        self.schedule_idle_stop(*snapshot);
        RouteOutcome {
            decision: RouteDecision {
                configured_backend: snapshot.backend,
                selected_backend: OcrBackendKind::PaddleOcr,
                fallback_reason: None,
            },
            result: res,
            start_wait_ms: start_wait,
            recognize_ms: recog_ms,
            fallback_ms: 0,
        }
    }

    /// Auto 后端路由：已安装 PaddleOCR 即优先使用，失败 fallback WinRT。
    async fn route_auto(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        snapshot: &OcrRuntimeSnapshot,
        request_png_size: (u32, u32),
    ) -> RouteOutcome {
        if ctx.should_stop() {
            let decision = RouteDecision {
                configured_backend: OcrBackendKind::Auto,
                selected_backend: OcrBackendKind::Windows,
                fallback_reason: Some("请求已取消或超时".to_string()),
            };
            return RouteOutcome {
                decision,
                result: Err(stop_error(ctx)),
                start_wait_ms: 0,
                recognize_ms: 0,
                fallback_ms: 0,
            };
        }

        // 0.22.10: auto 语义升级——已安装 PaddleOCR 即优先使用
        let installed = self.is_paddleocr_installed().await;
        if !installed {
            let (res, ms) = self.do_winrt_recognize(&png_data, ctx).await;
            return RouteOutcome {
                decision: RouteDecision {
                    configured_backend: OcrBackendKind::Auto,
                    selected_backend: OcrBackendKind::Windows,
                    fallback_reason: Some("未安装 PaddleOCR".to_string()),
                },
                result: res,
                start_wait_ms: 0,
                recognize_ms: ms,
                fallback_ms: 0,
            };
        }

        // 已安装：尝试 PaddleOCR
        let (res, start_wait, recog_ms) = {
            self.idle_cancel.notify_waiters();
            self.do_paddleocr_recognize(png_data.clone(), ctx, false, request_png_size)
                .await
        };

        let used_paddleocr = match &res {
            Ok(_) => true,
            Err(e) => !e.is_hot_only_not_ready(),
        };

        if used_paddleocr {
            self.schedule_idle_stop(*snapshot);

            // 检查是否需要 fallback WinRT
            if let Err(ref paddle_err) = res {
                // 输入本身的问题不回退——换后端无济于事；
                // 后端基础设施问题才回退 WinRT
                let should_fallback = !matches!(
                    paddle_err.category,
                    OcrErrorCategory::Cancelled
                        | OcrErrorCategory::DecodeError
                        | OcrErrorCategory::InputTooLarge
                );

                if should_fallback {
                    return self
                        .auto_fallback_to_winrt(png_data, ctx, paddle_err, start_wait, recog_ms)
                        .await;
                }
            }

            // 无需 fallback——返回 PaddleOCR 结果
            RouteOutcome {
                decision: RouteDecision {
                    configured_backend: OcrBackendKind::Auto,
                    selected_backend: OcrBackendKind::PaddleOcr,
                    fallback_reason: None,
                },
                result: res,
                start_wait_ms: start_wait,
                recognize_ms: recog_ms,
                fallback_ms: 0,
            }
        } else {
            // 未使用 PaddleOCR（lease 未就绪）——走 WinRT
            let (res2, ms) = self.do_winrt_recognize(&png_data, ctx).await;
            RouteOutcome {
                decision: RouteDecision {
                    configured_backend: OcrBackendKind::Auto,
                    selected_backend: OcrBackendKind::Windows,
                    fallback_reason: Some("PaddleOCR 未就绪".to_string()),
                },
                result: res2,
                start_wait_ms: 0,
                recognize_ms: ms,
                fallback_ms: 0,
            }
        }
    }

    /// auto 模式 PaddleOCR 失败后的 WinRT fallback 逻辑。
    ///
    /// **不变量**：deadline/cancel 后不得继续 WinRT fallback。
    async fn auto_fallback_to_winrt(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        paddle_err: &StructuredOcrError,
        start_wait: u64,
        recog_ms: u64,
    ) -> RouteOutcome {
        tracing::info!(error = %paddle_err, "auto 模式 PaddleOCR 识别失败，fallback 到 WinRT");

        // deadline/cancel 后不得继续 WinRT fallback
        if ctx.should_stop() {
            let err = stop_error(ctx);
            let decision = RouteDecision {
                configured_backend: OcrBackendKind::Auto,
                selected_backend: OcrBackendKind::Windows,
                fallback_reason: Some(format!("PaddleOCR 失败后取消: {err}")),
            };
            return RouteOutcome {
                decision,
                result: Err(err),
                start_wait_ms: start_wait,
                recognize_ms: recog_ms,
                fallback_ms: 0,
            };
        }

        let (fb_res, fb_ms) = self.do_winrt_recognize(&png_data, ctx).await;
        let decision = RouteDecision {
            configured_backend: OcrBackendKind::Auto,
            selected_backend: OcrBackendKind::Windows,
            fallback_reason: Some(format!("PaddleOCR 失败 fallback: {paddle_err}")),
        };

        match fb_res {
            Ok(ocr_result) => RouteOutcome {
                decision,
                result: Ok(ocr_result),
                start_wait_ms: start_wait,
                recognize_ms: recog_ms,
                fallback_ms: fb_ms,
            },
            Err(fb_err) => RouteOutcome {
                decision,
                result: Err(fb_err),
                start_wait_ms: start_wait,
                recognize_ms: recog_ms,
                fallback_ms: fb_ms,
            },
        }
    }

    /// 将 `RouteOutcome` 组装为最终 `RouteResult`。
    pub(super) fn assemble_route_result(
        outcome: RouteOutcome,
        total_elapsed_ms: u64,
    ) -> RouteResult {
        let RouteOutcome {
            decision,
            result,
            start_wait_ms,
            recognize_ms,
            fallback_ms,
        } = outcome;

        match result {
            Ok(ocr_result) => {
                if fallback_ms > 0 {
                    RouteResult::fallback_success(
                        decision,
                        ocr_result,
                        total_elapsed_ms,
                        start_wait_ms,
                        recognize_ms,
                        fallback_ms,
                    )
                } else {
                    RouteResult::success(
                        decision,
                        ocr_result,
                        total_elapsed_ms,
                        start_wait_ms,
                        recognize_ms,
                    )
                }
            }
            Err(e) => RouteResult::error(
                decision,
                e,
                total_elapsed_ms,
                start_wait_ms,
                recognize_ms,
                fallback_ms,
            ),
        }
    }

    /// 从 `RouteResult` 构建诊断快照并更新缓存。
    ///
    /// 0.22.8-D: 诊断字段从 engine_service 改为 executor 状态投影。
    pub(super) async fn build_and_update_diagnosis(
        &self,
        ctx: &OcrRequestContext,
        snapshot: &OcrRuntimeSnapshot,
        route_result: &RouteResult,
    ) {
        let lightweight_diagnosis = OcrRouteDiagnosis {
            configured_backend: snapshot.backend,
            last_selected_backend: Some(route_result.decision.selected_backend),
            last_fallback_reason: route_result.decision.fallback_reason.clone(),
            paddleocr_installed: self.is_paddleocr_installed().await,
            paddleocr_service_state: self.paddleocr_service_state().await,
            paddleocr_model_state: self.paddleocr_model_state().await,
            paddleocr_model_id: Some("PP-OCRv6".to_string()),
            paddleocr_model_revision: Some("ppocrv6-tiny".to_string()),
            paddleocr_instance_id: None,
            paddleocr_actual_backend: Some("onnx-ocr".to_string()),
            in_flight_count: self.in_flight.load(Ordering::SeqCst) as usize,
            lifecycle: format!("{:?}", snapshot.lifecycle),
            idle_ttl_seconds: snapshot.idle_ttl_seconds,
            last_error: route_result.error.clone(),
            winrt_available_languages: Vec::new(),
            winrt_engine_language: None,
            last_total_elapsed_ms: Some(route_result.total_elapsed_ms),
            last_start_wait_ms: Some(route_result.start_wait_ms),
            last_recognize_ms: Some(route_result.recognize_ms),
            last_fallback_ms: if route_result.fallback_ms > 0 {
                Some(route_result.fallback_ms)
            } else {
                None
            },
        };
        tracing::debug!(
            request_id = %ctx.request_id,
            configured_backend = %route_result.decision.configured_backend,
            selected_backend = %route_result.decision.selected_backend,
            fallback_reason = ?route_result.decision.fallback_reason,
            success = route_result.result.is_some(),
            total_elapsed_ms = route_result.total_elapsed_ms,
            start_wait_ms = route_result.start_wait_ms,
            recognize_ms = route_result.recognize_ms,
            fallback_ms = route_result.fallback_ms,
            "OCR 路由完成"
        );
        self.update_diagnosis(lightweight_diagnosis);
    }

    /// 路由前置检查：should_stop + 输入资源预算校验。
    ///
    /// 返回 `Ok(request_png_size)` 表示前置检查通过；
    /// 返回 `Err(RouteResult)` 表示前置检查失败，直接返回错误结果。
    pub(super) fn preflight_check(
        &self,
        ctx: &OcrRequestContext,
        png_data: &Bytes,
        snapshot: &OcrRuntimeSnapshot,
        total_start: Instant,
    ) -> Result<(u32, u32), RouteResult> {
        // 全局前置检查
        if ctx.should_stop() {
            let decision = RouteDecision {
                configured_backend: snapshot.backend,
                selected_backend: snapshot.backend,
                fallback_reason: None,
            };
            let err = stop_error(ctx);
            let total_elapsed_ms = total_start.elapsed().as_millis() as u64;
            return Err(RouteResult::error(decision, err, total_elapsed_ms, 0, 0, 0));
        }

        // 输入资源预算
        let request_png_size = match crate::domain::ocr::input_budget::validate_ocr_input(png_data)
        {
            Ok(size) => size,
            Err(e) => {
                let decision = RouteDecision {
                    configured_backend: snapshot.backend,
                    selected_backend: snapshot.backend,
                    fallback_reason: None,
                };
                let total_elapsed_ms = total_start.elapsed().as_millis() as u64;
                tracing::warn!(
                    category = %e.category,
                    error = %e.message,
                    "OCR 输入资源预算校验失败"
                );
                return Err(RouteResult::error(decision, e, total_elapsed_ms, 0, 0, 0));
            }
        };

        Ok(request_png_size)
    }

    /// 构建诊断快照（从缓存或配置构建，被 `diagnose()` 调用）。
    pub(super) async fn build_diagnosis(&self) -> OcrRouteDiagnosis {
        let cached = {
            let r = self.last_diagnosis.read();
            if let Ok(r) = r {
                r.as_ref().cloned()
            } else {
                None
            }
        };
        let (winrt_langs, winrt_engine_lang) = self.winrt_diagnostics().await;
        let paddleocr_installed = self.is_paddleocr_installed().await;
        let paddleocr_service_state = self.paddleocr_service_state().await;
        let paddleocr_model_state = self.paddleocr_model_state().await;
        let in_flight_count = self.in_flight.load(Ordering::SeqCst) as usize;

        if let Some(mut d) = cached {
            d.paddleocr_installed = paddleocr_installed;
            d.paddleocr_service_state = paddleocr_service_state;
            d.paddleocr_model_state = paddleocr_model_state;
            d.paddleocr_model_id = Some("PP-OCRv6".to_string());
            d.paddleocr_model_revision = Some("ppocrv6-tiny".to_string());
            d.paddleocr_instance_id = None;
            d.paddleocr_actual_backend = Some("onnx-ocr".to_string());
            d.in_flight_count = in_flight_count;
            d.winrt_available_languages = winrt_langs;
            d.winrt_engine_language = winrt_engine_lang;
            return d;
        }

        let cfg = crate::domain::config::ocr_config::get_ocr_config();
        OcrRouteDiagnosis {
            configured_backend: cfg.backend,
            last_selected_backend: None,
            last_fallback_reason: None,
            paddleocr_installed,
            paddleocr_service_state,
            paddleocr_model_state,
            paddleocr_model_id: Some("PP-OCRv6".to_string()),
            paddleocr_model_revision: Some("ppocrv6-tiny".to_string()),
            paddleocr_instance_id: None,
            paddleocr_actual_backend: Some("onnx-ocr".to_string()),
            in_flight_count,
            lifecycle: cfg.lifecycle.to_string(),
            idle_ttl_seconds: cfg.idle_ttl_seconds,
            last_error: None,
            winrt_available_languages: winrt_langs,
            winrt_engine_language: winrt_engine_lang,
            last_total_elapsed_ms: None,
            last_start_wait_ms: None,
            last_recognize_ms: None,
            last_fallback_ms: None,
        }
    }
}

/// 根据上下文返回取消或超时错误。
pub(super) fn stop_error(ctx: &OcrRequestContext) -> StructuredOcrError {
    if ctx.is_cancelled() {
        StructuredOcrError::cancelled()
    } else {
        StructuredOcrError::timeout()
    }
}
