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
fn sixty_fps_long_sampling_crosses_old_limits_with_only_a_sparse_directory() {
    let frames = FrameList::generated(108_000, |i| PlanFrame {
        src_idx: i as u32,
        duration_ms: (((i + 1) as u64 * 1000 / 60) - (i as u64 * 1000 / 60)) as u32,
    });
    let plan = SamplingPlan::from_frames(frames, 108_000, &Budget::default(), 60).unwrap();
    assert_eq!(plan.output_frames, 108_000);
    assert_eq!(plan.duration_ms(), 1_800_000);
    assert_eq!(plan.source_index(0), Some(0));
    assert_eq!(plan.source_index(107_999), Some(107_999));
    assert_eq!(plan.source_index(108_000), None);
    assert!(Budget::default().max_output_bytes > 256 << 20);
    assert!(
        Budget::for_work(1_800_000, 108_000, 1920, 1080).timeout
            > std::time::Duration::from_secs(1800)
    );
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
fn long_hold_respects_explicit_budget_without_allocating_samples() {
    let budget = Budget {
        max_duration_ms: 600_000,
        ..Default::default()
    };
    assert!(matches!(
        SamplingPlan::new(&frames(&[600_001]), 1, &budget),
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

#[tokio::test]
#[ignore = "需要 BLINK_FFMPEG_TEST_ARCHIVE 指向锁定发行包；验证各帧率的有声 MP4 尾部补齐"]
async fn real_mp4_audio_tail_at_each_frame_rate() {
    let temp = tempfile::tempdir().unwrap();
    let _runtime = crate::managed_runtime::ffmpeg::test_archive_lease(temp.path()).await;
    let task = acquire_task_lease_at(&temp.path().join("task.lock")).unwrap();
    let audio_dir = temp.path().join("audio");
    std::fs::create_dir(&audio_dir).unwrap();
    crate::gif::audio::write_manifest(
        &audio_dir,
        &crate::gif::audio::AudioManifest {
            version: 1,
            enabled: [true, false],
            committed_ms: 1001,
            complete: true,
            ..Default::default()
        },
    )
    .unwrap();
    let samples: Vec<u8> = (0..48_048)
        .flat_map(|i| {
            let v = ((i as f64 * 440.0 * std::f64::consts::TAU / 48_000.0).sin() * 8000.0) as i16;
            [v.to_le_bytes(), v.to_le_bytes()].concat()
        })
        .collect();
    std::fs::write(crate::gif::audio::track_path(&audio_dir, 0), samples).unwrap();
    for fps in [10, 15, 24, 30, 60] {
        let stats = export_mp4(
            &task,
            crate::managed_runtime::ffmpeg::test_lease_at(temp.path()),
            Synthetic {
                width: 64,
                height: 64,
                frames: 1,
            },
            VideoPlan {
                width: 64,
                height: 64,
                frames: frames(&[1001]).into(),
            },
            VideoOptions {
                fps,
                audio: Some(AudioInput {
                    dir: audio_dir.clone(),
                    spans: vec![AudioSpan {
                        source_ms: 0,
                        sound_ms: 1001,
                        silence_ms: 0,
                        output_ms: 1001,
                    }],
                    gain: [1.0, 0.0],
                }),
                ..Default::default()
            },
            temp.path().join(format!("audio-{fps}.mp4")),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        assert!(stats.has_audio);
        assert_eq!(stats.fps, fps);
        assert_eq!(stats.written_frames, fps + 1);
        assert_eq!(
            stats.duration_ms,
            ((fps as u64 + 1) * 1000).div_ceil(fps as u64)
        );
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
        frames: frames(&[33, 66, 700]).into(),
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
    let sixty = export_mp4(
        &task,
        crate::managed_runtime::ffmpeg::test_lease_at(temp.path()),
        Synthetic {
            width: 1920,
            height: 1080,
            frames: 60,
        },
        VideoPlan {
            width: 1920,
            height: 1080,
            frames: FrameList::generated(60, |i| PlanFrame {
                src_idx: i as u32,
                duration_ms: (((i + 1) * 1000 / 60) - (i * 1000 / 60)) as u32,
            }),
        },
        VideoOptions {
            fps: 60,
            ..Default::default()
        },
        temp.path().join("1080p60.mp4"),
        tokio_util::sync::CancellationToken::new(),
        Arc::new(|_| {}),
    )
    .await
    .unwrap();
    assert_eq!(
        (sixty.fps, sixty.written_frames, sixty.duration_ms),
        (60, 60, 1000)
    );
    eprintln!("real 1080p60 MP4 stats: {sixty:?}");
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
                frames: frames(&[100]).into(),
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
    for (w, h, durations) in [
        (2560, 1600, vec![250; 4]),
        (64, 64, vec![6000]),
        (64, 64, vec![1_800_000]),
    ] {
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
                frames: frames(&durations).into(),
            },
            VideoOptions::default(),
            temp.path()
                .join(format!("{w}-{h}-{}.mp4", durations.iter().sum::<u32>())),
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
            frames: frames(&[1000]).into(),
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
                frames: frames(&[500]).into(),
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

#[tokio::test]
#[ignore = "需要锁定 FFmpeg 发行包；真实静态/滚动/高动态与有声/无声估算校准"]
async fn real_mp4_estimate_fixtures() {
    use crate::gif::audio::{self, AudioManifest};
    use std::io::Write;
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let temp = tempfile::tempdir().unwrap();
    let initial = crate::managed_runtime::ffmpeg::test_archive_lease(temp.path()).await;
    drop(initial);
    let task = acquire_task_lease_at(&temp.path().join("task.lock")).unwrap();
    let audio_dir = temp.path().join("audio");
    std::fs::create_dir(&audio_dir).unwrap();
    audio::write_manifest(
        &audio_dir,
        &AudioManifest {
            version: 1,
            enabled: [true, false],
            committed_ms: 30_000,
            complete: true,
            ..Default::default()
        },
    )
    .unwrap();
    let mut pcm =
        std::io::BufWriter::new(std::fs::File::create(audio::track_path(&audio_dir, 0)).unwrap());
    for i in 0..30 * 48_000 {
        let v = if i % 109 < 55 { 1000i16 } else { -1000i16 };
        pcm.write_all(&v.to_le_bytes()).unwrap();
        pcm.write_all(&v.to_le_bytes()).unwrap();
    }
    pcm.flush().unwrap();
    drop(pcm);
    struct Content {
        mode: u32,
    }
    impl FrameSource for Content {
        fn dimensions(&self) -> (u32, u32) {
            (80, 48)
        }
        fn frame_count(&self) -> u32 {
            300
        }
        fn read_bgra(&mut self, index: u32) -> Result<Vec<u8>, MediaError> {
            let mut pixels = vec![0u8; 80 * 48 * 4];
            let mut random = index.wrapping_add(7);
            for (i, p) in pixels.chunks_exact_mut(4).enumerate() {
                let v = match self.mode {
                    0 => ((i / 80 / 8 + i % 80 / 8) % 2 * 180) as u8,
                    1 => (((i / 80 + index as usize) / 6) % 2 * 180) as u8,
                    _ => {
                        random = random.wrapping_mul(1664525).wrapping_add(1013904223);
                        (random >> 24) as u8
                    }
                };
                p.copy_from_slice(&[v, v.wrapping_mul(3), v.wrapping_add(30), 255]);
            }
            Ok(pixels)
        }
    }
    for (mode, label) in [(0, "static"), (1, "scroll"), (2, "dynamic")] {
        let plan = VideoPlan {
            width: 80,
            height: 48,
            frames: FrameList::generated(300, |i| PlanFrame {
                src_idx: i as u32,
                duration_ms: 100,
            }),
        };
        let sample = VideoPlan {
            frames: FrameList::generated(96, |i| PlanFrame {
                src_idx: ((i / 32) * (300 - 32) / 2 + i % 32) as u32,
                duration_ms: 62,
            }),
            ..plan.clone()
        };
        let sampled = export_mp4(
            &task,
            crate::managed_runtime::ffmpeg::test_lease_at(temp.path()),
            Content { mode },
            sample,
            VideoOptions {
                fps: 10,
                ..Default::default()
            },
            temp.path().join(format!("sample-{label}.mp4")),
            tokio_util::sync::CancellationToken::new(),
            Arc::new(|_| {}),
        )
        .await
        .unwrap();
        for has_audio in [false, true] {
            let audio = has_audio.then(|| AudioInput {
                dir: audio_dir.clone(),
                spans: vec![super::audio::Span {
                    source_ms: 0,
                    sound_ms: 30_000,
                    silence_ms: 0,
                    output_ms: 30_000,
                }],
                gain: [1.0, 0.0],
            });
            let actual = export_mp4(
                &task,
                crate::managed_runtime::ffmpeg::test_lease_at(temp.path()),
                Content { mode },
                plan.clone(),
                VideoOptions {
                    fps: 10,
                    audio,
                    ..Default::default()
                },
                temp.path().join(format!("actual-{label}-{has_audio}.mp4")),
                tokio_util::sync::CancellationToken::new(),
                Arc::new(|_| {}),
            )
            .await
            .unwrap();
            assert_eq!(actual.has_audio, has_audio);
            let range = estimate::mp4_size_range(sampled.bytes, 96 * 62, 30_000, has_audio);
            tracing::info!(
                label,
                has_audio,
                lower = range.lower,
                upper = range.upper,
                actual = actual.bytes,
                peak = actual.peak_child_bytes,
                "MP4 estimate fixture"
            );
            assert!(
                actual.bytes >= range.lower && actual.bytes <= range.upper,
                "{label} audio={has_audio}: actual={} range={range:?}",
                actual.bytes
            );
        }
    }
}
