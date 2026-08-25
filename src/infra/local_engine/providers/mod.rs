//! Runtime Provider trait、descriptor、安装事务、原子切换与 profile 解析（0.22.2）。
//!
//! ## 分层归属
//!
//! - `providers`：定义 provider trait 和 descriptor，编排 staging → generation promote
//!   → current.json 原子替换的通用事务流程。
//! - `providers/python`：`PythonVenvProvider` 完整实现。
//! - `providers/binary`：`ManagedBinaryProvider` 协议位（闭合变体，不实现完整逻辑）。
//! - 本模块不依赖 app/domain/tauri，只使用 infra 内部类型和标准库。
//!
//! ## 安装事务（§3.6）
//!
//! ```text
//! begin_install(descriptor, preference)
//!   → resolve_profile(descriptor, preference)
//!   → create staging/{operation-id}
//!   → provider.prepare_environment(staging_dir)
//!   → provider.self_test(staging_dir)
//!   → write generation manifest
//!   → promote staging → generations/{install-id}
//!   → atomic switch current.json
//!   → on failure: rollback to previous current.json
//! ```

pub mod binary;
pub mod python;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::runtime::{
    self, CleanupScope, ComputeBackend, ComputePreference, EngineId, FallbackReason,
    FallbackReasonKind, GenerationManifest, ManifestExtension, ModelContract, ResolvedProfile,
    RuntimeError, RuntimeKind, add_reference, generate_install_id, generate_operation_id,
    read_current_pointer, read_manifest, remove_reference, scan_artifact_references,
    validate_install_id, validate_operation_id, write_current_pointer, write_manifest,
};

// ── ProviderDescriptor ────────────────────────────────────────────────────

/// 引擎描述符（编译期内置 allowlist，不接受前端动态传入）。
///
/// 声明引擎静态事实：稳定 id、runtime kind、候选 profile、模型契约与安装计划。
/// adapter 行为由各 provider 实现，不编码成通用字符串 DSL。
#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    /// 引擎稳定标识符。
    pub engine_id: EngineId,
    /// 运行时种类（闭合枚举）。
    pub runtime_kind: RuntimeKind,
    /// 引擎显示名称（人类可读，非路径用）。
    pub display_name: String,
    /// 候选 profile 列表（按优先级排序，0 = 最高优先级）。
    pub profiles: Vec<ProfileCandidate>,
    /// 模型契约（锁定模型身份）。
    pub model_contract: ModelContract,
    /// 安装计划（provider 专属，闭合枚举）。
    pub install_plan: InstallPlan,
    /// 至少保留的 generation 数量（含 current）。
    pub min_generations: u32,
}

/// 候选 profile 声明。
#[derive(Debug, Clone)]
pub struct ProfileCandidate {
    /// profile 标识（如 `cpu-x64`、`cuda12-sm86`）。
    pub profile_id: String,
    /// 此 profile 对应的 compute backend。
    pub backend: ComputeBackend,
    /// 对应的 artifact id（可能多个 profile 共享同一 artifact）。
    pub artifact_id: runtime::ArtifactId,
    /// 兼容性检查器标识（provider 负责实现实际检查）。
    pub compatibility: CompatibilityCheck,
}

/// 兼容性检查类型（provider 负责实现实际检查逻辑）。
#[derive(Debug, Clone)]
pub enum CompatibilityCheck {
    /// 总是兼容（如 CPU x64）。
    Always,
    /// 需要 CUDA GPU（provider 检查 nvidia-smi / 驱动版本）。
    RequiresCuda { min_version: Option<String> },
    /// 需要 Vulkan 驱动。
    RequiresVulkan,
    /// 需要 DirectML 支持的 GPU。
    RequiresDirectml,
    /// 需要 CPU feature（如 AVX2）。
    RequiresCpuFeature { feature: String },
}

/// 安装计划（provider 专属，闭合枚举）。
#[derive(Debug, Clone)]
pub enum InstallPlan {
    /// Python venv 安装计划。
    PythonVenv(PythonInstallPlan),
    /// Managed binary 安装计划。
    ManagedBinary(BinaryInstallPlan),
}

/// 额外 pip 安装参数（闭合枚举，不接受任意字符串）。
///
/// 替代之前的 `Vec<String>`，避免命令注入风险。
/// 只允许编译期已知的、语义明确的安装选项。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PipExtraArg {
    /// 指定额外的 package index URL（如 PyTorch CUDA index）。
    /// 对应 `--extra-index-url <url>`。
    ExtraIndexUrl(String),
    /// 指定 `--no-deps`（不安装依赖，仅安装指定包）。
    /// 用于避免传递依赖版本漂移。
    NoDeps,
    /// 指定 `--no-build-isolation`（不使用构建隔离）。
    /// 用于预安装构建依赖的场景。
    NoBuildIsolation,
}

/// Python venv 安装计划。
#[derive(Debug, Clone)]
pub struct PythonInstallPlan {
    /// Python 版本（如 `3.12.8`）。
    pub python_version: String,
    /// Python distribution artifact id。
    pub python_artifact_id: runtime::ArtifactId,
    /// 锁定的包列表（含 SHA-256 hash，用于 `--require-hashes` 强校验）。
    pub packages: Vec<PackageLock>,
    /// uv 版本要求。
    pub uv_version: String,
    /// package index URL（如果非默认 PyPI）。
    pub index_url: Option<String>,
    /// 额外 pip 安装参数（闭合枚举，不接受任意字符串）。
    pub extra_pip_args: Vec<PipExtraArg>,
    /// self-test 脚本（Python 代码片段，在 venv 中执行）。
    pub self_test_script: String,
}

/// Managed binary 安装计划。
#[derive(Debug, Clone)]
pub struct BinaryInstallPlan {
    /// archive artifact id。
    pub archive_artifact_id: runtime::ArtifactId,
    /// archive 下载 URL。
    pub archive_url: String,
    /// archive SHA-256（hex）。
    pub archive_sha256: String,
    /// 可执行文件路径（相对于 generation 根）。
    pub executable: String,
    /// 引用的共享 stdlib artifact（可选，如 Blink 托管 Python distribution）。
    pub stdlib_artifact: Option<runtime::ArtifactIdentity>,
    /// required CPU features。
    pub required_cpu_features: Vec<String>,
    /// required drivers。
    pub required_drivers: Vec<String>,
    /// self-test 命令行（在 generation 根执行）。
    pub self_test_command: Vec<String>,
}

/// 包锁定条目。
///
/// 包含 SHA-256 hash 时，安装使用 `--require-hashes` 强校验，
/// 确保安装的 wheel 与 descriptor 声明的 hash 完全一致。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PackageLock {
    /// 包名（如 `torch`、`funasr`）。
    pub name: String,
    /// 锁定版本（如 `2.5.0` 或 `>=0.59`）。
    pub version: String,
    /// wheel SHA-256 hash（hex）。
    ///
    /// 如果提供，安装时使用 `--require-hashes` 强校验。
    /// 如果为 `None`，表示上游不提供稳定 checksum（如 PaddlePaddle），
    /// 此时安装不使用 `--require-hashes`，但 manifest 记录 `ChecksumSource::DownloadSource`。
    ///
    /// 对于多平台 wheel，这是第一个 hash（用于摘要/标识）。
    /// `all_hashes` 包含所有平台 wheel 的 hash，用于 `--require-hashes` 安装。
    pub sha256: Option<String>,
    /// 所有平台 wheel 的 SHA-256 hash 列表。
    ///
    /// `--require-hashes` 需要列出所有 hash 让 pip 匹配正确平台的 wheel。
    /// 如果为空，则使用 `sha256`（向后兼容）。
    #[serde(default)]
    pub all_hashes: Vec<String>,
}

// ── RuntimeProvider trait ─────────────────────────────────────────────────

/// Provider 准备环境的结果。
///
/// 包含 provider 在 staging 中准备环境后返回的 artifact 身份标识（含真实 hash）。
/// `InstallTransaction` 使用此信息填充 manifest 的 `artifact` 字段。
#[derive(Debug, Clone)]
pub struct PrepareResult {
    /// 已验证的 artifact 身份标识（包含 SHA-256 hash）。
    pub artifact: runtime::ArtifactIdentity,
}

/// Runtime Provider trait（provider-neutral 接口）。
///
/// 每个 provider 实现此 trait，把"如何取得并验证可执行环境"封装在受限 provider 中。
/// `PythonVenvProvider` 负责 uv/Python/venv/pip；
/// `ManagedBinaryProvider` 负责下载/解包/验证 archive。
#[async_trait::async_trait]
pub trait RuntimeProvider: Send + Sync {
    /// 运行时种类。
    fn kind(&self) -> RuntimeKind;

    /// 检查本机兼容性（如 CUDA GPU 是否存在、CPU feature 是否支持）。
    fn check_compatibility(&self, compatibility: &CompatibilityCheck)
    -> Result<bool, RuntimeError>;

    /// 在 staging 目录中准备环境。
    ///
    /// - PythonVenv: 确保 uv/Python、创建 venv、同步锁定依赖。
    /// - ManagedBinary: 下载/解包锁定 archive、验证文件 hash。
    ///
    /// 返回 `PrepareResult`，包含已验证的 artifact 身份标识（含真实 SHA-256 hash）。
    /// 此 hash 写入 generation manifest，用于后续完整性验证。
    async fn prepare_environment(
        &self,
        staging_dir: &std::path::Path,
        plan: &InstallPlan,
        resolved_profile: &ResolvedProfile,
    ) -> Result<PrepareResult, RuntimeError>;

    /// 执行 provider self-test。
    ///
    /// 在已准备好的环境中执行最小验证。
    async fn self_test(
        &self,
        generation_dir: &std::path::Path,
        plan: &InstallPlan,
    ) -> Result<(), RuntimeError>;

    /// 从已安装的 generation 读取 provider 专属状态。
    ///
    /// 返回 manifest extension（不含通用状态机字段）。
    fn build_manifest_extension(
        &self,
        generation_dir: &std::path::Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError>;

    /// 查询已安装 generation 的包状态（用于状态检查）。
    ///
    /// 返回 `Vec<PackageStatus>`，由 adapter 投影引擎专属字段。
    fn query_package_status(
        &self,
        generation_dir: &std::path::Path,
        plan: &InstallPlan,
    ) -> Result<Vec<runtime::PackageStatus>, RuntimeError>;

    /// 清理 provider 公共资产（uv cache、download cache 等）。
    ///
    /// 只清理 `scope` 指定范围，不误删其他引擎资产。
    fn cleanup_provider_cache(&self, scope: &ProviderCleanupScope) -> Result<(), RuntimeError>;
}

/// Provider 清理范围（区分引擎 generation 和 provider 公共资产）。
#[derive(Debug, Clone)]
pub enum ProviderCleanupScope {
    /// Provider 下载缓存（uv cache 等）。
    DownloadCache,
    /// Provider 公共 artifact（共享 Python distribution 等）。
    SharedArtifact(runtime::ArtifactId),
}

// ── InstallTransaction ───────────────────────────────────────────────────

/// 安装事务结果。
#[derive(Debug, Clone)]
pub struct InstallResult {
    /// 新安装的 generation install id。
    pub install_id: String,
    /// 本次 operation id。
    pub operation_id: String,
    /// 写入的 generation manifest。
    pub manifest: GenerationManifest,
    /// 是否发生了 profile fallback（requested != resolved）。
    pub fell_back: bool,
}

/// 安装事务编排器。
///
/// 编排 staging → prepare → self-test → manifest → promote → atomic switch 流程。
/// 安装失败自动回滚 staging；原子切换失败回滚到 previous current.json。
pub struct InstallTransaction<'a, P: RuntimeProvider> {
    descriptor: &'a ProviderDescriptor,
    provider: &'a P,
}

impl<'a, P: RuntimeProvider> InstallTransaction<'a, P> {
    /// 创建安装事务。
    pub fn new(descriptor: &'a ProviderDescriptor, provider: &'a P) -> Self {
        Self {
            descriptor,
            provider,
        }
    }

    /// 执行完整安装事务。
    ///
    /// 步骤：
    /// 1. resolve_profile: 解析 requested preference → resolved profile
    /// 2. create staging: 创建 `engines/{engine_id}/staging/{operation_id}`
    /// 3. prepare_environment: provider 在 staging 中准备环境
    /// 4. self_test: provider 执行最小验证
    /// 5. write manifest: 写 generation manifest
    /// 6. promote: staging → `generations/{install_id}`
    /// 7. atomic switch: 原子替换 current.json
    /// 8. on failure: 回滚
    pub async fn execute(
        &self,
        preference: ComputePreference,
    ) -> Result<InstallResult, RuntimeError> {
        let engine_id = &self.descriptor.engine_id;
        let operation_id = generate_operation_id();
        validate_operation_id(&operation_id)?;
        let install_id = generate_install_id();
        validate_install_id(&install_id)?;

        // ── 1. resolve profile ──
        let (resolved_profile, fallback_reasons) = self.resolve_profile(preference)?;

        let fell_back = !fallback_reasons.is_empty();

        // ── 2. create staging ──
        let staging = runtime::operation_staging_dir(engine_id, &operation_id);
        std::fs::create_dir_all(&staging).map_err(|e| RuntimeError::StagingCreateFailed {
            message: format!("创建 staging 目录失败: {e}"),
        })?;

        tracing::info!(
            engine = %engine_id,
            op = %operation_id,
            staging = %staging.display(),
            "staging 目录已创建"
        );

        // ── 3. prepare environment ──
        let prepare_result = match self
            .provider
            .prepare_environment(&staging, &self.descriptor.install_plan, &resolved_profile)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%e, "prepare_environment 失败，清理 staging");
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        // ── 4. self-test ──
        if let Err(e) = self
            .provider
            .self_test(&staging, &self.descriptor.install_plan)
            .await
        {
            tracing::warn!(%e, "self-test 失败，清理 staging");
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }

        // ── 5. build manifest extension ──
        let extension = match self
            .provider
            .build_manifest_extension(&staging, &self.descriptor.install_plan)
        {
            Ok(ext) => ext,
            Err(e) => {
                tracing::warn!(%e, "build_manifest_extension 失败，清理 staging");
                let _ = std::fs::remove_dir_all(&staging);
                return Err(e);
            }
        };

        // ── 6. write manifest (in staging first) ──
        let artifact_id = &resolved_profile.artifact_id;
        let artifact = prepare_result.artifact;
        let artifact_clone = artifact.clone();

        let manifest = GenerationManifest {
            schema_version: runtime::MANIFEST_SCHEMA_VERSION,
            engine_id: engine_id.clone(),
            runtime_kind: self.descriptor.runtime_kind,
            install_id: install_id.clone(),
            requested_preference: preference,
            resolved_profile: resolved_profile.clone(),
            installed_at_ms: runtime::now_ms(),
            artifact,
            model_contract: self.descriptor.model_contract.clone(),
            fallback_reasons,
            extension,
        };

        // 在 staging 目录中先写 manifest
        let staging_manifest = staging.join("manifest.json");
        runtime::atomic_write_json(&staging_manifest, &manifest)?;

        // ── 7. promote: staging → generations/{install_id} ──
        let gen_dir = runtime::generation_dir(engine_id, &install_id);
        std::fs::create_dir_all(runtime::generations_dir(engine_id))?;

        // 原子 move（rename）
        match std::fs::rename(&staging, &gen_dir) {
            Ok(()) => {
                tracing::info!(
                    engine = %engine_id,
                    install = %install_id,
                    "generation 提升成功"
                );
            }
            Err(e) => {
                // rename 失败（可能跨卷或目标已存在），尝试复制
                tracing::warn!(%e, "rename 失败，尝试复制 staging → generation");
                copy_dir_recursive(&staging, &gen_dir).map_err(|e| {
                    RuntimeError::GenerationPromoteFailed {
                        message: format!("提升 generation 失败: {e}"),
                    }
                })?;
                let _ = std::fs::remove_dir_all(&staging);
            }
        }

        // ── 8. add artifact reference ──
        add_reference(
            self.descriptor.runtime_kind,
            artifact_id,
            engine_id.as_str(),
            &install_id,
        )?;

        // ── 9. atomic switch current.json ──
        let previous = read_current_pointer(engine_id)?;

        let pointer = runtime::CurrentPointer {
            install_id: install_id.clone(),
            manifest_path: format!("generations/{install_id}/manifest.json"),
            updated_at_ms: runtime::now_ms(),
            schema_version: runtime::CURRENT_POINTER_SCHEMA_VERSION,
        };

        if let Err(e) = write_current_pointer(engine_id, &pointer) {
            tracing::error!(%e, "current.json 原子写入失败，回滚");
            // 回滚：移除新 generation，恢复旧指针
            let _ = std::fs::remove_dir_all(&gen_dir);
            remove_reference(
                self.descriptor.runtime_kind,
                artifact_id,
                engine_id.as_str(),
                &install_id,
            )?;
            if let Some(prev) = previous {
                let _ = write_current_pointer(engine_id, &prev);
            }
            return Err(e);
        }

        // ── 10. verify after switch（§3.6: 首次启动验证失败则原子切回上一 generation） ──
        //
        // current.json 原子切换后，执行最终验证：
        // - 读取刚写入的 manifest 确认完整性
        // - 验证 artifact hash 非空（provider 必须填充）
        // - 如果有 previous generation，验证 previous manifest 仍可读取（回滚保障）
        //
        // 验证失败时：
        // - 原子切回 previous current.json（如果有）
        // - 标记新 generation 为 deferred cleanup（不立即删除，但 current 不指向它）
        // - 返回错误
        if let Err(e) = self
            .verify_after_switch(engine_id, &install_id, &artifact_clone, &gen_dir)
            .await
        {
            tracing::error!(%e, "切换后 provider 验证失败，回滚到 previous generation");
            // 原子切回 previous
            if let Some(ref prev) = previous {
                if let Err(switch_err) = write_current_pointer(engine_id, prev) {
                    tracing::error!(%switch_err, "回滚 current.json 也失败！手动恢复需要 recover_current_pointer");
                }
            } else {
                // 没有 previous —— 清除 current.json 指针
                let _ = std::fs::remove_file(runtime::current_pointer_path(engine_id));
            }
            // 标记新 generation 为 deferred cleanup
            let _ = mark_deferred_cleanup(
                engine_id,
                &install_id,
                &format!("切换后 provider 验证失败: {e}"),
            );
            return Err(e);
        }

        tracing::info!(
            engine = %engine_id,
            install = %install_id,
            fell_back = fell_back,
            "安装事务完成（含切换后 provider 验证）"
        );

        Ok(InstallResult {
            install_id,
            operation_id,
            manifest,
            fell_back,
        })
    }

    /// current 切换后的 provider 验证。
    ///
    /// 在 current.json 原子切换后执行最终验证：
    /// - 读取刚写入的 manifest 确认完整性（schema + artifact hash + self-test）
    /// - 验证 artifact sha256 非空（provider 必须在 prepare_environment 中填充）
    /// - 对已提升的不可变 generation 重新执行 provider self-test
    ///
    /// 此函数是安装事务的最后一步，失败时调用方负责回滚 current.json。
    async fn verify_after_switch(
        &self,
        engine_id: &EngineId,
        install_id: &str,
        expected_artifact: &runtime::ArtifactIdentity,
        generation_dir: &std::path::Path,
    ) -> Result<(), RuntimeError> {
        // 读取并验证 manifest
        let manifest = read_manifest(engine_id, install_id)?;

        // 验证 artifact hash 非空
        if manifest.artifact.sha256.is_empty() {
            return Err(RuntimeError::SelfTestFailed {
                message: "manifest artifact sha256 为空，provider 未填充 hash".to_string(),
            });
        }

        // 验证 artifact identity 一致性
        if manifest.artifact != *expected_artifact {
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "manifest artifact identity 不匹配: expected={:?}, actual={:?}",
                    expected_artifact, manifest.artifact
                ),
            });
        }

        // 验证 self_test_passed
        let self_test_ok = match &manifest.extension {
            ManifestExtension::PythonVenv(ext) => ext.self_test_passed,
            ManifestExtension::ManagedBinary(ext) => ext.self_test_passed,
        };
        if !self_test_ok {
            return Err(RuntimeError::SelfTestFailed {
                message: "manifest extension 标记 self_test_passed=false".to_string(),
            });
        }

        // 不信任 manifest 中持久化的布尔值；对 promote 后、current 实际指向的
        // generation 再运行 provider 验证。服务启动/health activation verification
        // 由 0.22.3 LocalEngineService 接入。
        self.provider
            .self_test(generation_dir, &self.descriptor.install_plan)
            .await?;

        tracing::info!(
            engine = %engine_id,
            install = %install_id,
            sha256 = %manifest.artifact.sha256,
            "切换后 provider 验证通过"
        );
        Ok(())
    }

    /// 解析 compute preference → resolved profile。
    ///
    /// - `auto`：按 descriptor 声明的优先级回退，记录每次失败原因。
    /// - `gpu_auto`：只在 GPU backend 间选择。
    /// - 显式 `cpu/cuda/vulkan/directml`：失败返回可行动错误，不回退。
    fn resolve_profile(
        &self,
        preference: ComputePreference,
    ) -> Result<(ResolvedProfile, Vec<FallbackReason>), RuntimeError> {
        let profiles = &self.descriptor.profiles;
        if profiles.is_empty() {
            return Err(RuntimeError::ProfileResolutionFailed {
                message: "descriptor 未声明任何候选 profile".to_string(),
            });
        }

        let mut fallbacks = Vec::new();

        match preference {
            ComputePreference::Auto => {
                // 按 descriptor 优先级逐个尝试，记录失败原因
                for (i, candidate) in profiles.iter().enumerate() {
                    match self.provider.check_compatibility(&candidate.compatibility) {
                        Ok(true) => {
                            return Ok((
                                ResolvedProfile {
                                    profile_id: candidate.profile_id.clone(),
                                    backend: candidate.backend,
                                    artifact_id: candidate.artifact_id.clone(),
                                    priority: i as u32,
                                },
                                fallbacks,
                            ));
                        }
                        Ok(false) => {
                            fallbacks.push(FallbackReason {
                                rejected_profile: candidate.profile_id.clone(),
                                reason: FallbackReasonKind::HostIncompatible,
                                detail: "本机不兼容".to_string(),
                            });
                        }
                        Err(e) => {
                            fallbacks.push(FallbackReason {
                                rejected_profile: candidate.profile_id.clone(),
                                reason: FallbackReasonKind::HostIncompatible,
                                detail: format!("兼容性检查失败: {e}"),
                            });
                        }
                    }
                }
                Err(RuntimeError::ProfileResolutionFailed {
                    message: "auto: 所有候选 profile 均不兼容".to_string(),
                })
            }
            ComputePreference::GpuAuto => {
                // 只在 GPU backend 间选择
                for (i, candidate) in profiles.iter().enumerate() {
                    if !candidate.backend.is_gpu() {
                        continue;
                    }
                    match self.provider.check_compatibility(&candidate.compatibility) {
                        Ok(true) => {
                            return Ok((
                                ResolvedProfile {
                                    profile_id: candidate.profile_id.clone(),
                                    backend: candidate.backend,
                                    artifact_id: candidate.artifact_id.clone(),
                                    priority: i as u32,
                                },
                                fallbacks,
                            ));
                        }
                        Ok(false) => {
                            fallbacks.push(FallbackReason {
                                rejected_profile: candidate.profile_id.clone(),
                                reason: FallbackReasonKind::HostIncompatible,
                                detail: "本机不兼容".to_string(),
                            });
                        }
                        Err(e) => {
                            fallbacks.push(FallbackReason {
                                rejected_profile: candidate.profile_id.clone(),
                                reason: FallbackReasonKind::HostIncompatible,
                                detail: format!("兼容性检查失败: {e}"),
                            });
                        }
                    }
                }
                Err(RuntimeError::ProfileResolutionFailed {
                    message: "gpu_auto: 无兼容的 GPU profile".to_string(),
                })
            }
            ComputePreference::Cpu => {
                // 显式 CPU，失败不回退
                for (i, candidate) in profiles.iter().enumerate() {
                    if candidate.backend != ComputeBackend::Cpu {
                        continue;
                    }
                    match self.provider.check_compatibility(&candidate.compatibility) {
                        Ok(true) => {
                            return Ok((
                                ResolvedProfile {
                                    profile_id: candidate.profile_id.clone(),
                                    backend: candidate.backend,
                                    artifact_id: candidate.artifact_id.clone(),
                                    priority: i as u32,
                                },
                                fallbacks,
                            ));
                        }
                        Ok(false) => {
                            return Err(RuntimeError::ExplicitBackendFailed {
                                message: "cpu: CPU profile 兼容性检查失败".to_string(),
                            });
                        }
                        Err(e) => {
                            return Err(RuntimeError::ExplicitBackendFailed {
                                message: format!("cpu: {e}"),
                            });
                        }
                    }
                }
                Err(RuntimeError::ExplicitBackendFailed {
                    message: "cpu: descriptor 未声明 CPU profile".to_string(),
                })
            }
            ComputePreference::Cuda => {
                self.resolve_explicit_gpu(preference, ComputeBackend::Cuda, profiles)
            }
            ComputePreference::Vulkan => {
                self.resolve_explicit_gpu(preference, ComputeBackend::Vulkan, profiles)
            }
            ComputePreference::Directml => {
                self.resolve_explicit_gpu(preference, ComputeBackend::Directml, profiles)
            }
        }
    }

    /// 解析显式 GPU backend（失败不回退）。
    fn resolve_explicit_gpu(
        &self,
        preference: ComputePreference,
        target: ComputeBackend,
        profiles: &[ProfileCandidate],
    ) -> Result<(ResolvedProfile, Vec<FallbackReason>), RuntimeError> {
        for (i, candidate) in profiles.iter().enumerate() {
            if candidate.backend != target {
                continue;
            }
            match self.provider.check_compatibility(&candidate.compatibility) {
                Ok(true) => {
                    return Ok((
                        ResolvedProfile {
                            profile_id: candidate.profile_id.clone(),
                            backend: candidate.backend,
                            artifact_id: candidate.artifact_id.clone(),
                            priority: i as u32,
                        },
                        Vec::new(),
                    ));
                }
                Ok(false) => {
                    return Err(RuntimeError::ExplicitBackendFailed {
                        message: format!("{preference}: profile 兼容性检查失败"),
                    });
                }
                Err(e) => {
                    return Err(RuntimeError::ExplicitBackendFailed {
                        message: format!("{preference}: {e}"),
                    });
                }
            }
        }
        Err(RuntimeError::ExplicitBackendFailed {
            message: format!("{preference}: descriptor 未声明此 backend 的 profile"),
        })
    }
}

// ── Recovery ──────────────────────────────────────────────────────────────

/// 恢复 current.json 指针。
///
/// 如果 `current.json` 不存在或损坏，从 `generations/` 目录中扫描 manifest，
/// 选择 schema 兼容且安装时间最新的 generation 作为 current。
pub fn recover_current_pointer(engine_id: &EngineId) -> Result<Option<String>, RuntimeError> {
    // 先检查现有 current.json
    if let Ok(Some(ptr)) = read_current_pointer(engine_id) {
        // 验证指向的 generation 存在且 manifest 有效
        let manifest_result = read_manifest(engine_id, &ptr.install_id);
        if manifest_result.is_ok() {
            return Ok(Some(ptr.install_id));
        }
        tracing::warn!(
            engine = %engine_id,
            install = %ptr.install_id,
            "current.json 指向的 generation manifest 无效，尝试恢复"
        );
    }

    // 扫描所有 generation，找最新的有效 manifest
    let gens_dir = runtime::generations_dir(engine_id);
    if !gens_dir.exists() {
        return Ok(None);
    }

    let mut best: Option<(u64, String)> = None;

    for entry in std::fs::read_dir(&gens_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let install_id = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if validate_install_id(&install_id).is_err() {
            continue;
        }

        match read_manifest(engine_id, &install_id) {
            Ok(manifest) => {
                let ts = manifest.installed_at_ms;
                if best.is_none() || best.as_ref().unwrap().0 < ts {
                    best = Some((ts, install_id));
                }
            }
            Err(e) => {
                tracing::warn!(%e, install = %install_id, "跳过无效 manifest");
            }
        }
    }

    if let Some((_, install_id)) = best {
        tracing::info!(
            engine = %engine_id,
            install = %install_id,
            "恢复 current.json 指针"
        );
        let pointer = runtime::CurrentPointer {
            install_id: install_id.clone(),
            manifest_path: format!("generations/{install_id}/manifest.json"),
            updated_at_ms: runtime::now_ms(),
            schema_version: runtime::CURRENT_POINTER_SCHEMA_VERSION,
        };
        write_current_pointer(engine_id, &pointer)?;
        Ok(Some(install_id))
    } else {
        Ok(None)
    }
}

// ── Rollback ──────────────────────────────────────────────────────────────

/// 回滚到上一个 generation。
///
/// 读取 current.json，找到 previous generation（按安装时间排序的前一个），
/// 原子切换 current.json 指向 previous generation。
pub fn rollback_to_previous(engine_id: &EngineId) -> Result<Option<String>, RuntimeError> {
    let current = read_current_pointer(engine_id)?;

    let current_install_id = match current {
        Some(ref ptr) => ptr.install_id.clone(),
        None => return Ok(None),
    };

    // 扫描所有 generation，排除 current，找最新的
    let gens_dir = runtime::generations_dir(engine_id);
    if !gens_dir.exists() {
        return Ok(None);
    }

    let mut candidates: Vec<(u64, String)> = Vec::new();

    for entry in std::fs::read_dir(&gens_dir)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let install_id = match entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        if install_id == current_install_id {
            continue;
        }
        if validate_install_id(&install_id).is_err() {
            continue;
        }

        match read_manifest(engine_id, &install_id) {
            Ok(manifest) => {
                candidates.push((manifest.installed_at_ms, install_id));
            }
            Err(_) => continue,
        }
    }

    if candidates.is_empty() {
        tracing::warn!(engine = %engine_id, "无可回滚的 previous generation");
        return Ok(None);
    }

    // 按安装时间降序，选最新
    candidates.sort_by(|a, b| b.0.cmp(&a.0));
    let (_, previous_id) = &candidates[0];

    let pointer = runtime::CurrentPointer {
        install_id: previous_id.clone(),
        manifest_path: format!("generations/{previous_id}/manifest.json"),
        updated_at_ms: runtime::now_ms(),
        schema_version: runtime::CURRENT_POINTER_SCHEMA_VERSION,
    };
    write_current_pointer(engine_id, &pointer)?;

    tracing::info!(
        engine = %engine_id,
        from = %current_install_id,
        to = %previous_id,
        "已回滚到 previous generation"
    );

    Ok(Some(previous_id.clone()))
}

// ── Cleanup ───────────────────────────────────────────────────────────────

/// 执行清理操作。
///
/// 根据 `scope` 清理不同范围：
/// - `EngineGeneration`: 清理指定引擎的非 current generation 或指定 install_id。
/// - `EngineModelCache`: 清理指定引擎的模型缓存。
/// - `ProviderSharedArtifact`: 清理共享 artifact（需引用检查）。
/// - `ProviderDownloadCache`: 清理 provider 下载缓存。
pub fn execute_cleanup(scope: &CleanupScope) -> Result<(), RuntimeError> {
    match scope {
        CleanupScope::EngineGeneration {
            engine_id,
            install_ids,
        } => cleanup_engine_generations(engine_id, install_ids.as_ref()),
        CleanupScope::EngineModelCache { engine_id } => {
            let cache_dir = runtime::engine_model_cache_dir(engine_id);
            if cache_dir.exists() {
                std::fs::remove_dir_all(&cache_dir).map_err(|e| RuntimeError::CleanupFailed {
                    message: format!("删除模型缓存失败: {e}"),
                })?;
                tracing::info!(engine = %engine_id, "模型缓存已清理");
            }
            Ok(())
        }
        CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id,
        } => cleanup_shared_artifact(*runtime_kind, artifact_id),
        CleanupScope::ProviderDownloadCache { runtime_kind } => match runtime_kind {
            RuntimeKind::PythonVenv => {
                let cache = runtime::uv_cache_dir();
                if cache.exists() {
                    std::fs::remove_dir_all(&cache).map_err(|e| RuntimeError::CleanupFailed {
                        message: format!("删除 uv cache 失败: {e}"),
                    })?;
                    tracing::info!("uv 下载缓存已清理");
                }
                Ok(())
            }
            RuntimeKind::ManagedBinary => {
                // ManagedBinary download cache (future)
                Ok(())
            }
        },
    }
}

/// 清理引擎的非 current generation。
fn cleanup_engine_generations(
    engine_id: &EngineId,
    specific_install_ids: Option<&Vec<String>>,
) -> Result<(), RuntimeError> {
    let current = read_current_pointer(engine_id)?;
    let current_id = current.map(|c| c.install_id);

    let gens_dir = runtime::generations_dir(engine_id);
    if !gens_dir.exists() {
        return Ok(());
    }

    let deferred_cleanups = read_deferred_cleanups(engine_id);

    let to_clean: Vec<String> = match specific_install_ids {
        Some(ids) => ids.clone(),
        None => {
            // 清理所有非 current generation
            let mut all = Vec::new();
            for entry in std::fs::read_dir(&gens_dir)? {
                let entry = entry?;
                if !entry.file_type()?.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    if validate_install_id(name).is_ok() {
                        all.push(name.to_string());
                    }
                }
            }
            all
        }
    };

    for install_id in &to_clean {
        // 不清理 current generation
        if current_id.as_ref() == Some(install_id) {
            continue;
        }

        // 检查是否在 deferred cleanup 列表中
        if deferred_cleanups
            .iter()
            .any(|d| &d.install_id == install_id)
        {
            tracing::info!(
                engine = %engine_id,
                install = %install_id,
                "generation 被进程占用，标记 deferred cleanup，跳过强删"
            );
            continue;
        }

        let gen_dir = runtime::generation_dir(engine_id, install_id);

        // 先移除 artifact 引用
        if let Ok(manifest) = read_manifest(engine_id, install_id) {
            let _ = remove_reference(
                manifest.runtime_kind,
                &manifest.resolved_profile.artifact_id,
                engine_id.as_str(),
                install_id,
            );
        }

        if gen_dir.exists() {
            std::fs::remove_dir_all(&gen_dir).map_err(|e| RuntimeError::CleanupFailed {
                message: format!("删除 generation 失败: {e}"),
            })?;
            tracing::info!(install = %install_id, "generation 已清理");
        }
    }

    Ok(())
}

/// 清理共享 artifact（需要引用检查）。
fn cleanup_shared_artifact(
    runtime_kind: RuntimeKind,
    artifact_id: &runtime::ArtifactId,
) -> Result<(), RuntimeError> {
    // 检查显式引用计数
    let rc = runtime::ref_count(runtime_kind, artifact_id)?;
    if rc > 0 {
        return Err(RuntimeError::ArtifactStillReferenced {
            artifact_id: artifact_id.to_string(),
            ref_count: rc,
        });
    }

    // 再扫描所有 manifest 确认无引用
    let refs = scan_artifact_references(runtime_kind, artifact_id)?;
    if !refs.is_empty() {
        return Err(RuntimeError::ArtifactStillReferenced {
            artifact_id: artifact_id.to_string(),
            ref_count: refs.len(),
        });
    }

    let dir = runtime::shared_artifact_dir(runtime_kind, artifact_id);
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|e| RuntimeError::CleanupFailed {
            message: format!("删除共享 artifact 失败: {e}"),
        })?;
        tracing::info!(
            kind = %runtime_kind,
            artifact = %artifact_id,
            "共享 artifact 已清理"
        );
    }

    Ok(())
}

// ── Deferred Cleanup ──────────────────────────────────────────────────────

/// 读取 deferred cleanup 列表。
///
/// 从引擎的 deferred_cleanups.json 读取被进程占用的旧 generation 列表。
fn read_deferred_cleanups(engine_id: &EngineId) -> Vec<runtime::DeferredCleanup> {
    let path = runtime::engine_root(engine_id).join("deferred_cleanups.json");
    if !path.exists() {
        return Vec::new();
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => match serde_json::from_str::<Vec<runtime::DeferredCleanup>>(&content) {
            Ok(list) => list,
            Err(_) => Vec::new(),
        },
        Err(_) => Vec::new(),
    }
}

/// 标记 generation 为 deferred cleanup（被进程占用）。
pub fn mark_deferred_cleanup(
    engine_id: &EngineId,
    install_id: &str,
    reason: &str,
) -> Result<(), RuntimeError> {
    let mut list = read_deferred_cleanups(engine_id);

    // 避免重复
    if list.iter().any(|d| d.install_id == install_id) {
        return Ok(());
    }

    list.push(runtime::DeferredCleanup {
        install_id: install_id.to_string(),
        marked_at_ms: runtime::now_ms(),
        reason: reason.to_string(),
    });

    let path = runtime::engine_root(engine_id).join("deferred_cleanups.json");
    runtime::atomic_write_json(&path, &list)?;

    tracing::info!(
        engine = %engine_id,
        install = %install_id,
        reason = reason,
        "已标记 deferred cleanup"
    );

    Ok(())
}

/// 清除 deferred cleanup 标记。
pub fn clear_deferred_cleanup(engine_id: &EngineId, install_id: &str) -> Result<(), RuntimeError> {
    let mut list = read_deferred_cleanups(engine_id);
    list.retain(|d| d.install_id != install_id);

    let path = runtime::engine_root(engine_id).join("deferred_cleanups.json");
    if list.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        runtime::atomic_write_json(&path, &list)?;
    }

    Ok(())
}

// ── 工具函数 ──────────────────────────────────────────────────────────────

/// 递归复制目录。
fn copy_dir_recursive(src: &std::path::Path, dst: &std::path::Path) -> Result<(), std::io::Error> {
    if !dst.exists() {
        std::fs::create_dir_all(dst)?;
    }

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let from = entry.path();
        let to = dst.join(entry.file_name());

        if file_type.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)?;
        }
    }

    Ok(())
}

// ── 旧 FunASR venv 迁移 ───────────────────────────────────────────────────

/// 旧 FunASR venv 迁移结果。
#[derive(Debug, Clone)]
pub enum MigrationResult {
    /// 迁移成功：旧 venv 已登记为 generation。
    Migrated { install_id: String },
    /// 旧 venv 不存在，无需迁移。
    NotNeeded,
    /// 旧 venv 存在但不兼容，需要重建。
    NeedsRebuild { reason: String },
}

/// 检查并执行旧 FunASR venv 兼容迁移。
///
/// 旧路径：`%APPDATA%\blink\python\venv`
/// 新路径：`runtimes/engines/funasr/generations/{install_id}/`
///
/// 迁移策略：
/// 1. 旧 venv 不存在 → `NotNeeded`
/// 2. 旧 venv 存在且可验证兼容 → 登记为 generation，写 manifest，原子切换 current.json
/// 3. 旧 venv 存在但不兼容 → `NeedsRebuild`（不删除旧 venv 或模型缓存）
pub fn migrate_legacy_funasr_venv() -> Result<MigrationResult, RuntimeError> {
    let legacy_dir = runtime::legacy_funasr_venv_dir();

    if !legacy_dir.exists() || !legacy_dir.join("Scripts").join("python.exe").exists() {
        return Ok(MigrationResult::NotNeeded);
    }

    let engine_id = match EngineId::new("funasr") {
        Ok(id) => id,
        Err(_) => {
            return Ok(MigrationResult::NeedsRebuild {
                reason: "EngineId 校验失败".to_string(),
            });
        }
    };

    // 检查是否已有 current.json（已迁移过）
    if let Ok(Some(_)) = read_current_pointer(&engine_id) {
        // 已迁移过，旧 venv 可忽略
        return Ok(MigrationResult::NotNeeded);
    }

    tracing::info!(
        legacy = %legacy_dir.display(),
        "检测到旧 FunASR venv，尝试兼容迁移"
    );

    // 移动前先验证旧环境。不能以“目录存在”或历史上曾运行过代替兼容性
    // self-test，否则会把损坏环境登记成 current generation。
    let legacy_python = legacy_dir.join("Scripts").join("python.exe");
    let probe = crate::infra::platform::no_window(std::process::Command::new(&legacy_python))
        .args([
            "-I",
            "-c",
            "import sys; from importlib.metadata import version; print(f'{sys.version_info.major}.{sys.version_info.minor}.{sys.version_info.micro}'); print(version('torch')); print(version('funasr'))",
        ])
        .output();
    let probe = match probe {
        Ok(output) if output.status.success() => output,
        Ok(output) => {
            return Ok(MigrationResult::NeedsRebuild {
                reason: format!(
                    "旧 venv 兼容验证失败 (exit={:?}): {}",
                    output.status.code(),
                    String::from_utf8_lossy(&output.stderr).trim()
                ),
            });
        }
        Err(error) => {
            return Ok(MigrationResult::NeedsRebuild {
                reason: format!("无法执行旧 venv Python: {error}"),
            });
        }
    };
    let versions: Vec<&str> = std::str::from_utf8(&probe.stdout)
        .unwrap_or_default()
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    if versions.len() != 3 || !versions[0].starts_with("3.12.") {
        return Ok(MigrationResult::NeedsRebuild {
            reason: format!("旧 venv 版本探测结果不兼容: {versions:?}"),
        });
    }

    // Python distribution 的可复核身份至少绑定实际解释器内容，禁止再写
    // `legacy-migrated` 之类占位字符串。
    let python_bytes = std::fs::read(&legacy_python)?;
    let python_sha256 = format!("{:x}", Sha256::digest(&python_bytes));
    let python_version = versions[0].to_string();
    let torch_version = versions[1].to_string();
    let funasr_version = versions[2].to_string();

    // 生成 install_id
    let install_id = generate_install_id();
    let gen_dir = runtime::generation_dir(&engine_id, &install_id);
    let gens_dir = runtime::generations_dir(&engine_id);
    std::fs::create_dir_all(&gens_dir)?;

    // 尝试 symlink 或移动旧 venv 到 generation 目录
    // Windows 上 symlink 需要管理员权限，因此使用 junction 或直接移动
    // 这里采用保守策略：创建一个指针文件指向旧目录，而不是物理移动
    // 实际迁移由 0.22.3 的 LocalEngineService 决定是否物理移动

    // 先尝试 rename（同卷原子移动）
    match std::fs::rename(&legacy_dir, &gen_dir) {
        Ok(()) => {
            tracing::info!(install = %install_id, "旧 venv 已移动到 generation 目录");
        }
        Err(e) => {
            tracing::warn!(%e, "rename 失败，尝试 junction 或标记为 NeedsRebuild");
            // 回退：标记为需要重建，不删除旧 venv
            return Ok(MigrationResult::NeedsRebuild {
                reason: format!("无法移动旧 venv: {e}"),
            });
        }
    }

    // 构建 manifest（兼容版：使用旧 venv 的已知状态）
    let artifact_id = runtime::ArtifactId::new(format!("python-{python_version}-legacy-x64"))?;
    let manifest = GenerationManifest {
        schema_version: runtime::MANIFEST_SCHEMA_VERSION,
        engine_id: engine_id.clone(),
        runtime_kind: RuntimeKind::PythonVenv,
        install_id: install_id.clone(),
        requested_preference: ComputePreference::Cpu,
        resolved_profile: ResolvedProfile {
            profile_id: "cpu-x64-legacy".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: artifact_id.clone(),
            priority: 0,
        },
        installed_at_ms: runtime::now_ms(),
        artifact: runtime::ArtifactIdentity {
            runtime_kind: RuntimeKind::PythonVenv,
            artifact_id: artifact_id.clone(),
            sha256: python_sha256,
        },
        model_contract: ModelContract {
            model_id: "funasr-legacy".to_string(),
            revision: "legacy".to_string(),
            checksum_source: runtime::ChecksumSource::Unverified,
        },
        fallback_reasons: Vec::new(),
        extension: ManifestExtension::PythonVenv(runtime::PythonManifestExt {
            python_version,
            python_artifact_id: artifact_id.clone(),
            packages: vec![
                runtime::PackageStatus {
                    name: "torch".to_string(),
                    installed_version: Some(torch_version.clone()),
                    locked_version: torch_version,
                    satisfies_lock: true,
                },
                runtime::PackageStatus {
                    name: "funasr".to_string(),
                    installed_version: Some(funasr_version.clone()),
                    locked_version: funasr_version,
                    satisfies_lock: true,
                },
            ],
            uv_version: "unknown".to_string(),
            index_url: None,
            self_test_passed: true,
        }),
    };

    // 写 manifest
    write_manifest(&engine_id, &install_id, &manifest)?;

    // 添加 artifact 引用
    add_reference(
        RuntimeKind::PythonVenv,
        &artifact_id,
        engine_id.as_str(),
        &install_id,
    )?;

    // 原子切换 current.json
    let pointer = runtime::CurrentPointer {
        install_id: install_id.clone(),
        manifest_path: format!("generations/{install_id}/manifest.json"),
        updated_at_ms: runtime::now_ms(),
        schema_version: runtime::CURRENT_POINTER_SCHEMA_VERSION,
    };
    write_current_pointer(&engine_id, &pointer)?;

    tracing::info!(install = %install_id, "旧 FunASR venv 迁移完成");

    Ok(MigrationResult::Migrated { install_id })
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── fake provider for testing ─────────────────────────────────────────

    struct FakeProvider {
        compatible: bool,
        prepare_ok: bool,
        self_test_ok: bool,
    }

    #[async_trait::async_trait]
    impl RuntimeProvider for FakeProvider {
        fn kind(&self) -> RuntimeKind {
            RuntimeKind::PythonVenv
        }

        fn check_compatibility(
            &self,
            compatibility: &CompatibilityCheck,
        ) -> Result<bool, RuntimeError> {
            // Always 兼容性始终通过（模拟 CPU x64 的真实行为）
            if matches!(compatibility, CompatibilityCheck::Always) {
                return Ok(true);
            }
            Ok(self.compatible)
        }

        async fn prepare_environment(
            &self,
            staging_dir: &std::path::Path,
            _plan: &InstallPlan,
            resolved_profile: &ResolvedProfile,
        ) -> Result<PrepareResult, RuntimeError> {
            if !self.prepare_ok {
                return Err(RuntimeError::InstallFailed {
                    message: "fake prepare failure".to_string(),
                });
            }
            std::fs::create_dir_all(staging_dir.join("venv"))?;
            Ok(PrepareResult {
                artifact: runtime::ArtifactIdentity {
                    runtime_kind: RuntimeKind::PythonVenv,
                    artifact_id: resolved_profile.artifact_id.clone(),
                    sha256: "fake-hash-0001".to_string(),
                },
            })
        }

        async fn self_test(
            &self,
            _generation_dir: &std::path::Path,
            _plan: &InstallPlan,
        ) -> Result<(), RuntimeError> {
            if !self.self_test_ok {
                return Err(RuntimeError::SelfTestFailed {
                    message: "fake self-test failure".to_string(),
                });
            }
            Ok(())
        }

        fn build_manifest_extension(
            &self,
            _generation_dir: &std::path::Path,
            _plan: &InstallPlan,
        ) -> Result<ManifestExtension, RuntimeError> {
            Ok(ManifestExtension::PythonVenv(runtime::PythonManifestExt {
                python_version: "3.12.8".to_string(),
                python_artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
                packages: Vec::new(),
                uv_version: "0.6.10".to_string(),
                index_url: None,
                self_test_passed: true,
            }))
        }

        fn query_package_status(
            &self,
            _generation_dir: &std::path::Path,
            _plan: &InstallPlan,
        ) -> Result<Vec<runtime::PackageStatus>, RuntimeError> {
            Ok(Vec::new())
        }

        fn cleanup_provider_cache(
            &self,
            _scope: &ProviderCleanupScope,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
    }

    fn fake_descriptor(engine_id: &str, profiles: Vec<ProfileCandidate>) -> ProviderDescriptor {
        ProviderDescriptor {
            engine_id: EngineId::new(engine_id).unwrap(),
            runtime_kind: RuntimeKind::PythonVenv,
            display_name: "Fake Engine".to_string(),
            profiles,
            model_contract: ModelContract {
                model_id: "fake-model".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: runtime::ChecksumSource::Unverified,
            },
            install_plan: InstallPlan::PythonVenv(PythonInstallPlan {
                python_version: "3.12.8".to_string(),
                python_artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
                packages: Vec::new(),
                uv_version: "0.6.10".to_string(),
                index_url: None,
                extra_pip_args: Vec::new(),
                self_test_script: "pass".to_string(),
            }),
            min_generations: 2,
        }
    }

    fn cpu_profile() -> ProfileCandidate {
        ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
            compatibility: CompatibilityCheck::Always,
        }
    }

    fn cuda_profile() -> ProfileCandidate {
        ProfileCandidate {
            profile_id: "cuda12-sm86".to_string(),
            backend: ComputeBackend::Cuda,
            artifact_id: runtime::ArtifactId::new("python-3.12.8-cuda").unwrap(),
            compatibility: CompatibilityCheck::RequiresCuda {
                min_version: Some("12.0".to_string()),
            },
        }
    }

    // ── profile 解析测试 ──────────────────────────────────────────────────

    #[test]
    fn resolve_auto_falls_back_to_cpu() {
        // CUDA 不兼容 → 回退到 CPU
        let desc = fake_descriptor("test-auto-fallback", vec![cuda_profile(), cpu_profile()]);
        let provider = FakeProvider {
            compatible: false, // CUDA 不兼容
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let (resolved, fallbacks) = tx.resolve_profile(ComputePreference::Auto).unwrap();
        assert_eq!(resolved.backend, ComputeBackend::Cpu);
        assert_eq!(fallbacks.len(), 1);
        assert_eq!(fallbacks[0].rejected_profile, "cuda12-sm86");
    }

    #[test]
    fn resolve_auto_picks_first_compatible() {
        let desc = fake_descriptor("test-auto-first", vec![cpu_profile()]);
        let provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let (resolved, fallbacks) = tx.resolve_profile(ComputePreference::Auto).unwrap();
        assert_eq!(resolved.backend, ComputeBackend::Cpu);
        assert!(fallbacks.is_empty());
    }

    #[test]
    fn resolve_explicit_cpu_no_fallback() {
        // 显式 CPU，即使有 CUDA profile 也不回退
        let desc = fake_descriptor("test-explicit-cpu", vec![cuda_profile(), cpu_profile()]);
        let provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let (resolved, _) = tx.resolve_profile(ComputePreference::Cpu).unwrap();
        assert_eq!(resolved.backend, ComputeBackend::Cpu);
    }

    #[test]
    fn resolve_explicit_cuda_no_fallback_on_incompatible() {
        let desc = fake_descriptor(
            "test-explicit-cuda-fail",
            vec![cpu_profile(), cuda_profile()],
        );
        let provider = FakeProvider {
            compatible: false, // CUDA 不兼容
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let result = tx.resolve_profile(ComputePreference::Cuda);
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ExplicitBackendFailed { .. } => {}
            other => panic!("期望 ExplicitBackendFailed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_gpu_auto_skips_cpu() {
        let desc = fake_descriptor("test-gpu-auto", vec![cpu_profile(), cuda_profile()]);
        let provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let (resolved, _) = tx.resolve_profile(ComputePreference::GpuAuto).unwrap();
        assert_eq!(resolved.backend, ComputeBackend::Cuda);
    }

    #[test]
    fn resolve_gpu_auto_fails_when_no_gpu_compatible() {
        let desc = fake_descriptor("test-gpu-auto-fail", vec![cpu_profile(), cuda_profile()]);
        let provider = FakeProvider {
            compatible: false,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let result = tx.resolve_profile(ComputePreference::GpuAuto);
        assert!(result.is_err());
    }

    #[test]
    fn resolve_explicit_backend_not_in_descriptor() {
        // descriptor 只声明 CPU，显式请求 Vulkan
        let desc = fake_descriptor("test-vulkan-not-declared", vec![cpu_profile()]);
        let provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let result = tx.resolve_profile(ComputePreference::Vulkan);
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ExplicitBackendFailed { .. } => {}
            other => panic!("期望 ExplicitBackendFailed, got {other:?}"),
        }
    }

    #[test]
    fn resolve_auto_all_incompatible_fails() {
        let desc = fake_descriptor("test-auto-all-incompat", vec![cuda_profile()]);
        let provider = FakeProvider {
            compatible: false,
            prepare_ok: true,
            self_test_ok: true,
        };
        let tx = InstallTransaction::new(&desc, &provider);

        let result = tx.resolve_profile(ComputePreference::Auto);
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ProfileResolutionFailed { .. } => {}
            other => panic!("期望 ProfileResolutionFailed, got {other:?}"),
        }
    }

    // ── cleanup 测试 ──────────────────────────────────────────────────────

    #[test]
    fn cleanup_shared_artifact_with_refs_rejected() {
        let artifact_id = runtime::ArtifactId::new("test-cleanup-ref-0001").unwrap();
        let kind = RuntimeKind::PythonVenv;

        // 清理可能存在的测试数据
        let dir = runtime::shared_artifact_dir(kind, &artifact_id);
        let _ = std::fs::remove_dir_all(&dir);

        // 添加引用
        add_reference(kind, &artifact_id, "engine-a", "gen-001").unwrap();

        // 尝试清理应被拒绝
        let result = execute_cleanup(&CleanupScope::ProviderSharedArtifact {
            runtime_kind: kind,
            artifact_id: artifact_id.clone(),
        });
        assert!(result.is_err());
        match result.unwrap_err() {
            RuntimeError::ArtifactStillReferenced { ref_count, .. } => {
                assert!(ref_count >= 1);
            }
            other => panic!("期望 ArtifactStillReferenced, got {other:?}"),
        }

        // 移除引用后清理应成功
        remove_reference(kind, &artifact_id, "engine-a", "gen-001").unwrap();
        let result = execute_cleanup(&CleanupScope::ProviderSharedArtifact {
            runtime_kind: kind,
            artifact_id,
        });
        assert!(result.is_ok());
    }

    // ── deferred cleanup 测试 ─────────────────────────────────────────────

    #[test]
    fn deferred_cleanup_mark_and_clear() {
        let engine_id = EngineId::new("test-deferred-cleanup").unwrap();

        // 清理可能存在的测试数据
        let engine_root = runtime::engine_root(&engine_id);
        let _ = std::fs::remove_dir_all(&engine_root);

        mark_deferred_cleanup(&engine_id, "gen-001", "process still running").unwrap();

        let list = read_deferred_cleanups(&engine_id);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].install_id, "gen-001");

        // 重复标记不会增加
        mark_deferred_cleanup(&engine_id, "gen-001", "duplicate").unwrap();
        let list = read_deferred_cleanups(&engine_id);
        assert_eq!(list.len(), 1);

        // 清除
        clear_deferred_cleanup(&engine_id, "gen-001").unwrap();
        let list = read_deferred_cleanups(&engine_id);
        assert!(list.is_empty());

        // 清理测试数据
        let _ = std::fs::remove_dir_all(&engine_root);
    }

    #[tokio::test]
    async fn two_python_engines_have_isolated_generations_and_shared_artifact() {
        let shared_artifact = runtime::ArtifactId::new("python-3.12.8-isolation-test").unwrap();
        let shared_profile = ProfileCandidate {
            artifact_id: shared_artifact.clone(),
            ..cpu_profile()
        };
        let funasr = fake_descriptor("test-funasr-isolated", vec![shared_profile.clone()]);
        let paddleocr = fake_descriptor("test-paddleocr-isolated", vec![shared_profile]);
        let provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let _ = std::fs::remove_dir_all(runtime::engine_root(&funasr.engine_id));
        let _ = std::fs::remove_dir_all(runtime::engine_root(&paddleocr.engine_id));

        let funasr_result = InstallTransaction::new(&funasr, &provider)
            .execute(ComputePreference::Cpu)
            .await
            .unwrap();
        let paddle_result = InstallTransaction::new(&paddleocr, &provider)
            .execute(ComputePreference::Cpu)
            .await
            .unwrap();

        let funasr_generation =
            runtime::generation_dir(&funasr.engine_id, &funasr_result.install_id);
        let paddle_generation =
            runtime::generation_dir(&paddleocr.engine_id, &paddle_result.install_id);
        assert_ne!(funasr_generation, paddle_generation);
        assert!(funasr_generation.join("venv").is_dir());
        assert!(paddle_generation.join("venv").is_dir());
        assert_eq!(
            runtime::ref_count(RuntimeKind::PythonVenv, &shared_artifact).unwrap(),
            2
        );

        let _ = std::fs::remove_dir_all(runtime::engine_root(&funasr.engine_id));
        let _ = std::fs::remove_dir_all(runtime::engine_root(&paddleocr.engine_id));
        let _ = std::fs::remove_dir_all(runtime::shared_artifact_dir(
            RuntimeKind::PythonVenv,
            &shared_artifact,
        ));
    }

    #[tokio::test]
    async fn failed_reinstall_preserves_previous_current_generation() {
        let rollback_artifact = runtime::ArtifactId::new("python-3.12.8-rollback-test").unwrap();
        let descriptor = fake_descriptor(
            "test-install-rollback",
            vec![ProfileCandidate {
                artifact_id: rollback_artifact.clone(),
                ..cpu_profile()
            }],
        );
        let _ = std::fs::remove_dir_all(runtime::engine_root(&descriptor.engine_id));
        let good_provider = FakeProvider {
            compatible: true,
            prepare_ok: true,
            self_test_ok: true,
        };
        let first = InstallTransaction::new(&descriptor, &good_provider)
            .execute(ComputePreference::Cpu)
            .await
            .unwrap();

        let failing_provider = FakeProvider {
            compatible: true,
            prepare_ok: false,
            self_test_ok: true,
        };
        assert!(
            InstallTransaction::new(&descriptor, &failing_provider)
                .execute(ComputePreference::Cpu)
                .await
                .is_err()
        );
        let current = read_current_pointer(&descriptor.engine_id)
            .unwrap()
            .unwrap();
        assert_eq!(current.install_id, first.install_id);

        let _ = std::fs::remove_dir_all(runtime::engine_root(&descriptor.engine_id));
        let _ = std::fs::remove_dir_all(runtime::shared_artifact_dir(
            RuntimeKind::PythonVenv,
            &rollback_artifact,
        ));
    }
}
