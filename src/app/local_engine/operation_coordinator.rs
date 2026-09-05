//! 引擎 mutation 的应用运行时协调器。
//!
//! ## 双槽 claim（0.22.9 修复）
//!
//! 每引擎两个独立槽位，槽内互斥、跨槽并行：
//!
//! - **Mutating 槽**：进程级变更（start / stop / 环境安装 / 修复 / 切换事务 /
//!   ParaformerOnline 模型安装等）——这些操作会触碰进程与部署状态，
//!   必须全量互斥。状态提交的 operation 门（`commit_status_internal`）
//!   以本槽为唯一真源。
//! - **ModelStorage 槽**：纯模型资产操作（model_storage staging 的
//!   下载 / 修复 / 删除）——只写模型 payload 目录，不触碰进程与部署状态。
//!
//! 拆槽动机：模型下载耗时可达分钟级，此前它与 `stop` 全量互斥，导致
//! "下载新模型时无法停掉旧服务"（实测反馈）。模型下载与进程停止正交，
//! 各占一槽后 `stop` 不再被下载阻塞；同槽仍互斥，保证
//! `cleanup_orphan_staging` 等同 asset_key 操作不并发。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tokio_util::sync::CancellationToken;

use crate::domain::local_engine::{
    CancelOutcome, EngineId, ErrorPhase, LocalEngineError, LocalEngineErrorCode,
};

#[derive(Debug, Clone)]
struct ActiveClaim {
    operation_id: String,
    cancel_token: CancellationToken,
}

/// claim 槽位（决定 guard 释放时清理哪个槽）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClaimSlot {
    Mutating,
    ModelStorage,
}

pub struct EngineOperationCoordinator {
    /// 进程级变更槽——`active_operation`（状态 operation 门）的唯一来源
    mutating: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
    /// 模型资产操作槽——与 mutating 槽互不阻塞
    model_storage: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
}

impl Default for EngineOperationCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineOperationCoordinator {
    pub fn new() -> Self {
        Self {
            mutating: Arc::new(Mutex::new(HashMap::new())),
            model_storage: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    /// 申请进程级变更 claim（全量互斥语义，与既有调用方一致）。
    pub fn try_claim(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> Result<OperationGuard, LocalEngineError> {
        self.try_claim_in_slot(
            engine_id,
            operation_id,
            ClaimSlot::Mutating,
            "引擎操作进行中，请等待或取消",
        )
    }

    /// 申请模型资产操作 claim（只与同槽其他模型操作互斥）。
    ///
    /// 不阻塞 mutating 槽：模型下载期间 start/stop 等进程级操作照常执行；
    /// 反之模型操作也不被进程级操作阻塞。
    pub fn try_claim_model_storage(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> Result<OperationGuard, LocalEngineError> {
        self.try_claim_in_slot(
            engine_id,
            operation_id,
            ClaimSlot::ModelStorage,
            "模型操作进行中，请等待或取消",
        )
    }

    fn try_claim_in_slot(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        slot: ClaimSlot,
        busy_hint: &str,
    ) -> Result<OperationGuard, LocalEngineError> {
        if operation_id.is_empty() {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::InvalidConfig,
                ErrorPhase::Request,
                "操作 id 不能为空",
                "try_claim called with empty operation_id",
            ));
        }
        let mut claims = self.slot_mut(slot);
        if let Some(active) = claims.get(engine_id) {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::AlreadyRunning,
                ErrorPhase::Request,
                busy_hint,
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
            claims: self.claims_for_slot(slot),
            released: false,
        })
    }

    /// 取消匹配 operation_id 的活跃操作（双槽精确匹配，槽顺序不影响命中）。
    pub fn cancel(&self, engine_id: &EngineId, operation_id: &str) -> CancelOutcome {
        {
            let mutating = self.mutating.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(active) = mutating.get(engine_id)
                && active.operation_id == operation_id
            {
                active.cancel_token.cancel();
                return CancelOutcome::Cancelled;
            }
        }
        {
            let model_ops = self.model_storage.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(active) = model_ops.get(engine_id)
                && active.operation_id == operation_id
            {
                active.cancel_token.cancel();
                return CancelOutcome::Cancelled;
            }
        }
        // 无精确匹配——报 Mismatched（优先展示 mutating 槽的当前 id），否则无活跃操作
        {
            let mutating = self.mutating.lock().unwrap_or_else(|p| p.into_inner());
            if let Some(active) = mutating.get(engine_id) {
                return CancelOutcome::Mismatched {
                    current_operation_id: active.operation_id.clone(),
                };
            }
        }
        let model_ops = self.model_storage.lock().unwrap_or_else(|p| p.into_inner());
        match model_ops.get(engine_id) {
            Some(active) => CancelOutcome::Mismatched {
                current_operation_id: active.operation_id.clone(),
            },
            None => CancelOutcome::NoActiveOperation,
        }
    }

    /// 当前进程级变更操作的 operation_id（状态提交 operation 门的真源）。
    ///
    /// 只看 mutating 槽——模型资产操作不提交引擎状态。
    pub fn active_operation(&self, engine_id: &EngineId) -> Option<String> {
        self.mutating
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(engine_id)
            .map(|claim| claim.operation_id.clone())
    }

    /// 任一槽位的活跃 operation_id（mutating 优先）——测试/诊断用取消发现。
    #[cfg(test)]
    pub fn active_operation_any(&self, engine_id: &EngineId) -> Option<String> {
        if let Some(op) = self.active_operation(engine_id) {
            return Some(op);
        }
        self.model_storage
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .get(engine_id)
            .map(|claim| claim.operation_id.clone())
    }

    fn slot_mut(
        &self,
        slot: ClaimSlot,
    ) -> std::sync::MutexGuard<'_, HashMap<EngineId, ActiveClaim>> {
        match slot {
            ClaimSlot::Mutating => self.mutating.lock().unwrap_or_else(|p| p.into_inner()),
            ClaimSlot::ModelStorage => {
                self.model_storage.lock().unwrap_or_else(|p| p.into_inner())
            }
        }
    }

    fn claims_for_slot(&self, slot: ClaimSlot) -> Arc<Mutex<HashMap<EngineId, ActiveClaim>>> {
        match slot {
            ClaimSlot::Mutating => Arc::clone(&self.mutating),
            ClaimSlot::ModelStorage => Arc::clone(&self.model_storage),
        }
    }
}

#[derive(Debug)]
pub struct OperationGuard {
    engine_id: EngineId,
    operation_id: String,
    cancel_token: CancellationToken,
    /// guard 归属槽的共享 map——释放时只清理本槽（operation_id 匹配校验兜底）
    claims: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
    released: bool,
}

impl OperationGuard {
    pub fn cancel_token(&self) -> &CancellationToken {
        &self.cancel_token
    }
    pub fn is_cancelled(&self) -> bool {
        self.cancel_token.is_cancelled()
    }

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
        if claims
            .get(&self.engine_id)
            .is_some_and(|claim| claim.operation_id == self.operation_id)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(name: &str) -> EngineId {
        EngineId::new(name).unwrap()
    }

    #[test]
    fn claim_is_atomic_per_engine_and_parallel_across_engines() {
        let coordinator = EngineOperationCoordinator::new();
        let funasr = eid("funasr");
        let paddle = eid("paddleocr");
        let _first = coordinator.try_claim(&funasr, "model-install").unwrap();
        assert_eq!(
            coordinator
                .try_claim(&funasr, "environment-repair")
                .unwrap_err()
                .code,
            LocalEngineErrorCode::AlreadyRunning,
        );
        assert!(coordinator.try_claim(&paddle, "model-install").is_ok());
    }

    #[test]
    fn cancellation_does_not_release_worker_claim() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let guard = coordinator.try_claim(&engine, "old-worker").unwrap();
        assert_eq!(
            coordinator.cancel(&engine, "old-worker"),
            CancelOutcome::Cancelled
        );
        assert_eq!(
            coordinator
                .try_claim(&engine, "new-worker")
                .unwrap_err()
                .code,
            LocalEngineErrorCode::AlreadyRunning,
        );
        drop(guard);
        assert!(coordinator.try_claim(&engine, "new-worker").is_ok());
    }

    #[test]
    fn early_return_and_panic_release_guard() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        {
            let _guard = coordinator.try_claim(&engine, "early").unwrap();
        }
        assert!(coordinator.active_operation(&engine).is_none());
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = coordinator.try_claim(&engine, "panic").unwrap();
            panic!("worker panic");
        }));
        assert!(coordinator.active_operation(&engine).is_none());
    }

    // ── 双槽语义（0.22.9：模型下载不阻塞 stop）────────────────────────────

    #[test]
    fn stop_is_admitted_while_model_storage_claim_active() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let _download = coordinator.try_claim_model_storage(&engine, "dl-nano").unwrap();
        // 进程级操作照常申请——模型下载不阻塞 stop/start
        let stop = coordinator.try_claim(&engine, "stop-1").unwrap();
        assert_eq!(coordinator.active_operation(&engine), Some("stop-1".into()));
        drop(stop);
        drop(_download);
        assert!(coordinator.active_operation(&engine).is_none());
    }

    #[test]
    fn model_storage_ops_are_mutually_exclusive() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let _first = coordinator
            .try_claim_model_storage(&engine, "dl-a")
            .unwrap();
        assert_eq!(
            coordinator
                .try_claim_model_storage(&engine, "dl-b")
                .unwrap_err()
                .code,
            LocalEngineErrorCode::AlreadyRunning,
        );
    }

    #[test]
    fn mutating_ops_are_mutually_exclusive_across_slots() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let _stop = coordinator.try_claim(&engine, "stop-1").unwrap();
        assert_eq!(
            coordinator
                .try_claim(&engine, "stop-2")
                .unwrap_err()
                .code,
            LocalEngineErrorCode::AlreadyRunning,
        );
        assert_eq!(
            coordinator
                .try_claim(&engine, "install-env")
                .unwrap_err()
                .code,
            LocalEngineErrorCode::AlreadyRunning,
        );
    }

    #[test]
    fn cancel_finds_model_storage_claim() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let guard = coordinator
            .try_claim_model_storage(&engine, "dl-nano")
            .unwrap();
        // active_operation 只暴露 mutating 槽；any 才包含模型操作
        assert_eq!(coordinator.active_operation(&engine), None);
        assert_eq!(
            coordinator.active_operation_any(&engine),
            Some("dl-nano".into())
        );
        assert_eq!(
            coordinator.cancel(&engine, "dl-nano"),
            CancelOutcome::Cancelled
        );
        assert!(guard.is_cancelled());
        drop(guard);
        // 释放后双槽均空
        assert!(coordinator.try_claim_model_storage(&engine, "dl-2").is_ok());
    }

    #[test]
    fn cancel_prefers_exact_match_in_model_slot_while_mutating_active() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let model = coordinator
            .try_claim_model_storage(&engine, "dl-nano")
            .unwrap();
        let _stop = coordinator.try_claim(&engine, "stop-1").unwrap();
        // 双槽精确匹配：mutating 槽在途不阻碍取消模型操作
        assert_eq!(
            coordinator.cancel(&engine, "dl-nano"),
            CancelOutcome::Cancelled
        );
        assert!(model.is_cancelled());
        // 两个 id 都不匹配 → Mismatched（展示 mutating 槽当前 id）
        assert_eq!(
            coordinator.cancel(&engine, "unknown-op"),
            CancelOutcome::Mismatched {
                current_operation_id: "stop-1".into()
            }
        );
    }

    #[test]
    fn model_guard_release_does_not_remove_mutating_claim() {
        let coordinator = EngineOperationCoordinator::new();
        let engine = eid("funasr");
        let model = coordinator
            .try_claim_model_storage(&engine, "dl-nano")
            .unwrap();
        let _stop = coordinator.try_claim(&engine, "stop-1").unwrap();
        drop(model);
        // model guard 释放只清 model 槽；mutating 槽的 stop 不受影响
        assert_eq!(coordinator.active_operation(&engine), Some("stop-1".into()));
        assert!(coordinator.cancel(&engine, "stop-1") == CancelOutcome::Cancelled);
    }
}
