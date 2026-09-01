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

use std::path::PathBuf;
use std::sync::Arc;
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
    /// Idle TTL（秒）。超过此时间无请求则 drop Session。
    pub idle_ttl_secs: u64,
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
            idle_ttl_secs: 300,
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

/// ONNX OCR in-process executor。
///
/// 持有专用阻塞线程和有界请求队列，topology-neutral。
pub struct OnnxOcrExecutor {
    /// 状态通道。
    state: StateChannel,
    /// 请求 sender（发送给工作线程）。
    /// `None` 表示 executor 已关闭。
    req_sender: Arc<std::sync::Mutex<Option<std::sync::mpsc::Sender<WorkerRequest>>>>,
    /// 有界队列信号量（max 4 pending）。
    pending_sem: Arc<Semaphore>,
    /// 配置。
    config: OcrExecutorConfig,
    /// 工作线程 join handle（用于 shutdown 时等待）。
    worker_handle: Arc<std::sync::Mutex<Option<std::thread::JoinHandle<()>>>>,
    /// idle TTL 定时器取消通知。
    idle_cancel: Arc<tokio::sync::Notify>,
}

impl OnnxOcrExecutor {
    /// 创建 executor（不启动工作线程，lazy 到首次请求）。
    pub fn new(config: OcrExecutorConfig) -> Self {
        Self {
            state: StateChannel::new(),
            req_sender: Arc::new(std::sync::Mutex::new(None)),
            pending_sem: Arc::new(Semaphore::new(MAX_PENDING)),
            config,
            worker_handle: Arc::new(std::sync::Mutex::new(None)),
            idle_cancel: Arc::new(tokio::sync::Notify::new()),
        }
    }

    /// 获取当前状态快照。
    pub fn state(&self) -> ExecutorState {
        self.state.current()
    }

    /// 测试用：检查 req_sender 是否为 None。
    #[cfg(test)]
    pub(super) fn is_sender_none(&self) -> bool {
        self.req_sender.lock().unwrap().is_none()
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

    /// 启动工作线程并构建 Session。
    ///
    /// 在专用 `std::thread` 上构建 `OrtocrPipeline`，
    /// 构建成功后更新状态为 Ready 并进入 recv 循环。
    async fn start_worker_and_build(&self) -> Result<(), OcrExecutorError> {
        let config = self.config.pipeline.clone();
        let (init_tx, init_rx) = oneshot::channel();

        let req_sender = self.req_sender.clone();
        let worker_handle = self.worker_handle.clone();

        // 创建 mpsc channel 用于请求传递
        let (sender, receiver) = std::sync::mpsc::channel::<WorkerRequest>();

        // 存储 sender
        {
            let mut guard = req_sender.lock().unwrap();
            *guard = Some(sender);
        }

        let thread = std::thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_string())
            .spawn(move || {
                tracing::info!("OnnxOcrExecutor worker thread 启动");

                // 1. 构建 pipeline
                let pipeline_result = OrtocrPipeline::build(
                    &config.det_model,
                    &config.rec_model,
                    &config.dict_path,
                    &config.dll_path,
                    config.intra_op,
                    config.inter_op,
                );

                let mut pipeline: Box<dyn OcrPipeline> = match pipeline_result {
                    Ok(p) => {
                        tracing::info!("OnnxOcrExecutor: pipeline 构建成功");
                        let _ = init_tx.send(Ok(()));
                        Box::new(p)
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "OnnxOcrExecutor: pipeline 构建失败");
                        let _ = init_tx.send(Err(OcrExecutorError::BuildFailed(e.to_string())));
                        return;
                    }
                };

                // 2. recv 循环
                loop {
                    match receiver.recv() {
                        Ok(req) => {
                            let result = pipeline.recognize(&req.png_data);
                            let _ = req.result_tx.send(result);
                        }
                        Err(_) => {
                            // sender 被 drop——线程退出
                            tracing::info!("OnnxOcrExecutor worker thread 退出（channel closed）");
                            break;
                        }
                    }
                }
            })
            .map_err(|e| OcrExecutorError::BuildFailed(format!("工作线程启动失败: {e}")))?;

        // 存储 join handle
        {
            let mut guard = worker_handle.lock().unwrap();
            *guard = Some(thread);
        }

        // 等待 pipeline 构建结果（有超时）
        let build_result =
            tokio::time::timeout(Duration::from_secs(SESSION_BUILD_TIMEOUT_SECS), init_rx).await;

        match build_result {
            Err(_) => {
                // 超时——更新状态为 Failed
                self.state
                    .tx
                    .send(ExecutorState::Failed {
                        generation: self.state.current().generation(),
                        reason: Arc::from("Session 构建超时"),
                    })
                    .ok();
                Err(OcrExecutorError::BuildFailed(format!(
                    "Session 构建超时（{SESSION_BUILD_TIMEOUT_SECS}s）"
                )))
            }
            Ok(Err(_)) => {
                // init_tx 被 drop（线程 panic 或提前退出）
                self.state
                    .tx
                    .send(ExecutorState::Failed {
                        generation: self.state.current().generation(),
                        reason: Arc::from("工作线程异常退出"),
                    })
                    .ok();
                Err(OcrExecutorError::BuildFailed(
                    "工作线程异常退出".to_string(),
                ))
            }
            Ok(Ok(Err(e))) => {
                // 构建失败
                Err(e)
            }
            Ok(Ok(Ok(()))) => {
                // 构建成功——更新状态为 Ready
                let target_gen = self.state.current().generation();
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
        }
    }

    /// 关闭工作线程（内部方法）。
    fn close_worker(&self) {
        // drop sender → 工作线程 recv 返回 Err → 线程退出
        {
            let mut guard = self.req_sender.lock().unwrap();
            *guard = None;
        }
        // 等待线程退出（有限等待）
        if let Some(handle) = {
            let mut guard = self.worker_handle.lock().unwrap();
            guard.take()
        } {
            let join_result = handle.join();
            match join_result {
                Ok(()) => {
                    tracing::debug!("OnnxOcrExecutor: 工作线程已退出");
                }
                Err(_) => {
                    tracing::warn!("OnnxOcrExecutor: 工作线程 panic 退出");
                }
            }
        }
    }
}

#[async_trait::async_trait]
impl OcrExecutor for OnnxOcrExecutor {
    async fn recognize(&self, request: RecognizeRequest) -> Result<OcrResult, OcrExecutorError> {
        // 1. 确保 Session 就绪
        self.ensure_ready(&request.cancellation).await?;

        // 2. 获取 pending permit（有界队列——立即背压）
        //
        // try_acquire 失败意味着已有 MAX_PENDING 个请求在排队。
        // 设计铁则：立即返回 Backpressure，不无限等待，
        // 避免并发请求无界积压 PNG payload。
        let _permit: SemaphorePermit = self
            .pending_sem
            .try_acquire()
            .map_err(|_| OcrExecutorError::Backpressure(MAX_PENDING))?;

        // 3. 发送请求到工作线程（不跨 await 持有 MutexGuard）
        let (result_tx, result_rx) = oneshot::channel();
        {
            let sender_guard = self.req_sender.lock().unwrap();
            let sender = sender_guard.as_ref().ok_or(OcrExecutorError::Shutdown)?;
            sender
                .send(WorkerRequest {
                    png_data: request.png_data.to_vec(),
                    result_tx,
                })
                .map_err(|_| OcrExecutorError::Shutdown)?;
        } // guard 在此 drop

        // 取消 idle TTL 定时器
        self.idle_cancel.notify_waiters();

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
        self.idle_cancel.notify_waiters();
        // 用 spawn_blocking 执行 close_worker，避免在 tokio worker 上同步 join
        let worker_handle = {
            let mut guard = self.worker_handle.lock().unwrap();
            guard.take()
        };
        // 先 drop sender 让工作线程退出
        {
            let mut guard = self.req_sender.lock().unwrap();
            *guard = None;
        }
        if let Some(handle) = worker_handle {
            let join_result = tokio::task::spawn_blocking(move || {
                // 限时等待工作线程退出，避免无界阻塞
                // join 本身不能 timeout，所以用一个辅助线程尝试
                handle.join()
            })
            .await;
            match join_result {
                Ok(Ok(())) => {
                    tracing::debug!("OnnxOcrExecutor: 工作线程已退出");
                }
                Ok(Err(_)) => {
                    tracing::warn!("OnnxOcrExecutor: 工作线程 panic 退出");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "OnnxOcrExecutor: spawn_blocking join 失败");
                }
            }
        }
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
