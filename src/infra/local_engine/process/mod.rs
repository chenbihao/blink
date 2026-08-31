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
    /// stdio 管道模式（0.22.7：NDJSON 常驻 worker 用）。
    ///
    /// 默认 `Default`（stdin=null、stdout 进 LogPipe）保持既有行为；
    /// 双向协议 worker 需要 `stdin_piped + stdout_handoff`：
    /// - stdin pipe 保留在 `ManagedProcess`，由调用方 `take_worker_stdio` 取走；
    /// - stdout 不再泵入 LogPipe（协议通道），同样交给调用方；
    /// - stderr 始终泵入 LogPipe（worker 诊断只写 stderr）。
    pub stdio: StdioConfig,
}

/// 子进程 stdio 管道配置（默认全部关闭，维持既有行为）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StdioConfig {
    /// stdin 使用 pipe 并保留供调用方写入（默认 null）。
    pub stdin_piped: bool,
    /// stdout 交给调用方接管（不进 LogPipe）。要求 `stdin_piped` 同开。
    pub stdout_handoff: bool,
}

impl StdioConfig {
    /// 双向 NDJSON worker 模式。
    pub fn worker_protocol() -> Self {
        Self {
            stdin_piped: true,
            stdout_handoff: true,
        }
    }
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
            stdio: StdioConfig::default(),
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
    /// 双向协议 worker 的 stdio 句柄（start 后由调用方一次性取走）。
    /// None = 未启用 worker 模式或已被取走。
    /// 调用方持有 stdin 意味着：正常停止由调用方先 drop（EOF）触发 worker 自行退出。
    worker_stdio: Option<WorkerStdio>,
}

/// 双向协议 worker 的 stdio 句柄（0.22.7 NDJSON worker）。
pub struct WorkerStdio {
    pub stdin: tokio::process::ChildStdin,
    pub stdout: tokio::process::ChildStdout,
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
                worker_stdio: None,
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
        if let Some((old_token, _)) = holder.as_ref()
            && old_token != token
        {
            tracing::warn!(
                old_gen = old_token.generation,
                new_gen = token.generation,
                "旧 Job handle 仍存在，先关闭"
            );
            holder.take();
        }
        *holder = Some((token.clone(), handle));
    }
}

mod monitor;
mod start;
mod stop;

// ── test gates (cfg(test) only) ─────────────────────────────────────────

impl ManagedProcess {
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
                worker_stdio: None,
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
                worker_stdio: None,
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
    stdin: Option<tokio::process::ChildStdin>,
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
    if req.stdio.stdin_piped {
        cmd.stdin(std::process::Stdio::piped());
    } else {
        cmd.stdin(std::process::Stdio::null());
    }

    let mut child = cmd.spawn().map_err(|e| format!("spawn 失败: {e}"))?;

    let pid = child.id().unwrap_or(0);

    let stdin = if req.stdio.stdin_piped {
        child.stdin.take()
    } else {
        // 未启用 pipe 时 tokio 不会创建 stdin 句柄
        None
    };
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    Ok(SpawnedChild {
        child,
        stdin,
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
mod tests;

/// 生成随机 instance_id（公开接口，供 LaunchRequest 构造方/测试调用）。
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
