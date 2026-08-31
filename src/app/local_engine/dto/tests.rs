use super::*;
use crate::domain::local_engine::{
    DesiredState, EngineStatus, EnvironmentHealth, ModelHealth, ProcessState, ServiceHealth,
};
use crate::infra::local_engine::runtime::EngineId;
use serde_json::json;

// ── ProcessStateDto 各状态可序列化 ──────────────────────────────────────────

#[test]
fn process_state_dto_stopped_serializes() {
    let dto = ProcessStateDto {
        state: "stopped".to_string(),
        pid: None,
        reason: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["state"], "stopped");
    assert!(json.get("pid").is_none() || json["pid"].is_null());
    assert!(json.get("reason").is_none() || json["reason"].is_null());
}

#[test]
fn process_state_dto_starting_serializes() {
    let dto = ProcessStateDto {
        state: "starting".to_string(),
        pid: None,
        reason: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["state"], "starting");
}

#[test]
fn process_state_dto_running_serializes_with_pid() {
    let dto = ProcessStateDto {
        state: "running".to_string(),
        pid: Some(1234),
        reason: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["state"], "running");
    assert_eq!(json["pid"], 1234);
}

#[test]
fn process_state_dto_stopping_serializes() {
    let dto = ProcessStateDto {
        state: "stopping".to_string(),
        pid: None,
        reason: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["state"], "stopping");
}

#[test]
fn process_state_dto_exited_serializes_with_reason() {
    let dto = ProcessStateDto {
        state: "exited".to_string(),
        pid: None,
        reason: Some("exit code 1".to_string()),
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["state"], "exited");
    assert_eq!(json["reason"], "exit code 1");
}

// ── project_process_state 投影各变体 ─────────────────────────────────────────

#[test]
fn project_process_state_stopped() {
    let dto = project_process_state(&ProcessState::Stopped);
    assert_eq!(dto.state, "stopped");
    assert!(dto.pid.is_none());
    assert!(dto.reason.is_none());
}

#[test]
fn project_process_state_starting() {
    let dto = project_process_state(&ProcessState::Starting);
    assert_eq!(dto.state, "starting");
}

#[test]
fn project_process_state_running() {
    let dto = project_process_state(&ProcessState::Running { pid: 5678 });
    assert_eq!(dto.state, "running");
    assert_eq!(dto.pid, Some(5678));
}

#[test]
fn project_process_state_stopping() {
    let dto = project_process_state(&ProcessState::Stopping);
    assert_eq!(dto.state, "stopping");
}

#[test]
fn project_process_state_exited() {
    let dto = project_process_state(&ProcessState::Exited {
        reason: "crashed".to_string(),
    });
    assert_eq!(dto.state, "exited");
    assert_eq!(dto.reason, Some("crashed".to_string()));
}

// ── query 与 status event 使用同一 DTO shape ──────────────────────────────────

#[test]
fn project_status_produces_consistent_shape() {
    // 构造一个 domain EngineStatusSnapshot
    let engine_id = EngineId::new("funasr").unwrap();
    let status = EngineStatus {
        desired: DesiredState::Running,
        process: ProcessState::Running { pid: 4242 },
        service: ServiceHealth::Healthy,
        model: ModelHealth::Ready,
        environment: EnvironmentHealth::Ready,
        ..Default::default()
    };

    let snapshot = crate::domain::local_engine::EngineStatusSnapshot {
        engine_id,
        service_epoch: crate::domain::local_engine::ServiceEpoch::new(),
        revision: 1u64,
        status,
    };

    // query 路径：project_status
    let query_dto = project_status(&snapshot);
    let query_json = serde_json::to_value(&query_dto).unwrap();

    // event 路径：也调用 project_status（emit_status 内部调用同一函数）
    // 由于两者调用同一个函数，这里验证序列化 shape 一致
    let event_dto = project_status(&snapshot);
    let event_json = serde_json::to_value(&event_dto).unwrap();

    // 两者完全相同
    assert_eq!(query_json, event_json);

    // service_epoch 是字符串
    assert!(query_json["service_epoch"].is_string());
    assert!(
        query_json["service_epoch"]
            .as_str()
            .unwrap()
            .starts_with("epoch-")
    );

    // revision 是字符串
    assert!(query_json["revision"].is_string());
    assert_eq!(query_json["revision"], "1");

    // process 是显式 DTO 对象
    assert!(query_json["status"]["process"].is_object());
    assert_eq!(query_json["status"]["process"]["state"], "running");
    assert_eq!(query_json["status"]["process"]["pid"], 4242);
}

// ── service_epoch/revision 是字符串（不是数字）────────────────────────────

#[test]
fn engine_status_dto_service_epoch_revision_are_strings() {
    let dto = EngineStatusDto {
        engine_id: "funasr".to_string(),
        service_epoch: "epoch-abc123".to_string(),
        revision: "42".to_string(),
        status: EngineStatusWire {
            desired: "stopped".to_string(),
            operation: json!(null),
            environment: "missing".to_string(),
            process: ProcessStateDto {
                state: "stopped".to_string(),
                pid: None,
                reason: None,
            },
            service: "unknown".to_string(),
            model: "unknown".to_string(),
            available: false,
            backend: json!(null),
            last_error: None,
        },
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert!(json["service_epoch"].is_string());
    assert!(json["revision"].is_string());
    assert_eq!(json["service_epoch"], "epoch-abc123");
    assert_eq!(json["revision"], "42");
}

// ── EngineLogDto seq 是字符串 ──────────────────────────────────────────────

#[test]
fn engine_log_dto_seq_is_string() {
    let dto = EngineLogDto {
        engine_id: "funasr".to_string(),
        instance_id: "inst-abc".to_string(),
        operation_id: None,
        seq: "12345".to_string(),
        timestamp: "2026-08-26T00:00:00Z".to_string(),
        level: EngineLogLevel::Info,
        text: "test log".to_string(),
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert!(json["seq"].is_string());
    assert_eq!(json["seq"], "12345");
}

// ── ProcessStateDto 前端不会遇到字符串裸值 ──────────────────────────────────

#[test]
fn process_state_dto_never_serializes_as_bare_string() {
    // 旧 ProcessState enum 用 #[serde(rename_all = "snake_case")]，
    // Stopped/Starting/Stopping 序列化为裸字符串 "stopped"。
    // ProcessStateDto 必须序列化为对象 { "state": "stopped" }。
    let dto = ProcessStateDto {
        state: "stopped".to_string(),
        pid: None,
        reason: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    // 必须是对象，不是字符串
    assert!(json.is_object());
    // 前端可以安全执行 process.state，不会抛 TypeError
    assert_eq!(json["state"], "stopped");
}

// ── ProcessStateDto 可反序列化（round-trip）────────────────────────────────

#[test]
fn process_state_dto_round_trip() {
    let original = ProcessStateDto {
        state: "running".to_string(),
        pid: Some(9999),
        reason: None,
    };
    let json = serde_json::to_string(&original).unwrap();
    let deserialized: ProcessStateDto = serde_json::from_str(&json).unwrap();
    assert_eq!(deserialized.state, "running");
    assert_eq!(deserialized.pid, Some(9999));
}

// ── EnginePreferencesPatchDto deny_unknown_fields ──────────────────────────

#[test]
fn patch_dto_accepts_known_fields() {
    let json = json!({
        "compute_preference": "cpu",
        "auto_start": true,
        "lifecycle": "on_demand"
    });
    let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
    assert_eq!(dto.compute_preference, Some("cpu".to_string()));
    assert_eq!(dto.auto_start, Some(true));
    assert_eq!(dto.lifecycle, Some("on_demand".to_string()));
}

#[test]
fn patch_dto_accepts_partial_fields() {
    let json = json!({"compute_preference": "cuda"});
    let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
    assert_eq!(dto.compute_preference, Some("cuda".to_string()));
    assert!(dto.auto_start.is_none());
    assert!(dto.lifecycle.is_none());
}

#[test]
fn patch_dto_accepts_empty_object() {
    let json = json!({});
    let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
    assert!(dto.compute_preference.is_none());
    assert!(dto.auto_start.is_none());
    assert!(dto.lifecycle.is_none());
}

#[test]
fn patch_dto_rejects_unknown_fields() {
    let json = json!({
        "compute_preference": "cpu",
        "executable": "/bin/evil",
        "argv": ["--malicious"],
        "env": {"SECRET": "leaked"}
    });
    let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
    assert!(
        result.is_err(),
        "deny_unknown_fields should reject unknown fields"
    );
}

#[test]
fn patch_dto_rejects_engine_config_injection() {
    // 前端不应能注入 engine_config
    let json = json!({
        "compute_preference": "cpu",
        "engine_config": {"port": 9999, "token": "evil"}
    });
    let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
    assert!(result.is_err());
}

#[test]
fn patch_dto_rejects_script_path_injection() {
    let json = json!({
        "compute_preference": "cpu",
        "script_path": "/etc/passwd"
    });
    let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
    assert!(result.is_err());
}

// ── EnginePreferencesDto serialization ───────────────────────────────────

#[test]
fn preferences_dto_funasr_shape() {
    let dto = EnginePreferencesDto {
        engine_id: "funasr".to_string(),
        compute_preference: Some("cpu".to_string()),
        auto_start: Some(true),
        ocr_backend: None,
        lifecycle: None,
        requires_rebuild: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["engine_id"], "funasr");
    assert_eq!(json["compute_preference"], "cpu");
    assert_eq!(json["auto_start"], true);
    // lifecycle 和 requires_rebuild 被 skip_serializing_if 跳过
    assert!(json.get("lifecycle").is_none() || json["lifecycle"].is_null());
    assert!(json.get("requires_rebuild").is_none() || json["requires_rebuild"].is_null());
}

#[test]
fn preferences_dto_paddleocr_shape() {
    let dto = EnginePreferencesDto {
        engine_id: "paddleocr".to_string(),
        compute_preference: Some("auto".to_string()),
        auto_start: None,
        ocr_backend: Some("paddleocr".to_string()),
        lifecycle: Some("on_demand".to_string()),
        requires_rebuild: Some(true),
    };
    let json = serde_json::to_value(&dto).unwrap();
    assert_eq!(json["engine_id"], "paddleocr");
    assert_eq!(json["compute_preference"], "auto");
    // auto_start 为 None 被 skip
    assert!(json.get("auto_start").is_none() || json["auto_start"].is_null());
    assert_eq!(json["ocr_backend"], "paddleocr");
    assert_eq!(json["lifecycle"], "on_demand");
    assert_eq!(json["requires_rebuild"], true);
}

#[test]
fn preferences_dto_does_not_expose_internals() {
    let dto = EnginePreferencesDto {
        engine_id: "funasr".to_string(),
        compute_preference: Some("cpu".to_string()),
        auto_start: Some(true),
        ocr_backend: None,
        lifecycle: None,
        requires_rebuild: None,
    };
    let json = serde_json::to_value(&dto).unwrap();
    // 不包含 executable / argv / env / path / url / token
    assert!(json.get("executable").is_none());
    assert!(json.get("argv").is_none());
    assert!(json.get("env").is_none());
    assert!(json.get("engine_config").is_none());
    assert!(json.get("file_path").is_none());
    assert!(json.get("script_path").is_none());
    assert!(json.get("artifact_url").is_none());
    assert!(json.get("token").is_none());
    assert!(json.get("endpoint").is_none());
}
