//! ManagedProcess 状态模型（0.22.1）。
//!
//! 三维观测中的 process 维度，不做完整 EngineStatus。
//! 状态更新必须带 generation + instance_token 防旧任务覆盖。
//!
//! ## 并发安全设计
//!
//! - 每次 start 生成唯一 `InstanceToken`（generation + instance_id）。
//! - Running 提交必须验证：token 匹配 + 当前状态仍为 Starting + cancellation 未触发。
//! - stop 在 Starting 阶段设置 `cancelled` 标志，使迟到的 spawn 结果无法提交 Running。
//! - `stop_if_current(token)` 原子条件停止：只停止指定 token 的实例。
//! - 旧 generation 的退出事件不能覆盖新 generation。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// 进程状态快照（只读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessStatus {
    /// 进程未启动或已完全退出。
    Stopped,
    /// 正在启动（spawn 已发起，尚未确认 Running）。
    Starting,
    /// 进程运行中，附带 PID。
    Running { pid: u32 },
    /// 正在停止（graceful kill 已发起，等待退出或超时强杀）。
    Stopping,
    /// 进程已退出，附带退出原因。
    Exited { reason: ExitReason },
}

impl ProcessStatus {
    /// 是否处于活跃状态（Starting / Running / Stopping）。
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            ProcessStatus::Starting | ProcessStatus::Running { .. } | ProcessStatus::Stopping
        )
    }

    /// 是否已退出。
    pub fn is_exited(&self) -> bool {
        matches!(self, ProcessStatus::Exited { .. })
    }
}

/// 进程退出原因（结构化区分）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExitReason {
    /// 正常退出（exit code == 0）。
    NormalExit { code: i32 },
    /// 用户/上层主动调用 stop 后进程退出。
    Stopped { code: Option<i32> },
    /// 启动被取消（stop 在 Starting 阶段到达，迟到 child 被回收）。
    StartCancelled,
    /// 启动失败（spawn 或初始化失败）。
    StartFailed { message: String },
    /// 非零退出码（崩溃或异常退出）。
    NonZeroExit { code: i32 },
    /// stop 超时后强制终止。
    ForceKilled { deadline_exceeded: bool },
    /// 等待/IO 错误。
    WaitError { message: String },
}

impl ExitReason {
    /// 是否为主动停止导致的退出（非崩溃）。
    ///
    /// 用户 stop、切模重启、OCR idle TTL、应用退出均属于 deliberate stop，
    /// 不得被上层投影成"进程意外退出"。
    /// `Stopped`（stop 路径 force kill 后回收）、`StartCancelled`（Starting 阶段被取消）
    /// 和 `ForceKilled`（stop 超时后强制回收）都是主动停止的子路径。
    /// `NormalExit`（exit code == 0）也视为 deliberate——worker 正常自行退出。
    pub fn is_deliberate_stop(&self) -> bool {
        matches!(
            self,
            ExitReason::Stopped { .. }
                | ExitReason::StartCancelled
                | ExitReason::ForceKilled { .. }
                | ExitReason::NormalExit { .. }
        )
    }
}

/// 每次启动的唯一身份令牌。
///
/// 包含 generation（单调递增）和 instance_id（随机）。
/// 旧 token 的提交请求在新 token 已生效后会被拒绝。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstanceToken {
    /// 单调递增的 generation 序号。
    pub generation: u64,
    /// 随机生成的 instance_id（日志与诊断用）。
    pub instance_id: String,
}

/// 进程身份记录（用于安全终止验证）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessIdentity {
    /// 操作系统 PID。
    pub pid: u32,
    /// 可执行文件路径（用于终止前验证身份）。
    pub executable: PathBuf,
    /// OS 真实进程创建时间（Unix 毫秒时间戳，用于防 PID 复用）。
    /// 0 表示查询失败——`kill_process_tree_verified` 在此值时拒绝终止（fail-closed）。
    /// 不得用 Blink wall-clock 提交时刻冒充 OS creation time。
    pub start_time_ms: u64,
    /// 本次启动的 instance_id（随机生成，日志与诊断用）。
    pub instance_id: String,
}

/// 完整状态快照（含 generation 防旧任务覆盖）。
#[derive(Debug, Clone)]
pub struct ManagedProcessState {
    /// 当前 token（每次 start 递增），用于隔离旧 wait/stop 事件。
    pub token: InstanceToken,
    /// 进程状态。
    pub status: ProcessStatus,
    /// 进程身份信息（PID、可执行路径、启动时间等）。
    pub identity: Option<ProcessIdentity>,
    /// 当前 generation 的 cancellation flag。
    /// stop 在 Starting 阶段设置此标志，使迟到的 spawn 结果无法提交 Running。
    cancelled: Arc<AtomicBool>,
}

impl ManagedProcessState {
    /// 创建初始 Stopped 状态。
    pub fn initial() -> Self {
        Self {
            token: InstanceToken {
                generation: 0,
                instance_id: String::new(),
            },
            status: ProcessStatus::Stopped,
            identity: None,
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    /// 为新 start 准备 Starting 状态。
    /// 返回新 token 和 cancellation flag。
    pub fn begin_start(&mut self) -> InstanceToken {
        let new_gen = self.token.generation + 1;
        let new_token = InstanceToken {
            generation: new_gen,
            instance_id: generate_instance_id(),
        };
        self.token = new_token.clone();
        self.status = ProcessStatus::Starting;
        self.identity = None;
        self.cancelled = Arc::new(AtomicBool::new(false));
        new_token
    }

    /// 获取当前 token 的 cancellation flag 引用。
    pub fn cancellation_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancelled)
    }

    /// 提交 Running 状态。
    ///
    /// 验证条件（全部满足才提交）：
    /// 1. token 匹配当前 generation + instance_id；
    /// 2. 当前状态仍为 Starting；
    /// 3. cancellation 未触发。
    ///
    /// 返回 `CommitResult` 告知调用方是否需要回收 child。
    pub fn try_commit_running(
        &mut self,
        token: &InstanceToken,
        pid: u32,
        identity: ProcessIdentity,
    ) -> CommitResult {
        // 1. token 必须完全匹配
        if token != &self.token {
            tracing::debug!(
                gen = token.generation,
                current_gen = self.token.generation,
                "拒绝 Running 提交：token 不匹配"
            );
            return CommitResult::Rejected;
        }
        // 2. 当前状态必须仍为 Starting
        if self.status != ProcessStatus::Starting {
            tracing::debug!(
                gen = token.generation,
                status = ?self.status,
                "拒绝 Running 提交：状态不是 Starting"
            );
            return CommitResult::Rejected;
        }
        // 3. cancellation 必须未触发
        if self.cancelled.load(Ordering::Acquire) {
            tracing::info!(
                gen = token.generation,
                "拒绝 Running 提交：启动已被取消，需回收 child"
            );
            return CommitResult::Cancelled;
        }

        self.status = ProcessStatus::Running { pid };
        self.identity = Some(identity);
        CommitResult::Committed
    }

    /// 提交退出状态。
    ///
    /// 验证条件：
    /// 1. token generation 匹配当前 generation；
    /// 2. 当前状态为活跃状态（Starting / Running / Stopping）。
    ///
    /// 旧 generation 的退出事件不能覆盖新 generation。
    pub fn try_commit_exit(&mut self, token: &InstanceToken, reason: ExitReason) -> bool {
        // generation 必须匹配
        if token.generation != self.token.generation {
            tracing::debug!(
                gen = token.generation,
                current_gen = self.token.generation,
                "拒绝退出提交：generation 不匹配"
            );
            return false;
        }
        // 当前状态必须是活跃状态（不允许从 Stopped/Exited 转为 Exited）
        if !self.status.is_active() {
            tracing::debug!(
                gen = token.generation,
                status = ?self.status,
                "拒绝退出提交：当前状态非活跃"
            );
            return false;
        }

        self.status = ProcessStatus::Exited { reason };
        true
    }

    /// 标记当前 generation 的启动已被取消。
    ///
    /// 在 Starting 阶段被 stop 调用时使用。
    /// 设置 cancellation flag，使迟到的 spawn 结果无法通过 `try_commit_running`。
    /// 不改变 status（status 由 stop 路径后续设置为 Exited）。
    pub fn mark_cancelled(&self) {
        self.cancelled.store(true, Ordering::Release);
    }

    /// 检查当前 generation 是否已被取消。
    #[allow(dead_code)] // test cancellation semantics
    pub fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Acquire)
    }

    /// 检查指定 token 是否为当前实例。
    pub fn is_current(&self, token: &InstanceToken) -> bool {
        &self.token == token
    }

    /// 直接设置状态（仅供 stop 路径在持有锁时使用）。
    /// stop 路径已经验证了 token，可以直接设置。
    pub fn set_status_stopping(&mut self) {
        self.status = ProcessStatus::Stopping;
    }

    pub fn set_status_exited(&mut self, reason: ExitReason) {
        self.status = ProcessStatus::Exited { reason };
    }

    /// 获取当前 generation。
    pub fn generation(&self) -> u64 {
        self.token.generation
    }
}

/// `try_commit_running` 的返回值。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitResult {
    /// 提交成功，进程已标记为 Running。
    Committed,
    /// 提交被拒绝（token 不匹配或状态不合法），调用方应回收 child。
    Rejected,
    /// 启动已被取消，调用方必须回收 child。
    Cancelled,
}

impl CommitResult {
    /// 是否需要回收 child（Rejected 或 Cancelled）。
    pub fn needs_reclaim(self) -> bool {
        matches!(self, CommitResult::Rejected | CommitResult::Cancelled)
    }
}

/// 生成随机 instance_id。
fn generate_instance_id() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    format!("inst-{now:016x}-{:04x}", rand_word())
}

/// 简单伪随机（不引入 rand crate）。
fn rand_word() -> u16 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_state_is_stopped() {
        let state = ManagedProcessState::initial();
        assert_eq!(state.status, ProcessStatus::Stopped);
        assert_eq!(state.token.generation, 0);
        assert!(state.identity.is_none());
        assert!(!state.is_cancelled());
    }

    #[test]
    fn begin_start_increments_generation() {
        let mut state = ManagedProcessState::initial();
        let token1 = state.begin_start();
        assert_eq!(token1.generation, 1);
        assert_eq!(state.status, ProcessStatus::Starting);

        // 模拟取消后重新 start
        state.set_status_exited(ExitReason::StartCancelled);
        // 确保时间戳不同（instance_id 包含时间戳）
        std::thread::sleep(std::time::Duration::from_millis(2));
        let token2 = state.begin_start();
        assert_eq!(token2.generation, 2);
        assert_ne!(token1.instance_id, token2.instance_id);
    }

    #[test]
    fn commit_running_succeeds_for_current_token() {
        let mut state = ManagedProcessState::initial();
        let token = state.begin_start();

        let identity = ProcessIdentity {
            pid: 123,
            executable: PathBuf::from("/test"),
            start_time_ms: 0,
            instance_id: token.instance_id.clone(),
        };

        let result = state.try_commit_running(&token, 123, identity);
        assert_eq!(result, CommitResult::Committed);
        assert_eq!(state.status, ProcessStatus::Running { pid: 123 });
    }

    #[test]
    fn commit_running_rejected_for_old_token() {
        let mut state = ManagedProcessState::initial();
        let token1 = state.begin_start();

        // 模拟取消后重新 start
        state.mark_cancelled();
        state.set_status_exited(ExitReason::StartCancelled);
        let _token2 = state.begin_start();

        // 旧 token 的 spawn 结果到达
        let identity = ProcessIdentity {
            pid: 999,
            executable: PathBuf::from("/old"),
            start_time_ms: 0,
            instance_id: token1.instance_id.clone(),
        };
        let result = state.try_commit_running(&token1, 999, identity);
        assert_eq!(result, CommitResult::Rejected);
        assert_eq!(state.status, ProcessStatus::Starting); // 新 start 的状态不变
    }

    #[test]
    fn commit_running_cancelled_when_stop_during_starting() {
        let mut state = ManagedProcessState::initial();
        let token = state.begin_start();

        // stop 在 spawn 进行中到达
        state.mark_cancelled();

        // 迟到的 spawn 结果
        let identity = ProcessIdentity {
            pid: 42,
            executable: PathBuf::from("/test"),
            start_time_ms: 0,
            instance_id: token.instance_id.clone(),
        };
        let result = state.try_commit_running(&token, 42, identity);
        assert_eq!(result, CommitResult::Cancelled);
        assert!(result.needs_reclaim());
        // 状态仍为 Starting（stop 路径会设置为 Exited）
        assert_eq!(state.status, ProcessStatus::Starting);
    }

    #[test]
    fn commit_exit_rejected_for_old_generation() {
        let mut state = ManagedProcessState::initial();
        let token1 = state.begin_start();
        state.set_status_exited(ExitReason::StartCancelled);

        let _token2 = state.begin_start();
        state.set_status_exited(ExitReason::NormalExit { code: 0 });

        // 旧 generation 的退出事件
        let ok = state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 });
        assert!(!ok);
        // 当前状态不变
        assert_eq!(
            state.status,
            ProcessStatus::Exited {
                reason: ExitReason::NormalExit { code: 0 }
            }
        );
    }

    #[test]
    fn commit_exit_rejected_from_non_active_state() {
        let mut state = ManagedProcessState::initial();
        let token = state.begin_start();
        state.set_status_exited(ExitReason::NormalExit { code: 0 });

        // 从 Exited 再次提交 Exited
        let ok = state.try_commit_exit(&token, ExitReason::NonZeroExit { code: 1 });
        assert!(!ok);
    }

    #[test]
    fn is_current_checks_full_token() {
        let mut state = ManagedProcessState::initial();
        let token = state.begin_start();

        assert!(state.is_current(&token));

        // 新 start 后旧 token 不再 current
        let token2 = state.begin_start();
        assert!(!state.is_current(&token));
        assert!(state.is_current(&token2));
    }
}
