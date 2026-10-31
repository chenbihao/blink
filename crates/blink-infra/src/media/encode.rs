use super::{
    process::{self, RunOptions, TempOutput},
    *,
};
use crate::managed_runtime::ffmpeg::RuntimeLease;
use std::{
    path::PathBuf,
    sync::atomic::{AtomicU32, AtomicU64, Ordering},
};
use tokio::{sync::mpsc, time::Instant};
use tokio_util::sync::CancellationToken;

/// 30fps BGRA → MP4。整个任务只保留一个缓存帧、一个排队帧和一个写入帧。
pub async fn export_mp4<S: FrameSource>(
    _task: &TaskLease,
    runtime: RuntimeLease,
    mut source: S,
    plan: VideoPlan,
    options: VideoOptions,
    target: PathBuf,
    cancel: CancellationToken,
    progress: ProgressCallback,
) -> Result<VideoStats, MediaError> {
    let started = std::time::Instant::now();
    let preset = options.preset.unwrap_or_default();
    let deadline = Instant::now() + options.budget.timeout;
    let sampling = SamplingPlan::new(&plan.frames, source.frame_count(), &options.budget)?;
    let (width, height) = (plan.width, plan.height);
    if source.dimensions() != (width, height) || width == 0 || height == 0 {
        return Err(MediaError::InvalidPlan("素材尺寸不一致"));
    }
    if width > 2560 || height > 1600 {
        return Err(MediaError::Budget("素材最大 2560 × 1600，请缩小录制范围"));
    }
    let frame_bytes = width as usize * height as usize * 4;
    if frame_bytes > options.budget.max_frame_bytes.min(16 << 20) {
        return Err(MediaError::Budget("单帧解压体积"));
    }
    if !(1..=2).contains(&options.budget.threads) || options.budget.max_output_bytes == 0 {
        return Err(MediaError::InvalidPlan("线程或输出预算"));
    }
    let scaled_w = options.max_width.unwrap_or(width).min(width).max(1);
    let scaled_h = ((height as f64 * scaled_w as f64 / width as f64).round() as u32).max(1);
    let (out_w, out_h) = (scaled_w.next_multiple_of(2), scaled_h.next_multiple_of(2));
    let path = target.clone();
    let overwrite = options.overwrite;
    let temp = tokio::task::spawn_blocking(move || {
        if !overwrite && path.exists() {
            return Err(MediaError::FileExists);
        }
        TempOutput::create(&path)
    })
    .await
    .map_err(|_| MediaError::Process("准备文件任务失败"))??;
    progress(Progress {
        stage: "preparing",
        frames_done: 0,
        frames_total: sampling.output_frames,
    });
    let peak = AtomicU64::new(0);
    let total = sampling.output_frames;
    let stop = cancel.child_token();
    let _stop_guard = stop.clone().drop_guard();
    let producer_stop = stop.clone();
    let (tx, rx) = mpsc::channel(1);
    let samples = sampling.clone();
    let producer = tokio::task::spawn_blocking(move || {
        let mut cached: Option<(u32, Arc<Vec<u8>>)> = None;
        for n in 0..samples.output_frames {
            if producer_stop.is_cancelled() {
                return Err(MediaError::Cancelled);
            }
            let index = samples
                .source_index(n)
                .ok_or(MediaError::InvalidPlan("采样越界"))?;
            if cached.as_ref().map(|(i, _)| *i) != Some(index) {
                let frame = source.read_bgra(index)?;
                if frame.len() != frame_bytes {
                    return Err(MediaError::Source(index));
                }
                cached = Some((index, Arc::new(frame)));
            }
            if tx
                .blocking_send(Arc::clone(&cached.as_ref().unwrap().1))
                .is_err()
            {
                break;
            }
        }
        Ok::<_, MediaError>(())
    });
    let mut args = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-f",
        "rawvideo",
        "-pixel_format",
        "bgra",
        "-video_size",
        &format!("{width}x{height}"),
        "-framerate",
        "30",
        "-i",
        "pipe:0",
        "-an",
        "-c:v",
        preset.encoder(),
        "-preset",
        "veryfast",
        "-tune",
        "zerolatency",
        "-threads",
        &options.budget.threads.to_string(),
        "-filter_threads",
        "1",
        "-crf",
        options
            .preset
            .map_or(options.quality.crf(), VideoPreset::crf),
        "-vf",
        &format!(
            "scale={scaled_w}:{scaled_h}:flags=lanczos:in_range=full:out_range=tv:out_color_matrix=bt709,pad={out_w}:{out_h}:0:0:color=black,format={}",
            preset.pixel_format()
        ),
        "-color_range",
        "tv",
        "-colorspace",
        "bt709",
        "-color_primaries",
        "bt709",
        "-color_trc",
        "bt709",
        "-movflags",
        "+faststart",
        "-progress",
        "pipe:1",
        "-stats_period",
        "0.2",
        "-fs",
        &options.budget.max_output_bytes.to_string(),
        "-f",
        "mp4",
    ]);
    if preset == VideoPreset::Compact {
        // x265 的线程池不受通用 -threads 完整约束，显式限制，避免占满 CPU。
        args.extend(process::args(&[
            "-x265-params",
            "pools=1:frame-threads=1:wpp=0",
            "-tag:v",
            "hvc1",
        ]));
    }
    args.push(temp.0.as_os_str().to_owned());
    let encoded = process::run(
        &runtime.ffmpeg,
        &args,
        RunOptions {
            cancel: &cancel,
            deadline,
            input: Some(rx),
            progress: Some((progress.clone(), "encoding", total)),
            output_budget: Some((&temp.0, options.budget.max_output_bytes)),
            peak_bytes: &peak,
        },
    )
    .await;
    stop.cancel();
    let produced = producer
        .await
        .map_err(|_| MediaError::Process("素材读取任务失败"))?;
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    // 源帧损坏优先报告真实原因；编码提前结束时 producer 被关闭的管道唤醒。
    if let Err(error) = produced {
        if !matches!(error, MediaError::Cancelled) {
            return Err(error);
        }
    }
    encoded?;
    let bytes = tokio::fs::metadata(&temp.0).await?.len();
    if bytes == 0 || bytes > options.budget.max_output_bytes {
        return Err(MediaError::Budget("输出文件大小"));
    }
    progress(Progress {
        stage: "verifying",
        frames_done: 0,
        frames_total: total,
    });
    let mut args = process::args(&["-v", "error", "-show_streams", "-of", "json"]);
    args.push(temp.0.as_os_str().to_owned());
    let metadata = process::run(
        &runtime.ffprobe,
        &args,
        RunOptions {
            cancel: &cancel,
            deadline,
            input: None,
            progress: None,
            output_budget: None,
            peak_bytes: &peak,
        },
    )
    .await?;
    verify_metadata(&metadata, out_w, out_h, total, preset)?;
    let decoded = Arc::new(AtomicU32::new(0));
    let decoded2 = decoded.clone();
    let callback = progress.clone();
    let verify_progress: ProgressCallback = Arc::new(move |p| {
        decoded2.store(p.frames_done, Ordering::Relaxed);
        callback(p);
    });
    let mut args = process::args(&[
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-xerror",
        "-threads",
        "1",
        "-i",
    ]);
    args.push(temp.0.as_os_str().to_owned());
    args.extend(process::args(&[
        "-an",
        "-progress",
        "pipe:1",
        "-stats_period",
        "0.2",
        "-f",
        "null",
        "-",
    ]));
    process::run(
        &runtime.ffmpeg,
        &args,
        RunOptions {
            cancel: &cancel,
            deadline,
            input: None,
            progress: Some((verify_progress, "verifying", total)),
            output_budget: None,
            peak_bytes: &peak,
        },
    )
    .await?;
    if decoded.load(Ordering::Relaxed) != total {
        return Err(MediaError::Verify("解码帧数不符"));
    }
    if cancel.is_cancelled() {
        return Err(MediaError::Cancelled);
    }
    if Instant::now() >= deadline {
        return Err(MediaError::Timeout);
    }
    progress(Progress {
        stage: "committing",
        frames_done: total,
        frames_total: total,
    });
    let commit_cancel = cancel.clone();
    tokio::task::spawn_blocking(move || {
        if commit_cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(MediaError::Timeout);
        }
        crate::utils::fs::commit_file(&temp.0, &target, overwrite).map_err(|e| {
            if !overwrite && e.kind() == std::io::ErrorKind::AlreadyExists {
                MediaError::FileExists
            } else {
                MediaError::Io(e)
            }
        })
    })
    .await
    .map_err(|_| MediaError::Process("提交文件任务失败"))??;
    tracing::info!(
        ?preset,
        frames = total,
        width = out_w,
        height = out_h,
        bytes,
        elapsed_ms = started.elapsed().as_millis() as u64,
        "MP4 导出已验证并提交"
    );
    Ok(VideoStats {
        format: "mp4",
        codec: preset.codec(),
        width: out_w,
        height: out_h,
        source_frames: plan.frames.len() as u32,
        written_frames: total,
        duration_ms: sampling.duration_ms(),
        fps: SamplingPlan::FPS,
        bytes,
        encode_ms: started.elapsed().as_millis() as u64,
        peak_child_bytes: peak.load(Ordering::Relaxed),
    })
}

pub(super) fn verify_metadata(
    bytes: &[u8],
    width: u32,
    height: u32,
    frames: u32,
    preset: VideoPreset,
) -> Result<(), MediaError> {
    let value: serde_json::Value =
        serde_json::from_slice(bytes).map_err(|_| MediaError::Verify("元数据格式"))?;
    let stream = value["streams"]
        .as_array()
        .filter(|s| s.len() == 1)
        .and_then(|s| s.first());
    if !stream.is_some_and(|s| {
        s["codec_name"] == preset.codec()
            && s["codec_type"] == "video"
            && s["pix_fmt"] == preset.pixel_format()
            && s["width"] == width
            && s["height"] == height
            && s["nb_frames"] == frames.to_string()
            && s["avg_frame_rate"] == "30/1"
            && s["color_range"] == "tv"
            && s["color_space"] == "bt709"
            && s["duration"]
                .as_str()
                .and_then(|v| v.parse::<f64>().ok())
                .is_some_and(|d| (d - frames as f64 / 30.0).abs() < 0.002)
    }) {
        return Err(MediaError::Verify("视频编码、尺寸、色彩、帧数或时长不符"));
    }
    Ok(())
}
