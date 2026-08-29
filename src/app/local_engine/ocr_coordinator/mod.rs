//! OCR Coordinator — 路由 + 生命周期 + 并发管理（0.22.5）。
//!
//! `OcrCoordinator` 是 `OcrBackendRouter` 的具体实现，持有 `EngineManager`
//! 受限依赖，负责：路由 / 生命周期 / HTTP 识别 / 诊断。
//!
//! ## 并发模型与竞态防护（0.22.5 重构）
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
//! - [`client`]：PaddleOCR HTTP 请求（/recognize、/health）与 endpoint/token 获取
//! - [`mapping`]：Paddle 响应 → OcrResult 契约映射（纯函数）
//! - [`lifecycle`]：idle TTL / StopAfterUse 回收
//! - [`diagnostics`]：service/model 状态投影与诊断辅助
//! - [`tests`]：单元测试
mod client;
mod diagnostics;
mod lifecycle;
mod mapping;
mod singleflight;
#[cfg(test)]
mod tests;

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Notify, watch};
use tokio::time::Instant;

use crate::infra::local_engine::state::InstanceToken;

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
        if self.armed.load(Ordering::SeqCst) {
            if let Some(coord) = self.coordinator.upgrade() {
                coord.end_repair();
            }
        }
    }
}

/// Task 2: 条件停止结果——可区分 token 不匹配、已停止和成功停止。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ConditionalStopOutcome {
    /// 成功停止了目标实例
    Stopped,
    /// token 不匹配——当前实例已经不是目标实例，不停止新实例
    TokenMismatch,
    /// 条件停止内部错误——禁止兜底无条件 stop
    Error(String),
}

// ── OcrCoordinator ─────────────────────────────────────────────────────────

pub struct OcrCoordinator {
    engine_service: Arc<crate::app::local_engine::EngineManager>,
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
}

impl OcrCoordinator {
    pub fn new(engine_service: Arc<crate::app::local_engine::EngineManager>) -> Arc<Self> {
        let paddleocr_engine_id = EngineId::new(PADDLEOCR_ENGINE_ID_STR).unwrap();
        let (lifecycle_tx, lifecycle_rx) = watch::channel(LifecycleState::Idle { generation: 0 });
        Arc::new(Self {
            engine_service,
            paddleocr_engine_id,
            in_flight: Arc::new(AtomicU32::new(0)),
            lifecycle_tx,
            lifecycle_rx,
            idle_cancel: Arc::new(Notify::new()),
            start_elapsed_ms: Arc::new(std::sync::Mutex::new(None)),
            last_diagnosis: Arc::new(std::sync::RwLock::new(None)),
            repair_mode: Arc::new(AtomicBool::new(false)),
            starting_gate: Arc::new(AtomicBool::new(false)),
        })
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

    /// 进入 repair 模式——拒绝新 lease，等待 in-flight 完成，条件停止当前实例。
    ///
    /// 调用方（`repair_paddleocr` command）在执行清理/重装前调用此方法，
    /// 确保不会有新请求引用旧实例。
    ///
    /// Task 6: 返回 `RepairGuard` RAII，确保无论 repair 路径如何结束，
    /// `end_repair()` 都会被调用。
    pub async fn begin_repair(self: &Arc<Self>) -> RepairGuard {
        tracing::info!("OcrCoordinator: 进入 repair 模式，拒绝新 lease");
        self.repair_mode.store(true, Ordering::SeqCst);

        // 取消所有 pending idle TTL 定时器
        self.idle_cancel.notify_waiters();

        // 如果当前是 Ready，进入 Stopping 并停止实例
        let current_state = self.lifecycle_state();
        let target_token = if let LifecycleState::Ready {
            generation,
            instance_token,
        } = &current_state
        {
            self.lifecycle_tx
                .send(LifecycleState::Stopping {
                    generation: *generation,
                })
                .ok();
            Some((instance_token.clone(), *generation))
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

        // 停止 PaddleOCR 服务
        if let Some((token, target_gen)) = target_token {
            // Task 2: 使用 conditional_stop——区分 Stopped/TokenMismatch/Error
            let outcome = self.conditional_stop(target_gen, &token).await;
            match &outcome {
                ConditionalStopOutcome::Stopped => {
                    tracing::info!(generation = target_gen, "repair: 条件停止成功");
                    // 重置状态机
                    *self.start_elapsed_ms.lock().unwrap() = None;
                    self.lifecycle_tx
                        .send(LifecycleState::Idle {
                            generation: target_gen + 1,
                        })
                        .ok();
                }
                ConditionalStopOutcome::TokenMismatch => {
                    tracing::warn!(
                        generation = target_gen,
                        "repair: token 不匹配，新实例已接管"
                    );
                    // 不提交 Idle——新 generation 已经在运行
                }
                ConditionalStopOutcome::Error(msg) => {
                    // Task 2: 条件停止内部错误——禁止兜底无条件 stop
                    // Task 6: 不重置为 Idle——repair 仍需要通过 stop() 确保进程退出
                    tracing::error!(
                        generation = target_gen,
                        error = %msg,
                        "repair: conditional_stop 失败，尝试无条件 stop 确保进程退出"
                    );
                    // repair 路径需要确保进程退出——使用无条件 stop
                    let _ = self.engine_service.stop(&self.paddleocr_engine_id).await;
                    *self.start_elapsed_ms.lock().unwrap() = None;
                    self.lifecycle_tx
                        .send(LifecycleState::Idle {
                            generation: target_gen + 1,
                        })
                        .ok();
                }
            }
        } else {
            // 无 token（非 Ready 状态）——直接无条件停止
            let _ = self.engine_service.stop(&self.paddleocr_engine_id).await;
            let current_gen = self.lifecycle_state().generation();
            *self.start_elapsed_ms.lock().unwrap() = None;
            self.lifecycle_tx
                .send(LifecycleState::Idle {
                    generation: current_gen + 1,
                })
                .ok();
        }

        tracing::info!("OcrCoordinator: repair 前置完成，实例已停止");

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

    /// Task 2: 条件停止包装方法——调用 service.stop_if_current 并结合
    /// lifecycle 二次核对，返回可区分的 ConditionalStopOutcome。
    ///
    /// 调用前必须已经把 lifecycle 设置为 Stopping { generation }。
    ///
    /// - `Stopped`：service 层成功停止了目标实例，且 lifecycle 仍为该 generation 的 Stopping。
    /// - `TokenMismatch`：service 层 token 不匹配（返回 Ok(()) 但未停止），
    ///   或 lifecycle 在此期间已变化（新 generation 接管）。
    /// - `Error`：service 层返回内部错误，禁止兜底无条件 stop。
    async fn conditional_stop(
        &self,
        target_gen: u64,
        target_token: &InstanceToken,
    ) -> ConditionalStopOutcome {
        match self
            .engine_service
            .stop_if_current(&self.paddleocr_engine_id, target_token)
            .await
        {
            Ok(()) => {
                // 二次核对 lifecycle——如果状态已变化（新 generation 接管），不提交 Idle
                let current = self.lifecycle_state();
                match &current {
                    LifecycleState::Stopping { generation } if *generation == target_gen => {
                        ConditionalStopOutcome::Stopped
                    }
                    _ => {
                        tracing::debug!(
                            target_gen,
                            current = ?current,
                            "conditional_stop: lifecycle 已变化，视为 TokenMismatch"
                        );
                        ConditionalStopOutcome::TokenMismatch
                    }
                }
            }
            Err(e) => {
                // Task 2: 条件停止内部错误——禁止兜底无条件 stop
                tracing::error!(
                    %e,
                    target_gen,
                    "conditional_stop: stop_if_current 返回错误，不回退到无条件 stop"
                );
                ConditionalStopOutcome::Error(e.to_string())
            }
        }
    }

    /// 等待 deadline 到来（如果有的话），用于 select! 分支。
    async fn sleep_until_deadline(&self, ctx: &OcrRequestContext) {
        if let Some(deadline) = ctx.deadline {
            tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)).await;
        } else {
            std::future::pending::<()>().await;
        }
    }

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
        let result = self
            .paddleocr_recognize(
                png_data,
                ctx,
                &lease.endpoint_url,
                &lease.token,
                &lease,
                request_png_size,
            )
            .await;
        let recognize_ms = recognize_start.elapsed().as_millis() as u64;
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

    /// 关闭 OCR Coordinator。
    pub async fn shutdown(&self) {
        tracing::info!("OcrCoordinator shutdown: 取消 idle 定时器并停止 PaddleOCR");
        // 拒绝新 lease
        let current_state = self.lifecycle_state();
        let target_token = match &current_state {
            LifecycleState::Ready {
                generation,
                instance_token,
            } => {
                self.lifecycle_tx
                    .send(LifecycleState::Stopping {
                        generation: *generation,
                    })
                    .ok();
                Some(instance_token.clone())
            }
            _ => {
                self.lifecycle_tx
                    .send(LifecycleState::Stopping {
                        generation: current_state.generation(),
                    })
                    .ok();
                None
            }
        };
        // 取消所有 pending idle TTL 定时器
        self.idle_cancel.notify_waiters();
        // 等待 in-flight 请求完成（最多等 1s）
        let mut waited = 0u64;
        while self.in_flight.load(Ordering::SeqCst) > 0 && waited < 1000 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            waited += 10;
        }
        // 停止 PaddleOCR 服务
        // Task 2: shutdown 是最终清理路径，使用明确的 stop() 停止当前任意实例
        // 不使用 stop_if_current 的兜底——shutdown 需要确保进程退出
        if let Some(token) = target_token {
            // 先尝试条件停止——如果 token 匹配则优雅停止
            match self
                .engine_service
                .stop_if_current(&self.paddleocr_engine_id, &token)
                .await
            {
                Ok(()) => tracing::info!(
                    generation = token.generation,
                    "shutdown: stop_if_current 成功"
                ),
                Err(e) => {
                    // 条件停止失败——shutdown 路径使用无条件 stop 确保进程退出
                    tracing::warn!(%e, "shutdown: stop_if_current 失败，使用无条件 stop 确保进程退出");
                    let _ = self.engine_service.stop(&self.paddleocr_engine_id).await;
                }
            }
        } else {
            // 无 token——直接无条件停止
            let _ = self.engine_service.stop(&self.paddleocr_engine_id).await;
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
                    // auto 路径——hot-only，不触发冷启动
                    let (res, start_wait, recog_ms) = {
                        self.idle_cancel.notify_waiters();
                        self.do_paddleocr_recognize(png_data.clone(), ctx, true, request_png_size)
                            .await
                    };

                    let used_paddleocr = match &res {
                        Ok(_) => true,
                        Err(e) => {
                            e.category != crate::domain::ocr::error::OcrErrorCategory::ModelNotReady
                                || !e.message.contains("not_ready_hot_only")
                        }
                    };

                    if used_paddleocr {
                        self.schedule_idle_stop(snapshot);
                        if let Err(ref paddle_err) = res {
                            // 输入本身的问题（取消/解码失败/超预算）不回退——
                            // 换后端无济于事；后端基础设施问题才回退 WinRT
                            let should_fallback = match paddle_err.category {
                                crate::domain::ocr::error::OcrErrorCategory::Cancelled => false,
                                crate::domain::ocr::error::OcrErrorCategory::DecodeError => false,
                                crate::domain::ocr::error::OcrErrorCategory::InputTooLarge => false,
                                _ => true,
                            };
                            if should_fallback {
                                tracing::info!(error = %paddle_err, "auto 模式 PaddleOCR 热态识别失败，fallback 到 WinRT");
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
                                        Ok(ocr_result) => {
                                            (decision, Ok(ocr_result), start_wait, recog_ms, fb_ms)
                                        }
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
                        // 未使用 PaddleOCR——立即走 WinRT
                        let (res2, ms) = self.do_winrt_recognize(&png_data, ctx).await;
                        let decision = RouteDecision {
                            configured_backend: OcrBackendKind::Auto,
                            selected_backend: OcrBackendKind::Windows,
                            fallback_reason: Some("PaddleOCR 未热态 Ready".to_string()),
                        };
                        (decision, res2, 0u64, ms, 0u64)
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
            paddleocr_installed: false,
            paddleocr_service_state: "Unknown".to_string(),
            paddleocr_model_state: "Unknown".to_string(),
            paddleocr_model_id: None,
            paddleocr_model_revision: None,
            paddleocr_instance_id: None,
            paddleocr_actual_backend: None,
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
        let paddleocr_installed = self.is_paddleocr_installed().await;
        let paddleocr_service_state = self.paddleocr_service_state().await;
        let paddleocr_model_state = self.paddleocr_model_state().await;
        let (
            paddleocr_model_id,
            paddleocr_model_revision,
            paddleocr_instance_id,
            paddleocr_actual_backend,
        ) = self.paddleocr_health_info().await;
        let in_flight_count = self.in_flight.load(Ordering::SeqCst) as usize;

        if let Some(mut d) = cached {
            d.paddleocr_installed = paddleocr_installed;
            d.paddleocr_service_state = paddleocr_service_state;
            d.paddleocr_model_state = paddleocr_model_state;
            d.paddleocr_model_id = paddleocr_model_id;
            d.paddleocr_model_revision = paddleocr_model_revision;
            d.paddleocr_instance_id = paddleocr_instance_id;
            d.paddleocr_actual_backend = paddleocr_actual_backend;
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
            paddleocr_model_id,
            paddleocr_model_revision,
            paddleocr_instance_id,
            paddleocr_actual_backend,
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
