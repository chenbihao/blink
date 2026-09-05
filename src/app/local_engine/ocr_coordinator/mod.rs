//! OCR Coordinator — 路由 + 生命周期 + 并发管理（0.22.8-D）。
//!
//! `OcrCoordinator` 是 `OcrBackendRouter` 的具体实现，持有 `OnnxOcrExecutor`
//! 替代 Python HTTP 子进程，负责：路由 / 生命周期 / ONNX in-process 识别 / 诊断。
//!
//! ## 0.22.8-D 变更：Python HTTP → ONNX in-process
//!
//! - 启动路径：`engine_service.start()` → `executor.ensure_ready()`
//! - 识别路径：HTTP `/recognize` → `executor.recognize()`
//! - Lease 不再携带 endpoint/token，只保留 InFlightGuard
//! - idle TTL / shutdown / repair 统一调用 `executor.shutdown()`
//!
//! ## 并发模型与竞态防护（0.22.5 重构，0.22.8-D 适配）
//!
//! - **shared startup singleflight**：启动请求通过 `watch::Sender<LifecycleState>` 合并。
//!   状态包括 `generation`，支持 Idle/Starting/Ready/Stopping/Failed。
//!   `watch` 不会丢通知——waiter 通过 `changed()` 可靠等待状态转换。
//!   **关键**：winner 只负责触发独立后台启动 task，task 持有自己的 120s 上限，
//!   不持有任何业务请求 context——单个请求取消不会取消共享启动。
//! - **请求取消独立**：单个请求取消只能取消自己的等待，不得取消共享启动。
//!   waiter 通过 `select!` 同时监听 watch changed、请求 cancellation 和请求 deadline。
//! - **generation + instance token**：每次成功 start 递增 generation 并记录 instance token。
//!   stop 时验证 generation + token 匹配，旧 timer 不能停止新实例。
//! - **in-flight tracker**：`AtomicU32` 计数器，只统计真正取得 lease 的请求。
//!   RAII guard 确保即使 panic 也能正确递减。
//! - **idle TTL**：最后一个 in-flight lease 释放后启动定时器。
//!   timer fire 时二次验证 in-flight、generation、instance token。
//! - **shutdown**：`shutdown()` 拒绝新 lease，取消所有 pending idle 定时器并等待 stop 完成。
//! - **repair 闭环**：`begin_repair()` 阻止新 lease 并条件停止当前实例；
//!   `end_repair()` 恢复正常生命周期。
//!
//! ## 子模块（0.22 结构拆分）
//!
//! - [`singleflight`]：lease / in-flight / LifecycleState / shared startup 并发原语
//! - [`mapping`]：ONNX executor → OcrResult 契约映射（纯函数，含 line grouping）
//! - [`lifecycle`]：idle TTL / StopAfterUse 回收
//! - [`diagnostics`]：executor 状态投影与诊断辅助
//! - [`tests`]：单元测试
mod diagnostics;
mod lifecycle;
mod mapping;
mod singleflight;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use crate::infra::local_engine::onnx_ocr::{OcrExecutor, OnnxOcrExecutor, RecognizeRequest};
use bytes::Bytes;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::domain::capability::builtins::ocr_engine::{OcrResult, backend as get_global_backend};
use crate::domain::config::ocr_config::{OcrRuntimeSnapshot, get_ocr_config};
use crate::domain::ocr::config::OcrBackendKind;
use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::StructuredOcrError;
use crate::domain::ocr::router::{OcrBackendRouter, OcrRouteDiagnosis, RouteDecision, RouteResult};
use crate::infra::local_engine::runtime::EngineId;

use singleflight::{LeaseError, LifecycleState};

const PADDLEOCR_ENGINE_ID_STR: &str = "paddleocr";

/// Task 6: RAII guard for repair mode——确保无论 repair 路径如何结束，
/// repair mode 都会被退出。
///
/// `begin_repair()` 返回后创建此 guard，drop 时自动调用 `end_repair()`。
/// 修复成功与否由 command 层返回值表达；guard 只负责恢复生命周期。
pub(crate) struct RepairGuard {
    coordinator: std::sync::Weak<OcrCoordinator>,
    armed: std::sync::atomic::AtomicBool,
}

impl RepairGuard {
    fn new(coordinator: &Arc<OcrCoordinator>) -> Self {
        Self {
            coordinator: Arc::downgrade(coordinator),
            armed: std::sync::atomic::AtomicBool::new(true),
        }
    }
}

impl Drop for RepairGuard {
    fn drop(&mut self) {
        if self.armed.load(Ordering::SeqCst)
            && let Some(coord) = self.coordinator.upgrade()
        {
            coord.end_repair();
        }
    }
}

// ── OcrCoordinator ─────────────────────────────────────────────────────────

pub struct OcrCoordinator {
    /// ONNX in-process executor（0.22.8-D 替代 engine_service 做识别）。
    /// `None` 表示 executor 未注入（测试 / 未安装时）。
    ///
    /// 0.22.8-F: 使用 `RwLock` 包装，支持运行时热注入——
    /// 启动时 deployment 可能不存在（返回 None），用户安装后
    /// 通过 `inject_executor()` 替换。
    executor: std::sync::RwLock<Option<Arc<OnnxOcrExecutor>>>,
    #[allow(dead_code)]
    paddleocr_engine_id: EngineId,
    in_flight: Arc<AtomicU32>,
    lifecycle_tx: watch::Sender<LifecycleState>,
    lifecycle_rx: watch::Receiver<LifecycleState>,
    idle_cancel: Arc<Notify>,
    start_elapsed_ms: Arc<std::sync::Mutex<Option<u64>>>,
    last_diagnosis: Arc<std::sync::RwLock<Option<OcrRouteDiagnosis>>>,
    /// repair 模式标志——为 true 时拒绝新 lease。
    repair_mode: Arc<AtomicBool>,
    /// 原子 startup gate——确保 Idle → Starting 转换只有一个 winner。
    ///
    /// 使用 `compare_exchange(false, true)` 做原子 CAS：
    /// - 第一个成功者为 winner，负责 spawn shared startup task
    /// - 其他请求看到 true，知道已有 winner，等待 watch changed
    /// - 状态离开 Starting（Ready/Failed）时重置为 false
    starting_gate: Arc<AtomicBool>,
    /// EngineManager 弱引用（0.22.9 接线）——懒启动成功/闲置回收时
    /// 同步引擎卡片状态（start_inprocess / stop_inprocess）。
    /// Weak 避免与 AppHandle 状态树形成强引用环。
    engine_service: std::sync::OnceLock<std::sync::Weak<crate::app::local_engine::EngineManager>>,
}

impl OcrCoordinator {
    /// 创建 OcrCoordinator（0.22.8-D：不再需要 EngineManager）。
    ///
    /// executor 传 `None` 时，PaddleOCR 路径将不可用（auto 回退 WinRT）。
    pub fn new(executor: Option<Arc<OnnxOcrExecutor>>) -> Arc<Self> {
        let paddleocr_engine_id = EngineId::new(PADDLEOCR_ENGINE_ID_STR).unwrap();
        let (lifecycle_tx, lifecycle_rx) = watch::channel(LifecycleState::Idle { generation: 0 });
        Arc::new(Self {
            executor: std::sync::RwLock::new(executor),
            paddleocr_engine_id,
            in_flight: Arc::new(AtomicU32::new(0)),
            lifecycle_tx,
            lifecycle_rx,
            idle_cancel: Arc::new(Notify::new()),
            start_elapsed_ms: Arc::new(std::sync::Mutex::new(None)),
            last_diagnosis: Arc::new(std::sync::RwLock::new(None)),
            repair_mode: Arc::new(AtomicBool::new(false)),
            starting_gate: Arc::new(AtomicBool::new(false)),
            engine_service: std::sync::OnceLock::new(),
        })
    }

    /// 接线 EngineManager 弱引用（main.rs 构造后调用，非必需）。
    ///
    /// 接线后：OCR 懒启动成功会把引擎卡片同步为 Running，
    /// idle TTL / StopAfterUse 回收后同步为 Stopped。
    pub fn attach_engine_service(&self, service: &Arc<crate::app::local_engine::EngineManager>) {
        let _ = self.engine_service.set(std::sync::Arc::downgrade(service));
    }

    fn config_snapshot(&self) -> OcrRuntimeSnapshot {
        get_ocr_config().to_snapshot()
    }

    fn lifecycle_state(&self) -> LifecycleState {
        self.lifecycle_rx.borrow().clone()
    }

    /// 检查是否处于 repair 模式（拒绝新 lease）。
    fn is_in_repair_mode(&self) -> bool {
        self.repair_mode.load(Ordering::SeqCst)
    }

    /// 进入 repair 模式——拒绝新 lease，等待 in-flight 完成，停止 executor。
    ///
    /// 调用方（`repair_paddleocr` command）在执行清理/重装前调用此方法，
    /// 确保不会有新请求引用旧实例。
    ///
    /// 0.22.8-D: 停止路径从 `engine_service.stop()` 改为 `executor.shutdown()`。
    ///
    /// Task 6: 返回 `RepairGuard` RAII，确保无论 repair 路径如何结束，
    /// `end_repair()` 都会被调用。
    pub async fn begin_repair(self: &Arc<Self>) -> RepairGuard {
        tracing::info!("OcrCoordinator: 进入 repair 模式，拒绝新 lease");
        self.repair_mode.store(true, Ordering::SeqCst);

        // 取消所有 pending idle TTL 定时器
        self.idle_cancel.notify_waiters();

        // 如果当前是 Ready，进入 Stopping 并停止 executor
        let current_state = self.lifecycle_state();
        let target_gen = if let LifecycleState::Ready { generation, .. } = &current_state {
            self.lifecycle_tx
                .send(LifecycleState::Stopping {
                    generation: *generation,
                })
                .ok();
            Some(*generation)
        } else {
            None
        };

        // 等待 in-flight 请求完成（最多等 5s）
        let mut waited = 0u64;
        while self.in_flight.load(Ordering::SeqCst) > 0 && waited < 5000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 10;
        }
        if self.in_flight.load(Ordering::SeqCst) > 0 {
            tracing::warn!(
                in_flight = self.in_flight.load(Ordering::SeqCst),
                "repair: in-flight 请求未在 5s 内完成，继续强制停止"
            );
        }

        // 0.22.8-D: 停止 executor（无条件——repair 需要确保 Session drop）
        let executor_for_shutdown = self.executor.read().unwrap().clone();
        if let Some(ref executor) = executor_for_shutdown {
            executor.shutdown().await;
        }

        let reset_gen = target_gen.unwrap_or_else(|| self.lifecycle_state().generation());
        *self.start_elapsed_ms.lock().unwrap() = None;
        self.lifecycle_tx
            .send(LifecycleState::Idle {
                generation: reset_gen + 1,
            })
            .ok();

        tracing::info!("OcrCoordinator: repair 前置完成，executor 已停止");

        // Task 6: 返回 RAII guard
        RepairGuard::new(self)
    }

    /// 退出 repair 模式——恢复正常生命周期。
    ///
    /// 调用方在完成 repair（清理 + 重装 + 验证）后调用此方法。
    pub fn end_repair(&self) {
        tracing::info!("OcrCoordinator: 退出 repair 模式，恢复正常生命周期");
        self.repair_mode.store(false, Ordering::SeqCst);
        // 确保状态为 Idle——下次请求会触发正常启动
        let current_gen = self.lifecycle_state().generation();
        *self.start_elapsed_ms.lock().unwrap() = None;
        self.lifecycle_tx
            .send(LifecycleState::Idle {
                generation: current_gen + 1,
            })
            .ok();
    }

    /// 等待 deadline 到来（如果有的话），用于 select! 分支。
    async fn sleep_until_deadline(&self, ctx: &OcrRequestContext) {
        if let Some(deadline) = ctx.deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        } else {
            std::future::pending::<()>().await;
        }
    }

    /// 0.22.8-D: ONNX in-process 识别。通过 executor.recognize() 执行推理。
    ///
    /// **取消覆盖**：通过 select! 同时监听 ctx.cancellation.cancelled() 和 deadline。
    async fn do_paddleocr_recognize(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        hot_only: bool,
        request_png_size: (u32, u32),
    ) -> (Result<OcrResult, StructuredOcrError>, u64, u64) {
        let total_start = Instant::now();
        let lease = match self.acquire_lease(ctx, hot_only).await {
            Ok(l) => l,
            Err(LeaseError::Cancelled) => return (Err(StructuredOcrError::cancelled()), 0, 0),
            Err(LeaseError::Timeout) => return (Err(StructuredOcrError::timeout()), 0, 0),
            Err(LeaseError::NotReady) => {
                return (
                    Err(StructuredOcrError::model_not_ready("not_ready_hot_only")),
                    0,
                    0,
                );
            }
            Err(LeaseError::Error(e)) => return (Err(e), 0, 0),
        };
        let start_wait_ms = total_start.elapsed().as_millis() as u64;
        let recognize_start = Instant::now();

        let executor = self.executor.read().unwrap().clone();
        let executor = match &executor {
            Some(e) => e.clone(),
            None => {
                return (
                    Err(StructuredOcrError::backend_unavailable(
                        "ONNX executor 未注入",
                    )),
                    start_wait_ms,
                    0,
                );
            }
        };

        // 0.22.8-D: 构造 RecognizeRequest，通过 executor.recognize() 执行推理
        let request = RecognizeRequest {
            png_data: png_data.clone(),
            cancellation: ctx.cancellation.clone(),
            deadline: ctx.deadline.map(tokio::time::Instant::from_std),
        };

        // 执行识别——取消覆盖在 executor 内部通过 CancellationToken 处理
        let result = executor
            .recognize(request)
            .await
            .map_err(StructuredOcrError::from)
            .and_then(|ocr_result| {
                // 0.22.8-D: 映射 OcrResult —— executor 返回的已经是 OcrResult，
                // 通过 mapping 模块做 line grouping 和尺寸校验
                mapping::map_executor_result(ocr_result, request_png_size)
            });

        let recognize_ms = recognize_start.elapsed().as_millis() as u64;
        // lease 在此 drop——释放 InFlightGuard
        drop(lease);
        (result, start_wait_ms, recognize_ms)
    }

    /// WinRT 识别。只借用 Bytes slice，不复制。
    ///
    /// **取消覆盖**：WinRT 调用通过 select! 同时监听 ctx.cancellation.cancelled() 和 deadline。
    async fn do_winrt_recognize(
        &self,
        png_data: &Bytes,
        ctx: &OcrRequestContext,
    ) -> (Result<OcrResult, StructuredOcrError>, u64) {
        let start = Instant::now();
        let backend = get_global_backend();

        // Task 5: 取消 + deadline 覆盖：WinRT 调用 + ctx 取消 + deadline
        let result = tokio::select! {
            r = backend.recognize(png_data.as_ref()) => r.map_err(|e| StructuredOcrError::from(&e)),
            _ = ctx.cancellation.cancelled() => return (Err(StructuredOcrError::cancelled()), start.elapsed().as_millis() as u64),
            _ = self.sleep_until_deadline(ctx) => return (Err(StructuredOcrError::timeout()), start.elapsed().as_millis() as u64),
        };

        let elapsed_ms = start.elapsed().as_millis() as u64;
        (result, elapsed_ms)
    }

    /// 关闭 OCR Coordinator（0.22.8-D: 停止 executor）。
    pub async fn shutdown(&self) {
        tracing::info!("OcrCoordinator shutdown: 取消 idle 定时器并停止 ONNX executor");
        // 拒绝新 lease
        let current_state = self.lifecycle_state();
        let generation = current_state.generation();
        self.lifecycle_tx
            .send(LifecycleState::Stopping { generation })
            .ok();
        // 取消所有 pending idle TTL 定时器
        self.idle_cancel.notify_waiters();
        // 等待 in-flight 请求完成（最多等 1s）
        let mut waited = 0u64;
        while self.in_flight.load(Ordering::SeqCst) > 0 && waited < 1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 10;
        }
        // 0.22.8-D: 停止 executor
        let executor_for_shutdown = self.executor.read().unwrap().clone();
        if let Some(ref executor) = executor_for_shutdown {
            executor.shutdown().await;
        }
        // 重置状态机
        let final_gen = self.lifecycle_state().generation();
        *self.start_elapsed_ms.lock().unwrap() = None;
        self.lifecycle_tx
            .send(LifecycleState::Idle {
                generation: final_gen + 1,
            })
            .ok();
        tracing::info!("OcrCoordinator shutdown 完成");
    }

    /// 通知 coordinator 外部管理命令改变了引擎状态。
    pub async fn notify_external_state_change(&self) {
        tracing::info!(
            in_flight = self.in_flight.load(Ordering::SeqCst),
            "OcrCoordinator: 外部管理命令改变了引擎状态，清理缓存"
        );
        self.idle_cancel.notify_waiters();
        let current_gen = self.lifecycle_state().generation();
        *self.start_elapsed_ms.lock().unwrap() = None;
        self.lifecycle_tx
            .send(LifecycleState::Idle {
                generation: current_gen + 1,
            })
            .ok();
    }

    /// 运行时注入/替换 ONNX executor。
    ///
    /// 0.22.8-F: 启动时 deployment 可能不存在（executor=None），用户安装后
    /// 通过此方法注入新构建的 executor。如果已有旧 executor，先 shutdown 再替换。
    pub async fn inject_executor(&self, new_executor: Arc<OnnxOcrExecutor>) {
        // 如果已有旧 executor，先停止
        let old = {
            let mut w = self.executor.write().unwrap();
            let old = w.take();
            *w = Some(new_executor);
            old
        };
        if let Some(old) = old {
            tracing::info!("inject_executor: 停止旧 executor");
            old.shutdown().await;
        }
        // 重置状态机——下次 OCR 请求会触发 lazy load
        let current_gen = self.lifecycle_state().generation();
        *self.start_elapsed_ms.lock().unwrap() = None;
        self.lifecycle_tx
            .send(LifecycleState::Idle {
                generation: current_gen + 1,
            })
            .ok();
        tracing::info!("OcrCoordinator: executor 已注入，状态机已重置");
    }
}

// ── OcrBackendRouter impl ─────────────────────────────────────────────────

#[async_trait::async_trait]
impl OcrBackendRouter for OcrCoordinator {
    async fn recognize(&self, png_data: Bytes, ctx: &OcrRequestContext) -> RouteResult {
        let total_start = Instant::now();
        let snapshot = self.config_snapshot();

        // 全局前置检查
        if ctx.should_stop() {
            let decision = RouteDecision {
                configured_backend: snapshot.backend,
                selected_backend: snapshot.backend,
                fallback_reason: None,
            };
            let err = if ctx.is_cancelled() {
                StructuredOcrError::cancelled()
            } else {
                StructuredOcrError::timeout()
            };
            let total_elapsed_ms = total_start.elapsed().as_millis() as u64;
            return RouteResult::error(decision, err, total_elapsed_ms, 0, 0, 0);
        }

        // 输入资源预算（0.22.6.1）——在发送给任何后端之前执行：
        // 非空 / PNG header / compressed bytes / 单边尺寸 / decoded 像素预算。
        // 返回的尺寸复用为响应一致性校验基准，不再二次解析。
        let request_png_size = match crate::domain::ocr::input_budget::validate_ocr_input(&png_data)
        {
            Ok(size) => size,
            Err(e) => {
                let decision = RouteDecision {
                    configured_backend: snapshot.backend,
                    selected_backend: snapshot.backend,
                    fallback_reason: None,
                };
                let total_elapsed_ms = total_start.elapsed().as_millis() as u64;
                tracing::warn!(category = %e.category, error = %e.message, "OCR 输入资源预算校验失败");
                return RouteResult::error(decision, e, total_elapsed_ms, 0, 0, 0);
            }
        };

        let (decision, result, start_wait_ms, recognize_ms, fallback_ms) = match snapshot.backend {
            OcrBackendKind::Windows => {
                if ctx.should_stop() {
                    let err = if ctx.is_cancelled() {
                        StructuredOcrError::cancelled()
                    } else {
                        StructuredOcrError::timeout()
                    };
                    let decision = RouteDecision {
                        configured_backend: OcrBackendKind::Windows,
                        selected_backend: OcrBackendKind::Windows,
                        fallback_reason: None,
                    };
                    (decision, Err(err), 0u64, 0u64, 0u64)
                } else {
                    let (res, ms) = self.do_winrt_recognize(&png_data, ctx).await;
                    let decision = RouteDecision {
                        configured_backend: OcrBackendKind::Windows,
                        selected_backend: OcrBackendKind::Windows,
                        fallback_reason: None,
                    };
                    (decision, res, 0u64, ms, 0u64)
                }
            }
            OcrBackendKind::PaddleOcr => {
                // Task 3: InFlightGuard 现在绑定到 Lease，不在调用端独立创建
                let (res, start_wait, recog_ms) = {
                    self.idle_cancel.notify_waiters();
                    self.do_paddleocr_recognize(png_data.clone(), ctx, false, request_png_size)
                        .await
                };
                self.schedule_idle_stop(snapshot);
                let decision = RouteDecision {
                    configured_backend: OcrBackendKind::PaddleOcr,
                    selected_backend: OcrBackendKind::PaddleOcr,
                    fallback_reason: None,
                };
                (decision, res, start_wait, recog_ms, 0u64)
            }
            OcrBackendKind::Auto => {
                if ctx.should_stop() {
                    let decision = RouteDecision {
                        configured_backend: OcrBackendKind::Auto,
                        selected_backend: OcrBackendKind::Windows,
                        fallback_reason: Some("请求已取消或超时".to_string()),
                    };
                    let err = if ctx.is_cancelled() {
                        StructuredOcrError::cancelled()
                    } else {
                        StructuredOcrError::timeout()
                    };
                    (decision, Err(err), 0u64, 0u64, 0u64)
                } else {
                    // 0.22.10: auto 语义升级——已安装 PaddleOCR 即优先使用
                    //（允许 on-demand 冷启动）；未安装则直接 WinRT，无数秒等待
                    let installed = self.is_paddleocr_installed().await;
                    if !installed {
                        let (res, ms) = self.do_winrt_recognize(&png_data, ctx).await;
                        let decision = RouteDecision {
                            configured_backend: OcrBackendKind::Auto,
                            selected_backend: OcrBackendKind::Windows,
                            fallback_reason: Some("未安装 PaddleOCR".to_string()),
                        };
                        (decision, res, 0u64, ms, 0u64)
                    } else {
                        let (res, start_wait, recog_ms) = {
                            self.idle_cancel.notify_waiters();
                            self.do_paddleocr_recognize(
                                png_data.clone(),
                                ctx,
                                false,
                                request_png_size,
                            )
                            .await
                        };

                        let used_paddleocr =
                            match &res {
                                Ok(_) => true,
                                Err(e) => e.category
                                    != crate::domain::ocr::error::OcrErrorCategory::ModelNotReady
                                    || !e.message.contains("not_ready_hot_only"),
                            };

                        if used_paddleocr {
                            self.schedule_idle_stop(snapshot);
                            if let Err(ref paddle_err) = res {
                                // 输入本身的问题（取消/解码失败/超预算）不回退——
                                // 换后端无济于事；后端基础设施问题才回退 WinRT
                                let should_fallback = !matches!(
                                paddle_err.category,
                                crate::domain::ocr::error::OcrErrorCategory::Cancelled
                                    | crate::domain::ocr::error::OcrErrorCategory::DecodeError
                                    | crate::domain::ocr::error::OcrErrorCategory::InputTooLarge
                            );
                                if should_fallback {
                                    tracing::info!(error = %paddle_err, "auto 模式 PaddleOCR 识别失败，fallback 到 WinRT");
                                    // deadline/cancel 后不得继续 WinRT fallback
                                    if ctx.should_stop() {
                                        let err = if ctx.is_cancelled() {
                                            StructuredOcrError::cancelled()
                                        } else {
                                            StructuredOcrError::timeout()
                                        };
                                        let decision = RouteDecision {
                                            configured_backend: OcrBackendKind::Auto,
                                            selected_backend: OcrBackendKind::Windows,
                                            fallback_reason: Some(format!(
                                                "PaddleOCR 失败后取消: {err}"
                                            )),
                                        };
                                        (decision, Err(err), start_wait, recog_ms, 0u64)
                                    } else {
                                        let (fb_res, fb_ms) =
                                            self.do_winrt_recognize(&png_data, ctx).await;
                                        let decision = RouteDecision {
                                            configured_backend: OcrBackendKind::Auto,
                                            selected_backend: OcrBackendKind::Windows,
                                            fallback_reason: Some(format!(
                                                "PaddleOCR 失败 fallback: {paddle_err}"
                                            )),
                                        };
                                        match fb_res {
                                            Ok(ocr_result) => (
                                                decision,
                                                Ok(ocr_result),
                                                start_wait,
                                                recog_ms,
                                                fb_ms,
                                            ),
                                            Err(fb_err) => {
                                                (decision, Err(fb_err), start_wait, recog_ms, fb_ms)
                                            }
                                        }
                                    }
                                } else {
                                    let decision = RouteDecision {
                                        configured_backend: OcrBackendKind::Auto,
                                        selected_backend: OcrBackendKind::PaddleOcr,
                                        fallback_reason: None,
                                    };
                                    (decision, res, start_wait, recog_ms, 0u64)
                                }
                            } else {
                                let decision = RouteDecision {
                                    configured_backend: OcrBackendKind::Auto,
                                    selected_backend: OcrBackendKind::PaddleOcr,
                                    fallback_reason: None,
                                };
                                (decision, res, start_wait, recog_ms, 0u64)
                            }
                        } else {
                            // 未使用 PaddleOCR（lease 未就绪）——走 WinRT
                            let (res2, ms) = self.do_winrt_recognize(&png_data, ctx).await;
                            let decision = RouteDecision {
                                configured_backend: OcrBackendKind::Auto,
                                selected_backend: OcrBackendKind::Windows,
                                fallback_reason: Some("PaddleOCR 未就绪".to_string()),
                            };
                            (decision, res2, 0u64, ms, 0u64)
                        }
                    }
                }
            }
        };

        let total_elapsed_ms = total_start.elapsed().as_millis() as u64;
        let route_result = match result {
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
        };

        let lightweight_diagnosis = OcrRouteDiagnosis {
            configured_backend: snapshot.backend,
            last_selected_backend: Some(route_result.decision.selected_backend),
            last_fallback_reason: route_result.decision.fallback_reason.clone(),
            // 0.22.8-D: 诊断字段从 engine_service 改为 executor 状态投影
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
        route_result
    }

    async fn diagnose(&self) -> OcrRouteDiagnosis {
        let cached = {
            let r = self.last_diagnosis.read();
            if let Ok(r) = r {
                r.as_ref().cloned()
            } else {
                None
            }
        };
        let (winrt_langs, winrt_engine_lang) = self.winrt_diagnostics().await;
        // 0.22.8-D: 诊断从 executor 状态获取
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

        let cfg = get_ocr_config();
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

// ── ONNX executor 构建 helper（0.22.8-D）──────────────────────────────────

/// 从 active deployment 构建 OnnxOcrExecutor。
///
/// 读取 paddleocr engine 的 active deployment 目录，
/// 解析 det/rec/dict/dll 路径，构造 `OnnxOcrExecutor`。
/// 如果 deployment 不存在或路径缺失，返回 `None`。
///
/// 0.22.9：去掉未使用的 EngineManager 参数——executor 只读 active 部署，
/// OCR 请求路径（懒启动）也要调用本函数，不依赖调用方持有 manager。
/// 引擎卡片状态同步（fire-and-forget）。
///
/// OCR executor 的懒启动 / idle 回收与 EngineManager 的卡片状态是两套
/// 状态机——此函数把 executor 生命周期变化投影到引擎卡片（Running/Stopped），
/// 失败只记 debug 日志（best effort，不影响 OCR 请求）。
fn sync_engine_card(
    service: Option<std::sync::Arc<crate::app::local_engine::EngineManager>>,
    engine_id: EngineId,
    running: bool,
) {
    let Some(service) = service else {
        return;
    };
    tokio::spawn(async move {
        let result = if running {
            service.start_inprocess(&engine_id).await
        } else {
            service.stop_inprocess(&engine_id).await
        };
        if let Err(e) = result {
            tracing::debug!(engine = %engine_id, running, %e, "OCR 卡片状态同步失败（best effort）");
        }
    });
}

pub fn build_onnx_executor_from_deployment() -> Option<Arc<OnnxOcrExecutor>> {
    use crate::infra::local_engine::deployment::DeploymentStore;
    use crate::infra::local_engine::onnx_ocr::pipeline::PipelineConfig;
    use crate::infra::local_engine::onnx_ocr::{OcrExecutorConfig, OnnxOcrExecutor};
    use crate::infra::local_engine::runtime::EngineId;

    let engine_id = EngineId::new(PADDLEOCR_ENGINE_ID_STR).ok()?;

    // 从 active deployment pointer 获取部署目录（ONNX in-process implementation
    // 空间 = engine 级兼容真源，0.22.9 映射不搬迁）
    let _ = engine_id;
    let (_pointer, dir) = DeploymentStore::active_dir(
        &crate::app::local_engine::paddleocr::onnx_inprocess_deployment_space(),
    )
    .ok()
    .flatten()?;

    // ONNX 模型文件名（与 asset-lock.json 一致）
    let det_model = dir.join("pp-ocrv6_tiny_det.onnx");
    let rec_model = dir.join("pp-ocrv6_tiny_rec.onnx");
    let dict_path = dir.join("ppocrv6_tiny_dict.txt");
    let dll_path = dir.join("onnxruntime.dll");

    // 检查所有文件是否存在
    if !det_model.exists() || !rec_model.exists() || !dict_path.exists() || !dll_path.exists() {
        tracing::warn!(
            dir = %dir.display(),
            det = det_model.exists(),
            rec = rec_model.exists(),
            dict = dict_path.exists(),
            dll = dll_path.exists(),
            "ONNX executor 路径不完整，executor 未注入"
        );
        return None;
    }

    let config = OcrExecutorConfig {
        pipeline: PipelineConfig {
            det_model,
            rec_model,
            dict_path,
            dll_path,
            intra_op: 1,
            inter_op: 1,
        },
        idle_ttl_secs: 300,
    };

    let executor = OnnxOcrExecutor::new(config);
    tracing::info!("OnnxOcrExecutor 已从 deployment 构建");
    Some(Arc::new(executor))
}
