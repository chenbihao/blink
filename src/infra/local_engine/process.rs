//! ManagedProcess — 安全的通用子进程生命周期管理（0.22.1）。
//!
//! 从 FunASR maintenance 提取通用进程句柄与 stdout/stderr pump。
//! 用 ring buffer + broadcast 防止日志消费者缺席导致无界内存。
//! 引入 instance_id、PID/executable 身份和结构化退出原因。
//! 实现幂等 start/stop、启动取消、退出 wait、进程树回收与应用退出同步收尾。
//! 端口冲突不杀未知进程；只回收能证明属于 Blink 当前/遗留实例的进程。
//!
//! ## 并发安全（0.22.1 最终版）
//!
//! - start/start、stop/stop、start/stop 并发得到确定、最终一致的状态。
//! - 每次 start 生成唯一 `InstanceToken`（generation + instance_id）。
//! - **StartOperation 与 token 原子创建**：在单次 inner lock 内完成
//!   "检查状态 → 创建 token → 创建 StartOperation → 状态进入 Starting"。
//!   重复 start 不创建、覆盖或完成当前 StartOperation。
//! - **StopOperation 与 Stopping 原子创建**：首个 stop 在单次 inner lock 内
//!   完成 "验证 token → 创建 StopOperation → 状态改为 Stopping → 标记为 executor"。
//!   看到 Stopping 的后续 stop 只订阅已存在的 StopOperation，绝不创建新 operation。
//! - **OperationCompletion** 持久保存最终结果，后订阅的 waiter 仍能立即读到。
//! - Running 提交必须验证 token 匹配 + 状态为 Starting + 未取消。
//! - stop 在 Starting 阶段设置 cancellation flag，使迟到的 spawn 无法提交 Running。
//! - 如果 child 已 spawn 但提交被取消，必须回收该 child 及其进程树。
//! - 旧 generation 的退出事件不能覆盖新 generation。
//! - stdout/stderr task 比 child wait 更晚结束不会导致资源泄漏。
//! - child wait 所有权唯一：由 stop executor 或 wait_and_update 独占。
//!
//! ## 锁职责与锁顺序
//!
//! | 锁 | 类型 | 职责 | 临界区内禁止 |
//! |---|---|---|---|
//! | `inner` | `tokio::Mutex` | 状态快照、child handle、cancellation、start_op、stop_op | await 其他锁、await child |
//! | `job_holder` | `std::sync::Mutex` | Job Object handle take/replace | await 任何东西 |
//!
//! 锁顺序：inner → job_holder（同步，极短临界区）。
//! 禁止：持有 inner 时 await 任何东西（job_holder 除外，它是同步锁）。
//! 等待 operation completion 时不持有 inner 锁。

use std::collections::HashMap;
use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::process::Child;
use tokio::sync::{Mutex, watch};

use super::log_pipe::{
    LineAccumulator, LogEntry, LogPipe, LogPipeConfig, LogSource, LogSubscriber,
};
use super::state::{
    CommitResult, ExitReason, InstanceToken, ManagedProcessState, ProcessIdentity, ProcessStatus,
};

/// ManagedProcess 错误类型。
#[derive(Debug, thiserror::Error)]
pub enum ManagedProcessError {
    #[error("进程已在运行 (generation {generation})")]
    AlreadyRunning { generation: u64 },
    #[error("启动失败: {message}")]
    SpawnFailed { message: String },
    #[error("停止失败: {message}")]
    StopFailed { message: String },
    #[error("Windows Job Object 分配失败: {message}")]
    JobObjectFailed { message: String },
    #[error("内部状态不一致: {message}")]
    InternalInconsistency { message: String },
}

/// 强制停止配置（0.22.1：删除虚假 graceful 抽象）。
///
/// 当前没有真实调用方需要 pre_stop_hook（graceful HTTP /shutdown）。
/// stop 直接通过 child start_kill + Job Object 强制回收。
/// `force_stop_timeout` 是 start_kill 后等待 child 退出的超时；
/// 超时后通过 Job Object CloseHandle 强制回收进程树。
///
/// 未来如需 graceful stop（如 FunASR HTTP /shutdown），
/// 应在此处新增真实的 pre_stop 阶段，不要恢复虚假的 None hook。
#[derive(Clone, Debug)]
pub struct ShutdownConfig {
    /// start_kill 后等待 child 退出的超时时间。
    /// 超时后通过 Job Object CloseHandle 强制回收进程树。
    pub force_stop_timeout: Duration,
}

impl Default for ShutdownConfig {
    fn default() -> Self {
        Self {
            force_stop_timeout: Duration::from_secs(10),
        }
    }
}

/// 受管子进程启动请求（内部 Rust API，非 serde IPC DTO）。
///
/// 由可信的 Rust 调用方构造，不接受前端传入。
///
/// ## 日志配置
///
/// 日志配置（history/broadcast/max_line_bytes）属于 `ManagedProcess`，
/// 在 `ManagedProcess::new(log_config)` 时确定，是唯一真源。
/// `LaunchRequest` 不再携带 `log_config`（0.22.1 收敛）。
#[derive(Debug, Clone)]
pub struct LaunchRequest {
    pub executable: PathBuf,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    pub env: HashMap<String, String>,
    /// 当前启动路径用独立 instance_id 变量传递；字段保留供测试断言。
    #[allow(dead_code)]
    pub instance_id: String,
    pub label: String,
    pub shutdown: ShutdownConfig,
}

impl LaunchRequest {
    #[allow(dead_code)] // 测试便捷构造
    pub fn new(executable: PathBuf, label: impl Into<String>) -> Self {
        Self {
            executable,
            args: Vec::new(),
            current_dir: None,
            env: HashMap::new(),
            instance_id: generate_instance_id_pub(),
            label: label.into(),
            shutdown: ShutdownConfig::default(),
        }
    }
}

// ── OperationCompletion ───────────────────────────────────────────────────

/// 可重复读取最终结果的 completion 原语。
///
/// 使用 `watch::Sender<Option<T>>` 实现：
/// - 最终结果持久保存（存在 watch 的当前值中）
/// - 先完成、后订阅的 waiter 仍能立即读到结果
/// - 多个 waiter 都能获得同一结果
/// - 不依赖 Sender 尚未 drop（watch Sender drop 后 Receiver 仍可读最后一次值）
/// - 不存在丢通知（has_changed / borrow_and_mark_seen 可立即检测已完成）
///
/// `T` 必须 `Clone + Send + Sync`。
#[derive(Debug)]
struct OperationCompletion<T: Clone + Send + Sync> {
    tx: watch::Sender<Option<T>>,
    completed: std::sync::atomic::AtomicBool,
}

impl<T: Clone + Send + Sync + 'static> OperationCompletion<T> {
    fn new() -> (Self, watch::Receiver<Option<T>>) {
        let (tx, rx) = watch::channel(None);
        (
            Self {
                tx,
                completed: std::sync::atomic::AtomicBool::new(false),
            },
            rx,
        )
    }

    /// 完成操作。只允许完成一次。
    fn complete(&self, result: T) -> bool {
        if self
            .completed
            .compare_exchange(
                false,
                true,
                std::sync::atomic::Ordering::AcqRel,
                std::sync::atomic::Ordering::Acquire,
            )
            .is_err()
        {
            tracing::debug!("忽略 operation 的重复完成");
            return false;
        }
        let _ = self.tx.send(Some(result));
        true
    }

    /// 获取新的订阅者（后订阅者可立即读取已完成结果）。
    fn subscribe(&self) -> watch::Receiver<Option<T>> {
        self.tx.subscribe()
    }
}

// ── StartOperation ────────────────────────────────────────────────────────

/// start operation 的完成结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StartOutcome {
    /// start 成功，进程已进入 Running。
    Running { pid: u32 },
    /// spawn 失败。
    Failed { message: String },
    /// 启动被取消（stop 在 Starting 阶段到达，迟到 child 已回收）。
    Cancelled,
}

/// StartOperation — 与 InstanceToken 绑定的启动操作。
///
/// 在单次 inner lock 内原子创建，绑定到明确的 token。
/// 完成结果持久保存在 OperationCompletion 中，后订阅者可立即读取。
/// 旧 generation 不得完成新 generation 的 operation。
struct StartOperation {
    token: InstanceToken,
    completion: OperationCompletion<StartOutcome>,
}

impl StartOperation {
    fn new(token: InstanceToken) -> (Self, watch::Receiver<Option<StartOutcome>>) {
        let (completion, rx) = OperationCompletion::new();
        (Self { token, completion }, rx)
    }

    /// 完成此 operation。验证 token 仍匹配才完成。
    fn complete(&self, outcome: StartOutcome) {
        self.completion.complete(outcome);
    }

    /// 获取订阅者。
    fn subscribe(&self) -> watch::Receiver<Option<StartOutcome>> {
        self.completion.subscribe()
    }
}

// ── StopOperation ─────────────────────────────────────────────────────────

/// stop operation 的完成结果（可 Clone，供所有 waiter 共享）。
#[derive(Debug, Clone)]
pub enum StopOutcome {
    /// stop 成功完成，进程树已回收（退出原因经 ProcessStatus::Exited 事件传播）。
    Done,
    /// stop 失败。
    Failed { message: String },
}

/// StopOperation — 与 InstanceToken 绑定的停止操作。
///
/// 首个 stop 在单次 inner lock 内原子创建并标记为 executor。
/// 看到 Stopping 的后续 stop 只订阅已存在的 StopOperation。
struct StopOperation {
    completion: OperationCompletion<StopOutcome>,
}

impl StopOperation {
    fn new() -> (Self, watch::Receiver<Option<StopOutcome>>) {
        let (completion, rx) = OperationCompletion::new();
        (Self { completion }, rx)
    }

    /// 完成此 operation。
    fn complete(&self, outcome: StopOutcome) {
        self.completion.complete(outcome);
    }

    /// 获取订阅者。
    fn subscribe(&self) -> watch::Receiver<Option<StopOutcome>> {
        self.completion.subscribe()
    }
}

// ── StopPlan ──────────────────────────────────────────────────────────────

/// stop 路径的停止计划（锁内决策，锁外执行）。
enum StopPlan {
    /// 进程已停止，无需操作。
    AlreadyStopped,
    /// 已有 stop 在执行，等待其完成。
    /// 持有 StopOperation 的订阅者。
    WaitConcurrent {
        stop_rx: watch::Receiver<Option<StopOutcome>>,
    },
    /// stop 在 Starting 阶段到达，需取消启动并等待 start operation 完成。
    /// 当前调用是唯一 executor。
    /// `child` 是 Starting 阶段锁内 take 出的迟到 child（可能已 Running 提交，
    /// 也可能尚未提交——需在锁外根据 start outcome 决定回收策略）。
    CancelStart {
        child: Option<Child>,
        token: InstanceToken,
        force_timeout: Duration,
        start_rx: watch::Receiver<Option<StartOutcome>>,
        stop_op: Arc<StopOperation>,
    },
    /// stop 在 Running 阶段到达，需 kill child。
    /// 当前调用是唯一 executor。
    KillChild {
        child: Child,
        token: InstanceToken,
        force_timeout: Duration,
        stop_op: Arc<StopOperation>,
    },
    /// Running 状态但 child 缺失（内部不变量错误）。
    /// 尝试通过 Job Object 回收，返回结构化错误。
    RunningButNoChild {
        token: InstanceToken,
        stop_op: Arc<StopOperation>,
    },
}

// ── ManagedProcess ────────────────────────────────────────────────────────

/// ManagedProcess — 受管子进程生命周期句柄。
///
/// 幂等 start/stop，generation 隔离旧事件，有界日志管道。
/// `Arc<ManagedProcess>` 可安全跨 await 持有。
pub struct ManagedProcess {
    /// 内部状态（async 锁保护）。
    /// 临界区内禁止 await 其他锁或 await child。
    /// start_op 和 stop_op 也在 inner 中，保证与状态转换原子。
    inner: Mutex<ManagedProcessInner>,
    /// 日志管道（独立于状态锁，避免互锁）。
    log_pipe: Arc<LogPipe>,
    /// 日志配置（唯一真源）。
    log_config: LogPipeConfig,
    /// 状态变更通知（wait() 使用，避免轮询）。
    status_notify: watch::Sender<ProcessStatus>,
    /// 退出时的 kill 信号（同步原子，保证同步路径可靠）。
    shutdown_flag: Arc<std::sync::atomic::AtomicBool>,
    /// Job Object handle holder（同步锁，保证退出时必定关闭）。
    /// 连同 InstanceToken 保存，take 时验证 token。
    #[cfg(windows)]
    job_holder: std::sync::Mutex<
        Option<(
            InstanceToken,
            crate::infra::platform::process::JobObjectHandle,
        )>,
    >,
    /// 测试用 spawn gate（仅在 cfg(test) 时生效）。
    /// 默认为 None（不阻塞）。测试通过专用构造函数安装。
    /// 使用 watch::Sender<bool> 实现：false = 阻塞，true = 放行。
    #[cfg(test)]
    spawn_gate: Option<tokio::sync::watch::Sender<bool>>,
    /// 测试用提交 gate：子进程与 Job 已创建，但尚未向状态机发布 Running。
    #[cfg(test)]
    pre_running_commit_gate: Option<tokio::sync::watch::Sender<bool>>,
    #[cfg(test)]
    pre_running_commit_pid: std::sync::atomic::AtomicU32,
}

struct ManagedProcessInner {
    /// 状态快照（含 generation + token）。
    state: ManagedProcessState,
    /// tokio child handle（Running 时存在，stop/wait 时 take 出去）。
    child: Option<Child>,
    /// 当前 token 的 cancellation flag 引用。
    cancellation: Arc<std::sync::atomic::AtomicBool>,
    /// 当前实例的 force_stop_timeout。
    force_stop_timeout: Duration,
    /// 当前 start operation（Starting 时存在，Running/Exited 后保留直到下一 generation 替换）。
    start_op: Option<Arc<StartOperation>>,
    /// 当前 stop operation（Stopping 时存在，Exited 后保留直到下一 generation 替换）。
    stop_op: Option<Arc<StopOperation>>,
}

impl ManagedProcess {
    /// 创建新的 ManagedProcess（初始 Stopped 状态）。
    pub fn new(log_config: LogPipeConfig) -> Arc<Self> {
        let initial_status = ProcessStatus::Stopped;
        let (tx, _) = watch::channel(initial_status);
        Arc::new(Self {
            inner: Mutex::new(ManagedProcessInner {
                state: ManagedProcessState::initial(),
                child: None,
                cancellation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                force_stop_timeout: Duration::from_secs(10),
                start_op: None,
                stop_op: None,
            }),
            log_pipe: Arc::new(LogPipe::new(log_config.clone())),
            log_config,
            status_notify: tx,
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(windows)]
            job_holder: std::sync::Mutex::new(None),
            #[cfg(test)]
            spawn_gate: None,
            #[cfg(test)]
            pre_running_commit_gate: None,
            #[cfg(test)]
            pre_running_commit_pid: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// 创建带默认日志配置的 ManagedProcess。
    pub fn with_defaults() -> Arc<Self> {
        Self::new(LogPipeConfig::default())
    }

    /// 获取日志配置（只读，唯一真源）。
    #[allow(dead_code)] // 测试/诊断用配置读取
    pub fn log_config(&self) -> &LogPipeConfig {
        &self.log_config
    }

    // ── Job holder 辅助（同步锁，极短临界区）──────────────────────────────

    #[cfg(windows)]
    fn take_job_for_token(
        &self,
        token: &InstanceToken,
    ) -> Option<crate::infra::platform::process::JobObjectHandle> {
        let mut holder = self.job_holder.lock().unwrap();
        if let Some((job_token, _)) = holder.as_ref() {
            if job_token == token {
                holder.take().map(|(_, h)| h)
            } else {
                None
            }
        } else {
            None
        }
    }

    #[cfg(windows)]
    fn install_job(
        &self,
        token: &InstanceToken,
        handle: crate::infra::platform::process::JobObjectHandle,
    ) {
        let mut holder = self.job_holder.lock().unwrap();
        if let Some((old_token, _)) = holder.as_ref() {
            if old_token != token {
                tracing::warn!(
                    old_gen = old_token.generation,
                    new_gen = token.generation,
                    "旧 Job handle 仍存在，先关闭"
                );
                holder.take();
            }
        }
        *holder = Some((token.clone(), handle));
    }

    // ── start ──────────────────────────────────────────────────────────────

    /// 幂等 start：如果已在运行返回 AlreadyRunning，否则启动新进程。
    ///
    /// ## 原子性保证
    ///
    /// "检查允许启动 → 创建 token → 创建 StartOperation → 状态进入 Starting"
    /// 在单次 inner lock 内完成。重复 start 不创建、覆盖或完成当前 StartOperation。
    pub async fn start(self: &Arc<Self>, req: &LaunchRequest) -> Result<(), ManagedProcessError> {
        // Phase 1: 原子决策——在单次 inner lock 内完成状态检查 + token 创建 + StartOperation 创建
        let (token, _start_rx) = {
            let mut inner = self.inner.lock().await;
            match &inner.state.status {
                ProcessStatus::Running { .. }
                | ProcessStatus::Starting
                | ProcessStatus::Stopping => {
                    // 重复 start：不创建、不覆盖、不完成当前 operation
                    return Err(ManagedProcessError::AlreadyRunning {
                        generation: inner.state.generation(),
                    });
                }
                ProcessStatus::Exited { .. } | ProcessStatus::Stopped => {
                    // 清理上一代已完成的 operation
                    inner.start_op = None;
                    inner.stop_op = None;

                    // 创建新 token
                    let token = inner.state.begin_start();
                    inner.cancellation = inner.state.cancellation_flag();
                    inner.force_stop_timeout = req.shutdown.force_stop_timeout;

                    // 创建 StartOperation 绑定到此 token
                    let (start_op, start_rx) = StartOperation::new(token.clone());
                    inner.start_op = Some(Arc::new(start_op));

                    let _ = self.status_notify.send(ProcessStatus::Starting);
                    (token, start_rx)
                }
            }
        };

        tracing::info!(
            label = %req.label,
            instance_id = %token.instance_id,
            gen = token.generation,
            "ManagedProcess: starting"
        );

        // 测试 gate：仅当测试方安装了 spawn_gate 时才阻塞
        #[cfg(test)]
        {
            if let Some(ref gate_tx) = self.spawn_gate {
                let mut gate_rx = gate_tx.subscribe();
                if !*gate_rx.borrow() {
                    let _ = gate_rx.changed().await;
                }
            }
        }

        let spawn_result = spawn_child(req).await;

        match spawn_result {
            Ok(spawned) => {
                let SpawnedChild {
                    mut child,
                    stdout,
                    stderr,
                    pid,
                } = spawned;

                #[cfg(windows)]
                let job_handle = match crate::infra::platform::process::assign_job_object(pid) {
                    Ok(handle) => handle,
                    Err(e) => {
                        tracing::error!(%e, "Job Object 分配失败，终止子进程");
                        let kill_err = child.start_kill().err();
                        let wait_err = child.wait().await.err();
                        tracing::warn!(?kill_err, ?wait_err, "Job Object 失败后 child 回收");

                        let fail_msg = format!("Job Object 分配失败: {e}");
                        {
                            let mut inner = self.inner.lock().await;
                            if inner.state.is_current(&token) {
                                inner.state.set_status_exited(ExitReason::StartFailed {
                                    message: fail_msg.clone(),
                                });
                                let _ = self.status_notify.send(ProcessStatus::Exited {
                                    reason: ExitReason::StartFailed {
                                        message: fail_msg.clone(),
                                    },
                                });
                            }
                            // 完成属于自己的 StartOperation
                            if let Some(ref op) = inner.start_op {
                                if op.token == token {
                                    op.complete(StartOutcome::Failed {
                                        message: fail_msg.clone(),
                                    });
                                }
                            }
                        }
                        return Err(ManagedProcessError::JobObjectFailed { message: e });
                    }
                };

                // 查询真实 OS creation time（fail-closed：失败返回 0，kill_process_tree_verified 会拒绝）
                let creation_time = get_os_creation_time_ms(pid);

                let identity = ProcessIdentity {
                    pid,
                    executable: req.executable.clone(),
                    start_time_ms: creation_time,
                    instance_id: token.instance_id.clone(),
                };

                // Job 必须先按 token 安装，再公开 Running。这样任何观察到 Running
                // 的 stop 都能在同一时刻取得 child，并能通过 Job 回收进程树。
                #[cfg(windows)]
                self.install_job(&token, job_handle);

                #[cfg(test)]
                {
                    self.pre_running_commit_pid
                        .store(pid, std::sync::atomic::Ordering::Release);
                    if let Some(ref gate_tx) = self.pre_running_commit_gate {
                        let mut gate_rx = gate_tx.subscribe();
                        if !*gate_rx.borrow() {
                            let _ = gate_rx.changed().await;
                        }
                    }
                }

                let mut child_slot = Some(child);
                let commit_result = {
                    let mut inner = self.inner.lock().await;
                    let result = inner.state.try_commit_running(&token, pid, identity);
                    if result == CommitResult::Committed {
                        // Running 状态与 child 所有权在同一临界区内发布。
                        inner.child = child_slot.take();
                        let _ = self.status_notify.send(ProcessStatus::Running { pid });
                        if let Some(ref op) = inner.start_op {
                            if op.token == token {
                                op.complete(StartOutcome::Running { pid });
                            }
                        }
                    }
                    result
                };

                if commit_result.needs_reclaim() {
                    tracing::warn!(gen = token.generation, ?commit_result, "Running 提交失败，回收 child");
                    let mut child = child_slot
                        .take()
                        .expect("未提交 Running 时 child 必须仍由 start 持有");
                    let kill_err = child.start_kill().err();
                    let wait_result =
                        tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                    tracing::warn!(?kill_err, ?wait_result, "提交失败后 child 回收完成");
                    drop(child);

                    #[cfg(windows)]
                    {
                        if let Some(h) = self.take_job_for_token(&token) {
                            drop(h);
                        }
                    }

                    let outcome = if commit_result == CommitResult::Cancelled {
                        StartOutcome::Cancelled
                    } else {
                        StartOutcome::Failed {
                            message: "Running 提交被拒绝".to_string(),
                        }
                    };

                    {
                        let mut inner = self.inner.lock().await;
                        if inner.state.is_current(&token) {
                            let reason = if commit_result == CommitResult::Cancelled {
                                ExitReason::StartCancelled
                            } else {
                                ExitReason::StartFailed {
                                    message: "Running 提交被拒绝".to_string(),
                                }
                            };
                            inner.state.set_status_exited(reason.clone());
                            let _ = self.status_notify.send(ProcessStatus::Exited { reason });
                        }
                        // 完成属于自己的 StartOperation
                        if let Some(ref op) = inner.start_op {
                            if op.token == token {
                                op.complete(outcome);
                            }
                        }
                    }
                    return Ok(());
                }

                // 启动 pump（使用 ManagedProcess 的 log_config，唯一真源）
                let max_bytes = self.log_config.max_line_bytes;
                if let Some(stdout) = stdout {
                    let lp = self.log_pipe.clone();
                    tokio::spawn(async move {
                        pump_lines(stdout, LogSource::Stdout, &lp, max_bytes).await;
                    });
                }
                if let Some(stderr) = stderr {
                    let lp = self.log_pipe.clone();
                    tokio::spawn(async move {
                        pump_lines(stderr, LogSource::Stderr, &lp, max_bytes).await;
                    });
                }

                // 启动 wait task
                let inner_ref = Arc::clone(self);
                let token_clone = token.clone();
                tokio::spawn(async move {
                    inner_ref.wait_and_update(token_clone, pid).await;
                });

                tracing::info!(label = %req.label, pid, gen = token.generation, "ManagedProcess: started");
                Ok(())
            }
            Err(e) => {
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner
                            .state
                            .set_status_exited(ExitReason::StartFailed { message: e.clone() });
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: ExitReason::StartFailed { message: e.clone() },
                        });
                    }
                    // 完成属于自己的 StartOperation
                    if let Some(ref op) = inner.start_op {
                        if op.token == token {
                            op.complete(StartOutcome::Failed { message: e.clone() });
                        }
                    }
                }
                Err(ManagedProcessError::SpawnFailed { message: e })
            }
        }
    }

    // ── stop ───────────────────────────────────────────────────────────────

    /// 幂等 stop：如果未运行返回 Ok，否则发起停止。
    ///
    /// 多个并发 stop 共享同一个停止结果。
    pub async fn stop(&self) -> Result<(), ManagedProcessError> {
        self.stop_impl(None).await
    }

    /// 条件停止：只停止指定 token 的实例。
    pub async fn stop_if_current(&self, token: &InstanceToken) -> Result<(), ManagedProcessError> {
        self.stop_impl(Some(token)).await
    }

    async fn stop_impl(
        &self,
        expected_token: Option<&InstanceToken>,
    ) -> Result<(), ManagedProcessError> {
        // Phase 1: 原子决策——在单次 inner lock 内完成
        // 状态检查 + token 验证 + StopOperation 创建 + Stopping 状态转换
        let plan = {
            let mut inner = self.inner.lock().await;

            if let Some(tok) = expected_token {
                if !inner.state.is_current(tok) {
                    tracing::debug!(
                        expected_gen = tok.generation,
                        current_gen = inner.state.generation(),
                        "stop_if_current: token 不匹配，跳过"
                    );
                    return Ok(());
                }
            }

            match &inner.state.status {
                ProcessStatus::Stopped | ProcessStatus::Exited { .. } => StopPlan::AlreadyStopped,

                ProcessStatus::Stopping => {
                    // 已有 stop 在执行——只能订阅已存在的 StopOperation
                    // 绝不创建新 operation，绝不成为 executor
                    if let Some(ref stop_op) = inner.stop_op {
                        let stop_rx = stop_op.subscribe();
                        StopPlan::WaitConcurrent { stop_rx }
                    } else {
                        // 状态为 Stopping 但 stop_op 缺失——内部不变量被破坏
                        tracing::error!(
                            gen = inner.state.generation(),
                            "stop: Stopping 状态但 stop_op 缺失（内部不变量错误）"
                        );
                        return Err(ManagedProcessError::InternalInconsistency {
                            message: "Stopping 状态但 stop_op 缺失".to_string(),
                        });
                    }
                }

                ProcessStatus::Starting => {
                    // stop 在 Starting 阶段到达——取消启动
                    inner.state.mark_cancelled();

                    let token = inner.state.token.clone();
                    let force_timeout = inner.force_stop_timeout;

                    // 原子创建 StopOperation 并成为 executor
                    let (stop_op, _stop_rx) = StopOperation::new();
                    let stop_op = Arc::new(stop_op);
                    inner.stop_op = Some(Arc::clone(&stop_op));

                    // 状态改为 Stopping
                    inner.state.set_status_stopping();
                    let _ = self.status_notify.send(ProcessStatus::Stopping);

                    // 获取 StartOperation 的订阅者（用于等待 start 完成）
                    let start_rx = if let Some(ref start_op) = inner.start_op {
                        if start_op.token == token {
                            start_op.subscribe()
                        } else {
                            // start_op token 不匹配——不应该发生
                            tracing::error!(
                                gen = token.generation,
                                "stop: start_op token 不匹配（内部不变量错误）"
                            );
                            return Err(ManagedProcessError::InternalInconsistency {
                                message: "start_op token 不匹配".to_string(),
                            });
                        }
                    } else {
                        // start_op 缺失——不应该发生（Starting 状态必须有 start_op）
                        tracing::error!(
                            gen = token.generation,
                            "stop: Starting 状态但 start_op 缺失（内部不变量错误）"
                        );
                        return Err(ManagedProcessError::InternalInconsistency {
                            message: "Starting 状态但 start_op 缺失".to_string(),
                        });
                    };

                    // 取出 child（如果有迟到 child）
                    let child = inner.child.take();

                    StopPlan::CancelStart {
                        child,
                        token,
                        force_timeout,
                        start_rx,
                        stop_op,
                    }
                }

                ProcessStatus::Running { .. } => {
                    let token = inner.state.token.clone();
                    let force_timeout = inner.force_stop_timeout;

                    // 原子创建 StopOperation 并成为 executor
                    let (stop_op, _stop_rx) = StopOperation::new();
                    let stop_op = Arc::new(stop_op);
                    inner.stop_op = Some(Arc::clone(&stop_op));

                    // 状态改为 Stopping
                    inner.state.set_status_stopping();
                    let _ = self.status_notify.send(ProcessStatus::Stopping);

                    // 取出 child
                    let child = inner.child.take();

                    if let Some(child) = child {
                        StopPlan::KillChild {
                            child,
                            token,
                            force_timeout,
                            stop_op,
                        }
                    } else {
                        // Running 状态但 child 缺失——内部不变量错误
                        tracing::error!(
                            gen = token.generation,
                            "stop: Running 状态但 child 缺失（内部不变量错误）"
                        );
                        StopPlan::RunningButNoChild { token, stop_op }
                    }
                }
            }
        };

        match plan {
            StopPlan::AlreadyStopped => Ok(()),

            StopPlan::WaitConcurrent { mut stop_rx } => {
                // 等待已有 stop operation 完成
                if stop_rx.borrow().is_none() {
                    let _ = stop_rx.changed().await;
                }
                // 读取最终结果
                match stop_rx.borrow().clone() {
                    Some(StopOutcome::Done) => Ok(()),
                    Some(StopOutcome::Failed { message }) => {
                        Err(ManagedProcessError::StopFailed { message })
                    }
                    None => {
                        // 不应该发生——completion 已完成但结果为 None
                        tracing::error!("stop waiter: completion 返回 None（内部不变量错误）");
                        Err(ManagedProcessError::InternalInconsistency {
                            message: "stop completion 返回 None".to_string(),
                        })
                    }
                }
            }

            StopPlan::CancelStart {
                child,
                token,
                force_timeout,
                mut start_rx,
                stop_op,
            } => {
                // 当前调用是唯一 executor

                // 等待 StartOperation 完成——不超时。
                // start 的 spawn 要么成功要么失败，最终必定完成 StartOperation。
                // 超时后假装成功会破坏 stop postcondition（进程树可能仍存活）。
                if start_rx.borrow().is_none() {
                    let _ = start_rx.changed().await;
                }

                // 根据 start outcome 决定回收策略
                let start_outcome = start_rx.borrow().clone();
                match start_outcome {
                    Some(StartOutcome::Running { pid }) => {
                        // start 成功了但被取消——需要 kill child
                        // start 已经存储了 child，我们需要从 inner 取出
                        tracing::info!(pid, gen = token.generation, "stop: start 完成但已取消，kill child");
                        let child_to_kill = {
                            let mut inner = self.inner.lock().await;
                            inner.child.take()
                        };
                        if let Some(mut child) = child_to_kill {
                            let _ = child.start_kill();
                            let wait_result =
                                tokio::time::timeout(force_timeout, child.wait()).await;
                            if wait_result.is_err() {
                                tracing::warn!(pid, gen = token.generation, "stop: cancel-start child kill 超时");
                            }
                        }
                    }
                    Some(StartOutcome::Cancelled) => {
                        // start 已自行回收 child——无需再次回收
                        tracing::info!(gen = token.generation, "stop: start 已取消并回收 child");
                    }
                    Some(StartOutcome::Failed { .. }) => {
                        // start 失败——child 不存在或已被回收
                        tracing::info!(gen = token.generation, "stop: start 已失败，无需回收 child");
                    }
                    None => {
                        // start operation 未完成——不应该发生（我们已等待 changed）
                        // 但作为防御，如果有迟到 child 则回收
                        tracing::warn!(gen = token.generation, "stop: start operation 未完成结果");
                    }
                }

                // 回收可能在锁外产生的迟到 child（如果 start 在 CancelStart 后才 spawn 成功）
                if let Some(mut late_child) = child {
                    let pid = late_child.id().unwrap_or(0);
                    tracing::info!(pid, gen = token.generation, "stop: 回收 Starting 阶段迟到 child");
                    let _ = late_child.start_kill();
                    let wait_result = tokio::time::timeout(force_timeout, late_child.wait()).await;
                    if wait_result.is_err() {
                        tracing::warn!(pid, gen = token.generation, "stop: 迟到 child kill 超时");
                    }
                }

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        tracing::info!(gen = token.generation, "stop: drop Job handle");
                        drop(h);
                    }
                }

                let exit_reason = ExitReason::StartCancelled;
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(exit_reason.clone());
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: exit_reason.clone(),
                        });
                    }
                }

                // 完成 StopOperation
                stop_op.complete(StopOutcome::Done);
                Ok(())
            }

            StopPlan::KillChild {
                mut child,
                token,
                force_timeout,
                stop_op,
            } => {
                // 当前调用是唯一 executor
                let pid = child.id().unwrap_or(0);
                tracing::info!(pid, gen = token.generation, "ManagedProcess: stop");

                let kill_err = child.start_kill().err();
                if let Some(e) = &kill_err {
                    tracing::warn!(%e, pid, "child.start_kill 返回错误");
                }

                let timeout_result = tokio::time::timeout(force_timeout, child.wait()).await;

                let (exit_reason, stop_outcome) = match timeout_result {
                    Ok(Ok(status)) => {
                        let code = status.code();
                        let reason = ExitReason::Stopped { code };
                        tracing::info!(pid, gen = token.generation, "ManagedProcess: stopped (force kill)");
                        (reason, StopOutcome::Done)
                    }
                    Ok(Err(e)) => {
                        let reason = ExitReason::WaitError {
                            message: format!("child wait 错误: {e}"),
                        };
                        tracing::error!(%e, pid, gen = token.generation, "child.wait 返回错误");
                        (
                            reason.clone(),
                            StopOutcome::Failed {
                                message: format!("child wait 错误: {e}"),
                            },
                        )
                    }
                    Err(_) => {
                        tracing::warn!(pid, gen = token.generation, "ManagedProcess: force_stop 超时，强制回收");

                        #[cfg(windows)]
                        {
                            if let Some(h) = self.take_job_for_token(&token) {
                                tracing::info!(pid, gen = token.generation, "Job handle drop (KILL_ON_JOB_CLOSE)");
                                drop(h);
                            }
                        }

                        // 非 Windows：超时后无法依赖 Job Object，执行有限 deadline 的最终 wait
                        #[cfg(not(windows))]
                        {
                            let final_wait =
                                tokio::time::timeout(Duration::from_secs(3), child.wait()).await;
                            if let Ok(Ok(status)) = final_wait {
                                let code = status.code();
                                let reason = ExitReason::ForceKilled {
                                    deadline_exceeded: true,
                                };
                                let _ = self.status_notify.send(ProcessStatus::Exited {
                                    reason: reason.clone(),
                                });
                                let mut inner = self.inner.lock().await;
                                if inner.state.is_current(&token) {
                                    inner.state.set_status_exited(reason.clone());
                                }
                                stop_op.complete(StopOutcome::Done);
                                return Ok(());
                            }
                        }

                        // Windows：Job Object drop 后 child 应已退出，但仍然 wait 确认
                        let final_wait = child.wait().await;
                        let reason = ExitReason::ForceKilled {
                            deadline_exceeded: true,
                        };
                        tracing::info!(pid, gen = token.generation, ?final_wait, "Job Object 回收后 child 退出");
                        (reason, StopOutcome::Done)
                    }
                };

                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(exit_reason.clone());
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: exit_reason,
                        });
                    }
                }

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        drop(h);
                    }
                }

                // 完成 StopOperation
                stop_op.complete(stop_outcome);
                Ok(())
            }

            StopPlan::RunningButNoChild { token, stop_op } => {
                // Running 状态但 child 缺失——内部不变量错误
                // 尝试通过 Job Object 回收，返回结构化错误
                tracing::error!(gen = token.generation, "stop: RunningButNoChild——尝试 Job Object 回收");

                #[cfg(windows)]
                {
                    if let Some(h) = self.take_job_for_token(&token) {
                        tracing::info!(gen = token.generation, "RunningButNoChild: drop Job handle");
                        drop(h);
                    }
                }

                let fail_msg = "Running 状态但 child 缺失（内部不变量错误）".to_string();
                {
                    let mut inner = self.inner.lock().await;
                    if inner.state.is_current(&token) {
                        inner.state.set_status_exited(ExitReason::WaitError {
                            message: fail_msg.clone(),
                        });
                        let _ = self.status_notify.send(ProcessStatus::Exited {
                            reason: ExitReason::WaitError {
                                message: fail_msg.clone(),
                            },
                        });
                    }
                }

                stop_op.complete(StopOutcome::Failed {
                    message: fail_msg.clone(),
                });
                Err(ManagedProcessError::InternalInconsistency { message: fail_msg })
            }
        }
    }

    // ── wait / snapshot / 公共 API ──────────────────────────────────────────

    /// 等待进程退出（async）。如果进程已退出或未运行，立即返回。
    ///
    /// 使用 watch channel 而非固定轮询，避免超时伪装成功。
    #[allow(dead_code)] // 测试断言进程终态用
    pub async fn wait(&self) -> Result<ProcessStatus, ManagedProcessError> {
        {
            let inner = self.inner.lock().await;
            if inner.state.status.is_exited() || inner.state.status == ProcessStatus::Stopped {
                return Ok(inner.state.status.clone());
            }
        }

        let mut rx = self.status_notify.subscribe();
        loop {
            {
                let inner = self.inner.lock().await;
                if inner.state.status.is_exited() || inner.state.status == ProcessStatus::Stopped {
                    return Ok(inner.state.status.clone());
                }
            }
            if rx.changed().await.is_err() {
                let inner = self.inner.lock().await;
                return Ok(inner.state.status.clone());
            }
            let status = rx.borrow().clone();
            if status.is_exited() || status == ProcessStatus::Stopped {
                return Ok(status);
            }
        }
    }

    /// 获取当前状态快照（只读）。
    pub async fn snapshot(&self) -> ManagedProcessState {
        let inner = self.inner.lock().await;
        inner.state.clone()
    }

    /// 获取当前 token。
    pub async fn current_token(&self) -> InstanceToken {
        let inner = self.inner.lock().await;
        inner.state.token.clone()
    }

    /// 检查指定 token 是否为当前实例（条件停止辅助）。
    pub async fn is_current_token(&self, token: &InstanceToken) -> bool {
        let inner = self.inner.lock().await;
        inner.state.is_current(token)
    }

    /// 获取 PID（如果运行中）。
    pub async fn pid(&self) -> Option<u32> {
        let inner = self.inner.lock().await;
        match &inner.state.status {
            ProcessStatus::Running { pid } => Some(*pid),
            _ => None,
        }
    }

    /// 获取日志历史。
    pub async fn log_history(&self) -> Vec<LogEntry> {
        self.log_pipe.history().await
    }

    /// 订阅实时日志流。
    pub fn subscribe_logs(&self) -> LogSubscriber {
        self.log_pipe.subscribe()
    }

    /// 获取截断行计数。
    #[allow(dead_code)] // 测试断言日志洪泛截断用
    pub fn log_truncated_count(&self) -> u64 {
        self.log_pipe.truncated_line_count()
    }

    /// 订阅进程状态变更通知（0.22.6.3）。
    ///
    /// 返回 `watch::Receiver<ProcessStatus>`，调用方可以：
    /// - `rx.borrow().clone()` 获取当前状态快照
    /// - `rx.changed().await` 等待状态变更
    ///
    /// `EngineManager` 使用此方法监听进程意外退出，
    /// 在 server crash 后收敛 `EngineStatus` 到 Exited/Unreachable。
    pub fn subscribe_status(&self) -> watch::Receiver<ProcessStatus> {
        self.status_notify.subscribe()
    }

    // ── shutdown_blocking ──────────────────────────────────────────────────

    /// 应用退出时的同步 kill-on-close 路径。
    ///
    /// 不依赖 async mutex（退出路径可能在非 async 上下文调用）。
    /// 通过 shutdown_flag + Job Object CloseHandle 确保可靠回收。
    pub fn shutdown_blocking(&self) {
        self.shutdown_flag
            .store(true, std::sync::atomic::Ordering::Release);

        if let Ok(mut guard) = self.inner.try_lock() {
            if let Some(child) = guard.child.as_mut() {
                let _ = child.start_kill();
                tracing::info!(
                    gen = guard.state.generation(),
                    "ManagedProcess: shutdown_blocking kill sent"
                );
            }
            guard
                .state
                .set_status_exited(ExitReason::Stopped { code: None });
            let _ = self.status_notify.send(ProcessStatus::Exited {
                reason: ExitReason::Stopped { code: None },
            });
        } else {
            tracing::warn!(
                "ManagedProcess: shutdown_blocking 无法获取锁（可能正在 stop），依赖 Job Object 回收"
            );
        }

        #[cfg(windows)]
        {
            let mut holder = self.job_holder.lock().unwrap();
            if holder.take().is_some() {
                tracing::info!("ManagedProcess: shutdown_blocking Job handle dropped");
            }
        }
    }

    // ── wait_and_update ────────────────────────────────────────────────────

    /// 内部：wait task 在子进程退出后更新状态。
    ///
    /// child wait 所有权唯一：此 task 通过 `try_wait` 轮询。
    /// 如果 stop 取走了 child，此 task 退出。
    async fn wait_and_update(self: Arc<Self>, token: InstanceToken, pid: u32) {
        loop {
            if self
                .shutdown_flag
                .load(std::sync::atomic::Ordering::Acquire)
            {
                tracing::debug!(pid, gen = token.generation, "wait task: shutdown flag set, exiting");
                return;
            }

            let exit_status = {
                let mut inner = self.inner.lock().await;
                if !inner.state.is_current(&token) {
                    tracing::debug!(
                        gen = token.generation,
                        current = inner.state.generation(),
                        "wait task: token 过期，退出"
                    );
                    return;
                }
                if inner.child.is_none() {
                    return;
                }
                let child = inner.child.as_mut().unwrap();
                child.try_wait().ok().flatten()
            };

            match exit_status {
                Some(status) => {
                    let reason = if status.success() {
                        ExitReason::NormalExit {
                            code: status.code().unwrap_or(0),
                        }
                    } else {
                        ExitReason::NonZeroExit {
                            code: status.code().unwrap_or(-1),
                        }
                    };

                    let mut inner = self.inner.lock().await;
                    if !inner.state.is_current(&token) {
                        tracing::debug!(
                            gen = token.generation,
                            "wait task: token 过期，不更新状态"
                        );
                        return;
                    }

                    inner.state.try_commit_exit(&token, reason.clone());
                    inner.child = None;

                    #[cfg(windows)]
                    {
                        if let Some(h) = self.take_job_for_token(&token) {
                            drop(h);
                        }
                    }

                    let _ = self.status_notify.send(ProcessStatus::Exited {
                        reason: reason.clone(),
                    });

                    tracing::info!(
                        pid,
                        gen = token.generation,
                        reason = ?reason,
                        "ManagedProcess: process exited"
                    );
                    return;
                }
                None => {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                }
            }
        }
    }

    /// 测试辅助：释放 spawn gate，允许 start 继续 spawn。
    #[cfg(test)]
    pub fn release_spawn_gate(&self) {
        if let Some(ref gate_tx) = self.spawn_gate {
            let _ = gate_tx.send(true);
        }
    }

    /// 测试辅助：释放 Running 原子提交前的 gate。
    #[cfg(test)]
    pub fn release_pre_running_commit_gate(&self) {
        if let Some(ref gate_tx) = self.pre_running_commit_gate {
            let _ = gate_tx.send(true);
        }
    }

    /// 测试辅助：获取已 spawn、尚未发布 Running 的 PID。
    #[cfg(test)]
    pub fn pre_running_commit_pid_for_test(&self) -> u32 {
        self.pre_running_commit_pid
            .load(std::sync::atomic::Ordering::Acquire)
    }

    /// 测试专用构造：创建带 spawn gate 的 ManagedProcess。
    /// start 在 spawn 前会 await gate，需调用 release_spawn_gate() 放行。
    #[cfg(test)]
    pub fn with_spawn_gate_for_test() -> Arc<Self> {
        let initial_status = ProcessStatus::Stopped;
        let (tx, _) = watch::channel(initial_status);
        let (gate_tx, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            inner: Mutex::new(ManagedProcessInner {
                state: ManagedProcessState::initial(),
                child: None,
                cancellation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                force_stop_timeout: Duration::from_secs(10),
                start_op: None,
                stop_op: None,
            }),
            log_pipe: Arc::new(LogPipe::new(LogPipeConfig::default())),
            log_config: LogPipeConfig::default(),
            status_notify: tx,
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(windows)]
            job_holder: std::sync::Mutex::new(None),
            #[cfg(test)]
            spawn_gate: Some(gate_tx),
            #[cfg(test)]
            pre_running_commit_gate: None,
            #[cfg(test)]
            pre_running_commit_pid: std::sync::atomic::AtomicU32::new(0),
        })
    }

    /// 测试专用构造：在 child/Job 创建后、Running+child 原子提交前暂停。
    #[cfg(test)]
    pub fn with_pre_running_commit_gate_for_test() -> Arc<Self> {
        let initial_status = ProcessStatus::Stopped;
        let (tx, _) = watch::channel(initial_status);
        let (gate_tx, _) = tokio::sync::watch::channel(false);
        Arc::new(Self {
            inner: Mutex::new(ManagedProcessInner {
                state: ManagedProcessState::initial(),
                child: None,
                cancellation: Arc::new(std::sync::atomic::AtomicBool::new(false)),
                force_stop_timeout: Duration::from_secs(10),
                start_op: None,
                stop_op: None,
            }),
            log_pipe: Arc::new(LogPipe::new(LogPipeConfig::default())),
            log_config: LogPipeConfig::default(),
            status_notify: tx,
            shutdown_flag: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            #[cfg(windows)]
            job_holder: std::sync::Mutex::new(None),
            spawn_gate: None,
            pre_running_commit_gate: Some(gate_tx),
            pre_running_commit_pid: std::sync::atomic::AtomicU32::new(0),
        })
    }
}

// ── spawn 辅助 ───────────────────────────────────────────────────────────

struct SpawnedChild {
    child: Child,
    stdout: Option<tokio::process::ChildStdout>,
    stderr: Option<tokio::process::ChildStderr>,
    pid: u32,
}

async fn spawn_child(req: &LaunchRequest) -> Result<SpawnedChild, String> {
    let mut cmd =
        crate::infra::platform::no_window_tokio(tokio::process::Command::new(&req.executable));

    cmd.args(&req.args);

    if let Some(ref dir) = req.current_dir {
        cmd.current_dir(dir);
    }

    for (k, v) in &req.env {
        cmd.env(k, v);
    }

    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::null());

    let mut child = cmd.spawn().map_err(|e| format!("spawn 失败: {e}"))?;

    let pid = child.id().unwrap_or(0);

    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(SpawnedChild {
        child,
        stdout,
        stderr,
        pid,
    })
}

/// 排空子进程管道，转发到 LogPipe。
async fn pump_lines<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    source: LogSource,
    log_pipe: &LogPipe,
    max_line_bytes: usize,
) {
    use tokio::io::AsyncReadExt;

    let mut reader = reader;
    let mut acc = LineAccumulator::new(max_line_bytes);
    let mut read_buf = vec![0u8; 8192];

    loop {
        match reader.read(&mut read_buf).await {
            Ok(0) => {
                if let Some((text, truncated)) = acc.finish() {
                    log_pipe.append(source, text, truncated).await;
                }
                break;
            }
            Ok(n) => {
                let lines = acc.push_data(&read_buf[..n]);
                for (text, truncated) in lines {
                    log_pipe.append(source, text, truncated).await;
                }
            }
            Err(e) => {
                tracing::debug!(%e, ?source, "pipe read error");
                if let Some((text, truncated)) = acc.finish() {
                    log_pipe.append(source, text, truncated).await;
                }
                break;
            }
        }
    }
}

/// 获取 OS 真实进程创建时间（Unix 毫秒）。
/// 失败返回 0（fail-closed：kill_process_tree_verified 会拒绝终止）。
#[cfg(windows)]
fn get_os_creation_time_ms(pid: u32) -> u64 {
    crate::infra::platform::process::get_process_creation_time_ms(pid).unwrap_or(0)
}

#[cfg(not(windows))]
fn get_os_creation_time_ms(_pid: u32) -> u64 {
    0
}

#[cfg(test)]
mod operation_completion_tests {
    use super::{OperationCompletion, StopOutcome};

    #[test]
    fn completion_is_once_only_and_late_subscriber_reads_first_result() {
        let (completion, _initial_rx) = OperationCompletion::new();
        assert!(completion.complete("first".to_string()));
        assert!(!completion.complete("second".to_string()));

        let late_rx = completion.subscribe();
        assert_eq!(late_rx.borrow().as_deref(), Some("first"));
    }

    #[test]
    fn failed_stop_outcome_is_persistent_for_all_waiters() {
        let (completion, mut first_rx) = OperationCompletion::new();
        let second_rx = completion.subscribe();
        assert!(completion.complete(StopOutcome::Failed {
            message: "forced failure".to_string(),
        }));

        let first = first_rx.borrow_and_update().clone();
        let second = second_rx.borrow().clone();
        assert!(matches!(
            first,
            Some(StopOutcome::Failed { ref message }) if message == "forced failure"
        ));
        assert!(matches!(
            second,
            Some(StopOutcome::Failed { ref message }) if message == "forced failure"
        ));

        let _ = StopOutcome::Done;
    }
}

/// 生成随机 instance_id（公开接口，供 LaunchRequest 构造方/测试调用）。
#[allow(dead_code)] // 测试用便捷入口；链条下游由本项激活
pub fn generate_instance_id_pub() -> String {
    generate_instance_id()
}

/// 生成随机 instance_id。
fn generate_instance_id() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("inst-{now:016x}-{:04x}", rand_word())
}

/// 简单伪随机（不引入 rand crate）。
fn rand_word() -> u16 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
}
