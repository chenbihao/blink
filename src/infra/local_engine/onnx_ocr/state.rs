//! Executor 生命周期状态（0.22.8-C）。
//!
//! 状态转换图：
//!
//! ```text
//! ┌──────┐  first request  ┌──────────┐  success  ┌───────┐
//! │ Idle │ ──────────────→ │ Starting │ ────────→ │ Ready │
//! └──────┘                  └──────────┘           └───────┘
//!                                │                      │
//!                           fail│                TTL / shutdown
//!                                ↓                      ↓
//!                           ┌────────┐           ┌──────────┐
//!                           │ Failed │           │ Stopping  │
//!                           └────────┘           └──────────┘
//!                                │                      │
//!                           retry                     done
//!                                ↓                      ↓
//!                           ┌──────────┐           ┌──────────┐
//!                           │ Idle     │←──────────│ Stopped  │
//!                           │(gen+1)   │           │ (=Idle)  │
//!                           └──────────┘           └──────────┘
//! ```
//!
//! 使用 `tokio::sync::watch` 广播状态——watch 不丢通知，
//! waiter 通过 `changed()` 可靠等待状态转换。

use std::sync::Arc;
use std::time::Instant;

use tokio::sync::watch;

/// 生命周期状态（通过 watch channel 广播）。
#[derive(Debug, Clone, Default)]
pub enum ExecutorState {
    /// 未加载。首次请求触发 lazy load。
    #[default]
    Idle,
    /// 正在构建 ORT Session（DLL init + model load）。
    /// 此状态由 winner 设置，其余请求等待。
    Starting {
        /// 请求 generation（每次 Idle→Starting 递增）。
        generation: u64,
    },
    /// Session 就绪，可接受推理请求。
    Ready {
        generation: u64,
        /// Session 就绪时刻（单调时钟），用于 TTL 计算。
        ready_at: Instant,
    },
    /// 正在停止（shutdown 或 TTL drop）。
    Stopping { generation: u64 },
    /// 构建失败。新请求可重试。
    Failed {
        generation: u64,
        /// 失败原因（用于诊断）。
        reason: Arc<str>,
    },
}

impl ExecutorState {
    /// 当前 generation（所有状态变体都有）。
    pub fn generation(&self) -> u64 {
        match &self {
            ExecutorState::Idle => 0,
            ExecutorState::Starting { generation }
            | ExecutorState::Ready { generation, .. }
            | ExecutorState::Stopping { generation }
            | ExecutorState::Failed { generation, .. } => *generation,
        }
    }

    /// 是否处于 Ready 状态。
    pub fn is_ready(&self) -> bool {
        matches!(self, ExecutorState::Ready { .. })
    }
}

impl std::fmt::Display for ExecutorState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self {
            ExecutorState::Idle => write!(f, "Idle"),
            ExecutorState::Starting { generation } => {
                write!(f, "Starting(gen={generation})")
            }
            ExecutorState::Ready { generation, .. } => {
                write!(f, "Ready(gen={generation})")
            }
            ExecutorState::Stopping { generation } => {
                write!(f, "Stopping(gen={generation})")
            }
            ExecutorState::Failed { generation, .. } => {
                write!(f, "Failed(gen={generation})")
            }
        }
    }
}

/// 状态通道句柄（tx + rx 共享 Arc）。
///
/// 持有一个保活的 receiver，确保 `send()` 不会因无 receiver 而失败。
#[derive(Clone)]
pub struct StateChannel {
    pub tx: Arc<watch::Sender<ExecutorState>>,
    /// 保活 receiver——防止 `send()` 返回 `Err`。
    /// 通过 `Arc<dyn Send + Sync>` 持有，不直接使用。
    _keepalive_rx: Arc<watch::Receiver<ExecutorState>>,
}

impl StateChannel {
    /// 创建新通道，初始状态为 Idle。
    pub fn new() -> Self {
        let (tx, rx) = watch::channel(ExecutorState::Idle);
        Self {
            tx: Arc::new(tx),
            _keepalive_rx: Arc::new(rx),
        }
    }

    /// 获取当前状态快照。
    pub fn current(&self) -> ExecutorState {
        self.tx.borrow().clone()
    }

    /// 订阅状态变更。
    pub fn subscribe(&self) -> watch::Receiver<ExecutorState> {
        self.tx.subscribe()
    }

    /// 原子 CAS：如果当前状态匹配 `expected`，则更新为 `new`。
    /// 返回是否成功。
    pub fn compare_swap(
        &self,
        expected_fn: impl Fn(&ExecutorState) -> bool,
        new: ExecutorState,
    ) -> bool {
        self.tx.send_if_modified(|current| {
            if expected_fn(current) {
                *current = new;
                true
            } else {
                false
            }
        })
    }
}

impl Default for StateChannel {
    fn default() -> Self {
        Self::new()
    }
}
