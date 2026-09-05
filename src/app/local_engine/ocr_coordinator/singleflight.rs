//! Single-flight 并发原语：Ready lease、in-flight 计数、LifecycleState watch、
//! shared startup task 与条件提交。只做并发合并与生命周期状态机，不做业务路由。
//!
//! 0.22.8-D: 移除 endpoint/token 获取，Lease 只保留 InFlightGuard。
//! shared startup task 调用 `executor.ensure_ready()` 替代 `engine_service.start()`。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;

use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::StructuredOcrError;
use crate::infra::local_engine::state::InstanceToken;

use super::OcrCoordinator;

const SHARED_STARTUP_TIMEOUT: Duration = Duration::from_secs(120);

// ── RAII in-flight lease ──────────────────────────────────────────────────

pub(super) struct InFlightGuard {
    counter: Arc<AtomicU32>,
}

impl InFlightGuard {
    pub(super) fn new(counter: Arc<AtomicU32>) -> Self {
        counter.fetch_add(1, Ordering::SeqCst);
        Self { counter }
    }
}

impl Drop for InFlightGuard {
    fn drop(&mut self) {
        self.counter.fetch_sub(1, Ordering::SeqCst);
    }
}

/// RAII guard for starting_gate——在 shared startup task 结束时自动重置 gate。
///
/// 确保无论 task 成功或失败，starting_gate 都会被重置为 false，
/// 允许下一轮启动请求竞争。
pub(super) struct StartingGateGuard {
    gate: Arc<AtomicBool>,
}

impl StartingGateGuard {
    pub(super) fn new(gate: Arc<AtomicBool>) -> Self {
        Self { gate }
    }
}

impl Drop for StartingGateGuard {
    fn drop(&mut self) {
        self.gate.store(false, Ordering::SeqCst);
    }
}

// ── Lease ─────────────────────────────────────────────────────────────────
/// Ready lease——0.22.8-D: 只绑定 InFlightGuard。
///
/// 0.22.8-D: 不再携带 endpoint_url/token（ONNX executor 是 in-process 的，
/// 无需 HTTP endpoint）。InFlightGuard 仍需，用于 idle TTL 和并发计数。
pub(super) struct Lease {
    /// Task 3: InFlightGuard 绑定到 Lease，Lease drop 自动释放 in-flight。
    pub(super) _guard: Option<InFlightGuard>,
}

pub(super) enum LeaseError {
    NotReady,
    Cancelled,
    Timeout,
    Error(StructuredOcrError),
}
impl From<StructuredOcrError> for LeaseError {
    fn from(e: StructuredOcrError) -> Self {
        LeaseError::Error(e)
    }
}

// ── LifecycleState ────────────────────────────────────────────────────────

/// 生命周期状态（通过 watch channel 广播，不丢通知）。
#[derive(Debug, Clone, PartialEq)]
pub(super) enum LifecycleState {
    Idle {
        generation: u64,
    },
    Starting {
        generation: u64,
    },
    Ready {
        generation: u64,
        instance_token: InstanceToken,
    },
    Stopping {
        generation: u64,
    },
    Failed {
        generation: u64,
        error: Arc<StructuredOcrError>,
    },
}

impl LifecycleState {
    pub(super) fn generation(&self) -> u64 {
        match self {
            LifecycleState::Idle { generation }
            | LifecycleState::Starting { generation }
            | LifecycleState::Ready { generation, .. }
            | LifecycleState::Stopping { generation }
            | LifecycleState::Failed { generation, .. } => *generation,
        }
    }
}

/// 仅允许失败之后到达的新请求把指定 Failed generation 推进到下一轮 Idle。
/// `send_if_modified` 保证并发请求不会重复递增 generation。
pub(super) fn reset_failed_for_new_request(
    tx: &watch::Sender<LifecycleState>,
    failed_generation: u64,
) -> bool {
    tx.send_if_modified(|state| {
        if matches!(
            state,
            LifecycleState::Failed { generation, .. } if *generation == failed_generation
        ) {
            *state = LifecycleState::Idle {
                generation: failed_generation + 1,
            };
            true
        } else {
            false
        }
    })
}

impl OcrCoordinator {
    /// 0.22.8-D: 共享启动 singleflight——调用 executor.ensure_ready()。
    ///
    /// **核心**：winner 只负责触发独立后台启动 task。
    /// task 持有自己的 120s 上限，不持有任何业务请求 context。
    /// 单个请求取消只能取消自己的等待，不能取消共享启动。
    async fn ensure_paddleocr_started(
        &self,
        ctx: &OcrRequestContext,
    ) -> Result<u64, StructuredOcrError> {
        let mut rx = self.lifecycle_rx.clone();
        let mut participating_generation = None;

        loop {
            // repair 模式拒绝新 lease
            if self.is_in_repair_mode() {
                return Err(StructuredOcrError::start_failed(
                    "OCR 引擎正在修复中，请稍后重试",
                ));
            }

            if ctx.should_stop() {
                return Err(if ctx.is_cancelled() {
                    StructuredOcrError::cancelled()
                } else {
                    StructuredOcrError::timeout()
                });
            }

            let current = rx.borrow().clone();

            match &current {
                LifecycleState::Ready { generation, .. } => {
                    if self.is_paddleocr_ready().await {
                        let elapsed = self.start_elapsed_ms.lock().unwrap().unwrap_or(0);
                        return Ok(elapsed);
                    } else {
                        tracing::debug!(generation, "Ready 但 executor 不 Ready，过时");
                        let _ = self.lifecycle_tx.send(LifecycleState::Idle {
                            generation: generation + 1,
                        });
                        continue;
                    }
                }
                LifecycleState::Starting { generation } => {
                    participating_generation = Some(*generation);
                    tracing::debug!("Starting，等待独立后台 task 完成");
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
                        _ = self.sleep_until_deadline(ctx) => return Err(StructuredOcrError::timeout()),
                    }
                    continue;
                }
                LifecycleState::Stopping { .. } => {
                    tracing::debug!("Stopping，等待停止完成");
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
                        _ = self.sleep_until_deadline(ctx) => return Err(StructuredOcrError::timeout()),
                    }
                    continue;
                }
                LifecycleState::Failed { generation, error } => {
                    if participating_generation == Some(*generation) {
                        tracing::debug!(generation, "Failed，返回错误给当前 generation waiter");
                        return Err(error.as_ref().clone());
                    }

                    let failed_generation = *generation;
                    let reset = reset_failed_for_new_request(&self.lifecycle_tx, failed_generation);
                    if reset {
                        tracing::info!(
                            failed_generation,
                            next_generation = failed_generation + 1,
                            "新 OCR 请求重置上一轮启动失败状态"
                        );
                    }
                    continue;
                }
                LifecycleState::Idle { generation } => {
                    participating_generation = Some(*generation);
                    let is_winner = self
                        .starting_gate
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok();
                    if !is_winner {
                        tracing::debug!(generation, "Idle → Starting: 已有 winner，等待");
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
                            _ = self.sleep_until_deadline(ctx) => return Err(StructuredOcrError::timeout()),
                        }
                        continue;
                    }
                    let next_gen = *generation;
                    self.lifecycle_tx
                        .send(LifecycleState::Starting {
                            generation: next_gen,
                        })
                        .ok();
                    // 0.22.8-D: spawn 独立后台启动 task（调用 executor.ensure_ready）
                    self.spawn_shared_startup_task(next_gen);
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
                        _ = self.sleep_until_deadline(ctx) => return Err(StructuredOcrError::timeout()),
                    }
                    continue;
                }
            }
        }
    }

    /// Task 1: 条件提交 Ready——只在当前状态仍为 `Starting { generation }` 时提交。
    fn commit_start_ready_if_current(
        lifecycle_tx: &watch::Sender<LifecycleState>,
        generation: u64,
        instance_token: InstanceToken,
        start_elapsed_ms: &Arc<std::sync::Mutex<Option<u64>>>,
        total_ms: u64,
    ) -> bool {
        let current = lifecycle_tx.borrow().clone();
        match &current {
            LifecycleState::Starting {
                generation: cur_gen,
            } if *cur_gen == generation => {
                lifecycle_tx
                    .send(LifecycleState::Ready {
                        generation,
                        instance_token,
                    })
                    .ok();
                *start_elapsed_ms.lock().unwrap() = Some(total_ms);
                tracing::info!(
                    generation,
                    total_ms,
                    "shared startup: 成功，ONNX executor Ready"
                );
                true
            }
            _ => {
                tracing::warn!(
                    generation,
                    current = ?current,
                    "shared startup: 状态已变化，丢弃 Ready 提交"
                );
                false
            }
        }
    }

    /// Task 1: 条件提交 Failed——只在当前状态仍为 `Starting { generation }` 时提交。
    fn commit_start_failed_if_current(
        lifecycle_tx: &watch::Sender<LifecycleState>,
        generation: u64,
        error: Arc<StructuredOcrError>,
        start_elapsed_ms: &Arc<std::sync::Mutex<Option<u64>>>,
    ) -> bool {
        let current = lifecycle_tx.borrow().clone();
        match &current {
            LifecycleState::Starting {
                generation: cur_gen,
            } if *cur_gen == generation => {
                lifecycle_tx
                    .send(LifecycleState::Failed { generation, error })
                    .ok();
                *start_elapsed_ms.lock().unwrap() = None;
                tracing::warn!(generation, "shared startup: 失败已提交");
                true
            }
            _ => {
                tracing::warn!(
                    generation,
                    current = ?current,
                    "shared startup: 状态已变化，丢弃 Failed 提交"
                );
                false
            }
        }
    }

    /// 0.22.8-D: spawn 独立后台启动 task——调用 executor.ensure_ready()。
    ///
    /// task 持有自己的 120s 上限，即使所有请求都取消了，启动也会继续完成。
    /// 成功后设置 Ready 状态，失败后设置 Failed 状态。
    fn spawn_shared_startup_task(&self, generation: u64) {
        // 0.22.9：'static spawn 不能捕获 &self——提前取 owned 的 manager 升级与引擎 id
        let engine_service = self.engine_service.get().and_then(|w| w.upgrade());
        let engine_id = self.paddleocr_engine_id.clone();
        let executor = self.executor.read().unwrap().clone();
        let executor = match executor {
            Some(e) => e,
            // executor 未注入（用户未手动点「启动」）——0.22.9 起懒构建：
            // 从 active deployment 直接构建 executor（与启动命令同源），
            // 使首次 OCR 请求即可用，无需手动预热。
            // 部署缺失时提交 Failed 并给可行动提示。
            None => match super::build_onnx_executor_from_deployment() {
                Some(built) => {
                    tracing::info!("OCR 懒启动：executor 未注入，已从 active deployment 构建");
                    *self.executor.write().unwrap() = Some(Arc::clone(&built));
                    built
                }
                None => {
                    // 必须重置 starting_gate，否则后续请求永远拿不到 winner
                    self.starting_gate.store(false, Ordering::SeqCst);
                    Self::commit_start_failed_if_current(
                        &self.lifecycle_tx,
                        generation,
                        Arc::new(StructuredOcrError::backend_unavailable(
                            "OCR ONNX 环境未安装，请在设置页「引擎」中安装 PaddleOCR 环境",
                        )),
                        &self.start_elapsed_ms,
                    );
                    return;
                }
            },
        };
        let lifecycle_tx = self.lifecycle_tx.clone();
        let start_elapsed_ms = self.start_elapsed_ms.clone();
        let starting_gate = self.starting_gate.clone();

        tokio::spawn(async move {
            let start_time = Instant::now();
            let total_deadline = tokio::time::Instant::now() + SHARED_STARTUP_TIMEOUT;

            tracing::info!(generation, "shared startup task 开始 (ONNX executor)");

            let _gate_guard = StartingGateGuard::new(starting_gate.clone());

            let commit_failed = |error: StructuredOcrError| {
                Self::commit_start_failed_if_current(
                    &lifecycle_tx,
                    generation,
                    Arc::new(error),
                    &start_elapsed_ms,
                );
            };

            // 0.22.8-D: 通过 executor.ensure_ready() 启动
            // executor 内部有 lazy load + singleflight，这里调用会触发 Session 构建
            let cancel_token = CancellationToken::new();
            let remaining = total_deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);

            if remaining.is_zero() {
                commit_failed(StructuredOcrError::start_failed(format!(
                    "ONNX executor 启动超时（{}s）",
                    SHARED_STARTUP_TIMEOUT.as_secs()
                )));
                return;
            }

            // 调用 executor.ensure_ready()——内部有 watch + starting gate
            let ensure_result =
                tokio::time::timeout(remaining, executor.ensure_ready(&cancel_token)).await;

            let elapsed_ms = start_time.elapsed().as_millis() as u64;

            match ensure_result {
                Err(_) => {
                    commit_failed(StructuredOcrError::start_failed(format!(
                        "ONNX executor 启动超时（{}s）",
                        SHARED_STARTUP_TIMEOUT.as_secs()
                    )));
                    tracing::warn!(generation, "shared startup: 超时");
                }
                Ok(Err(e)) => {
                    commit_failed(StructuredOcrError::start_failed(format!(
                        "ONNX executor 启动失败: {e}"
                    )));
                    tracing::warn!(generation, %e, "shared startup: executor 启动失败");
                }
                Ok(Ok(())) => {
                    // 0.22.8-D: executor 不使用 InstanceToken——构造一个合成的 token
                    let instance_token = InstanceToken {
                        generation,
                        instance_id: format!("onnx-ocr-{generation}"),
                    };
                    Self::commit_start_ready_if_current(
                        &lifecycle_tx,
                        generation,
                        instance_token,
                        &start_elapsed_ms,
                        elapsed_ms,
                    );
                    // 0.22.9：懒启动成功 → 引擎卡片同步为 Running（best effort）
                    if let Some(service) = engine_service {
                        let eid = engine_id.clone();
                        tokio::spawn(async move {
                            if let Err(e) = service.start_inprocess(&eid).await {
                                tracing::debug!(engine = %eid, %e, "OCR 卡片状态同步失败（best effort）");
                            }
                        });
                    }
                }
            }
        });
    }

    /// 0.22.8-D: 原子获取 Ready lease。
    ///
    /// 不再获取 endpoint/token——ONNX executor 是 in-process 的。
    /// 只验证 lifecycle 状态并创建 InFlightGuard。
    pub(super) async fn acquire_lease(
        &self,
        ctx: &OcrRequestContext,
        hot_only: bool,
    ) -> Result<Lease, LeaseError> {
        // repair 模式拒绝新 lease
        if self.is_in_repair_mode() {
            return Err(LeaseError::NotReady);
        }

        if hot_only {
            if ctx.should_stop() {
                return Err(LeaseError::Cancelled);
            }
            // hot-only 也必须二次核对状态
            let state = self.lifecycle_state();
            match state {
                LifecycleState::Ready {
                    generation,
                    instance_token,
                } => {
                    if self.is_paddleocr_ready().await {
                        // 二次核对——lifecycle 状态可能已变化
                        let state2 = self.lifecycle_state();
                        let (gen2, token2) = match state2 {
                            LifecycleState::Ready {
                                generation,
                                instance_token,
                            } => (generation, instance_token),
                            _ => return Err(LeaseError::NotReady),
                        };
                        if gen2 != generation || token2 != instance_token {
                            return Err(LeaseError::NotReady);
                        }
                        // 在确认 Ready 后创建 InFlightGuard
                        let guard = InFlightGuard::new(self.in_flight.clone());
                        Ok(Lease {
                            _guard: Some(guard),
                        })
                    } else {
                        Err(LeaseError::NotReady)
                    }
                }
                _ => Err(LeaseError::NotReady),
            }
        } else {
            // 返回值（启动耗时）仅用于诊断语义
            let _start_elapsed = self.ensure_paddleocr_started(ctx).await?;
            // repair 模式可能在启动期间被触发
            if self.is_in_repair_mode() {
                return Err(LeaseError::NotReady);
            }
            if ctx.should_stop() {
                return Err(if ctx.is_cancelled() {
                    LeaseError::Cancelled
                } else {
                    LeaseError::Timeout
                });
            }
            let state = self.lifecycle_state();
            let (generation, instance_token) = match state {
                LifecycleState::Ready {
                    generation,
                    instance_token,
                } => (generation, instance_token),
                _ => return Err(LeaseError::NotReady),
            };
            // 二次核对——repair 可能在此期间触发
            if self.is_in_repair_mode() {
                return Err(LeaseError::NotReady);
            }
            let guard = InFlightGuard::new(self.in_flight.clone());
            // 二次核对 lifecycle
            let state2 = self.lifecycle_state();
            let (gen2, token2) = match state2 {
                LifecycleState::Ready {
                    generation,
                    instance_token,
                } => (generation, instance_token),
                _ => {
                    drop(guard);
                    return Err(LeaseError::NotReady);
                }
            };
            if gen2 != generation || token2 != instance_token {
                drop(guard);
                return Err(LeaseError::NotReady);
            }
            Ok(Lease {
                _guard: Some(guard),
            })
        }
    }
}
