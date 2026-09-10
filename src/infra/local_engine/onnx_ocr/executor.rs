//! OnnxOcrExecutor 核心实现（0.22.8-C）。
//!
//! ## 架构概览
//!
//! ```text
//! tokio 异步世界                     专用阻塞线程
//! ┌──────────────────┐              ┌─────────────────────────┐
//! │ OnnxOcrExecutor   │              │ Worker Thread            │
//! │ ├─ state_channel  │  send req    │ ├─ OcrPipeline (OAROCR)  │
//! │ ├─ req_sender ─────────────────→│ ├─ recv loop             │
//! │ ├─ permit sem(4)  │              │ │  └─ pipeline.recognize │
//! │ └─ idle timer      │  ←─ result ─┤ └─ on drop → thread exit │
//! └──────────────────┘   oneshot     └─────────────────────────┘
//! ```
//!
//! ## 并发模型
//!
//! - **有界队列**：`Semaphore` with 4 permits。第 5 个请求立即返回
//!   `BackendUnavailable`（背压），不无限堆积。
//! - **专用阻塞线程**：`std::thread::spawn`，不在 tokio 线程池上执行。
//! - **oneshot 回传**：每个请求通过 `tokio::sync::oneshot` 回传结果。
//! - **诚实取消**：请求携带 `CancellationToken`，取消后：
//!   - 等待中的请求：立即返回 `Cancelled`（select! 抢占 recv）
//!   - 已在工作线程上执行的请求：完成后结果被丢弃（ORT 不支持中断推理）
//!
//! ## 生命周期
//!
//! - **Lazy load**：首次请求触发 `Idle → Starting → Ready`。
//! - **TTL drop**：idle 超时后 `Ready → Stopping → Idle`，Session drop。
//! - **Shutdown**：drop executor → close sender → thread 自然退出。
//!
//! ## Worker 资源上限（0.22.18 修订）
//!
//! native ORT pipeline 构建不可中断。超时后旧 worker 进入退休状态：
//! sender 被 drop，构建完成后 recv 返回 Err 自然退出。
//! **退休 worker 仍占用线程和内存**直到 native 构建自然完成。
//!
//! 不变量：
//! - `in_flight_count`（active + retired-未退出）受 `MAX_INFLIGHT_WORKERS` 限制
//! - 超过上限时新构建请求返回 `BuildInProgress` 结构化错误，不创建新线程
//! - build timeout 立即 drop sender，撤销该 generation 的提交权
//! - shutdown 使用有界 join（辅助线程 + timeout），不无限等待 native build
//! - 退休 worker 退出后可通过 `reclaim_finished` 回收并恢复额度

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use bytes::Bytes;
use tokio::sync::{Semaphore, SemaphorePermit, oneshot};
use tokio_util::sync::CancellationToken;

use crate::domain::capability::builtins::ocr_engine::OcrResult;
use crate::domain::ocr::error::StructuredOcrError;

use super::pipeline::{OcrPipeline, OrtocrPipeline, PipelineConfig, PipelineError};
use super::state::{ExecutorState, StateChannel};

/// 有界队列容量——最多 4 个 pending 请求。
const MAX_PENDING: usize = 4;

/// Session 构建超时（秒）。
/// ORT DLL 加载 + det/rec 模型加载通常 < 5s，给 30s 余量。
const SESSION_BUILD_TIMEOUT_SECS: u64 = 30;

/// 工作线程名（用于诊断）。
const WORKER_THREAD_NAME: &str = "blink-onnx-ocr-worker";

/// 同时存活（active + retired-未退出）的 worker 数量上限。
///
/// 限制的是真实存活的 native worker 数量，不是 JoinHandle 容器长度。
/// 达到此上限时，新的构建请求返回 `BuildInProgress` 错误，
/// 不创建新线程，防止反复重试无限放大资源。
const MAX_INFLIGHT_WORKERS: usize = 2;

/// shutdown 时 join 等待超时（秒）。
/// native build 卡住时不应无限等待，超过此时间后线程 detached。
const SHUTDOWN_JOIN_TIMEOUT_SECS: u64 = 10;

/// 识别请求——从异步世界传到工作线程的载荷。
struct WorkerRequest {
    /// PNG 图片 bytes。
    png_data: Vec<u8>,
    /// 结果回传通道。
    result_tx: oneshot::Sender<Result<OcrResult, PipelineError>>,
}

/// Executor 配置。
#[derive(Debug, Clone)]
pub struct OcrExecutorConfig {
    /// Pipeline 配置（模型路径、DLL 路径、线程数）。
    pub pipeline: PipelineConfig,
}

impl Default for OcrExecutorConfig {
    fn default() -> Self {
        Self {
            pipeline: PipelineConfig {
                det_model: PathBuf::new(),
                rec_model: PathBuf::new(),
                dict_path: PathBuf::new(),
                dll_path: PathBuf::new(),
                intra_op: 1,
                inter_op: 1,
            },
        }
    }
}

/// Executor 错误。
#[derive(Debug, thiserror::Error)]
pub enum OcrExecutorError {
    #[error("Executor 未就绪: {0}")]
    NotReady(String),
    #[error("Executor 已关闭")]
    Shutdown,
    #[error("背压：队列已满（{0} pending）")]
    Backpressure(usize),
    #[error("Pipeline 错误: {0}")]
    Pipeline(String),
    #[error("Session 构建失败: {0}")]
    BuildFailed(String),
    #[error("Session 正在构建中，请稍后重试")]
    BuildInProgress,
    #[error("请求被取消")]
    Cancelled,
    #[error("请求超时")]
    Timeout,
}

impl From<OcrExecutorError> for StructuredOcrError {
    fn from(e: OcrExecutorError) -> Self {
        match &e {
            OcrExecutorError::NotReady(msg) => StructuredOcrError::model_not_ready(msg),
            OcrExecutorError::Shutdown => {
                StructuredOcrError::backend_unavailable("ONNX OCR executor 已关闭")
            }
            OcrExecutorError::Backpressure(n) => StructuredOcrError::backend_unavailable(format!(
                "OCR 队列已满（{n} pending），请稍后重试"
            )),
            OcrExecutorError::Pipeline(msg) => StructuredOcrError::protocol_error(msg),
            OcrExecutorError::BuildFailed(msg) => {
                StructuredOcrError::start_failed(format!("Session 构建失败: {msg}"))
            }
            OcrExecutorError::BuildInProgress => {
                StructuredOcrError::backend_unavailable("OCR Session 正在构建中，请稍后重试")
            }
            OcrExecutorError::Cancelled => StructuredOcrError::cancelled(),
            OcrExecutorError::Timeout => StructuredOcrError::timeout(),
        }
    }
}

/// 识别请求参数。
#[derive(Debug, Clone)]
pub struct RecognizeRequest {
    /// PNG 图片 bytes。
    pub png_data: Bytes,
    /// 请求取消 token。
    pub cancellation: CancellationToken,
    /// 请求 deadline（单调时钟）。
    pub deadline: Option<tokio::time::Instant>,
}

/// OCR Executor trait——可替换为 fake 实现（测试用）。
#[async_trait::async_trait]
pub trait OcrExecutor: Send + Sync {
    /// 执行 OCR 识别。
    async fn recognize(&self, request: RecognizeRequest) -> Result<OcrResult, OcrExecutorError>;

    /// 关闭 executor，释放资源。
    async fn shutdown(&self);
}

/// Pipeline 构建函数类型——可注入用于测试。
pub(super) type BuildFn =
    Arc<dyn Fn(&PipelineConfig) -> Result<Box<dyn OcrPipeline>, PipelineError> + Send + Sync>;

/// 默认 pipeline 构建函数——调用 `OrtocrPipeline::build`。
fn default_build_fn() -> BuildFn {
    Arc::new(|config: &PipelineConfig| {
        OrtocrPipeline::build(
            &config.det_model,
            &config.rec_model,
            &config.dict_path,
            &config.dll_path,
            config.intra_op,
            config.inter_op,
        )
        .map(|p| Box::new(p) as Box<dyn OcrPipeline>)
    })
}

/// 退休 worker——包含 JoinHandle 和退出信号。
/// 线程在 native 构建完成后检测到 sender 被 drop 自然退出。
struct RetiredWorker {
    handle: Option<std::thread::JoinHandle<()>>,
}

/// ONNX OCR in-process executor.
///
/// 持有专用阻塞线程和有界请求队列，topology-neutral。
///
/// ## Worker 生命周期与代际隔离
///
/// 每一代 worker 都有独立 generation identity。当超时触发新 generation 构建时：
/// - 旧 generation 的 sender 被 drop，旧线程在完成构建后检测到 sender 被 drop 即退出
/// - 旧 generation 的 JoinHandle 被移入 `retired_workers` 列表
/// - 旧 generation 不得提交 Ready 状态，也不得接收新请求
/// - `in_flight_count`（active + retired-未退出）受 `MAX_INFLIGHT_WORKERS` 限制
/// - 达到上限时返回 `BuildInProgress`，不创建新线程
pub struct OnnxOcrExecutor {
    /// 状态通道。
    state: StateChannel,
    /// 请求 sender（发送给当前 generation 的工作线程）。
    /// `None` 表示 executor 已关闭或未启动。
    req_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<WorkerRequest>>>>,
    /// 有界队列信号量（max 4 pending）。
    pending_sem: Arc<Semaphore>,
    /// 配置。
    config: OcrExecutorConfig,
    /// 当前工作线程 join handle（用于 shutdown 时等待）。
    worker_handle: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
    /// 退休 worker 列表——超时后旧 worker 在此异步回收。
    retired_workers: Arc<std::sync::Mutex<Vec<RetiredWorker>>>,
    /// 存活 worker 计数（active + retired-未退出）。
    /// 受 `MAX_INFLIGHT_WORKERS` 限制。
    in_flight_count: Arc<AtomicU32>,
    /// 可注入的 pipeline 构建函数（测试用）。
    build_fn: BuildFn,
    /// Session 构建超时（测试可注入短超时）。
    build_timeout: Duration,
}

impl OnnxOcrExecutor {
    /// 创建 executor（不启动工作线程，lazy 到首次请求）。
    pub fn new(config: OcrExecutorConfig) -> Self {
        Self::with_build_fn(config, default_build_fn())
    }

    /// 创建 executor 并注入构建函数（测试用）。
    pub(super) fn with_build_fn(config: OcrExecutorConfig, build_fn: BuildFn) -> Self {
        Self {
            state: StateChannel::new(),
            req_sender: Arc::new(std::sync::Mutex::new(None)),
            pending_sem: Arc::new(Semaphore::new(MAX_PENDING)),
            config,
            worker_handle: Arc::new(std::sync::Mutex::new(None)),
            retired_workers: Arc::new(std::sync::Mutex::new(Vec::new())),
            in_flight_count: Arc::new(AtomicU32::new(0)),
            build_fn,
            build_timeout: Duration::from_secs(SESSION_BUILD_TIMEOUT_SECS),
        }
    }

    /// 测试用：设置构建超时。
    #[cfg(test)]
    pub(super) fn with_build_timeout(mut self, timeout: Duration) -> Self {
        self.build_timeout = timeout;
        self
    }

    /// 获取当前状态快照。
    pub fn state(&self) -> ExecutorState {
        self.state.current()
    }

    /// 测试用：检查 req_sender 是否为 None。
    #[cfg(test)]
    pub(super) fn is_sender_none(&self) -> bool {
        self.req_sender
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_none()
    }

    /// 测试用：获取存活 worker 数量（active + retired-未退出）。
    #[cfg(test)]
    pub(super) fn in_flight_count(&self) -> u32 {
        self.in_flight_count.load(Ordering::SeqCst)
    }

    /// 测试用：检查是否有退休 worker 存在。
    #[cfg(test)]
    pub(super) fn retired_count(&self) -> usize {
        self.retired_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .len()
    }

    /// 测试用：回收已完成的退休 worker，恢复 in-flight 额度。
    #[cfg(test)]
    pub(super) fn reclaim_finished(&self) {
        self.reclaim_finished_inner();
    }

    /// 确保工作线程已启动且 Session 就绪。
    ///
    /// 使用 watch + starting gate 合并并发启动请求。
    pub async fn ensure_ready(
        &self,
        cancellation: &CancellationToken,
    ) -> Result<(), OcrExecutorError> {
        let mut rx = self.state.subscribe();
        let mut participating_gen = None;

        loop {
            if cancellation.is_cancelled() {
                return Err(OcrExecutorError::Cancelled);
            }

            let current = rx.borrow().clone();

            match &current {
                ExecutorState::Idle => {
                    participating_gen = Some(current.generation());

                    // 先回收已完成的退休 worker，恢复额度
                    self.reclaim_finished_inner();

                    // 检查 in-flight worker 上限
                    let in_flight = self.in_flight_count.load(Ordering::SeqCst);
                    if in_flight as usize >= MAX_INFLIGHT_WORKERS {
                        tracing::warn!(
                            in_flight,
                            max = MAX_INFLIGHT_WORKERS,
                            "OnnxOcrExecutor: 存活 worker 达上限，拒绝创建新 worker"
                        );
                        return Err(OcrExecutorError::BuildInProgress);
                    }

                    // CAS: Idle → Starting（只有一个 winner）
                    let target_gen = current.generation();
                    let won = self.state.compare_swap(
                        |s| matches!(s, ExecutorState::Idle),
                        ExecutorState::Starting {
                            generation: target_gen,
                        },
                    );

                    if !won {
                        // 已有 winner——等待状态变化
                        tokio::select! {
                            _ = rx.changed() => {}
                            _ = cancellation.cancelled() => return Err(OcrExecutorError::Cancelled),
                        }
                        continue;
                    }

                    // 是 winner——启动工作线程并构建 Session
                    tracing::info!(
                        generation = target_gen,
                        "OnnxOcrExecutor: winner, 启动 Session 构建"
                    );

                    match self.start_worker_and_build().await {
                        Ok(()) => {
                            // 成功——state 已在 start_worker_and_build 中更新为 Ready
                            return Ok(());
                        }
                        Err(e) => {
                            // 失败——更新 state 为 Failed
                            self.state
                                .tx
                                .send(ExecutorState::Failed {
                                    generation: target_gen,
                                    reason: Arc::from(e.to_string().as_str()),
                                })
                                .ok();
                            return Err(e);
                        }
                    }
                }
                ExecutorState::Starting { generation } => {
                    participating_gen = Some(*generation);
                    tracing::debug!(generation, "OnnxOcrExecutor: Starting, 等待");
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = cancellation.cancelled() => return Err(OcrExecutorError::Cancelled),
                    }
                    continue;
                }
                ExecutorState::Ready { .. } => {
                    return Ok(());
                }
                ExecutorState::Stopping { generation } => {
                    participating_gen = Some(*generation);
                    tracing::debug!(generation, "OnnxOcrExecutor: Stopping, 等待");
                    tokio::select! {
                        _ = rx.changed() => {}
                        _ = cancellation.cancelled() => return Err(OcrExecutorError::Cancelled),
                    }
                    continue;
                }
                ExecutorState::Failed { generation, reason } => {
                    if participating_gen == Some(*generation) {
                        // 参与了本轮失败——返回错误
                        return Err(OcrExecutorError::BuildFailed(reason.to_string()));
                    }
                    // 新请求——推进到 Idle（重试）
                    let failed_gen = *generation;
                    self.state.compare_swap(
                        |s| matches!(s, ExecutorState::Failed { generation, .. } if *generation == failed_gen),
                        ExecutorState::Idle,
                    );
                    continue;
                }
            }
        }
    }

    /// 将旧 worker 退休——drop sender 并将 handle 移入退休列表。
    ///
    /// 退休的 worker 在完成当前构建后检测到 sender 被 drop 即退出。
    /// 退休 worker 仍计入 `in_flight_count`，直到线程退出被 `reclaim_finished` 回收。
    fn retire_current_worker(&self) {
        // 1. drop 旧 sender → 旧线程 recv 返回 Err → 退出
        //    对于尚未完成构建的线程，构建完成后 recv 直接返回 Err 退出
        let old_sender = {
            let mut guard = self.req_sender.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        drop(old_sender);

        // 2. 取出旧 handle，移入退休列表
        let old_handle = {
            let mut guard = self.worker_handle.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(handle) = old_handle {
            let mut retired = self
                .retired_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            retired.push(RetiredWorker {
                handle: Some(handle),
            });
            tracing::info!(
                retired_count = retired.len(),
                in_flight = self.in_flight_count.load(Ordering::SeqCst),
                "旧 worker 已退休，等待构建完成后自然退出"
            );
        }
    }

    /// 释放一个已结束 worker 占用的额度。
    ///
    /// 每个成功 spawn 的 handle 只能调用一次；使用 checked update 防止生命周期
    /// 缺陷在 release 构建中把计数器下溢成极大值。
    fn release_worker_quota(&self, reason: &'static str) {
        if self
            .in_flight_count
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |count| {
                count.checked_sub(1)
            })
            .is_err()
        {
            tracing::error!(reason, "worker quota 重复释放或计数已失真");
        }
    }

    /// init channel 已返回失败时，构建线程已经退出（或正在完成 unwind）。
    /// 在这里同步收走 current sender/handle，并且只释放一次 quota，避免下一次
    /// retry 又把同一个已计数释放的 handle 退休并在 reclaim 时二次递减。
    fn reap_failed_current_worker(&self) {
        {
            let mut sender = self.req_sender.lock().unwrap_or_else(|e| e.into_inner());
            *sender = None;
        }
        let handle = {
            let mut current = self.worker_handle.lock().unwrap_or_else(|e| e.into_inner());
            current.take()
        };
        if let Some(handle) = handle {
            let _ = handle.join();
            self.release_worker_quota("init_failed");
        }
    }

    /// 回收已完成的退休 worker——只 join 已退出的线程。
    ///
    /// `JoinHandle::is_finished()` 判断线程是否已退出，只 join 已完成的。
    /// 未完成的线程保留在列表中，仍计入 `in_flight_count`。
    /// 回收完成后递减 `in_flight_count`，恢复构建额度。
    fn reclaim_finished_inner(&self) {
        let finished = {
            let mut retired = self
                .retired_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            let mut pending = Vec::with_capacity(retired.len());
            let mut finished = Vec::new();
            for mut worker in retired.drain(..) {
                match worker.handle.take() {
                    Some(handle) if handle.is_finished() => finished.push(handle),
                    Some(handle) => pending.push(RetiredWorker {
                        handle: Some(handle),
                    }),
                    None => {}
                }
            }
            *retired = pending;
            finished
        };

        let reclaimed = finished.len();
        for handle in finished {
            let _ = handle.join();
            self.release_worker_quota("retired_finished");
        }

        if reclaimed > 0 {
            let remaining_retired = self
                .retired_workers
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .len();
            tracing::info!(
                reclaimed,
                remaining_retired,
                in_flight = self.in_flight_count.load(Ordering::SeqCst),
                "回收已退出的退休 worker"
            );
        }
    }

    /// 启动工作线程并构建 Session。
    ///
    /// 在专用 `std::thread` 上构建 pipeline（通过 `build_fn`），
    /// 构建成功后更新状态为 Ready 并进入 recv 循环。
    ///
    /// **代际隔离**：如果当前已有 worker（超时重试场景），
    /// 先将旧 worker 退休（drop sender + 移入退休列表），
    /// 再启动新 generation 的 worker。这确保：
    /// - 旧 worker 构建完成后不会提交 Ready（sender 已 drop，recv 返回 Err 退出）
    /// - 旧 worker 不会接收新请求
    /// - JoinHandle 不会被静默覆盖丢弃
    async fn start_worker_and_build(&self) -> Result<(), OcrExecutorError> {
        let config = self.config.pipeline.clone();
        let (init_tx, init_rx) = oneshot::channel();

        let req_sender = self.req_sender.clone();
        let worker_handle = self.worker_handle.clone();
        let build_fn = self.build_fn.clone();

        // 如果当前已有 worker（超时重试场景），先将旧 worker 退休
        let had_existing = {
            let guard = req_sender.lock().unwrap_or_else(|e| e.into_inner());
            guard.is_some()
        };
        if had_existing {
            tracing::info!("start_worker_and_build: 检测到已有 worker，将其退休");
            self.retire_current_worker();
        }

        // 创建 mpsc channel 用于请求传递
        let (sender, receiver) = std::sync::mpsc::channel::<WorkerRequest>();

        // 存储 sender
        {
            let mut guard = req_sender.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(sender);
        }

        // 递增 in-flight 计数
        self.in_flight_count.fetch_add(1, Ordering::SeqCst);

        // 记录启动时的 target generation——用于构建完成后验证状态一致性
        let target_gen = self.state.current().generation();

        let thread = std::thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_string())
            .spawn(move || {
                tracing::info!(generation = target_gen, "OnnxOcrExecutor worker thread 启动");

                // 1. 构建 pipeline（通过注入的 build_fn）
                let pipeline_result = build_fn(&config);

                let mut pipeline: Box<dyn OcrPipeline> = match pipeline_result {
                    Ok(p) => {
                        tracing::info!(generation = target_gen, "OnnxOcrExecutor: pipeline 构建成功");
                        let _ = init_tx.send(Ok(()));
                        p
                    }
                    Err(e) => {
                        tracing::error!(generation = target_gen, error = %e, "OnnxOcrExecutor: pipeline 构建失败");
                        let _ = init_tx.send(Err(OcrExecutorError::BuildFailed(e.to_string())));
                        return;
                    }
                };

                // 2. recv 循环
                // 如果 sender 已被 drop（超时后退休），recv 返回 Err → 线程退出
                // 此时 pipeline 已构建完成但不会被使用——资源通过 drop 自然释放
                loop {
                    match receiver.recv() {
                        Ok(req) => {
                            let result = pipeline.recognize(&req.png_data);
                            let _ = req.result_tx.send(result);
                        }
                        Err(_) => {
                            // sender 被 drop——线程退出
                            // 可能是正常 shutdown，也可能是超时后被退休
                            tracing::info!(
                                generation = target_gen,
                                "OnnxOcrExecutor worker thread 退出（channel closed）"
                            );
                            break;
                        }
                    }
                }
            })
            .map_err(|e| {
                // 线程启动失败——回滚 in-flight 计数
                {
                    let mut sender = self.req_sender.lock().unwrap_or_else(|p| p.into_inner());
                    *sender = None;
                }
                self.release_worker_quota("spawn_failed");
                OcrExecutorError::BuildFailed(format!("工作线程启动失败: {e}"))
            })?;

        // 存储 join handle
        {
            let mut guard = worker_handle.lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(thread);
        }

        // 等待 pipeline 构建结果（有超时）
        let build_result = tokio::time::timeout(self.build_timeout, init_rx).await;

        match build_result {
            Err(_) => {
                // 超时——立即 drop sender，撤销该 generation 的提交权
                // native 构建不可中断，但 sender 被 drop 后：
                // - 构建完成后 recv 返回 Err → 线程自然退出
                // - 不会进入 recv 循环持有 pipeline
                tracing::warn!(
                    generation = target_gen,
                    timeout_secs = SESSION_BUILD_TIMEOUT_SECS,
                    "Session 构建超时，立即撤销提交权（drop sender）"
                );

                // 立即 drop sender —— 撤销提交权
                {
                    let mut guard = self.req_sender.lock().unwrap_or_else(|e| e.into_inner());
                    *guard = None;
                }

                // 将当前 worker 退休（移入 retired 列表，in_flight_count 不变）
                // 退休 worker 在 native 构建完成后自然退出
                let old_handle = {
                    let mut guard = self.worker_handle.lock().unwrap_or_else(|e| e.into_inner());
                    guard.take()
                };
                if let Some(handle) = old_handle {
                    let mut retired = self
                        .retired_workers
                        .lock()
                        .unwrap_or_else(|e| e.into_inner());
                    retired.push(RetiredWorker {
                        handle: Some(handle),
                    });
                }

                // 更新状态为 Failed
                self.state
                    .tx
                    .send(ExecutorState::Failed {
                        generation: target_gen,
                        reason: Arc::from("Session 构建超时"),
                    })
                    .ok();
                Err(OcrExecutorError::BuildFailed(format!(
                    "Session 构建超时（{SESSION_BUILD_TIMEOUT_SECS}s）"
                )))
            }
            Ok(Err(_)) => {
                // init_tx 被 drop（线程 panic 或提前退出）
                self.reap_failed_current_worker();

                self.state
                    .tx
                    .send(ExecutorState::Failed {
                        generation: target_gen,
                        reason: Arc::from("工作线程异常退出"),
                    })
                    .ok();
                Err(OcrExecutorError::BuildFailed(
                    "工作线程异常退出".to_string(),
                ))
            }
            Ok(Ok(Err(e))) => {
                // 构建失败——收走 current handle 并释放其唯一 quota。
                self.reap_failed_current_worker();

                self.state
                    .tx
                    .send(ExecutorState::Failed {
                        generation: target_gen,
                        reason: Arc::from(e.to_string().as_str()),
                    })
                    .ok();
                Err(e)
            }
            Ok(Ok(Ok(()))) => {
                // 构建成功——只有在状态仍为 Starting { generation: target_gen } 时才提交 Ready
                let current = self.state.current();
                match current {
                    ExecutorState::Starting { generation } if generation == target_gen => {
                        self.state
                            .tx
                            .send(ExecutorState::Ready {
                                generation: target_gen,
                                ready_at: std::time::Instant::now(),
                            })
                            .ok();
                        tracing::info!(generation = target_gen, "OnnxOcrExecutor: Ready");
                        Ok(())
                    }
                    _ => {
                        // 状态已变化（超时重试或其他 generation 接管）——
                        // 丢弃 Ready，不覆盖新 generation 的状态
                        // 将此 worker 退休
                        tracing::warn!(
                            generation = target_gen,
                            current = %current,
                            "OnnxOcrExecutor: 构建完成但状态已变化，丢弃 Ready"
                        );
                        self.retire_current_worker();
                        Err(OcrExecutorError::BuildFailed(
                            "Session 构建完成但 generation 已过期".to_string(),
                        ))
                    }
                }
            }
        }
    }

    /// 关闭工作线程（内部方法）——有界 join 回收。
    ///
    /// 使用辅助线程 + timeout 实现 join 有界等待：
    /// - 先 drop sender 让工作线程退出
    /// - 在辅助线程上 join，主线程等待辅助线程（有 timeout）
    /// - 超时后辅助线程 detached，原工作线程继续自然退出
    fn close_worker(&self) {
        // drop sender → 工作线程 recv 返回 Err → 线程退出
        {
            let mut guard = self.req_sender.lock().unwrap_or_else(|e| e.into_inner());
            *guard = None;
        }
        // 取出当前 handle
        let handle_opt = {
            let mut guard = self.worker_handle.lock().unwrap_or_else(|e| e.into_inner());
            guard.take()
        };
        if let Some(handle) = handle_opt {
            // 有界 join：在辅助线程上等待，主线程设 timeout
            let join_result = bounded_join(handle, Duration::from_secs(SHUTDOWN_JOIN_TIMEOUT_SECS));
            match join_result {
                JoinResult::Ok => {
                    tracing::debug!("OnnxOcrExecutor: 工作线程已退出");
                    self.in_flight_count.fetch_sub(1, Ordering::SeqCst);
                }
                JoinResult::Panic => {
                    tracing::warn!("OnnxOcrExecutor: 工作线程 panic 退出");
                    self.in_flight_count.fetch_sub(1, Ordering::SeqCst);
                }
                JoinResult::Timeout => {
                    tracing::warn!(
                        timeout_secs = SHUTDOWN_JOIN_TIMEOUT_SECS,
                        "OnnxOcrExecutor: 工作线程 join 超时，线程 detached（native build 完成后自然退出）"
                    );
                    // 线程 detached——in_flight_count 不递减
                    // 线程在 native build 完成后因 sender 已 drop 自然退出
                }
            }
        }
        // 回收所有已完成的退休 worker
        self.reclaim_finished_inner();
        // 对未完成的退休 worker 也尝试有界 join
        let mut retired = self
            .retired_workers
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for rw in retired.drain(..) {
            if let Some(handle) = rw.handle {
                let join_result =
                    bounded_join(handle, Duration::from_secs(SHUTDOWN_JOIN_TIMEOUT_SECS));
                match join_result {
                    JoinResult::Ok | JoinResult::Panic => {
                        self.in_flight_count.fetch_sub(1, Ordering::SeqCst);
                    }
                    JoinResult::Timeout => {
                        tracing::warn!("退休 worker join 超时，detached");
                    }
                }
            }
        }
    }
}

/// 有界 join 结果。
enum JoinResult {
    /// 线程正常退出。
    Ok,
    /// 线程 panic 退出。
    Panic,
    /// join 超时——线程 detached，仍在运行。
    Timeout,
}

/// 在辅助线程上执行有界 join。
///
/// `JoinHandle::join()` 本身不支持 timeout。此函数在另一个辅助线程上
/// 执行 join，主线程通过 channel + timeout 等待结果。
/// 超时后辅助线程被 detached，原线程继续自然退出。
fn bounded_join(handle: std::thread::JoinHandle<()>, timeout: Duration) -> JoinResult {
    let (tx, rx) = std::sync::mpsc::channel();
    let _helper = std::thread::spawn(move || {
        let result = handle.join();
        let _ = tx.send(result);
    });
    match rx.recv_timeout(timeout) {
        Ok(Ok(())) => JoinResult::Ok,
        Ok(Err(_)) => JoinResult::Panic,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => JoinResult::Timeout,
        // 此分支不应出现——helper 线程退出必先 send
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => JoinResult::Panic,
    }
}

#[async_trait::async_trait]
impl OcrExecutor for OnnxOcrExecutor {
    async fn recognize(&self, request: RecognizeRequest) -> Result<OcrResult, OcrExecutorError> {
        // 1. 确保 Session 就绪
        self.ensure_ready(&request.cancellation).await?;

        // 2. 获取 pending permit（有界队列——立即背压）
        let _permit: SemaphorePermit = self
            .pending_sem
            .try_acquire()
            .map_err(|_| OcrExecutorError::Backpressure(MAX_PENDING))?;

        // 3. 发送请求到工作线程（不跨 await 持有 MutexGuard）
        let (result_tx, result_rx) = oneshot::channel();
        {
            let sender_guard = self.req_sender.lock().unwrap_or_else(|e| e.into_inner());
            let sender = sender_guard.as_ref().ok_or(OcrExecutorError::Shutdown)?;
            sender
                .send(WorkerRequest {
                    png_data: request.png_data.to_vec(),
                    result_tx,
                })
                .map_err(|_| OcrExecutorError::Shutdown)?;
        } // guard 在此 drop

        // 4. 等待结果（支持取消和超时）
        let result = tokio::select! {
            r = result_rx => {
                r.map_err(|_| OcrExecutorError::Shutdown)?
            }
            _ = request.cancellation.cancelled() => {
                return Err(OcrExecutorError::Cancelled);
            }
            _ = async {
                if let Some(deadline) = request.deadline {
                    tokio::time::sleep_until(deadline).await;
                } else {
                    std::future::pending::<()>().await;
                }
            } => {
                return Err(OcrExecutorError::Timeout);
            }
        };

        // 5. 映射结果
        result.map_err(|e| OcrExecutorError::Pipeline(e.to_string()))
    }

    async fn shutdown(&self) {
        tracing::info!("OnnxOcrExecutor shutdown: 关闭工作线程");
        // 在 spawn_blocking 中执行 close_worker，避免在 tokio worker 上同步 join
        // close_worker 内部使用有界 join，不会无限阻塞
        let req_sender = self.req_sender.clone();
        let worker_handle = self.worker_handle.clone();
        let retired_workers = self.retired_workers.clone();
        let in_flight_count = self.in_flight_count.clone();

        tokio::task::spawn_blocking(move || {
            // drop sender
            {
                let mut guard = req_sender.lock().unwrap_or_else(|e| e.into_inner());
                *guard = None;
            }
            // 有界 join 当前 worker
            let handle_opt = {
                let mut guard = worker_handle.lock().unwrap_or_else(|e| e.into_inner());
                guard.take()
            };
            if let Some(handle) = handle_opt {
                let join_result =
                    bounded_join(handle, Duration::from_secs(SHUTDOWN_JOIN_TIMEOUT_SECS));
                match join_result {
                    JoinResult::Ok => {
                        tracing::debug!("OnnxOcrExecutor: 工作线程已退出");
                        in_flight_count.fetch_sub(1, Ordering::SeqCst);
                    }
                    JoinResult::Panic => {
                        tracing::warn!("OnnxOcrExecutor: 工作线程 panic 退出");
                        in_flight_count.fetch_sub(1, Ordering::SeqCst);
                    }
                    JoinResult::Timeout => {
                        tracing::warn!("OnnxOcrExecutor: 工作线程 join 超时，detached");
                    }
                }
            }
            // 有界 join 退休 worker
            let mut retired = retired_workers.lock().unwrap_or_else(|e| e.into_inner());
            for rw in retired.drain(..) {
                if let Some(handle) = rw.handle {
                    let join_result =
                        bounded_join(handle, Duration::from_secs(SHUTDOWN_JOIN_TIMEOUT_SECS));
                    match join_result {
                        JoinResult::Ok | JoinResult::Panic => {
                            in_flight_count.fetch_sub(1, Ordering::SeqCst);
                        }
                        JoinResult::Timeout => {
                            tracing::warn!("退休 worker join 超时，detached");
                        }
                    }
                }
            }
        })
        .await
        .ok();

        let target_gen = self.state.current().generation();
        self.state
            .tx
            .send(ExecutorState::Stopping {
                generation: target_gen,
            })
            .ok();
        self.state.tx.send(ExecutorState::Idle).ok();
        tracing::info!("OnnxOcrExecutor shutdown 完成");
    }
}

impl Drop for OnnxOcrExecutor {
    fn drop(&mut self) {
        // 确保工作线程退出
        self.close_worker();
    }
}
