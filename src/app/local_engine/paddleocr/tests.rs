//! PaddleOCR adapter 回归测试（0.22.10：Python HTTP 链路测试已随栈退役删除）。

use super::*;
use crate::domain::local_engine::{CapabilityKind, LifecyclePolicy};
use crate::infra::local_engine::runtime::ComputePreference;

#[test]
fn descriptor_has_correct_engine_id() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().engine_id.as_str(), PADDLEOCR_ENGINE_ID);
}

#[test]
fn descriptor_has_ocr_capability() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Ocr);
}

#[test]
fn descriptor_has_on_demand_lifecycle() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().lifecycle, LifecyclePolicy::OnDemand);
}

#[test]
fn paddleocr_models_are_deployment_managed_not_model_storage() {
    let adapter = PaddleocrAdapter::new();
    // ONNX 模型资产由 deployment 事务管理（asset-lock + model generation），
    // 不经 FunASR 式 model_storage active pointer。
    assert!(!adapter.uses_managed_model_storage());
}

#[test]
fn descriptor_has_cpu_profile_only() {
    let adapter = PaddleocrAdapter::new();
    let candidates = &adapter.descriptor().install_plan.compute_candidates;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].preference, ComputePreference::Cpu);
}

#[test]
fn descriptor_model_identity_matches() {
    let adapter = PaddleocrAdapter::new();
    let descriptor = adapter.descriptor();
    let (det_model, rec_model) =
        crate::domain::ocr::config::PaddleModel::Tiny.official_model_names();
    let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
    assert_eq!(descriptor.model_contract.model_id, expected_model_id);
    assert_eq!(descriptor.model_contract.revision, "ppocrv6-tiny-onnx");
}

/// 验证 ONNX provider descriptor 的 model identity 一致。
#[test]
fn provider_descriptor_model_identity_matches() {
    let pd = make_paddleocr_onnx_provider_descriptor();
    let (det_model, rec_model) =
        crate::domain::ocr::config::PaddleModel::Tiny.official_model_names();
    let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
    assert_eq!(pd.model_contract.model_id, expected_model_id);
    assert_eq!(pd.model_contract.revision, "ppocrv6-tiny-onnx");
}

/// model_revision 不应使用 cache_files:N 格式
#[test]
fn model_revision_not_cache_files_format() {
    let adapter = PaddleocrAdapter::new();
    let revision = &adapter.descriptor().model_contract.revision;
    assert!(
        !revision.starts_with("cache_files:"),
        "model_revision 不应使用 cache_files:N 格式，实际: {}",
        revision
    );
    assert_eq!(revision, "ppocrv6-tiny-onnx");
}

#[test]
fn engine_config_from_ocr_config_defaults_to_tiny() {
    let cfg = PaddleOcrEngineConfig::from_ocr_config();
    assert_eq!(cfg.model, "tiny");
}

/// 0.22.10：paddleocr 为 ONNX in-process 引擎，prepare_launch 必须 fail-closed，
/// 不得假装可以启动子进程。
#[test]
fn prepare_launch_is_fail_closed_for_inprocess_engine() {
    use crate::domain::local_engine::{ArtifactId, ComputeBackend, ResolvedProfile};
    use crate::infra::local_engine::port::Endpoint;

    let adapter = PaddleocrAdapter::new();
    let ctx = LaunchContext {
        endpoint: Endpoint::new(0),
        engine_id: PADDLEOCR_ENGINE_ID.to_string(),
        instance_id: "test-instance".to_string(),
        token: "test-token".to_string(),
        resolved_profile: ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("test").unwrap(),
            priority: 0,
        },
    };
    let config = AdapterConfig::new();
    let result = adapter.prepare_launch(&ctx, &config);
    assert!(result.is_err(), "in-process 引擎不应产生 LaunchDescriptor");
    let err = result.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
}

/// 0.22.10：无子进程 health 协议，map_health 返回 Unknown（不伪造健康状态）。
#[test]
fn map_health_returns_unknown_for_inprocess_engine() {
    let adapter = PaddleocrAdapter::new();
    let mapping = adapter.map_health(&serde_json::json!({}));
    assert_eq!(
        mapping.service,
        crate::domain::local_engine::ServiceHealth::Unknown
    );
    assert_eq!(
        mapping.model,
        crate::domain::local_engine::ModelHealth::Unknown
    );
}
