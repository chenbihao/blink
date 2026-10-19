//! 托管 Everything 运行时 command（0.26.14）。
//!
//! 设置页「内置搜索引擎」区块的入口：安装（按需下载受管 1.4 便携版）、
//! 状态、启停实例、显式 UAC 安装 Everything 服务、卸载。安装进度经
//! `EventNames::MANAGED_RUNTIME_INSTALL` 事件实时推送（与脚本解释器同构）。

use crate::app::command_error::CommandError;
use crate::domain::search::everything_runtime::{DEFAULT_MANAGED_PORT, TEST_START_BUDGET};
use crate::infra::event_names::EventNames;
use crate::infra::managed_runtime::{self, KIND_EVERYTHING};
use tauri::Manager;

static RUNTIME_OPERATION: std::sync::LazyLock<std::sync::Arc<tokio::sync::Mutex<()>>> =
    std::sync::LazyLock::new(|| std::sync::Arc::new(tokio::sync::Mutex::new(())));
fn operation_guard() -> Result<tokio::sync::OwnedMutexGuard<()>, CommandError> {
    RUNTIME_OPERATION.clone().try_lock_owned().map_err(|_| {
        CommandError::new(
            "managed_runtime_busy",
            "正在执行运行时操作，请稍后重试",
            true,
        )
    })
}

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
pub async fn install_managed_everything(app: tauri::AppHandle) -> Result<(), CommandError> {
    let _operation = operation_guard()?;
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
    let _operation = operation_guard()?;
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
pub async fn start_managed_everything(app: tauri::AppHandle) -> Result<u16, CommandError> {
    let _operation = operation_guard()?;
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
    rt.resume();
    rt.ensure_started(DEFAULT_MANAGED_PORT, TEST_START_BUDGET)
        .await
        .map_err(|e| CommandError::new("managed_runtime_start", e.to_string(), true))
}

/// 卸载托管 Everything（先停实例再删目录；占用删除失败如实报错）。
#[tauri::command]
pub async fn uninstall_managed_everything(app: tauri::AppHandle) -> Result<(), CommandError> {
    let _operation = operation_guard()?;
    if let Some(rt) = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|ss| ss.everything_runtime())
    {
        rt.stop().await;
    }
    if let Some(exe) = managed_runtime::resolve(KIND_EVERYTHING) {
        tokio::task::spawn_blocking(move || -> Result<(), String> {
            if crate::infra::platform::shell::managed_everything_service_installed(&exe)? {
                crate::infra::platform::shell::run_elevated_wait(
                    &exe,
                    "-instance blink -uninstall-service",
                )?;
                for _ in 0..30 {
                    if !crate::infra::platform::shell::managed_everything_service_installed(&exe)? {
                        return Ok(());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                return Err("服务仍占用运行时，保留安装文件，请稍后重试".to_string());
            }
            Ok(())
        })
        .await
        .map_err(|e| CommandError::new("managed_runtime_service", e.to_string(), true))?
        .map_err(|e| CommandError::new("managed_runtime_service", e, true))?;
    }
    tokio::task::spawn_blocking(|| managed_runtime::uninstall(KIND_EVERYTHING))
        .await
        .map_err(|e| CommandError::new("managed_runtime_io", e.to_string(), true))?
        .map_err(|e| CommandError::new(e.code(), e.to_string(), true))
}

/// 安装仅属于 Blink 的索引服务；在阻塞线程等待结果，不阻塞窗口线程。
#[tauri::command]
pub async fn install_everything_service(app: tauri::AppHandle) -> Result<(), CommandError> {
    let _operation = operation_guard()?;
    let exe = managed_runtime::resolve(KIND_EVERYTHING).ok_or_else(|| {
        CommandError::new(
            "managed_runtime_not_installed",
            "请先安装 Everything",
            false,
        )
    })?;
    let rt = app
        .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
        .and_then(|ss| ss.everything_runtime());
    if let Some(rt) = &rt {
        rt.stop().await;
    }
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        crate::infra::platform::shell::managed_everything_service_installed(&exe)?;
        crate::infra::platform::shell::run_elevated_wait(&exe, "-instance blink -install-service")?;
        if !crate::infra::platform::shell::managed_everything_service_installed(&exe)? {
            return Err("安装程序已退出，但未检测到索引服务".to_string());
        }
        Ok(())
    })
    .await
    .map_err(|e| CommandError::new("managed_runtime_service", e.to_string(), true))?
    .map_err(|e| CommandError::new("managed_runtime_service", e, true))?;
    if let Some(rt) = rt {
        rt.resume();
        rt.ensure_started(DEFAULT_MANAGED_PORT, TEST_START_BUDGET)
            .await
            .map_err(|e| CommandError::new("managed_runtime_start", e.to_string(), true))?;
    }
    Ok(())
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
