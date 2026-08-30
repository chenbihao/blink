//! FunASR adapter 回归测试（0.22.7.4 起：GGUF 常驻 worker 唯一实现）。
//!
//! 旧 Python/PyTorch 链路的 venv、依赖锁、嵌入脚本、HTTP 端点与子模型
//! 测试已随 0.22.7.4 切换删除；本文件只保留对新实现仍成立的契约。

use super::*;
use crate::domain::local_engine::{CapabilityKind, LifecyclePolicy, ModelHealth, ServiceHealth};
use crate::infra::local_engine::providers::InstallPlan;
use crate::infra::local_engine::runtime::{
    ArtifactId, ComputeBackend, ComputePreference, ResolvedProfile, RuntimePlan,
};

// ── descriptor 稳定 id 和闭合 profile ──

#[test]
fn descriptor_has_stable_engine_id() {
    let adapter = FunasrAdapter::new();
    assert_eq!(adapter.descriptor().engine_id.as_str(), FUNASR_ENGINE_ID);
}

#[test]
fn descriptor_has_closed_capability_kind() {
    let adapter = FunasrAdapter::new();
    assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Stt);
}

#[test]
fn descriptor_has_manual_lifecycle() {
    let adapter = FunasrAdapter::new();
    assert_eq!(adapter.descriptor().lifecycle, LifecyclePolicy::Manual);
}

#[test]
fn descriptor_validates_ok() {
    let adapter = FunasrAdapter::new();
    assert!(adapter.descriptor().validate().is_ok());
}

#[test]
fn descriptor_declares_cpu_preference_only() {
    // 首版 CPU 闭环（phase §5.8.5：GPU 未实测不开）
    let adapter = FunasrAdapter::new();
    let desc = adapter.descriptor();
    assert!(desc.has_preference(ComputePreference::Cpu));
    assert!(
        !desc.has_preference(ComputePreference::Cuda),
        "未实测的 CUDA preference 不应声明"
    );
}

#[test]
fn descriptor_allows_cpu_profile() {
    let adapter = FunasrAdapter::new();
    let profile = ResolvedProfile {
        profile_id: "cpu-x64".to_string(),
        backend: ComputeBackend::Cpu,
        artifact_id: ArtifactId::new("funasr-gguf-worker-v0.2.6").unwrap(),
        priority: 0,
    };
    assert!(adapter.descriptor().is_profile_allowed(&profile));
}

#[test]
fn descriptor_rejects_undeclared_profile() {
    let adapter = FunasrAdapter::new();
    let profile = ResolvedProfile {
        profile_id: "vulkan-x64".to_string(),
        backend: ComputeBackend::Vulkan,
        artifact_id: ArtifactId::new("funasr-gguf-worker-v0.2.6").unwrap(),
        priority: 0,
    };
    assert!(!adapter.descriptor().is_profile_allowed(&profile));
}

/// descriptor 默认 model_contract 与 GGUF 目录对齐（SenseVoice Q8）。
#[test]
fn descriptor_model_contract_matches_gguf_catalog() {
    let adapter = FunasrAdapter::new();
    let contract = &adapter.descriptor().model_contract;
    assert_eq!(contract.model_id, gguf::GGUF_SENSEVOICE_ID);
    assert_eq!(contract.revision, gguf::GGUF_MODEL_REVISION);
}

// ── 旧 SttConfig 反序列化收口：旧模型 id 归一化为 GGUF id ──

/// 旧 SenseVoice 选择（完整 ModelScope id / 各短名）→ SenseVoice GGUF。
#[test]
fn old_sensevoice_config_deserializes_to_gguf_id() {
    for legacy in [
        "iic/SenseVoiceSmall",
        "sensevoice",
        "SenseVoice",
        "SenseVoiceSmall",
    ] {
        let json = format!(r#"{{"funasr_model": "{legacy}"}}"#);
        let local: crate::domain::config::stt_config::LocalEngineConfig =
            serde_json::from_str(&json).unwrap();
        assert_eq!(
            local.funasr_model,
            crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID,
            "旧 id {legacy} 应归一化为 SenseVoice GGUF"
        );
    }
}

/// 旧 Paraformer 选择（短名 / 完整 ModelScope id / 历史错误 id）→ Paraformer GGUF。
#[test]
fn old_paraformer_config_deserializes_to_gguf_id() {
    for legacy in [
        "paraformer-zh",
        "iic/speech_seaco_paraformer_large_asr_nat-zh-cn-16k-common-vocab8404-pytorch",
        "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404",
    ] {
        let json = format!(r#"{{"funasr_model": "{legacy}"}}"#);
        let local: crate::domain::config::stt_config::LocalEngineConfig =
            serde_json::from_str(&json).unwrap();
        assert_eq!(
            local.funasr_model,
            crate::domain::config::stt_config::GGUF_PARAFORMER_MODEL_ID,
            "旧 id {legacy} 应归一化为 Paraformer GGUF"
        );
    }
}

/// 旧真流式模型 id（已废弃）→ 默认 SenseVoice GGUF。
#[test]
fn old_streaming_model_id_normalizes_to_gguf_default() {
    for legacy in [
        "paraformer-zh-streaming",
        "iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online",
    ] {
        let json = format!(r#"{{"funasr_model": "{legacy}"}}"#);
        let local: crate::domain::config::stt_config::LocalEngineConfig =
            serde_json::from_str(&json).unwrap();
        assert_eq!(
            local.funasr_model,
            crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID
        );
    }
}

/// 默认模型即 SenseVoice GGUF（0.22.7.4 起）。
#[test]
fn default_config_uses_gguf_sensevoice() {
    let local = crate::domain::config::stt_config::LocalEngineConfig::default();
    assert_eq!(
        local.funasr_model,
        crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID
    );
}

/// 完整旧配置（含 VAD/设备字段）反序列化后其余字段保持不变。
#[test]
fn old_stt_config_fields_deserialization_unchanged() {
    let json = r#"{
        "server_port": 9000,
        "funasr_model": "iic/SenseVoiceSmall",
        "device": "cpu",
        "use_itn": true,
        "auto_start_server": false,
        "vad": {
            "silence_threshold": 0.005,
            "min_silence_ms": 300,
            "min_sentence_ms": 800
        }
    }"#;
    let local: crate::domain::config::stt_config::LocalEngineConfig =
        serde_json::from_str(json).unwrap();
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(
        funasr_config.funasr_model,
        crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID
    );
    assert_eq!(funasr_config.device, "cpu");
    assert!(funasr_config.use_itn);
    assert!(!funasr_config.auto_start_server);
    assert_eq!(funasr_config.vad.silence_threshold, 0.005);
    assert_eq!(funasr_config.vad.min_silence_ms, 300);
    assert_eq!(funasr_config.vad.min_sentence_ms, 800);
}

// ── hotwords/ITN/VAD/model 参数映射不变 ──

#[test]
fn funasr_engine_config_preserves_hotwords() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        hotwords: Some("美团 100, 快手 80".to_string()),
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.hotwords.as_deref(), Some("美团 100, 快手 80"));
}

#[test]
fn funasr_engine_config_preserves_itn() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        use_itn: false,
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert!(!funasr_config.use_itn);
}

#[test]
fn funasr_engine_config_preserves_vad() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        vad: crate::domain::config::stt_config::VadConfig {
            silence_threshold: 0.003,
            min_silence_ms: 200,
            min_sentence_ms: 600,
        },
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.vad.silence_threshold, 0.003);
    assert_eq!(funasr_config.vad.min_silence_ms, 200);
    assert_eq!(funasr_config.vad.min_sentence_ms, 600);
}

#[test]
fn funasr_engine_config_preserves_model() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        funasr_model: crate::domain::config::stt_config::GGUF_PARAFORMER_MODEL_ID.to_string(),
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(
        funasr_config.funasr_model,
        crate::domain::config::stt_config::GGUF_PARAFORMER_MODEL_ID
    );
}

#[test]
fn funasr_engine_config_preserves_device() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        device: "cuda".to_string(),
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.device, "cuda");
}

#[test]
fn funasr_engine_config_preserves_auto_start() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        auto_start_server: true,
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert!(funasr_config.auto_start_server);
}

#[test]
fn funasr_engine_config_round_trip_json() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        server_port: 9000,
        funasr_model: crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID.to_string(),
        device: "cuda".to_string(),
        num_threads: Some(4),
        auto_start_server: true,
        hotwords: Some("美团 100".to_string()),
        use_itn: false,
        ..Default::default()
    };
    let config = FunasrEngineConfig::from_stt_config(&local);
    let json = serde_json::to_string(&config).unwrap();
    let back: FunasrEngineConfig = serde_json::from_str(&json).unwrap();
    assert_eq!(
        back.funasr_model,
        crate::domain::config::stt_config::GGUF_SENSEVOICE_MODEL_ID
    );
    assert_eq!(back.device, "cuda");
    assert!(!back.use_itn);
    assert!(back.auto_start_server);
}

// ── health model Loading/Ready/Error 映射 ──

#[test]
fn health_maps_model_ready() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
        "model_loaded": true,
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Ready);
}

#[test]
fn health_maps_stdio_worker_ready_as_healthy() {
    // GGUF worker 的 NDJSON ready 协议不携带 HTTP health 的 status=ok。
    let raw = serde_json::json!({
        "type": "ready",
        "engine_id": "funasr",
        "instance_id": "inst-test",
        "model_status": "ready",
        "model_id": "gguf/paraformer-zh-q8",
        "model_revision": "gguf-v0.2.6",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Ready);
}

#[test]
fn health_maps_model_loading() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "loading",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Loading);
}

#[test]
fn health_maps_model_downloading() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "downloading",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Downloading);
}

#[test]
fn health_maps_model_error() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "error",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Failed);
}

#[test]
fn health_maps_model_idle() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "idle",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::NotLoaded);
}

#[test]
fn health_maps_service_unreachable() {
    let raw = serde_json::json!({});
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Unreachable);
}

#[test]
fn health_falls_back_to_model_loaded_bool() {
    // 旧版 ready JSON 没有 model_status 字段
    let raw = serde_json::json!({
        "status": "ok",
        "model_loaded": true,
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Ready);
}

#[test]
fn health_falls_back_to_loading_when_model_not_loaded() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_loaded": false,
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Loading);
}

#[test]
fn health_maps_backend_observation() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
        "backend": "cpu",
        "device_name": "Intel i7",
    });
    let mapping = map_funasr_health(&raw);
    assert!(mapping.backend.is_some());
    let backend = mapping.backend.unwrap();
    assert_eq!(backend.actual_backend, ComputeBackend::Cpu);
    assert_eq!(backend.device_name, "Intel i7");
}

/// requested/actual 语义：模型 Loading/Idle 时不得把请求设备冒充 actual backend。
#[test]
fn health_loading_has_no_backend_observation() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "loading",
        "requested_backend": "cpu",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Loading);
    assert!(
        mapping.backend.is_none(),
        "未建立实际执行后端时不得把请求设备冒充 actual backend"
    );
}

#[test]
fn health_maps_cuda_backend() {
    let raw = serde_json::json!({
        "status": "ok",
        "backend": "cuda",
        "device_name": "RTX 4060",
    });
    let mapping = map_funasr_health(&raw);
    let backend = mapping.backend.unwrap();
    assert_eq!(backend.actual_backend, ComputeBackend::Cuda);
}

#[test]
fn health_maps_model_id_and_revision() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
        "model_id": gguf::GGUF_SENSEVOICE_ID,
        "model_revision": gguf::GGUF_MODEL_REVISION,
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model_id, Some(gguf::GGUF_SENSEVOICE_ID.to_string()));
    assert_eq!(
        mapping.model_revision,
        Some(gguf::GGUF_MODEL_REVISION.to_string())
    );
}

// ── health engine/instance/token 不匹配失败 ──
// 这些测试验证 ready JSON 缺少身份字段时的行为。
// 完整的身份校验由 EngineManager 在调用 map_health 后，
// 使用 ServiceIdentityInput::verify 核对 engine id、instance id 和 token。

#[test]
fn health_without_identity_fields_still_maps_model_status() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
    });
    let mapping = map_funasr_health(&raw);
    // service 标记为 Healthy，但 EngineManager 会在后续身份校验中降级
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Ready);
}

#[test]
fn health_with_mismatched_engine_id_does_not_verify() {
    // 验证 ServiceIdentityInput 的身份校验逻辑
    use crate::infra::local_engine::port::{Endpoint, ServiceIdentityInput, ServiceIdentityResult};

    let input = ServiceIdentityInput {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        token: "secret-token-xyz".to_string(),
        endpoint: Endpoint::new(8000),
    };

    // 回显了错误的 engine_id
    let observed = ServiceIdentityResult {
        engine_id: Some("wrong-engine".to_string()),
        instance_id: Some("inst-abc".to_string()),
        token_fingerprint: Some(input.token_fingerprint()),
        endpoint: Some("127.0.0.1:8000".to_string()),
    };

    let result = input.verify(&observed);
    assert!(matches!(
        result,
        crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
    ));
}

#[test]
fn health_with_mismatched_instance_id_does_not_verify() {
    use crate::infra::local_engine::port::{Endpoint, ServiceIdentityInput, ServiceIdentityResult};

    let input = ServiceIdentityInput {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        token: "secret-token-xyz".to_string(),
        endpoint: Endpoint::new(8000),
    };

    let observed = ServiceIdentityResult {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("wrong-instance".to_string()),
        token_fingerprint: Some(input.token_fingerprint()),
        endpoint: Some("127.0.0.1:8000".to_string()),
    };

    let result = input.verify(&observed);
    assert!(matches!(
        result,
        crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
    ));
}

#[test]
fn health_with_mismatched_token_does_not_verify() {
    use crate::infra::local_engine::port::{Endpoint, ServiceIdentityInput, ServiceIdentityResult};

    let input = ServiceIdentityInput {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        token: "secret-token-xyz".to_string(),
        endpoint: Endpoint::new(8000),
    };

    let observed = ServiceIdentityResult {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-abc".to_string()),
        token_fingerprint: Some("00000000".to_string()),
        endpoint: Some("127.0.0.1:8000".to_string()),
    };

    let result = input.verify(&observed);
    assert!(matches!(
        result,
        crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
    ));
}

#[test]
fn health_with_all_fields_matching_verifies() {
    use crate::infra::local_engine::port::{
        Endpoint, IdentityVerification, ServiceIdentityInput, ServiceIdentityResult,
    };

    let input = ServiceIdentityInput {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        token: "secret-token-xyz".to_string(),
        endpoint: Endpoint::new(8000),
    };

    let observed = ServiceIdentityResult {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-abc".to_string()),
        token_fingerprint: Some(input.token_fingerprint()),
        endpoint: Some("127.0.0.1:8000".to_string()),
    };

    let result = input.verify(&observed);
    assert_eq!(result, IdentityVerification::Verified);
}

#[test]
fn health_with_no_identity_fields_does_not_verify() {
    use crate::infra::local_engine::port::{Endpoint, ServiceIdentityInput, ServiceIdentityResult};

    let input = ServiceIdentityInput {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        token: "secret-token-xyz".to_string(),
        endpoint: Endpoint::new(8000),
    };

    // 完全不回显身份字段
    let observed = ServiceIdentityResult {
        engine_id: None,
        instance_id: None,
        token_fingerprint: None,
        endpoint: None,
    };

    let result = input.verify(&observed);
    assert!(matches!(
        result,
        crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
    ));
}

// ── StdioWorker：无端口、无 kill 语义 ──

#[test]
fn unknown_port_occupation_does_not_kill() {
    // StdioWorker 引擎没有端口概念；descriptor 不包含任何 kill/端口终止语义
    let adapter = FunasrAdapter::new();
    let desc = adapter.descriptor();
    let json = serde_json::to_string(desc).unwrap();
    assert!(!json.contains("kill"));
    assert!(!json.contains("terminate"));
}

// ── adapter 契约 ──

#[test]
fn make_funasr_adapter_returns_valid_adapter() {
    let adapter = make_funasr_adapter();
    assert_eq!(adapter.descriptor().engine_id.as_str(), FUNASR_ENGINE_ID);
    assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Stt);
}

/// self_test：active deployment 结构检查。结果取决于机器状态（是否已安装），
/// 只验证返回了结果且失败文案指向引擎页。
#[test]
fn adapter_self_test_returns_result() {
    let adapter = FunasrAdapter::new();
    let result = adapter.self_test();
    if !result.passed {
        let reason = result.failure_reason.unwrap_or_default();
        assert!(
            !reason.contains("语音输入"),
            "错误文案不应指向'语音输入页': {reason}"
        );
    }
}

#[test]
fn adapter_diagnostics_returns_entries() {
    let adapter = FunasrAdapter::new();
    let diag = adapter.diagnostics();
    assert!(!diag.entries.is_empty());
}

#[test]
fn adapter_prepare_launch_rejects_undeclared_profile() {
    let adapter = FunasrAdapter::new();
    let undeclared_profile = ResolvedProfile {
        profile_id: "vulkan-x64".to_string(),
        backend: ComputeBackend::Vulkan,
        artifact_id: ArtifactId::new("funasr-gguf-worker-v0.2.6").unwrap(),
        priority: 0,
    };
    let ctx = LaunchContext {
        endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
        engine_id: "funasr".to_string(),
        instance_id: "inst-test".to_string(),
        token: "test-token-abcdef0123456789".to_string(),
        resolved_profile: undeclared_profile,
    };
    let config = AdapterConfig::new();
    let result = adapter.prepare_launch(&ctx, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
}

// ── 0.22.7 GGUF adapter 契约测试 ──────────────────────────────────────────

/// GGUF descriptor：ManagedBinary runtime + StdioWorker 传输 + CPU profile。
#[test]
fn gguf_descriptor_declares_stdio_worker_transport() {
    let adapter = FunasrAdapter::new();
    let d = adapter.descriptor();
    assert_eq!(d.runtime_kind, RuntimePlan::ManagedBinary);
    assert_eq!(
        d.service_transport,
        crate::domain::local_engine::ServiceTransport::StdioWorker
    );
    assert!(d.is_profile_allowed(&ResolvedProfile {
        profile_id: "cpu-x64".to_string(),
        backend: ComputeBackend::Cpu,
        artifact_id: ArtifactId::new("funasr-gguf-worker-v0.2.6").unwrap(),
        priority: 0,
    }));
}

/// 唯一实现：adapter 只注册 `funasr` 一个 engine id（不注册第二个引擎）。
#[test]
fn gguf_adapter_uses_single_funasr_engine_id() {
    let adapter = FunasrAdapter::new();
    assert_eq!(adapter.descriptor().engine_id.as_str(), "funasr");
}

/// GGUF 模型目录：三个模型、id 稳定、nano 双文件、hash 锁定非空。
#[test]
fn gguf_model_catalog_locked() {
    let specs = gguf::gguf_model_specs();
    assert_eq!(specs.len(), 3, "SenseVoice + Paraformer + Nano");
    assert!(specs.iter().all(|s| {
        s.files
            .iter()
            .all(|f| f.sha256.len() == 64 && f.url.starts_with("https://huggingface.co/"))
    }));
    let nano = gguf::find_gguf_spec(gguf::GGUF_NANO_ID).expect("nano spec");
    assert_eq!(nano.files.len(), 2, "Nano 需要 encoder + LLM 双 GGUF");
}

/// 旧模型 id → GGUF id 的确定迁移映射（真源在 domain 配置层）。
#[test]
fn gguf_legacy_model_migration_mapping() {
    assert_eq!(
        gguf::migrate_legacy_model_id("iic/SenseVoiceSmall"),
        Some(gguf::GGUF_SENSEVOICE_ID)
    );
    assert_eq!(
        gguf::migrate_legacy_model_id("paraformer-zh"),
        Some(gguf::GGUF_PARAFORMER_ID)
    );
    assert_eq!(gguf::migrate_legacy_model_id("unknown-model"), None);
}

/// GGUF provider descriptor：bundled 安装 + self-test 命令。
#[test]
fn gguf_provider_descriptor_bundled_plan() {
    let pd = make_funasr_provider_descriptor();
    match &pd.install_plan {
        InstallPlan::ManagedBinary(plan) => {
            assert_eq!(plan.bundled_dir.as_deref(), Some("bin/funasr-worker"));
            assert!(
                plan.self_test_command
                    .contains(&"--blink-selftest".to_string())
            );
        }
        other => panic!("GGUF 应为 ManagedBinary 计划: {other:?}"),
    }
}

// ── 0.22.7.2 真实端到端（env 门控：BLINK_E2E_GGUF=1）──────────────────────
//
// 覆盖验收链路：安装环境（捆绑 worker hash 校验）→ 安装模型（真实下载 +
// SHA-256 验证）→ start（NDJSON ready 握手 + 身份/指纹校验）→ get_connection
// （worker transport）→ 转录固定音频（非空 UTF-8）→ stop（优雅退出 + PID 归零）。
//
// 前置：`cargo xtask funasr-worker` 已构建 worker；固定音频 fixture 存在。
#[tokio::test(flavor = "multi_thread")]
async fn gguf_real_end_to_end_sensevoice() {
    if std::env::var("BLINK_E2E_GGUF").ok().as_deref() != Some("1") {
        eprintln!("跳过：设置 BLINK_E2E_GGUF=1 运行真实 GGUF 端到端测试");
        return;
    }
    let fixture = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    )
    .join("testdata/stt/funasr-runtime/generated/blink-spike.wav");
    if !fixture.is_file() {
        eprintln!("跳过：固定音频 fixture 不存在（{}）", fixture.display());
        return;
    }

    use crate::app::local_engine::model_installer::ModelRegistry;
    use crate::app::local_engine::registry::EngineRegistry;
    use crate::app::local_engine::{EngineManager, NoopEventPort};

    let registry = std::sync::Arc::new(EngineRegistry::new_with_adapters(vec![
        super::make_funasr_adapter(),
    ]));
    let service = EngineManager::new_with_providers(
        registry,
        std::sync::Arc::new(NoopEventPort),
        [(
            EngineId::new(FUNASR_ENGINE_ID).unwrap(),
            make_funasr_provider_descriptor(),
        )]
        .into_iter()
        .collect(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        ModelRegistry::new_with_models(
            gguf::gguf_model_specs()
                .iter()
                .map(gguf::gguf_model_descriptor)
                .collect(),
        ),
        std::sync::Arc::new(super::FunasrGgufModelInstallWorker::new()),
    );

    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();

    // 1. 安装环境（bundled worker + hash 校验 + self-test）
    let cfg = crate::domain::local_engine::AdapterConfig {
        engine_config: serde_json::to_value(FunasrEngineConfig {
            funasr_model: gguf::GGUF_SENSEVOICE_ID.to_string(),
            device: "cpu".to_string(),
            num_threads: None,
            hotwords: None,
            use_itn: true,
            vad: Default::default(),
            auto_start_server: false,
        })
        .unwrap(),
        preferred_port: None,
        compute_preference: Some(ComputePreference::Cpu),
    };
    service
        .install(&engine_id, cfg.clone())
        .await
        .expect("环境安装");

    // 2. 安装模型（真实下载 254MB + SHA-256 校验 + 单 active 事务）
    let installed = service
        .install_model(
            &engine_id,
            gguf::GGUF_SENSEVOICE_ID,
            Some("e2e-gguf".to_string()),
        )
        .await
        .expect("模型安装");
    assert!(installed.success, "模型安装事务失败: {:?}", installed.error);

    // 3. start：ready 握手 + 身份校验（Model Ready 才返回 Ok）
    service
        .start(&engine_id, cfg.clone())
        .await
        .expect("GGUF worker 启动");

    // 4. 连接快照携带 worker transport
    let conn = service
        .get_connection(&engine_id)
        .await
        .expect("get_connection")
        .expect("运行中应有连接");
    assert!(conn.worker.is_some(), "StdioWorker 引擎应附带 transport");
    let transport = conn.worker.unwrap();

    // 5. 转录固定音频（0.5s 前缀 + 完整 5.708s，覆盖伪流式快照语义）
    let wav_bytes = std::fs::read(&fixture).expect("读取 fixture");
    let full_text = transport.transcribe(&wav_bytes).await.expect("完整转录");
    assert!(!full_text.trim().is_empty(), "完整音频转录不应为空");
    assert!(
        full_text.contains("blink") || full_text.contains("recognition"),
        "识别内容应包含语音关键词: {full_text}"
    );

    // 0.5s 前缀快照（伪流式首个预览的音频量）
    let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes).expect("解析 WAV");
    let prefix = &samples[..(16000 / 2).min(samples.len())];
    let prefix_wav = crate::domain::stt::wav::pcm_to_wav(prefix, 16000, 1);
    let prefix_text = transport.transcribe(&prefix_wav).await.expect("前缀转录");
    eprintln!("0.5s 前缀识别: {prefix_text:?}; 完整识别: {full_text:?}");

    // 连续请求：同一 PID 内多次推理（常驻验证）
    for i in 0..3 {
        let t = transport.transcribe(&wav_bytes).await.expect("连续转录");
        assert_eq!(t, full_text, "同输入应得到稳定文本（第 {} 次）", i + 1);
    }

    // 6. 优雅停止：状态收敛 Stopped（managed 引用清除 → 旧 PID 归零）
    service.stop(&engine_id).await.expect("停止");
    let status = service.get_status(&engine_id).await.expect("get_status");
    assert_eq!(
        status.status.process,
        crate::domain::local_engine::ProcessState::Stopped,
        "停止后进程状态应为 Stopped（旧 PID 归零）"
    );

    // 7. 临时音频目录清空
    let audio_dir = worker::engine_audio_tmp_dir(&engine_id);
    if audio_dir.exists() {
        let count = std::fs::read_dir(&audio_dir).unwrap().flatten().count();
        assert_eq!(count, 0, "停止后 audio-tmp 应为空（残留 {count} 个）");
    }
}

// ── 0.22.7.2 真实崩溃重启（env 门控：BLINK_E2E_GGUF=1）─────────────────────
//
// 验收：worker 异常退出（外部 kill）→ exit monitor 收敛状态 + 销毁旧客户端 →
// 旧 transport 请求失败（管道断开，不伪装健康）→ 重启产生新实例身份 →
// 新 transport 恢复可用。
#[tokio::test(flavor = "multi_thread")]
async fn gguf_real_worker_crash_and_restart() {
    if std::env::var("BLINK_E2E_GGUF").ok().as_deref() != Some("1") {
        eprintln!("跳过：设置 BLINK_E2E_GGUF=1 运行真实 GGUF 崩溃重启测试");
        return;
    }
    // 与 E2E 主测试共享磁盘根目录（同 cargo test 进程），但模型可能未装
    // （单独运行本测试时）——用真实 installer 确保模型就位（已装则重新走
    // 事务，staging 全新 → 会重新下载；跨进程独立根目录时这是必要成本）。
    use crate::app::local_engine::model_installer::ModelRegistry;
    use crate::app::local_engine::registry::EngineRegistry;
    use crate::app::local_engine::{EngineManager, NoopEventPort};

    let registry = std::sync::Arc::new(EngineRegistry::new_with_adapters(vec![
        super::make_funasr_adapter(),
    ]));
    let service = EngineManager::new_with_providers(
        registry,
        std::sync::Arc::new(NoopEventPort),
        [(
            EngineId::new(FUNASR_ENGINE_ID).unwrap(),
            make_funasr_provider_descriptor(),
        )]
        .into_iter()
        .collect(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        ModelRegistry::new_with_models(
            gguf::gguf_model_specs()
                .iter()
                .map(gguf::gguf_model_descriptor)
                .collect(),
        ),
        std::sync::Arc::new(super::FunasrGgufModelInstallWorker::new()),
    );
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let cfg = crate::domain::local_engine::AdapterConfig {
        engine_config: serde_json::json!({
            "funasr_model": gguf::GGUF_SENSEVOICE_ID,
            "device": "cpu",
            "use_itn": true,
        }),
        preferred_port: None,
        compute_preference: Some(ComputePreference::Cpu),
    };

    service
        .install(&engine_id, cfg.clone())
        .await
        .expect("环境安装（已装则幂等）");

    // 模型就位（单独运行本测试时需要真实安装；已装则 install_model 幂等修复）
    let installed = service
        .install_model(
            &engine_id,
            gguf::GGUF_SENSEVOICE_ID,
            Some("e2e-crash".to_string()),
        )
        .await
        .expect("模型安装");
    assert!(installed.success, "模型安装失败: {:?}", installed.error);

    // 首次启动
    service
        .start(&engine_id, cfg.clone())
        .await
        .expect("首次启动");
    let conn1 = service
        .get_connection(&engine_id)
        .await
        .expect("get_connection")
        .expect("运行中");
    let transport1 = conn1.worker.expect("worker transport");
    let old_instance = conn1.instance_id.clone();

    // 外部 kill worker（模拟崩溃）
    let status = service.get_status(&engine_id).await.unwrap();
    let pid = match status.status.process {
        crate::domain::local_engine::ProcessState::Running { pid } => pid,
        other => panic!("启动后应为 Running，实际 {other:?}"),
    };
    let kill = crate::infra::platform::no_window(std::process::Command::new("taskkill"))
        .args(["/F", "/PID", &pid.to_string()])
        .output()
        .expect("taskkill 执行");
    assert!(
        kill.status.success(),
        "taskkill 失败: {}",
        String::from_utf8_lossy(&kill.stderr)
    );

    // 等待 exit monitor 收敛（最多 15s）
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        let s = service.get_status(&engine_id).await.unwrap();
        if matches!(
            s.status.process,
            crate::domain::local_engine::ProcessState::Exited { .. }
        ) {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "exit monitor 未在 15s 内收敛崩溃状态"
        );
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }

    // 旧 transport 必须失败（管道断开——不伪装健康）
    let wav = crate::domain::stt::wav::pcm_to_wav(&[0.0f32; 1600], 16000, 1);
    let stale = transport1.transcribe(&wav).await;
    assert!(stale.is_err(), "崩溃后旧 transport 请求必须失败");

    // 重启：新实例身份
    service
        .start(&engine_id, cfg.clone())
        .await
        .expect("崩溃后重启");
    let conn2 = service
        .get_connection(&engine_id)
        .await
        .expect("get_connection")
        .expect("运行中");
    assert_ne!(
        conn2.instance_id, old_instance,
        "重启必须产生新的 instance identity"
    );
    let transport2 = conn2.worker.expect("新 worker transport");

    // 新 transport 可用（1s 静音即可——验证通道而非识别质量）
    let ok = transport2.transcribe(&wav).await;
    assert!(ok.is_ok(), "新实例 transport 应恢复可用: {:?}", ok.err());

    service.stop(&engine_id).await.expect("收尾停止");
}

// ── 0.22.7.3 三模型矩阵 + 切换重启（env 门控：BLINK_E2E_GGUF=1）────────────
//
// 验收：
// - 三模型各自：安装 → 启动 ready → 0.5/1/2s 预览快照 + 完整 final（非空
//   UTF-8）→ 停止（PID 归零）；
// - Nano 不做延迟断言（自回归，粗粒度伪流式语义）；
// - 模型切换：A 运行 → 停止 → B 启动 = 新 PID/新实例；同一时刻全系统只有
//   一个 funasr-*-worker.exe 进程（单常驻铁则）。
#[tokio::test(flavor = "multi_thread")]
async fn gguf_real_three_models_and_switch() {
    if std::env::var("BLINK_E2E_GGUF").ok().as_deref() != Some("1") {
        eprintln!("跳过：设置 BLINK_E2E_GGUF=1 运行三模型矩阵测试");
        return;
    }
    use crate::app::local_engine::model_installer::ModelRegistry;
    use crate::app::local_engine::registry::EngineRegistry;
    use crate::app::local_engine::{EngineManager, NoopEventPort};

    // 离线模型缓存（开发机预下载目录）——存在则免网络
    let cache_dir = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    )
    .join("target/gguf-models");
    if cache_dir.is_dir() {
        // SAFETY: 仅门控 E2E 测试内设置；BLINK_GGUF_MODEL_CACHE 只被 GGUF
        // 安装 worker 读取（同进程内其他测试不消费该变量），无并发读方。
        unsafe { std::env::set_var("BLINK_GGUF_MODEL_CACHE", &cache_dir) };
    }

    let registry = std::sync::Arc::new(EngineRegistry::new_with_adapters(vec![
        super::make_funasr_adapter(),
    ]));
    let service = EngineManager::new_with_providers(
        registry,
        std::sync::Arc::new(NoopEventPort),
        [(
            EngineId::new(FUNASR_ENGINE_ID).unwrap(),
            make_funasr_provider_descriptor(),
        )]
        .into_iter()
        .collect(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        ModelRegistry::new_with_models(
            gguf::gguf_model_specs()
                .iter()
                .map(gguf::gguf_model_descriptor)
                .collect(),
        ),
        std::sync::Arc::new(super::FunasrGgufModelInstallWorker::new()),
    );
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();

    let fixture = std::path::PathBuf::from(
        std::env::var("CARGO_MANIFEST_DIR").unwrap_or_else(|_| ".".to_string()),
    )
    .join("testdata/stt/funasr-runtime/generated/blink-spike.wav");
    if !fixture.is_file() {
        eprintln!("跳过：固定音频 fixture 不存在（{}）", fixture.display());
        return;
    }
    let wav_bytes = std::fs::read(&fixture).expect("读取 fixture");
    let samples = crate::domain::stt::wav::parse_wav_to_f32(&wav_bytes).expect("解析 WAV");

    let make_cfg = |model: &str| crate::domain::local_engine::AdapterConfig {
        engine_config: serde_json::json!({
            "funasr_model": model,
            "device": "cpu",
            "use_itn": true,
        }),
        preferred_port: None,
        compute_preference: Some(ComputePreference::Cpu),
    };

    // 环境一次安装
    service
        .install(&engine_id, make_cfg(gguf::GGUF_SENSEVOICE_ID))
        .await
        .expect("环境安装");

    // 三模型逐个：安装 → 启动 → 预览快照矩阵 + final → 停止
    let models = [
        gguf::GGUF_SENSEVOICE_ID,
        gguf::GGUF_PARAFORMER_ID,
        gguf::GGUF_NANO_ID,
    ];
    let mut last_pid: Option<u32> = None;
    for model in models {
        let installed = service
            .install_model(&engine_id, model, Some("e2e-matrix".to_string()))
            .await
            .unwrap_or_else(|e| panic!("安装 {model} 失败: {e}"));
        assert!(
            installed.success,
            "安装 {model} 失败: {:?}",
            installed.error
        );

        service
            .start(&engine_id, make_cfg(model))
            .await
            .unwrap_or_else(|e| panic!("启动 {model} 失败: {e}"));

        let status = service.get_status(&engine_id).await.unwrap();
        let pid = match status.status.process {
            crate::domain::local_engine::ProcessState::Running { pid } => pid,
            other => panic!("{model} 启动后应 Running: {other:?}"),
        };
        // 切换后必须是全新 PID（旧 worker 已停止）
        if let Some(prev) = last_pid {
            assert_ne!(pid, prev, "模型切换后 PID 必须不同（旧实例未回收？）");
        }
        // 单常驻：全系统 funasr-*-worker.exe 进程数 == 1
        assert_eq!(
            count_worker_processes(),
            1,
            "同一时刻只允许一个 worker 常驻（模型 {model} 运行中）"
        );

        let conn = service
            .get_connection(&engine_id)
            .await
            .unwrap()
            .expect("连接");
        let transport = conn.worker.expect("transport");

        // 预览快照矩阵：0.5s / 1s / 2s（每段独立请求）+ 完整 final
        for dur_ms in [500u32, 1000, 2000] {
            let n = (16000u32 * dur_ms / 1000) as usize;
            let prefix = &samples[..n.min(samples.len())];
            let pw = crate::domain::stt::wav::pcm_to_wav(prefix, 16000, 1);
            let t = transport
                .transcribe(&pw)
                .await
                .unwrap_or_else(|e| panic!("{model} {dur_ms}ms 预览失败: {e}"));
            eprintln!("{model} {dur_ms}ms 预览: {t:?}");
            assert!(
                t.chars().all(|c| !c.is_control()),
                "预览必须是合法 UTF-8 文本"
            );
        }
        let full = transport
            .transcribe(&wav_bytes)
            .await
            .unwrap_or_else(|e| panic!("{model} final 失败: {e}"));
        assert!(!full.trim().is_empty(), "{model} final 文本不应为空");
        eprintln!("{model} final: {full:?}");

        // 停止 → PID 归零
        service.stop(&engine_id).await.expect("停止");
        let st = service.get_status(&engine_id).await.unwrap();
        assert_eq!(
            st.status.process,
            crate::domain::local_engine::ProcessState::Stopped,
            "{model} 停止后进程应 Stopped"
        );
        assert_eq!(count_worker_processes(), 0, "停止后不应有 worker 进程");
        last_pid = Some(pid);
    }
    eprintln!("三模型矩阵 + 切换全部通过");
}

/// 统计全系统 funasr-*-worker.exe 进程数（单常驻断言用）。
fn count_worker_processes() -> usize {
    let out = crate::infra::platform::no_window(std::process::Command::new("powershell"))
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Name 'funasr-*-worker' -ErrorAction SilentlyContinue | Measure-Object).Count",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}
