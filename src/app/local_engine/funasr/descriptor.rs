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
};
use crate::infra::local_engine::runtime::{
    ArtifactId, ChecksumSource, ComputeBackend, ComputePreference, EngineId, ModelContract,
    RuntimePlan,
};

use super::FUNASR_ENGINE_ID;

/// GGUF worker runtime artifact id（绑定 runtime-llamacpp-v0.2.6 源码 pin）。
pub const FUNASR_GGUF_ARTIFACT_ID: &str = "funasr-gguf-worker-v0.2.6";

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 FunASR GGUF 常驻 worker 的 `EngineDefinition`。
///
/// runtime = ManagedBinary（`cargo xtask funasr-worker` 从锁定源码构建、
/// 随发布捆绑的三个 exe）；service_transport = StdioWorker（NDJSON stdin/stdout）。
pub(super) fn make_funasr_descriptor() -> EngineDefinition {
    let artifact = ArtifactId::new(FUNASR_GGUF_ARTIFACT_ID).unwrap();

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
            artifact_ids: vec![artifact.clone()],
            // 首版 CPU 闭环（phase §5.8.5：GPU 未实测不开）
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: artifact.clone(),
            }],
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

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimePlan::ManagedBinary,
        display_name: "FunASR 语音识别".to_string(),
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
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
            stdlib_artifact: None,
            required_cpu_features: Vec::new(),
            required_drivers: Vec::new(),
            self_test_command: vec![
                "funasr-sensevoice-worker.exe".to_string(),
                "--blink-selftest".to_string(),
            ],
            bundled_dir: Some("bin/funasr-worker".to_string()),
        }),
    }
}
