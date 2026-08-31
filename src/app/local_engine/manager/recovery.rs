//! EngineManager 恢复用例：孤儿引擎进程的 lease 探测与手动终止（stop_orphan_engine）。

use super::*;

impl EngineManager {
    // ── stop_orphan_engine（0.22.6.6）─────────────────────────────────────

    /// 手动停止孤儿引擎进程。
    ///
    /// 当 lease 恢复扫描发现遗留进程时，用户可通过设置页手动调用此方法终止。
    ///
    /// **安全策略**（fail-closed）：
    /// 1. 扫描 lease 文件，查找指定 engine 的 lease
    /// 2. 使用 `build_process_evidence` 查询 OS 进程身份
    /// 3. 使用 `probe_health_evidence` 探测 health 端点
    /// 4. 调用 `decide_recovery` 纯函数做恢复判定
    /// 5. 如果判定为 `Adoptable`，使用 `kill_process_tree_verified` 验证身份后终止
    /// 6. 终止后清除 lease 文件
    ///
    /// 证据不足时返回错误，不降级为仅 PID kill。
    pub async fn stop_orphan_engine(
        &self,
        engine_id: &EngineId,
    ) -> Result<super::super::dto::OrphanStopResultDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let engine_id_str = engine_id.to_string();

        // 1. 扫描 lease 文件，查找匹配的 lease
        let leases = tokio::task::spawn_blocking(crate::infra::local_engine::lease::scan_leases)
            .await
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Request,
                    "扫描 lease 失败",
                    format!("spawn_blocking join 错误: {e}"),
                )
            })?;

        let lease = leases
            .iter()
            .find(|l| l.engine_id == engine_id_str)
            .cloned();

        let lease = match lease {
            Some(l) => l,
            None => {
                return Ok(super::super::dto::OrphanStopResultDto {
                    engine_id: engine_id_str,
                    stopped: false,
                    reason: "lease_not_found".to_string(),
                    detail: Some("未找到该引擎的 lease 文件".to_string()),
                });
            }
        };

        // 2. 在 spawn_blocking 中构建进程证据
        let pid = lease.pid;
        let process_evidence = tokio::task::spawn_blocking(move || {
            crate::infra::local_engine::lease_recovery::build_process_evidence(pid)
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "查询进程证据失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        // 3. 异步探测 health 端点
        let health_evidence =
            crate::infra::local_engine::lease_recovery::probe_health_evidence(&lease.endpoint)
                .await;

        // 4. 调用 decide_recovery 做恢复判定
        let decision = crate::infra::local_engine::lease::decide_recovery(
            &lease,
            &process_evidence,
            health_evidence.as_ref(),
        );

        let result = match &decision {
            crate::infra::local_engine::lease::RecoveryDecision::Adoptable { pid, .. } => {
                // 5. 使用 kill_process_tree_verified 验证身份后终止
                let expected_exe = std::path::PathBuf::from(&lease.executable);
                let expected_creation = lease.creation_time_ms;
                let pid_val = *pid;

                let kill_result = tokio::task::spawn_blocking(move || {
                    crate::infra::platform::process::kill_process_tree_verified(
                        pid_val,
                        &expected_exe,
                        expected_creation,
                    )
                })
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Stop,
                        "终止进程失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?;

                match kill_result {
                    Ok(()) => {
                        // 6. 清除 lease 文件
                        if let Err(e) =
                            crate::infra::local_engine::lease::remove_lease_force(&engine_id_str)
                        {
                            tracing::warn!(
                                engine = %engine_id_str,
                                %e,
                                "孤儿进程已终止但清除 lease 失败"
                            );
                        }

                        super::super::dto::OrphanStopResultDto {
                            engine_id: engine_id_str,
                            stopped: true,
                            reason: "adoptable_killed".to_string(),
                            detail: Some(format!("进程 {} 已验证身份并终止", lease.pid)),
                        }
                    }
                    Err(e) => super::super::dto::OrphanStopResultDto {
                        engine_id: engine_id_str,
                        stopped: false,
                        reason: "kill_failed".to_string(),
                        detail: Some(format!("终止进程失败: {e}")),
                    },
                }
            }
            crate::infra::local_engine::lease::RecoveryDecision::DoNotAdopt(diag) => {
                let reason_str = match &diag.reason {
                    crate::infra::local_engine::lease::RecoveryReason::PidNotFound => {
                        // 进程已退出，清除 stale lease
                        if let Err(e) =
                            crate::infra::local_engine::lease::remove_lease_force(&engine_id_str)
                        {
                            tracing::warn!(
                                engine = %engine_id_str,
                                %e,
                                "PID 不存在但清除 lease 失败"
                            );
                        }
                        "pid_not_exist".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::ExecutableMismatch {
                        ..
                    } => "executable_mismatch".to_string(),
                    crate::infra::local_engine::lease::RecoveryReason::CreationTimeMismatch {
                        ..
                    } => "creation_time_mismatch".to_string(),
                    crate::infra::local_engine::lease::RecoveryReason::CreationTimeMissing => {
                        "creation_time_missing".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::ProcessQueryFailed => {
                        "process_query_failed".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::TokenFingerprintMismatch => {
                        "token_fingerprint_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::InstanceIdMismatch => {
                        "instance_id_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::EngineIdMismatch => {
                        "engine_id_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::HealthUnreachable => {
                        "health_unreachable".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::SchemaVersion { .. } => {
                        "schema_version_mismatch".to_string()
                    }
                };

                super::super::dto::OrphanStopResultDto {
                    engine_id: engine_id_str,
                    stopped: false,
                    reason: reason_str,
                    detail: Some(diag.detail.clone()),
                }
            }
        };

        Ok(result)
    }
}
