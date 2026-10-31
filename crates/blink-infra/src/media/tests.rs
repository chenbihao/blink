use super::*;
fn frames(durations: &[u32]) -> Vec<PlanFrame> {
    durations
        .iter()
        .enumerate()
        .map(|(i, ms)| PlanFrame {
            src_idx: i as u32,
            duration_ms: *ms,
        })
        .collect()
}
#[test]
fn cumulative_sampling_has_no_per_frame_rounding_drift() {
    let plan = SamplingPlan::new(&frames(&vec![33; 1000]), 1000, &Budget::default()).unwrap();
    assert_eq!(plan.output_frames, 990);
    assert_eq!(plan.duration_ms(), 33_000);
    assert_eq!(plan.source_index(0), Some(0));
    assert_eq!(plan.source_index(989), Some(998));
    assert_eq!(plan.source_index(990), None);
}
#[test]
fn mixed_deleted_speed_hold_and_tiny_tail_use_cumulative_clock() {
    let frames = [
        PlanFrame {
            src_idx: 1,
            duration_ms: 20,
        },
        PlanFrame {
            src_idx: 4,
            duration_ms: 80,
        },
        PlanFrame {
            src_idx: 7,
            duration_ms: 700,
        },
    ];
    let plan = SamplingPlan::new(&frames, 8, &Budget::default()).unwrap();
    assert_eq!(plan.output_frames, 24);
    assert_eq!(
        (0..5)
            .map(|i| plan.source_index(i).unwrap())
            .collect::<Vec<_>>(),
        vec![1, 4, 4, 7, 7]
    );
    assert_eq!(plan.source_index(23), Some(7));
    assert_eq!(
        SamplingPlan::new(&self::frames(&[1]), 1, &Budget::default())
            .unwrap()
            .output_frames,
        1
    );
}
#[test]
fn long_hold_rejected_before_allocating_samples() {
    assert!(matches!(
        SamplingPlan::new(&frames(&[600_001]), 1, &Budget::default()),
        Err(MediaError::Budget(_))
    ));
    assert!(SamplingPlan::new(&frames(&[0]), 1, &Budget::default()).is_err());
    assert!(SamplingPlan::new(&frames(&[33]), 0, &Budget::default()).is_err());
    let plan = SamplingPlan::new(&frames(&[600_000]), 1, &Budget::default()).unwrap();
    assert_eq!(plan.output_frames, 18000);
    assert_eq!(plan.source_index(17999), Some(0));
}
#[test]
fn every_duration_rounds_up_by_less_than_one_output_frame() {
    for ms in 1..=20_000 {
        let plan = SamplingPlan::new(&frames(&[ms]), 1, &Budget::default()).unwrap();
        assert!(plan.output_frames as u64 * 1000 >= ms as u64 * 30);
        assert!((plan.output_frames as u64 * 1000) < ms as u64 * 30 + 1000);
    }
}
#[test]
fn task_lease_is_exclusive_and_released() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("task.lock");
    let first = acquire_task_lease_at(&path).unwrap();
    assert!(matches!(
        acquire_task_lease_at(&path),
        Err(MediaError::Busy)
    ));
    drop(first);
    assert!(acquire_task_lease_at(&path).is_ok());
}

#[test]
fn artifact_validation_rejects_wrong_codec_clock_color_or_extra_streams() {
    let valid = serde_json::json!({"streams":[{"codec_type":"video","codec_name":"h264","width":66,"height":68,
        "pix_fmt":"yuv420p","nb_frames":"24","avg_frame_rate":"30/1","duration":"0.800000","color_range":"tv","color_space":"bt709"}]});
    assert!(
        encode::verify_metadata(
            &serde_json::to_vec(&valid).unwrap(),
            66,
            68,
            24,
            VideoPreset::General
        )
        .is_ok()
    );
    for (field, value) in [
        ("codec_name", serde_json::json!("hevc")),
        ("width", serde_json::json!(64)),
        ("nb_frames", serde_json::json!("3")),
        ("avg_frame_rate", serde_json::json!("15/1")),
        ("duration", serde_json::json!("0.9")),
        ("pix_fmt", serde_json::json!("yuv444p")),
        ("color_range", serde_json::json!("pc")),
        ("color_space", serde_json::json!("bt470bg")),
    ] {
        let mut wrong = valid.clone();
        wrong["streams"][0][field] = value;
        assert!(
            encode::verify_metadata(
                &serde_json::to_vec(&wrong).unwrap(),
                66,
                68,
                24,
                VideoPreset::General
            )
            .is_err(),
            "{field}"
        );
    }
    let mut audio = valid.clone();
    audio["streams"]
        .as_array_mut()
        .unwrap()
        .push(serde_json::json!({"codec_type":"audio"}));
    assert!(
        encode::verify_metadata(
            &serde_json::to_vec(&audio).unwrap(),
            66,
            68,
            24,
            VideoPreset::General
        )
        .is_err()
    );
    assert!(encode::verify_metadata(b"not-json", 66, 68, 24, VideoPreset::General).is_err());
}

#[test]
fn preset_validation_accepts_only_the_requested_codec_and_chroma() {
    for preset in [
        VideoPreset::General,
        VideoPreset::HighQuality,
        VideoPreset::Text,
        VideoPreset::Compact,
    ] {
        let codec = if preset == VideoPreset::Compact {
            "hevc"
        } else {
            "h264"
        };
        let pixels = if preset == VideoPreset::Text {
            "yuv444p"
        } else {
            "yuv420p"
        };
        let mut metadata = serde_json::json!({"streams":[{"codec_type":"video","codec_name":codec,"width":66,"height":68,
            "pix_fmt":pixels,"nb_frames":"24","avg_frame_rate":"30/1","duration":"0.800000","color_range":"tv","color_space":"bt709"}]});
        assert!(
            encode::verify_metadata(&serde_json::to_vec(&metadata).unwrap(), 66, 68, 24, preset)
                .is_ok()
        );
        metadata["streams"][0]["pix_fmt"] = serde_json::json!(if pixels == "yuv420p" {
            "yuv444p"
        } else {
            "yuv420p"
        });
        assert!(
            encode::verify_metadata(&serde_json::to_vec(&metadata).unwrap(), 66, 68, 24, preset)
                .is_err()
        );
    }
}

#[test]
fn edited_duration_and_source_frame_budget_are_both_enforced() {
    let budget = Budget {
        max_duration_ms: 1000,
        max_source_frames: 2,
        ..Default::default()
    };
    assert!(SamplingPlan::new(&frames(&[500, 500]), 2, &budget).is_ok());
    assert!(matches!(
        SamplingPlan::new(&frames(&[1001]), 1, &budget),
        Err(MediaError::Budget(_))
    ));
    assert!(matches!(
        SamplingPlan::new(&frames(&[1, 1, 1]), 3, &budget),
        Err(MediaError::Budget(_))
    ));
}
#[test]
fn temp_file_is_unique_and_removed_without_touching_target() {
    let temp = tempfile::tempdir().unwrap();
    let target = temp.path().join("成品 with space.mp4");
    std::fs::write(&target, b"keep").unwrap();
    let a = process::TempOutput::create(&target).unwrap();
    let b = process::TempOutput::create(&target).unwrap();
    assert_ne!(a.0, b.0);
    let path = a.0.clone();
    drop(a);
    assert!(!path.exists());
    assert_eq!(std::fs::read(target).unwrap(), b"keep");
}

#[cfg(windows)]
#[tokio::test]
async fn supervised_process_timeout_cancel_stderr_and_broken_pipe() {
    use std::sync::atomic::AtomicU64;
    use tokio::time::Instant;
    use tokio_util::sync::CancellationToken;
    let peak = AtomicU64::new(0);
    let cancel = CancellationToken::new();
    let run = |script: &str| process::args(&["-NoProfile", "-NonInteractive", "-Command", script]);
    let exe = std::path::Path::new("powershell.exe");
    let options = |deadline| process::RunOptions {
        cancel: &cancel,
        deadline,
        input: None,
        progress: None,
        output_budget: None,
        peak_bytes: &peak,
    };
    let error = process::run(
        exe,
        &run("[Console]::Error.Write(('x' * 1000000)); exit 7"),
        options(Instant::now() + std::time::Duration::from_secs(10)),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MediaError::Process(_)));
    let error = process::run(
        exe,
        &run("Start-Sleep -Seconds 30"),
        options(Instant::now() + std::time::Duration::from_millis(200)),
    )
    .await
    .unwrap_err();
    assert!(matches!(error, MediaError::Timeout));
    let (tx, rx) = tokio::sync::mpsc::channel(1);
    tx.send(Arc::new(vec![0; 2 << 20])).await.unwrap();
    drop(tx);
    let (writer, reader) = tokio::io::duplex(64);
    drop(reader);
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(1),
        process::write_input(writer, rx),
    )
    .await
    .unwrap();
    assert!(matches!(result, Err(MediaError::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe));
    let trigger = async {
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        cancel.cancel();
    };
    let args = run("Start-Sleep -Seconds 30");
    let (result, _) = tokio::join!(
        process::run(
            exe,
            &args,
            options(Instant::now() + std::time::Duration::from_secs(10))
        ),
        trigger
    );
    assert!(matches!(result, Err(MediaError::Cancelled)));
}

struct Synthetic {
    width: u32,
    height: u32,
    frames: u32,
}
impl FrameSource for Synthetic {
    fn dimensions(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    fn frame_count(&self) -> u32 {
        self.frames
    }
    fn read_bgra(&mut self, index: u32) -> Result<Vec<u8>, MediaError> {
        let mut bytes = vec![0; self.width as usize * self.height as usize * 4];
        for (n, p) in bytes.chunks_exact_mut(4).enumerate() {
            p.copy_from_slice(&[(n as u32 + index * 17) as u8, (n / 19) as u8, 220, 255]);
        }
        Ok(bytes)
    }
}

/// 发行包显式注入，真实进程全链路在隔离目录；.20 复用此入口扩展资源样例。
#[tokio::test]
#[ignore = "需要 BLINK_FFMPEG_TEST_ARCHIVE 指向锁定发行包，显式运行"]
async fn real_mp4_pipeline_smoke() {
    let temp = tempfile::tempdir().unwrap();
    let lease = crate::managed_runtime::ffmpeg::test_archive_lease(temp.path()).await;
    let task = acquire_task_lease_at(&temp.path().join("task.lock")).unwrap();
    let plan = VideoPlan {
        width: 65,
        height: 67,
        frames: frames(&[33, 66, 700]),
    };
    use crate::gif::{
        CaptureRegion, EncodeFrame, EncodePlan, ExportOptions, SessionMeta, SpoolReader,
        SpoolWriter,
    };
    let spool_root = temp.path().join("spool");
    let id = "shared-snapshot";
    let meta = SessionMeta {
        spool_version: crate::gif::spool::SPOOL_VERSION,
        id: id.into(),
        region: CaptureRegion {
            x: 0,
            y: 0,
            w: 65,
            h: 67,
        },
        fps: 30,
        show_cursor: false,
        created_at: 0,
        state: "recording".into(),
        ts_ms: Vec::new(),
        last_dur_ms: 700,
        spool_bytes: 0,
        stop_reason: None,
    };
    let mut writer = SpoolWriter::create(&spool_root, meta).unwrap();
    let mut synth = Synthetic {
        width: 65,
        height: 67,
        frames: 3,
    };
    for (i, ts) in [0, 33, 99].into_iter().enumerate() {
        writer
            .append_frame(ts, &synth.read_bgra(i as u32).unwrap())
            .unwrap();
    }
    writer.finalize("user").unwrap();
    let reader = SpoolReader::open(&spool_root, id).unwrap();
    let gif_plan = EncodePlan {
        src_w: 65,
        src_h: 67,
        loop_count: Some(3),
        frames: plan
            .frames
            .iter()
            .map(|f| EncodeFrame {
                src_idx: f.src_idx,
                delay_cs: (f.duration_ms / 10).clamp(2, 600),
            })
            .collect(),
    };
    let gif_path = temp.path().join("same-snapshot.gif");
    let gif_stats = crate::gif::export_gif(
        &reader,
        &gif_plan,
        &ExportOptions::default(),
        &gif_path,
        &std::sync::atomic::AtomicBool::new(false),
        &|_, _, _| {},
    )
    .unwrap();
    assert_eq!(
        (gif_stats.width, gif_stats.height, gif_stats.written_frames),
        (65, 67, 3)
    );
    let mut decoder = gif::DecodeOptions::new()
        .read_info(std::fs::File::open(gif_path).unwrap())
        .unwrap();
    let mut delay = 0;
    while let Some(frame) = decoder.read_next_frame().unwrap() {
        delay += frame.delay as u32;
    }
    assert_eq!(
        delay, 79,
        "GIF 保留原来的百分之一秒量化；MP4 使用毫秒累计时钟"
    );
    let target = temp.path().join("中文 with space.mp4");
    let stats = export_mp4(
        &task,
        lease,
        reader,
        plan,
        VideoOptions::default(),
        target.clone(),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
    .unwrap();
    assert_eq!(
        (
            stats.width,
            stats.height,
            stats.written_frames,
            stats.duration_ms
        ),
        (66, 68, 24, 800)
    );
    assert_eq!(&std::fs::read(&target).unwrap()[4..8], b"ftyp");
    assert_eq!(
        std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|f| f.file_name().to_string_lossy().starts_with(".blink-media-"))
            .count(),
        0
    );
    eprintln!("real MP4 stats: {stats:?}");
    for preset in [
        VideoPreset::General,
        VideoPreset::HighQuality,
        VideoPreset::Text,
        VideoPreset::Compact,
    ] {
        let runtime = crate::managed_runtime::ffmpeg::test_lease_at(temp.path());
        let stats = export_mp4(
            &task,
            runtime,
            Synthetic {
                width: 65,
                height: 67,
                frames: 1,
            },
            VideoPlan {
                width: 65,
                height: 67,
                frames: frames(&[100]),
            },
            VideoOptions {
                preset: Some(preset),
                ..Default::default()
            },
            temp.path().join(format!("preset-{preset:?}.mp4")),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert_eq!(
            (stats.width, stats.height, stats.written_frames),
            (66, 68, 3)
        );
        assert_eq!(
            stats.codec,
            if preset == VideoPreset::Compact {
                "hevc"
            } else {
                "h264"
            }
        );
    }
    for (w, h, durations) in [(2560, 1600, vec![250; 4]), (64, 64, vec![6000])] {
        let runtime = crate::managed_runtime::ffmpeg::test_lease_at(temp.path());
        let count = durations.len() as u32;
        let stats = export_mp4(
            &task,
            runtime,
            Synthetic {
                width: w,
                height: h,
                frames: count,
            },
            VideoPlan {
                width: w,
                height: h,
                frames: frames(&durations),
            },
            VideoOptions::default(),
            temp.path().join(format!("{w}-{h}.mp4")),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert_eq!(
            stats.written_frames,
            (durations.iter().sum::<u32>() as u64 * 30).div_ceil(1000) as u32
        );
        eprintln!("budget sample: {stats:?}");
    }
    let runtime = crate::managed_runtime::ffmpeg::test_lease_at(temp.path());
    let mut opts = VideoOptions::default();
    opts.budget.max_output_bytes = 100;
    let rejected = temp.path().join("rejected.mp4");
    let result = export_mp4(
        &task,
        runtime,
        Synthetic {
            width: 65,
            height: 67,
            frames: 1,
        },
        VideoPlan {
            width: 65,
            height: 67,
            frames: frames(&[1000]),
        },
        opts,
        rejected.clone(),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await;
    assert!(result.is_err());
    assert!(!rejected.exists());
    for scenario in ["cancel-verify", "target-race", "timeout"] {
        let runtime = crate::managed_runtime::ffmpeg::test_lease_at(temp.path());
        let target = temp.path().join(format!("{scenario}.mp4"));
        let token = tokio_util::sync::CancellationToken::new();
        let callback_token = token.clone();
        let callback_target = target.clone();
        let mut options = VideoOptions::default();
        if scenario == "timeout" {
            options.budget.timeout = std::time::Duration::from_millis(1);
        }
        let result = export_mp4(
            &task,
            runtime,
            Synthetic {
                width: 65,
                height: 67,
                frames: 1,
            },
            VideoPlan {
                width: 65,
                height: 67,
                frames: frames(&[500]),
            },
            options,
            target.clone(),
            token,
            Arc::new(move |p| {
                if scenario == "cancel-verify" && p.stage == "verifying" {
                    callback_token.cancel();
                }
                if scenario == "target-race" && p.stage == "committing" {
                    std::fs::write(&callback_target, b"keep").unwrap();
                }
            }),
        )
        .await;
        match scenario {
            "cancel-verify" => {
                assert!(matches!(result, Err(MediaError::Cancelled)));
                assert!(!target.exists());
            }
            "target-race" => {
                assert!(result.is_err());
                assert_eq!(std::fs::read(&target).unwrap(), b"keep");
            }
            _ => {
                assert!(matches!(result, Err(MediaError::Timeout)));
                assert!(!target.exists());
            }
        }
    }
    assert_eq!(
        std::fs::read_dir(temp.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|f| f.file_name().to_string_lossy().starts_with(".blink-media-"))
            .count(),
        0
    );
}
