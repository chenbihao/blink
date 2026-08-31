//! Tauri EventPort 实现（0.22.3）。
//!
//! 把 `EngineManager` 的通用事件投影为 Tauri emit：
//! - `blink://local-engine-status` — 通用引擎状态快照
//! - `blink://local-engine-log` — 通用引擎日志条目
//!
//! ## 分层归属
//!
//! - 本模块在 `app` 层，持有 `AppHandle`，发送 Tauri 事件。
//! - `domain`/`infra` 层不依赖本模块。

use std::sync::Arc;

use tauri::{AppHandle, Emitter};

use crate::domain::event_names::EventNames;
use crate::domain::local_engine::EngineStatusSnapshot;
use crate::infra::local_engine::runtime::EngineId;

use super::EventPort;
use super::dto::{EngineLogDto, EngineLogLevel, EngineStatusDto, project_status};
use super::operation_log_store::{OperationLogEntry, OperationLogStore};

/// Tauri 事件投影出口。
///
/// 持有 `AppHandle`，把 service 产生的通用事件 emit 到前端。
///
/// 安装日志（`emit_install_log`）同时写入共享 `OperationLogStore`，
/// 供 IPC command 会话内回放。
pub struct TauriEventPort {
    app: AppHandle,
    /// 会话内 operation 日志存储——与实时事件同时写入。
    operation_log_store: Arc<OperationLogStore>,
}

impl TauriEventPort {
    /// 创建 TauriEventPort。
    ///
    /// `operation_log_store` 由 app 层构造为 managed state，
    /// 在安装日志路径同时写入 store 和实时 Tauri 事件。
    pub fn new(app: AppHandle, operation_log_store: Arc<OperationLogStore>) -> Self {
        Self {
            app,
            operation_log_store,
        }
    }
}

impl EventPort for TauriEventPort {
    /// 广播引擎状态快照。
    ///
    /// 通用事件 `blink://local-engine-status` payload 与 `get_local_engine_status`
    /// command 返回的 `EngineStatusDto` **完全相同**——两者调用同一个 `project_status`
    /// 投影函数，前端只维护一套解析器。
    fn emit_status(&self, snapshot: &EngineStatusSnapshot) {
        let payload: EngineStatusDto = project_status(snapshot);
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_STATUS, payload);
    }

    /// 广播引擎日志条目（运行时日志，以 `instance_id` 隔离）。
    ///
    /// 通用事件 `blink://local-engine-log` payload 与 `get_local_engine_logs`
    /// command 返回的 `EngineLogDto` **完全相同**——`seq` 字符串化，
    /// 前端历史与实时事件使用同一去重逻辑。
    fn emit_log(
        &self,
        engine_id: &EngineId,
        instance_id: &str,
        seq: u64,
        level: EngineLogLevel,
        line: &str,
    ) {
        let payload = EngineLogDto {
            engine_id: engine_id.as_str().to_string(),
            instance_id: instance_id.to_string(),
            operation_id: None,
            seq: seq.to_string(),
            timestamp: chrono::Utc::now().to_rfc3339(),
            level,
            text: line.to_string(),
        };
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_LOG, payload);
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
        level: EngineLogLevel,
        text: &str,
    ) {
        let timestamp = chrono::Utc::now().to_rfc3339();
        let payload = EngineLogDto {
            engine_id: engine_id.as_str().to_string(),
            instance_id: String::new(),
            operation_id: Some(operation_id.to_string()),
            seq: seq.to_string(),
            timestamp: timestamp.clone(),
            level,
            text: text.to_string(),
        };
        let _ = self.app.emit(EventNames::LOCAL_ENGINE_LOG, payload);

        // 同时写入会话内 operation 日志存储
        self.operation_log_store.append(OperationLogEntry {
            engine_id: engine_id.to_string(),
            operation_id: operation_id.to_string(),
            seq,
            timestamp,
            level: level.to_string(),
            text: text.to_string(),
        });
    }

    /// 广播安装阶段变更（0.22.6 H4）。
    ///
    /// 前端通过 `blink://local-engine-install-stage` 事件实时显示安装进度。
    fn emit_install_stage(&self, engine_id: &EngineId, operation_id: &str, stage: &str) {
        let payload = serde_json::json!({
            "engine_id": engine_id.as_str(),
            "operation_id": operation_id,
            "stage": stage,
        });
        let _ = self
            .app
            .emit(EventNames::LOCAL_ENGINE_INSTALL_STAGE, payload);
    }
}

/// 工厂函数——创建 `Arc<dyn EventPort>`。
///
/// `operation_log_store` 由 app 层构造为 managed state，
/// 在安装日志路径同时写入 store 和实时 Tauri 事件。
pub fn make_event_port(
    app: AppHandle,
    operation_log_store: Arc<OperationLogStore>,
) -> Arc<dyn EventPort> {
    Arc::new(TauriEventPort::new(app, operation_log_store))
}
