//! EngineManager 存储扫描与清理用例：
//! scan_storage / cleanup_targets 及其阻塞辅助函数（spawn_blocking 中执行）。

use super::*;

/// 单个清理目标的执行结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupTargetOutcome {
    /// 已删除，释放 bytes。
    Cleaned(u64),
    /// Windows 占用等——已记 cleanup residue（非产品状态），等待后续清理。
    Deferred(u64),
}

impl EngineManager {
    /// 清理引擎资产。
    ///
    /// 前端提交 `target_ids`，后端重新解析每个 target_id，不信任前端提交的路径/size/shared/current。
    ///
    /// 禁止提交任意路径。active 部署不可删除。
    /// 共享资产经过 active manifest 引用检查。
    ///
    /// 清理结束后 `operation` 归位 Idle——结果由本返回值表达。
    pub async fn cleanup_targets(
        &self,
        engine_id: &EngineId,
        target_ids: &[String],
        operation_id: Option<String>,
    ) -> Result<super::super::dto::CleanupResultDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        self.get_entry(engine_id).await?;

        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let _guard = self.coordinator.try_claim(engine_id, &op_id)?;

        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Cleaning,
                operation_id: op_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: false, // cleanup 进入删除阶段后不可取消
            };
        })
        .await?;

        // target 解析 + 磁盘删除（measure/execute_cleanup）都是阻塞 IO——
        // 整体放 spawn_blocking，claim 仍由 guard 持有。
        let eid = engine_id.clone();
        let targets: Vec<String> = target_ids.to_vec();
        let outcomes = tokio::task::spawn_blocking(move || {
            targets
                .into_iter()
                .map(|target_id| {
                    (
                        target_id.clone(),
                        resolve_and_cleanup_target_blocking(&eid, &target_id),
                    )
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Cleanup,
                "清理执行失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        let mut cleaned = Vec::new();
        let mut skipped = Vec::new();
        let mut deferred = Vec::new();
        let mut released: u64 = 0;
        let mut errors = Vec::new();

        for (target_id, outcome) in outcomes {
            match outcome {
                Ok(CleanupTargetOutcome::Cleaned(bytes)) => {
                    released += bytes;
                    cleaned.push(target_id);
                }
                Ok(CleanupTargetOutcome::Deferred(bytes)) => {
                    // Windows 文件占用等——slot 记 residue，等待后续清理
                    released += bytes;
                    deferred.push(target_id);
                }
                Err(e) => {
                    let reason = e.to_string();
                    tracing::warn!(
                        engine = %engine_id,
                        target = %target_id,
                        error = %reason,
                        "cleanup 跳过"
                    );
                    errors.push(format!("{target_id}: {reason}"));
                    skipped.push(target_id);
                }
            }
        }

        // 终态：归位 Idle——清理结果由返回值表达，不留 busy 残留
        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.clear_operation();
        })
        .await?;

        Ok(super::super::dto::CleanupResultDto {
            engine_id: engine_id.to_string(),
            operation_id: op_id,
            cleaned_target_ids: cleaned,
            skipped_target_ids: skipped,
            released_bytes: released,
            deferred_target_ids: deferred,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
        })
    }

    /// 扫描引擎存储——返回所有可诊断/可清理的存储目标。
    ///
    /// 在 `spawn_blocking` 中执行，不阻塞 Tauri 事件线程或启动主链路。
    pub async fn scan_storage(
        &self,
        engine_id: &EngineId,
    ) -> Result<super::super::dto::EngineStorageDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        let engine_id_owned = engine_id.clone();
        let model_ids: Vec<String> = self
            .model_registry
            .list(engine_id)
            .iter()
            .map(|m| m.model_id.clone())
            .collect();
        let result = tokio::task::spawn_blocking(move || {
            scan_engine_storage_blocking(&model_ids, &engine_id_owned)
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "存储扫描失败",
                format!("spawn_blocking panic: {e}"),
            )
        })?;

        result.map_err(|e| from_runtime(ErrorPhase::Request, "存储扫描失败", &e))
    }
}

// ── 存储扫描辅助（spawn_blocking 执行）────────────────────────────────────

/// 阻塞式存储扫描——在 `spawn_blocking` 中执行。
///
/// 扫描引擎环境（engine 级 + 各 implementation 级部署空间）、已安装模型、
/// 引擎私有缓存、共享托管运行时/下载缓存与旧版遗留。目标类别只表达用户
/// 可理解的对象（见 `StorageTargetKindDto`），不暴露 slot/journal/residue
/// 或 provider 内部类型名。
///
/// **空间隔离（0.22.9）**：每个部署空间的 active 指针与 residue 独立读取，
/// target_id 以 `environment:` / `environment:impl-{implementation}:` 前缀
/// 区分；无法映射到闭合枚举的 `impl-*` 目录不产生任何清理目标（fail-closed，
/// 不可删）。模型 payload 仍按 engine + model 管理，与部署空间正交。
fn scan_engine_storage_blocking(
    model_ids: &[String],
    engine_id: &EngineId,
) -> Result<super::super::dto::EngineStorageDto, crate::infra::local_engine::runtime::RuntimeError>
{
    use crate::infra::local_engine::runtime::{ArtifactId, RuntimePlan};

    let mut targets = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut releasable_bytes: u64 = 0;

    // 引擎的全部部署空间（engine 级 + 已知 implementation 级）
    let spaces = DeploymentStore::spaces_for_engine(engine_id)?;

    // ── 1. 引擎环境（各空间的 active 不可删；非 active = 清理残留）──
    for space in &spaces {
        let active_slot = DeploymentStore::read_pointer(space)?.map(|p| p.slot);
        let residue_slots: Vec<String> = DeploymentStore::read_residue(space)?
            .into_iter()
            .map(|r| r.slot)
            .collect();
        // target_id 前缀：engine 级 = `environment:`；
        // implementation 级 = `environment:impl-{wire}:`（按实现独立删除保护）
        let scope_prefix = match space.implementation() {
            None => String::new(),
            Some(implementation) => format!(
                "{}:",
                crate::infra::local_engine::deployment::DeploymentSpace::impl_dir_name(
                    implementation
                )
            ),
        };

        for slot in ["slot-a", "slot-b"] {
            let dir = space.slot_dir(slot);
            if !dir.exists() {
                continue;
            }
            let size = dir_size(&dir);
            total_bytes += size;

            let is_current = active_slot.as_deref() == Some(slot);
            let removable = !is_current;
            if removable {
                releasable_bytes += size;
            }

            let space_label = match space.implementation() {
                None => String::new(),
                Some(implementation) => format!("（{}）", implementation.as_str()),
            };
            let label_fallback = if is_current {
                format!("当前环境{space_label}（不可删除）")
            } else if residue_slots.iter().any(|s| s == slot) {
                format!("环境清理残留{space_label}（被占用）")
            } else {
                format!("残留环境{space_label}")
            };

            targets.push(super::super::dto::StorageTargetDto {
                target_id: format!("environment:{scope_prefix}{slot}"),
                kind: super::super::dto::StorageTargetKindDto::EngineEnvironment,
                engine_id: Some(engine_id.to_string()),
                label_key: "local_engine.storage.engine_environment".to_string(),
                label_fallback,
                size_bytes: size,
                current: is_current,
                removable,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: if is_current {
                    Some("current_environment".to_string())
                } else {
                    None
                },
                affected_engine_ids: None,
                reference_count: None,
                path_display: Some(dir.display().to_string()),
            });
        }
    }

    // ── 2. 事务构建残留（staging）——引擎全部空间的私有缓存 ──
    let mut staging_size: u64 = 0;
    for space in &spaces {
        let staging = space.staging_dir();
        if staging.exists() {
            staging_size += dir_size(&staging);
        }
    }
    if staging_size > 0 {
        total_bytes += staging_size;
        releasable_bytes += staging_size;

        targets.push(super::super::dto::StorageTargetDto {
            target_id: "cache:staging".to_string(),
            kind: super::super::dto::StorageTargetKindDto::EngineCache,
            engine_id: Some(engine_id.to_string()),
            label_key: "local_engine.storage.engine_staging".to_string(),
            label_fallback: "事务构建残留".to_string(),
            size_bytes: staging_size,
            current: false,
            removable: true,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: None,
            affected_engine_ids: None,
            reference_count: None,
            path_display: Some(runtime::engine_root(engine_id).display().to_string()),
        });
    }

    // ── 3. 引擎自有模型目录——已安装模型资产之外的孤儿残留 ──
    // `models/{engine}` 同时是已安装模型的资产根：统计与清理都必须排除
    // 托管资产目录，否则已安装模型会被重复计入"可清理"（虚标 1.1 GB 的根因）。
    let model_cache_dir = runtime::engine_model_cache_dir(engine_id);
    if model_cache_dir.exists() {
        let orphan_bytes = model_cache_orphan_bytes(&model_cache_dir);
        if orphan_bytes > 0 {
            total_bytes += orphan_bytes;
            releasable_bytes += orphan_bytes;

            targets.push(super::super::dto::StorageTargetDto {
                target_id: "cache:model_cache".to_string(),
                kind: super::super::dto::StorageTargetKindDto::EngineCache,
                engine_id: Some(engine_id.to_string()),
                label_key: "local_engine.storage.engine_model_cache".to_string(),
                label_fallback: "模型缓存残留".to_string(),
                size_bytes: orphan_bytes,
                current: false,
                removable: true,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: None,
                affected_engine_ids: None,
                reference_count: None,
                path_display: Some(model_cache_dir.display().to_string()),
            });
        }
    }

    // ── 4. 已安装模型（删除走模型管理的引用检查，不在存储清理中删除） ──
    for model_id in model_ids {
        let asset_key = mstore::encode_asset_key(model_id);
        let state = mstore::restore_model_state(engine_id, &asset_key);
        let installed = matches!(
            state,
            Ok(mstore::RestoredModelState::Installed { .. })
                | Ok(mstore::RestoredModelState::Corrupted { .. })
        );
        if !installed {
            continue;
        }
        let Ok(asset_dir) = mstore::asset_root(engine_id, &asset_key) else {
            continue;
        };
        let size = dir_size(&asset_dir);
        total_bytes += size;

        targets.push(super::super::dto::StorageTargetDto {
            target_id: format!("model:{model_id}"),
            kind: super::super::dto::StorageTargetKindDto::InstalledModel,
            engine_id: Some(engine_id.to_string()),
            label_key: "local_engine.storage.installed_model".to_string(),
            label_fallback: format!("已安装模型 {model_id}"),
            size_bytes: size,
            current: false,
            removable: false,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: Some("model_managed".to_string()),
            affected_engine_ids: None,
            reference_count: None,
            path_display: Some(asset_dir.display().to_string()),
        });
    }

    // ── 5. 共享托管运行时 ──
    // 引用真源 = 各引擎 active 部署 manifest（无独立 refcount 数据）
    let shared_root = runtime::runtimes_root().join("shared");
    if shared_root.exists() {
        for provider_entry in std::fs::read_dir(&shared_root)? {
            let provider_entry = provider_entry?;
            if !provider_entry.file_type()?.is_dir() {
                continue;
            }
            let provider_name = provider_entry.file_name();
            let provider_str = provider_name.to_string_lossy().to_string();
            let runtime_kind = match provider_str.as_str() {
                "python_venv" => RuntimePlan::PythonVenv,
                "managed_binary" => RuntimePlan::ManagedBinary,
                _ => continue,
            };

            for artifact_entry in std::fs::read_dir(provider_entry.path())? {
                let artifact_entry = artifact_entry?;
                if !artifact_entry.file_type()?.is_dir() {
                    continue;
                }
                let artifact_name = match artifact_entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                let artifact_id = match ArtifactId::new(&artifact_name) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let artifact_dir = artifact_entry.path();
                let size = dir_size(&artifact_dir);
                total_bytes += size;

                // 扫描 active 部署 manifest 引用
                let refs = runtime::scan_artifact_references(runtime_kind, &artifact_id)
                    .unwrap_or_default();
                let ref_count = refs.len() as u32;
                let affected: Vec<String> = refs
                    .iter()
                    .map(|r| r.engine_id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let is_shared = affected.len() > 1 || !affected.contains(&engine_id.to_string());
                let removable = ref_count == 0 || !is_shared;
                let blocked = if !removable {
                    Some(format!("被 {} 个引擎引用", ref_count))
                } else {
                    None
                };

                if removable {
                    releasable_bytes += size;
                }

                let target_id = format!(
                    "shared_runtime:{}:{}",
                    runtime_kind.provider_id(),
                    artifact_name
                );

                targets.push(super::super::dto::StorageTargetDto {
                    target_id,
                    kind: super::super::dto::StorageTargetKindDto::SharedRuntime,
                    engine_id: None,
                    label_key: "local_engine.storage.shared_runtime".to_string(),
                    label_fallback: format!("共享托管运行时 ({artifact_name})"),
                    size_bytes: size,
                    current: false,
                    removable,
                    shared: true,
                    requires_separate_confirmation: true,
                    blocked_reason: blocked,
                    affected_engine_ids: Some(affected),
                    reference_count: Some(ref_count),
                    path_display: Some(artifact_dir.display().to_string()),
                });
            }
        }
    }

    // ── 6. 共享下载缓存 ──
    let uv_cache = runtime::uv_cache_dir();
    if uv_cache.exists() {
        let size = dir_size(&uv_cache);
        total_bytes += size;
        releasable_bytes += size;

        targets.push(super::super::dto::StorageTargetDto {
            target_id: "shared_download_cache:python_venv".to_string(),
            kind: super::super::dto::StorageTargetKindDto::SharedDownloadCache,
            engine_id: None,
            label_key: "local_engine.storage.shared_download_cache".to_string(),
            label_fallback: "共享下载缓存".to_string(),
            size_bytes: size,
            current: false,
            removable: true,
            shared: true,
            requires_separate_confirmation: true,
            blocked_reason: None,
            affected_engine_ids: None,
            reference_count: None,
            path_display: Some(uv_cache.display().to_string()),
        });
    }

    // ── 7. 旧版遗留资产 ──
    // 旧版 ModelScope 用户级公共缓存——仅在确有诊断价值时展示
    if engine_id.as_str() == "funasr"
        && let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"))
        && legacy_dir.exists()
    {
        let size = dir_size(&legacy_dir);
        if size > 0 {
            total_bytes += size;

            // legacy 资产不自动标记为 removable——需要单独确认
            targets.push(super::super::dto::StorageTargetDto {
                target_id: "legacy:modelscope".to_string(),
                kind: super::super::dto::StorageTargetKindDto::LegacyAsset,
                engine_id: Some(engine_id.to_string()),
                label_key: "local_engine.storage.legacy_modelscope".to_string(),
                label_fallback: "旧版 ModelScope 缓存残留".to_string(),
                size_bytes: size,
                current: false,
                removable: false,
                shared: true,
                requires_separate_confirmation: true,
                blocked_reason: Some("需单独确认和手动清理".to_string()),
                affected_engine_ids: None,
                reference_count: None,
                path_display: Some(legacy_dir.display().to_string()),
            });
        }
    }

    // ── 8. 已退役实现的部署空间（handoff-11：ParaformerOnline ONNX 退役）──
    // 不自动删除；在残留清理列表中暴露，由用户显式一键清理。
    if engine_id.as_str() == "funasr" {
        let retired_dir = crate::infra::local_engine::runtime::engine_root(engine_id)
            .join("impl-paraformer_onnx_worker");
        if retired_dir.exists() {
            let size = dir_size(&retired_dir);
            if size > 0 {
                total_bytes += size;
                releasable_bytes += size;

                targets.push(super::super::dto::StorageTargetDto {
                    target_id: "legacy:retired-paraformer-onnx".to_string(),
                    kind: super::super::dto::StorageTargetKindDto::LegacyAsset,
                    engine_id: Some(engine_id.to_string()),
                    label_key: "local_engine.storage.retired_paraformer_onnx".to_string(),
                    label_fallback: "已退役的 Paraformer-Online ONNX 资产".to_string(),
                    size_bytes: size,
                    current: false,
                    removable: true,
                    shared: false,
                    requires_separate_confirmation: true,
                    blocked_reason: None,
                    affected_engine_ids: None,
                    reference_count: None,
                    path_display: Some(retired_dir.display().to_string()),
                });
            }
        }
    }

    Ok(super::super::dto::EngineStorageDto {
        engine_id: Some(engine_id.to_string()),
        targets,
        total_size_bytes: total_bytes,
        releasable_size_bytes: releasable_bytes,
    })
}

/// 测量 cleanup scope 的字节数（不执行删除）。
/// 解析 target_id 并执行清理（**阻塞**——磁盘删除，须在 spawn_blocking 中调用）。
///
/// target_id 格式：
/// - `environment:{slot}` — engine 级空间的非 active 引擎环境
/// - `environment:impl-{implementation}:{slot}` — implementation 级空间的
///   非 active 引擎环境（active 删除保护按 implementation 独立生效）
/// - `cache:staging` — 事务构建残留（引擎全部空间）
/// - `cache:model_cache` — 引擎模型目录中的孤儿残留（已安装模型资产受保护）
/// - `shared_runtime:{runtime_kind}:{artifact_id}` — 共享托管运行时
/// - `shared_download_cache:{runtime_kind}` — 共享下载缓存
/// - `model:{model_id}` — 已安装模型（拒绝：删除走模型管理）
/// - `legacy:{kind}` — 旧版遗留资产（拒绝自动清理）
/// - `legacy:retired-paraformer-onnx` — 已退役 ONNX 部署空间（目录级整删）
fn resolve_and_cleanup_target_blocking(
    engine_id: &EngineId,
    target_id: &str,
) -> Result<CleanupTargetOutcome, crate::infra::local_engine::runtime::RuntimeError> {
    use crate::infra::local_engine::deployment::DeploymentSpace;
    use crate::infra::local_engine::providers::execute_cleanup;
    use crate::infra::local_engine::runtime::CleanupScope;

    // `environment:` 目标解析：engine 级 / implementation 级空间
    // （`environment:impl-{wire}:{slot}`；未知 implementation fail-closed 拒绝）
    if let Some(rest) = target_id.strip_prefix("environment:") {
        let (space, slot) = if let Some(impl_rest) = rest.strip_prefix("impl-") {
            let (impl_wire, slot) = impl_rest.split_once(':').ok_or_else(|| {
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: format!("无效的 environment target_id: {target_id}"),
                }
            })?;
            let implementation = DeploymentSpace::parse_impl_dir_name(&format!("impl-{impl_wire}"))
                .ok_or_else(|| {
                    crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                        message: format!("未知的 implementation 部署空间: impl-{impl_wire}"),
                    }
                })?;
            (
                DeploymentSpace::resolve(engine_id, implementation),
                slot.to_string(),
            )
        } else {
            (DeploymentSpace::engine(engine_id), rest.to_string())
        };

        runtime::validate_slot_name(&slot)?;

        // active slot 不可删除（只看目标空间自己的指针——删除保护按 implementation 生效）
        let active = DeploymentStore::read_pointer(&space)?;
        if active.as_ref().is_some_and(|p| p.slot == slot) {
            return Err(
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: "active 部署不可删除".to_string(),
                },
            );
        }

        let scope = CleanupScope::EngineDeploymentSlot { space, slot };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        // delete_slot_if_not_active 占用时记 residue（Ok(false)）——
        // 查询目标空间 residue 判断是否残留
        let deferred = match &scope {
            CleanupScope::EngineDeploymentSlot { space, slot } => {
                DeploymentStore::read_residue(space)?
                    .iter()
                    .any(|r| &r.slot == slot)
            }
            _ => false,
        };
        if deferred {
            Ok(CleanupTargetOutcome::Deferred(size))
        } else {
            Ok(CleanupTargetOutcome::Cleaned(size))
        }
    } else if target_id == "cache:staging" {
        let scope = CleanupScope::EngineStaging {
            engine_id: engine_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if target_id == "cache:model_cache" {
        let scope = CleanupScope::EngineModelCache {
            engine_id: engine_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if let Some(rest) = target_id.strip_prefix("shared_runtime:") {
        // shared:{runtime_kind}:{artifact_id}
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: format!("无效的 shared target_id: {target_id}"),
                },
            );
        }
        let runtime_kind = match parts[0] {
            "python_venv" => crate::infra::local_engine::runtime::RuntimePlan::PythonVenv,
            "managed_binary" => crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary,
            _ => {
                return Err(
                    crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                        message: format!("未知的 runtime_kind: {}", parts[0]),
                    },
                );
            }
        };
        let artifact_id =
            crate::infra::local_engine::runtime::ArtifactId::new(parts[1]).map_err(|e| {
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: format!("无效的 artifact id: {e}"),
                }
            })?;
        let scope = CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id: artifact_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if let Some(kind) = target_id.strip_prefix("shared_download_cache:") {
        let runtime_kind = match kind {
            "python_venv" => crate::infra::local_engine::runtime::RuntimePlan::PythonVenv,
            "managed_binary" => crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary,
            _ => {
                return Err(
                    crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                        message: format!("未知的 runtime_kind: {kind}"),
                    },
                );
            }
        };
        let scope = CleanupScope::ProviderDownloadCache { runtime_kind };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if target_id.starts_with("model:") {
        // 已安装模型不通过存储清理删除——引用检查/selected/active
        // 冲突规则在 EngineManager::delete_model 统一裁决
        Err(
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: "模型删除请使用模型管理（含引用检查）".to_string(),
            },
        )
    } else if target_id == "legacy:retired-paraformer-onnx" {
        // handoff-11：已退役 ParaformerOnline ONNX 部署空间——目录级整删
        //（实现已退役，不存在 active 引用；内容为 ORT DLL + 模型资产）
        let dir = crate::infra::local_engine::runtime::engine_root(engine_id)
            .join("impl-paraformer_onnx_worker");
        if !dir.exists() {
            return Ok(CleanupTargetOutcome::Cleaned(0));
        }
        let size = dir_size(&dir);
        std::fs::remove_dir_all(&dir).map_err(|e| {
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: format!("删除已退役 ONNX 部署空间失败: {e}"),
            }
        })?;
        tracing::info!(
            engine_id = %engine_id,
            path = %dir.display(),
            size_bytes = size,
            "已清理退役 Paraformer-Online ONNX 部署空间"
        );
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if target_id.starts_with("legacy:") {
        // legacy 资产——只清理可证明归属的
        // 目前不自动清理 legacy，只标记
        Err(
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: "legacy 资产需要手动确认和单独清理".to_string(),
            },
        )
    } else {
        Err(
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: format!("未知/无效的 target_id: {target_id}"),
            },
        )
    }
}

fn measure_cleanup_scope(scope: &crate::infra::local_engine::runtime::CleanupScope) -> u64 {
    use crate::infra::local_engine::runtime::CleanupScope;

    match scope {
        CleanupScope::EngineDeploymentSlot { space, slot } => dir_size(&space.slot_dir(slot)),
        CleanupScope::EngineStaging { engine_id } => {
            // 引擎全部空间的 staging（engine 级 + implementation 级）
            DeploymentStore::spaces_for_engine(engine_id)
                .map(|spaces| {
                    spaces
                        .iter()
                        .map(|s| dir_size(&s.staging_dir()))
                        .sum::<u64>()
                })
                .unwrap_or(0)
        }
        CleanupScope::EngineModelCache { engine_id } => {
            let dir = crate::infra::local_engine::runtime::engine_model_cache_dir(engine_id);
            model_cache_orphan_bytes(&dir)
        }
        CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id,
        } => {
            let dir = crate::infra::local_engine::runtime::shared_artifact_dir(
                *runtime_kind,
                artifact_id,
            );
            dir_size(&dir)
        }
        CleanupScope::ProviderDownloadCache { runtime_kind } => match runtime_kind {
            crate::infra::local_engine::runtime::RuntimePlan::PythonVenv => {
                let dir = crate::infra::local_engine::runtime::uv_cache_dir();
                dir_size(&dir)
            }
            crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary => 0,
            crate::infra::local_engine::runtime::RuntimePlan::OnnxRuntime => 0,
        },
    }
}

/// `models/{engine}` 中非托管子项的总字节数（孤儿目录 / 安装残留）。
///
/// 托管判定用 `mstore::is_managed_asset_dir`——与 `execute_cleanup` 的
/// 删除保护同一条规则，保证"扫描显示可清理的字节"与"清理实际删除的字节"一致。
fn model_cache_orphan_bytes(model_cache_dir: &std::path::Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(model_cache_dir) else {
        return 0;
    };
    let mut total: u64 = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && mstore::is_managed_asset_dir(&path) {
            continue;
        }
        total += dir_size(&path);
    }
    total
}

/// 递归计算目录/文件大小（字节数）。
fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    if path.is_file() {
        return std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for entry in entries.flatten() {
                let path = entry.path();
                match entry.file_type() {
                    Ok(t) if t.is_dir() => stack.push(path),
                    Ok(t) if t.is_file() => {
                        total += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                    }
                    _ => {}
                }
            }
        }
    }
    total
}
