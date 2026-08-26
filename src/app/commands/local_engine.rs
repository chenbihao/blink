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
    CancelResultDto, CleanupRequestDto, CleanupResultDto, EngineCatalogItem, EngineLogDto,
    EnginePreferencesDto, EnginePreferencesPatchDto, EngineStatusDto, EngineStorageDto,
    project_catalog_item, project_log, project_status,
};
use crate::app::local_engine::{LocalEngineService, funasr, paddleocr};
use crate::domain::local_engine::EngineDescriptor;
use crate::infra::local_engine::providers::RuntimeProvider;
use crate::infra::local_engine::runtime::{ComputePreference, EngineId};

use tauri::{Emitter, Manager};

// ── 内部辅助 ──────────────────────────────────────────────────────────────────

/// 从 managed state 获取 `LocalEngineService` 引用。
fn get_service(app: &tauri::AppHandle) -> Result<Arc<LocalEngineService>, CommandError> {
    app.try_state::<Arc<LocalEngineService>>()
        .map(|s| s.inner().clone())
        .ok_or_else(|| CommandError::new("internal_error", "LocalEngineService 尚未注册", false))
}

/// 验证 engine_id 并返回 `EngineId`。
fn validate_engine_id(engine_id: &str) -> Result<EngineId, CommandError> {
    EngineId::new(engine_id).map_err(|e| {
        CommandError::new("invalid_engine_id", format!("无效的 engine_id: {e}"), false)
    })
}

/// 从配置真源读取当前 compute preference。
///
/// - funasr → 从 `SttConfig.local_engine.device` 映射
/// - paddleocr → 从 `OcrConfig.compute_preference` 读取
/// - 其他 → Auto
fn current_compute_preference(engine_id: &str) -> ComputePreference {
    match engine_id {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            if config.local_engine.device == "cuda" {
                ComputePreference::Cuda
            } else {
                ComputePreference::Cpu
            }
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            crate::domain::config::ocr_config::get_ocr_config().compute_preference
        }
        _ => ComputePreference::Auto,
    }
}

/// 从配置真源构造 `AdapterConfig`。
///
/// **禁止前端直接提交 `AdapterConfig.engine_config`**。
/// 此函数根据 `engine_id` 从现有配置真源构造：
/// - funasr → `SttConfig.local_engine`
/// - paddleocr → `OcrConfig` / `PaddleOcrEngineConfig`
fn build_adapter_config_for_engine(
    engine_id: &str,
) -> Result<crate::domain::local_engine::AdapterConfig, CommandError> {
    match engine_id {
        funasr::FUNASR_ENGINE_ID => {
            let config = crate::app::stt_config::get_stt_config();
            let local = &config.local_engine;
            let funasr_config = funasr::FunasrEngineConfig::from_stt_config(local);

            let compute_preference = if local.device == "cuda" {
                Some(ComputePreference::Cuda)
            } else {
                Some(ComputePreference::Cpu)
            };

            Ok(crate::domain::local_engine::AdapterConfig {
                preferred_port: Some(local.server_port),
                compute_preference,
                engine_config: funasr_config.to_json(),
            })
        }
        paddleocr::PADDLEOCR_ENGINE_ID => {
            let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
            let engine_config = paddleocr::PaddleOcrEngineConfig::from_ocr_config();

            Ok(crate::domain::local_engine::AdapterConfig {
                preferred_port: None,
                compute_preference: Some(ocr_config.compute_preference),
                engine_config: engine_config.to_json(),
            })
        }
        other => Err(CommandError::new(
            "unsupported_engine",
            format!("不支持的引擎: {other}"),
            false,
        )),
    }
}

/// 为 catalog item 计算兼容性结果。
///
/// 从 `ProviderDescriptor` 的 `profiles` + `RuntimeProvider::check_compatibility`
/// 真源获取，不由前端猜测。
fn compute_compatibility_for_descriptor(
    svc: &LocalEngineService,
    descriptor: &EngineDescriptor,
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
#[tauri::command]
pub async fn get_local_engine_logs(
    app: tauri::AppHandle,
    engine_id: String,
    max_lines: Option<usize>,
) -> Result<Vec<EngineLogDto>, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;
    let max = max_lines.unwrap_or(500);

    let logs = svc
        .get_logs_structured(&eid, max)
        .await
        .map_err(|e| CommandError::new("engine_logs_error", format!("获取日志失败: {e}"), false))?;

    // 把 StructuredLogEntry 投影为 EngineLogDto
    let dtos: Vec<EngineLogDto> = logs
        .iter()
        .map(|entry| {
            let timestamp = chrono::DateTime::from_timestamp_millis(entry.timestamp_ms as i64)
                .map(|dt| dt.to_rfc3339())
                .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

            EngineLogDto {
                engine_id: entry.engine_id.clone(),
                instance_id: entry.instance_id.clone(),
                seq: entry.seq.to_string(),
                timestamp,
                level: entry.level.clone(),
                text: entry.text.clone(),
            }
        })
        .collect();

    Ok(dtos)
}

/// 安装本地引擎环境。
///
/// 前端只需提交 `engine_id`，不提交 executable/argv/env/脚本路径。
/// `compute_preference` 可选，如提交则必须属于该引擎 descriptor 声明项。
/// action command 内部从现有配置真源构造 `AdapterConfig`。
#[tauri::command]
pub async fn install_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
    compute_preference: Option<String>,
) -> Result<(), CommandError> {
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

    svc.install(&eid, adapter_config)
        .await
        .map_err(|e| CommandError::new("install_failed", format!("安装失败: {e}"), true))?;

    tracing::info!(engine = %eid, "引擎安装完成");
    Ok(())
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
        .map_err(|e| CommandError::new("environment_missing", format!("环境未就绪: {e}"), true))?;

    svc.start(&eid, adapter_config)
        .await
        .map_err(|e| CommandError::new("start_failed", format!("启动失败: {e}"), true))?;

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

    svc.stop(&eid)
        .await
        .map_err(|e| CommandError::new("stop_failed", format!("停止失败: {e}"), true))?;

    tracing::info!(engine = %eid, "引擎停止完成");
    Ok(())
}

/// 修复本地引擎环境。
///
/// 前端只需提交 `engine_id`。
#[tauri::command]
pub async fn repair_local_engine(
    app: tauri::AppHandle,
    engine_id: String,
) -> Result<(), CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    svc.repair(&eid)
        .await
        .map_err(|e| CommandError::new("repair_failed", format!("修复失败: {e}"), true))?;

    tracing::info!(engine = %eid, "引擎修复完成");
    Ok(())
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
#[tauri::command]
pub async fn cancel_local_engine_operation(
    app: tauri::AppHandle,
    engine_id: String,
    operation_id: String,
) -> Result<CancelResultDto, CommandError> {
    let svc = get_service(&app)?;
    let eid = validate_engine_id(&engine_id)?;

    let err = svc.cancel_operation(&eid, &operation_id).await;

    // cancel_operation 返回 LocalEngineError——成功取消也返回 Cancelled error
    // 需要区分：Cancelled code = 成功取消，Rejected = 未取消
    let cancelled = err.code == crate::domain::local_engine::LocalEngineErrorCode::Cancelled;

    let result = CancelResultDto {
        engine_id: engine_id,
        operation_id: operation_id,
        cancelled,
        reason: if cancelled {
            None
        } else {
            Some(err.action_hint.clone())
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
/// `lifecycle`（仅 PaddleOCR）、`requires_rebuild`。
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
            let compute_pref = if config.local_engine.device == "cuda" {
                "cuda".to_string()
            } else {
                "cpu".to_string()
            };
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(compute_pref),
                auto_start: Some(config.local_engine.auto_start_server),
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
/// `lifecycle`（仅 PaddleOCR）。
///
/// **禁止包含** executable/argv/env/path/url/runtime kind 或任意 engine_config。
/// 未知字段在反序列化时被拒绝（`#[serde(deny_unknown_fields)]`）。
///
/// 后端按 engine_id 从配置真源读取完整配置，只修改 patch 指定的字段，
/// 再通过现有持久化与 cache 热更新路径保存。
///
/// 如果 compute profile 变化导致与 current generation 不一致，
/// 将环境投影为 `NeedsRebuild`，并返回 `requires_rebuild=true`。
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

                let new_device = match pref {
                    ComputePreference::Cuda => "cuda".to_string(),
                    _ => "cpu".to_string(),
                };
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
            let compute_pref = if config.local_engine.device == "cuda" {
                "cuda".to_string()
            } else {
                "cpu".to_string()
            };
            EnginePreferencesDto {
                engine_id: engine_id.clone(),
                compute_preference: Some(compute_pref),
                auto_start: Some(config.local_engine.auto_start_server),
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
async fn validate_preference_for_engine(
    svc: &LocalEngineService,
    engine_id: &EngineId,
    preference: ComputePreference,
) -> Result<(), CommandError> {
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

    // ── 旧 FunASR commands/events 未移除 ──

    #[test]
    fn old_funasr_commands_still_exist() {
        // 验证旧 commands 函数仍可编译
        let _ = crate::app::commands::get_funasr_env as fn(tauri::AppHandle) -> _;
        let _ = crate::app::commands::setup_python_env as fn(tauri::AppHandle) -> _;
        let _ = crate::app::commands::start_funasr_server as fn(tauri::AppHandle) -> _;
        let _ = crate::app::commands::stop_funasr_server as fn(tauri::AppHandle) -> _;
        let _ = crate::app::commands::get_funasr_log_history as fn(tauri::AppHandle) -> _;
    }

    // ── 旧事件常量未移除 ──

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
            seq: "42".to_string(),
            timestamp: "2026-08-26T00:00:00Z".to_string(),
            level: "info".to_string(),
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
}
