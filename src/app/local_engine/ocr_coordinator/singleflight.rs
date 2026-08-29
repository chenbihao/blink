//! Single-flight 并发原语：Ready lease、in-flight 计数、LifecycleState watch、
//! shared startup task 与条件提交。只做并发合并与生命周期状态机，不做业务路由。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Duration;

use tokio::sync::watch;
use tokio::time::Instant;

use crate::domain::local_engine::status::ModelHealth;
use crate::domain::ocr::config::PaddleModel;
use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::StructuredOcrError;
use crate::infra::local_engine::runtime::EngineId;
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
/// Ready lease——绑定 generation、instance token 和 in-flight guard。
///
/// 所有字段在 acquire 时刻原子绑定，确保 lease 不会引用过时实例。
/// Task 3: InFlightGuard 绑定到 Lease，drop 时自动释放 in-flight。
pub(super) struct Lease {
    pub(super) endpoint_url: String,
    pub(super) token: String,
    /// Task 3: InFlightGuard 绑定到 Lease，Lease drop 自动释放 in-flight。
    pub(super) _guard: Option<InFlightGuard>,
}

impl Lease {
    /// 返回当前实例的模型契约（model_id + model_revision）。
    ///
    /// 传入 response mapper 做严格校验，确保响应来自正确的实例。
    pub(super) fn model_contract(&self) -> (String, &'static str) {
        let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
        (
            format!("PP-OCRv6:{}:{}", det_model, rec_model),
            "ppocrv6-tiny",
        )
    }
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

impl OcrCoordinator {
    /// 共享启动 singleflight（watch，不丢通知）。
    ///
    /// **核心重构**：winner 只负责触发独立后台启动 task。
    /// task 持有自己的 120s 上限，不持有任何业务请求 context。
    /// 单个请求取消只能取消自己的等待，不能取消共享启动。
    async fn ensure_paddleocr_started(
        &self,
        ctx: &OcrRequestContext,
    ) -> Result<u64, StructuredOcrError> {
        let mut rx = self.lifecycle_rx.clone();

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
                        tracing::debug!(generation, "Ready 但 service 不 Ready，过时");
                        let _ = self.lifecycle_tx.send(LifecycleState::Idle {
                            generation: generation + 1,
                        });
                        continue;
                    }
                }
                LifecycleState::Starting { .. } => {
                    tracing::debug!("Starting，等待独立后台 task 完成");
                    // waiter 只等待状态变化，不执行启动
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
                    // Task 4: waiter 观察到 Failed 后返回相同错误，
                    // 不在当前调用内部自动重启。
                    // 只有失败之后到达的独立新请求才可开启下一 generation。
                    tracing::debug!(generation, "Failed，返回错误给 waiter");
                    return Err(error.as_ref().clone());
                }
                LifecycleState::Idle { generation } => {
                    // 原子 gate CAS：确保只有一个 winner（Handoff B.I.1）
                    // compare_exchange(false, true) 是原子的——
                    // 第一个成功者为 winner，其余为 loser
                    let is_winner = self
                        .starting_gate
                        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
                        .is_ok();
                    if !is_winner {
                        // 已有 winner——等待状态变化
                        tracing::debug!(generation, "Idle → Starting: 已有 winner，等待");
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
                            _ = self.sleep_until_deadline(ctx) => return Err(StructuredOcrError::timeout()),
                        }
                        continue;
                    }
                    // 确认是 winner——发送 Starting 状态
                    let next_gen = *generation;
                    self.lifecycle_tx
                        .send(LifecycleState::Starting {
                            generation: next_gen,
                        })
                        .ok();
                    // spawn 独立后台启动 task（不持有任何业务请求 ctx）
                    self.spawn_shared_startup_task(next_gen);
                    // winner 也等待后台 task 完成
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

    /// spawn 独立后台启动 task——不持有任何业务请求 context。
    ///
    /// task 持有自己的 120s 上限，即使所有请求都取消了，启动也会继续完成。
    /// 成功后设置 Ready 状态，失败后设置 Failed 状态。
    /// Task 1: 条件提交 Ready——只在当前状态仍为 `Starting { generation }` 时提交。
    ///
    /// 如果当前状态已经变化（新 generation、Stopping、Idle、Ready、repair 等），
    /// 丢弃旧任务结果，不覆盖当前 lifecycle。
    ///
    /// 如果旧任务已经启动了一个实例但无法提交 Ready，调用方应使用该任务
    /// 取得的 InstanceToken 条件停止该实例（不无条件停止当前实例）。
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
                // 仍然是当前 generation 的 Starting——安全提交 Ready
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
                    "shared startup: 成功，PaddleOCR Ready"
                );
                true
            }
            _ => {
                // 当前状态已变化——丢弃旧任务结果
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

    fn spawn_shared_startup_task(&self, generation: u64) {
        let engine_service = self.engine_service.clone();
        let engine_id = self.paddleocr_engine_id.clone();
        let lifecycle_tx = self.lifecycle_tx.clone();
        let start_elapsed_ms = self.start_elapsed_ms.clone();
        let starting_gate = self.starting_gate.clone();

        tokio::spawn(async move {
            let start_time = Instant::now();
            // Task 1: 单一总 deadline——整个启动流程的总预算
            let total_deadline = tokio::time::Instant::now() + SHARED_STARTUP_TIMEOUT;

            tracing::info!(generation, "shared startup task 开始");

            // task 结束时确保重置 starting_gate（无论成功或失败）
            let _gate_guard = StartingGateGuard::new(starting_gate.clone());

            // helper: 提交 Failed
            let commit_failed = |error: StructuredOcrError| {
                Self::commit_start_failed_if_current(
                    &lifecycle_tx,
                    generation,
                    Arc::new(error),
                    &start_elapsed_ms,
                );
            };

            // 1. 检查环境是否安装
            let installed = {
                let status = match engine_service.get_status(&engine_id).await {
                    Ok(s) => s,
                    Err(e) => {
                        tracing::warn!(generation, %e, "shared startup: 获取状态失败");
                        commit_failed(StructuredOcrError::start_failed(format!(
                            "获取状态失败: {e}"
                        )));
                        return;
                    }
                };
                status.status.environment
                    == crate::domain::local_engine::status::EnvironmentHealth::Ready
            };

            if !installed {
                commit_failed(StructuredOcrError::start_failed("环境未安装"));
                tracing::warn!(generation, "shared startup: 环境未安装");
                return;
            }

            // 2. 构建 adapter config
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            let engine_config =
                crate::app::local_engine::paddleocr::PaddleOcrEngineConfig::from_ocr_config();
            let adapter_config = crate::domain::local_engine::AdapterConfig {
                preferred_port: Some(9100),
                compute_preference: Some(ocr_config.compute_preference),
                engine_config: engine_config.to_json(),
            };

            // 3. 执行启动——使用总 deadline 的剩余时间
            let remaining = total_deadline
                .checked_duration_since(tokio::time::Instant::now())
                .unwrap_or(Duration::ZERO);
            if remaining.is_zero() {
                commit_failed(StructuredOcrError::start_failed(format!(
                    "PaddleOCR 启动超时（{}s）",
                    SHARED_STARTUP_TIMEOUT.as_secs()
                )));
                tracing::warn!(generation, "shared startup: 超时");
                return;
            }

            let start_result =
                tokio::time::timeout(remaining, engine_service.start(&engine_id, adapter_config))
                    .await;

            let elapsed_ms = start_time.elapsed().as_millis() as u64;

            match start_result {
                Err(_) => {
                    commit_failed(StructuredOcrError::start_failed(format!(
                        "PaddleOCR 启动超时（{}s）",
                        SHARED_STARTUP_TIMEOUT.as_secs()
                    )));
                    tracing::warn!(generation, "shared startup: 超时");
                }
                Ok(Err(e)) => {
                    commit_failed(StructuredOcrError::start_failed(format!("{e}")));
                    tracing::warn!(generation, %e, "shared startup: 启动失败");
                }
                Ok(Ok(())) => {
                    // 4. 等待 model ready——使用总 deadline 的剩余时间
                    let remaining = total_deadline
                        .checked_duration_since(tokio::time::Instant::now())
                        .unwrap_or(Duration::ZERO);
                    if remaining.is_zero() {
                        commit_failed(StructuredOcrError::start_failed(format!(
                            "PaddleOCR 模型加载超时（{}s）",
                            SHARED_STARTUP_TIMEOUT.as_secs()
                        )));
                        tracing::warn!(generation, "shared startup: 模型加载超时");
                        return;
                    }

                    let model_ready =
                        wait_for_model_ready_static(&engine_service, &engine_id, remaining).await;

                    match model_ready {
                        Ok(wait_ms) => {
                            let total_ms = elapsed_ms + wait_ms;
                            // Task 1: Ready 只能携带从 service 获取到的真实 InstanceToken。
                            // 获取不到 token 时启动必须失败。
                            let instance_token =
                                match engine_service.get_current_instance_token(&engine_id).await {
                                    Ok(Some(token)) => token,
                                    Ok(None) => {
                                        commit_failed(StructuredOcrError::start_failed(
                                            "启动成功但无法获取 instance token，拒绝发布 Ready",
                                        ));
                                        tracing::warn!(
                                            generation,
                                            "shared startup: instance token 为 None"
                                        );
                                        return;
                                    }
                                    Err(e) => {
                                        commit_failed(StructuredOcrError::start_failed(format!(
                                            "获取 instance token 失败: {e}"
                                        )));
                                        tracing::warn!(
                                            generation,
                                            %e,
                                            "shared startup: 获取 instance token 失败"
                                        );
                                        return;
                                    }
                                };

                            // Task 1: 条件提交 Ready
                            Self::commit_start_ready_if_current(
                                &lifecycle_tx,
                                generation,
                                instance_token,
                                &start_elapsed_ms,
                                total_ms,
                            );
                        }
                        Err(e) => {
                            commit_failed(StructuredOcrError::start_failed(e));
                            tracing::warn!(generation, "shared startup: 模型加载失败");
                        }
                    }
                }
            }
        });
    }

    /// 原子获取 Ready lease。
    ///
    /// 在获取 endpoint 和验证 generation 之间不存在 TOCTOU 窗口——
    /// 所有字段在同一个函数调用中原子绑定。
    ///
    /// Task 3: InFlightGuard 只能在确认当前 Ready generation/token 后创建，
    /// 创建 guard 后必须二次核对 lifecycle，不返回陈旧 lease。
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
                        match self.get_paddleocr_endpoint(ctx).await {
                            Some((endpoint_url, token)) => {
                                // Task 3: 二次核对——endpoint 获取后 repair 可能已触发
                                if self.is_in_repair_mode() {
                                    return Err(LeaseError::NotReady);
                                }
                                // Task 3: 二次核对——lifecycle 状态可能已变化
                                let state2 = self.lifecycle_state();
                                let (gen2, token2) = match state2 {
                                    LifecycleState::Ready {
                                        generation,
                                        instance_token,
                                    } => (generation, instance_token),
                                    _ => return Err(LeaseError::NotReady),
                                };
                                // 如果 generation/token 已变化，不返回旧 lease
                                if gen2 != generation || token2 != instance_token {
                                    return Err(LeaseError::NotReady);
                                }
                                // Task 3: 在确认 Ready 后创建 InFlightGuard
                                let guard = InFlightGuard::new(self.in_flight.clone());
                                Ok(Lease {
                                    endpoint_url,
                                    token,
                                    _guard: Some(guard),
                                })
                            }
                            None => Err(LeaseError::NotReady),
                        }
                    } else {
                        Err(LeaseError::NotReady)
                    }
                }
                _ => Err(LeaseError::NotReady),
            }
        } else {
            // 返回值（启动耗时）仅用于诊断语义；Lease 不再携带未读字段
            let _start_elapsed = self.ensure_paddleocr_started(ctx).await?;
            // repair 模式可能在启动期间被触发
            if self.is_in_repair_mode() {
                return Err(LeaseError::NotReady);
            }
            // 取消覆盖：endpoint 获取前再次检查
            if ctx.should_stop() {
                return Err(if ctx.is_cancelled() {
                    LeaseError::Cancelled
                } else {
                    LeaseError::Timeout
                });
            }
            // 原子获取 endpoint + generation + instance_token
            let (endpoint_url, token) =
                self.get_paddleocr_endpoint(ctx).await.ok_or_else(|| {
                    LeaseError::Error(StructuredOcrError::protocol_error(
                        "无法获取 PaddleOCR endpoint",
                    ))
                })?;
            // endpoint 获取后再次检查取消
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
            // Task 3: 二次核对——repair 可能在此期间触发
            if self.is_in_repair_mode() {
                return Err(LeaseError::NotReady);
            }
            // Task 3: 在确认 Ready 后创建 InFlightGuard
            let guard = InFlightGuard::new(self.in_flight.clone());
            // Task 3: 二次核对 lifecycle——guard 创建后验证状态未变
            let state2 = self.lifecycle_state();
            let (gen2, token2) = match state2 {
                LifecycleState::Ready {
                    generation,
                    instance_token,
                } => (generation, instance_token),
                _ => {
                    // 状态已变化，释放 guard 并返回错误
                    drop(guard);
                    return Err(LeaseError::NotReady);
                }
            };
            if gen2 != generation || token2 != instance_token {
                drop(guard);
                return Err(LeaseError::NotReady);
            }
            Ok(Lease {
                endpoint_url,
                token,
                _guard: Some(guard),
            })
        }
    }
}

// ── 独立 model ready 等待（不持有请求 ctx）──────────────────────────────────

/// 等待 PaddleOCR model Ready——独立函数，不持有请求 context。
///
/// 由 shared startup task 调用。即使所有请求都取消了，模型等待也会继续。
async fn wait_for_model_ready_static(
    engine_service: &Arc<crate::app::local_engine::EngineManager>,
    engine_id: &EngineId,
    timeout: Duration,
) -> Result<u64, String> {
    let poll_interval = Duration::from_secs(1);
    let start = Instant::now();

    loop {
        if start.elapsed() >= timeout {
            return Err(format!("模型加载超时（{}s）", timeout.as_secs()));
        }

        let status = engine_service
            .get_status(engine_id)
            .await
            .map_err(|e| format!("获取状态失败: {e}"))?;

        match status.status.model {
            ModelHealth::Ready => {
                return Ok(start.elapsed().as_millis() as u64);
            }
            ModelHealth::Failed => return Err("模型加载失败".to_string()),
            _ => {
                tokio::time::sleep(poll_interval).await;
            }
        }
    }
}
