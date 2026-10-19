//! 托管 Everything 运行时 command（0.26.14）。
//!
//! 设置页「内置搜索引擎」区块的入口：安装（按需下载受管 1.4 便携版）、
//! 状态、启停实例、显式 UAC 安装 Everything 服务、卸载。安装进度经
//! `EventNames::MANAGED_RUNTIME_INSTALL` 事件实时推送（与脚本解释器同构）。

use crate::app::command_error::CommandError;
use crate::infra::event_names::EventNames;
use crate::domain::search::everything_runtime::TEST_START_BUDGET;
use crate::infra::managed_runtime::{self, KIND_EVERYTHING};
use tauri::Manager;

/// 进度事件节流下限（与脚本解释器侧一致）。
const PROGRESS_EMIT_MIN_INTERVAL_MS: u64 = 200;

/// 事件桥 reporter：把 stage/log/progress 转为前端事件（kind 固定 everything）。
struct EventReporter {
    app: tauri::AppHandle,
    last_progress_ms: std::sync::Mutex<Option<u64>>,
}

impl EventReporter {
    fn emit(&self, payload: serde_json::Value) {
        use tauri::Emitter;
        if let Err(e) = self.app.emit(EventNames::MANAGED_RUNTIME_INSTALL, payload) {
            tracing::warn!(error = %e, "托管运行时安装事件发送失败");
        }
    }
}

impl managed_runtime::InstallReporter for EventReporter {
    fn on_stage(&self, stage: &str) {
        self.emit(serde_json::json!({ "kind": KIND_EVERYTHING, "stage": stage }));
    }

    fn on_log(&self, level: &str, text: &str) {
        match level {
            "warn" => tracing::warn!(kind = KIND_EVERYTHING, "{text}"),
            "error" => tracing::error!(kind = KIND_EVERYTHING, "{text}"),
            _ => tracing::info!(kind = KIND_EVERYTHING, "{text}"),
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
            "kind": KIND_EVERYTHING,
            "stage": "downloading",
            "downloaded": downloaded,
            "total": total,
        }));
    }
}

/// 安装托管 Everything（幂等；进度经事件推送）。安装完成不自动启动——
/// 实例由首次文件搜索懒启动或设置页「启动并测试」触发。
#[tauri::command]
pub async fn install_managed_everything(
    app: tauri::AppHandle,
) -> Result<(), CommandError> {
    let reporter = EventReporter {
        app: app.clone(),
        last_progress_ms: std::sync::Mutex::new(None),
    };
    match managed_runtime::install(KIND_EVERYTHING, None, &reporter).await {
        Ok(()) => Ok(()),
        Err(e) => {
            reporter.emit(serde_json::json!({
                "kind": KIND_EVERYTHING,
                "stage": "failed",
                "message": e.to_string(),
            }));
            tracing::warn!(error = %e, "托管 Everything 安装失败");
            Err(CommandError::new(e.code(), e.to_string(), true))
        }
    }
}

/// 停止托管实例（卸载前/服务安装后重启用）。幂等。
#[tauri::command]
pub async fn stop_managed_everything(app: tauri::AppHandle) -> Result<(), CommandError> {
    if let Some(rt) = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|ss| ss.everything_runtime())
    {
        rt.stop().await;
    }
    Ok(())
}

/// 启动托管实例并等待就绪（设置页「启动并测试」，长预算）。
/// 返回就绪端口；未安装/超时返回结构化错误。
#[tauri::command]
pub async fn start_managed_everything(
    app: tauri::AppHandle,
) -> Result<u16, CommandError> {
    let Some(rt) = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|ss| ss.everything_runtime())
    else {
        return Err(CommandError::new(
            "managed_runtime_not_ready",
            "搜索引擎尚未初始化",
            false,
        ));
    };
    let cfg = {
        let pool = &app.state::<crate::infra::data::DbPools>().config;
        crate::app::config::get_file_search_config(pool).await
    };
    rt.ensure_started(cfg.managed_everything_port, TEST_START_BUDGET)
        .await
        .map_err(|e| CommandError::new("managed_runtime_start", e.to_string(), true))
}

/// 卸载托管 Everything（先停实例再删目录；占用删除失败如实报错）。
#[tauri::command]
pub async fn uninstall_managed_everything(app: tauri::AppHandle) -> Result<(), CommandError> {
    if let Some(rt) = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|ss| ss.everything_runtime())
    {
        rt.stop().await;
    }
    managed_runtime::uninstall(KIND_EVERYTHING)
        .map_err(|e| CommandError::new(e.code(), e.to_string(), true))
}

/// 显式 UAC 安装 Everything 服务（实时 NTFS 索引；拒绝则保持慢扫）。
///
/// 只发起提权请求（runas）；用户在 UAC 框中同意后 Everything 自行安装服务。
/// 返回 Ok 仅表示提权请求已受理，服务安装结果不据此断言——安装后需重启
/// 托管实例以使用服务（前端提示 + 「重启实例」入口）。
#[tauri::command]
pub fn install_everything_service() -> Result<(), CommandError> {
    let Some(exe) = managed_runtime::resolve(KIND_EVERYTHING) else {
        return Err(CommandError::new(
            "managed_runtime_not_installed",
            "请先安装内置搜索引擎",
            false,
        ));
    };
    let args = format!(
        "-instance {} -install-service",
        crate::domain::search::everything_runtime::INSTANCE_NAME
    );
    crate::infra::platform::shell::run_elevated(&exe, &args).map_err(|e| {
        // 用户取消 UAC 通常表现为 code=5：如实提示，不视为系统错误。
        CommandError::new("managed_runtime_uac", e, true)
    })
}

/// 启动恢复：清扫托管运行时孤儿 staging（main.rs 启动序列调用，不走 IPC）。
pub fn managed_runtime_startup_sweep() {
    std::thread::spawn(|| {
        let cleaned = managed_runtime::sweep_staging();
        if cleaned > 0 {
            tracing::info!(count = cleaned, "启动清扫：托管运行时孤儿 staging");
        }
    });
}
