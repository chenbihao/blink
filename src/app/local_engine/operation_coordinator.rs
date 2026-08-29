//! 引擎 mutation 的应用运行时协调器。

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

pub struct EngineOperationCoordinator {
    claims: Arc<Mutex<HashMap<EngineId, ActiveClaim>>>,
}

impl Default for EngineOperationCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

impl EngineOperationCoordinator {
    pub fn new() -> Self {
        Self {
            claims: Arc::new(Mutex::new(HashMap::new())),
        }
    }

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

    pub fn active_operation(&self, engine_id: &EngineId) -> Option<String> {
        self.lock()
            .get(engine_id)
            .map(|claim| claim.operation_id.clone())
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<EngineId, ActiveClaim>> {
        self.claims
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

#[derive(Debug)]
pub struct OperationGuard {
    engine_id: EngineId,
    operation_id: String,
    cancel_token: CancellationToken,
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
}
