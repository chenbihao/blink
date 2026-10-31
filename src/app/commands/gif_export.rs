//! 录制编辑器的双格式导出：冻结毫秒快照，共用任务身份/限流/取消/终态。
use crate::app::{
    command_error::CommandError,
    gif_service,
    media_service::{self, Job, JobSnapshot},
};
use blink_infra::{
    event_names::EventNames,
    gif as infra_gif,
    managed_runtime::{ManagedRuntimeError, ffmpeg},
    media,
};
use std::{
    path::PathBuf,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};
use tauri::{AppHandle, Emitter};

#[derive(Clone, Copy, Debug, serde::Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum ExportFormat {
    Gif,
    Mp4,
}
impl ExportFormat {
    fn extension(self) -> &'static str {
        match self {
            Self::Gif => "gif",
            Self::Mp4 => "mp4",
        }
    }
}
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportRequest {
    pub session_id: String,
    pub path: String,
    pub format: ExportFormat,
    pub max_width: Option<u32>,
    pub quality: Option<String>,
    pub overwrite: Option<bool>,
}
fn media_error(e: media::MediaError) -> CommandError {
    let retryable = matches!(
        e,
        media::MediaError::Busy
            | media::MediaError::Timeout
            | media::MediaError::Process(_)
            | media::MediaError::Io(_)
            | media::MediaError::Verify(_)
    );
    CommandError::new(e.code(), e.to_string(), retryable)
}
fn require_editor(window: &tauri::WebviewWindow) -> Result<(), CommandError> {
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "此操作仅用于录制编辑器",
            false,
        ));
    }
    Ok(())
}
fn validate(request: &ExportRequest) -> Result<(), CommandError> {
    let path = std::path::Path::new(&request.path);
    if !path.is_absolute()
        || !path
            .extension()
            .and_then(|v| v.to_str())
            .is_some_and(|v| v.eq_ignore_ascii_case(request.format.extension()))
    {
        return Err(CommandError::new(
            "media_path",
            "请选择与导出格式一致的绝对文件路径",
            false,
        ));
    }
    if !infra_gif::spool::is_valid_session_id(&request.session_id) {
        return Err(CommandError::new("gif_session", "素材会话标识无效", false));
    }
    if request.max_width.is_some_and(|w| !(80..=2560).contains(&w)) {
        return Err(CommandError::new(
            "media_size",
            "导出宽度应在 80–2560 像素之间",
            false,
        ));
    }
    if request
        .quality
        .as_deref()
        .is_some_and(|q| !matches!(q, "high" | "medium" | "low"))
    {
        return Err(CommandError::new("media_quality", "未知画质预设", false));
    }
    Ok(())
}
struct Prepared {
    reader: infra_gif::SpoolReader,
    snapshot: blink_domain::gif::ExportSnapshot,
    runtime: Option<ffmpeg::RuntimeLease>,
    job: Job,
    request: ExportRequest,
}

async fn start_export(
    app: AppHandle,
    request: ExportRequest,
    require_current: bool,
) -> Result<
    (
        String,
        tokio::sync::oneshot::Receiver<Result<serde_json::Value, CommandError>>,
    ),
    CommandError,
> {
    validate(&request)?;
    let prepared = tauri::async_runtime::spawn_blocking(move || {
        let (reader, service) = super::gif::with_session(&app, &request.session_id)?;
        super::gif::ensure_timeline(&service, &reader, &request.session_id);
        let runtime = if request.format == ExportFormat::Mp4 {
            Some(ffmpeg::acquire().map_err(|error| {
                if matches!(
                    error,
                    ManagedRuntimeError::AlreadyInstalling | ManagedRuntimeError::InstanceBusy
                ) {
                    return CommandError::new(
                        "media_busy",
                        "媒体引擎正在安装或更新，请稍后导出",
                        true,
                    );
                }
                CommandError::new(
                    "media_runtime_missing",
                    "MP4 需要媒体引擎，请在设置中安装或修复 FFmpeg 后重试",
                    true,
                )
            })?)
        } else {
            None
        };
        let snapshot = {
            let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
            timelines
                .get(&request.session_id)
                .ok_or_else(|| CommandError::new("gif_session", "时间轴不存在", false))?
                .export_snapshot(reader.meta.region.w, reader.meta.region.h)
        };
        if snapshot.frames.is_empty() {
            return Err(CommandError::new("media_plan", "没有可导出的帧", false));
        }
        if snapshot.src_w == 0
            || snapshot.src_h == 0
            || snapshot.src_w > 2560
            || snapshot.src_h > 1600
        {
            return Err(CommandError::new(
                "media_size",
                "素材尺寸超出支持范围（最大 2560×1600），请缩小录制范围",
                false,
            ));
        }
        // 与原生窗口关闭/换会话采用同一锁顺序，避免关闭后迟到的准备请求启动旧任务。
        let current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if require_current && current.as_deref() != Some(request.session_id.as_str()) {
            return Err(CommandError::new(
                "media_stale_session",
                "编辑会话已切换，请重新导出",
                true,
            ));
        }
        let emitter = app.clone();
        let job = media_service::shared()
            .reserve(
                request.session_id.clone(),
                snapshot.revision,
                request.format.extension(),
                runtime.as_ref().map(|r| r.build_id().to_owned()),
                Arc::new(move |snapshot| {
                    let _ = emitter.emit(EventNames::GIF_EXPORT_PROGRESS, snapshot);
                }),
            )
            .map_err(media_error)?;
        drop(current);
        Ok::<_, CommandError>(Prepared {
            reader,
            snapshot,
            runtime,
            job,
            request,
        })
    })
    .await
    .map_err(|_| CommandError::new("media_prepare", "导出准备任务失败", true))??;
    let id = prepared.job.job_id.clone();
    let (tx, rx) = tokio::sync::oneshot::channel();
    // 与 IPC future 解耦，前端重载可按 id 查询；原生窗口结束由后端主动取消。
    tauri::async_runtime::spawn(async move {
        let Prepared {
            reader,
            snapshot,
            runtime,
            job,
            request,
        } = prepared;
        let callback = job.progress_callback();
        let output_path = request.path.clone();
        let mut result = match request.format {
            ExportFormat::Mp4 => {
                let plan = media::VideoPlan {
                    width: snapshot.src_w,
                    height: snapshot.src_h,
                    frames: snapshot
                        .frames
                        .iter()
                        .map(|f| media::PlanFrame {
                            src_idx: f.src_idx,
                            duration_ms: f.dur_ms,
                        })
                        .collect(),
                };
                let quality = match request.quality.as_deref() {
                    Some("high") => media::Quality::High,
                    Some("low") => media::Quality::Low,
                    _ => media::Quality::Medium,
                };
                let options = media::VideoOptions {
                    max_width: request.max_width,
                    quality,
                    overwrite: request.overwrite.unwrap_or(false),
                    ..Default::default()
                };
                match runtime {
                    Some(runtime) => media::export_mp4(
                        job.lease(),
                        runtime,
                        reader,
                        plan,
                        options,
                        PathBuf::from(request.path),
                        job.cancel.clone(),
                        callback,
                    )
                    .await
                    .map(|stats| serde_json::to_value(stats).unwrap_or_default())
                    .map_err(media_error),
                    None => Err(CommandError::new(
                        "media_runtime_missing",
                        "媒体引擎不可用",
                        true,
                    )),
                }
            }
            ExportFormat::Gif => {
                let plan = gif_service::to_encode_plan(&snapshot.gif_plan());
                let quality = match request.quality.as_deref() {
                    Some("high") => infra_gif::ExportQuality::High,
                    Some("low") => infra_gif::ExportQuality::Low,
                    _ => infra_gif::ExportQuality::Medium,
                };
                let options = infra_gif::ExportOptions {
                    max_width: request.max_width,
                    quality,
                    overwrite: request.overwrite.unwrap_or(false),
                };
                let cancel = job.gif_cancel.clone();
                let mut worker = tauri::async_runtime::spawn_blocking(move || {
                    let progress = |stage: infra_gif::ExportStage, done: u32, total: u32| {
                        if done == total || done % (total / 20).max(1) == 0 {
                            callback(media::Progress {
                                stage: stage.as_str(),
                                frames_done: done,
                                frames_total: total,
                            });
                        }
                    };
                    infra_gif::export_gif(
                        &reader,
                        &plan,
                        &options,
                        std::path::Path::new(&request.path),
                        &cancel,
                        &progress,
                    )
                });
                let mut timed_out = false;
                let outcome = tokio::select! {
                    biased;
                    result = &mut worker => result,
                    _ = job.cancel.cancelled() => { job.gif_cancel.store(true, Ordering::Relaxed); worker.await },
                    _ = tokio::time::sleep(Duration::from_secs(1800)) => { timed_out = true; job.gif_cancel.store(true, Ordering::Relaxed); worker.await },
                };
                match outcome {
                    Ok(Ok(stats)) => {
                        let mut value = serde_json::to_value(stats).unwrap_or_default();
                        value["format"] = "gif".into();
                        Ok(value)
                    }
                    Ok(Err(infra_gif::ExportError::Cancelled)) if timed_out => {
                        Err(media_error(media::MediaError::Timeout))
                    }
                    Ok(Err(infra_gif::ExportError::Cancelled)) => {
                        Err(media_error(media::MediaError::Cancelled))
                    }
                    Ok(Err(error)) => Err(CommandError::new("gif_export", error.to_string(), true)),
                    Err(_) => Err(CommandError::new(
                        "gif_export",
                        "导出线程失败，请重试",
                        true,
                    )),
                }
            }
        };
        if let Ok(stats) = &mut result {
            stats["outputPath"] = output_path.into();
        }
        if let Err(error) = &result {
            tracing::debug!(job_id = %job.job_id, code = %error.code, "媒体导出未提交");
        }
        job.finish(result.clone().map_err(|e| (e.code, e.message)));
        let _ = tx.send(result);
    });
    Ok((id, rx))
}

#[tauri::command]
pub async fn gif_begin_export(
    app: AppHandle,
    window: tauri::WebviewWindow,
    request: ExportRequest,
) -> Result<String, CommandError> {
    require_editor(&window)?;
    Ok(start_export(app, request, true).await?.0)
}
/// 旧 GIF IPC 保留，分派到同一服务，不保留第二套编码/终态逻辑。
#[tauri::command]
pub async fn gif_export(
    app: AppHandle,
    session_id: String,
    path: String,
    max_width: Option<u32>,
    quality: Option<String>,
    overwrite: Option<bool>,
) -> Result<serde_json::Value, CommandError> {
    let (_, result) = start_export(
        app,
        ExportRequest {
            session_id,
            path,
            format: ExportFormat::Gif,
            max_width,
            quality,
            overwrite,
        },
        false,
    )
    .await?;
    result
        .await
        .map_err(|_| CommandError::new("media_interrupted", "导出任务中断，请重试", true))?
}
#[tauri::command]
pub fn gif_export_status(
    window: tauri::WebviewWindow,
    session_id: String,
    job_id: Option<String>,
) -> Result<Option<JobSnapshot>, CommandError> {
    require_editor(&window)?;
    Ok(media_service::shared().snapshot(Some(&session_id), job_id.as_deref()))
}
#[tauri::command]
pub fn gif_cancel_export_job(
    window: tauri::WebviewWindow,
    session_id: String,
    job_id: String,
) -> Result<(), CommandError> {
    require_editor(&window)?;
    media_service::shared().cancel(Some(&session_id), Some(&job_id));
    Ok(())
}
#[tauri::command]
pub fn gif_export_cancel(session_id: String) -> Result<(), CommandError> {
    media_service::shared().cancel(Some(&session_id), None);
    Ok(())
}
#[tauri::command]
pub async fn gif_export_availability(
    window: tauri::WebviewWindow,
) -> Result<ffmpeg::RuntimeStatus, CommandError> {
    require_editor(&window)?;
    tauri::async_runtime::spawn_blocking(ffmpeg::status)
        .await
        .map_err(|_| CommandError::new("media_status", "无法读取媒体引擎状态", true))?
        .map_err(|_| {
            CommandError::new("media_status", "无法读取媒体引擎状态，请在设置中检查", true)
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn export_request_rejects_wrong_extension_relative_path_and_invalid_session() {
        let mut request = ExportRequest {
            session_id: "recording-1".into(),
            path: "C:\\中文 with space.MP4".into(),
            format: ExportFormat::Mp4,
            max_width: None,
            quality: None,
            overwrite: None,
        };
        assert!(validate(&request).is_ok());
        request.path = "C:\\video.gif".into();
        assert!(validate(&request).is_err());
        request.path = "relative.mp4".into();
        assert!(validate(&request).is_err());
        request.path = "C:\\video.mp4".into();
        request.session_id = "../other".into();
        assert!(validate(&request).is_err());
    }
}
