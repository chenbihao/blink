//! FunASR descriptor 装配（0.22.7.4 起：GGUF 常驻 worker 唯一实现）。
//!
//! domain 层 `EngineDefinition` 与 infra 层 `ProviderDescriptor`
//! （安装事务用）的编译期构造。旧 Python/PyTorch descriptor 已随
//! 0.22.7.4 切换删除——`make_funasr_*` 即 GGUF 实现。

use std::time::Duration;

use crate::domain::local_engine::{
    CapabilityKind, ComputeCandidate, EngineDefinition, EngineDisplay, EngineTimeouts,
    InstallPlanRef, LifecyclePolicy, ResourceBudget, ServiceTransport,
};
use crate::infra::local_engine::providers::{
    BinaryInstallPlan, CompatibilityCheck, InstallPlan, ProfileCandidate, ProviderDescriptor,
    debug_local_artifact_dir,
};
use crate::infra::local_engine::runtime::{
    ArtifactId, ChecksumSource, ComputeBackend, ComputePreference, EngineId, ModelContract,
    RuntimePlan,
};

use super::FUNASR_ENGINE_ID;

/// FunASR runtime 基础 artifact id（0.22.16.3 immutable runtime manifest）。
pub const FUNASR_GGUF_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-base";
/// FunASR Vulkan runtime artifact（0.22.16.3 immutable runtime manifest）。
pub const FUNASR_GGUF_VULKAN_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-vulkan";
/// SenseVoice CUDA runtime artifact（0.22.16.3 immutable runtime manifest）。
pub const FUNASR_GGUF_CUDA_ARTIFACT_ID: &str = "funasr-runtime-windows-x64-cuda";
const FUNASR_RUNTIME_LOCK: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/resources/stt/funasr-gguf/runtime-lock.json"
));

/// 从 immutable runtime lock 读取网络 artifact 来源。
///
/// 锁文件未填充真实 hash 时返回空值，让 provider 按设计 fail-closed；
/// 不在应用运行时猜测 URL、版本或 hash。
fn locked_runtime_artifact(artifact_id: &str) -> (String, String) {
    let Ok(lock) = serde_json::from_str::<serde_json::Value>(FUNASR_RUNTIME_LOCK) else {
        return (String::new(), String::new());
    };
    let Some(artifact) = lock
        .get("artifacts")
        .and_then(|value| value.get(artifact_id))
    else {
        return (String::new(), String::new());
    };
    (
        artifact
            .get("url")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
        artifact
            .get("sha256")
            .and_then(|value| value.as_str())
            .unwrap_or_default()
            .to_string(),
    )
}

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 FunASR GGUF 常驻 worker 的 `EngineDefinition`。
///
/// runtime = ManagedBinary（`cargo xtask funasr-worker` 从锁定源码构建、
/// 随发布捆绑的三个 exe）；service_transport = StdioWorker（NDJSON stdin/stdout）。
pub(super) fn make_funasr_descriptor() -> EngineDefinition {
    let artifact = ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap();
    let vulkan_artifact = ArtifactId::new(FUNASR_GGUF_VULKAN_ARTIFACT_ID).unwrap();
    let cuda_artifact = ArtifactId::new(FUNASR_GGUF_CUDA_ARTIFACT_ID).unwrap();

    EngineDefinition {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "FunASR 语音识别".to_string(),
            description: "本地 FunASR 语音转文字（llama.cpp/GGUF 常驻 worker）".to_string(),
            icon: "mic".to_string(),
            version: "0.22.7".to_string(),
        },
        capability_kind: CapabilityKind::Stt,
        // GGUF worker：无 HTTP 端口，stdin/stdout NDJSON 协议
        service_transport: ServiceTransport::StdioWorker,
        runtime_kind: RuntimePlan::ManagedBinary,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::ManagedBinary,
            artifact_ids: vec![
                artifact.clone(),
                vulkan_artifact.clone(),
                cuda_artifact.clone(),
            ],
            // SenseVoice 与 Paraformer 已通过 Vulkan 采用门；Nano 在采用门通过前
            // 继续 CPU-only。CUDA 当前仅 SenseVoice 开放。
            // worker 与 manifest 仍是实际可用性的权威来源。
            // 候选按模型声明，不能把某个模型的 backend 传播给同引擎其他模型。
            compute_candidates: super::gguf::gguf_model_specs()
                .iter()
                .flat_map(|spec| {
                    let cpu = ComputeCandidate {
                        model_id: spec.model_id.to_string(),
                        preference: ComputePreference::Cpu,
                        backend: ComputeBackend::Cpu,
                        profile_id: "cpu-x64".to_string(),
                        artifact_id: artifact.clone(),
                    };
                    let mut gpu = Vec::new();
                    if matches!(
                        spec.model_id,
                        super::gguf::GGUF_SENSEVOICE_ID | super::gguf::GGUF_PARAFORMER_ID
                    ) {
                        gpu.push(ComputeCandidate {
                            model_id: spec.model_id.to_string(),
                            preference: ComputePreference::Vulkan,
                            backend: ComputeBackend::Vulkan,
                            profile_id: "vulkan-x64".to_string(),
                            artifact_id: vulkan_artifact.clone(),
                        });
                    }
                    if spec.model_id == super::gguf::GGUF_SENSEVOICE_ID {
                        gpu.push(ComputeCandidate {
                            model_id: spec.model_id.to_string(),
                            preference: ComputePreference::Cuda,
                            backend: ComputeBackend::Cuda,
                            profile_id: "cuda12-x64".to_string(),
                            artifact_id: cuda_artifact.clone(),
                        });
                    }
                    gpu.into_iter().chain(std::iter::once(cpu))
                })
                .collect(),
            schema_version: 1,
        },
        // 默认契约：SenseVoice Q8（实际期望身份来自 model_storage manifest）
        model_contract: ModelContract {
            model_id: super::gguf::GGUF_SENSEVOICE_ID.to_string(),
            revision: super::gguf::GGUF_MODEL_REVISION.to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::Manual,
        timeouts: EngineTimeouts {
            // 模型常驻加载：Q8 GGUF ~254MB 读取 + 初始化 + 目录指纹哈希
            start_timeout: Duration::from_secs(20),
            model_load_timeout: Duration::from_secs(60),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            estimated_env_disk_mb: Some(8),     // 三个 worker exe ~7MB
            estimated_model_disk_mb: Some(243), // SenseVoice Q8 ~243MB
            estimated_stable_ram_mb: Some(280), // spike 实测常驻 ~251MiB + 音频
            estimated_peak_ram_mb: Some(512),
        },
    }
}

// ── ProviderDescriptor 构造 ──────────────────────────────────────────────────

/// 构造 FunASR 的 `ProviderDescriptor`（infra 安装事务用；GGUF 实现）。
pub fn make_funasr_provider_descriptor() -> ProviderDescriptor {
    let artifact = ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap();
    let vulkan_artifact = ArtifactId::new(FUNASR_GGUF_VULKAN_ARTIFACT_ID).unwrap();
    let cuda_artifact = ArtifactId::new(FUNASR_GGUF_CUDA_ARTIFACT_ID).unwrap();
    let (vulkan_url, vulkan_sha256) = locked_runtime_artifact(FUNASR_GGUF_VULKAN_ARTIFACT_ID);
    let (cuda_url, cuda_sha256) = locked_runtime_artifact(FUNASR_GGUF_CUDA_ARTIFACT_ID);
    let local_base_dir = debug_local_artifact_dir(FUNASR_GGUF_ARTIFACT_ID);
    let local_vulkan_dir = debug_local_artifact_dir(FUNASR_GGUF_VULKAN_ARTIFACT_ID);
    let local_cuda_dir = debug_local_artifact_dir(FUNASR_GGUF_CUDA_ARTIFACT_ID);

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimePlan::ManagedBinary,
        display_name: "FunASR 语音识别".to_string(),
        profiles: std::iter::once(ProfileCandidate {
            model_id: super::gguf::GGUF_SENSEVOICE_ID.to_string(),
            profile_id: "vulkan-x64".to_string(),
            backend: ComputeBackend::Vulkan,
            artifact_id: vulkan_artifact.clone(),
            compatibility: CompatibilityCheck::RequiresVulkan,
        })
        .chain(std::iter::once(ProfileCandidate {
            model_id: super::gguf::GGUF_SENSEVOICE_ID.to_string(),
            profile_id: "cuda12-x64".to_string(),
            backend: ComputeBackend::Cuda,
            artifact_id: cuda_artifact.clone(),
            compatibility: CompatibilityCheck::RequiresCuda {
                min_version: Some("12.0".to_string()),
            },
        }))
        .chain(std::iter::once(ProfileCandidate {
            model_id: super::gguf::GGUF_PARAFORMER_ID.to_string(),
            profile_id: "vulkan-x64".to_string(),
            backend: ComputeBackend::Vulkan,
            artifact_id: vulkan_artifact.clone(),
            compatibility: CompatibilityCheck::RequiresVulkan,
        }))
        .chain(std::iter::once(ProfileCandidate {
            model_id: super::gguf::GGUF_SENSEVOICE_ID.to_string(),
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }))
        .chain(
            super::gguf::gguf_model_specs()
                .iter()
                .skip(1)
                .map(|spec| ProfileCandidate {
                    model_id: spec.model_id.to_string(),
                    profile_id: "cpu-x64".to_string(),
                    backend: ComputeBackend::Cpu,
                    artifact_id: artifact.clone(),
                    compatibility: CompatibilityCheck::Always,
                }),
        )
        .collect(),
        model_contract: ModelContract {
            model_id: super::gguf::GGUF_SENSEVOICE_ID.to_string(),
            revision: super::gguf::GGUF_MODEL_REVISION.to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::ManagedBinary(BinaryInstallPlan {
            archive_artifact_id: artifact,
            // bundled 模式：文件来自随发布资源目录，hash 以同目录 manifest 为准
            archive_url: "bundled:bin/funasr-worker".to_string(),
            archive_sha256: String::new(),
            executable: "funasr-sensevoice-worker.exe".to_string(),
            model_executables: vec![
                (
                    super::gguf::GGUF_SENSEVOICE_ID.to_string(),
                    "funasr-sensevoice-worker.exe".to_string(),
                ),
                (
                    super::gguf::GGUF_PARAFORMER_ID.to_string(),
                    "funasr-paraformer-worker.exe".to_string(),
                ),
                (
                    super::gguf::GGUF_NANO_ID.to_string(),
                    "funasr-nano-worker.exe".to_string(),
                ),
            ],
            stdlib_artifact: None,
            required_cpu_features: Vec::new(),
            required_drivers: Vec::new(),
            self_test_command: vec![
                "funasr-sensevoice-worker.exe".to_string(),
                "--blink-backend-probe".to_string(),
            ],
            bundled_dir: local_base_dir
                .clone()
                .or_else(|| Some("bin/funasr-worker".to_string())),
            artifact_plans: vec![
                crate::infra::local_engine::providers::BinaryArtifactPlan {
                    artifact_id: ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap(),
                    archive_url: "bundled:bin/funasr-worker".to_string(),
                    archive_sha256: String::new(),
                    executable: "funasr-sensevoice-worker.exe".to_string(),
                    backend_dir: None,
                    dependencies: Vec::new(),
                    file_allowlist: Vec::new(),
                    bundled_dir: local_base_dir.or_else(|| Some("bin/funasr-worker".to_string())),
                },
                crate::infra::local_engine::providers::BinaryArtifactPlan {
                    artifact_id: vulkan_artifact,
                    archive_url: vulkan_url,
                    archive_sha256: vulkan_sha256,
                    executable: "funasr-sensevoice-worker.exe".to_string(),
                    backend_dir: None,
                    dependencies: vec![ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap()],
                    file_allowlist: Vec::new(),
                    bundled_dir: local_vulkan_dir,
                },
                crate::infra::local_engine::providers::BinaryArtifactPlan {
                    artifact_id: cuda_artifact,
                    archive_url: cuda_url,
                    archive_sha256: cuda_sha256,
                    executable: "funasr-sensevoice-worker.exe".to_string(),
                    backend_dir: None,
                    dependencies: vec![ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap()],
                    file_allowlist: Vec::new(),
                    bundled_dir: local_cuda_dir,
                },
            ],
        }),
    }
}
