//! 诊断查询域：状态/日志/诊断/运行时基础状态查询与打开目录命令。
//!
//! 只读查询为主（`open_*` 仅打开资源管理器目录），不安装、不启动。
//! 日志合并/去重/排序复用 mod.rs 的 `get_merged_logs` 单一真源。

use crate::app::command_error::CommandError;
use crate::app::local_engine::dto::{
    DiagnosticEntryDto, EngineDiagnosticsDto, EngineLogDto, EngineStatusDto, OrphanRecoveryDto,
    environment_health_to_string, project_diagnostics, project_process_state, project_status,
    service_health_to_string,
};

use super::{get_merged_logs, get_service, project_process_state_dto, validate_engine_id};

/// 获取本地引擎状态。
///
/// - 有 `engine_id` 时返回单引擎状态（数组含一项）
/// - 无 `engine_id` 时返回全部引擎状态
///
/// **只读查询，无副作用。** status query 与 `LOCAL_ENGINE_STATUS` event 使用同一 DTO shape。
#[tauri::command]
pub async fn get_local_engine_status(
    app: tauri::AppHandle,
    engine_id: Option<String>,
) -> Result<Vec<EngineStatusDto>, CommandError> {
    let svc = get_service(&app)?;

    match engine_id {
        Some(id) => {
            let eid = validate_engine_id(&id)?;
            let snapshot = svc.get_status(&eid).await.map_err(|e| {
                CommandError::new(
                    "engine_status_error",
                    format!("获取引擎状态失败: {e}"),
                    false,
                )
            })?;
            Ok(vec![project_status(&snapshot)])
        }
        None => {
            let snapshots = svc.get_all_status().await;
            Ok(snapshots.iter().map(project_status).collect())
        }
    }
}

/// 获取本地引擎日志历史。
///
/// 返回结构化日志 DTO，包含 `engine_id`、`instance_id`、`seq`、`timestamp`、`level`、`text`。
/// 历史与 `LOCAL_ENGINE_LOG` 实时事件使用同一 shape。
///
/// **只读查询，无副作用。**
///
/// 合并两个日志来源：
/// - **instance 日志**：来自 `ManagedProcess` ring buffer（`EngineManager::get_logs_structured`）
/// - **operation 日志**：来自 `OperationLogStore`（会话内回放，环境/模型安装日志）
///
/// 去重身份：`(source_kind, source_id, seq)`
/// - instance 日志：`("instance", instance_id, seq)`
/// - operation 日志：`("operation", operation_id, seq)`
///
/// 合并后按 timestamp 排序；timestamp 相同时用 `(source_kind, source_id, seq)` 做稳定 tie-break。
/// 最后统一执行 `max_lines` 截断。
#[tauri::command]
pub async fn get_local_engine_logs(
    app: tauri::AppHandle,
    engine_id: String,
    max_lines: Option<usize>,
) -> Result<Vec<EngineLogDto>, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;
    let max = max_lines.unwrap_or(500);

    // 合并/去重/排序/截断逻辑单一真源在 `get_merged_logs`——
    // 与 `get_engine_diagnostics` 共用，不再复制第二份。
    get_merged_logs(&app, &svc, &eid, max).await
}

// ── 只读运行时诊断与打开目录命令（0.22.6 H4）─────────────────────────────────

/// 获取运行时基础状态（只读）。
///
/// 返回 Python/uv 基础环境状态、所有引擎的汇总概览。
/// **只读查询，不安装、不启动、不修改任何状态。**
#[tauri::command]
pub async fn get_runtime_foundation_status(
    app: tauri::AppHandle,
) -> Result<serde_json::Value, CommandError> {
    let svc = get_service(&app)?;

    let catalog = svc.catalog().await;
    let mut engines = Vec::with_capacity(catalog.len());

    for descriptor in &catalog {
        let engine_id_str = descriptor.engine_id.to_string();
        let snapshot = svc.get_status(&descriptor.engine_id).await.map_err(|e| {
            CommandError::new(
                "engine_status_error",
                format!("获取引擎状态失败: {e}"),
                false,
            )
        })?;

        // 状态投影复用 dto 真源函数——不在 command 层复制状态推断规则
        engines.push(serde_json::json!({
            "engine_id": engine_id_str,
            "display_name": descriptor.display,
            "environment": environment_health_to_string(snapshot.status.environment.clone()),
            "process": project_process_state(&snapshot.status.process),
            "service": service_health_to_string(snapshot.status.service),
        }));
    }

    Ok(serde_json::json!({
        "engines": engines,
        "python_provider": "python_venv",
    }))
}

/// 获取引擎诊断信息（只读）。
///
/// 返回单个引擎的详细诊断：环境健康、进程状态、服务状态、最近日志。
/// **只读查询，不安装、不启动。** 不下载音频、不执行实际转写。
///
/// 调用 `EngineManager::get_status()` + `get_diagnostics()` + 双源日志查询 + orphan recovery，
/// 返回闭合 `EngineDiagnosticsDto`，不再使用 `json!` 手拼。
#[tauri::command]
pub async fn get_engine_diagnostics(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EngineDiagnosticsDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    // ── 1. 状态快照 ──
    let snapshot = svc.get_status(&eid).await.map_err(|e| {
        CommandError::new(
            "engine_status_error",
            format!("获取引擎状态失败: {e}"),
            false,
        )
    })?;

    // ── 2. adapter 专属诊断 ──
    let adapter_diag = svc.get_diagnostics(&eid).await.map_err(|e| {
        CommandError::new(
            "engine_diagnostics_error",
            format!("获取诊断失败: {e}"),
            false,
        )
    })?;
    let adapter_diagnostics: Vec<DiagnosticEntryDto> = project_diagnostics(&adapter_diag);

    // ── 3. 双源日志查询（instance + operation）──
    // 使用与 get_local_engine_logs 相同的合并逻辑（get_merged_logs 单一真源），
    // 但固定 50 行上限。诊断视图容忍日志读取失败（warn + 空列表），
    // 不掩盖其余诊断信息。
    let recent_logs = match get_merged_logs(&app, &svc, &eid, 50).await {
        Ok(logs) => logs,
        Err(e) => {
            tracing::warn!(engine_id = %eid, error = %e, "诊断视图获取日志失败");
            Vec::new()
        }
    };

    // ── 4. orphan recovery ──
    let orphan_recovery = scan_orphan_recovery(&eid).await;

    // ── 5. 投影为闭合 DTO ──
    Ok(EngineDiagnosticsDto {
        engine_id,
        environment: environment_health_to_string(snapshot.status.environment.clone()),
        process: project_process_state_dto(&snapshot.status.process),
        service: service_health_to_string(snapshot.status.service),
        adapter_diagnostics,
        recent_logs,
        orphan_recovery,
    })
}

/// 扫描引擎的孤儿进程恢复状态，返回闭合 DTO。
///
/// 不暴露 PID、路径、token、endpoint 等敏感字段。
async fn scan_orphan_recovery(
    engine_id: &crate::infra::local_engine::runtime::EngineId,
) -> OrphanRecoveryDto {
    let engine_id_str = engine_id.to_string();

    // 扫描 lease 文件
    let leases = match tokio::task::spawn_blocking(|| {
        crate::infra::local_engine::lease::scan_leases()
    })
    .await
    {
        Ok(l) => l,
        Err(_) => {
            return OrphanRecoveryDto {
                present: false,
                actionable: false,
                reason: "scan_failed".to_string(),
            };
        }
    };

    let lease = leases.iter().find(|l| l.engine_id == engine_id_str);
    let Some(lease) = lease else {
        return OrphanRecoveryDto {
            present: false,
            actionable: false,
            reason: "no_lease".to_string(),
        };
    };

    let lease = lease.clone();
    let pid = lease.pid;

    // 构建进程证据
    let process_evidence = match tokio::task::spawn_blocking(move || {
        crate::infra::local_engine::lease_recovery::build_process_evidence(pid)
    })
    .await
    {
        Ok(evidence) => evidence,
        Err(_) => {
            return OrphanRecoveryDto {
                present: false,
                actionable: false,
                reason: "process_query_failed".to_string(),
            };
        }
    };

    // 探测 health 端点
    let health_evidence =
        crate::infra::local_engine::lease_recovery::probe_health_evidence(&lease.endpoint).await;

    // 调用 decide_recovery 做恢复判定
    let decision = crate::infra::local_engine::lease::decide_recovery(
        &lease,
        &process_evidence,
        health_evidence.as_ref(),
    );

    match &decision {
        crate::infra::local_engine::lease::RecoveryDecision::Adoptable { .. } => {
            OrphanRecoveryDto {
                present: true,
                actionable: true,
                reason: "adoptable".to_string(),
            }
        }
        crate::infra::local_engine::lease::RecoveryDecision::DoNotAdopt(diag) => {
            let reason_str = match &diag.reason {
                crate::infra::local_engine::lease::RecoveryReason::PidNotFound => "pid_not_exist",
                crate::infra::local_engine::lease::RecoveryReason::ExecutableMismatch {
                    ..
                } => "executable_mismatch",
                crate::infra::local_engine::lease::RecoveryReason::CreationTimeMismatch {
                    ..
                } => "creation_time_mismatch",
                crate::infra::local_engine::lease::RecoveryReason::CreationTimeMissing => {
                    "creation_time_missing"
                }
                crate::infra::local_engine::lease::RecoveryReason::ProcessQueryFailed => {
                    "process_query_failed"
                }
                crate::infra::local_engine::lease::RecoveryReason::TokenFingerprintMismatch => {
                    "token_fingerprint_mismatch"
                }
                crate::infra::local_engine::lease::RecoveryReason::InstanceIdMismatch => {
                    "instance_id_mismatch"
                }
                crate::infra::local_engine::lease::RecoveryReason::EngineIdMismatch => {
                    "engine_id_mismatch"
                }
                crate::infra::local_engine::lease::RecoveryReason::HealthUnreachable => {
                    "health_unreachable"
                }
                crate::infra::local_engine::lease::RecoveryReason::SchemaVersion { .. } => {
                    "schema_version_mismatch"
                }
            };
            OrphanRecoveryDto {
                present: true,
                actionable: false,
                reason: reason_str.to_string(),
            }
        }
    }
}

/// 打开引擎数据目录（在资源管理器中打开）。
///
/// 打开 `engines/{engine_id}` 目录，包含 venv、模型缓存等。
#[tauri::command]
pub async fn open_engine_folder(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<(), CommandError> {
    let _svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let dir = crate::infra::local_engine::runtime::engine_root(&eid);

    if !dir.exists() {
        return Err(CommandError::new(
            "folder_not_found",
            format!("引擎目录不存在: {}", dir.display()),
            false,
        ));
    }

    // 使用 Windows ShellExecute 打开目录
    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(&dir)
            .spawn()
            .map_err(|e| {
                CommandError::new("open_folder_failed", format!("打开目录失败: {e}"), false)
            })?;
    }

    tracing::info!(engine = %eid, dir = %dir.display(), "已打开引擎目录");
    Ok(())
}

/// 打开运行时基础目录（在资源管理器中打开）。
///
/// 打开 `engines/` 根目录，包含所有引擎子目录。
#[tauri::command]
pub async fn open_runtime_folder(_app: tauri::AppHandle) -> Result<(), CommandError> {
    let dir = crate::infra::local_engine::runtime::runtimes_root().join("engines");

    if !dir.exists() {
        std::fs::create_dir_all(&dir).map_err(|e| {
            CommandError::new("create_folder_failed", format!("创建目录失败: {e}"), false)
        })?;
    }

    #[cfg(windows)]
    {
        std::process::Command::new("explorer.exe")
            .arg(&dir)
            .spawn()
            .map_err(|e| {
                CommandError::new("open_folder_failed", format!("打开目录失败: {e}"), false)
            })?;
    }

    tracing::info!(dir = %dir.display(), "已打开运行时基础目录");
    Ok(())
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {

    // ── 日志 DTO shape 包含 instance_id + seq ──

    #[test]
    fn log_dto_has_instance_id_and_seq() {
        use crate::app::local_engine::dto::EngineLogLevel;

        use crate::app::local_engine::dto::EngineLogDto;
        let dto = EngineLogDto {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc12345".to_string(),
            operation_id: None,
            seq: "42".to_string(),
            timestamp: "2026-08-26T00:00:00Z".to_string(),
            level: EngineLogLevel::Info,
            text: "test log line".to_string(),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json.get("instance_id").is_some());
        assert!(json.get("seq").is_some());
        assert!(json.get("engine_id").is_some());
        assert!(json.get("timestamp").is_some());
        assert!(json.get("level").is_some());
        assert!(json.get("text").is_some());
    }
}
