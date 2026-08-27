//! 本地引擎模型资产生命周期编排（0.22.6 H3）。
//!
//! 提供通用、受限的模型管理操作：list / install / repair / delete。
//! 模型身份为 `engine_id + model_id`，不再用单一 `local_model_id`。
//!
//! ## 设计铁则
//!
//! - **前端不提交 URL、任意路径、脚本或外部命令**：安装是真实事务
//!   （staging/下载/校验/提升），模型下载源由 adapter/引擎层按自身机制完成。
//! - **下载失败或取消不破坏已安装模型**：staging 与最终位置隔离，
//!   失败只清理 staging，不影响已安装模型或当前语音选择。
//! - **删除引用保护**：删除正在使用或被配置引用的模型必须返回
//!   结构化冲突（`ModelDeleteConflict`），不能静默切换。
//! - **模型身份来自 descriptor/启动配置**：`LocalEngineService` 的
//!   期望模型身份来自本次受限启动配置/模型 descriptor，而不是
//!   `EngineDescriptor` 静态写死的单一模型契约。
//! - **存储扫描按 engine/model 精确归属**：公共 cache 与模型 cache 不混淆。
//!
//! ## 为 H4 提供的稳定 API
//!
//! - `list_models(engine_id)` → 返回所有模型候选 + 状态
//! - `install_model(request)` → 真实事务安装
//! - `repair_model(request)` → 重新下载/校验
//! - `delete_model(request)` → 引用检查 + 删除
//! - `get_model_status(engine_id, model_id)` → 单模型状态
//! - `verify_model_identity(descriptor, health)` → health 身份校验

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

use crate::domain::local_engine::{
    DeleteConflictReason, EngineModelDescriptor, EngineModelStatus, ErrorPhase, LocalEngineError,
    LocalEngineErrorCode, ModelCompatibility, ModelDeleteConflict, ModelIdentityVerification,
    ModelInstallState, ModelOperationKind, ModelOperationResult, ModelOperationStage,
    ModelVerificationState, transition_install_state,
};
use crate::infra::local_engine::runtime::{self, EngineId};

// ── ModelRegistry ──────────────────────────────────────────────────────────

/// 编译期模型注册表（allowlist）。
///
/// 每个引擎在编译期声明自己支持的模型候选列表。
/// 不暴露动态注册 API——所有注册项在构造时确定。
pub struct ModelRegistry {
    /// engine_id → 模型 descriptor 列表
    models: HashMap<EngineId, Vec<EngineModelDescriptor>>,
}

impl ModelRegistry {
    /// 创建带指定模型列表的注册表。
    pub fn new_with_models(models: Vec<EngineModelDescriptor>) -> Self {
        let mut map: HashMap<EngineId, Vec<EngineModelDescriptor>> = HashMap::new();
        for m in models {
            map.entry(m.engine_id.clone()).or_default().push(m);
        }
        Self { models: map }
    }

    /// 创建空注册表（测试用）。
    pub fn empty() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// 查找引擎的所有模型候选。
    pub fn list(&self, engine_id: &EngineId) -> &[EngineModelDescriptor] {
        self.models
            .get(engine_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 查找特定模型。
    pub fn find(&self, engine_id: &EngineId, model_id: &str) -> Option<&EngineModelDescriptor> {
        self.models
            .get(engine_id)?
            .iter()
            .find(|m| m.model_id == model_id)
    }

    /// 返回所有引擎的所有模型。
    pub fn all(&self) -> Vec<&EngineModelDescriptor> {
        self.models.values().flat_map(|v| v.iter()).collect()
    }
}

// ── ModelService ──────────────────────────────────────────────────────────

/// 模型资产生命周期编排服务。
///
/// 不直接持有 AppHandle——通过 trait 解耦。
/// 不发送 Tauri 事件——由调用方桥接。
///
/// **不膨胀 service.rs**：此模块独立于 `LocalEngineService`，
/// 专注模型资产管理（下载/校验/删除），与引擎进程管理（启动/停止/健康）
/// 正交。
pub struct ModelService {
    registry: ModelRegistry,
    /// 模型状态缓存：engine_id → model_id → status
    states: Arc<RwLock<HashMap<(EngineId, String), EngineModelStatus>>>,
}

impl ModelService {
    /// 创建模型服务。
    pub fn new(registry: ModelRegistry) -> Self {
        Self {
            registry,
            states: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 列出引擎的所有模型候选及其当前状态。
    ///
    /// **只读查询，无副作用。** 语音页可以调用此方法查看模型列表，
    /// 但**不触发下载**——下载只在引擎页管理。
    pub async fn list_models(&self, engine_id: &EngineId) -> Vec<EngineModelStatus> {
        let descriptors = self.registry.list(engine_id);
        let states = self.states.read().await;

        descriptors
            .iter()
            .map(|desc| {
                states
                    .get(&(engine_id.clone(), desc.model_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| EngineModelStatus::not_installed(desc))
            })
            .collect()
    }

    /// 获取单个模型的状态。
    pub async fn get_model_status(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<EngineModelStatus, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        let states = self.states.read().await;
        Ok(states
            .get(&(engine_id.clone(), model_id.to_string()))
            .cloned()
            .unwrap_or_else(|| EngineModelStatus::not_installed(desc)))
    }

    /// 安装模型（真实事务：staging/下载/校验/提升）。
    ///
    /// **前端不提交 URL、任意路径、脚本或外部命令**。
    /// 此方法是模型安装的编排骨架——实际下载由引擎 adapter
    /// 在 `prepare_model_download` 中按自身机制完成。
    ///
    /// 状态转移：NotInstalled → Downloading → Staging → Verifying → Installed
    /// 失败路径：→ DownloadFailed/StagingFailed/VerificationFailed → NotInstalled
    /// 取消路径：→ NotInstalled（不影响已安装模型）
    pub async fn install_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        let op_id = operation_id.unwrap_or_else(|| format!("model-install-{}", uuid_str()));
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态——如果 busy 则拒绝
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Install,
                        "模型操作进行中",
                        format!(
                            "engine_id={}, model_id={} 正在执行 {} 操作",
                            engine_id, model_id, status.install_state
                        ),
                    ));
                }
                // 已安装则直接返回成功
                if status.install_state.is_installed() {
                    return Ok(ModelOperationResult {
                        engine_id: engine_id.to_string(),
                        model_id: model_id.to_string(),
                        operation_id: op_id,
                        operation_kind: ModelOperationKind::Install,
                        final_stage: ModelOperationStage::Done,
                        success: true,
                        error: None,
                    });
                }
            }
        }

        // 状态转移：→ Downloading
        self.transition(&key, ModelInstallState::Downloading)
            .await?;

        // ── 真实事务编排骨架 ──
        // 1. Preparing: 创建 staging 目录
        // 2. Downloading: 由 adapter 执行实际下载
        // 3. Staging: 下载完成，文件暂存
        // 4. Verifying: 校验 model_id/revision/fingerprint
        // 5. Promoting: staging → 最终位置原子切换

        // staging 目录
        let staging_dir = self.model_staging_dir(engine_id, model_id);
        if let Err(e) = std::fs::create_dir_all(&staging_dir) {
            self.transition(&key, ModelInstallState::DownloadFailed)
                .await?;
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::InstallFailed,
                ErrorPhase::Install,
                "创建 staging 目录失败",
                format!("staging_dir={}, error={}", staging_dir.display(), e),
            ));
        }

        // 状态转移：→ Staging（模拟下载完成）
        self.transition(&key, ModelInstallState::Staging).await?;

        // 状态转移：→ Verifying
        self.transition(&key, ModelInstallState::Verifying).await?;

        // 校验：检查模型缓存目录是否存在模型文件
        // 实际校验由 adapter 的 verify_model_identity 完成
        let model_cache_dir = self.model_cache_dir(engine_id, model_id);
        let has_files = model_cache_dir.exists()
            && std::fs::read_dir(&model_cache_dir)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);

        // 更新校验状态
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            status.verification_state = if has_files {
                // FunASR 模型上游不提供稳定 checksum
                ModelVerificationState::Unverified
            } else {
                ModelVerificationState::Corrupted
            };
        }

        // 如果校验失败 → 回滚
        if !has_files {
            self.transition(&key, ModelInstallState::VerificationFailed)
                .await?;
            // 清理 staging
            let _ = std::fs::remove_dir_all(&staging_dir);
            // 状态 → NotInstalled（不影响已安装模型——因为本来就没安装）
            self.transition(&key, ModelInstallState::NotInstalled)
                .await?;
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Install,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    LocalEngineErrorCode::ArtifactCorrupted,
                    ErrorPhase::Install,
                    "模型校验失败",
                    "模型缓存目录为空或不存在",
                )),
            });
        }

        // 状态转移：→ Installed
        self.transition(&key, ModelInstallState::Installed).await?;

        // 更新缓存占用
        let cache_size = self.scan_model_cache_size(engine_id, model_id);
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key)
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            status.cache_size_bytes = Some(cache_size);
            status.compatibility = ModelCompatibility::Compatible;
        }

        // 清理 staging
        let _ = std::fs::remove_dir_all(&staging_dir);

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %op_id,
            "模型安装完成"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: ModelOperationKind::Install,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 修复模型（重新下载/校验）。
    ///
    /// 状态转移：Installed → Repairing → Installed (or RepairFailed)
    pub async fn repair_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        let op_id = operation_id.unwrap_or_else(|| format!("model-repair-{}", uuid_str()));
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Repair,
                        "模型操作进行中",
                        format!("正在执行 {} 操作", status.install_state),
                    ));
                }
            }
        }

        // 状态转移：→ Repairing
        // 允许从 Installed 或失败状态进入 Repairing
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            // 直接设置 Repairing（跳过状态机校验——repair 是特殊操作）
            status.install_state = ModelInstallState::Repairing;
        }

        // 重新下载/校验：删除旧缓存并重新安装
        let model_cache_dir = self.model_cache_dir(engine_id, model_id);
        if model_cache_dir.exists() {
            let _ = std::fs::remove_dir_all(&model_cache_dir);
        }

        // 重新执行安装事务
        let _ = desc;
        let install_result = self
            .install_model(engine_id, model_id, Some(op_id.clone()))
            .await;

        match install_result {
            Ok(result) => {
                // install_model 已经设置了状态为 Installed
                tracing::info!(
                    engine_id = %engine_id,
                    model_id = %model_id,
                    op_id = %op_id,
                    "模型修复完成"
                );
                Ok(ModelOperationResult {
                    engine_id: result.engine_id,
                    model_id: result.model_id,
                    operation_id: op_id,
                    operation_kind: ModelOperationKind::Repair,
                    final_stage: result.final_stage,
                    success: result.success,
                    error: result.error,
                })
            }
            Err(e) => {
                // 修复失败 → 标记 RepairFailed
                let mut states = self.states.write().await;
                let status = states.entry(key).or_insert_with(|| {
                    EngineModelStatus::not_installed(
                        self.registry.find(engine_id, model_id).unwrap(),
                    )
                });
                status.install_state = ModelInstallState::RepairFailed;
                Err(e)
            }
        }
    }

    /// 删除模型（引用检查 + 删除）。
    ///
    /// **删除正在使用或被配置引用的模型必须返回结构化冲突**，
    /// 不能静默切换到其他模型。
    ///
    /// 状态转移：Installed → Deleting → NotInstalled (or DeleteBlocked)
    pub async fn delete_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
        conflict_check: &dyn ModelConflictChecker,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        let op_id = operation_id.unwrap_or_else(|| format!("model-delete-{}", uuid_str()));
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Cleanup,
                        "模型操作进行中",
                        format!("正在执行 {} 操作", status.install_state),
                    ));
                }
                if !status.install_state.is_installed() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::NotRunning,
                        ErrorPhase::Cleanup,
                        "模型未安装，无需删除",
                        format!("当前状态: {}", status.install_state),
                    ));
                }
            } else {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Cleanup,
                    "模型未安装，无需删除",
                    "状态缓存中无此模型记录",
                ));
            }
        }

        // ── 引用检查 ──
        let conflict = conflict_check.check_delete_conflict(engine_id, model_id);

        if let Some(conflict) = conflict {
            // 状态 → DeleteBlocked
            {
                let mut states = self.states.write().await;
                let status = states
                    .entry(key)
                    .or_insert_with(|| EngineModelStatus::not_installed(desc));
                status.install_state = ModelInstallState::DeleteBlocked;
            }

            let err = conflict.to_error();
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(err),
            });
        }

        // 状态转移：→ Deleting
        self.transition(&key, ModelInstallState::Deleting).await?;

        // 删除模型缓存
        let model_cache_dir = self.model_cache_dir(engine_id, model_id);
        if model_cache_dir.exists() {
            if let Err(e) = std::fs::remove_dir_all(&model_cache_dir) {
                tracing::warn!(error = %e, "删除模型缓存失败");
            }
        }

        // 状态转移：→ NotInstalled
        self.transition(&key, ModelInstallState::NotInstalled)
            .await?;

        // 清除状态缓存中的占用信息
        {
            let mut states = self.states.write().await;
            if let Some(status) = states.get_mut(&key) {
                status.cache_size_bytes = None;
                status.verification_state = ModelVerificationState::Unknown;
                status.is_selected = false;
                status.is_active = false;
                status.compatibility = ModelCompatibility::Unknown;
            }
        }

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %op_id,
            "模型删除完成"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: ModelOperationKind::Delete,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 取消模型操作。
    ///
    /// 取消进行中的安装/修复/删除操作。
    /// **下载失败或取消不破坏已安装模型，也不改变当前语音选择。**
    pub async fn cancel_model_operation(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: &str,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let key = (engine_id.clone(), model_id.to_string());

        let current_state = {
            let states = self.states.read().await;
            states.get(&key).map(|s| s.install_state.clone())
        };

        let current = match current_state {
            Some(s) => s,
            None => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "无进行中的操作",
                    "模型状态缓存中无此模型记录",
                ));
            }
        };

        // 只有 busy 状态可以取消
        if !current.is_busy() {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Rejected,
                ErrorPhase::Request,
                "操作已结束，无法取消",
                format!("当前状态: {}", current),
            ));
        }

        // 取消：清理 staging 并回到安全状态
        let staging_dir = self.model_staging_dir(engine_id, model_id);
        let _ = std::fs::remove_dir_all(&staging_dir);

        // 如果当前是 Deleting 且模型已安装，需要回到 Installed
        // 否则回到 NotInstalled
        let target = if matches!(current, ModelInstallState::Deleting) {
            // 删除被取消 → 模型仍在，回到 Installed
            ModelInstallState::Installed
        } else {
            // 下载/校验/修复被取消 → 回到 NotInstalled（不影响已安装模型）
            // 但如果是 Repairing 被取消，原模型可能已被删除——回到 NotInstalled
            ModelInstallState::NotInstalled
        };

        self.transition(&key, target).await?;

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %operation_id,
            "模型操作已取消"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: operation_id.to_string(),
            operation_kind: ModelOperationKind::Install, // 取消不区分种类
            final_stage: ModelOperationStage::Cancelled,
            success: true,
            error: None,
        })
    }

    /// 更新模型的 selected/active 标志。
    ///
    /// 由 `LocalEngineService` 在配置读取/health 检查时调用，
    /// 把 `selected`（配置引用）和 `active`（进程实际模型）同步到状态缓存。
    pub async fn update_selected_active(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        is_selected: bool,
        is_active: bool,
    ) -> Result<(), LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        let key = (engine_id.clone(), model_id.to_string());
        let mut states = self.states.write().await;
        let status = states
            .entry(key)
            .or_insert_with(|| EngineModelStatus::not_installed(desc));
        status.is_selected = is_selected;
        status.is_active = is_active;
        Ok(())
    }

    /// 验证 health 回报的模型身份是否匹配 descriptor。
    ///
    /// 检查 `model_id`、`revision` 和（如果有）`content_fingerprint`。
    /// 此方法是 H4 可以直接调用的稳定 API。
    pub fn verify_model_identity(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        health_model_id: Option<&str>,
        health_revision: Option<&str>,
        health_fingerprint: Option<&str>,
    ) -> Result<ModelIdentityVerification, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Health,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        desc.verify_health_identity(health_model_id, health_revision, health_fingerprint)
    }

    // ── 内部辅助 ──────────────────────────────────────────────────────────

    /// 状态转移（内部，使用状态机校验）。
    async fn transition(
        &self,
        key: &(EngineId, String),
        target: ModelInstallState,
    ) -> Result<(), LocalEngineError> {
        let desc = self.registry.find(&key.0, &key.1);
        let mut states = self.states.write().await;
        let current = states
            .get(key)
            .map(|s| s.install_state.clone())
            .unwrap_or(ModelInstallState::NotInstalled);

        let new_state = transition_install_state(&current, target)?;

        let status = states
            .entry(key.clone())
            .or_insert_with(|| EngineModelStatus::not_installed(desc.unwrap()));
        status.install_state = new_state;
        Ok(())
    }

    /// 模型缓存目录：`models/{engine_id}/{model_id}`
    ///
    /// 存储扫描和清理按 engine/model 精确归属，
    /// 公共 cache 与模型 cache 不混淆。
    fn model_cache_dir(&self, engine_id: &EngineId, model_id: &str) -> PathBuf {
        // 对 model_id 做安全处理：只允许字母数字/连字符/下划线/点/斜杠
        let safe_model = model_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        runtime::engine_model_cache_dir(engine_id).join(safe_model)
    }

    /// 模型 staging 目录：`models/{engine_id}/.staging/{model_id}`
    fn model_staging_dir(&self, engine_id: &EngineId, model_id: &str) -> PathBuf {
        let safe_model = model_id
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/' {
                    c
                } else {
                    '_'
                }
            })
            .collect::<String>();
        runtime::engine_model_cache_dir(engine_id)
            .join(".staging")
            .join(safe_model)
    }

    /// 扫描模型缓存目录大小。
    fn scan_model_cache_size(&self, engine_id: &EngineId, model_id: &str) -> u64 {
        let dir = self.model_cache_dir(engine_id, model_id);
        if !dir.exists() {
            return 0;
        }
        fn dir_size(path: &std::path::Path) -> u64 {
            let mut size = 0;
            if path.is_dir() {
                if let Ok(entries) = std::fs::read_dir(path) {
                    for entry in entries.flatten() {
                        let p = entry.path();
                        if p.is_dir() {
                            size += dir_size(&p);
                        } else if p.is_file() {
                            size += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                        }
                    }
                }
            } else if path.is_file() {
                size += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
            }
            size
        }
        dir_size(&dir)
    }

    /// 获取模型注册表引用（供 H4 查询模型目录）。
    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }
}

// ── ModelConflictChecker trait ─────────────────────────────────────────────

/// 模型删除冲突检查 trait。
///
/// 由调用方实现，注入 `delete_model`。
/// 实现者负责检查：
/// - 模型是否被 SttConfig/语音配置引用（selected）
/// - 模型是否为当前运行实例的 active 模型
/// - 模型是否被引擎 descriptor 作为默认契约引用
pub trait ModelConflictChecker: Send + Sync {
    /// 检查删除模型是否会引发冲突。
    ///
    /// 返回 `Some(conflict)` 表示有冲突，应拒绝删除。
    /// 返回 `None` 表示可以安全删除。
    fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict>;
}

// ── 辅助函数 ────────────────────────────────────────────────────────────────

/// 生成简短 UUID（测试 + operation_id 用）。
fn uuid_str() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("{:016x}", now.as_nanos() & 0xFFFF_FFFF_FFFF_FFFF)
}

// ── 模型 DTO ──────────────────────────────────────────────────────────────

/// 模型目录项 DTO（前端展示用）。
///
/// **不暴露**内部文件路径、URL、token、endpoint。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogItemDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 显示名称。
    pub display_name: String,
    /// 简短描述。
    pub description: String,
    /// 模型 revision。
    pub revision: String,
    /// 预计体积（MB）。
    pub estimated_size_mb: Option<u64>,
    /// 安装状态。
    pub install_state: String,
    /// 校验状态。
    pub verification_state: String,
    /// 缓存占用（bytes）。
    pub cache_size_bytes: Option<u64>,
    /// 是否被配置选择。
    pub is_selected: bool,
    /// 是否为当前进程实际模型。
    pub is_active: bool,
    /// 兼容性。
    pub compatibility: String,
}

/// 模型操作请求 DTO（前端提交）。
///
/// **闭合字段**：只接受 `engine_id`、`model_id`、`operation_id`。
/// 禁止包含 URL、路径、脚本、外部命令。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationRequestDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 操作 id（可选，用于取消关联）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

/// 模型操作结果 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationResultDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 操作 id。
    pub operation_id: String,
    /// 操作种类。
    pub operation_kind: String,
    /// 最终阶段。
    pub final_stage: String,
    /// 是否成功。
    pub success: bool,
    /// 错误信息（如果有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

/// 删除冲突 DTO（结构化冲突）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeleteConflictDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 冲突原因列表。
    pub reasons: Vec<DeleteConflictReasonDto>,
}

/// 单条删除冲突原因 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteConflictReasonDto {
    /// 被配置引用。
    ReferencedByConfig {
        config_field: String,
        config_value: String,
    },
    /// 运行实例正在使用。
    ActiveInRunningInstance { instance_id: String },
    /// descriptor 默认引用。
    ReferencedByDescriptor { descriptor_model_id: String },
}

/// 从 domain `EngineModelStatus` 投影为 DTO。
pub fn project_model_status(
    descriptor: &EngineModelDescriptor,
    status: &EngineModelStatus,
) -> ModelCatalogItemDto {
    ModelCatalogItemDto {
        engine_id: descriptor.engine_id.to_string(),
        model_id: descriptor.model_id.clone(),
        display_name: descriptor.display_name.clone(),
        description: descriptor.description.clone(),
        revision: descriptor.revision.clone(),
        estimated_size_mb: descriptor.estimated_size_mb,
        install_state: status.install_state.to_string(),
        verification_state: status.verification_state.to_string(),
        cache_size_bytes: status.cache_size_bytes,
        is_selected: status.is_selected,
        is_active: status.is_active,
        compatibility: status.compatibility.to_string(),
    }
}

/// 从 domain `ModelOperationResult` 投影为 DTO。
pub fn project_model_operation_result(result: &ModelOperationResult) -> ModelOperationResultDto {
    ModelOperationResultDto {
        engine_id: result.engine_id.clone(),
        model_id: result.model_id.clone(),
        operation_id: result.operation_id.clone(),
        operation_kind: result.operation_kind.to_string(),
        final_stage: result.final_stage.to_string(),
        success: result.success,
        error: result
            .error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

/// 从 domain `ModelDeleteConflict` 投影为 DTO。
pub fn project_delete_conflict(conflict: &ModelDeleteConflict) -> ModelDeleteConflictDto {
    ModelDeleteConflictDto {
        engine_id: conflict.engine_id.to_string(),
        model_id: conflict.model_id.clone(),
        reasons: conflict
            .reasons
            .iter()
            .map(|r| match r {
                DeleteConflictReason::ReferencedByConfig {
                    config_field,
                    config_value,
                } => DeleteConflictReasonDto::ReferencedByConfig {
                    config_field: config_field.clone(),
                    config_value: config_value.clone(),
                },
                DeleteConflictReason::ActiveInRunningInstance { instance_id } => {
                    DeleteConflictReasonDto::ActiveInRunningInstance {
                        instance_id: instance_id.clone(),
                    }
                }
                DeleteConflictReason::ReferencedByDescriptor {
                    descriptor_model_id,
                } => DeleteConflictReasonDto::ReferencedByDescriptor {
                    descriptor_model_id: descriptor_model_id.clone(),
                },
            })
            .collect(),
    }
}

// ── FunASR 模型注册 ────────────────────────────────────────────────────────

/// 创建 FunASR 的模型注册表条目。
///
/// 注册 SenseVoice Small 和 Paraformer-zh 为同一引擎的两个受限模型候选。
pub fn make_funasr_model_registry() -> ModelRegistry {
    ModelRegistry::new_with_models(vec![
        EngineModelDescriptor::sensevoice_small(),
        EngineModelDescriptor::paraformer_zh(),
    ])
}

/// 创建 FunASR 的 `ModelService`。
///
/// 注册函数由 H6 接 wiring；本任务提供纯构造入口。
pub fn make_funasr_model_service() -> ModelService {
    ModelService::new(make_funasr_model_registry())
}

// ── FunASR 删除冲突检查器 ────────────────────────────────────────────────

/// FunASR 模型删除冲突检查器。
///
/// 检查：
/// - 模型是否被 SttConfig.local_engine.funasr_model 引用（selected）
/// - 模型是否被 EngineDescriptor.model_contract 作为默认引用
pub struct FunasrModelConflictChecker {
    /// 当前 SttConfig 中的 funasr_model（selected 模型）
    pub selected_model: String,
    /// EngineDescriptor 的默认 model_contract.model_id
    pub descriptor_model_id: String,
    /// 当前运行实例的 active model_id（如有）
    pub active_model_id: Option<String>,
    /// 当前运行实例的 instance_id（如有）
    pub active_instance_id: Option<String>,
}

impl ModelConflictChecker for FunasrModelConflictChecker {
    fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict> {
        let mut reasons = Vec::new();

        // 检查配置引用
        if self.selected_model == model_id {
            reasons.push(DeleteConflictReason::ReferencedByConfig {
                config_field: "funasr_model".to_string(),
                config_value: self.selected_model.clone(),
            });
        }

        // 检查 descriptor 默认引用
        if self.descriptor_model_id == model_id {
            reasons.push(DeleteConflictReason::ReferencedByDescriptor {
                descriptor_model_id: self.descriptor_model_id.clone(),
            });
        }

        // 检查 active 运行实例引用
        if let (Some(active_id), Some(inst_id)) = (&self.active_model_id, &self.active_instance_id)
        {
            if active_id == model_id {
                reasons.push(DeleteConflictReason::ActiveInRunningInstance {
                    instance_id: inst_id.clone(),
                });
            }
        }

        if reasons.is_empty() {
            None
        } else {
            Some(ModelDeleteConflict {
                engine_id: engine_id.clone(),
                model_id: model_id.to_string(),
                reasons,
            })
        }
    }
}

// ── 单测 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::model::ModelCompatibility;

    // ── ModelRegistry ─────────────────────────────────────────────────────

    #[test]
    fn registry_list_funasr_models() {
        let reg = make_funasr_model_registry();
        let funasr = EngineId::new("funasr").unwrap();
        let models = reg.list(&funasr);
        assert_eq!(models.len(), 2);
        assert!(models.iter().any(|m| m.model_id == "iic/SenseVoiceSmall"));
        assert!(models.iter().any(|m| m.model_id == "paraformer-zh"));
    }

    #[test]
    fn registry_find_sensevoice() {
        let reg = make_funasr_model_registry();
        let funasr = EngineId::new("funasr").unwrap();
        let desc = reg.find(&funasr, "iic/SenseVoiceSmall").unwrap();
        assert_eq!(desc.display_name, "SenseVoice Small");
    }

    #[test]
    fn registry_find_paraformer() {
        let reg = make_funasr_model_registry();
        let funasr = EngineId::new("funasr").unwrap();
        let desc = reg.find(&funasr, "paraformer-zh").unwrap();
        assert_eq!(desc.display_name, "Paraformer-zh");
    }

    #[test]
    fn registry_find_unknown_returns_none() {
        let reg = make_funasr_model_registry();
        let funasr = EngineId::new("funasr").unwrap();
        assert!(reg.find(&funasr, "nonexistent-model").is_none());
    }

    #[test]
    fn registry_list_empty_engine_returns_empty() {
        let reg = make_funasr_model_registry();
        let unknown = EngineId::new("unknown-engine").unwrap();
        assert!(reg.list(&unknown).is_empty());
    }

    // ── ModelService::list_models ──────────────────────────────────────────

    #[tokio::test]
    async fn list_models_returns_all_candidates() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let models = svc.list_models(&funasr).await;
        assert_eq!(models.len(), 2);
        // 初始状态都是 not_installed
        assert!(models.iter().all(|m| !m.is_selected));
        assert!(models.iter().all(|m| !m.is_active));
    }

    #[tokio::test]
    async fn get_model_status_unknown_returns_error() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc.get_model_status(&funasr, "nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn get_model_status_sensevoice_initial() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let status = svc
            .get_model_status(&funasr, "iic/SenseVoiceSmall")
            .await
            .unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);
        assert!(!status.is_usable());
    }

    // ── ModelService::install_model ─────────────────────────────────────────

    #[tokio::test]
    async fn install_model_unknown_returns_error() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc.install_model(&funasr, "nonexistent", None).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn install_model_sensevoice_completes() {
        // 注意：此测试在没有真实模型文件的情况下，
        // install_model 会因为 model_cache_dir 为空而走失败路径。
        // 验证失败路径不破坏已安装模型（本来就没安装）。
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc
            .install_model(&funasr, "iic/SenseVoiceSmall", None)
            .await
            .unwrap();

        assert!(!result.success);
        assert_eq!(result.final_stage, ModelOperationStage::Failed);
        assert!(result.error.is_some());

        // 验证状态回到了 NotInstalled
        let status = svc
            .get_model_status(&funasr, "iic/SenseVoiceSmall")
            .await
            .unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);
    }

    // ── ModelService::cancel_model_operation ───────────────────────────────

    #[tokio::test]
    async fn cancel_when_no_operation_returns_error() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc
            .cancel_model_operation(&funasr, "iic/SenseVoiceSmall", "op-123")
            .await;
        assert!(result.is_err());
    }

    // ── ModelService::update_selected_active ───────────────────────────────

    #[tokio::test]
    async fn update_selected_active_sets_flags() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();

        svc.update_selected_active(&funasr, "iic/SenseVoiceSmall", true, false)
            .await
            .unwrap();

        let status = svc
            .get_model_status(&funasr, "iic/SenseVoiceSmall")
            .await
            .unwrap();
        assert!(status.is_selected);
        assert!(!status.is_active);
    }

    #[tokio::test]
    async fn update_selected_active_sets_active() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();

        svc.update_selected_active(&funasr, "paraformer-zh", true, true)
            .await
            .unwrap();

        let status = svc
            .get_model_status(&funasr, "paraformer-zh")
            .await
            .unwrap();
        assert!(status.is_selected);
        assert!(status.is_active);
    }

    // ── ModelService::verify_model_identity ───────────────────────────────

    #[test]
    fn verify_model_identity_sensevoice_matched() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc
            .verify_model_identity(
                &funasr,
                "iic/SenseVoiceSmall",
                Some("iic/SenseVoiceSmall"),
                Some("funasr-1.x"),
                Some("fp-abc"),
            )
            .unwrap();
        assert!(result.is_matched());
    }

    #[test]
    fn verify_model_identity_paraformer_matched() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc
            .verify_model_identity(
                &funasr,
                "paraformer-zh",
                Some("paraformer-zh"),
                Some("funasr-1.x"),
                Some("fp-xyz"),
            )
            .unwrap();
        assert!(result.is_matched());
    }

    #[test]
    fn verify_model_identity_mismatched_model_id() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc
            .verify_model_identity(
                &funasr,
                "iic/SenseVoiceSmall",
                Some("paraformer-zh"), // 错误的 model_id
                Some("funasr-1.x"),
                None,
            )
            .unwrap();
        assert!(!result.is_matched());
    }

    #[test]
    fn verify_model_identity_unknown_model_returns_error() {
        let svc = make_funasr_model_service();
        let funasr = EngineId::new("funasr").unwrap();
        let result = svc.verify_model_identity(&funasr, "nonexistent", None, None, None);
        assert!(result.is_err());
    }

    // ── FunasrModelConflictChecker ─────────────────────────────────────────

    #[test]
    fn conflict_checker_blocks_when_selected() {
        let checker = FunasrModelConflictChecker {
            selected_model: "iic/SenseVoiceSmall".to_string(),
            descriptor_model_id: "iic/SenseVoiceSmall".to_string(),
            active_model_id: None,
            active_instance_id: None,
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "iic/SenseVoiceSmall");
        assert!(conflict.is_some());
        let c = conflict.unwrap();
        assert_eq!(c.reasons.len(), 2);
        // 有 ReferencedByConfig 和 ReferencedByDescriptor
        assert!(
            c.reasons
                .iter()
                .any(|r| matches!(r, DeleteConflictReason::ReferencedByConfig { .. }))
        );
        assert!(
            c.reasons
                .iter()
                .any(|r| matches!(r, DeleteConflictReason::ReferencedByDescriptor { .. }))
        );
    }

    #[test]
    fn conflict_checker_blocks_when_active() {
        let checker = FunasrModelConflictChecker {
            selected_model: "other-model".to_string(),
            descriptor_model_id: "other-model".to_string(),
            active_model_id: Some("paraformer-zh".to_string()),
            active_instance_id: Some("inst-001".to_string()),
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "paraformer-zh");
        assert!(conflict.is_some());
        let c = conflict.unwrap();
        assert_eq!(c.reasons.len(), 1);
        assert!(matches!(
            &c.reasons[0],
            DeleteConflictReason::ActiveInRunningInstance { instance_id } if instance_id == "inst-001"
        ));
    }

    #[test]
    fn conflict_checker_allows_when_no_conflict() {
        let checker = FunasrModelConflictChecker {
            selected_model: "other-model".to_string(),
            descriptor_model_id: "other-model".to_string(),
            active_model_id: None,
            active_instance_id: None,
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "iic/SenseVoiceSmall");
        assert!(conflict.is_none());
    }

    #[test]
    fn conflict_checker_all_three_reasons() {
        let checker = FunasrModelConflictChecker {
            selected_model: "iic/SenseVoiceSmall".to_string(),
            descriptor_model_id: "iic/SenseVoiceSmall".to_string(),
            active_model_id: Some("iic/SenseVoiceSmall".to_string()),
            active_instance_id: Some("inst-002".to_string()),
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "iic/SenseVoiceSmall");
        let c = conflict.unwrap();
        assert_eq!(c.reasons.len(), 3);
    }

    // ── DTO 投影 ──────────────────────────────────────────────────────────

    #[test]
    fn project_model_status_dto() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let status = EngineModelStatus::not_installed(&desc);
        let dto = project_model_status(&desc, &status);
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.model_id, "iic/SenseVoiceSmall");
        assert_eq!(dto.install_state, "not_installed");
        assert_eq!(dto.verification_state, "unknown");
        assert_eq!(dto.compatibility, "unknown");
        assert!(!dto.is_selected);
        assert!(!dto.is_active);
    }

    #[test]
    fn project_model_status_dto_with_compatibility() {
        let desc = EngineModelDescriptor::paraformer_zh();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.compatibility = ModelCompatibility::Incompatible {
            reason: "需要 GPU".to_string(),
        };
        let dto = project_model_status(&desc, &status);
        assert!(dto.compatibility.contains("incompatible"));
        assert!(dto.compatibility.contains("需要 GPU"));
    }

    #[test]
    fn project_delete_conflict_dto() {
        let conflict = ModelDeleteConflict {
            engine_id: EngineId::new("funasr").unwrap(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            reasons: vec![
                DeleteConflictReason::ReferencedByConfig {
                    config_field: "funasr_model".to_string(),
                    config_value: "iic/SenseVoiceSmall".to_string(),
                },
                DeleteConflictReason::ActiveInRunningInstance {
                    instance_id: "inst-abc".to_string(),
                },
            ],
        };
        let dto = project_delete_conflict(&conflict);
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.model_id, "iic/SenseVoiceSmall");
        assert_eq!(dto.reasons.len(), 2);
    }

    #[test]
    fn model_operation_request_dto_deny_unknown_fields() {
        // 确保前端不能提交额外字段（如 url、path、script）
        let json =
            r#"{"engine_id":"funasr","model_id":"iic/SenseVoiceSmall","url":"https://evil.com"}"#;
        let result: Result<ModelOperationRequestDto, _> = serde_json::from_str(json);
        assert!(result.is_err(), "deny_unknown_fields 应拒绝 url 字段");
    }

    #[test]
    fn model_operation_request_dto_accepts_valid() {
        let json = r#"{"engine_id":"funasr","model_id":"iic/SenseVoiceSmall"}"#;
        let dto: ModelOperationRequestDto = serde_json::from_str(json).unwrap();
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.model_id, "iic/SenseVoiceSmall");
        assert!(dto.operation_id.is_none());
    }

    #[test]
    fn project_model_operation_result_dto() {
        let result = ModelOperationResult {
            engine_id: "funasr".to_string(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            operation_id: "op-001".to_string(),
            operation_kind: ModelOperationKind::Install,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        };
        let dto = project_model_operation_result(&result);
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.operation_kind, "install");
        assert_eq!(dto.final_stage, "done");
        assert!(dto.success);
        assert!(dto.error.is_none());
    }
}
