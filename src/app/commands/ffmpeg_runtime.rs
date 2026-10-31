//! 媒体运行时安装操作：后端保留快照，窗口关闭不丢失进度。
use crate::{
    app::command_error::CommandError,
    infra::{
        event_names::EventNames,
        managed_runtime::{InstallReporter, ffmpeg},
    },
};
use serde::{Deserialize, Serialize};
use std::sync::{Arc, LazyLock, Mutex};
use tauri::Emitter;
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaRuntimeAction {
    Install,
    Repair,
    Diagnose,
    Uninstall,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaRuntimeOperation {
    id: String,
    action: MediaRuntimeAction,
    stage: String,
    downloaded: u64,
    total: Option<u64>,
    cancellable: bool,
    #[serde(skip)]
    cancel: CancellationToken,
}

#[derive(Default)]
struct OperationState {
    revision: u64,
    operation: Option<MediaRuntimeOperation>,
    last_error: Option<String>,
}
impl OperationState {
    /// 应用退出不同于 UI 取消：请求协作停止，等待在途提交/清理完成。
    fn request_shutdown(&self) -> bool {
        if let Some(operation) = &self.operation {
            operation.cancel.cancel();
            false
        } else {
            true
        }
    }
}

pub async fn shutdown_ffmpeg_runtime() {
    let _ = tokio::time::timeout(std::time::Duration::from_secs(5), async {
        loop {
            if STATE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .request_shutdown()
            {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    })
    .await;
}
static STATE: LazyLock<Arc<Mutex<OperationState>>> =
    LazyLock::new(|| Arc::new(Mutex::new(OperationState::default())));
static EPOCH: LazyLock<String> = LazyLock::new(|| {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
});

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MediaRuntimeSnapshot {
    epoch: String,
    revision: u64,
    runtime: ffmpeg::RuntimeStatus,
    operation: Option<MediaRuntimeOperation>,
    last_error: Option<String>,
}

#[tauri::command]
pub async fn get_ffmpeg_runtime_status(
    window: tauri::WebviewWindow,
) -> Result<MediaRuntimeSnapshot, CommandError> {
    ensure_settings(&window)?;
    tokio::task::spawn_blocking(|| {
        let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        Ok(MediaRuntimeSnapshot {
            epoch: EPOCH.clone(),
            revision: state.revision,
            runtime: ffmpeg::status()
                .map_err(|e| CommandError::new(e.code(), e.to_string(), true))?,
            operation: state.operation.clone(),
            last_error: state.last_error.clone(),
        })
    })
    .await
    .map_err(|e| CommandError::new("media_runtime_status", e.to_string(), true))?
}

struct Reporter {
    app: tauri::AppHandle,
    id: String,
    last_emit: Mutex<Option<std::time::Instant>>,
}
impl Reporter {
    fn change(&self, stage: Option<&str>, progress: Option<(u64, Option<u64>)>) {
        {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            let Some(op) = state.operation.as_mut().filter(|op| op.id == self.id) else {
                return;
            };
            if let Some(stage) = stage {
                op.stage = stage.into();
                op.cancellable = !matches!(stage, "promoting" | "done" | "removing");
            }
            if let Some((done, total)) = progress {
                op.downloaded = done;
                op.total = total;
            }
            state.revision += 1;
        }
        let mut last = self.last_emit.lock().unwrap_or_else(|e| e.into_inner());
        if stage.is_none() && last.is_some_and(|t| t.elapsed().as_millis() < 200) {
            return;
        }
        *last = Some(std::time::Instant::now());
        self.notify();
    }
    fn notify(&self) {
        if let Err(error) = self.app.emit(
            EventNames::MANAGED_RUNTIME_INSTALL,
            serde_json::json!({"kind": ffmpeg::KIND, "operationId": self.id}),
        ) {
            tracing::debug!(%error, "媒体运行时状态通知失败，前端可查询快照");
        }
    }
}
impl InstallReporter for Reporter {
    fn on_stage(&self, stage: &str) {
        self.change(Some(stage), None);
    }
    fn on_progress(&self, done: u64, total: Option<u64>) {
        self.change(None, Some((done, total)));
    }
    fn on_log(&self, level: &str, text: &str) {
        match level {
            "warn" => tracing::warn!(kind = ffmpeg::KIND, "{text}"),
            "error" => tracing::error!(kind = ffmpeg::KIND, "{text}"),
            _ => tracing::debug!(kind = ffmpeg::KIND, "{text}"),
        }
    }
}

#[tauri::command]
pub fn begin_ffmpeg_runtime_operation(
    app: tauri::AppHandle,
    window: tauri::WebviewWindow,
    action: MediaRuntimeAction,
) -> Result<String, CommandError> {
    ensure_settings(&window)?;
    let (id, cancel) = {
        let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
        if state.operation.is_some() {
            return Err(CommandError::new(
                "media_runtime_busy",
                "媒体运行时正在执行操作",
                true,
            ));
        }
        state.revision += 1;
        let id = format!("{}-{}", *EPOCH, state.revision);
        let cancel = CancellationToken::new();
        state.last_error = None;
        state.operation = Some(MediaRuntimeOperation {
            id: id.clone(),
            action,
            stage: "preparing".into(),
            downloaded: 0,
            total: None,
            cancellable: !matches!(action, MediaRuntimeAction::Uninstall),
            cancel: cancel.clone(),
        });
        (id, cancel)
    };
    let operation_id = id.clone();
    tauri::async_runtime::spawn(async move {
        let reporter = Arc::new(Reporter {
            app,
            id,
            last_emit: Mutex::new(None),
        });
        reporter.notify();
        tracing::info!(?action, "媒体运行时操作开始");
        let runner = reporter.clone();
        // JoinError 也必须释放操作态，不能让安装卡片永久 busy。
        let result = tokio::spawn(async move {
            match action {
                MediaRuntimeAction::Install => {
                    ffmpeg::install(false, &cancel, runner.as_ref()).await
                }
                MediaRuntimeAction::Repair => ffmpeg::install(true, &cancel, runner.as_ref()).await,
                MediaRuntimeAction::Diagnose => ffmpeg::diagnose(&cancel, runner.as_ref()).await,
                MediaRuntimeAction::Uninstall => {
                    runner.on_stage("removing");
                    tokio::task::spawn_blocking(ffmpeg::uninstall)
                        .await
                        .map_err(|e| {
                            crate::infra::managed_runtime::ManagedRuntimeError::InstallFailed(
                                e.to_string(),
                            )
                        })?
                }
            }
        })
        .await;
        let error = match result {
            Ok(Ok(())) | Ok(Err(crate::infra::managed_runtime::ManagedRuntimeError::Cancelled)) => {
                None
            }
            Ok(Err(e)) => Some(e.to_string()),
            Err(e) => Some(format!("媒体运行时任务失败: {e}")),
        };
        if let Some(error) = &error {
            tracing::warn!(?action, operation_id = %reporter.id, %error, "媒体运行时操作失败");
        }
        {
            let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
            if state
                .operation
                .as_ref()
                .is_some_and(|op| op.id == reporter.id)
            {
                state.operation = None;
                state.last_error = error;
                state.revision += 1;
            }
        }
        tracing::info!(?action, "媒体运行时操作结束");
        reporter.notify();
    });
    Ok(operation_id)
}

#[tauri::command]
pub fn cancel_ffmpeg_runtime_operation(
    window: tauri::WebviewWindow,
    operation_id: String,
) -> Result<(), CommandError> {
    ensure_settings(&window)?;
    let state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(operation) = state.operation.as_ref().filter(|op| op.id == operation_id) {
        if !operation.cancellable {
            return Err(CommandError::new(
                "media_runtime_committing",
                "正在提交，请等待操作结束",
                true,
            ));
        }
        operation.cancel.cancel();
    }
    Ok(())
}

fn ensure_settings(window: &tauri::WebviewWindow) -> Result<(), CommandError> {
    if window.label() == "settings" {
        Ok(())
    } else {
        Err(CommandError::new(
            "media_runtime_forbidden",
            "媒体运行时管理仅限设置窗口",
            false,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn shutdown_cancels_work_without_pretending_cleanup_is_finished() {
        let mut state = OperationState::default();
        assert!(state.request_shutdown());
        let cancel = CancellationToken::new();
        state.operation = Some(MediaRuntimeOperation {
            id: "one".into(),
            action: MediaRuntimeAction::Install,
            stage: "promoting".into(),
            downloaded: 1,
            total: Some(1),
            cancellable: false,
            cancel: cancel.clone(),
        });
        assert!(!state.request_shutdown());
        assert!(cancel.is_cancelled());
        assert!(
            state.operation.is_some(),
            "等 worker 回收并提交真实终态，而非先清除 busy"
        );
        state.operation = None;
        assert!(state.request_shutdown());
    }
}
