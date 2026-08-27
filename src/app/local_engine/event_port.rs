//! Tauri EventPort 实现（0.22.3）。
//!
//! 把 `LocalEngineService` 的通用事件投影为 Tauri emit：
//! - `blink://local-engine-status` — 通用引擎状态快照
//! - `blink://local-engine-log` — 通用引擎日志条目
//!
//! ## 旧 FunASR 兼容投影
//!
//! 旧前端仍依赖 `blink://funasr-server-status` 和 `blink://funasr-server-log` 事件。
//! 在 app 层做临时兼容投影——从通用 status/log 事件派生旧事件，禁止启动第二套
//! polling/state producer。
//!
//! ## 分层归属
//!
//! - 本模块在 `app` 层，持有 `AppHandle`，发送 Tauri 事件。
//! - `domain`/`infra` 层不依赖本模块。

use std::sync::Arc;

use tauri::{AppHandle, Emitter};

use crate::domain::event_names::EventNames;
use crate::domain::local_engine::{
    EngineStatus, EngineStatusSnapshot, ModelHealth, ProcessState, ServiceHealth,
};
use crate::infra::local_engine::runtime::EngineId;

use super::dto::{EngineLogDto, EngineStatusDto, project_status};
use super::service::EventPort;

/// Tauri 事件投影出口。
///
/// 持有 `AppHandle`，把 service 产生的通用事件 emit 到前端。
/// 同时做旧 FunASR 兼容投影——禁止启动第二套 polling/state producer。
pub struct TauriEventPort {
    app: AppHandle,
}

impl TauriEventPort {
    /// 创建 TauriEventPort。
    pub fn new(app: AppHandle) -> Self {
        Self { app }
    }
}

impl EventPort for TauriEventPort {
    /// 广播引擎状态快照。
    ///
    /// 通用事件 `blink://local-engine-status` payload 与 `get_local_engine_status`
    /// command 返回的 `EngineStatusDto` **完全相同**——两者调用同一个 `project_status`
    /// 投影函数，前端只维护一套解析器。
    ///
    /// 兼容投影：如果 engine_id 是 funasr，额外 emit 旧
    /// `blink://funasr-server-status` 事件，payload 格式 `{ stage, ... }`。
    fn emit_status(&self, snapshot: &EngineStatusSnapshot) {
        // ── 通用事件：与 query 使用同一 DTO shape ──
        let payload: EngineStatusDto = project_status(snapshot);
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_STATUS, payload);

        // ── 旧 FunASR 兼容投影 ──
        if snapshot.engine_id.as_str() == crate::app::local_engine::funasr::FUNASR_ENGINE_ID {
            let compat = project_funasr_status(&snapshot.status);
            let _ = self.app.emit(EventNames::FUNASR_SERVER_STATUS, compat);
        }
    }

    /// 广播引擎日志条目（运行时日志，以 `instance_id` 隔离）。
    ///
    /// 通用事件 `blink://local-engine-log` payload 与 `get_local_engine_logs`
    /// command 返回的 `EngineLogDto` **完全相同**——`seq` 字符串化，
    /// 前端历史与实时事件使用同一去重逻辑。
    ///
    /// 兼容投影：如果 engine_id 是 funasr，额外 emit 旧
    /// `blink://funasr-server-log` 事件，payload 格式 `{ line }`。
    fn emit_log(&self, engine_id: &EngineId, instance_id: &str, seq: u64, line: &str) {
        // ── 通用事件：与 query 使用同一 DTO shape ──
        let payload = EngineLogDto {
            engine_id: engine_id.as_str().to_string(),
            instance_id: instance_id.to_string(),
            operation_id: None,
            seq: seq.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: "info".to_string(),
            text: line.to_string(),
        };
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_LOG, payload);

        // ── 旧 FunASR 兼容投影 ──
        if engine_id.as_str() == crate::app::local_engine::funasr::FUNASR_ENGINE_ID {
            let _ = self.app.emit(
                EventNames::FUNASR_SERVER_LOG,
                serde_json::json!({ "line": line }),
            );
        }
    }

    /// 广播安装日志条目（安装时日志，以 `operation_id` 隔离）。
    ///
    /// 与运行时日志共用 `blink://local-engine-log` 事件，
    /// payload 中的 `operation_id` 字段区分安装日志。
    /// `instance_id` 为空字符串，前端可通过 `operation_id` 过滤安装日志。
    fn emit_install_log(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        seq: u64,
        level: &str,
        text: &str,
    ) {
        let payload = EngineLogDto {
            engine_id: engine_id.as_str().to_string(),
            instance_id: String::new(),
            operation_id: Some(operation_id.to_string()),
            seq: seq.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level: level.to_string(),
            text: text.to_string(),
        };
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_LOG, payload);
    }
}

/// 从通用 `EngineStatus` 投影为旧 `blink://funasr-server-status` payload。
///
/// 旧前端期望的 stage 值：
/// - `"starting"` — process=Starting
/// - `"ready"` — service=Healthy && model=Ready
/// - `"error"` — last_error 存在
/// - `"loading"` — service=Healthy && model=Loading
/// - `"stopped"` — process=Stopped
///
/// **Task G**: 固定 shape——每次投影都包含 `stage` 字段，
/// `ready`/`loading` 时附带 `model`（从 SttConfig 读取），
/// `error` 时附带 `error`。
fn project_funasr_status(status: &EngineStatus) -> serde_json::Value {
    let stage = match (&status.process, &status.service, &status.model) {
        (ProcessState::Starting, _, _) => "starting",
        (ProcessState::Stopped, _, _) => "stopped",
        (ProcessState::Exited { .. }, _, _) => "error",
        (_, ServiceHealth::Healthy, ModelHealth::Ready) => "ready",
        (_, ServiceHealth::Healthy, ModelHealth::Loading | ModelHealth::Downloading) => "loading",
        (_, ServiceHealth::Unreachable, _) => "error",
        (_, ServiceHealth::Degraded, _) => "error",
        _ => "starting",
    };

    let mut payload = serde_json::json!({
        "stage": stage,
    });

    // ready/loading 时附带 model 信息（旧前端 voice.js 期望显示模型名）
    if stage == "ready" || stage == "loading" {
        let config = crate::app::stt_config::get_stt_config();
        payload["model"] = serde_json::Value::String(config.local_engine.funasr_model.clone());
        payload["port"] =
            serde_json::Value::Number(serde_json::Number::from(config.local_engine.server_port));
    }

    if stage == "error" {
        if let Some(ref err) = status.last_error {
            payload["error"] = serde_json::Value::String(err.to_string());
        }
    }

    payload
}

/// 工厂函数——创建 `Arc<dyn EventPort>`。
pub fn make_event_port(app: AppHandle) -> Arc<dyn EventPort> {
    Arc::new(TauriEventPort::new(app))
}
