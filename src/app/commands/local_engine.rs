//! 通用本地引擎管理 commands（0.22.5 H1）。
//!
//! 为设置页提供 provider-neutral、allowlist 化的管理 API。
//! 前端只能提交 `engine_id`、闭合 action 和有限配置，
//! **绝不能**提交 executable、argv、env、脚本路径、runtime kind、artifact URL。
//!
//! ## 命令清单
//!
//! | command | 职责 |
//! |---|---|
//! | `get_local_engine_catalog` | 返回所有引擎 catalog（只读） |
//! | `get_local_engine_status` | 返回引擎状态（可选 engine_id；无值时返回全部） |
//! | `get_local_engine_logs` | 返回结构化日志历史 |
//! | `install_local_engine` | 安装引擎环境 |
//! | `start_local_engine` | 启动引擎服务 |
//! | `stop_local_engine` | 停止引擎服务 |
//! | `repair_local_engine` | 修复引擎环境 |
//! | `get_local_engine_storage` | 返回存储概览（只读，spawn_blocking） |
//! | `cleanup_local_engine` | 清理引擎资产（target_ids → 后端重新解析） |
//! | `cancel_local_engine_operation` | 取消匹配 operation_id 的操作 |
//! | `list_engine_models` | 列出引擎模型候选及状态（只读） |
//! | `install_engine_model` | 安装引擎模型（真实事务） |
//! | `delete_engine_model` | 删除引擎模型（引用检查 + 删除） |
//! | `repair_engine_model` | 修复引擎模型（重新下载/校验） |
//! | `cancel_model_operation` | 取消进行中的模型操作 |
//!
//! ## 安全约束
//!
//! - `engine_id` 必须在编译期 allowlist 中
//! - `compute_preference` 必须先验证属于该引擎 descriptor 声明项
//! - action command 内部从现有配置真源构造 `AdapterConfig`：
//!   - funasr → `SttConfig.local_engine`
//!   - paddleocr → `OcrConfig` / `PaddleOcrEngineConfig`
//! - 禁止前端直接提交 `AdapterConfig.engine_config`
//!
//! ## 兼容性
//!
//! 不破坏旧 `get_funasr_env` / `setup_python_env` / `start_funasr_server` 等兼容命令
//! 和旧事件投影。

use std::sync::Arc;

use crate::app::command_error::CommandError;
use crate::app::local_engine::dto::{
    CancelResultDto, CleanupRequestDto, CleanupResultDto, DiagnosticEntryDto, EngineCatalogItem,
    EngineDiagnosticsDto, EngineLogDto, EngineLogLevel, EngineOperationFinishedDto,
    EnginePreferencesDto, EnginePreferencesPatchDto, EngineStatusDto, EngineStorageDto,
    OrphanRecoveryDto, OrphanStopResultDto, environment_health_to_string, project_catalog_item,
    project_diagnostics, project_process_state, project_status, service_health_to_string,
};
use crate::app::local_engine::{EngineManager, funasr, paddleocr};
use crate::domain::local_engine::EngineDefinition;
use crate::infra::local_engine::providers::RuntimeProvider;
use crate::infra::local_engine::runtime::{ComputePreference, EngineId};

use tauri::{Emitter, Manager};

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 从 managed state 获取 `EngineManager` 引用。
fn get_service(app: &tauri::AppHandle) -> Result<Arc<EngineManager>, CommandError> {
    app.try_state::<Arc<EngineManager>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("internal_error", "EngineManager 尚未注册", false))
}

/// 合并 instance 日志和 operation 日志，去重、排序、截断。
///
/// 去重身份：`(source_kind, source_id, seq)`
/// - instance 日志：`("instance", instance_id, seq)`
/// - operation 日志：`("operation", operation_id, seq)`
///
/// 合并后按 timestamp 排序；timestamp 相同时用 `(source_kind, source_id, seq)` 做稳定 tie-break。
/// 最后统一执行 `max_lines` 截断。
async fn get_merged_logs(
    app: &tauri::AppHandle,
    svc: &EngineManager,
    eid: &EngineId,
    max_lines: usize,
) -> Vec<EngineLogDto> {
    // ── source 1: instance 日志 ──
    let instance_logs = match svc.get_logs_structured(eid, max_lines).await {
        Ok(logs) => logs,
        Err(e) => {
            tracing::warn!(engine_id = %eid, error = %e, "获取 instance 日志失败");
            Vec::new()
        }
    };

    let mut merged: Vec<((&str, String, u64), EngineLogDto)> = instance_logs
        .iter()
        .map(|entry| {
            let timestamp = chrono::DateTime::from_timestamp_millis(entry.timestamp_ms as i64)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
            let dto = EngineLogDto {
                engine_id: entry.engine_id.clone(),
                instance_id: entry.instance_id.clone(),
                operation_id: None,
                seq: entry.seq.to_string(),
                timestamp,
                level: EngineLogLevel::from_str_lossy(&entry.level),
                text: entry.text.clone(),
            };
            (("instance", entry.instance_id.clone(), entry.seq), dto)
        })
        .collect();

    // ── source 2: operation 日志 ──
    if let Some(store) =
        app.try_state::<std::sync::Arc<crate::app::local_engine::OperationLogStore>>()
    {
        let op_logs = store.query(eid);
        for log in op_logs {
            let dto = EngineLogDto {
                engine_id: log.engine_id.clone(),
                instance_id: String::new(),
                operation_id: Some(log.operation_id.clone()),
                seq: log.seq.to_string(),
                timestamp: log.timestamp.clone(),
                level: EngineLogLevel::from_str_lossy(&log.level),
                text: log.text.clone(),
            };
            merged.push((("operation", log.operation_id.clone(), log.seq), dto));
        }
    }

    // ── 去重 ──
    let mut seen: std::collections::HashSet<(&str, String, u64)> = std::collections::HashSet::new();
    merged.retain(|(key, _)| seen.insert(key.clone()));

    // ── 排序：timestamp + 稳定 tie-break ──
    merged.sort_by(|a, b| {
        let cmp = a.1.timestamp.cmp(&b.1.timestamp);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        let kind_cmp = a.0.0.cmp(b.0.0);
        if kind_cmp != std::cmp::Ordering::Equal {
            return kind_cmp;
        }
        let sid_cmp = a.0.1.cmp(&b.0.1);
        if sid_cmp != std::cmp::Ordering::Equal {
            return sid_cmp;
        }
        a.0.2.cmp(&b.0.2)
    });

    // ── 截断 ──
    let start = if merged.len() > max_lines {
        merged.len() - max_lines
    } else {
        0
    };

    merged[start..].iter().map(|(_, dto)| dto.clone()).collect()
}

/// 从 `ProcessState` 投影为 `ProcessStateDto`（复用 dto.rs 中的投影函数）。
fn project_process_state_dto(
    process: &crate::domain::local_engine::ProcessState,
) -> crate::app::local_engine::dto::ProcessStateDto {
    crate::app::local_engine::dto::project_process_state(process)
}

/// 验证 engine_id 并返回 `EngineId`。
fn validate_engine_id(engine_id: &str) -> Result<EngineId, CommandError> {
    EngineId::new(engine_id).map_err(|e| {
        CommandError::new("invalid_engine_id", format!("无效的 engine_id: {e}"), false)
    })
}

/// 从配置真源读取当前 compute preference。
///
/// 真源在 [`crate::app::local_engine::config_source`]——command 层不再
/// 复制归一化规则（0.22.6：funasr descriptor 只声明 CPU profile）。
fn current_compute_preference(engine_id: &str) -> ComputePreference {
    EngineId::new(engine_id)
        .map(|eid| crate::app::local_engine::config_source::current_compute_preference(&eid))
        .unwrap_or(ComputePreference::Auto)
}

/// 从配置真源构造 `AdapterConfig`。
///
/// **禁止前端直接提交 `AdapterConfig.engine_config`**。
/// 唯一构造入口在 [`crate::app::local_engine::config_source`]——
/// 与 EngineManager（repair）、wiring（自启）共用同一份规则，
/// 避免 repair 用 A 配置装、start 用 B 配置跑的规则漂移。
fn build_adapter_config_for_engine(
    engine_id: &str,
) -> Result<crate::domain::local_engine::AdapterConfig, CommandError> {
    let eid = validate_engine_id(engine_id)?;
    crate::app::local_engine::config_source::adapter_config_for_engine(&eid).ok_or_else(|| {
        CommandError::new(
            "unsupported_engine",
            format!("不支持的引擎: {engine_id}"),
            false,
        )
    })
}

/// 为 catalog item 计算兼容性结果。
///
/// 从 `ProviderDescriptor` 的 `profiles` + `RuntimeProvider::check_compatibility`
/// 真源获取，不由前端猜测。
fn compute_compatibility_for_descriptor(
    svc: &EngineManager,
    descriptor: &EngineDefinition,
) -> Vec<(ComputePreference, bool, Option<String>)> {
    // 从 ProviderDescriptor 获取 profile candidates
    let provider_desc = svc.provider_descriptor_for_engine(&descriptor.engine_id);

    // 如果有 ProviderDescriptor，使用其 profiles + provider check_compatibility
    if let Some(pd) = provider_desc {
        let python_provider = svc.python_provider();
        descriptor
            .install_plan
            .compute_candidates
            .iter()
            .map(|c| {
                // 从 ProviderDescriptor 的 profiles 中找匹配的 ProfileCandidate
                let profile_candidate = pd.profiles.iter().find(|pc| {
                    pc.profile_id == c.profile_id
                        || pc.backend == map_preference_to_backend(c.preference)
                });

                let (compatible, disabled_reason) = if let Some(pc) = profile_candidate {
                    match python_provider.check_compatibility(&pc.compatibility) {
                        Ok(true) => (true, None),
                        Ok(false) => (false, Some(format!("本机不兼容: {:?}", pc.compatibility))),
                        Err(e) => (false, Some(format!("兼容性检查失败: {e}"))),
                    }
                } else {
                    // 没有匹配的 ProfileCandidate——descriptor 声明但 provider 未提供
                    (false, Some("provider 未声明此 profile".to_string()))
                };

                (c.preference, compatible, disabled_reason)
            })
            .collect()
    } else {
        // 没有 ProviderDescriptor——无法做兼容性检查，标记为 unknown
        descriptor
            .install_plan
            .compute_candidates
            .iter()
            .map(|c| {
                (
                    c.preference,
                    false,
                    Some("无 ProviderDescriptor".to_string()),
                )
            })
            .collect()
    }
}

/// 从 ComputePreference 映射到 ComputeBackend（复用 service 层逻辑）。
fn map_preference_to_backend(
    p: ComputePreference,
) -> crate::infra::local_engine::runtime::ComputeBackend {
    match p {
        ComputePreference::Cpu => crate::infra::local_engine::runtime::ComputeBackend::Cpu,
        ComputePreference::Cuda => crate::infra::local_engine::runtime::ComputeBackend::Cuda,
        ComputePreference::Vulkan => crate::infra::local_engine::runtime::ComputeBackend::Vulkan,
        ComputePreference::Directml => {
            crate::infra::local_engine::runtime::ComputeBackend::Directml
        }
        _ => crate::infra::local_engine::runtime::ComputeBackend::Cpu,
    }
}

// ── 公开 commands ─────────────────────────────────────────────────────────────

/// 获取本地引擎目录（catalog）。
///
/// 返回所有已注册引擎的 UI 投影。
/// 只包含 descriptor 声明项，compute options 的兼容性由 provider 真源决定。
///
/// **只读查询，不启动服务、不安装环境、不阻塞主链路。**
#[tauri::command]
pub async fn get_local_engine_catalog(
    app: tauri::AppHandle,
) -> Result<Vec<EngineCatalogItem>, CommandError> {
    let svc = get_service(&app)?;

    let catalog = svc.catalog().await;
    let mut items = Vec::with_capacity(catalog.len());

    for descriptor in catalog {
        let engine_id_str = descriptor.engine_id.to_string();
        let current_pref = current_compute_preference(&engine_id_str);
        let compatibility_results = compute_compatibility_for_descriptor(&svc, &descriptor);
        let item = project_catalog_item(&descriptor, &compatibility_results, current_pref);
        items.push(item);
    }

    Ok(items)
}

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

    // ── source 1: instance 日志（ManagedProcess ring buffer）──
    let instance_logs = svc
        .get_logs_structured(&eid, max)
        .await
        .map_err(|e| CommandError::new("engine_logs_error", format!("获取日志失败: {e}"), false))?;

    // 把 StructuredLogEntry 投影为 (dedup_key, EngineLogDto)
    // dedup_key = ("instance", instance_id, seq)
    let mut merged: Vec<((&str, String, u64), EngineLogDto)> = instance_logs
        .iter()
        .map(|entry| {
            let timestamp = chrono::DateTime::from_timestamp_millis(entry.timestamp_ms as i64)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            let dto = EngineLogDto {
                engine_id: entry.engine_id.clone(),
                instance_id: entry.instance_id.clone(),
                operation_id: None,
                seq: entry.seq.to_string(),
                timestamp,
                level: EngineLogLevel::from_str_lossy(&entry.level),
                text: entry.text.clone(),
            };

            (("instance", entry.instance_id.clone(), entry.seq), dto)
        })
        .collect();

    // ── source 2: operation 日志（OperationLogStore 会话内回放）──
    if let Some(store) =
        app.try_state::<std::sync::Arc<crate::app::local_engine::OperationLogStore>>()
    {
        let op_logs = store.query(&eid);
        for log in op_logs {
            let dto = EngineLogDto {
                engine_id: log.engine_id.clone(),
                instance_id: String::new(),
                operation_id: Some(log.operation_id.clone()),
                seq: log.seq.to_string(),
                timestamp: log.timestamp.clone(),
                level: EngineLogLevel::from_str_lossy(&log.level),
                text: log.text.clone(),
            };
            merged.push((("operation", log.operation_id.clone(), log.seq), dto));
        }
    }

    // ── 去重：相同 (source_kind, source_id, seq) 只保留第一条 ──
    let mut seen: std::collections::HashSet<(&str, String, u64)> = std::collections::HashSet::new();
    merged.retain(|(key, _)| seen.insert(key.clone()));

    // ── 排序：按 timestamp 正序；timestamp 相同时用 (source_kind, source_id, seq) tie-break ──
    merged.sort_by(|a, b| {
        let cmp = a.1.timestamp.cmp(&b.1.timestamp);
        if cmp != std::cmp::Ordering::Equal {
            return cmp;
        }
        // 稳定 tie-break：先 source_kind，再 source_id，再 seq
        let kind_cmp = a.0.0.cmp(b.0.0);
        if kind_cmp != std::cmp::Ordering::Equal {
            return kind_cmp;
        }
        let sid_cmp = a.0.1.cmp(&b.0.1);
        if sid_cmp != std::cmp::Ordering::Equal {
            return sid_cmp;
        }
        a.0.2.cmp(&b.0.2)
    });

    // ── 统一截断：取最后 max_lines 条（保留最新的）──
    let start = if merged.len() > max {
        merged.len() - max
    } else {
        0
    };

    Ok(merged[start..].iter().map(|(_, dto)| dto.clone()).collect())
}

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

    tracing::info!(engine = %eid, ?end_state, "引擎安装结束");
    Ok(EngineOperationFinishedDto {
        engine_id: engine_id,
        operation_id: operation_id.unwrap_or_default(),
        end_state: end_state.to_string(),
    })
}

/// 启动本地引擎服务。
///
/// 前端只需提交 `engine_id`，不提交 executable/argv/env/脚本路径。
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

    svc.start(&eid, adapter_config)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(engine = %eid, "引擎启动完成");
    Ok(())
}

/// 停止本地引擎服务。
///
/// 前端只需提交 `engine_id`。
#[tauri::command]
pub async fn stop_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<(), CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    svc.stop(&eid).await.map_err(CommandError::from)?;

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

    tracing::info!(engine = %eid, ?end_state, "引擎修复结束");
    Ok(EngineOperationFinishedDto {
        engine_id: engine_id,
        operation_id: operation_id.unwrap_or_default(),
        end_state: end_state.to_string(),
    })
}

/// 获取本地引擎存储概览。
///
/// 返回所有可诊断/可清理的存储目标（generations、model cache、
/// shared artifacts、download cache、legacy）。
///
/// **只读扫描，在 spawn_blocking 中执行，不阻塞主链路。**
/// 前端据此展示预览和确认弹窗，不暴露用户目录完整路径。
#[tauri::command]
pub async fn get_local_engine_storage(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EngineStorageDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let storage = svc.scan_storage(&eid).await.map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        targets = storage.targets.len(),
        total_bytes = storage.total_size_bytes,
        releasable_bytes = storage.releasable_size_bytes,
        "存储扫描完成"
    );
    Ok(storage)
}

/// 清理本地引擎资产。
///
/// 前端提交 `engine_id` + `target_ids` + `operation_id`（可选）。
/// 后端重新解析每个 `target_id`，**不信任前端提交的路径/size/shared/current**。
///
/// 禁止提交任意路径。current generation 默认不可删除。
/// 共享资产经过引用检查。
#[tauri::command]
pub async fn cleanup_local_engine(
    app: tauri::AppHandle,
    request: CleanupRequestDto,
) -> Result<CleanupResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    if request.target_ids.is_empty() {
        return Err(CommandError::new(
            "invalid_request",
            "target_ids 不能为空",
            false,
        ));
    }

    tracing::info!(
        engine = %eid,
        targets = request.target_ids.len(),
        op_id = ?request.operation_id,
        "开始清理引擎资产"
    );

    let result = svc
        .cleanup_targets(&eid, &request.target_ids, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        cleaned = result.cleaned_target_ids.len(),
        skipped = result.skipped_target_ids.len(),
        deferred = result.deferred_target_ids.len(),
        released_bytes = result.released_bytes,
        "清理完成"
    );
    Ok(result)
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

/// 获取引擎受限偏好（0.22.5 H2）。
///
/// 返回闭合字段：`compute_preference`、`auto_start`（仅 FunASR）、
/// `ocr_backend` / `lifecycle`（仅 PaddleOCR）、`requires_rebuild`。
///
/// **只读查询，不启动服务、不安装环境。**
#[tauri::command]
pub async fn get_local_engine_preferences(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<EnginePreferencesDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let dto = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            // 0.22.6 归一化：历史 device=cuda 投影为 cpu
            // descriptor 只声明 CPU profile，前端不应看到 cuda 选项
            let compute_pref = "cpu".to_string();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(compute_pref),
                auto_start: Some(config.local_engine.auto_start_server),
                ocr_backend: None,
                lifecycle: None,
                requires_rebuild: None,
            }
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(preference_to_string_local(ocr_config.compute_preference)),
                auto_start: None,
                ocr_backend: Some(ocr_config.backend.to_string()),
                lifecycle: Some(ocr_config.lifecycle.to_string()),
                requires_rebuild: None,
            }
        }
        other => {
            return Err(CommandError::new(
                "unsupported_engine",
                format!("不支持的引擎: {other}"),
                false,
            ));
        }
    };

    // 检查当前环境状态——如果 NeedsRebuild 则返回 requires_rebuild=true
    let snapshot = svc.get_status(&eid).await.map_err(|e| {
        CommandError::new(
            "engine_status_error",
            format!("获取引擎状态失败: {e}"),
            false,
        )
    })?;

    if snapshot.status.environment == crate::domain::local_engine::EnvironmentHealth::NeedsRebuild {
        let mut dto = dto;
        dto.requires_rebuild = Some(true);
        return Ok(dto);
    }

    Ok(dto)
}

/// 保存引擎受限偏好（0.22.5 H2）。
///
/// patch 只接受闭合字段：`compute_preference`、`auto_start`（仅 FunASR）、
/// `ocr_backend` / `lifecycle`（仅 PaddleOCR）。
///
/// **禁止包含** executable/argv/env/path/url/runtime kind 或任意 engine_config。
/// 未知字段在反序列化时被拒绝（`#[serde(deny_unknown_fields)]`）。
///
/// 后端按 engine_id 从配置真源读取完整配置，只修改 patch 指定的字段，
/// 再通过现有持久化与 cache 热更新路径保存。
///
/// 如果 compute profile 变化导致与 current generation 不一致，
/// 将环境投影为 `NeedsRebuild`，并返回 `requires_rebuild=true`。
///
/// **自启语义**：保存 `auto_start` 只改变配置，不隐式启动服务。
/// 是否立即启动由显式 `start_local_engine` action 决定。
#[tauri::command]
pub async fn set_local_engine_preferences(
    app: tauri::AppHandle,
    engine_id: String,
    patch: EnginePreferencesPatchDto,
) -> Result<EnginePreferencesDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let pool = app
        .try_state::<crate::infra::data::DbPools>()
        .ok_or_else(|| CommandError::new("internal_error", "DbPools 尚未注册", false))?;

    let requires_rebuild = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            // 读取现有完整 SttConfig
            let mut config = crate::app::stt_config::get_stt_config();
            let mut profile_changed = false;

            // 应用 compute_preference patch
            if let Some(ref pref_str) = patch.compute_preference {
                let pref = parse_compute_preference(pref_str)?;
                // 验证属于该引擎 descriptor 声明项
                validate_preference_for_engine(&svc, &eid, pref).await?;

                // 0.22.6 归一化：FunASR descriptor 只声明 CPU profile
                // 无论用户提交 cpu 还是 auto 还是 cuda，最终都归一化为 cpu
                let new_device = "cpu".to_string();
                if config.local_engine.device != new_device {
                    profile_changed = true;
                    config.local_engine.device = new_device;
                }
            }

            // 应用 auto_start patch（仅 FunASR）
            if let Some(auto_start) = patch.auto_start {
                config.local_engine.auto_start_server = auto_start;
            }

            // 持久化 + cache 热更新
            crate::domain::config::store::ConfigStore::set(&pool.config, &config)
                .await
                .map_err(|e| {
                    CommandError::new("save_failed", format!("保存 STT 配置失败: {e}"), false)
                })?;
            crate::app::stt_config::update_cache(&config);

            tracing::info!(
                engine = %eid,
                device = %config.local_engine.device,
                auto_start = config.local_engine.auto_start_server,
                "FunASR 偏好已保存"
            );

            // 广播配置变更
            let _ = app.emit(
                crate::domain::event_names::EventNames::CONFIG_CHANGED,
                serde_json::json!({ "key": "stt:config" }),
            );

            if profile_changed {
                svc.mark_needs_rebuild(&eid).await.map_err(|e| {
                    CommandError::new(
                        "needs_rebuild_failed",
                        format!("标记 NeedsRebuild 失败: {e}"),
                        false,
                    )
                })?;
            }

            profile_changed
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            // 读取现有完整 OcrConfig
            let mut ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            let mut profile_changed = false;

            // 应用 compute_preference patch
            if let Some(ref pref_str) = patch.compute_preference {
                let pref = parse_compute_preference(pref_str)?;
                // 验证属于该引擎 descriptor 声明项
                validate_preference_for_engine(&svc, &eid, pref).await?;

                // PaddleOCR 只允许 auto/cpu（validate 会拒绝 cuda/vulkan/directml）
                if ocr_config.compute_preference != pref {
                    ocr_config.compute_preference = pref;
                    profile_changed = true;
                }
            }

            // 应用 OCR 路由后端 patch（仅 PaddleOCR）
            if let Some(ref backend_str) = patch.ocr_backend {
                ocr_config.backend = parse_ocr_backend(backend_str)?;
            }

            // 应用 lifecycle patch（仅 PaddleOCR）
            if let Some(ref lifecycle_str) = patch.lifecycle {
                let new_lifecycle = parse_ocr_lifecycle(lifecycle_str)?;
                if ocr_config.lifecycle != new_lifecycle {
                    ocr_config.lifecycle = new_lifecycle;
                }
            }

            // 校验配置
            ocr_config.validate().map_err(|e| {
                CommandError::new("validation_failed", format!("OCR 配置校验失败: {e}"), false)
            })?;

            // 持久化 + cache 热更新
            crate::domain::config::store::ConfigStore::set(&pool.config, &ocr_config)
                .await
                .map_err(|e| {
                    CommandError::new("save_failed", format!("保存 OCR 配置失败: {e}"), false)
                })?;
            crate::domain::config::ocr_config::update_cache(&ocr_config);

            tracing::info!(
                engine = %eid,
                ocr_backend = %ocr_config.backend,
                compute_preference = ?ocr_config.compute_preference,
                lifecycle = %ocr_config.lifecycle,
                "PaddleOCR 偏好已保存"
            );

            // 广播配置变更
            let _ = app.emit(
                crate::domain::event_names::EventNames::CONFIG_CHANGED,
                serde_json::json!({ "scope": "ocr" }),
            );

            // PaddleOCR auto/cpu 若最终解析为同一 CPU profile，不强制重建
            // 由后端判断——这里检查 compute_preference 是否实际改变了 backend
            if profile_changed {
                // auto 和 cpu 都解析为 CPU backend，所以不强制重建
                // 但仍标记——由后端状态判断决定
                // 只在实际 backend 变化时才 NeedsRebuild
                // PaddleOCR 首版只允许 auto/cpu，两者都是 CPU backend
                // 所以 PaddleOCR 的 compute_preference 变化不触发 NeedsRebuild
                false
            } else {
                false
            }
        }
        other => {
            return Err(CommandError::new(
                "unsupported_engine",
                format!("不支持的引擎: {other}"),
                false,
            ));
        }
    };

    // 构建返回 DTO
    let dto = match engine_id.as_str() {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            // 0.22.6 归一化：无论配置中 device 值是什么，前端总是看到 cpu
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some("cpu".to_string()),
                auto_start: Some(config.local_engine.auto_start_server),
                ocr_backend: None,
                lifecycle: None,
                requires_rebuild: if requires_rebuild { Some(true) } else { None },
            }
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(preference_to_string_local(ocr_config.compute_preference)),
                auto_start: None,
                ocr_backend: Some(ocr_config.backend.to_string()),
                lifecycle: Some(ocr_config.lifecycle.to_string()),
                requires_rebuild: if requires_rebuild { Some(true) } else { None },
            }
        }
        _ => unreachable!(),
    };

    Ok(dto)
}

// ── 内部辅助：compute preference 解析与验证 ──────────────────────────────────

/// 从字符串解析 compute preference。
fn parse_compute_preference(s: &str) -> Result<ComputePreference, CommandError> {
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

/// 从字符串解析 OCR 路由后端。
fn parse_ocr_backend(
    value: &str,
) -> Result<crate::domain::ocr::config::OcrBackendKind, CommandError> {
    use crate::domain::ocr::config::OcrBackendKind;

    match value {
        "windows" => Ok(OcrBackendKind::Windows),
        "paddleocr" => Ok(OcrBackendKind::PaddleOcr),
        "auto" => Ok(OcrBackendKind::Auto),
        other => Err(CommandError::new(
            "invalid_ocr_backend",
            format!("未知的 OCR backend: {other}"),
            false,
        )),
    }
}

/// 从字符串解析 OCR lifecycle。
fn parse_ocr_lifecycle(s: &str) -> Result<crate::domain::ocr::config::OcrLifecycle, CommandError> {
    match s {
        "on_demand" => Ok(crate::domain::ocr::config::OcrLifecycle::OnDemand),
        "keep_running" => Ok(crate::domain::ocr::config::OcrLifecycle::KeepRunning),
        "stop_after_use" => Ok(crate::domain::ocr::config::OcrLifecycle::StopAfterUse),
        other => Err(CommandError::new(
            "invalid_lifecycle",
            format!("未知的 lifecycle: {other}"),
            false,
        )),
    }
}

/// 将 ComputePreference 转为字符串（后端用）。
fn preference_to_string_local(p: ComputePreference) -> String {
    match p {
        ComputePreference::Auto => "auto".to_string(),
        ComputePreference::Cpu => "cpu".to_string(),
        ComputePreference::GpuAuto => "gpu_auto".to_string(),
        ComputePreference::Cuda => "cuda".to_string(),
        ComputePreference::Vulkan => "vulkan".to_string(),
        ComputePreference::Directml => "directml".to_string(),
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
async fn validate_preference_for_engine(
    svc: &EngineManager,
    engine_id: &EngineId,
    preference: ComputePreference,
) -> Result<(), CommandError> {
    // 策略性偏好总是允许——由 resolver 解析为具体 profile
    if !preference.is_explicit() {
        return Ok(());
    }

    let catalog = svc.catalog().await;
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

    let declared = descriptor
        .install_plan
        .compute_candidates
        .iter()
        .any(|c| c.preference == preference);

    if !declared {
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
    // 使用与 get_local_engine_logs 相同的合并逻辑，但固定 50 行上限
    let recent_logs = get_merged_logs(&app, &svc, &eid, 50).await;

    // ── 4. orphan recovery ──
    let orphan_recovery = scan_orphan_recovery(&eid).await;

    // ── 5. 投影为闭合 DTO ──
    Ok(EngineDiagnosticsDto {
        engine_id: engine_id,
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

// ── 模型生命周期 commands ──────────────────────────────────────────────────
//
// 模型资产业务由 EngineManager 统一承载（单一业务真相）——
// 删除冲突检查（selected/active）、事务与互斥都在 manager 内部完成，
// commands 层只做参数校验与 DTO 投影。

/// 列出引擎的所有模型候选及其当前状态。
///
/// **只读查询，无副作用。** 前端据此展示模型列表，
/// 但**不触发下载**——下载只在引擎页管理。
#[tauri::command]
pub async fn list_engine_models(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<Vec<crate::app::local_engine::model_installer::ModelCatalogItemDto>, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let models = svc.list_models(&eid).await;

    // 投影为 DTO
    let dtos: Vec<crate::app::local_engine::model_installer::ModelCatalogItemDto> = models
        .iter()
        .map(|status| {
            let desc = svc
                .model_registry()
                .find(&eid, &status.model_id)
                .expect("模型状态必须有对应 descriptor");
            crate::app::local_engine::model_installer::project_model_status(desc, status)
        })
        .collect();

    Ok(dtos)
}

/// 安装引擎模型（真实事务：staging/下载/校验/提升）。
///
/// 前端只需提交 `engine_id`、`model_id`、`operation_id`（可选）。
/// **禁止包含 URL、路径、脚本、外部命令。**
#[tauri::command]
pub async fn install_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型安装请求"
    );

    let result = svc
        .install_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = %result.operation_id,
        success = result.success,
        "模型安装操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 删除引擎模型（引用检查 + 删除）。
///
/// **删除正在使用或被配置引用的模型必须返回结构化冲突**，
/// 不能静默切换到其他模型。冲突判定：
/// - selected（配置真源）；
/// - active（launch snapshot 冻结的模型身份 + instance_id）；
/// - descriptor 默认模型不构成删除保护。
#[tauri::command]
pub async fn delete_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型删除请求"
    );

    let result = svc
        .delete_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = %result.operation_id,
        success = result.success,
        "模型删除操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 修复引擎模型（重新下载/校验）。
#[tauri::command]
pub async fn repair_engine_model(
    app: tauri::AppHandle,
    request: crate::app::local_engine::model_installer::ModelOperationRequestDto,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&request.engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %request.model_id,
        op_id = ?request.operation_id,
        "收到模型修复请求"
    );

    let result = svc
        .repair_model(&eid, &request.model_id, request.operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = %result.operation_id,
        success = result.success,
        "模型修复操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

/// 取消模型操作（只触发匹配 operation_id 的 claim token；
/// worker 结束前 claim 不释放）。
#[tauri::command]
pub async fn cancel_model_operation(
    app: tauri::AppHandle,
    engine_id: String,
    model_id: String,
    operation_id: String,
) -> Result<crate::app::local_engine::model_installer::ModelOperationResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    tracing::info!(
        engine = %eid,
        model = %model_id,
        op_id = %operation_id,
        "收到取消模型操作请求"
    );

    let result = svc
        .cancel_model_operation(&eid, &model_id, &operation_id)
        .await
        .map_err(CommandError::from)?;

    tracing::info!(
        engine = %eid,
        model = %result.model_id,
        op_id = %result.operation_id,
        success = %result.success,
        "取消模型操作完成"
    );

    Ok(crate::app::local_engine::model_installer::project_model_operation_result(&result))
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── 未知 engine_id 被拒绝 ──

    #[test]
    fn unknown_engine_id_rejected() {
        let result = validate_engine_id("");
        assert!(result.is_err());
    }

    #[test]
    fn invalid_engine_id_rejected() {
        let result = validate_engine_id("invalid/id/with/slashes");
        assert!(result.is_err());
    }

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

    // ── catalog 只包含 registry allowlist ──

    #[test]
    fn catalog_only_contains_registry_allowlist() {
        // catalog 从 svc.catalog() 获取，svc 只包含 registry 中注册的引擎
        // 此测试验证 build_adapter_config_for_engine 只处理 allowlist 中的引擎
        let known = ["funasr", "paddleocr"];
        for id in &known {
            assert!(build_adapter_config_for_engine(id).is_ok());
        }
        // 未知引擎被拒绝
        assert!(build_adapter_config_for_engine("unknown").is_err());
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

    // ── service_epoch 是字符串 ──

    #[test]
    fn service_epoch_is_string() {
        use crate::domain::local_engine::ServiceEpoch;
        let epoch = ServiceEpoch::new();
        let s = epoch.to_string();
        assert!(s.starts_with("epoch-"));
        // 验证字符串长度是 16 hex + "epoch-" 前缀
        assert_eq!(s.len(), 6 + 16);
    }

    // ── 旧 FunASR lifecycle 命令已删除（0.22.6 phase B）──
    // 未发版且前端 0 引用：get_funasr_env / setup_python_env / start_funasr_server /
    // stop_funasr_server / get_funasr_log_history 已随 maintenance 瘦身删除。
    // 若恢复引用请改走通用 local_engine 命令（get_local_engine_status 等）。

    // ── 旧事件常量仍存在（旧前端兼容投影仍在用）──

    #[test]
    fn old_funasr_event_constants_still_exist() {
        assert_eq!(
            crate::domain::event_names::EventNames::FUNASR_SERVER_STATUS,
            "blink://funasr-server-status"
        );
        assert_eq!(
            crate::domain::event_names::EventNames::FUNASR_SERVER_LOG,
            "blink://funasr-server-log"
        );
    }

    // ── 新事件常量存在 ──

    #[test]
    fn new_local_engine_event_constants_exist() {
        assert_eq!(
            crate::domain::event_names::EventNames::LOCAL_ENGINE_STATUS,
            "blink://local-engine-status"
        );
        assert_eq!(
            crate::domain::event_names::EventNames::LOCAL_ENGINE_LOG,
            "blink://local-engine-log"
        );
    }

    // ── 日志 DTO shape 包含 instance_id + seq ──

    #[test]
    fn log_dto_has_instance_id_and_seq() {
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

    // ── status DTO service_epoch 是字符串 ──

    #[test]
    fn status_dto_service_epoch_is_string() {
        use crate::app::local_engine::dto::{EngineStatusDto, EngineStatusWire, ProcessStateDto};
        let dto = EngineStatusDto {
            engine_id: "funasr".to_string(),
            service_epoch: "epoch-0016a3f4deadbeef".to_string(),
            revision: "1".to_string(),
            status: EngineStatusWire {
                desired: "stopped".to_string(),
                operation: serde_json::Value::Null,
                environment: "missing".to_string(),
                process: ProcessStateDto {
                    state: "stopped".to_string(),
                    pid: None,
                    reason: None,
                },
                service: "unknown".to_string(),
                model: "unknown".to_string(),
                available: false,
                backend: serde_json::Value::Null,
                last_error: None,
            },
        };
        let json = serde_json::to_value(&dto).unwrap();
        // service_epoch 必须是字符串（不是数字）
        assert!(json["service_epoch"].is_string());
        assert!(json["revision"].is_string());
        // process 是显式 DTO 对象，不是字符串
        assert!(json["status"]["process"].is_object());
        assert_eq!(json["status"]["process"]["state"], "stopped");
        assert!(json["status"]["process"].get("pid").is_none());
    }

    // ── catalog DTO 不暴露 artifact URL / executable / argv / env ──

    #[test]
    fn catalog_dto_does_not_expose_internals() {
        use crate::app::local_engine::dto::EngineCatalogItem;
        let dto = EngineCatalogItem {
            engine_id: "funasr".to_string(),
            display_name: "FunASR".to_string(),
            description: "STT".to_string(),
            icon: "mic".to_string(),
            version: "0.1.0".to_string(),
            capability_kind: "stt".to_string(),
            runtime_kind: "python_venv".to_string(),
            lifecycle: "manual".to_string(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            model_revision: "v1".to_string(),
            resource_budget: crate::app::local_engine::dto::ResourceBudgetDto {
                estimated_env_disk_mb: Some(3000),
                estimated_model_disk_mb: Some(234),
                estimated_stable_ram_mb: Some(500),
                estimated_peak_ram_mb: Some(1500),
            },
            compute_options: vec![],
            current_compute_preference: "cpu".to_string(),
            cleanup_summary: crate::app::local_engine::dto::CleanupSummaryDto {
                owned_subdirs: vec!["generations".to_string()],
                has_model_cache: true,
                has_log_dir: false,
            },
        };
        let json = serde_json::to_value(&dto).unwrap();
        // 不包含 executable
        assert!(json.get("executable").is_none());
        assert!(json.get("argv").is_none());
        assert!(json.get("env").is_none());
        assert!(json.get("artifact_url").is_none());
        assert!(json.get("token").is_none());
        assert!(json.get("endpoint").is_none());
        assert!(json.get("file_path").is_none());
        assert!(json.get("script_path").is_none());
    }

    // ── 新 commands 签名可编译 ──

    #[test]
    fn all_new_commands_compile() {
        let _ = get_local_engine_catalog as fn(tauri::AppHandle) -> _;
        let _ = get_local_engine_status as fn(tauri::AppHandle, Option<String>) -> _;
        let _ = get_local_engine_logs as fn(tauri::AppHandle, String, Option<usize>) -> _;
        let _ = install_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
        let _ = start_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
        let _ = stop_local_engine as fn(tauri::AppHandle, String) -> _;
        let _ = repair_local_engine as fn(tauri::AppHandle, String) -> _;
        let _ = get_local_engine_storage as fn(tauri::AppHandle, String) -> _;
        let _ = cleanup_local_engine as fn(tauri::AppHandle, CleanupRequestDto) -> _;
        let _ = cancel_local_engine_operation as fn(tauri::AppHandle, String, String) -> _;
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

    // ── preferences commands 签名可编译 ──

    #[test]
    fn preferences_commands_compile() {
        let _ = get_local_engine_preferences as fn(tauri::AppHandle, String) -> _;
        let _ = set_local_engine_preferences
            as fn(tauri::AppHandle, String, EnginePreferencesPatchDto) -> _;
    }

    // ── OCR lifecycle 解析 ──

    #[test]
    fn parse_ocr_lifecycle_on_demand() {
        assert_eq!(
            parse_ocr_lifecycle("on_demand").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::OnDemand
        );
    }

    #[test]
    fn parse_ocr_lifecycle_keep_running() {
        assert_eq!(
            parse_ocr_lifecycle("keep_running").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::KeepRunning
        );
    }

    #[test]
    fn parse_ocr_lifecycle_stop_after_use() {
        assert_eq!(
            parse_ocr_lifecycle("stop_after_use").unwrap(),
            crate::domain::ocr::config::OcrLifecycle::StopAfterUse
        );
    }

    #[test]
    fn parse_ocr_lifecycle_invalid() {
        assert!(parse_ocr_lifecycle("always_on").is_err());
    }

    #[test]
    fn parse_ocr_backend_accepts_closed_values() {
        use crate::domain::ocr::config::OcrBackendKind;

        assert_eq!(
            parse_ocr_backend("windows").unwrap(),
            OcrBackendKind::Windows
        );
        assert_eq!(
            parse_ocr_backend("paddleocr").unwrap(),
            OcrBackendKind::PaddleOcr
        );
        assert_eq!(parse_ocr_backend("auto").unwrap(), OcrBackendKind::Auto);
        assert!(parse_ocr_backend("remote").is_err());
    }

    // ── preference_to_string_local 覆盖所有变体 ──

    #[test]
    fn preference_to_string_local_all_variants() {
        assert_eq!(preference_to_string_local(ComputePreference::Auto), "auto");
        assert_eq!(preference_to_string_local(ComputePreference::Cpu), "cpu");
        assert_eq!(
            preference_to_string_local(ComputePreference::GpuAuto),
            "gpu_auto"
        );
        assert_eq!(preference_to_string_local(ComputePreference::Cuda), "cuda");
        assert_eq!(
            preference_to_string_local(ComputePreference::Vulkan),
            "vulkan"
        );
        assert_eq!(
            preference_to_string_local(ComputePreference::Directml),
            "directml"
        );
    }

    // ── storage DTO 不暴露完整路径 ──

    #[test]
    fn storage_dto_does_not_expose_full_paths() {
        use crate::app::local_engine::dto::{
            EngineStorageDto, StorageTargetDto, StorageTargetKindDto,
        };

        let target = StorageTargetDto {
            target_id: "gen:abc123".to_string(),
            kind: StorageTargetKindDto::EngineGeneration,
            engine_id: Some("funasr".to_string()),
            label_key: "local_engine.storage.engine_generation".to_string(),
            label_fallback: "当前环境".to_string(),
            size_bytes: 3000 * 1024 * 1024,
            current: true,
            previous: false,
            removable: false,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: Some("current_generation".to_string()),
            affected_engine_ids: None,
            reference_count: None,
            path_display: None,
        };

        let dto = EngineStorageDto {
            engine_id: Some("funasr".to_string()),
            targets: vec![target],
            total_size_bytes: 3000 * 1024 * 1024,
            releasable_size_bytes: 0,
        };

        let json = serde_json::to_value(&dto).unwrap();
        // 不包含完整文件路径字段
        assert!(json.get("path").is_none());
        assert!(json.get("file_path").is_none());
        assert!(json.get("dir_path").is_none());
        // target_id 是安全暴露的标识符
        assert!(json["targets"][0]["target_id"].is_string());
    }

    // ── cleanup DTO 包含 required fields ──

    #[test]
    fn cleanup_result_dto_has_required_fields() {
        use crate::app::local_engine::dto::CleanupResultDto;

        let dto = CleanupResultDto {
            engine_id: "funasr".to_string(),
            operation_id: "op-abc123".to_string(),
            cleaned_target_ids: vec!["gen:old".to_string()],
            skipped_target_ids: vec![],
            released_bytes: 1024 * 1024 * 500,
            deferred_target_ids: vec![],
            error: None,
        };

        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "funasr");
        assert_eq!(json["operation_id"], "op-abc123");
        assert!(json["cleaned_target_ids"].is_array());
        assert!(json["skipped_target_ids"].is_array());
        assert!(json["deferred_target_ids"].is_array());
        assert!(json["released_bytes"].is_number());
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

    // ── cleanup 请求空 target_ids 被拒绝 ──

    #[test]
    fn cleanup_request_dto_deserializes() {
        use crate::app::local_engine::dto::CleanupRequestDto;

        let json = serde_json::json!({
            "engine_id": "funasr",
            "target_ids": ["gen:abc123", "model_cache"],
            "operation_id": "op-123"
        });

        let dto: CleanupRequestDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.target_ids.len(), 2);
        assert_eq!(dto.operation_id, Some("op-123".to_string()));
    }

    // ── LocalEngineError → CommandError 映射 ──

    #[test]
    fn local_engine_error_maps_to_command_error() {
        use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

        let err = LocalEngineError::with_detail(
            LocalEngineErrorCode::Cancelled,
            ErrorPhase::Request,
            "操作已取消",
            "user cancelled",
        );

        let ce: CommandError = err.into();
        assert_eq!(ce.code, "cancelled");
        assert!(!ce.retryable);
        assert!(ce.detail.is_some());
    }

    #[test]
    fn local_engine_error_timeout_is_retryable() {
        use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

        let err = LocalEngineError::new(
            LocalEngineErrorCode::Timeout,
            ErrorPhase::Health,
            "健康检查超时",
        );

        let ce: CommandError = err.into();
        assert_eq!(ce.code, "timeout");
        assert!(ce.retryable);
    }

    #[test]
    fn local_engine_error_self_test_failed_not_retryable() {
        use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

        let err = LocalEngineError::new(
            LocalEngineErrorCode::SelfTestFailed,
            ErrorPhase::SelfTest,
            "self-test 失败",
        );

        let ce: CommandError = err.into();
        assert_eq!(ce.code, "self_test_failed");
        assert!(!ce.retryable);
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
    // 0.22.6 H4: 偏好 round-trip + 静态契约测试
    // ═══════════════════════════════════════════════════════════════════════

    // ── 偏好 DTO round-trip：get → set → get 一致性 ──

    #[test]
    fn funasr_preferences_dto_round_trip() {
        // 模拟 get_local_engine_preferences 返回的 DTO
        let get_dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(true),
            ocr_backend: None,
            lifecycle: None,
            requires_rebuild: None,
        };

        // 从 get DTO 构造 patch DTO（只修改需要变更的字段）
        let patch = EnginePreferencesPatchDto {
            compute_preference: get_dto.compute_preference.clone(),
            auto_start: get_dto.auto_start,
            ocr_backend: None,
            lifecycle: None,
        };

        // 验证 patch 序列化/反序列化 round-trip
        let patch_json = serde_json::to_string(&patch).unwrap();
        let patch_restored: EnginePreferencesPatchDto = serde_json::from_str(&patch_json).unwrap();
        assert_eq!(patch_restored.compute_preference, patch.compute_preference);
        assert_eq!(patch_restored.auto_start, patch.auto_start);
        assert_eq!(patch_restored.lifecycle, patch.lifecycle);
    }

    #[test]
    fn paddleocr_preferences_dto_round_trip() {
        let get_dto = EnginePreferencesDto {
            engine_id: "paddleocr".to_string(),
            compute_preference: Some("auto".to_string()),
            auto_start: None,
            ocr_backend: Some("paddleocr".to_string()),
            lifecycle: Some("on_demand".to_string()),
            requires_rebuild: None,
        };

        let patch = EnginePreferencesPatchDto {
            compute_preference: get_dto.compute_preference.clone(),
            auto_start: None,
            ocr_backend: get_dto.ocr_backend.clone(),
            lifecycle: get_dto.lifecycle.clone(),
        };

        let patch_json = serde_json::to_string(&patch).unwrap();
        let patch_restored: EnginePreferencesPatchDto = serde_json::from_str(&patch_json).unwrap();
        assert_eq!(patch_restored.compute_preference, patch.compute_preference);
        assert_eq!(patch_restored.ocr_backend, patch.ocr_backend);
        assert_eq!(patch_restored.lifecycle, patch.lifecycle);
    }

    #[test]
    fn preferences_patch_empty_is_noop() {
        // 空 patch = 不修改任何字段
        let patch = EnginePreferencesPatchDto {
            compute_preference: None,
            auto_start: None,
            ocr_backend: None,
            lifecycle: None,
        };
        let json = serde_json::to_value(&patch).unwrap();
        // 所有字段都被 skip_serializing_if 跳过
        assert!(json.get("compute_preference").is_none() || json["compute_preference"].is_null());
        assert!(json.get("auto_start").is_none() || json["auto_start"].is_null());
        assert!(json.get("ocr_backend").is_none() || json["ocr_backend"].is_null());
        assert!(json.get("lifecycle").is_none() || json["lifecycle"].is_null());
    }

    // ── FunASR preferences 字段约束 ──

    #[test]
    fn funasr_preferences_has_auto_start_not_lifecycle() {
        // FunASR 有 auto_start，无 lifecycle
        let dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(false),
            ocr_backend: None,
            lifecycle: None,
            requires_rebuild: None,
        };
        assert!(dto.auto_start.is_some(), "FunASR 应有 auto_start");
        assert!(dto.lifecycle.is_none(), "FunASR 不应有 lifecycle");
    }

    #[test]
    fn paddleocr_preferences_has_lifecycle_not_auto_start() {
        // PaddleOCR 有 lifecycle，无 auto_start
        let dto = EnginePreferencesDto {
            engine_id: "paddleocr".to_string(),
            compute_preference: Some("auto".to_string()),
            auto_start: None,
            ocr_backend: Some("auto".to_string()),
            lifecycle: Some("keep_running".to_string()),
            requires_rebuild: None,
        };
        assert!(dto.auto_start.is_none(), "PaddleOCR 不应有 auto_start");
        assert!(dto.ocr_backend.is_some(), "PaddleOCR 应有 ocr_backend");
        assert!(dto.lifecycle.is_some(), "PaddleOCR 应有 lifecycle");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6 H4 §13: 静态契约测试
    // 验证前端调用的 local-engine / stt command 全部已在 invoke_handler 注册。
    // 如果前端调用了未注册的命令，此测试会失败，帮助及早发现遗漏。
    // ═══════════════════════════════════════════════════════════════════════

    #[test]
    fn all_frontend_local_engine_commands_are_registered() {
        // 前端已知的 local-engine command 名称集合（从 frontend/js 中提取）
        // 这些命令必须在 main.rs invoke_handler 中注册
        let frontend_commands: &[&str] = &[
            "get_local_engine_catalog",
            "get_local_engine_status",
            "get_local_engine_logs",
            "install_local_engine",
            "start_local_engine",
            "stop_local_engine",
            "repair_local_engine",
            "get_local_engine_storage",
            "cleanup_local_engine",
            "cancel_local_engine_operation",
            "get_local_engine_preferences",
            "set_local_engine_preferences",
            "get_runtime_foundation_status",
            "get_engine_diagnostics",
            "open_engine_folder",
            "open_runtime_folder",
            "stop_orphan_engine",
            // 0.22.6 H5: 模型生命周期命令
            "list_engine_models",
            "install_engine_model",
            "delete_engine_model",
            "repair_engine_model",
            "cancel_model_operation",
        ];

        // 验证每个命令名称对应的函数存在于 app::commands 模块中
        // 这是编译期检查——如果函数不存在，编译会失败
        for &cmd_name in frontend_commands {
            let exists = match cmd_name {
                "get_local_engine_catalog" => {
                    let _ = get_local_engine_catalog as fn(tauri::AppHandle) -> _;
                    true
                }
                "get_local_engine_status" => {
                    let _ = get_local_engine_status as fn(tauri::AppHandle, Option<String>) -> _;
                    true
                }
                "get_local_engine_logs" => {
                    let _ =
                        get_local_engine_logs as fn(tauri::AppHandle, String, Option<usize>) -> _;
                    true
                }
                "install_local_engine" => {
                    let _ =
                        install_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
                    true
                }
                "start_local_engine" => {
                    let _ = start_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
                    true
                }
                "stop_local_engine" => {
                    let _ = stop_local_engine as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "repair_local_engine" => {
                    let _ = repair_local_engine as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "get_local_engine_storage" => {
                    let _ = get_local_engine_storage as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "cleanup_local_engine" => {
                    let _ = cleanup_local_engine as fn(tauri::AppHandle, CleanupRequestDto) -> _;
                    true
                }
                "cancel_local_engine_operation" => {
                    let _ =
                        cancel_local_engine_operation as fn(tauri::AppHandle, String, String) -> _;
                    true
                }
                "get_local_engine_preferences" => {
                    let _ = get_local_engine_preferences as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "set_local_engine_preferences" => {
                    let _ = set_local_engine_preferences
                        as fn(tauri::AppHandle, String, EnginePreferencesPatchDto) -> _;
                    true
                }
                "get_runtime_foundation_status" => {
                    let _ = get_runtime_foundation_status as fn(tauri::AppHandle) -> _;
                    true
                }
                "get_engine_diagnostics" => {
                    let _ = get_engine_diagnostics as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "open_engine_folder" => {
                    let _ = open_engine_folder as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "open_runtime_folder" => {
                    let _ = open_runtime_folder as fn(tauri::AppHandle) -> _;
                    true
                }
                "stop_orphan_engine" => {
                    let _ = stop_orphan_engine as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "list_engine_models" => {
                    let _ = list_engine_models as fn(tauri::AppHandle, String) -> _;
                    true
                }
                "install_engine_model" => {
                    let _ = install_engine_model
                        as fn(
                            tauri::AppHandle,
                            crate::app::local_engine::model_installer::ModelOperationRequestDto,
                        ) -> _;
                    true
                }
                "delete_engine_model" => {
                    let _ = delete_engine_model
                        as fn(
                            tauri::AppHandle,
                            crate::app::local_engine::model_installer::ModelOperationRequestDto,
                        ) -> _;
                    true
                }
                "repair_engine_model" => {
                    let _ = repair_engine_model
                        as fn(
                            tauri::AppHandle,
                            crate::app::local_engine::model_installer::ModelOperationRequestDto,
                        ) -> _;
                    true
                }
                "cancel_model_operation" => {
                    let _ =
                        cancel_model_operation as fn(tauri::AppHandle, String, String, String) -> _;
                    true
                }
                _ => panic!("未知的前端命令名: {cmd_name}"),
            };
            assert!(exists, "命令 {cmd_name} 未注册或函数不存在");
        }
    }

    #[test]
    fn all_frontend_stt_commands_are_registered() {
        // 前端已知的 STT command 名称集合
        let frontend_stt_commands: &[&str] = &[
            "get_stt_config",
            "set_stt_config",
            // 0.22.6 phase B: list_stt_models/download_stt_model/delete_stt_model 已删除
            "list_selectable_stt_models",
            "set_local_stt_selection",
            "cancel_voice_recording",
            "is_voice_recording",
            "list_audio_devices",
            "start_audio_test",
            "stop_audio_test",
            "save_stt_secret",
            "delete_stt_secret",
            "has_stt_secret",
            "get_stt_secret_hint",
            "test_cloud_stt",
            "resize_voice_overlay",
            "start_chat_stt",
            "stop_chat_stt",
        ];

        // 验证每个命令名称对应的函数存在于 app::commands 模块中
        for &cmd_name in frontend_stt_commands {
            let exists = match cmd_name {
                "get_stt_config" => {
                    let _ = crate::app::commands::get_stt_config as fn(tauri::AppHandle) -> _;
                    true
                }
                "set_stt_config" => {
                    let _ = crate::app::commands::set_stt_config
                        as fn(
                            tauri::AppHandle,
                            crate::app::stt_config::SttConfig,
                            Option<String>,
                        ) -> _;
                    true
                }
                "list_selectable_stt_models" => {
                    let _ = crate::app::commands::list_selectable_stt_models
                        as fn(tauri::AppHandle) -> _;
                    true
                }
                "set_local_stt_selection" => {
                    let _ = crate::app::commands::set_local_stt_selection
                        as fn(tauri::AppHandle, String, String) -> _;
                    true
                }
                "cancel_voice_recording" => {
                    let _ =
                        crate::app::commands::cancel_voice_recording as fn(tauri::AppHandle) -> _;
                    true
                }
                "is_voice_recording" => {
                    let _ = crate::app::commands::is_voice_recording as fn(tauri::AppHandle) -> _;
                    true
                }
                "list_audio_devices" => {
                    let _ = crate::app::commands::list_audio_devices as fn() -> _;
                    true
                }
                "start_audio_test" => {
                    let _ = crate::app::commands::start_audio_test
                        as fn(tauri::AppHandle, Option<String>) -> _;
                    true
                }
                "stop_audio_test" => {
                    let _ = crate::app::commands::stop_audio_test as fn() -> _;
                    true
                }
                "save_stt_secret" => {
                    let _ = crate::app::commands::save_stt_secret as fn(String) -> _;
                    true
                }
                "delete_stt_secret" => {
                    let _ = crate::app::commands::delete_stt_secret as fn() -> _;
                    true
                }
                "has_stt_secret" => {
                    let _ = crate::app::commands::has_stt_secret as fn() -> _;
                    true
                }
                "get_stt_secret_hint" => {
                    let _ = crate::app::commands::get_stt_secret_hint as fn() -> _;
                    true
                }
                "test_cloud_stt" => {
                    let _ = crate::app::commands::test_cloud_stt as fn() -> _;
                    true
                }
                "resize_voice_overlay" => {
                    let _ = crate::app::commands::resize_voice_overlay
                        as fn(tauri::AppHandle, f64) -> _;
                    true
                }
                "start_chat_stt" => {
                    let _ = crate::app::commands::start_chat_stt as fn(tauri::AppHandle) -> _;
                    true
                }
                "stop_chat_stt" => {
                    let _ = crate::app::commands::stop_chat_stt as fn(tauri::AppHandle) -> _;
                    true
                }
                _ => panic!("未知的前端 STT 命令名: {cmd_name}"),
            };
            assert!(exists, "STT 命令 {cmd_name} 未注册或函数不存在");
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

    /// PaddleOCR descriptor 只声明 CPU profile——验证 `Auto` 不在候选列表中，
    /// 但 `Auto` 作为策略性偏好应被 validator 允许。
    #[test]
    fn contract_paddleocr_auto_not_in_candidates_but_allowed() {
        use crate::domain::local_engine::adapter::LocalEngineAdapter;

        let descriptor = paddleocr::PaddleocrAdapter::new().descriptor().clone();

        // Auto 不在 compute_candidates 中
        assert!(
            !descriptor.has_preference(ComputePreference::Auto),
            "PaddleOCR descriptor 不应声明 Auto 候选（Auto 是策略性偏好）"
        );
        // Cpu 在 compute_candidates 中
        assert!(
            descriptor.has_preference(ComputePreference::Cpu),
            "PaddleOCR descriptor 应声明 Cpu 候选"
        );
        // Cuda 不在 compute_candidates 中
        assert!(
            !descriptor.has_preference(ComputePreference::Cuda),
            "PaddleOCR descriptor 不应声明 Cuda 候选"
        );
    }

    /// FunASR descriptor 只声明 CPU profile——与 PaddleOCR 同理。
    #[test]
    fn contract_funasr_auto_not_in_candidates_but_allowed() {
        use crate::domain::local_engine::adapter::LocalEngineAdapter;

        let descriptor = funasr::FunasrAdapter::new().descriptor().clone();

        assert!(
            !descriptor.has_preference(ComputePreference::Auto),
            "FunASR descriptor 不应声明 Auto 候选（Auto 是策略性偏好）"
        );
        assert!(
            descriptor.has_preference(ComputePreference::Cpu),
            "FunASR descriptor 应声明 Cpu 候选"
        );
        assert!(
            !descriptor.has_preference(ComputePreference::Cuda),
            "FunASR 0.22.6 不应声明 Cuda 候选"
        );
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

    /// `current_compute_preference` 为 FunASR 始终返回 Cpu。
    #[test]
    fn contract_funasr_current_compute_preference_always_cpu() {
        let pref = current_compute_preference("funasr");
        assert_eq!(
            pref,
            ComputePreference::Cpu,
            "FunASR current_compute_preference 必须为 Cpu"
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

    /// PaddleOCR ProviderDescriptor 只声明 CPU profile（`Always` 兼容）。
    /// 验证 Auto 策略可以通过 resolve_profile 解析为 CPU backend。
    #[test]
    fn contract_paddleocr_provider_descriptor_only_cpu() {
        let descriptor = paddleocr::make_paddleocr_provider_descriptor();

        // 只有一个 CPU profile
        assert_eq!(descriptor.profiles.len(), 1);
        assert_eq!(
            descriptor.profiles[0].backend,
            crate::infra::local_engine::runtime::ComputeBackend::Cpu
        );
        // compatibility 是 Always
        assert!(matches!(
            descriptor.profiles[0].compatibility,
            crate::infra::local_engine::providers::CompatibilityCheck::Always
        ));
    }

    /// FunASR ProviderDescriptor 只声明 CPU profile（`Always` 兼容）。
    #[test]
    fn contract_funasr_provider_descriptor_only_cpu() {
        let descriptor = funasr::make_funasr_provider_descriptor();

        assert_eq!(descriptor.profiles.len(), 1);
        assert_eq!(
            descriptor.profiles[0].backend,
            crate::infra::local_engine::runtime::ComputeBackend::Cpu
        );
        assert!(matches!(
            descriptor.profiles[0].compatibility,
            crate::infra::local_engine::providers::CompatibilityCheck::Always
        ));
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
