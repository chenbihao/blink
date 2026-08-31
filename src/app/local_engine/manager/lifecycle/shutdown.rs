use super::*;

impl EngineManager {
    // ── shutdown_all ────────────────────────────────────────────────────────

    /// 异步遍历所有受管实例并回收。
    ///
    /// 单个失败不能阻止其他实例回收；最终返回汇总错误并记录结构化日志。
    #[allow(dead_code)] // 预留 API：当前生产退出用 shutdown_all_blocking，async 版本待后续接入
    pub async fn shutdown_all(&self) -> Result<(), Vec<LocalEngineError>> {
        let entries = self.entries.read().await;
        let mut errors = Vec::new();

        for (engine_id, entry) in entries.iter() {
            let managed_opt = entry.managed_process.lock().await.clone();
            if let Some(managed) = managed_opt {
                tracing::info!(engine = %engine_id, "shutdown_all: 回收引擎实例");
                self.graceful_stop_worker(engine_id, entry, &managed).await;
                if let Err(e) = managed.stop().await {
                    let err = from_process(ErrorPhase::Stop, "shutdown_all 回收失败", &e);
                    tracing::error!(engine = %engine_id, %err, "shutdown_all: 回收失败");
                    errors.push(err);
                } else {
                    // 更新状态
                    let _ = self
                        .commit_status_internal(engine_id, None, |status| {
                            status.desired = DesiredState::Stopped;
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                        })
                        .await;
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 同步阻塞版本的 shutdown_all（应用退出用）。
    ///
    /// 遍历 `process_registry`（同步 Mutex，不依赖 async lock），
    /// 对每个 ManagedProcess 调用 `shutdown_blocking()`。
    /// 单个失败不阻止其他回收。
    ///
    /// **0.22.3 Task E**: 不依赖 `entries` 的 async lock——
    /// `process_registry` 是独立的同步 Mutex，shutdown 路径可靠。
    pub fn shutdown_all_blocking(&self) {
        // 同步遍历 process_registry——不依赖 async entries lock
        let registry = self.process_registry.lock().unwrap();
        for (key, managed) in registry.iter() {
            tracing::info!(
                engine = %key.engine_id,
                instance = %key.instance_id,
                "shutdown_all_blocking: 回收"
            );
            managed.shutdown_blocking();
        }
    }
}
