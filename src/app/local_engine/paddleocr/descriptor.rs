//! PaddleOCR descriptor 装配：domain 层 `EngineDefinition` 与 infra 层
//! `ProviderDescriptor`（安装事务用）的编译期构造。

use std::time::Duration;

use crate::domain::local_engine::{
    CapabilityKind, ComputeCandidate, EngineDefinition, EngineDisplay, EngineTimeouts,
    InstallPlanRef, LifecyclePolicy, ResourceBudget, ServiceTransport,
};
use crate::domain::ocr::config::PaddleModel;
use crate::infra::local_engine::providers::python::PythonVenvProvider;
use crate::infra::local_engine::providers::{
    CompatibilityCheck, InstallPlan, PipExtraArg, ProfileCandidate, ProviderDescriptor,
    PythonInstallPlan,
};
use crate::infra::local_engine::runtime::{
    ArtifactId, ChecksumSource, ComputeBackend, ComputePreference, EngineId, ModelContract,
    RuntimePlan,
};

use super::PADDLEOCR_ENGINE_ID;
use super::locks::locked_packages;

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 PaddleOCR 编译期 descriptor。
///
/// descriptor 必须锁定现有 Python/package/profile/model contract。
/// 使用 0.22.2 `PythonVenvProvider`；不新造第二套安装器。
pub(super) fn make_paddleocr_descriptor() -> EngineDefinition {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    EngineDefinition {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "PP-OCRv6 文字识别".to_string(),
            description: "本地 PaddleOCR PP-OCRv6 文字识别（Python/PaddlePaddle）".to_string(),
            icon: "scan-text".to_string(),
            version: "0.22.4".to_string(),
        },
        capability_kind: CapabilityKind::Ocr,
        service_transport: ServiceTransport::Http,
        runtime_kind: RuntimePlan::PythonVenv,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::PythonVenv,
            artifact_ids: vec![python_artifact.clone()],
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: python_artifact.clone(),
            }],
            schema_version: 1,
        },
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::OnDemand,
        // PaddleOCR tiny 冷启动 ~2.4s + 模型加载，start timeout 设为 30s
        // 模型首次下载可能需要更长时间
        timeouts: EngineTimeouts {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(120),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            // spike 实测 venv 785.9MB + 模型 169.1MB ≈ 955MB，向上取整
            estimated_env_disk_mb: Some(960),
            // tiny det + rec models ~10MB（169.1MB 含三档共享，tiny 单独约 10MB）
            estimated_model_disk_mb: Some(10),
            // spike 实测稳定工作集 ~408MB
            estimated_stable_ram_mb: Some(410),
            // spike 实测峰值工作集 ~1136MB（接近但未超 1.2GB 门）
            estimated_peak_ram_mb: Some(1140),
        },
    }
}

// ── ProviderDescriptor 构造 ──────────────────────────────────────────────────

/// 构造 PaddleOCR 的 `ProviderDescriptor`（infra 层安装事务用）。
///
/// 与 `make_paddleocr_descriptor()`（domain 层 `EngineDefinition`）互补。
///
/// **包列表来源**：`resources/ocr/paddleocr/locked-requirements.txt`（唯一锁源）。
/// 以 `include_str!` 嵌入，运行时解析生成 `PackageLock` 列表。
/// 不再手写第二份包清单——避免 lock.json 与 Rust descriptor 漂移。
///
/// **安装策略**：`--require-hashes --no-deps`——强制 hash 校验 + 禁止传递依赖
/// 自动解析，确保安装的 wheel 与锁文件完全一致。
pub fn make_paddleocr_provider_descriptor() -> ProviderDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    ProviderDescriptor {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimePlan::PythonVenv,
        display_name: "PP-OCRv6 文字识别".to_string(),
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: python_artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny".to_string(),
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
            extra_pip_args: vec![PipExtraArg::NoDeps],
            self_test_script:
                "import paddle; import paddleocr; import fastapi; import uvicorn; paddle.utils.run_check()".to_string(),
        }),
    }
}

/// 创建 PaddleOCR 的 `PythonVenvProvider` 实例。
///
/// `EngineManager` 持有此实例，在 `install` 时传给 `InstallTransaction`。
pub fn make_paddleocr_python_provider() -> PythonVenvProvider {
    PythonVenvProvider::new()
}
