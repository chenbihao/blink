//! 托管脚本解释器 command（0.25.20）。
//!
//! Python / Node 解释器由 Blink 托管分发：版本编译期锁定
//! （resources/script-interpreters/asset-lock.json）、安装到
//! `runtimes/script_interpreters/`、与用户系统 PATH 隔离。
//! 本模块暴露设置页所需的安装/状态/卸载入口；安装进度经
//! `EventNames::SCRIPT_INTERPRETER_INSTALL` 事件实时推送。

use crate::app::command_error::CommandError;
use crate::infra::event_names::EventNames;
use crate::infra::script_interpreter::{
    self, InstallReporter, ScriptInterpreterKind, ScriptInterpreterStatus,
};

/// 进度事件节流下限（同 infra 侧约定 ≥200ms 一条；本侧再兜底防抖动）。
const PROGRESS_EMIT_MIN_INTERVAL_MS: u64 = 200;

/// 全部托管解释器的安装状态（只读，无副作用）。
#[tauri::command]
pub fn script_interpreters_status() -> Vec<ScriptInterpreterStatus> {
    script_interpreter::status()
}

/// 事件桥 reporter：把 stage/log/progress 转为前端事件。
struct EventReporter {
    app: tauri::AppHandle,
    kind: &'static str,
    last_progress_ms: std::sync::Mutex<Option<u64>>,
}

impl EventReporter {
    fn emit(&self, payload: serde_json::Value) {
        use tauri::Emitter;
        if let Err(e) = self.app.emit(EventNames::SCRIPT_INTERPRETER_INSTALL, payload) {
            tracing::warn!(error = %e, "脚本解释器安装事件发送失败");
        }
    }
}

impl InstallReporter for EventReporter {
    fn on_stage(&self, stage: &str) {
        self.emit(serde_json::json!({ "kind": self.kind, "stage": stage }));
    }

    fn on_log(&self, level: &str, text: &str) {
        // 安装日志走 tracing（等级按内容前缀映射），不逐条轰事件——
        // 关键节点（done/failed）已由 stage 事件表达。
        match level {
            "warn" => tracing::warn!(kind = self.kind, "{text}"),
            "error" => tracing::error!(kind = self.kind, "{text}"),
            _ => tracing::info!(kind = self.kind, "{text}"),
        }
    }

    fn on_progress(&self, downloaded: u64, total: Option<u64>) {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);
        {
            let mut last = self.last_progress_ms.lock().expect("进度节流锁中毒");
            if let Some(t) = *last
                && now.saturating_sub(t) < PROGRESS_EMIT_MIN_INTERVAL_MS
            {
                return;
            }
            *last = Some(now);
        }
        self.emit(serde_json::json!({
            "kind": self.kind,
            "stage": "downloading",
            "downloaded": downloaded,
            "total": total,
        }));
    }
}

/// 安装托管解释器（幂等；进度经事件推送）。
///
/// command 本身会 await 安装完成（下载 11-34MB，视网络数秒到数分钟）；
/// 前端并行监听进度事件更新 UI，不依赖本返回值渲染中间态。
#[tauri::command]
pub async fn install_script_interpreter(
    app: tauri::AppHandle,
    kind: String,
) -> Result<(), CommandError> {
    let kind = ScriptInterpreterKind::parse(&kind)
        .map_err(|e| CommandError::new("script_interpreter_unknown_kind", e.to_string(), false))?;
    let kind_name = kind.as_str();
    let reporter = EventReporter {
        app: app.clone(),
        kind: kind_name,
        last_progress_ms: std::sync::Mutex::new(None),
    };
    match script_interpreter::install(kind, None, &reporter).await {
        Ok(()) => Ok(()),
        Err(e) => {
            reporter.emit(serde_json::json!({
                "kind": kind_name,
                "stage": "failed",
                "message": e.to_string(),
            }));
            tracing::warn!(kind = kind_name, error = %e, "脚本解释器安装失败");
            Err(CommandError::new(e.code(), e.to_string(), true))
        }
    }
}

/// 卸载托管解释器（解释器被插件进程占用时失败，提示重启后重试）。
#[tauri::command]
pub fn uninstall_script_interpreter(kind: String) -> Result<(), CommandError> {
    let kind = ScriptInterpreterKind::parse(&kind)
        .map_err(|e| CommandError::new("script_interpreter_unknown_kind", e.to_string(), false))?;
    script_interpreter::uninstall(kind)
        .map_err(|e| CommandError::new(e.code(), e.to_string(), true))
}

/// 启动恢复：清扫孤儿 staging（显式恢复动作，异步不阻塞主链路）。
///
/// 由 main.rs 启动序列调用（不走 IPC）。
pub fn startup_sweep() {
    std::thread::spawn(|| {
        let cleaned = script_interpreter::sweep_staging();
        if cleaned > 0 {
            tracing::info!(count = cleaned, "启动清扫：脚本解释器孤儿 staging");
        }
    });
}
