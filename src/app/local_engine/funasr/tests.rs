//! FunASR adapter 回归测试（0.22.7.4 起：GGUF 常驻 worker 唯一实现）。
//!
//! 旧 Python/PyTorch 链路的 venv、依赖锁、嵌入脚本、HTTP 端点与子模型
//! 测试已随 0.22.7.4 切换删除；本文件只保留对新实现仍成立的契约。

use super::*;
use crate::domain::local_engine::{CapabilityKind, LifecyclePolicy, ModelHealth, ServiceHealth};
use crate::infra::local_engine::providers::InstallPlan;
use crate::infra::local_engine::runtime::{
    ArtifactId, ComputeBackend, ComputePreference, EngineId, ResolvedProfile, RuntimePlan,
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

/// 旧真流式 Paraformer 模型 id（已废弃）→ Paraformer GGUF（同种类迁移，不静默换模型）。
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
            crate::domain::config::stt_config::GGUF_PARAFORMER_MODEL_ID,
            "旧真流式 id {legacy} 应归一化为 Paraformer GGUF（同种类迁移）"
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
    assert!(!funasr_config.auto_start_server);
    assert_eq!(funasr_config.vad.silence_threshold, 0.005);
    assert_eq!(funasr_config.vad.min_silence_ms, 300);
    assert_eq!(funasr_config.vad.min_sentence_ms, 800);
}

// ── VAD/model 参数映射不变 ──

#[test]
fn funasr_engine_config_preserves_vad() {
    let local = crate::domain::config::stt_config::LocalEngineConfig {
        vad: crate::domain::config::stt_config::VadConfig {
            silence_threshold: 0.003,
            min_silence_ms: 200,
            min_sentence_ms: 600,
            soft_window_s: 10,
            hard_window_s: 14,
            max_uncommitted_s: 16,
        },
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.vad.silence_threshold, 0.003);
    assert_eq!(funasr_config.vad.min_silence_ms, 200);
    assert_eq!(funasr_config.vad.min_sentence_ms, 600);
    // 0.23.7：窗口字段同步投影，保证 engine_config 消费方拿到完整 VAD 形状
    assert_eq!(funasr_config.vad.soft_window_s, 10);
    assert_eq!(funasr_config.vad.hard_window_s, 14);
    assert_eq!(funasr_config.vad.max_uncommitted_s, 16);
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
    assert!(back.auto_start_server);
}

#[test]
fn worker_threads_safe_auto_caps_background_cpu_load() {
    assert_eq!(gguf::resolve_worker_threads(None, 1), 1);
    assert_eq!(gguf::resolve_worker_threads(None, 4), 2);
    assert_eq!(gguf::resolve_worker_threads(None, 16), 4);
    assert_eq!(gguf::resolve_worker_threads(None, 64), 4);
}

#[test]
fn worker_threads_respects_explicit_valid_setting() {
    assert_eq!(gguf::resolve_worker_threads(Some(2), 16), 2);
    assert_eq!(gguf::resolve_worker_threads(Some(8), 16), 8);
    assert_eq!(gguf::resolve_worker_threads(Some(0), 16), 4);
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
    let decoded = crate::domain::stt::wav::decode_wav(&wav_bytes).expect("解析 WAV");
    let samples = decoded.samples;
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

/// 私有 corpus 实机验收：仅在 `BLINK_STT_CORPUS_DIR` 指向有效目录时运行。
///
/// 不安装环境、不下载模型；本机没有已安装模型时安全跳过。失败信息只包含
/// opaque case id 与计时，不包含文件名、路径或识别正文。
#[tokio::test(flavor = "multi_thread")]
async fn private_corpus_manifest_end_to_end() {
    let Some(corpus_dir) = super::corpus_runner::should_run() else {
        return;
    };

    use crate::app::local_engine::model_installer::ModelRegistry;
    use crate::app::local_engine::registry::EngineRegistry;
    use crate::app::local_engine::{EngineManager, NoopEventPort};
    use crate::domain::local_engine::{EngineId, ModelInstallState, ProcessState};

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
        ModelRegistry::new_with_models(
            gguf::gguf_model_specs()
                .iter()
                .map(gguf::gguf_model_descriptor)
                .collect(),
        ),
        std::sync::Arc::new(super::FunasrGgufModelInstallWorker::new()),
    );
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let Some(installed_model) = service
        .list_models(&engine_id)
        .await
        .into_iter()
        .find(|model| model.install_state == ModelInstallState::Installed)
    else {
        return;
    };

    let mut config = crate::app::local_engine::config_source::funasr_adapter_config();
    config.engine_config["funasr_model"] =
        serde_json::Value::String(installed_model.model_id.clone());
    if service.start(&engine_id, config).await.is_err() {
        return;
    }

    let connection = service
        .get_connection(&engine_id)
        .await
        .expect("get_connection")
        .expect("running worker connection");
    assert_eq!(
        connection.model_id.as_deref(),
        Some(installed_model.model_id.as_str())
    );
    let transport = connection.worker.expect("running worker transport");
    let status = service.get_status(&engine_id).await.expect("get_status");
    let worker_pid = match status.status.process {
        ProcessState::Running { pid } => Some(pid),
        _ => None,
    };

    let run = super::corpus_runner::run_corpus(super::corpus_runner::CorpusRunnerConfig {
        corpus_dir,
        transport: Some(transport),
        worker_pid,
    })
    .await;
    service.stop(&engine_id).await.expect("stop corpus worker");

    let results = run.expect("private corpus runner");
    assert_eq!(
        results.len(),
        11,
        "private corpus manifest case count drifted"
    );
    assert!(
        results.iter().all(|result| result.matched),
        "{}",
        super::corpus_runner::format_anonymous_summary(&results)
    );
    assert!(
        results
            .iter()
            .all(|result| result.peak_memory_bytes.is_some()),
        "worker peak memory metric missing"
    );
}

/// 开发工作区实机闭环：使用已构建且经 manifest 锁定的 worker 与本地模型缓存，
/// 不写用户 AppData，也不触发下载。仅显式设置
/// `BLINK_STT_CORPUS_STANDALONE=1` 时启用。
#[tokio::test(flavor = "multi_thread")]
async fn private_corpus_standalone_worker_end_to_end() {
    if std::env::var("BLINK_STT_CORPUS_STANDALONE").ok().as_deref() != Some("1") {
        return;
    }
    let Some(corpus_dir) = super::corpus_runner::should_run() else {
        return;
    };

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let worker_dir = root.join("resources/bin/funasr-worker");
    let worker_exe = worker_dir.join("funasr-sensevoice-worker.exe");
    let model = root.join("target/gguf-models/sensevoice-small-q8.gguf");
    if !worker_exe.is_file() || !model.is_file() {
        return;
    }

    let audio_dir_guard = tempfile::Builder::new()
        .prefix("private-corpus-audio-")
        .tempdir_in(root.join("target"))
        .expect("create private corpus audio tempdir");
    let audio_dir = audio_dir_guard.path().to_path_buf();

    let mut command = tokio::process::Command::new(&worker_exe);
    command
        .args(["-m", model.to_str().unwrap(), "--stdin-server"])
        .current_dir(&worker_dir)
        .env("BLINK_ENGINE_ID", FUNASR_ENGINE_ID)
        .env("BLINK_INSTANCE_ID", "private-corpus")
        .env("BLINK_ENGINE_TOKEN", "private-corpus-token")
        .env("BLINK_MODEL_ID", gguf::GGUF_SENSEVOICE_ID)
        .env("BLINK_MODEL_REVISION", "private-corpus")
        .env("BLINK_MODEL_PAYLOAD_DIR", model.parent().unwrap())
        .env("BLINK_AUDIO_DIR", &audio_dir)
        .env("BLINK_WORKER_THREADS", "4")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::infra::platform::no_window_tokio(command)
        .spawn()
        .expect("spawn standalone corpus worker");
    let pid = child.id();
    let stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");
    let client = crate::infra::local_engine::worker_proto::NdjsonWorkerClient::new(stdin, stdout);
    client
        .hello(std::time::Duration::from_secs(30))
        .await
        .expect("standalone worker ready");
    let transport: std::sync::Arc<dyn crate::domain::stt::SttTransport> = std::sync::Arc::new(
        worker::GgufSttTransport::new(client.clone(), audio_dir.clone()),
    );

    let results = super::corpus_runner::run_corpus(super::corpus_runner::CorpusRunnerConfig {
        corpus_dir,
        transport: Some(transport),
        worker_pid: pid,
    })
    .await
    .expect("standalone private corpus runner");

    client.request_shutdown().await;
    drop(client);
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }
    assert_eq!(
        results.len(),
        11,
        "private corpus manifest case count drifted"
    );
    assert!(
        results.iter().all(|result| result.matched),
        "{}",
        super::corpus_runner::format_anonymous_summary(&results)
    );
    assert!(
        results
            .iter()
            .all(|result| result.peak_memory_bytes.is_some()),
        "worker peak memory metric missing"
    );
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
    let decoded = crate::domain::stt::wav::decode_wav(&wav_bytes).expect("解析 WAV");
    let samples = decoded.samples;

    let make_cfg = |model: &str| crate::domain::local_engine::AdapterConfig {
        engine_config: serde_json::json!({
            "funasr_model": model,
            "device": "cpu",
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

// ── 0.23.7 真实伪流式链路回放（env 门控：BLINK_STT_REAL_PSEUDO=1）──────────
//
// 用真实 worker 驱动生产 PseudoStreamingSttEngine（离线 WAV 按 100ms 块喂入，
// 不 sleep 模拟实时），验证定稿推进、回滚收敛、松键收尾与可配置的未提交
// 上限在真实链路工作。与离线 sweep 互补：sweep 近似模拟异步提交，这里走
// 真实的 spawn finalize / commit / rollback 路径。
//
// 观察口径（报告只含数值与匿名 id，不含转写正文）：
// - confirmed_events：每次 confirmed 增长的（已喂入音频 ms, 已确认末端 ms）；
// - peak_buffer_ms：max(pcm_samples)（缓冲有界性）；
// - finalize_ms：松键收尾墙钟耗时；final_chars：终稿字符数（>0 即收尾有效）。
#[tokio::test(flavor = "multi_thread")]
async fn pseudo_streaming_real_worker_replay() {
    if std::env::var("BLINK_STT_REAL_PSEUDO").ok().as_deref() != Some("1") {
        eprintln!("跳过：设置 BLINK_STT_REAL_PSEUDO=1 运行真实伪流式链路回放");
        return;
    }
    let Some(corpus_dir) = super::corpus_runner::should_run() else {
        eprintln!("跳过：未设置 BLINK_STT_CORPUS_DIR");
        return;
    };

    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let worker_dir = root.join("resources/bin/funasr-worker");
    let worker_exe = worker_dir.join("funasr-nano-worker.exe");
    // Nano 模型走 AppData 安装缓存（只读；音频临时目录仍在 target 下）
    let appdata = std::env::var("APPDATA").expect("APPDATA");
    let model_root = std::path::PathBuf::from(&appdata)
        .join("blink/models/funasr/gguf-fun-asr-nano-q4km-9faa9616b982");
    let active = std::fs::read_to_string(model_root.join("active.json")).ok();
    let payload = active.and_then(|text| {
        serde_json::from_str::<serde_json::Value>(&text)
            .ok()
            .and_then(|v| {
                let slot = v["slot_id"].as_str()?.to_string();
                Some(model_root.join("slots").join(slot).join("payload"))
            })
    });
    let (encoder, llm) = match &payload {
        Some(payload)
            if payload.join("funasr-encoder-f16.gguf").is_file()
                && payload.join("qwen3-0.6b-q4km.gguf").is_file() =>
        {
            (
                payload.join("funasr-encoder-f16.gguf"),
                payload.join("qwen3-0.6b-q4km.gguf"),
            )
        }
        _ => {
            eprintln!("跳过：AppData 缺少已安装的 Nano 模型 payload");
            return;
        }
    };
    if !worker_exe.is_file() {
        eprintln!("跳过：本地缺少 funasr-nano-worker.exe");
        return;
    }
    let payload_dir = payload.expect("payload resolved above");

    // 只回放未列入 manifest 的长录音（与 sweep 的 unlabeled 集一致）
    let manifest = super::corpus_runner::load_manifest(&corpus_dir).expect("load manifest");
    let listed: std::collections::HashSet<_> = manifest
        .cases
        .iter()
        .map(|case| case.filename.to_lowercase())
        .collect();
    let mut wavs: Vec<std::path::PathBuf> = std::fs::read_dir(&corpus_dir)
        .expect("read corpus dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .filter(|path| {
            path.is_file()
                && path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("wav"))
                && !listed.contains(&path.file_name().unwrap().to_string_lossy().to_lowercase())
        })
        .collect();
    wavs.sort();
    assert!(!wavs.is_empty(), "corpus dir has no unlabeled long wavs");

    let audio_dir_guard = tempfile::Builder::new()
        .prefix("real-pseudo-audio-")
        .tempdir_in(root.join("target"))
        .expect("create audio tempdir");
    let audio_dir = audio_dir_guard.path().to_path_buf();

    let mut command = tokio::process::Command::new(&worker_exe);
    command
        .args([
            "--enc",
            encoder.to_str().unwrap(),
            "-m",
            llm.to_str().unwrap(),
            "--stdin-server",
        ])
        .current_dir(&worker_dir)
        .env("BLINK_ENGINE_ID", "funasr")
        .env("BLINK_INSTANCE_ID", "real-pseudo-replay")
        .env("BLINK_ENGINE_TOKEN", "real-pseudo-token")
        .env("BLINK_MODEL_ID", "gguf/fun-asr-nano-q4km")
        .env("BLINK_MODEL_REVISION", "gguf-v0.2.6")
        .env("BLINK_MODEL_PAYLOAD_DIR", &payload_dir)
        .env("BLINK_AUDIO_DIR", &audio_dir)
        .env("BLINK_WORKER_THREADS", "4")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true);
    let mut child = crate::infra::platform::no_window_tokio(command)
        .spawn()
        .expect("spawn worker for real pseudo replay");
    let stdin = child.stdin.take().expect("worker stdin");
    let stdout = child.stdout.take().expect("worker stdout");
    let client = crate::infra::local_engine::worker_proto::NdjsonWorkerClient::new(stdin, stdout);
    client
        .hello(std::time::Duration::from_secs(60))
        .await
        .expect("worker ready within 60s");

    // A（现状默认 8/12/12）与 H（长上下文 10/14/16，未提交上限与硬窗分离）：
    // 同一批音频上 A 应出现 cap 兜底边界而 H 不应出现——直接证明未提交上限
    // 读取的是配置值，而非 VAD 内部计时。
    use crate::domain::config::stt_config::{LocalEngineConfig, SttConfig, VadConfig};
    use crate::domain::stt::SttEngine;
    let profiles: Vec<(&str, VadConfig)> = vec![
        ("A", VadConfig::default()),
        (
            "H",
            VadConfig {
                min_sentence_ms: 1000,
                soft_window_s: 10,
                hard_window_s: 14,
                max_uncommitted_s: 16,
                ..VadConfig::default()
            },
        ),
    ];

    let mut report_cases = Vec::new();
    for (label, vad) in profiles {
        let config = SttConfig {
            local_engine: LocalEngineConfig {
                vad: vad.clone(),
                ..LocalEngineConfig::default()
            },
            ..SttConfig::default()
        };
        let transport: std::sync::Arc<dyn crate::domain::stt::SttTransport> = std::sync::Arc::new(
            worker::GgufSttTransport::new(client.clone(), audio_dir.clone()),
        );
        let conn = crate::domain::stt::SttEngineConnection {
            host: "127.0.0.1".into(),
            port: 0,
            engine_id: "funasr".into(),
            instance_id: "real-pseudo-replay".into(),
            transport: Some(transport),
        };
        let engine =
            crate::domain::stt::pseudo_streaming::PseudoStreamingSttEngine::from_connection(
                &config, conn,
            )
            .expect("engine constructs");

        for path in &wavs {
            let wav = std::fs::read(path).expect("read wav");
            let audio =
                super::corpus_runner::decode_and_normalize(&wav).expect("decode and normalize");
            let case_id = {
                use sha2::{Digest, Sha256};
                format!("new_{:x}", Sha256::digest(&wav))[..8].to_string()
            };
            let sample_rate = 16_000usize;
            let mut fed_samples = 0usize;
            let mut confirmed_events: Vec<serde_json::Value> = Vec::new();
            let mut peak_buffer_ms = 0f64;
            let mut peak_uncommitted_ms = 0f64;

            for chunk in audio.samples.chunks(sample_rate / 10) {
                engine.transcribe_chunk(chunk).await.expect("chunk ok");
                fed_samples += chunk.len();
                // 实时速率喂入：让流式阶段的定稿/回滚真实发生（快速回放时
                // 推理慢于喂入，所有边界都会推迟到松键收尾，掩盖窗口差异）
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let stats = engine.stream_stats();
                peak_buffer_ms =
                    peak_buffer_ms.max(stats.pcm_samples as f64 * 1000.0 / sample_rate as f64);
                peak_uncommitted_ms = peak_uncommitted_ms.max(
                    (fed_samples.saturating_sub(stats.pcm_committed_end)) as f64 * 1000.0
                        / sample_rate as f64,
                );
                let committed_ms = stats.pcm_committed_end * 1000 / sample_rate;
                let need_push = match confirmed_events.last() {
                    Some(last) => last["committed_ms"] != committed_ms,
                    None => true,
                };
                if need_push {
                    confirmed_events.push(serde_json::json!({
                        "fed_ms": fed_samples * 1000 / sample_rate,
                        "committed_ms": committed_ms,
                    }));
                }
            }

            // 松键收尾：等待后台定稿 + 终段识别
            let finalize_started = std::time::Instant::now();
            let final_text = engine.finalize().await.expect("finalize ok");
            let finalize_ms = finalize_started.elapsed().as_millis() as u64;

            report_cases.push(serde_json::json!({
                "profile": label,
                "case_id": case_id,
                "duration_ms": audio.samples.len() * 1000 / sample_rate,
                "confirmed_events": confirmed_events,
                "peak_buffer_ms": peak_buffer_ms.round() as u64,
                "peak_uncommitted_ms": peak_uncommitted_ms.round() as u64,
                "finalize_ms": finalize_ms,
                "final_chars": final_text.chars().count(),
            }));
            println!(
                "profile {label} case {case_id}: events={} peak_buffer={}ms peak_uncommitted={}ms finalize={finalize_ms}ms final_chars={}",
                confirmed_events.len(),
                peak_buffer_ms.round() as u64,
                peak_uncommitted_ms.round() as u64,
                final_text.chars().count()
            );
        }
        engine.reset();
    }

    client.request_shutdown().await;
    drop(client);
    if tokio::time::timeout(std::time::Duration::from_secs(5), child.wait())
        .await
        .is_err()
    {
        let _ = child.kill().await;
    }

    let output = root.join("target/stt-vad-real-pseudo-replay.json");
    let report = serde_json::json!({
        "scope": "Real PseudoStreamingSttEngine replay (A vs H) over unlabeled long wavs; numeric metrics only, no transcript content.",
        "cases": report_cases,
    });
    std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
        .expect("write real pseudo replay report");
}
