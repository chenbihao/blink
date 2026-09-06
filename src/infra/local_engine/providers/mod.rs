//! Runtime Provider trait、descriptor、部署安装事务与 profile 解析。
//!
//! ## 分层归属
//!
//! - `providers`：定义 provider trait 和 descriptor，编排 staging → slot promote
//!   → deployment.json 原子切换的通用事务流程（journal fail-closed）。
//! - `providers/binary`：`ManagedBinaryProvider` 协议位（闭合变体，不实现完整逻辑）。
//! - `providers/onnx`：`OnnxRuntimeProvider`（0.22.8，ORT + 模型联合安装）。
//! - 0.22.10：`providers/python`（PythonVenvProvider）已随 Python/uv 栈退役删除；
//!   磁盘上的 legacy PythonVenv manifest 仍可安全读取（serde 变体保留）。
//! - 本模块不依赖 app/tauri，只使用 domain 身份类型和 infra 内部类型。
//!
//! ## 安装事务（slot + journal，见 `infra/local_engine/deployment`）
//!
//! ```text
//! begin:      DeploymentStore::begin → journal{Building, candidate, previous}
//! build:      staging/{operation-id}/ → provider.prepare_environment → self-test
//! promote:    staging → slot-{candidate}（rename）
//! pre-switch: journal.phase = Switched（指针切换之前写）
//! switch:     deployment.json → candidate（原子替换）
//! verify:     重读 manifest + artifact identity；失败 → 自动回滚 previous
//! commit:     journal.phase = Committed → 删除旧 slot（占用记 residue）→ 清 journal
//! ```
//!
//! 事务期间最多存在 old + candidate 两个 slot；成功后稳定状态只保留 active。

pub mod binary;
pub mod onnx;

use super::deployment::{
    DEPLOYMENT_POINTER_SCHEMA_VERSION, DeploymentPointer, DeploymentSlot, DeploymentSpace,
    DeploymentStore, TransactionPhase,
};
use super::runtime::{
    self, CleanupScope, ComputeBackend, ComputePreference, DeploymentManifest, EngineId,
    FallbackReason, FallbackReasonKind, ManifestExtension, ModelContract, ResolvedProfile,
    RuntimeError, RuntimePlan, generate_install_id, scan_artifact_references,
    validate_operation_id,
};

// ── ProviderDescriptor ────────────────────────────────────────────────────

/// 引擎描述符（编译期内置 allowlist，不接受前端动态传入）。
///
/// 声明引擎静态事实：稳定 id、runtime plan、候选 profile、模型契约与安装计划。
/// adapter 行为由各 provider 实现，不编码成通用字符串 DSL。
#[derive(Debug, Clone)]
pub struct ProviderDescriptor {
    /// 引擎稳定标识符。
    pub engine_id: EngineId,
    /// 运行时计划（闭合枚举）。
    pub runtime_kind: RuntimePlan,
    /// 引擎显示名称（人类可读，非路径用）。
    pub display_name: String,
    /// 候选 profile 列表（按优先级排序，0 = 最高优先级）。
    pub profiles: Vec<ProfileCandidate>,
    /// 模型契约（锁定模型身份）。
    pub model_contract: ModelContract,
    /// 安装计划（provider 专属，闭合枚举）。
    pub install_plan: InstallPlan,
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
///
/// 闭合协议枚举，只保留 0.22.7 GGUF 链路明确需要的最小集合
/// （cuda / vulkan / cpu feature）；GPU/feature 分支由 ManagedBinary
/// provider 落地时构造，当前只使用 `Always` 与 `RequiresCuda`。
#[allow(dead_code)] // 0.22.7 ManagedBinary 协议位——变体由 binary descriptor 构造
#[derive(Debug, Clone)]
pub enum CompatibilityCheck {
    /// 总是兼容（如 CPU x64）。
    Always,
    /// 需要 CUDA GPU（provider 检查 nvidia-smi / 驱动版本）。
    RequiresCuda { min_version: Option<String> },
    /// 需要 Vulkan 驱动。
    RequiresVulkan,
    /// 需要 CPU feature（如 AVX2）。
    RequiresCpuFeature { feature: String },
}

/// 安装计划（provider 专属，闭合枚举）。
#[derive(Debug, Clone)]
pub enum InstallPlan {
    /// Managed binary 安装计划（协议位保留：首个 binary 引擎接入时构造）。
    #[allow(dead_code)]
    ManagedBinary(BinaryInstallPlan),
    /// ONNX Runtime 安装计划（0.22.8）。
    ///
    /// ORT DLL 使用版本化、不可变 artifact；模型 det/rec/dictionary 使用
    /// `model_storage` generation。Provider 负责 staging、hash 校验和 promote，
    /// 不伪装成 `ManagedBinary`（不启动子进程）。
    #[allow(dead_code)]
    OnnxRuntime(OnnxInstallPlan),
}

/// Managed binary 安装计划。
///
/// 两种来源（互斥）：
/// - `bundled_dir = Some`：随 Blink 发布捆绑的 worker 资源目录
///   （0.22.7 GGUF worker；安装时从资源目录校验 hash 后复制，无网络）；
/// - 否则：锁定 URL 下载 archive（网络型 binary 引擎预留）。
#[derive(Debug, Clone)]
pub struct BinaryInstallPlan {
    /// archive artifact id。
    pub archive_artifact_id: runtime::ArtifactId,
    /// archive 下载 URL。
    #[allow(dead_code)] // 网络型 binary 引擎（预留）消费；bundled 模式不使用
    pub archive_url: String,
    /// archive SHA-256（hex）。
    #[allow(dead_code)] // 网络型 binary 引擎（预留）消费；bundled 模式以随发布 manifest 为准
    pub archive_sha256: String,
    /// 可执行文件路径（相对于部署根）。
    pub executable: String,
    /// 引用的共享 stdlib artifact（可选，预留协议位，当前无引擎使用）。
    pub stdlib_artifact: Option<runtime::ArtifactIdentity>,
    /// required CPU features。
    pub required_cpu_features: Vec<String>,
    /// required drivers。
    pub required_drivers: Vec<String>,
    /// self-test 命令行（在部署根执行）。
    pub self_test_command: Vec<String>,
    /// 捆绑资源目录（相对于发布资源根，如 "bin/funasr-worker"）。
    /// `Some` 时安装走捆绑资源 + 随发布 manifest 校验，忽略网络字段。
    pub bundled_dir: Option<String>,
}

/// ONNX Runtime 安装计划（0.22.8）。
///
/// 描述 ORT DLL 和模型的联合 staging/promote 事务。
/// DLL 和模型分别 staging、校验和 promote，未被 deployment 引用的
/// 中间产物只作为可清扫 residue；只有两者联合 self-test 通过后
/// 才提交 engine deployment pointer。
#[allow(dead_code)] // 0.22.8 A 包协议位——B 包消费
#[derive(Debug, Clone)]
pub struct OnnxInstallPlan {
    /// ORT DLL artifact id（版本化、不可变）。
    pub dll_artifact_id: runtime::ArtifactId,
    /// ORT 版本（如 `1.20.0`）。
    pub ort_version: String,
    /// ORT DLL 下载 URL。
    #[allow(dead_code)]
    pub dll_url: String,
    /// ORT DLL SHA-256（hex）。
    #[allow(dead_code)]
    pub dll_sha256: String,
    /// ORT `inter_op` 线程数（默认 1）。
    pub inter_op: u32,
    /// ORT `intra_op` 线程数上限（不超过 4，避免抢占 Alt+Space 主链路）。
    pub intra_op: u32,
    /// 执行 provider 模式（如 `cpu` / `dml`）。
    pub execution_provider: String,
}

// ── InstallSink ───────────────────────────────────────────────────────────

/// 安装进度与日志 sink——provider/事务通过此 trait 上报进度和实时日志。
///
/// **设计铁则**：
/// - 不依赖 Tauri/`AppHandle`/`windows` crate——infra 层 trait。
/// - 调用方（app 层）提供实现，把 sink 调用桥接为 Tauri 事件。
/// - Provider 实现应在执行子进程或长耗时下载（模型/archive）期间，
///   实时把 stdout/stderr 行通过 `on_log` 上报，不等进程结束。
/// - `on_stage` 用于提交阶段变更（Preparing/Downloading/...）。
/// - 所有方法接受 `&self`，实现需内部可变（如 `Mutex`/`Arc`）。
/// - 日志洪泛保护：实现应内部做速率限制或有界缓冲。
///
/// **日志隔离**：
/// - 安装日志以 `operation_id` 隔离，不与运行服务日志（`instance_id`）混淆。
/// - sink 实现持有当前 `operation_id`，自动附加到每条日志。
pub trait InstallSink: Send + Sync {
    /// 上报阶段变更。
    ///
    /// `stage` 是稳定的 wire 字符串（对应 `OperationStage` 的 Display 值）。
    /// 调用方应在每次阶段切换时调用此方法。
    fn on_stage(&self, stage: &str);

    /// 上报一行安装日志。
    ///
    /// `level` 是 "info" / "warn" / "error"。
    /// `text` 是已做 UTF-8 lossy + 长度截断的日志行。
    /// 实现应内部做洪泛保护（如单位时间内最多 N 条）。
    fn on_log(&self, level: &str, text: &str);
}

/// 空实现（测试用）。
#[cfg(test)]
pub struct NoopInstallSink;

#[cfg(test)]
impl InstallSink for NoopInstallSink {
    fn on_stage(&self, _stage: &str) {}
    fn on_log(&self, _level: &str, _text: &str) {}
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
/// `ManagedBinaryProvider` 负责捆绑/下载/解包/验证 archive；
/// `OnnxRuntimeProvider` 负责 ORT DLL 与模型的联合 staging/校验。
#[async_trait::async_trait]
pub trait RuntimeProvider: Send + Sync {
    /// 运行时计划。
    fn kind(&self) -> RuntimePlan;

    /// 检查本机兼容性（如 CUDA GPU 是否存在、CPU feature 是否支持）。
    fn check_compatibility(&self, compatibility: &CompatibilityCheck)
    -> Result<bool, RuntimeError>;

    /// 在 staging 目录中准备环境。
    ///
    /// - ManagedBinary: 下载/解包锁定 archive、验证文件 hash。
    /// - OnnxRuntime: ORT DLL 与模型联合 staging、校验。
    ///
    /// 返回 `PrepareResult`，包含已验证的 artifact 身份标识（含真实 SHA-256 hash）。
    /// 此 hash 写入部署 manifest，用于后续完整性验证。
    ///
    /// `cancel_token` 用于在 provider 内部长耗时操作（如模型下载、
    /// archive 下载）执行期间响应取消信号。Provider 实现应在 `tokio::select!`
    /// 中同时监听操作 future 和 `cancel_token.cancelled()`，
    /// 被取消时返回 `RuntimeError::OperationCancelled`。
    ///
    /// `sink` 用于实时上报安装进度和 stdout/stderr 日志。
    async fn prepare_environment(
        &self,
        staging_dir: &std::path::Path,
        plan: &InstallPlan,
        resolved_profile: &ResolvedProfile,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<PrepareResult, RuntimeError>;

    /// 执行 provider self-test。
    ///
    /// 在已准备好的环境中执行最小验证。
    async fn self_test(
        &self,
        deployment_dir: &std::path::Path,
        plan: &InstallPlan,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError>;

    /// 从已安装的部署读取 provider 专属状态。
    ///
    /// 返回 manifest extension（不含通用状态机字段）。
    fn build_manifest_extension(
        &self,
        deployment_dir: &std::path::Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError>;
}

// ── InstallTransaction ───────────────────────────────────────────────────

/// 安装事务结果。
#[derive(Debug, Clone)]
pub struct InstallResult {
    /// 新部署的 install id。
    pub install_id: String,
    /// 本次 operation id。
    pub operation_id: String,
    /// 是否发生了 profile fallback（requested != resolved）。
    pub fell_back: bool,
}

/// 安装事务编排器。
///
/// 编排 journal begin → staging prepare → self-test → promote slot →
/// 原子切换 deployment.json → 切换后验证 → commit（删旧 slot）/ rollback。
///
/// 事务写入**构造时指定的 `DeploymentSpace`**（engine id + implementation）——
/// 一个 implementation 的安装事务只作用于自己的 pointer/slot/journal，
/// 与同引擎其他 implementation 的部署互不可见。
///
/// 失败语义：
/// - 切换前失败：清 staging，old 部署不受影响；
/// - 切换后验证失败：指针原子切回 previous，candidate slot 尽力删除
///   （Windows 占用时记 residue），返回错误；
/// - 成功：删除旧 slot（占用记 residue），清除 journal。
pub struct InstallTransaction<'a, P: RuntimeProvider> {
    descriptor: &'a ProviderDescriptor,
    provider: &'a P,
    space: DeploymentSpace,
}

impl<'a, P: RuntimeProvider> InstallTransaction<'a, P> {
    /// 创建安装事务。
    ///
    /// `space` 决定事务的部署空间（pointer/slot/journal/staging 位置）；
    /// 其 engine 归属必须与 descriptor 一致（不一致 fail-closed 拒绝）。
    pub fn new(
        descriptor: &'a ProviderDescriptor,
        provider: &'a P,
        space: DeploymentSpace,
    ) -> Self {
        debug_assert_eq!(
            space.engine_id(),
            &descriptor.engine_id,
            "DeploymentSpace 的 engine 与 ProviderDescriptor 不一致"
        );
        Self {
            descriptor,
            provider,
            space,
        }
    }

    /// 执行完整安装事务。
    ///
    /// `operation_id` 由调用方（EngineManager）生成并已登记操作协调器；
    /// journal、staging 目录与取消语义都以此 id 为准。
    pub async fn execute(
        &self,
        operation_id: &str,
        preference: ComputePreference,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<InstallResult, RuntimeError> {
        let space = &self.space;
        let engine_id = space.engine_id();
        if engine_id != &self.descriptor.engine_id {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "部署空间 engine ({}) 与 descriptor engine ({}) 不一致",
                    engine_id, self.descriptor.engine_id
                ),
            });
        }
        debug_assert_eq!(
            self.provider.kind(),
            self.descriptor.runtime_kind,
            "provider 与 descriptor 的 runtime_kind 不一致"
        );
        tracing::info!(
            engine = %engine_id,
            implementation = ?space.implementation(),
            display = %self.descriptor.display_name,
            operation_id,
            "开始运行时安装事务"
        );
        validate_operation_id(operation_id)?;
        let install_id = generate_install_id();
        runtime::validate_install_id(&install_id)?;

        // ── 0. 初始取消检查 ──
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            if let Some(s) = sink {
                s.on_stage("cancelled");
            }
            return Err(RuntimeError::OperationCancelled {
                message: "安装事务在开始前被取消".to_string(),
            });
        }

        if let Some(s) = sink {
            s.on_stage("preparing");
            s.on_log("info", "正在准备安装环境...");
        }

        // ── 1. resolve profile ──
        let (resolved_profile, fallback_reasons) = self.resolve_profile(preference)?;
        let fell_back = !fallback_reasons.is_empty();

        // ── 2. 事务 begin：写 journal（任何破坏性步骤之前），确定 candidate slot ──
        let mut journal = DeploymentStore::begin(space, operation_id, &install_id)?;
        let candidate_slot = DeploymentSlot::parse(&journal.candidate_slot)?;
        let candidate_dir = candidate_slot.dir_in(space);

        // 清扫其他 operation 的孤儿 staging（保留本 operation 目录）
        DeploymentStore::sweep_staging_except(space, operation_id);

        let staging = space.operation_staging_dir(operation_id);
        std::fs::create_dir_all(&staging).map_err(|e| {
            // begin 已写 journal——清掉，避免下次启动误恢复
            let _ = DeploymentStore::clear_journal(space);
            RuntimeError::StagingCreateFailed {
                message: format!("创建 staging 目录失败: {e}"),
            }
        })?;

        if let Some(s) = sink {
            s.on_log(
                "info",
                &format!("staging 目录已创建: {}", staging.display()),
            );
        }

        tracing::info!(
            engine = %engine_id,
            op = %operation_id,
            candidate_slot = %candidate_slot,
            staging = %staging.display(),
            "部署事务开始（journal 已写入）"
        );

        // ── 3. prepare environment ──
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = DeploymentStore::clear_journal(space);
            if let Some(s) = sink {
                s.on_stage("cancelled");
            }
            return Err(RuntimeError::OperationCancelled {
                message: format!("安装事务在 prepare_environment 前被取消 (op={operation_id})"),
            });
        }

        if let Some(s) = sink {
            s.on_stage("downloading");
        }

        let prepare_result = match self
            .provider
            .prepare_environment(
                &staging,
                &self.descriptor.install_plan,
                &resolved_profile,
                cancel_token,
                sink,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!(%e, "prepare_environment 失败，清理 staging");
                if let Some(s) = sink {
                    s.on_stage("failed");
                    s.on_log("error", &format!("prepare_environment 失败: {e}"));
                }
                let _ = std::fs::remove_dir_all(&staging);
                let _ = DeploymentStore::clear_journal(space);
                return Err(e);
            }
        };

        // ── 4. self-test（staging 内，候选环境验证）──
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = DeploymentStore::clear_journal(space);
            if let Some(s) = sink {
                s.on_stage("cancelled");
            }
            return Err(RuntimeError::OperationCancelled {
                message: format!("安装事务在 self-test 前被取消 (op={operation_id})"),
            });
        }

        if let Some(s) = sink {
            s.on_stage("verifying");
            s.on_log("info", "正在验证候选环境...");
        }

        if let Err(e) = self
            .provider
            .self_test(&staging, &self.descriptor.install_plan, cancel_token, sink)
            .await
        {
            tracing::warn!(%e, "候选环境 self-test 失败，清理 staging");
            if let Some(s) = sink {
                s.on_stage("failed");
                s.on_log("error", &format!("候选环境验证失败: {e}"));
            }
            let _ = std::fs::remove_dir_all(&staging);
            let _ = DeploymentStore::clear_journal(space);
            return Err(e);
        }

        if let Some(s) = sink {
            s.on_stage("promoting");
            s.on_log("info", "正在提升新环境...");
        }

        // ── 5. build manifest extension + manifest（先写进 staging） ──
        let extension = match self
            .provider
            .build_manifest_extension(&staging, &self.descriptor.install_plan)
        {
            Ok(ext) => ext,
            Err(e) => {
                tracing::warn!(%e, "build_manifest_extension 失败，清理 staging");
                let _ = std::fs::remove_dir_all(&staging);
                let _ = DeploymentStore::clear_journal(space);
                return Err(e);
            }
        };

        let artifact = prepare_result.artifact;
        let artifact_clone = artifact.clone();

        let manifest = DeploymentManifest {
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

        let staging_manifest = staging.join("manifest.json");
        runtime::atomic_write_json(&staging_manifest, &manifest).inspect_err(|_e| {
            let _ = std::fs::remove_dir_all(&staging);
            let _ = DeploymentStore::clear_journal(space);
        })?;

        // ── 6. promote: staging → slot-{candidate} ──
        match std::fs::rename(&staging, &candidate_dir) {
            Ok(()) => {
                tracing::info!(
                    engine = %engine_id,
                    install = %install_id,
                    slot = %candidate_slot,
                    "candidate slot 提升成功"
                );
            }
            Err(e) => {
                // rename 失败（可能被占用），尝试复制——大型目录用 spawn_blocking
                tracing::warn!(%e, "rename 失败，尝试复制 staging → candidate slot");
                let staging_for_copy = staging.clone();
                let candidate_for_copy = candidate_dir.clone();
                let copy_result = tokio::task::spawn_blocking(move || {
                    copy_dir_recursive(&staging_for_copy, &candidate_for_copy)
                })
                .await;
                let _ = std::fs::remove_dir_all(&staging);
                match copy_result {
                    Ok(Ok(())) => {}
                    Ok(Err(e2)) => {
                        let _ = DeploymentStore::clear_journal(space);
                        return Err(RuntimeError::GenerationPromoteFailed {
                            message: format!("提升 candidate slot 失败: rename={e}, copy={e2}"),
                        });
                    }
                    Err(e2) => {
                        let _ = DeploymentStore::clear_journal(space);
                        return Err(RuntimeError::GenerationPromoteFailed {
                            message: format!(
                                "提升 candidate slot 失败: rename={e}, spawn_blocking={e2}"
                            ),
                        });
                    }
                }
            }
        }

        // ── 7. pre-switch: journal 先推进到 Switched，再切换指针 ──
        if let Some(s) = sink {
            s.on_stage("switching");
            s.on_log("info", "正在切换到新环境...");
        }
        DeploymentStore::advance_phase(space, &mut journal, TransactionPhase::Switched)?;

        let pointer = DeploymentPointer {
            install_id: install_id.clone(),
            slot: candidate_slot.as_str().to_string(),
            updated_at_ms: runtime::now_ms(),
            schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
        };

        if let Err(e) = DeploymentStore::write_pointer(space, &pointer) {
            tracing::error!(%e, "deployment.json 原子写入失败，回滚");
            // 回滚：删除 candidate slot（占用记 residue），恢复由 journal 恢复逻辑兜底
            DeploymentStore::delete_slot_if_not_active(
                space,
                candidate_slot.as_str(),
                "指针切换失败，丢弃 candidate",
            )?;
            DeploymentStore::clear_journal(space)?;
            return Err(e);
        }

        // ── 8. 切换后验证：失败自动回滚 previous ──
        if let Some(s) = sink {
            s.on_stage("validating");
            s.on_log("info", "正在执行切换后验证...");
        }

        if let Err(e) = self
            .verify_after_switch(space, candidate_slot, &install_id, &artifact_clone)
            .await
        {
            tracing::error!(%e, "切换后验证失败，自动回滚到 previous 部署");
            if let Some(s) = sink {
                s.on_log("error", &format!("切换后验证失败，正在回滚: {e}"));
            }
            if let Err(rollback_err) = Self::rollback_switch(space, &journal, candidate_slot) {
                tracing::error!(%rollback_err, "回滚失败——启动恢复将按 journal fail-closed 处理");
                // 保留 journal 让下次启动恢复；返回原错误
                return Err(e);
            }
            return Err(e);
        }

        // ── 9. commit：journal → Committed，删除旧 slot，清 journal ──
        DeploymentStore::advance_phase(space, &mut journal, TransactionPhase::Committed)?;
        if let Some(prev) = &journal.previous {
            let deleted = DeploymentStore::delete_slot_if_not_active(
                space,
                &prev.slot,
                "更新成功，删除旧部署",
            )?;
            if !deleted {
                tracing::warn!(
                    engine = %engine_id,
                    slot = %prev.slot,
                    "旧 slot 被占用，已记 cleanup residue（非产品状态）"
                );
            }
        }
        DeploymentStore::clear_journal(space)?;

        tracing::info!(
            engine = %engine_id,
            install = %install_id,
            slot = %candidate_slot,
            fell_back = fell_back,
            "部署事务完成（切换后验证通过，旧 slot 已清理）"
        );

        if let Some(s) = sink {
            s.on_stage("completed");
            s.on_log("info", "安装完成");
        }

        Ok(InstallResult {
            install_id,
            operation_id: operation_id.to_string(),
            fell_back,
        })
    }

    /// 切换后验证失败时的回滚：指针切回 previous，candidate 记 residue。
    ///
    /// 成功后清除 journal（稳定状态只剩 active）。只作用于事务空间。
    fn rollback_switch(
        space: &DeploymentSpace,
        journal: &super::deployment::TransactionJournal,
        candidate_slot: DeploymentSlot,
    ) -> Result<(), RuntimeError> {
        match &journal.previous {
            Some(prev) => {
                let prev_pointer = DeploymentPointer {
                    install_id: prev.install_id.clone(),
                    slot: prev.slot.clone(),
                    updated_at_ms: runtime::now_ms(),
                    schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
                };
                DeploymentStore::write_pointer(space, &prev_pointer)?;
            }
            None => {
                DeploymentStore::remove_pointer(space)?;
            }
        }
        DeploymentStore::delete_slot_if_not_active(
            space,
            candidate_slot.as_str(),
            "切换后验证失败，丢弃 candidate",
        )?;
        DeploymentStore::clear_journal(space)?;
        Ok(())
    }

    /// 指针切换后的验证。
    ///
    /// - 读取刚写入的 manifest 确认完整性（schema + artifact identity）
    /// - 验证 artifact identity 一致性
    ///
    /// 完整 provider self-test 已在同一不可变 candidate slot 提升前执行。
    /// 提升是同卷原子 rename，不改变 payload 内容；切换后重复执行 torch/funasr
    /// import 只会把安装时间翻倍。运行时可用性由后续 start + token health 验证。
    ///
    /// 失败时调用方执行自动回滚。
    async fn verify_after_switch(
        &self,
        space: &DeploymentSpace,
        slot: DeploymentSlot,
        install_id: &str,
        expected_artifact: &runtime::ArtifactIdentity,
    ) -> Result<(), RuntimeError> {
        let manifest = DeploymentStore::read_slot_manifest(space, slot.as_str())?;

        if manifest.install_id != install_id {
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "manifest install_id 不匹配: expected={install_id}, actual={}",
                    manifest.install_id
                ),
            });
        }

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
            ManifestExtension::OnnxRuntime(ext) => ext.self_test_passed,
        };
        if !self_test_ok {
            return Err(RuntimeError::SelfTestFailed {
                message: "manifest extension 标记 self_test_passed=false".to_string(),
            });
        }

        tracing::info!(
            engine = %space.engine_id(),
            install = %install_id,
            sha256 = %manifest.artifact.sha256,
            "切换后 manifest 与 artifact identity 验证通过"
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

// ── Cleanup ───────────────────────────────────────────────────────────────

/// 执行清理操作。
///
/// 根据 `scope` 清理不同范围：
/// - `EngineDeploymentSlot`: 删除一个部署空间内非 active slot（占用记 residue）。
///   active 判定只看该空间自己的指针——删除保护按 implementation 生效。
/// - `EngineStaging`: 清扫引擎全部空间（engine 级 + implementation 级）的
///   孤儿 staging。
/// - `EngineModelCache`: 清理指定引擎模型目录中的孤儿残留（已安装模型
///   资产受保护，只删非托管子项；与 deployment 空间正交）。
/// - `ProviderSharedArtifact`: 清理共享 artifact（需各空间 active manifest 引用检查）。
/// - `ProviderDownloadCache`: 清理 provider 下载缓存。
pub fn execute_cleanup(scope: &CleanupScope) -> Result<(), RuntimeError> {
    match scope {
        CleanupScope::EngineDeploymentSlot { space, slot } => {
            runtime::validate_slot_name(slot)?;
            let active = DeploymentStore::read_pointer(space)?.map(|p| p.slot);
            if active.as_deref() == Some(slot.as_str()) {
                return Err(RuntimeError::CleanupFailed {
                    message: "active 部署不可删除".to_string(),
                });
            }
            DeploymentStore::delete_slot_if_not_active(space, slot, "cleanup 请求")?;
            Ok(())
        }
        CleanupScope::EngineStaging { engine_id } => {
            // 引擎全部空间的孤儿 staging（事务按 engine 串行，无并发事务）
            for space in DeploymentStore::spaces_for_engine(engine_id)? {
                DeploymentStore::sweep_orphan_staging(&space);
            }
            Ok(())
        }
        CleanupScope::EngineModelCache { engine_id } => {
            let cache_dir = runtime::engine_model_cache_dir(engine_id);
            // 校验基准是模型根——`models/` 与 `runtimes/engines/` 平级，
            // 对 engine_root 校验永远失败（0.22.x 该清理从未生效的原因）
            let models_root = runtime::models_root();
            runtime::ensure_path_within(&models_root, &cache_dir)?;
            if cache_dir.exists() {
                // `models/{engine}` 同时是已安装模型的资产根——只清理非托管
                // 子项（孤儿/安装残留），整删会把已安装模型一起删掉
                for entry in std::fs::read_dir(&cache_dir)? {
                    let path = entry?.path();
                    if path.is_dir()
                        && crate::infra::local_engine::model_storage::is_managed_asset_dir(&path)
                    {
                        continue;
                    }
                    runtime::ensure_path_within(&models_root, &path)?;
                    let result = if path.is_dir() {
                        std::fs::remove_dir_all(&path)
                    } else {
                        std::fs::remove_file(&path)
                    };
                    result.map_err(|e| RuntimeError::CleanupFailed {
                        message: format!("删除模型缓存残留失败: {e}"),
                    })?;
                    tracing::info!(
                        engine = %engine_id,
                        path = %path.display(),
                        "模型缓存残留已清理"
                    );
                }
            }
            Ok(())
        }
        CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id,
        } => cleanup_shared_artifact(*runtime_kind, artifact_id),
        CleanupScope::ProviderDownloadCache { runtime_kind } => match runtime_kind {
            RuntimePlan::PythonVenv => {
                let cache = runtime::uv_cache_dir();
                let runtimes_root = runtime::runtimes_root();
                runtime::ensure_path_within(&runtimes_root, &cache)?;
                if cache.exists() {
                    std::fs::remove_dir_all(&cache).map_err(|e| RuntimeError::CleanupFailed {
                        message: format!("删除 uv cache 失败: {e}"),
                    })?;
                    tracing::info!("uv 下载缓存已清理");
                }
                Ok(())
            }
            RuntimePlan::ManagedBinary => {
                // ManagedBinary download cache (future)
                Ok(())
            }
            RuntimePlan::OnnxRuntime => {
                // OnnxRuntime download cache (0.22.8 protocol seam)
                // ORT DLL 和模型按需下载，缓存清理由 B 包实现
                Ok(())
            }
        },
    }
}

/// 清理共享 artifact。
///
/// 引用真源是**当前有效 deployment manifest**（`scan_artifact_references`），
/// 不维护独立 refcount 数据。任何引擎的 active 部署引用此 artifact 时拒绝删除。
fn cleanup_shared_artifact(
    runtime_kind: RuntimePlan,
    artifact_id: &runtime::ArtifactId,
) -> Result<(), RuntimeError> {
    // 扫描所有引擎的 active 部署 manifest 确认无引用
    let refs = scan_artifact_references(runtime_kind, artifact_id)?;
    if !refs.is_empty() {
        return Err(RuntimeError::ArtifactStillReferenced {
            artifact_id: artifact_id.to_string(),
            ref_count: refs.len(),
        });
    }

    let dir = runtime::shared_artifact_dir(runtime_kind, artifact_id);

    let runtimes_root = runtime::runtimes_root();
    runtime::ensure_path_within(&runtimes_root, &dir)?;

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

#[cfg(test)]
mod tests;
