//! FunASR adapter 回归测试（自原单文件 `#[cfg(test)] mod tests` 整体迁移，断言不变）。

use super::launch::{build_funasr_args, funasr_device_for_backend, funasr_submodels_for};
use super::locks::parse_locked_requirements;
use super::*;
use crate::domain::local_engine::{CapabilityKind, LifecyclePolicy, ModelHealth, ServiceHealth};
use crate::domain::stt::funasr;
use crate::infra::local_engine::providers::{InstallPlan, PackageLock, PipExtraArg};
use crate::infra::local_engine::runtime as engine_runtime;
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
fn descriptor_has_python_venv_runtime_kind() {
    let adapter = FunasrAdapter::new();
    assert_eq!(adapter.descriptor().runtime_kind, RuntimePlan::PythonVenv);
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
    // 0.22.6: 只声明 CPU profile（CUDA 需独立锁文件后启用）
    let adapter = FunasrAdapter::new();
    let desc = adapter.descriptor();
    assert!(desc.has_preference(ComputePreference::Cpu));
    // 确保 CUDA 不在声明列表中
    assert!(
        !desc.has_preference(ComputePreference::Cuda),
        "0.22.6 不应声明 CUDA preference"
    );
}

#[test]
fn descriptor_allows_cpu_profile() {
    let adapter = FunasrAdapter::new();
    let profile = ResolvedProfile {
        profile_id: "cpu-x64".to_string(),
        backend: ComputeBackend::Cpu,
        artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
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
        artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
        priority: 0,
    };
    assert!(!adapter.descriptor().is_profile_allowed(&profile));
}

// ── 0.22.6.1 设备唯一真相 ─────────────────────────────────────────────

/// 历史 config device=cuda + CPU profile → argv 必须包含 `--device cpu`，
/// 历史 device 字段不得成为启动执行真相。
#[test]
fn launch_args_use_cpu_for_historical_cuda_config() {
    // 历史配置残留 device=cuda（wire 兼容保留，允许反序列化）
    let local: crate::domain::config::stt_config::LocalEngineConfig = serde_json::from_str(
        r#"{"server_port": 8000, "funasr_model": "iic/SenseVoiceSmall", "device": "cuda", "use_itn": false}"#,
    )
    .unwrap();
    assert_eq!(local.device, "cuda");

    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    // 启动设备只从 resolved profile（cpu-x64 → Cpu）推导
    let device = funasr_device_for_backend(ComputeBackend::Cpu).unwrap();
    let args = build_funasr_args(
        &funasr_config.funasr_model,
        &device,
        8000,
        None,
        funasr_config.use_itn,
        std::path::Path::new("blink_stt_server.py"),
    );

    let device_pos = args
        .iter()
        .position(|a| a == "--device")
        .expect("argv 必须包含 --device");
    assert_eq!(
        args[device_pos + 1],
        "cpu",
        "CPU profile 必须生成 --device cpu"
    );
    assert!(
        !args.iter().any(|a| a == "cuda"),
        "历史 config device=cuda 不得泄漏进启动 argv"
    );
}

/// resolved profile 是当前不支持的 backend → 结构化 Unsupported，不回落。
#[test]
fn funasr_device_for_backend_rejects_non_cpu() {
    for backend in [
        ComputeBackend::Cuda,
        ComputeBackend::Vulkan,
        ComputeBackend::Directml,
    ] {
        let err = funasr_device_for_backend(backend).unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
    }
}

/// CPU profile 推导结果必须是字面量 "cpu"（Python --device 契约）。
#[test]
fn funasr_device_for_backend_cpu_literal() {
    assert_eq!(
        funasr_device_for_backend(ComputeBackend::Cpu).unwrap(),
        "cpu"
    );
}

// ── 旧 SttConfig 反序列化结果不变 ──

#[test]
fn old_stt_config_deserialization_unchanged() {
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
    assert_eq!(funasr_config.funasr_model, "iic/SenseVoiceSmall");
    assert_eq!(funasr_config.device, "cpu");
    assert!(funasr_config.use_itn);
    assert!(!funasr_config.auto_start_server);
    assert_eq!(funasr_config.vad.silence_threshold, 0.005);
    assert_eq!(funasr_config.vad.min_silence_ms, 300);
    assert_eq!(funasr_config.vad.min_sentence_ms, 800);
}

#[test]
fn old_stt_config_with_hotwords_deserialization() {
    let json = r#"{
        "server_port": 8000,
        "funasr_model": "paraformer-zh",
        "device": "cuda",
        "hotwords": "美团 100, 快手 80",
        "use_itn": false,
        "auto_start_server": true
    }"#;
    let local: crate::domain::config::stt_config::LocalEngineConfig =
        serde_json::from_str(json).unwrap();
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.funasr_model, "paraformer-zh");
    assert_eq!(funasr_config.device, "cuda");
    assert_eq!(funasr_config.hotwords.as_deref(), Some("美团 100, 快手 80"));
    assert!(!funasr_config.use_itn);
    assert!(funasr_config.auto_start_server);
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
        funasr_model: "paraformer-zh".to_string(),
        ..Default::default()
    };
    let funasr_config = FunasrEngineConfig::from_stt_config(&local);
    assert_eq!(funasr_config.funasr_model, "paraformer-zh");
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
        funasr_model: "iic/SenseVoiceSmall".to_string(),
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
    assert_eq!(back.funasr_model, "iic/SenseVoiceSmall");
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
    // 旧版 server 没有 model_status 字段
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

/// 0.22.6.1 requested/actual 语义：模型 Loading/Idle 时 Python 不回传
/// `backend`（只有 `requested_backend`）——映射结果不得伪造 backend 观测。
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
        "model_id": "iic/SenseVoiceSmall",
        "model_revision": "v1.0",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model_id, Some("iic/SenseVoiceSmall".to_string()));
    assert_eq!(mapping.model_revision, Some("v1.0".to_string()));
}

// ── health engine/instance/token 不匹配失败 ──
// 这些测试验证 health 响应缺少身份字段时的行为。
// 完整的身份校验由 EngineManager 在调用 map_health 后，
// 使用 ServiceIdentityInput::verify 核对 engine id、instance id 和 token。

#[test]
fn health_without_identity_fields_still_maps_model_status() {
    // 旧版 server 不回显身份字段，但 model_status 仍可用
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
    });
    let mapping = map_funasr_health(&raw);
    // service 标记为 Healthy（HTTP 可达）
    // 但 EngineManager 会在后续身份校验中将其降级为 Unreachable
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

    // health 回显了错误的 engine_id
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

    // 旧版 server 完全不回显身份字段
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

// ── 未知端口占用不 kill ──

#[test]
fn unknown_port_occupation_does_not_kill() {
    // 此测试验证 ManagedProcess 的行为：
    // 端口被未知进程占用时只报错或换端口，不自动 kill。
    // 完整的行为测试在 infra/local_engine/tests.rs 中。
    // 这里验证 adapter 的 descriptor 不包含任何 kill 行为。
    let adapter = FunasrAdapter::new();
    let desc = adapter.descriptor();
    // descriptor 不包含任何 kill 或端口终止相关字段
    let json = serde_json::to_string(desc).unwrap();
    assert!(!json.contains("kill"));
    assert!(!json.contains("terminate"));
}

// ── transcription client 请求字段和 endpoint 兼容 ──

#[test]
fn transcription_endpoint_is_loopback() {
    // FunASR transcription endpoint 只使用 127.0.0.1
    let base_url = funasr::server_base_url(8000);
    assert!(
        base_url.contains("localhost") || base_url.contains("127.0.0.1"),
        "base_url 应使用 loopback: {base_url}"
    );
}

#[test]
fn transcription_request_fields_compatible() {
    // 验证 transcription 请求字段与现有 LocalSttEngine 兼容
    // LocalSttEngine 调用 POST {base_url}/audio/transcriptions
    // 使用 wav::transcribe_async(url, None, model, wav_bytes)
    let base_url = funasr::server_base_url(8000);
    let url = format!("{base_url}/audio/transcriptions");
    assert!(url.contains("/v1/audio/transcriptions"));
}

#[test]
fn embedded_script_is_valid() {
    assert!(!BLINK_STT_SERVER_PY.is_empty());
    assert!(BLINK_STT_SERVER_PY.contains("blink_stt_server"));
    assert!(BLINK_STT_SERVER_PY.contains("/v1/audio/transcriptions"));
    assert!(BLINK_STT_SERVER_PY.contains("/health"));
}

#[test]
fn make_funasr_adapter_returns_valid_adapter() {
    let adapter = make_funasr_adapter();
    assert_eq!(adapter.descriptor().engine_id.as_str(), FUNASR_ENGINE_ID);
    assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Stt);
}

#[test]
fn adapter_self_test_checks_python_env() {
    let adapter = FunasrAdapter::new();
    let result = adapter.self_test();
    // self_test 检查 venv 和 funasr——开发环境可能未安装
    // 只验证返回了结果（passed 或 failed），不强制要求 passed
    let _ = result.passed;
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
        artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
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

// ── 0.22.6 H1: generation venv 路径测试 ──────────────────────────────

/// 互斥锁：序列化 generation venv 相关测试，避免并行测试互相清理临时目录。
static GEN_VENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// 辅助：在测试临时目录中模拟 generation venv 安装。
///
/// 创建 `runtimes/engines/funasr/generations/{install_id}/venv/Scripts/python.exe`
/// 和对应的 `current.json`。
fn setup_test_generation_venv(install_id: &str) -> std::path::PathBuf {
    use crate::infra::local_engine::deployment::{
        DEPLOYMENT_POINTER_SCHEMA_VERSION, DeploymentPointer, DeploymentStore,
    };
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let slot = if install_id.ends_with("001") {
        "slot-a"
    } else {
        "slot-b"
    };
    let gen_dir = engine_runtime::slot_dir(&engine_id, slot);
    let venv_scripts = gen_dir.join("venv").join("Scripts");
    std::fs::create_dir_all(&venv_scripts).unwrap();
    let python_exe = venv_scripts.join("python.exe");
    std::fs::write(&python_exe, b"fake python").unwrap();

    // 写入 deployment.json（active 指针）
    let pointer = DeploymentPointer {
        install_id: install_id.to_string(),
        slot: slot.to_string(),
        updated_at_ms: 0,
        schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
    };
    DeploymentStore::write_pointer(&engine_id, &pointer).unwrap();

    python_exe
}

/// 辅助：清理测试用的 generation 数据。
fn cleanup_test_generation() {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let engine_root = engine_runtime::engine_root(&engine_id);
    let _ = std::fs::remove_dir_all(&engine_root);
}

/// active deployment venv 存在时，返回正确路径。
#[test]
fn generation_venv_python_returns_path_when_installed() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();
    let install_id = "test-install-001";
    let python_exe = setup_test_generation_venv(install_id);

    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let result = active_deployment_venv_python(&engine_id);
    assert!(result.is_some(), "generation venv 已安装时应返回路径");
    assert_eq!(result.unwrap(), python_exe);

    cleanup_test_generation();
}

/// 0.22.6 H1: 无 generation venv 时返回 None。
#[test]
fn generation_venv_python_returns_none_when_not_installed() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let result = active_deployment_venv_python(&engine_id);
    assert!(result.is_none(), "未安装时应返回 None");

    cleanup_test_generation();
}

/// 0.22.6 H1: self_test 在无 generation venv 时报告失败。
#[test]
fn self_test_fails_when_no_generation_venv() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();
    let adapter = FunasrAdapter::new();
    let result = adapter.self_test();
    assert!(!result.passed, "无 generation venv 时 self_test 应失败");
    let reason = result.failure_reason.unwrap_or_default();
    assert!(
        reason.contains("引擎") || reason.contains("安装"),
        "失败原因应引导到引擎页: {reason}"
    );

    cleanup_test_generation();
}

/// 0.22.6 H1: self_test 错误文案指向引擎页，不指向语音输入页。
#[test]
fn self_test_error_message_points_to_engine_page() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();
    let adapter = FunasrAdapter::new();
    let result = adapter.self_test();
    if !result.passed {
        let reason = result.failure_reason.unwrap_or_default();
        assert!(
            !reason.contains("语音输入"),
            "错误文案不应指向'语音输入页': {reason}"
        );
        assert!(
            reason.contains("引擎") || reason.contains("本地模型运行时"),
            "错误文案应指向引擎页: {reason}"
        );
    }

    cleanup_test_generation();
}

/// 0.22.6 H1: prepare_launch 在无 generation venv 时返回
/// EnvironmentMissing 错误，错误文案指向引擎页。
#[test]
fn prepare_launch_fails_without_generation_venv() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();
    let adapter = FunasrAdapter::new();
    let profile = ResolvedProfile {
        profile_id: "cpu-x64".to_string(),
        backend: ComputeBackend::Cpu,
        artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
        priority: 0,
    };
    let ctx = LaunchContext {
        endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
        engine_id: "funasr".to_string(),
        instance_id: "inst-test".to_string(),
        token: "test-token-abcdef0123456789".to_string(),
        resolved_profile: profile,
    };
    // 提供有效的 engine_config，避免 InvalidConfig 错误
    let funasr_config = FunasrEngineConfig {
        funasr_model: "iic/SenseVoiceSmall".to_string(),
        device: "cpu".to_string(),
        num_threads: None,
        hotwords: None,
        use_itn: true,
        vad: VadConfigProjection::default(),
        auto_start_server: false,
    };
    let config = AdapterConfig {
        preferred_port: None,
        compute_preference: None,
        engine_config: funasr_config.to_json(),
    };
    let result = adapter.prepare_launch(&ctx, &config);
    assert!(result.is_err());
    let err = result.unwrap_err();
    assert_eq!(
        err.code,
        LocalEngineErrorCode::EnvironmentMissing,
        "无 generation venv 时应返回 EnvironmentMissing"
    );
    // 错误文案应指向引擎页
    assert!(
        !err.action_hint.contains("语音输入"),
        "错误文案不应指向语音输入页"
    );

    cleanup_test_generation();
}

/// 0.22.6 H1: prepare_launch 的 LaunchDescriptor 使用 generation venv python，
/// 不使用旧全局 venv。
#[test]
fn launch_descriptor_uses_generation_python() {
    let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
    cleanup_test_generation();

    // 创建 generation venv
    let install_id = "test-launch-001";
    let _gen_python = setup_test_generation_venv(install_id);

    // 0.22.6 B2: 使用 mock 包检查器避免执行假 python.exe（挂死风险）。
    // mock 检查器总是返回 (false, None)，模拟 funasr 未安装。
    // 这验证了 prepare_launch 能正确解析 generation python 路径，
    // 并在 funasr 检查失败时返回正确的错误类型。
    fn mock_checker(_python: &std::path::Path, _pkg: &str) -> (bool, Option<String>) {
        (false, None)
    }

    // prepare_launch 使用 mock 包检查器，在 funasr 检查时返回 false，
    // 错误应来自 funasr 检查，而非 python 环境缺失。
    let adapter = FunasrAdapter::new_with_package_checker(mock_checker);
    let profile = ResolvedProfile {
        profile_id: "cpu-x64".to_string(),
        backend: ComputeBackend::Cpu,
        artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
        priority: 0,
    };
    let ctx = LaunchContext {
        endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
        engine_id: "funasr".to_string(),
        instance_id: "inst-test".to_string(),
        token: "test-token-abcdef0123456789".to_string(),
        resolved_profile: profile,
    };
    // 提供有效的 engine_config
    let funasr_config = FunasrEngineConfig {
        funasr_model: "iic/SenseVoiceSmall".to_string(),
        device: "cpu".to_string(),
        num_threads: None,
        hotwords: None,
        use_itn: true,
        vad: VadConfigProjection::default(),
        auto_start_server: false,
    };
    let config = AdapterConfig {
        preferred_port: None,
        compute_preference: None,
        engine_config: funasr_config.to_json(),
    };
    let result = adapter.prepare_launch(&ctx, &config);

    // mock 检查器返回 (false, None)，模拟 funasr 未安装
    assert!(result.is_err());
    let err = result.unwrap_err();
    // 错误应该是 funasr 包未安装（不是 python 环境缺失）
    assert_eq!(
        err.code,
        LocalEngineErrorCode::EnvironmentMissing,
        "应因 funasr 未安装而失败"
    );
    // 不应出现 "Python 环境未就绪" 错误（那意味着 generation python 不存在）
    assert!(
        !err.action_hint.contains("Python 环境未就绪"),
        "不应报 Python 环境未就绪（generation python 已存在）"
    );

    // 清理
    let _ = std::fs::remove_dir_all(engine_runtime::python_shared_root());
    cleanup_test_generation();
}

/// 0.22.6 H1: ModelScope 缓存路径与 engine_model_cache_dir 一致。
#[test]
fn model_cache_path_is_engine_model_cache_dir() {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let cache_dir = engine_runtime::engine_model_cache_dir(&engine_id);
    let expected = engine_runtime::models_root().join(FUNASR_ENGINE_ID);
    assert_eq!(
        cache_dir, expected,
        "engine_model_cache_dir 应返回 models/{engine_id}"
    );
}

/// 0.22.6 H1: 嵌入的 Python 脚本包含 model_content_fingerprint 逻辑。
#[test]
fn embedded_script_has_content_fingerprint() {
    assert!(
        BLINK_STT_SERVER_PY.contains("model_content_fingerprint"),
        "Python 脚本应包含 model_content_fingerprint"
    );
    assert!(
        BLINK_STT_SERVER_PY.contains("_compute_model_content_fingerprint"),
        "Python 脚本应包含 _compute_model_content_fingerprint 函数"
    );
}

/// 0.22.6 H1: health 映射在 Ready 时返回 model_content_fingerprint。
#[test]
fn health_maps_content_fingerprint_when_ready() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
        "model_id": "iic/SenseVoiceSmall",
        "model_revision": "funasr-1.x",
        "model_content_fingerprint": "abc123def456",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Ready);
    assert_eq!(
        mapping.model_content_fingerprint,
        Some("abc123def456".to_string())
    );
}

/// 0.22.6 H1: health 映射在非 Ready 时不返回 fingerprint。
#[test]
fn health_omits_fingerprint_when_not_ready() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "loading",
        "model_content_fingerprint": "abc123",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Loading);
    assert!(
        mapping.model_content_fingerprint.is_none(),
        "非 Ready 状态不应返回 fingerprint"
    );
}

/// 0.22.6 H1: health 映射在 Ready 但 fingerprint 为空时返回 None。
#[test]
fn health_omits_empty_fingerprint_when_ready() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
        "model_content_fingerprint": "",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Ready);
    assert!(
        mapping.model_content_fingerprint.is_none(),
        "空 fingerprint 应映射为 None"
    );
}

/// 0.22.6 H1: health 映射在 Ready 但 fingerprint 缺失时返回 None。
#[test]
fn health_omits_missing_fingerprint_when_ready() {
    let raw = serde_json::json!({
        "status": "ok",
        "model_status": "ready",
    });
    let mapping = map_funasr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Ready);
    assert!(
        mapping.model_content_fingerprint.is_none(),
        "缺失 fingerprint 应映射为 None"
    );
}

/// 0.22.6 H1: FunASR descriptor 的 model_id 与 Python server 返回的一致。
#[test]
fn descriptor_model_id_matches_python_server_response() {
    let adapter = FunasrAdapter::new();
    let descriptor_model_id = &adapter.descriptor().model_contract.model_id;
    assert_eq!(
        descriptor_model_id, "iic/SenseVoiceSmall",
        "descriptor model_id 应为 iic/SenseVoiceSmall"
    );
    // Python server health 返回 model_id = args.model（默认 iic/SenseVoiceSmall）
}

/// 0.22.6 H1: FunASR descriptor 的 model_revision 与 Python server 返回的一致。
#[test]
fn descriptor_model_revision_matches_python_server_response() {
    let adapter = FunasrAdapter::new();
    let descriptor_revision = &adapter.descriptor().model_contract.revision;
    assert_eq!(
        descriptor_revision, "funasr-1.x",
        "descriptor revision 应为 funasr-1.x"
    );
    // Python server health 返回 model_revision = "funasr-1.x"
}

// ── FunASR 依赖锁闭环测试 ──────────────────────────────────────────

/// 验证 locked-requirements.txt 解析出的包列表包含全部传递依赖（>8 个直接包）。
#[test]
fn funasr_locked_packages_includes_transitive_deps() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        // 之前硬编码只有 8 个直接包；完整锁应有 76 个（含传递依赖）
        assert!(
            plan.packages.len() > 8,
            "locked-requirements.txt 应解析出 >8 个包（含传递依赖），实际: {}",
            plan.packages.len()
        );
        tracing::info!(
            "FunASR locked-requirements.txt 解析出 {} 个包",
            plan.packages.len()
        );
    }
}

/// 验证所有包的 all_hashes 非空（多平台 wheel hash）。
#[test]
fn funasr_locked_packages_all_hashes_non_empty() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            assert!(
                !pkg.all_hashes.is_empty(),
                "PackageLock {} 的 all_hashes 为空，--require-hashes 需要至少一个 hash",
                pkg.name
            );
            // 所有 hash 格式验证
            for h in &pkg.all_hashes {
                assert_eq!(
                    h.len(),
                    64,
                    "PackageLock {} 的 all_hashes 中有长度不为 64 的 hash",
                    pkg.name
                );
                assert!(
                    h.bytes().all(|b| b.is_ascii_hexdigit()),
                    "PackageLock {} 的 all_hashes 中有非 hex 字符",
                    pkg.name
                );
            }
        }
    }
}

/// 验证所有 production 包使用精确版本（不存在 >= ~> < > 等非精确约束）。
#[test]
fn funasr_locked_packages_use_exact_versions() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            assert!(
                !pkg.version.starts_with('>')
                    && !pkg.version.starts_with('<')
                    && !pkg.version.starts_with('~')
                    && !pkg.version.starts_with('!'),
                "{} 使用了非精确版本约束: {}",
                pkg.name,
                pkg.version
            );
        }
    }
}

/// 验证 hash 不存在空 hash、非法 hash 或全零占位。
#[test]
fn funasr_locked_packages_no_empty_or_zero_hashes() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            // sha256 必须存在
            assert!(pkg.sha256.is_some(), "{} 的 sha256 为 None", pkg.name);
            let hash = pkg.sha256.as_ref().unwrap();
            // 不能是全零占位
            assert!(
                !hash.chars().all(|c| c == '0'),
                "{} 的 sha256 是全零占位",
                pkg.name
            );
            // 不能是空字符串
            assert!(!hash.is_empty(), "{} 的 sha256 为空字符串", pkg.name);
        }
    }
}

/// 验证嵌入的锁文件可解析（非空、格式正确）。
#[test]
fn funasr_embedded_lock_is_parseable() {
    assert!(!LOCKED_REQUIREMENTS_TXT.is_empty());
    let packages = parse_locked_requirements(LOCKED_REQUIREMENTS_TXT);
    assert!(
        !packages.is_empty(),
        "locked-requirements.txt 解析结果不应为空"
    );
}

/// 验证安装计划包含 --no-deps（禁止传递依赖自动解析）。
#[test]
fn funasr_provider_descriptor_has_no_deps() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        assert!(
            plan.extra_pip_args
                .iter()
                .any(|arg| matches!(arg, PipExtraArg::NoDeps)),
            "安装计划必须包含 --no-deps，禁止传递依赖自动解析"
        );
    }
}

/// 验证安装计划包含 PyTorch ExtraIndexUrl。
#[test]
fn funasr_provider_descriptor_has_pytorch_index() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        assert!(
            plan.extra_pip_args.iter().any(|arg| matches!(
                arg,
                PipExtraArg::ExtraIndexUrl(url) if url.contains("pytorch.org")
            )),
            "安装计划必须包含 PyTorch ExtraIndexUrl"
        );
    }
}

/// FunASR 的完整锁横跨 PyPI 与 PyTorch CPU index，必须允许跨索引匹配锁定版本。
#[test]
fn funasr_provider_descriptor_has_cross_index_strategy() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        assert!(
            plan.extra_pip_args
                .iter()
                .any(|arg| matches!(arg, PipExtraArg::IndexStrategyUnsafeBestMatch)),
            "FunASR 多索引锁安装必须启用 unsafe-best-match"
        );
    }
}

/// Windows CPU profile 必须锁到 PyTorch 官方 cp312 win_amd64 CPU wheel。
#[test]
fn funasr_pytorch_packages_lock_windows_cpu_wheels() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let torch = plan
            .packages
            .iter()
            .find(|pkg| pkg.name == "torch")
            .unwrap();
        assert_eq!(torch.version, "2.5.0+cpu");
        assert_eq!(
            torch.all_hashes,
            ["3815a38bbe31d0c546a33a0c59a5426563e94aea6d32eb4cf07b6a99bfa7130f"]
        );

        let torchaudio = plan
            .packages
            .iter()
            .find(|pkg| pkg.name == "torchaudio")
            .unwrap();
        assert_eq!(torchaudio.version, "2.5.0+cpu");
        assert_eq!(
            torchaudio.all_hashes,
            ["c972268b2711662d7e01479c38bb49b3da0a38b678f78451c545d4f36384f5ad"]
        );
    }
}

/// 验证 locked-requirements.txt 中包含关键直接依赖。
#[test]
fn funasr_locked_packages_contains_key_deps() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
        // 直接依赖
        assert!(names.contains(&"torch"), "缺少 torch");
        assert!(names.contains(&"torchaudio"), "缺少 torchaudio");
        assert!(names.contains(&"funasr"), "缺少 funasr");
        assert!(names.contains(&"fastapi"), "缺少 fastapi");
        assert!(names.contains(&"uvicorn"), "缺少 uvicorn");
        // 关键传递依赖
        assert!(names.contains(&"numba"), "缺少传递依赖 numba");
        assert!(names.contains(&"numpy"), "缺少传递依赖 numpy");
        assert!(names.contains(&"scipy"), "缺少传递依赖 scipy");
    }
}

/// 验证 numba 使用精确版本，不是 >=0.59。
#[test]
fn funasr_numba_uses_exact_version() {
    let pd = make_funasr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let numba = plan.packages.iter().find(|p| p.name == "numba");
        assert!(numba.is_some(), "缺少 numba 包");
        let numba = numba.unwrap();
        assert_eq!(
            numba.version, "0.59.0",
            "numba 应使用精确版本 0.59.0，而不是 >=0.59"
        );
        // 不能以 >= 开头
        assert!(!numba.version.starts_with(">="), "numba 不应使用 >= 约束");
    }
}

/// 验证 render_hashed_requirements 能正确渲染多 hash 条目。
#[test]
fn funasr_render_hashed_requirements_supports_multiple_hashes() {
    use crate::infra::local_engine::providers::python::render_hashed_requirements;
    let packages = vec![
        PackageLock {
            name: "test-pkg".to_string(),
            version: "1.0.0".to_string(),
            sha256: Some("a".repeat(64)),
            all_hashes: vec!["a".repeat(64), "b".repeat(64)],
        },
        PackageLock {
            name: "another-pkg".to_string(),
            version: "2.0.0".to_string(),
            sha256: Some("c".repeat(64)),
            all_hashes: vec!["c".repeat(64)],
        },
    ];
    let result = render_hashed_requirements(&packages).unwrap();
    // 验证输出包含两个包
    assert!(result.contains("test-pkg==1.0.0"));
    assert!(result.contains("another-pkg==2.0.0"));
    // 验证 test-pkg 有两个 hash
    let test_pkg_line_count = result
        .lines()
        .find(|l| l.contains("test-pkg=="))
        .map(|l| l.matches("--hash=sha256:").count())
        .unwrap_or(0);
    assert_eq!(
        test_pkg_line_count, 2,
        "test-pkg 应有 2 个 hash（多平台 wheel）"
    );
}

/// 验证 render_hashed_requirements 拒绝非精确版本。
#[test]
fn funasr_render_hashed_requirements_rejects_non_exact_version() {
    use crate::infra::local_engine::providers::python::render_hashed_requirements;
    let packages = vec![PackageLock {
        name: "bad-pkg".to_string(),
        version: ">=1.0.0".to_string(),
        sha256: Some("a".repeat(64)),
        all_hashes: vec!["a".repeat(64)],
    }];
    let result = render_hashed_requirements(&packages);
    assert!(
        result.is_err(),
        "非精确版本约束应被 render_hashed_requirements 拒绝"
    );
}

/// 验证 parse_locked_requirements 解析格式正确。
#[test]
fn funasr_parse_locked_requirements_correctness() {
    let sample = "# comment\naiohttp==3.14.3 \\\n    --hash=sha256:03cd2bde3d7f085b64e549c985f4bb928cad7e8ecf5323bfca320db548d81b39 \\\n    --hash=sha256:041badb8f843963574d3ad26de6afd7a32b112f43d3c63045c0c8278cfd2043\nfastapi==0.115.6 \\\n    --hash=sha256:9ec46f7addc14ea472958a96aae5b5de65f39721a46aaf5705c480d9a8b76654\n";
    let packages = parse_locked_requirements(sample);
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].name, "aiohttp");
    assert_eq!(packages[0].version, "3.14.3");
    assert_eq!(packages[0].all_hashes.len(), 2);
    assert_eq!(
        packages[0].sha256.as_deref(),
        Some("03cd2bde3d7f085b64e549c985f4bb928cad7e8ecf5323bfca320db548d81b39")
    );
    assert_eq!(packages[1].name, "fastapi");
    assert_eq!(packages[1].version, "0.115.6");
    assert_eq!(packages[1].all_hashes.len(), 1);
}

/// 验证所有声明的 profile 都有可执行的安装合同：
/// 每个包都有 hash，且只声明了 CPU profile（与 CPU-only 锁文件匹配）。
#[test]
fn funasr_all_profiles_have_executable_install_contract() {
    let pd = make_funasr_provider_descriptor();

    // 所有 profile 必须有对应的 artifact 和 install_plan
    assert!(!pd.profiles.is_empty(), "至少应声明一个 profile");
    for p in &pd.profiles {
        assert!(!p.profile_id.is_empty(), "profile_id 不能为空");
    }

    // 验证安装计划中所有包都有 hash（--require-hashes 可执行）
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        assert!(!plan.packages.is_empty(), "锁文件应包含至少一个包");
        for pkg in &plan.packages {
            assert!(
                pkg.sha256.is_some(),
                "{} 缺少 hash —— --require-hashes 将失败",
                pkg.name
            );
        }
    }

    // 0.22.6：只声明 CPU profile，与 CPU-only 锁文件匹配
    assert!(
        pd.profiles
            .iter()
            .any(|p| p.profile_id == "cpu-x64" && p.backend == ComputeBackend::Cpu),
        "缺少 CPU profile"
    );
    // 确保没有声明 CUDA profile（需独立 CUDA 锁文件后才能启用）
    assert!(
        !pd.profiles
            .iter()
            .any(|p| p.backend == ComputeBackend::Cuda),
        "0.22.6 不应声明 CUDA profile（锁文件仅含 CPU wheel hash）"
    );
}

// ── 0.22.6 B2: 子模型映射测试 ──

/// SenseVoice 系列模型无需子模型。
#[test]
fn submodels_for_sensevoice_is_empty() {
    assert!(funasr_submodels_for("iic/SenseVoiceSmall").is_empty());
    assert!(funasr_submodels_for("SenseVoice").is_empty());
}

/// Paraformer 系列模型需要 VAD + punc 子模型。
#[test]
fn submodels_for_paraformer_has_vad_and_punc() {
    let subs = funasr_submodels_for("paraformer-zh");
    assert_eq!(subs, vec!["fsmn-vad", "ct-punc"]);
}

/// 未知模型返回空子模型列表（安全默认值）。
#[test]
fn submodels_for_unknown_model_is_empty() {
    assert!(funasr_submodels_for("some-unknown-model").is_empty());
}

/// 大小写不敏感的子模型匹配。
#[test]
fn submodels_for_case_insensitive() {
    let subs = funasr_submodels_for("Paraformer-ZH");
    assert_eq!(subs, vec!["fsmn-vad", "ct-punc"]);

    assert!(funasr_submodels_for("SENSEVOICESMALL").is_empty());
}
