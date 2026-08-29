//! 引擎操作协调器（进程级唯一变更操作真相）。
//!
//! ## 语义（铁则）
//!
//! - **key 只有 engine_id**：同一引擎同一时刻最多一个变更操作
//!   （安装/更新/修复/回滚/清理/启动/停止/模型资产操作）。
//! - **原子 claim**：busy 检查与 operation 登记在同一次锁内完成，
//!   不存在"检查后、登记前"的窗口。
//! - **RAII OperationGuard**：worker 真正结束（guard drop）前不能释放
//!   claim——cancel 只触发 token，不移除 claim，因此 cancel 后不能立即
//!   允许下一个操作。
//! - **cancel 只作用于匹配 operation_id 的 token**：错配/过期操作不可取消
//!   任何在途操作。
//! - **completed operation 不再是 busy state**：guard drop 后 claim 移除，
//!   迟到的 cancel 只得到 `NotRunning`。
//!
//! 本模块是纯内存协调原语：不做 IO、不持进程句柄、不发送事件，
//! 可以在任何异步上下文安全使用（内部临界区无 await）。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use super::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};
use super::identity::EngineId;

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

/// 活跃 claim 记录。
#[derive(Debug, Clone)]
struct ActiveClaim {
    operation_id: String,
    cancel_token: CancellationToken,
}

/// 进程级唯一的引擎操作协调器。
///
/// 由 `EngineManager` 持有一份；所有变更操作必须先 `try_claim` 取得
/// guard 才能执行。
///
/// 并发模型：单个 `std::sync::Mutex<HashMap<EngineId, ActiveClaim>>`，
/// 临界区内只有 HashMap 读写，无 IO、无 await，锁持有时间为纳秒级。
pub struct EngineOperationCoordinator {
    claims: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
}

impl Default for EngineOperationCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineOperationCoordinator {
    /// 创建协调器。
    pub fn new() -> Self {
        Self {
            claims: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 原子完成 busy 检查与 operation claim。
    ///
    /// - 引擎无活跃操作 → 登记 claim，返回 RAII guard；
    /// - 引擎有活跃操作 → 返回 `AlreadyRunning`（附当前 operation_id）。
    ///
    /// `operation_id` 必须非空——空 id 无法被 cancel 匹配，直接拒绝。
    pub fn try_claim(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> Result<OperationGuard, LocalEngineError> {
        if operation_id.is_empty() {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::InvalidConfig,
                ErrorPhase::Request,
                "操作 id 不能为空",
                "try_claim called with empty operation_id",
            ));
        }
        let mut claims = self.lock();
        if let Some(active) = claims.get(engine_id) {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::AlreadyRunning,
                ErrorPhase::Request,
                "引擎操作进行中，请等待或取消",
                format!(
                    "engine_id={} 正在执行 operation_id={}",
                    engine_id, active.operation_id
                ),
            ));
        }
        let cancel_token = CancellationToken::new();
        claims.insert(
            engine_id.clone(),
            ActiveClaim {
                operation_id: operation_id.to_string(),
                cancel_token: cancel_token.clone(),
            },
        );
        Ok(OperationGuard {
            engine_id: engine_id.clone(),
            operation_id: operation_id.to_string(),
            cancel_token,
            claims: Arc::clone(&self.claims),
            released: false,
        })
    }

    /// 取消匹配 `operation_id` 的活跃操作。
    ///
    /// - 无活跃操作 → `CancelOutcome::NoActiveOperation`
    ///   （completed operation 不再是 busy state）；
    /// - operation_id 不匹配 → `CancelOutcome::Mismatched`（不触发任何 token）；
    /// - 匹配 → 触发该 token，返回 `CancelOutcome::Cancelled`。
    ///   claim 仍由 worker 的 guard 持有，直到 worker 真正结束才释放。
    pub fn cancel(&self, engine_id: &EngineId, operation_id: &str) -> CancelOutcome {
        let claims = self.lock();
        match claims.get(engine_id) {
            None => CancelOutcome::NoActiveOperation,
            Some(active) if active.operation_id == operation_id => {
                active.cancel_token.cancel();
                CancelOutcome::Cancelled
            }
            Some(active) => CancelOutcome::Mismatched {
                current_operation_id: active.operation_id.clone(),
            },
        }
    }

    /// 查询引擎当前活跃的 operation_id（None = 空闲）。
    ///
    /// 只读查询，不构成 claim。
    pub fn active_operation(&self, engine_id: &EngineId) -> Option<String> {
        let claims = self.lock();
        claims.get(engine_id).map(|c| c.operation_id.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EngineId, ActiveClaim>> {
        self.claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// RAII 操作 guard——持有期间该 engine 的变更操作 claim 不释放。
///
/// worker（安装/修复/启动/停止等 future）必须持有 guard 直到真正结束；
/// guard drop 时自动释放 claim，此后引擎才允许下一个操作。
///
/// **禁止**手动提前 drop guard 来"腾出"引擎——那会破坏
/// "cancel 后不得立即允许下一个操作"的语义。
#[derive(Debug)]
pub struct OperationGuard {
    engine_id: EngineId,
    operation_id: String,
    cancel_token: CancellationToken,
    claims: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
    released: bool,
}

impl OperationGuard {
    /// 本操作的 cancel token（worker 在长耗时步骤 select 它）。
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }

    /// 是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

    /// 显式释放 claim（等价于 drop）——测试模拟 worker 结束用。
    ///
    /// 幂等：drop 时不会二次释放。
    #[cfg(test)]
    pub fn release(mut self) {
        self.release_inner();
    }

    fn release_inner(&mut self) {
        if self.released {
            return;
        }
        self.released = true;
        let mut claims = self
            .claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // 只移除属于本操作的 claim——防御性检查，避免误删后来者
        if claims
            .get(&self.engine_id)
            .map(|c| c.operation_id == self.operation_id)
            .unwrap_or(false)
        {
            claims.remove(&self.engine_id);
        }
    }
}

impl Drop for OperationGuard {
    fn drop(&mut self) {
        self.release_inner();
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::Duration;

    fn eid(name: &str) -> EngineId {
        EngineId::new(name).unwrap()
    }

    #[test]
    fn claim_and_release_roundtrip() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let guard = coord.try_claim(&engine, "op-1").unwrap();
        assert_eq!(coord.active_operation(&engine).as_deref(), Some("op-1"));

        guard.release();
        assert_eq!(coord.active_operation(&engine), None);
    }

    #[test]
    fn second_claim_rejected_while_busy() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let _guard = coord.try_claim(&engine, "op-1").unwrap();
        let err = coord.try_claim(&engine, "op-2").unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);
        // 错误详情包含当前 operation_id，便于诊断
        assert!(err.detail.contains("op-1"));
    }

    #[test]
    fn different_engines_claim_independently() {
        let coord = EngineOperationCoordinator::new();
        let a = eid("funasr");
        let b = eid("paddleocr");

        let _ga = coord.try_claim(&a, "op-a").unwrap();
        let _gb = coord.try_claim(&b, "op-b").unwrap();
        assert_eq!(coord.active_operation(&a).as_deref(), Some("op-a"));
        assert_eq!(coord.active_operation(&b).as_deref(), Some("op-b"));
    }

    #[test]
    fn empty_operation_id_rejected() {
        let coord = EngineOperationCoordinator::new();
        let err = coord.try_claim(&eid("funasr"), "").unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
    }

    #[test]
    fn cancel_matches_only_same_operation_id() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let guard = coord.try_claim(&engine, "op-1").unwrap();
        // 错配 → Mismatched，且不触发 token
        match coord.cancel(&engine, "op-stale") {
            CancelOutcome::Mismatched {
                current_operation_id,
            } => {
                assert_eq!(current_operation_id, "op-1");
            }
            other => panic!("应返回 Mismatched: {other:?}"),
        }
        assert!(!guard.is_cancelled());

        // 匹配 → 触发 token
        assert!(coord.cancel(&engine, "op-1").is_cancelled());
        assert!(guard.is_cancelled());
    }

    #[test]
    fn cancel_after_completion_is_no_active_operation() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let guard = coord.try_claim(&engine, "op-1").unwrap();
        guard.release();

        // completed operation 不再是 busy state
        assert_eq!(
            coord.cancel(&engine, "op-1"),
            CancelOutcome::NoActiveOperation
        );
    }

    /// cancel 后旧 worker 尚未退出（guard 仍持有）时，
    /// 下一个操作必须仍被拒绝。
    #[tokio::test]
    async fn cancel_does_not_release_claim_until_worker_finishes() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let guard = coord.try_claim(&engine, "op-1").unwrap();
        assert!(coord.cancel(&engine, "op-1").is_cancelled());

        // worker 尚未结束——即使已被取消，也不能立即开始下一个操作
        let err = coord.try_claim(&engine, "op-2").unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

        // worker 真正结束（guard drop）后，下一个操作才被允许
        drop(guard);
        assert!(coord.try_claim(&engine, "op-2").is_ok());
    }

    /// 并发 claim 竞争：N 个并发尝试只有 1 个成功（barrier 对齐后同时发起）。
    #[tokio::test]
    async fn concurrent_claims_admit_exactly_one() {
        let coord = Arc::new(EngineOperationCoordinator::new());
        let engine = eid("funasr");
        let barrier = Arc::new(tokio::sync::Barrier::new(8));

        let mut handles = Vec::new();
        for i in 0..8 {
            let coord = Arc::clone(&coord);
            let barrier = Arc::clone(&barrier);
            let engine = engine.clone();
            handles.push(tokio::spawn(async move {
                barrier.wait().await;
                coord.try_claim(&engine, &format!("op-{i}"))
            }));
        }

        let mut ok = 0;
        for h in handles {
            if h.await.unwrap().is_ok() {
                ok += 1;
            }
        }
        assert_eq!(ok, 1);
    }

    /// 真实并发 worker 生命周期：worker 在 Notify 上等待取消信号，
    /// cancel 触发后 worker 结束前 claim 不释放。
    #[tokio::test]
    async fn worker_finish_gate_via_notify() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        let guard = coord.try_claim(&engine, "op-install").unwrap();
        let token = guard.cancel_token().clone();
        let worker_done = Arc::new(tokio::sync::Notify::new());

        let worker_done_signal = Arc::clone(&worker_done);
        let worker = tokio::spawn(async move {
            // 模拟长耗时安装：等待取消信号
            tokio::select! {
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_secs(60)) => {}
            }
            // 模拟 worker 收尾工作仍在进行
            tokio::time::sleep(Duration::from_millis(20)).await;
            drop(guard);
            worker_done_signal.notify_waiters();
        });

        // 取消操作
        assert!(coord.cancel(&engine, "op-install").is_cancelled());

        // worker 尚未退出：下一个操作仍被拒绝
        let err = coord.try_claim(&engine, "op-next").unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

        // 等 worker 真正结束（先订阅再 await，避免错过通知）
        let subscribed = worker_done.notified();
        tokio::pin!(subscribed);
        worker.await.unwrap();
        subscribed.await;

        assert_eq!(coord.active_operation(&engine), None);
        assert!(coord.try_claim(&engine, "op-next").is_ok());
    }

    /// guard 是 Send：可以跨 await 持有（安装事务全程持有）。
    #[tokio::test]
    async fn guard_is_send_across_await() {
        let coord = EngineOperationCoordinator::new();
        let engine = eid("funasr");

        fn assert_send<T: Send>(_: &T) {}

        let guard = coord.try_claim(&engine, "op-1").unwrap();
        assert_send(&guard);
        tokio::time::sleep(Duration::from_millis(1)).await;
        drop(guard);
    }
}
