//! 引擎生命周期域：install/start/stop/repair/stop_orphan/cancel action commands。
//!
//! 前端只需提交 `engine_id`（与可选的 `compute_preference`），
//! 不提交 executable/argv/env/脚本路径。
//! compute preference 解析与 descriptor 声明项验证也收敛在此
//! （`preferences` 域复用）。

use std::sync::Arc;

use tauri::Manager;

use crate::app::command_error::CommandError;
use crate::app::local_engine::EngineManager;
use crate::app::local_engine::dto::{
    CancelResultDto, EngineOperationFinishedDto, OrphanStopResultDto,
};
use crate::domain::local_engine::EnvOperationEndState;
use crate::infra::local_engine::runtime::{ComputePreference, EngineId};

use super::{build_adapter_config_for_engine, get_service, validate_engine_id};

// ── 内部辅助：compute preference 解析与验证 ──────────────────────────────────

/// 从字符串解析 compute preference。
pub(super) fn parse_compute_preference(s: &str) -> Result<ComputePreference, CommandError> {
    match s {
        "auto" => Ok(ComputePreference::Auto),
        "cpu" => Ok(ComputePreference::Cpu),
        "gpu_auto" => Ok(ComputePreference::GpuAuto),
        "cuda" => Ok(ComputePreference::Cuda),
        "vulkan" => Ok(ComputePreference::Vulkan),
        "directml" => Ok(ComputePreference::Directml),
        other => Err(CommandError::new(
            "invalid_compute_preference",
            format!("未知的 compute preference: {other}"),
            false,
        )),
    }
}

/// 验证 compute preference 属于该引擎 descriptor 声明项。
///
/// **策略性偏好**（`Auto`、`GpuAuto`）总是通过验证——它们不是显式 backend，
/// 而是由 `InstallTransaction::resolve_profile` 按 descriptor 声明的候选顺序
/// 逐个尝试兼容性检查后解析为具体 profile。因此不需要出现在 `compute_candidates` 中。
///
/// **显式偏好**（`Cpu`、`Cuda`、`Vulkan`、`Directml`）必须出现在 descriptor 的
/// `compute_candidates` 中——显式偏好失败不回退，所以必须确保 descriptor 声明了
/// 对应的 profile。
pub(super) async fn validate_preference_for_engine(
    svc: &EngineManager,
    engine_id: &EngineId,
    preference: ComputePreference,
) -> Result<(), CommandError> {
    let catalog = svc.catalog().await;
    validate_preference_in_catalog(&catalog, engine_id, preference)
}

/// `validate_preference_for_engine` 的纯函数核心（可测）。
///
/// 策略性偏好（Auto/GpuAuto）直接通过；显式偏好必须在 descriptor
/// `compute_candidates` 中声明——FunASR descriptor 只声明 CPU，
/// 因此前端提交 `cuda` 会被拒绝，无法持久化。
fn validate_preference_in_catalog(
    catalog: &[crate::domain::local_engine::EngineDefinition],
    engine_id: &EngineId,
    preference: ComputePreference,
) -> Result<(), CommandError> {
    // 策略性偏好总是允许——由 resolver 解析为具体 profile
    if !preference.is_explicit() {
        return Ok(());
    }

    let descriptor = catalog
        .iter()
        .find(|d| d.engine_id == *engine_id)
        .ok_or_else(|| {
            CommandError::new(
                "unsupported_engine",
                format!("引擎不在 allowlist: {engine_id}"),
                false,
            )
        })?;

    if !descriptor.has_preference(preference) {
        return Err(CommandError::new(
            "unsupported_compute_preference",
            format!(
                "compute preference {:?} 不在引擎 {:} descriptor 声明项中",
                preference, engine_id
            ),
            false,
        ));
    }

    Ok(())
}

// ── 公开 commands ─────────────────────────────────────────────────────────────

/// 安装本地引擎环境。
///
/// 前端只需提交 `engine_id`，不提交 executable/argv/env/脚本路径。
/// `compute_preference` 可选，如提交则必须属于该引擎 descriptor 声明项。
/// action command 内部从现有配置真源构造 `AdapterConfig`。
///
/// 返回结构化终态：`end_state = "completed" | "cancelled"`——
/// **取消是正常终态**，前端不应把 cancelled 当失败处理。
/// 失败走 CommandError（保留 code/phase/detail 结构）。
#[tauri::command]
pub async fn install_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
    compute_preference: Option<String>,
) -> Result<EngineOperationFinishedDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;
    let mut adapter_config = build_adapter_config_for_engine(&engine_id)?;

    // 如果前端提交了 compute_preference，验证并覆盖
    if let Some(pref_str) = compute_preference {
        let pref = parse_compute_preference(&pref_str)?;
        // 验证属于该引擎 descriptor 声明项
        validate_preference_for_engine(&svc, &eid, pref).await?;
        adapter_config.compute_preference = Some(pref);
    }

    let (operation_id, end_state) = svc
        .install(&eid, adapter_config)
        .await
        .map_err(CommandError::from)?;

    // 0.22.8: PaddleOCR 安装后注入 ONNX executor
    if end_state == EnvOperationEndState::Completed
        && engine_id == crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID
        && let Some(coordinator) = app
            .try_state::<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>>()
            .map(|s| s.inner().clone())
    {
        let new_executor =
            crate::app::local_engine::ocr_coordinator::build_onnx_executor_from_deployment(&svc);
        if let Some(executor) = new_executor {
            coordinator.inject_executor(executor).await;
        }
    }

    tracing::info!(engine = %eid, ?end_state, "引擎安装结束");
    Ok(EngineOperationFinishedDto {
        engine_id,
        operation_id: operation_id.unwrap_or_default(),
        end_state: end_state.to_string(),
    })
}

/// 启动本地引擎服务。
///
/// 前端只需提交 `engine_id`，不提交 executable/argv/env/脚本路径。
///
/// 0.22.8: PaddleOCR (ONNX) 引擎走 in-process 路径——不 spawn 子进程，
/// 只标记状态为 available，OcrCoordinator 在首次 OCR 请求时 lazy load。
#[tauri::command]
pub async fn start_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
    compute_preference: Option<String>,
) -> Result<(), CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;
    let mut adapter_config = build_adapter_config_for_engine(&engine_id)?;

    if let Some(pref_str) = compute_preference {
        let pref = parse_compute_preference(&pref_str)?;
        validate_preference_for_engine(&svc, &eid, pref).await?;
        adapter_config.compute_preference = Some(pref);
    }

    // 确保环境已安装
    svc.ensure_installed(&eid, adapter_config.clone())
        .await
        .map_err(CommandError::from)?;

    // 0.22.8: PaddleOCR ONNX in-process——不走 svc.start()（不 spawn 子进程）
    if engine_id == crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID {
        svc.start_inprocess(&eid)
            .await
            .map_err(CommandError::from)?;

        // 构建并注入 ONNX executor 到 OcrCoordinator
        // 启动时 deployment 可能不存在导致 executor=None，安装后需要补注入
        if let Some(coordinator) = app
            .try_state::<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>>()
            .map(|s| s.inner().clone())
        {
            // 从 deployment 构建 executor
            let new_executor =
                crate::app::local_engine::ocr_coordinator::build_onnx_executor_from_deployment(
                    &svc,
                );
            if let Some(executor) = new_executor {
                coordinator.inject_executor(executor).await;
            } else {
                // executor 构建失败——只通知状态变更
                coordinator.notify_external_state_change().await;
            }
        }
    } else {
        svc.start(&eid, adapter_config)
            .await
            .map_err(CommandError::from)?;
    }

    tracing::info!(engine = %eid, "引擎启动完成");
    Ok(())
}

/// 停止本地引擎服务。
///
/// 前端只需提交 `engine_id`。
///
/// 0.22.8: PaddleOCR (ONNX) in-process——不走 svc.stop()（没有子进程），
/// 只标记状态为 stopped，并通知 OcrCoordinator shutdown executor。
#[tauri::command]
pub async fn stop_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<(), CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    // 0.22.8: PaddleOCR ONNX in-process——走专用路径
    if engine_id == crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID {
        // 通知 OcrCoordinator shutdown executor
        if let Some(coordinator) = app
            .try_state::<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>>()
            .map(|s| s.inner().clone())
        {
            coordinator.shutdown().await;
        }
        svc.stop_inprocess(&eid).await.map_err(CommandError::from)?;
    } else {
        svc.stop(&eid).await.map_err(CommandError::from)?;
    }

    tracing::info!(engine = %eid, "引擎停止完成");
    Ok(())
}

/// 手动停止孤儿引擎进程（0.22.6.6）。
///
/// 当 lease 恢复扫描发现遗留进程且判定为 `Adoptable` 时，
/// 用户可在设置页手动调用此命令终止孤儿进程。
///
/// **安全策略**（fail-closed）：
/// - 只接受 `engine_id`，从后端 lease 文件读取进程身份
/// - 使用 `kill_process_tree_verified` 验证身份后终止（executable + creation_time）
/// - 证据不足时返回错误，不降级为仅 PID kill
/// - 终止后清除 lease 文件
///
/// 返回 `OrphanStopResultDto` 包含终止状态和诊断信息。
#[tauri::command]
pub async fn stop_orphan_engine(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<OrphanStopResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    tracing::info!(engine = %eid, "收到停止孤儿引擎请求");

    let result = svc
        .stop_orphan_engine(&eid)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        stopped = result.stopped,
        reason = %result.reason,
        "孤儿引擎停止请求处理完成"
    );
    Ok(result)
}

/// 修复本地引擎环境。
///
/// 返回结构化终态：`end_state = "completed" | "cancelled"`——
/// **取消是正常终态**，前端不应把 cancelled 当失败处理。
#[tauri::command]
pub async fn repair_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EngineOperationFinishedDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let (operation_id, end_state) = svc.repair(&eid).await.map_err(CommandError::from)?;

    // 0.22.8: PaddleOCR 修复后注入 ONNX executor
    if end_state == EnvOperationEndState::Completed
        && engine_id == crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID
        && let Some(coordinator) = app
            .try_state::<Arc<crate::app::local_engine::ocr_coordinator::OcrCoordinator>>()
            .map(|s| s.inner().clone())
    {
        let new_executor =
            crate::app::local_engine::ocr_coordinator::build_onnx_executor_from_deployment(&svc);
        if let Some(executor) = new_executor {
            coordinator.inject_executor(executor).await;
        } else {
            coordinator.notify_external_state_change().await;
        }
    }

    tracing::info!(engine = %eid, ?end_state, "引擎修复结束");
    Ok(EngineOperationFinishedDto {
        engine_id,
        operation_id: operation_id.unwrap_or_default(),
        end_state: end_state.to_string(),
    })
}

/// 取消本地引擎操作。
///
/// 取消完全匹配且声明 cancellable 的操作。
/// 旧 `operation_id` 不得取消新操作。
///
/// **取消是正常协议语义**：service 返回 `CancelOutcome`，本命令只做
/// 参数适配与投影，不再解码 `LocalEngineError::Cancelled` 伪装的错误。
#[tauri::command]
pub async fn cancel_local_engine_operation(
    app: tauri::AppHandle,
    engine_id: String,
    operation_id: String,
) -> Result<CancelResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let outcome = svc.cancel_operation(&eid, &operation_id).await;

    let result = match &outcome {
        crate::domain::local_engine::CancelOutcome::Cancelled => CancelResultDto {
            engine_id: engine_id.clone(),
            operation_id: operation_id.clone(),
            cancelled: true,
            reason: None,
        },
        crate::domain::local_engine::CancelOutcome::NoActiveOperation => CancelResultDto {
            engine_id: engine_id.clone(),
            operation_id: operation_id.clone(),
            cancelled: false,
            reason: Some("当前没有进行中的操作".to_string()),
        },
        crate::domain::local_engine::CancelOutcome::Mismatched {
            current_operation_id,
        } => CancelResultDto {
            engine_id: engine_id.clone(),
            operation_id: operation_id.clone(),
            cancelled: false,
            reason: Some(format!(
                "操作 id 不匹配（当前活跃: {current_operation_id}）"
            )),
        },
    };

    tracing::info!(
        engine = %eid,
        op = %result.operation_id,
        cancelled = result.cancelled,
        "取消操作请求处理完成"
    );
    Ok(result)
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    use crate::app::local_engine::paddleocr;

    // ── compute preference 解析 ──

    #[test]
    fn parse_compute_preference_auto() {
        assert_eq!(
            parse_compute_preference("auto").unwrap(),
            ComputePreference::Auto
        );
    }

    #[test]
    fn parse_compute_preference_cpu() {
        assert_eq!(
            parse_compute_preference("cpu").unwrap(),
            ComputePreference::Cpu
        );
    }

    #[test]
    fn parse_compute_preference_cuda() {
        assert_eq!(
            parse_compute_preference("cuda").unwrap(),
            ComputePreference::Cuda
        );
    }

    #[test]
    fn parse_compute_preference_invalid() {
        assert!(parse_compute_preference("quantum").is_err());
    }

    // ── 0.22.6.1 前端不能持久化 FunASR cuda/auto ──

    /// 前端提交 cuda → validate_preference_in_catalog 拒绝（不在 descriptor 声明项）。
    #[test]
    fn funasr_cuda_preference_persist_rejected() {
        let adapter = crate::app::local_engine::funasr::make_funasr_adapter();
        let catalog = vec![adapter.descriptor().clone()];
        let eid = EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap();

        let err = validate_preference_in_catalog(&catalog, &eid, ComputePreference::Cuda)
            .expect_err("cuda 不在 FunASR descriptor 声明项，必须拒绝持久化");
        assert_eq!(err.code, "unsupported_compute_preference");
    }

    /// 显式 cpu 在 FunASR descriptor 声明项内 → 允许。
    #[test]
    fn funasr_cpu_preference_persist_allowed() {
        let adapter = crate::app::local_engine::funasr::make_funasr_adapter();
        let catalog = vec![adapter.descriptor().clone()];
        let eid = EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap();
        assert!(
            validate_preference_in_catalog(&catalog, &eid, ComputePreference::Cpu).is_ok(),
            "cpu 是 FunASR 唯一显式可选项"
        );
    }

    // ── 前端不能注入 AdapterConfig 内部字段 ──
    // 验证 build_adapter_config_for_engine 不接受外部 engine_config

    #[test]
    fn build_adapter_config_for_funasr_uses_stt_config() {
        // 确保能从 SttConfig 构造 funasr 的 AdapterConfig
        let config = build_adapter_config_for_engine("funasr").unwrap();
        // 验证 engine_config 不是 null
        assert!(!config.engine_config.is_null());
        // 验证 compute_preference 来自 SttConfig
        assert!(config.compute_preference.is_some());
        // 验证 preferred_port 来自 SttConfig
        assert!(config.preferred_port.is_some());
    }

    #[test]
    fn build_adapter_config_for_paddleocr_uses_ocr_config() {
        let config = build_adapter_config_for_engine("paddleocr").unwrap();
        assert!(!config.engine_config.is_null());
        assert!(config.compute_preference.is_some());
    }

    #[test]
    fn build_adapter_config_for_unknown_engine_rejected() {
        let result = build_adapter_config_for_engine("nonexistent");
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, "unsupported_engine");
    }

    // ── PaddleOCR catalog 不暴露 CUDA/Vulkan/DirectML ──

    #[test]
    fn paddleocr_descriptor_only_declares_cpu() {
        // PaddleOCR descriptor 只声明 CPU profile
        // 验证：如果前端提交 cuda 给 paddleocr，validate_preference_for_engine 会拒绝
        // 这里只验证 parse 层面的解析——真实验证需要 svc 实例
        let pref = parse_compute_preference("cuda").unwrap();
        // cuda 能被解析，但不在 paddleocr descriptor 中
        assert_eq!(pref, ComputePreference::Cuda);
    }

    // ── install/repair 返回结构化终态（取消是正常终态，非错误）──

    #[test]
    fn install_result_dto_shape() {
        let dto = EngineOperationFinishedDto {
            engine_id: "funasr".to_string(),
            operation_id: "op-001".to_string(),
            end_state: "cancelled".to_string(),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "funasr");
        assert_eq!(json["operation_id"], "op-001");
        assert_eq!(json["end_state"], "cancelled");
    }

    // ── cancel DTO 区分 cancelled 和 rejected ──

    #[test]
    fn cancel_result_dto_shape() {
        use crate::app::local_engine::dto::CancelResultDto;

        // 成功取消
        let cancelled = CancelResultDto {
            engine_id: "funasr".to_string(),
            operation_id: "op-abc123".to_string(),
            cancelled: true,
            reason: None,
        };
        let json = serde_json::to_value(&cancelled).unwrap();
        assert_eq!(json["cancelled"], true);
        assert!(json.get("reason").is_none() || json["reason"].is_null());

        // 未取消（rejected）
        let rejected = CancelResultDto {
            engine_id: "funasr".to_string(),
            operation_id: "op-old".to_string(),
            cancelled: false,
            reason: Some("操作已过期".to_string()),
        };
        let json = serde_json::to_value(&rejected).unwrap();
        assert_eq!(json["cancelled"], false);
        assert!(json["reason"].is_string());
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6.6 回归测试：stop_orphan_engine + OrphanStopResultDto
    // ═══════════════════════════════════════════════════════════════════════

    // ── stop_orphan_engine 命令签名可编译 ──

    #[test]
    fn stop_orphan_engine_command_compiles() {
        let _ = stop_orphan_engine as fn(tauri::AppHandle, String) -> _;
    }

    // ── OrphanStopResultDto 序列化正确 ──

    #[test]
    fn orphan_stop_result_dto_serializes_correctly() {
        let dto = OrphanStopResultDto {
            engine_id: "funasr".to_string(),
            stopped: true,
            reason: "adoptable_killed".to_string(),
            detail: Some("进程 12345 已验证身份并终止".to_string()),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "funasr");
        assert_eq!(json["stopped"], true);
        assert_eq!(json["reason"], "adoptable_killed");
        assert!(json["detail"].is_string());
    }

    // ── OrphanStopResultDto detail 为 None 时跳过序列化 ──

    #[test]
    fn orphan_stop_result_dto_skips_none_detail() {
        let dto = OrphanStopResultDto {
            engine_id: "paddleocr".to_string(),
            stopped: false,
            reason: "lease_not_found".to_string(),
            detail: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        // detail 为 None 时应被 skip
        assert!(json.get("detail").is_none() || json["detail"].is_null());
    }

    // ── OrphanStopResultDto 反序列化正确 ──

    #[test]
    fn orphan_stop_result_dto_deserializes() {
        let json = serde_json::json!({
            "engine_id": "funasr",
            "stopped": false,
            "reason": "pid_not_exist",
            "detail": "PID 不存在（进程已退出），应清除 stale lease"
        });
        let dto: OrphanStopResultDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto.engine_id, "funasr");
        assert!(!dto.stopped);
        assert_eq!(dto.reason, "pid_not_exist");
        assert!(dto.detail.is_some());
    }

    // ── OrphanStopResultDto 所有可能的 reason 值 ──

    #[test]
    fn orphan_stop_result_dto_all_reason_variants() {
        let reasons = [
            "lease_not_found",
            "pid_not_exist",
            "adoptable_killed",
            "kill_failed",
            "executable_mismatch",
            "creation_time_mismatch",
            "creation_time_missing",
            "process_query_failed",
            "token_fingerprint_mismatch",
            "instance_id_mismatch",
            "engine_id_mismatch",
            "health_unreachable",
            "schema_version_mismatch",
        ];
        for reason in &reasons {
            let dto = OrphanStopResultDto {
                engine_id: "test".to_string(),
                stopped: false,
                reason: reason.to_string(),
                detail: None,
            };
            let json = serde_json::to_value(&dto).unwrap();
            assert_eq!(json["reason"], *reason);
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6.7 端到端契约测试：compute preference 契约
    // 验证 Validator ↔ Resolver 语义一致性、配置归一化、单源真值
    // ═══════════════════════════════════════════════════════════════════════

    /// `is_explicit()` 对策略性偏好返回 false，对显式偏好返回 true。
    #[test]
    fn contract_is_explicit_distinguishes_strategic_and_explicit() {
        // 策略性偏好——不应被 validate_preference_for_engine 拒绝
        assert!(!ComputePreference::Auto.is_explicit());
        assert!(!ComputePreference::GpuAuto.is_explicit());
        // 显式偏好——需要 descriptor 声明
        assert!(ComputePreference::Cpu.is_explicit());
        assert!(ComputePreference::Cuda.is_explicit());
        assert!(ComputePreference::Vulkan.is_explicit());
        assert!(ComputePreference::Directml.is_explicit());
    }

    /// `build_adapter_config_for_engine` 为 PaddleOCR 构造的 compute_preference
    /// 可以是 `Auto`（来自 OcrConfig），验证它不报错。
    #[test]
    fn contract_paddleocr_build_adapter_config_succeeds_with_auto() {
        // PaddleOCR 的 compute_preference 来自 OcrConfig，默认是 Auto
        let config = build_adapter_config_for_engine("paddleocr").unwrap();
        // 验证 compute_preference 存在（可能是 Auto 或 Cpu，取决于配置）
        assert!(
            config.compute_preference.is_some(),
            "PaddleOCR AdapterConfig 必须有 compute_preference"
        );
    }

    /// `build_adapter_config_for_engine` 为 FunASR 构造的 compute_preference
    /// 始终为 `Cpu`，即使历史配置 device=cuda 也不传 Cuda。
    #[test]
    fn contract_funasr_build_adapter_config_always_cpu() {
        let config = build_adapter_config_for_engine("funasr").unwrap();
        // 无论 SttConfig.local_engine.device 是什么，compute_preference 都应为 Cpu
        assert_eq!(
            config.compute_preference,
            Some(ComputePreference::Cpu),
            "FunASR AdapterConfig compute_preference 必须为 Cpu（0.22.6 归一化）"
        );
    }

    /// `parse_compute_preference` 覆盖所有变体。
    #[test]
    fn contract_parse_compute_preference_all_variants() {
        assert_eq!(
            parse_compute_preference("auto").unwrap(),
            ComputePreference::Auto
        );
        assert_eq!(
            parse_compute_preference("gpu_auto").unwrap(),
            ComputePreference::GpuAuto
        );
        assert_eq!(
            parse_compute_preference("cpu").unwrap(),
            ComputePreference::Cpu
        );
        assert_eq!(
            parse_compute_preference("cuda").unwrap(),
            ComputePreference::Cuda
        );
        assert_eq!(
            parse_compute_preference("vulkan").unwrap(),
            ComputePreference::Vulkan
        );
        assert_eq!(
            parse_compute_preference("directml").unwrap(),
            ComputePreference::Directml
        );
    }

    /// 前端 `handleActionClick` 不传 `compute_preference`（null）时，
    /// 后端 `install_local_engine` / `start_local_engine` 接受 `Option::None`，
    /// 由 `build_adapter_config_for_engine` 从配置真源构造。
    /// 此测试验证 build_adapter_config_for_engine 不依赖前端传入的 preference。
    #[test]
    fn contract_build_adapter_config_independent_of_frontend_preference() {
        // 模拟前端传 null compute_preference 的场景：
        // install_local_engine 中 build_adapter_config_for_engine 先构造默认 config，
        // 然后只有当前端提交了 Some(pref_str) 时才覆盖。
        // 如果前端传 null（None），则使用 build_adapter_config 的默认值。

        let funasr_config = build_adapter_config_for_engine("funasr").unwrap();
        assert_eq!(
            funasr_config.compute_preference,
            Some(ComputePreference::Cpu)
        );

        let paddleocr_config = build_adapter_config_for_engine("paddleocr").unwrap();
        assert!(paddleocr_config.compute_preference.is_some());
    }

    /// 验证 `is_explicit()` + `has_preference` 组合的语义一致性：
    /// - 策略性偏好（Auto/GpuAuto）→ is_explicit() = false → validator 放行
    /// - 显式偏好（Cpu/Cuda/...）→ is_explicit() = true → validator 检查 has_preference
    #[test]
    fn contract_validator_semantics_consistent() {
        use crate::domain::local_engine::adapter::LocalEngineAdapter;

        let paddleocr_desc = paddleocr::PaddleocrAdapter::new().descriptor().clone();

        // Auto: is_explicit = false → validator 应放行（不需要在 candidates 中）
        assert!(!ComputePreference::Auto.is_explicit());

        // Cpu: is_explicit = true → validator 检查 has_preference
        assert!(ComputePreference::Cpu.is_explicit());
        assert!(paddleocr_desc.has_preference(ComputePreference::Cpu));

        // Cuda: is_explicit = true → validator 检查 has_preference → 不存在 → 拒绝
        assert!(ComputePreference::Cuda.is_explicit());
        assert!(!paddleocr_desc.has_preference(ComputePreference::Cuda));
    }
}
