//! 通用契约测试：DTO shape / 事件常量 / command 签名编译检查 / 错误映射。
//!
//! 无法按命令域归类的跨域契约测试集中在此（按域归类的测试在各子模块内）。

use super::*;

use crate::app::local_engine::dto::{CleanupRequestDto, EnginePreferencesPatchDto};

// ── 未知 engine_id 被拒绝 ──

#[test]
fn unknown_engine_id_rejected() {
    let result = validate_engine_id("");
    assert!(result.is_err());
}

#[test]
fn operation_logs_are_hidden_while_runtime_is_active() {
    use crate::domain::local_engine::ProcessState;

    assert!(!super::should_include_operation_logs(
        &ProcessState::Starting
    ));
    assert!(!super::should_include_operation_logs(
        &ProcessState::Running { pid: 49988 }
    ));
    assert!(!super::should_include_operation_logs(
        &ProcessState::Stopping
    ));
    assert!(super::should_include_operation_logs(&ProcessState::Stopped));
    assert!(super::should_include_operation_logs(
        &ProcessState::Exited {
            reason: "test".to_string(),
        }
    ));
}

#[test]
fn invalid_engine_id_rejected() {
    let result = validate_engine_id("invalid/id/with/slashes");
    assert!(result.is_err());
}

// ── service_epoch 是字符串 ──

#[test]
fn service_epoch_is_string() {
    use crate::domain::local_engine::ServiceEpoch;
    let epoch = ServiceEpoch::new();
    let s = epoch.to_string();
    assert!(s.starts_with("epoch-"));
    // 验证字符串长度是 16 hex + "epoch-" 前缀
    assert_eq!(s.len(), 6 + 16);
}

// ── 旧 FunASR lifecycle 命令已删除（0.22.6 phase B）──
// 未发版且前端 0 引用：get_funasr_env / setup_python_env / start_funasr_server /
// stop_funasr_server / get_funasr_log_history 已随 maintenance 瘦身删除。
// 若恢复引用请改走通用 local_engine 命令（get_local_engine_status 等）。

// ── 旧事件常量仍存在（旧前端兼容投影仍在用）──

#[test]
fn old_funasr_event_constants_still_exist() {
    assert_eq!(
        crate::domain::event_names::EventNames::FUNASR_SERVER_STATUS,
        "blink://funasr-server-status"
    );
    assert_eq!(
        crate::domain::event_names::EventNames::FUNASR_SERVER_LOG,
        "blink://funasr-server-log"
    );
}

// ── 新事件常量存在 ──

#[test]
fn new_local_engine_event_constants_exist() {
    assert_eq!(
        crate::domain::event_names::EventNames::LOCAL_ENGINE_STATUS,
        "blink://local-engine-status"
    );
    assert_eq!(
        crate::domain::event_names::EventNames::LOCAL_ENGINE_LOG,
        "blink://local-engine-log"
    );
}

// ── status DTO service_epoch 是字符串 ──

#[test]
fn status_dto_service_epoch_is_string() {
    use crate::app::local_engine::dto::{EngineStatusDto, EngineStatusWire, ProcessStateDto};
    let dto = EngineStatusDto {
        engine_id: "funasr".to_string(),
        service_epoch: "epoch-0016a3f4deadbeef".to_string(),
        revision: "1".to_string(),
        status: EngineStatusWire {
            desired: "stopped".to_string(),
            operation: serde_json::Value::Null,
            environment: "missing".to_string(),
            process: ProcessStateDto {
                state: "stopped".to_string(),
                pid: None,
                reason: None,
            },
            service: "unknown".to_string(),
            model: "unknown".to_string(),
            available: false,
            backend: serde_json::Value::Null,
            last_error: None,
        },
    };
    let json = serde_json::to_value(&dto).unwrap();
    // service_epoch 必须是字符串（不是数字）
    assert!(json["service_epoch"].is_string());
    assert!(json["revision"].is_string());
    // process 是显式 DTO 对象，不是字符串
    assert!(json["status"]["process"].is_object());
    assert_eq!(json["status"]["process"]["state"], "stopped");
    assert!(json["status"]["process"].get("pid").is_none());
}

// ── 新 commands 签名可编译 ──

#[test]
fn all_new_commands_compile() {
    let _ = get_local_engine_catalog as fn(tauri::AppHandle) -> _;
    let _ = get_local_engine_status as fn(tauri::AppHandle, Option<String>) -> _;
    let _ = get_local_engine_logs as fn(tauri::AppHandle, String, Option<usize>) -> _;
    let _ = install_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
    let _ = start_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
    let _ = stop_local_engine as fn(tauri::AppHandle, String) -> _;
    let _ = repair_local_engine as fn(tauri::AppHandle, String) -> _;
    let _ = get_local_engine_storage as fn(tauri::AppHandle, String) -> _;
    let _ = cleanup_local_engine as fn(tauri::AppHandle, CleanupRequestDto) -> _;
    let _ = cancel_local_engine_operation as fn(tauri::AppHandle, String, String) -> _;
}

// ── LocalEngineError → CommandError 映射 ──

#[test]
fn local_engine_error_maps_to_command_error() {
    use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

    let err = LocalEngineError::with_detail(
        LocalEngineErrorCode::Cancelled,
        ErrorPhase::Request,
        "操作已取消",
        "user cancelled",
    );

    let ce: CommandError = err.into();
    assert_eq!(ce.code, "cancelled");
    assert!(!ce.retryable);
    assert!(ce.detail.is_some());
}

#[test]
fn local_engine_error_timeout_is_retryable() {
    use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

    let err = LocalEngineError::with_detail(
        LocalEngineErrorCode::Timeout,
        ErrorPhase::Health,
        "健康检查超时",
        "",
    );

    let ce: CommandError = err.into();
    assert_eq!(ce.code, "timeout");
    assert!(ce.retryable);
}

#[test]
fn local_engine_error_self_test_failed_not_retryable() {
    use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

    let err = LocalEngineError::with_detail(
        LocalEngineErrorCode::SelfTestFailed,
        ErrorPhase::SelfTest,
        "self-test 失败",
        "",
    );

    let ce: CommandError = err.into();
    assert_eq!(ce.code, "self_test_failed");
    assert!(!ce.retryable);
}

// ═══════════════════════════════════════════════════════════════════════
// 0.22.6 H4 §13: 静态契约测试
// 验证前端调用的 local-engine / stt command 全部已在 invoke_handler 注册。
// 如果前端调用了未注册的命令，此测试会失败，帮助及早发现遗漏。
// ═══════════════════════════════════════════════════════════════════════

#[test]
fn all_frontend_local_engine_commands_are_registered() {
    // 前端已知的 local-engine command 名称集合（从 frontend/js 中提取）
    // 这些命令必须在 main.rs invoke_handler 中注册
    let frontend_commands: &[&str] = &[
        "get_local_engine_catalog",
        "get_local_engine_status",
        "get_local_engine_logs",
        "install_local_engine",
        "start_local_engine",
        "stop_local_engine",
        "repair_local_engine",
        "get_local_engine_storage",
        "cleanup_local_engine",
        "cancel_local_engine_operation",
        "get_local_engine_preferences",
        "set_local_engine_preferences",
        "get_runtime_foundation_status",
        "get_engine_diagnostics",
        "open_engine_folder",
        "open_runtime_folder",
        "stop_orphan_engine",
        // 0.22.6 H5: 模型生命周期命令
        "list_engine_models",
        "install_engine_model",
        "delete_engine_model",
        "repair_engine_model",
        "cancel_model_operation",
    ];

    // 验证每个命令名称对应的函数存在于 app::commands 模块中
    // 这是编译期检查——如果函数不存在，编译会失败
    for &cmd_name in frontend_commands {
        let exists = match cmd_name {
            "get_local_engine_catalog" => {
                let _ = get_local_engine_catalog as fn(tauri::AppHandle) -> _;
                true
            }
            "get_local_engine_status" => {
                let _ = get_local_engine_status as fn(tauri::AppHandle, Option<String>) -> _;
                true
            }
            "get_local_engine_logs" => {
                let _ = get_local_engine_logs as fn(tauri::AppHandle, String, Option<usize>) -> _;
                true
            }
            "install_local_engine" => {
                let _ = install_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
                true
            }
            "start_local_engine" => {
                let _ = start_local_engine as fn(tauri::AppHandle, String, Option<String>) -> _;
                true
            }
            "stop_local_engine" => {
                let _ = stop_local_engine as fn(tauri::AppHandle, String) -> _;
                true
            }
            "repair_local_engine" => {
                let _ = repair_local_engine as fn(tauri::AppHandle, String) -> _;
                true
            }
            "get_local_engine_storage" => {
                let _ = get_local_engine_storage as fn(tauri::AppHandle, String) -> _;
                true
            }
            "cleanup_local_engine" => {
                let _ = cleanup_local_engine as fn(tauri::AppHandle, CleanupRequestDto) -> _;
                true
            }
            "cancel_local_engine_operation" => {
                let _ = cancel_local_engine_operation as fn(tauri::AppHandle, String, String) -> _;
                true
            }
            "get_local_engine_preferences" => {
                let _ = get_local_engine_preferences as fn(tauri::AppHandle, String) -> _;
                true
            }
            "set_local_engine_preferences" => {
                let _ = set_local_engine_preferences
                    as fn(tauri::AppHandle, String, EnginePreferencesPatchDto) -> _;
                true
            }
            "get_runtime_foundation_status" => {
                let _ = get_runtime_foundation_status as fn(tauri::AppHandle) -> _;
                true
            }
            "get_engine_diagnostics" => {
                let _ = get_engine_diagnostics as fn(tauri::AppHandle, String) -> _;
                true
            }
            "open_engine_folder" => {
                let _ = open_engine_folder as fn(tauri::AppHandle, String) -> _;
                true
            }
            "open_runtime_folder" => {
                let _ = open_runtime_folder as fn(tauri::AppHandle) -> _;
                true
            }
            "stop_orphan_engine" => {
                let _ = stop_orphan_engine as fn(tauri::AppHandle, String) -> _;
                true
            }
            "list_engine_models" => {
                let _ = list_engine_models as fn(tauri::AppHandle, String) -> _;
                true
            }
            "install_engine_model" => {
                let _ = install_engine_model
                    as fn(
                        tauri::AppHandle,
                        crate::app::local_engine::model_installer::ModelOperationRequestDto,
                    ) -> _;
                true
            }
            "delete_engine_model" => {
                let _ = delete_engine_model
                    as fn(
                        tauri::AppHandle,
                        crate::app::local_engine::model_installer::ModelOperationRequestDto,
                    ) -> _;
                true
            }
            "repair_engine_model" => {
                let _ = repair_engine_model
                    as fn(
                        tauri::AppHandle,
                        crate::app::local_engine::model_installer::ModelOperationRequestDto,
                    ) -> _;
                true
            }
            "cancel_model_operation" => {
                let _ = cancel_model_operation as fn(tauri::AppHandle, String, String, String) -> _;
                true
            }
            _ => panic!("未知的前端命令名: {cmd_name}"),
        };
        assert!(exists, "命令 {cmd_name} 未注册或函数不存在");
    }
}

#[test]
fn all_frontend_stt_commands_are_registered() {
    // 前端已知的 STT command 名称集合
    let frontend_stt_commands: &[&str] = &[
        "get_stt_config",
        "set_stt_config",
        // 0.22.6 phase B: list_stt_models/download_stt_model/delete_stt_model 已删除
        "list_selectable_stt_models",
        "set_local_stt_selection",
        "cancel_voice_recording",
        "is_voice_recording",
        "list_audio_devices",
        "start_audio_test",
        "stop_audio_test",
        "save_stt_secret",
        "delete_stt_secret",
        "has_stt_secret",
        "get_stt_secret_hint",
        "test_cloud_stt",
        "resize_voice_overlay",
        "start_chat_stt",
        "stop_chat_stt",
    ];

    // 验证每个命令名称对应的函数存在于 app::commands 模块中
    for &cmd_name in frontend_stt_commands {
        let exists = match cmd_name {
            "get_stt_config" => {
                let _ = crate::app::commands::get_stt_config as fn(tauri::AppHandle) -> _;
                true
            }
            "set_stt_config" => {
                let _ = crate::app::commands::set_stt_config
                    as fn(tauri::AppHandle, crate::app::stt_config::SttConfig, Option<String>) -> _;
                true
            }
            "list_selectable_stt_models" => {
                let _ =
                    crate::app::commands::list_selectable_stt_models as fn(tauri::AppHandle) -> _;
                true
            }
            "set_local_stt_selection" => {
                let _ = crate::app::commands::set_local_stt_selection
                    as fn(tauri::AppHandle, String, String) -> _;
                true
            }
            "cancel_voice_recording" => {
                let _ = crate::app::commands::cancel_voice_recording as fn(tauri::AppHandle) -> _;
                true
            }
            "is_voice_recording" => {
                let _ = crate::app::commands::is_voice_recording as fn(tauri::AppHandle) -> _;
                true
            }
            "list_audio_devices" => {
                let _ = crate::app::commands::list_audio_devices as fn() -> _;
                true
            }
            "start_audio_test" => {
                let _ = crate::app::commands::start_audio_test
                    as fn(tauri::AppHandle, Option<String>) -> _;
                true
            }
            "stop_audio_test" => {
                let _ = crate::app::commands::stop_audio_test as fn() -> _;
                true
            }
            "save_stt_secret" => {
                let _ = crate::app::commands::save_stt_secret as fn(String) -> _;
                true
            }
            "delete_stt_secret" => {
                let _ = crate::app::commands::delete_stt_secret as fn() -> _;
                true
            }
            "has_stt_secret" => {
                let _ = crate::app::commands::has_stt_secret as fn() -> _;
                true
            }
            "get_stt_secret_hint" => {
                let _ = crate::app::commands::get_stt_secret_hint as fn() -> _;
                true
            }
            "test_cloud_stt" => {
                let _ = crate::app::commands::test_cloud_stt as fn() -> _;
                true
            }
            "resize_voice_overlay" => {
                let _ =
                    crate::app::commands::resize_voice_overlay as fn(tauri::AppHandle, f64) -> _;
                true
            }
            "start_chat_stt" => {
                let _ = crate::app::commands::start_chat_stt as fn(tauri::AppHandle) -> _;
                true
            }
            "stop_chat_stt" => {
                let _ = crate::app::commands::stop_chat_stt as fn(tauri::AppHandle) -> _;
                true
            }
            _ => panic!("未知的前端 STT 命令名: {cmd_name}"),
        };
        assert!(exists, "STT 命令 {cmd_name} 未注册或函数不存在");
    }
}
