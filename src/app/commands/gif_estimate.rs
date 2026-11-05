//! Bounded content samples share the heavy task slot; real exports preempt them.
use super::gif_export::{ExportFormat, ExportRequest};
use crate::app::{command_error::CommandError, gif_service::GifService, media_service};
use blink_infra::{gif as gif_io, media};
use std::{path::PathBuf, sync::Arc, sync::atomic::Ordering, time::Duration};
use tauri::AppHandle;

#[tauri::command]
pub fn gif_cancel_estimate(
    app: AppHandle,
    window: tauri::WebviewWindow,
    session_id: String,
) -> Result<(), CommandError> {
    let _ = app;
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "仅用于录制编辑器",
            false,
        ));
    }
    media_service::shared().cancel_estimate(&session_id);
    Ok(())
}

#[tauri::command]
pub async fn gif_estimate(
    app: AppHandle,
    window: tauri::WebviewWindow,
    request: ExportRequest,
) -> Result<serde_json::Value, CommandError> {
    if window.label() != "gif-editor" {
        return Err(CommandError::new(
            "permission_denied",
            "仅用于录制编辑器",
            false,
        ));
    }
    // The worker survives an IPC future being dropped, so cancellation can join
    // its pipes/processes and release leases before a real export takes the slot.
    tauri::async_runtime::spawn(async move { estimate_impl(app, request).await })
        .await
        .map_err(|_| CommandError::new("media_estimate", "估算任务中断", true))?
}
async fn estimate_impl(
    app: AppHandle,
    request: ExportRequest,
) -> Result<serde_json::Value, CommandError> {
    let prepared = tauri::async_runtime::spawn_blocking(move || {
        let mut validation = request.clone();
        if validation.path.is_empty() {
            validation.path = GifService::root()
                .join(if validation.format == ExportFormat::Mp4 {
                    "estimate.mp4"
                } else {
                    "estimate.gif"
                })
                .to_string_lossy()
                .into_owned();
        }
        super::gif_export::validate(&validation)?;
        let (reader, service) = super::gif::with_session(&app, &request.session_id)?;
        let timelines = service.timelines.lock().unwrap_or_else(|e| e.into_inner());
        let tl = timelines
            .get(&request.session_id)
            .ok_or_else(|| CommandError::new("gif_session", "请先打开会话", true))?;
        let snapshot = tl.export_snapshot(reader.meta.region.w, reader.meta.region.h);
        let total_ms = tl.effective_total_ms();
        drop(timelines);
        let disk = request
            .path
            .is_empty()
            .then_some(None)
            .unwrap_or_else(|| {
                std::path::Path::new(&request.path)
                    .parent()
                    .map(blink_infra::platform::disk_space::query)
            })
            .transpose()
            .map_err(|e| CommandError::new("media_space", e.to_string(), true))?;
        let source_disk = blink_infra::platform::disk_space::query(reader.dir())
            .map_err(|e| CommandError::new("media_space", e.to_string(), true))?;
        let runtime = if request.format == ExportFormat::Mp4 {
            Some(
                blink_infra::managed_runtime::ffmpeg::acquire().map_err(|_| {
                    CommandError::new("media_runtime_missing", "需要 FFmpeg 媒体引擎", true)
                })?,
            )
        } else {
            None
        };
        let current = service
            .editor_session
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if current.as_deref() != Some(request.session_id.as_str()) {
            return Err(CommandError::new(
                "media_stale_session",
                "编辑会话已切换",
                true,
            ));
        }
        let job = media_service::shared()
            .reserve(
                request.session_id.clone(),
                snapshot.revision,
                if request.format == ExportFormat::Mp4 {
                    "estimate_mp4"
                } else {
                    "estimate_gif"
                },
                runtime.as_ref().map(|r| r.build_id().to_owned()),
                Arc::new(|_| {}),
            )
            .map_err(|e| CommandError::new(e.code(), e.to_string(), true))?;
        drop(current);
        Ok::<_, CommandError>((
            reader,
            snapshot,
            total_ms,
            disk,
            source_disk,
            runtime,
            job,
            request,
        ))
    })
    .await
    .map_err(|_| CommandError::new("media_estimate", "估算准备失败", true))??;
    let (reader, snapshot, total_ms, disk, source_disk, runtime, job, request) = prepared;
    let revision = snapshot.revision;
    let audio = gif_io::audio::manifest(reader.dir())
        .map_err(|e| CommandError::new("media_audio", e.to_string(), true))?;
    let has_audio = request.format == ExportFormat::Mp4
        && audio.as_ref().is_some_and(|a| {
            a.enabled
                .iter()
                .enumerate()
                .any(|(i, on)| *on && !snapshot.mix[i].muted && snapshot.mix[i].gain > 0.0)
        });
    let audio_extra = if has_audio {
        snapshot
            .audio
            .iter()
            .fold(0u64, |total, span| {
                total
                    .saturating_add(span.sound_ms)
                    .saturating_add(span.silence_ms)
            })
            .saturating_mul(gif_io::audio::BYTES_PER_MS)
    } else {
        0
    };
    let source_count = snapshot.frames.len();
    if source_count == 0 {
        return Err(CommandError::new("media_plan", "没有有效帧", false));
    }
    // Three representative neighborhoods, at most 96 source frames / 6 seconds.
    let count = source_count.min(96);
    let frames = snapshot.frames.clone();
    let sample = media::FrameList::generated(count, move |i| {
        let index = if source_count <= 96 {
            i
        } else {
            let section = i / 32;
            let center = section * (source_count - 32) / 2;
            center + i % 32
        };
        let mut f = frames.get(index.min(source_count - 1)).unwrap();
        f.dur_ms = f.dur_ms.clamp(1, (6000 / count as u32).min(2000));
        f
    });
    let mut sample_snapshot = snapshot.clone();
    sample_snapshot.frames = sample.clone();
    // Estimate output units from cached totals; never scan the whole long timeline
    // merely to start a bounded content sample. GIF grouping/dedup makes this approximate.
    let gif_units = (source_count as u64)
        .min(total_ms.div_ceil(20))
        .max(total_ms.div_ceil(u16::MAX as u64 * 10));
    let sample_gif = sample_snapshot.gif_plan();
    let sample_gif_units = sample_gif
        .frames
        .iter()
        .map(|f| (f.delay_cs as u64).div_ceil(u16::MAX as u64))
        .sum::<u64>();
    let sample_ms = sample.iter().map(|f| f.dur_ms as u64).sum::<u64>();
    let extension = if request.format == ExportFormat::Mp4 {
        "mp4"
    } else {
        "gif"
    };
    let path = reader
        .dir()
        .join(format!(".estimate-{}.{}", job.job_id, extension));
    struct Remove(PathBuf);
    impl Drop for Remove {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }
    let cleanup = Remove(path.clone());
    let bytes = if let Some(runtime) = runtime {
        let plan = media::VideoPlan {
            width: snapshot.src_w,
            height: snapshot.src_h,
            frames: sample.map(|f| media::PlanFrame {
                src_idx: f.src_idx,
                duration_ms: f.dur_ms,
            }),
        };
        let options = media::VideoOptions {
            max_width: request.max_width,
            preset: request.preset,
            quality: match request.quality.as_deref() {
                Some("high") => media::Quality::High,
                Some("low") => media::Quality::Low,
                _ => media::Quality::Medium,
            },
            fps: request.fps.unwrap_or(reader.meta.fps),
            budget: media::Budget {
                timeout: Duration::from_secs(60),
                ..Default::default()
            },
            ..Default::default()
        };
        media::export_mp4(
            job.lease(),
            runtime,
            reader,
            plan,
            options,
            path,
            job.cancel.clone(),
            Arc::new(|_| {}),
        )
        .await
        .map(|s| (s.bytes, None))
        .map_err(|e| CommandError::new(e.code(), e.to_string(), true))
    } else {
        let plan = gif_io::EncodePlan {
            src_w: snapshot.src_w,
            src_h: snapshot.src_h,
            loop_count: Some(0),
            frames: sample_gif.frames.map(|f| gif_io::EncodeFrame {
                src_idx: f.src_idx,
                delay_cs: f.delay_cs,
            }),
        };
        let options = gif_io::ExportOptions {
            max_width: request.max_width,
            quality: match request.quality.as_deref() {
                Some("high") => gif_io::ExportQuality::High,
                Some("low") => gif_io::ExportQuality::Low,
                _ => gif_io::ExportQuality::Medium,
            },
            overwrite: false,
        };
        let cancel = job.gif_cancel.clone();
        let mut task = tauri::async_runtime::spawn_blocking(move || {
            gif_io::export_gif(&reader, &plan, &options, &path, &cancel, &|_, _, _| {})
        });
        let result = tokio::select! {result=&mut task=>result,_=job.cancel.cancelled()=>{job.gif_cancel.store(true,Ordering::Relaxed);task.await},_=tokio::time::sleep(Duration::from_secs(60))=>{job.gif_cancel.store(true,Ordering::Relaxed);task.await}};
        result
            .map_err(|_| CommandError::new("media_estimate", "抽样任务失败", true))?
            .map(|s| (s.bytes, Some(s.written_frames)))
            .map_err(|e| CommandError::new("media_estimate", e.to_string(), true))
    };
    // Cleanup and runtime/pipe completion precede releasing the shared task slot.
    drop(cleanup);
    let result=bytes.map(|(sample_bytes, gif_written)| {
        let range = if let Some(written) = gif_written {
            media::estimate::SizeRange::around(predict_gif_bytes(sample_bytes, sample_gif_units, written, gif_units, total_ms))
        } else {
            media::estimate::mp4_size_range(sample_bytes, sample_ms, total_ms, has_audio)
        };
        serde_json::json!({"revision":revision,"lowerBytes":range.lower,"upperBytes":range.upper,
            "sampleBytes":sample_bytes,"sampleMs":sample_ms,"targetDisk":disk,"sourceDisk":source_disk,
            "audioEstimated":has_audio,"extraBytes":range.upper.saturating_add(audio_extra),"audioTempBytes":audio_extra,"sampleFrames":count})
    });
    job.finish(
        result
            .as_ref()
            .map(|v| v.clone())
            .map_err(|e| (e.code.clone(), e.message.clone())),
    );
    result
}

#[tauri::command]
pub async fn gif_storage_status()
-> Result<blink_infra::platform::disk_space::DiskSpace, CommandError> {
    tauri::async_runtime::spawn_blocking(|| {
        let mut path = GifService::root();
        while !path.exists() && path.pop() {}
        blink_infra::platform::disk_space::query(&path)
            .map_err(|e| CommandError::new("media_space", e.to_string(), true))
    })
    .await
    .map_err(|_| CommandError::new("media_space", "空间查询失败", true))?
}

fn extrapolate(bytes: u64, overhead: u64, factor: f64) -> u64 {
    let fixed = bytes.min(overhead);
    fixed.saturating_add(((bytes - fixed) as f64 * factor).clamp(0.0, u64::MAX as f64) as u64)
}

fn predict_gif_bytes(
    bytes: u64,
    sample_units: u64,
    written: u32,
    total_units: u64,
    total_ms: u64,
) -> u64 {
    // A static sample collapses to a single GIF frame. Scaling its header by
    // the source frame count would overestimate a long static recording by orders
    // of magnitude. Only the format's maximum-delay splits still add frames.
    let factor = if written <= 1 {
        total_ms.div_ceil(u16::MAX as u64 * 10).max(1) as f64
    } else {
        total_units as f64 / sample_units.max(1) as f64
    };
    extrapolate(bytes, 800, factor)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn static_gif_estimate_keeps_one_header_and_accounts_for_long_delay_splits() {
        assert_eq!(predict_gif_bytes(1000, 96, 1, 18_000, 120_000), 1000);
        assert_eq!(predict_gif_bytes(1000, 1, 1, 1, 1_800_000), 1400);
        assert_eq!(predict_gif_bytes(1000, 96, 96, 960, 120_000), 2800);
    }

    #[test]
    fn gif_estimate_interval_matches_static_scroll_and_dynamic_fixtures() {
        let subscriber = tracing_subscriber::fmt()
            .with_test_writer()
            .with_max_level(tracing::Level::INFO)
            .finish();
        let _logging = tracing::subscriber::set_default(subscriber);
        use gif_io::{
            CaptureRegion, EncodeFrame, EncodePlan, ExportOptions, SessionMeta, SpoolReader,
            SpoolWriter,
        };
        use std::sync::atomic::AtomicBool;
        for mode in ["static", "scroll", "dynamic"] {
            let root = tempfile::tempdir().unwrap();
            let meta = SessionMeta {
                spool_version: gif_io::SPOOL_VERSION,
                id: mode.into(),
                region: CaptureRegion {
                    x: 0,
                    y: 0,
                    w: 80,
                    h: 48,
                },
                fps: 10,
                show_cursor: false,
                created_at: 0,
                state: "recording".into(),
                ts_ms: vec![],
                last_dur_ms: 100,
                spool_bytes: 0,
                stop_reason: None,
            };
            let mut writer = SpoolWriter::create(root.path(), meta).unwrap();
            let mut random = 7u32;
            for frame in 0..300u32 {
                let mut pixels = vec![0u8; 80 * 48 * 4];
                for (i, px) in pixels.chunks_exact_mut(4).enumerate() {
                    let v = match mode {
                        "static" => ((i / 80 / 8 + i % 80 / 8) % 2 * 180) as u8,
                        "scroll" => (((i / 80 + frame as usize) / 6) % 2 * 180) as u8,
                        _ => {
                            random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                            (random >> 24) as u8
                        }
                    };
                    px.copy_from_slice(&[v, v.wrapping_mul(3), v.wrapping_add(30), 255]);
                }
                writer.append_frame(frame as u64 * 100, &pixels).unwrap();
            }
            writer.finalize("user").unwrap();
            let reader = SpoolReader::open(root.path(), mode).unwrap();
            let full = EncodePlan {
                src_w: 80,
                src_h: 48,
                loop_count: Some(0),
                frames: media::FrameList::generated(300, |i| EncodeFrame {
                    src_idx: i as u32,
                    delay_cs: 10,
                }),
            };
            let sample = EncodePlan {
                frames: media::FrameList::generated(96, |i| EncodeFrame {
                    src_idx: ((i / 32) * (300 - 32) / 2 + i % 32) as u32,
                    delay_cs: 6,
                }),
                ..full.clone()
            };
            let cancel = AtomicBool::new(false);
            let sampled = gif_io::export_gif(
                &reader,
                &sample,
                &ExportOptions::default(),
                &root.path().join("sample.gif"),
                &cancel,
                &|_, _, _| {},
            )
            .unwrap();
            let actual = gif_io::export_gif(
                &reader,
                &full,
                &ExportOptions::default(),
                &root.path().join("actual.gif"),
                &cancel,
                &|_, _, _| {},
            )
            .unwrap();
            let predicted =
                predict_gif_bytes(sampled.bytes, 96, sampled.written_frames, 300, 30_000);
            tracing::info!(
                mode,
                predicted,
                lower = predicted / 2,
                upper = predicted.saturating_mul(2),
                actual = actual.bytes,
                error_percent = (predicted as f64 / actual.bytes as f64 - 1.0) * 100.0,
                "GIF estimate fixture"
            );
            assert!(actual.bytes >= predicted / 2 && actual.bytes <= predicted.saturating_mul(2));
        }
    }
}
