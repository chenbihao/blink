//! 引擎操作的纯领域结果协议。

use serde::{Deserialize, Serialize};

// ── CancelOutcome ───────────────────────────────────────────────────────────

/// 取消请求的结果——取消是正常协议语义，**不用错误类型表达**。
///
/// 调用方（command 层）直接投影为 IPC 响应，不再解码
/// `LocalEngineError::Cancelled` 伪装的"成功错误"。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum CancelOutcome {
    /// 取消信号已发送给匹配的活跃操作。
    ///
    /// worker 结束前 claim 仍由其 guard 持有——取消是终态请求，
    /// 实际收尾由 worker 以 `Cancelled` 终态结束。
    Cancelled,
    /// 当前没有活跃操作（已完成/已失败/未开始）。
    NoActiveOperation,
    /// operation_id 与当前活跃操作不匹配——不触发任何 token。
    Mismatched {
        /// 当前活跃操作的 id（供前端核对）。
        current_operation_id: String,
    },
}

impl CancelOutcome {
    /// 是否成功发出取消信号。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled)
    }
}
