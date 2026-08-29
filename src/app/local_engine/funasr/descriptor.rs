//! FunASR descriptor 装配：domain 层 `EngineDefinition` 与 infra 层
//! `ProviderDescriptor`（安装事务用）的编译期构造。

use std::time::Duration;

use crate::domain::local_engine::{
    CapabilityKind, ComputeCandidate, EngineDefinition, EngineDisplay, EngineTimeouts,
    InstallPlanRef, LifecyclePolicy, ResourceBudget,
};
use crate::infra::local_engine::providers::{
    CompatibilityCheck, InstallPlan, PipExtraArg, ProfileCandidate, ProviderDescriptor,
    PythonInstallPlan,
};
use crate::infra::local_engine::runtime::{
    ArtifactId, ChecksumSource, ComputeBackend, ComputePreference, EngineId, ModelContract,
    RuntimePlan,
};

use super::FUNASR_ENGINE_ID;
use super::locks::locked_packages;

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 FunASR 编译期 descriptor。
///
/// descriptor 必须锁定现有 Python/package/profile/model contract。
/// 使用 0.22.2 `PythonVenvProvider`；不新造第二套安装器。
pub(super) fn make_funasr_descriptor() -> EngineDefinition {
    // Python distribution artifact（引用 provider 管理的锁定标识）
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    EngineDefinition {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "FunASR 语音识别".to_string(),
            description: "本地 FunASR 语音转文字（Python/PyTorch）".to_string(),
            icon: "mic".to_string(),
            version: "0.10.4".to_string(),
        },
        capability_kind: CapabilityKind::Stt,
        runtime_kind: RuntimePlan::PythonVenv,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::PythonVenv,
            artifact_ids: vec![python_artifact.clone()],
            // 0.22.6：只声明 CPU profile。锁文件仅包含 CPU-only PyTorch wheel hash，
            // 声明 CUDA profile 会导致安装时 hash mismatch。CUDA 支持需独立锁文件后
            // 再启用。
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: python_artifact.clone(),
            }],
            schema_version: 1,
        },
        // 模型契约：FunASR 模型由 ModelScope 下载，上游不提供稳定 checksum
        model_contract: ModelContract {
            model_id: "iic/SenseVoiceSmall".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::Manual,
        // FunASR 首次启动需要下载模型（~234MB），超时设为 300s
        timeouts: EngineTimeouts {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(300),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            estimated_env_disk_mb: Some(3000),  // venv + torch + funasr ~3GB
            estimated_model_disk_mb: Some(234), // SenseVoiceSmall ~234MB
            estimated_stable_ram_mb: Some(500),
            estimated_peak_ram_mb: Some(1500),
        },
    }
}

// ── ProviderDescriptor 构造 ──────────────────────────────────────────────────

/// 构造 FunASR 的 `ProviderDescriptor`（infra 层安装事务用）。
///
/// 与 `make_funasr_descriptor()`（domain 层 `EngineDefinition`）互补。
///
/// **包列表来源**：`resources/stt/funasr/locked-requirements.txt`（唯一锁源）。
/// 以 `include_str!` 嵌入，运行时解析生成 `PackageLock` 列表。
/// 不再手写第二份包清单——避免 lock.json 与 Rust descriptor 漂移。
///
/// **安装策略**：`--require-hashes --no-deps`——强制 hash 校验 + 禁止传递依赖
/// 自动解析，确保安装的 wheel 与锁文件完全一致。
///
/// **PyTorch index**：torch/torchaudio 来自 `https://download.pytorch.org/whl/cpu`，
/// 其余包来自 PyPI。锁文件已通过 `--index-url` + `--extra-index-url` 生成，
/// 包含两个 index 的 wheel hash。安装时通过 `ExtraIndexUrl` 传入 PyTorch index，
/// 并以 `unsafe-best-match` 允许 uv 为锁定版本跨索引查找候选；精确版本、
/// `--require-hashes` 与 `--no-deps` 继续约束最终安装内容。
pub fn make_funasr_provider_descriptor() -> ProviderDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimePlan::PythonVenv,
        display_name: "FunASR 语音识别".to_string(),
        // 0.22.6：只声明 CPU profile。CUDA profile 需独立 CUDA 锁文件后启用。
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: python_artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id: "iic/SenseVoiceSmall".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: python_artifact,
            // 唯一锁源：从嵌入的 locked-requirements.txt 解析
            packages: locked_packages(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            // --no-deps：禁止传递依赖自动解析，全部由锁文件覆盖
            // ExtraIndexUrl：PyTorch CPU index，用于 torch/torchaudio wheel
            extra_pip_args: vec![
                PipExtraArg::NoDeps,
                PipExtraArg::ExtraIndexUrl("https://download.pytorch.org/whl/cpu".to_string()),
                PipExtraArg::IndexStrategyUnsafeBestMatch,
            ],
            self_test_script: "import funasr; import torch; import fastapi; import uvicorn"
                .to_string(),
        }),
    }
}
