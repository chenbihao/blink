//! PaddleOCR descriptor 装配：domain 层 `EngineDefinition` 与 infra 层
//! `ProviderDescriptor`（安装事务用）的编译期构造。
//!
//! 0.22.10：Python venv descriptor 与 `PythonVenvProvider` 已随 Python/uv
//! 栈退役删除；稳定 `paddleocr` engine id 下的唯一实现是 ONNX in-process。

use std::time::Duration;

use crate::domain::local_engine::{
    CapabilityKind, ComputeCandidate, EngineDefinition, EngineDisplay, EngineTimeouts,
    InstallPlanRef, LifecyclePolicy, ResourceBudget, ServiceTransport,
};
use crate::domain::ocr::config::PaddleModel;
use crate::infra::local_engine::providers::{
    CompatibilityCheck, InstallPlan, OnnxInstallPlan, ProfileCandidate, ProviderDescriptor,
};
use crate::infra::local_engine::runtime::{
    ChecksumSource, ComputeBackend, ComputePreference, EngineId, ModelContract, RuntimePlan,
};

use super::PADDLEOCR_ENGINE_ID;

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 PaddleOCR 编译期 descriptor（0.22.8：已切换到 ONNX Runtime）。
///
/// 稳定 `paddleocr` engine id 不变；runtime 从 `PythonVenv` 切到 `OnnxRuntime`。
/// OCR 使用 in-process lazy Session，不 spawn 子进程；
/// `service_transport` 保持 `Http` 仅为 legacy 兼容读取（旧 Python manifest
/// 仍可安全反序列化），新 deployment 不再启动 HTTP server。
pub(super) fn make_paddleocr_descriptor() -> EngineDefinition {
    let dll_artifact_id = crate::infra::local_engine::asset_lock::ort_dll_artifact_id()
        .expect("asset-lock.json 必须可解析且 ORT artifact id 构造成功");

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    EngineDefinition {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "PP-OCRv6 文字识别".to_string(),
            description: "本地 PP-OCRv6 文字识别（ONNX Runtime）".to_string(),
            icon: "scan-text".to_string(),
            version: "0.22.8".to_string(),
        },
        capability_kind: CapabilityKind::Ocr,
        // 0.22.8: ONNX in-process，不启动 HTTP server。
        // 保留 Http 仅为 legacy manifest 兼容反序列化。
        service_transport: ServiceTransport::Http,
        runtime_kind: RuntimePlan::OnnxRuntime,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimePlan::OnnxRuntime,
            artifact_ids: vec![dll_artifact_id.clone()],
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: dll_artifact_id.clone(),
            }],
            schema_version: 1,
        },
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny-onnx".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::OnDemand,
        // ONNX Session 构建（DLL load + det/rec model load）通常 < 5s
        timeouts: EngineTimeouts {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(120),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            // ORT DLL ~11MB + det/rec/dict 模型 ~6MB
            estimated_env_disk_mb: Some(20),
            estimated_model_disk_mb: Some(6),
            // ONNX in-process，无子进程常驻开销
            estimated_stable_ram_mb: Some(100),
            estimated_peak_ram_mb: Some(200),
        },
    }
}

// ── ONNX Runtime ProviderDescriptor（0.22.8-B）──────────────────────────────

/// 构造 PaddleOCR 的 ONNX Runtime `ProviderDescriptor`（0.22.8-B）。
///
/// - 使用 `RuntimePlan::OnnxRuntime`，由 `OnnxRuntimeProvider` 消费
/// - ORT DLL artifact id 从 `asset-lock.json` 编译期嵌入解析
/// - 模型 contract 使用 PP-OCRv6 tiny 的固定 revision
/// - CPU-only profile（`CompatibilityCheck::Always`）
///
/// **设计铁则**：
/// - 不新增面向用户的 `paddleocr-onnx` engine id——稳定 id 不变
/// - 旧 Python manifest 保留为 legacy 读取，不参与新运行时 fallback
/// - asset lock 的 URL/SHA-256/size 是唯一锁源，descriptor 只引用 artifact id
pub fn make_paddleocr_onnx_provider_descriptor() -> ProviderDescriptor {
    let dll_artifact_id = crate::infra::local_engine::asset_lock::ort_dll_artifact_id()
        .expect("asset-lock.json 必须可解析且 ORT artifact id 构造成功");

    let lock = crate::infra::local_engine::asset_lock::parse_asset_lock()
        .expect("asset-lock.json 必须可解析");

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    ProviderDescriptor {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimePlan::OnnxRuntime,
        display_name: "PP-OCRv6 文字识别 (ONNX)".to_string(),
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: dll_artifact_id.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny-onnx".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::OnnxRuntime(OnnxInstallPlan {
            dll_artifact_id,
            ort_version: lock.ort.version,
            dll_url: lock.ort.url,
            // DLL SHA-256 从 asset lock 获取（onnxruntime.dll 的 hash）
            dll_sha256: lock
                .ort
                .files
                .iter()
                .find(|f| f.path.ends_with("onnxruntime.dll"))
                .map(|f| f.sha256.clone())
                .unwrap_or_default(),
            inter_op: 1,
            intra_op: 4,
            execution_provider: "cpu".to_string(),
        }),
    }
}
