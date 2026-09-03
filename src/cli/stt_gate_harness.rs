//! STT 生产 Gate Harness（0.22.9 Handoff 07B）。
//!
//! 隐藏内部 CLI 入口——不进入普通 CLI help / Capability / MCP。
//! 通过真实 production launcher（`ParaformerOnlineAdapter::launch`）、
//! `ManagedProcess`、`StreamWorkerClient` 和 `ParaformerOnline` runner 投喂音频。
//!
//! ## 两个子模式
//!
//! 1. **延迟 harness** (`--mode latency`)：WAV 解码后按 16kHz 实时时钟投喂，
//!    记录 first-partial / final-after-release / RTF / 队列峰值 / Busy / 内存。
//! 2. **生命周期 harness** (`--mode lifecycle`)：100 次 start→Begin→Audio→Cancel/End→stop
//!    + 10 次 worker kill→wait→restart→Ready + orphan/泄漏检查。
//!
//! ## 设计铁则
//!
//! - 不调用 Spike E 的 NDJSON worker 或绕过生产路径
//! - 不计算或宣称 CER
//! - 不注册模型、不改变默认模型/VAD
//! - 不实现 FSMN-VAD
//! - 不改 UI、不改 phase/spec/product 文档
//! - WAV 解码后按 16kHz 真实时钟投喂（每 10ms 发送 160 samples）
//! - 使用 deadline/interval 校正累计漂移
//! - 计时起点为第一个有效语音样本进入 production STT port
//! - first-partial 终点为 VoiceService 可消费的非空 Partial 到达
//! - final-after-release 从 End/hold release 到非空 Final 到达

use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::domain::stt::{StreamingSttPort, SttEvent};
use crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter;

// ── 参数解析 ─────────────────────────────────────────────────────────────

/// Gate harness 参数。
struct HarnessArgs {
    /// `--deployment <dir>`：deployment 目录路径
    deployment: PathBuf,
    /// `--wav <path>`：WAV 文件路径
    wav: PathBuf,
    /// `--mode <latency|lifecycle>`：harness 模式
    mode: HarnessMode,
    /// `--output <path>`：JSON 结果输出路径
    output: PathBuf,
    /// `--rounds <N>`：延迟 harness 轮数（默认 20）
    rounds: usize,
    /// `--lifecycle-count <N>`：生命周期 harness 的 start/stop 次数（默认 100）
    lifecycle_count: usize,
    /// `--kill-count <N>`：kill/restart 次数（默认 10）
    kill_count: usize,
    /// `--ready-timeout <secs>`：Ready 超时秒数（默认 120）
    ready_timeout_secs: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HarnessMode {
    Latency,
    Lifecycle,
}

fn parse_args(args: &[String]) -> Result<HarnessArgs, String> {
    let mut deployment = None;
    let mut wav = None;
    let mut mode = None;
    let mut output = None;
    let mut rounds = 20usize;
    let mut lifecycle_count = 100usize;
    let mut kill_count = 10usize;
    let mut ready_timeout_secs = 120u64;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--deployment" => {
                i += 1;
                deployment = args.get(i).map(PathBuf::from);
            }
            "--wav" => {
                i += 1;
                wav = args.get(i).map(PathBuf::from);
            }
            "--mode" => {
                i += 1;
                mode = args.get(i).and_then(|s| match s.as_str() {
                    "latency" => Some(HarnessMode::Latency),
                    "lifecycle" => Some(HarnessMode::Lifecycle),
                    _ => None,
                });
                if mode.is_none() {
                    return Err(format!(
                        "未知 mode: {}",
                        args.get(i).map(|s| s.as_str()).unwrap_or("")
                    ));
                }
            }
            "--output" => {
                i += 1;
                output = args.get(i).map(PathBuf::from);
            }
            "--rounds" => {
                i += 1;
                rounds = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --rounds")?;
            }
            "--lifecycle-count" => {
                i += 1;
                lifecycle_count = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --lifecycle-count")?;
            }
            "--kill-count" => {
                i += 1;
                kill_count = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --kill-count")?;
            }
            "--ready-timeout" => {
                i += 1;
                ready_timeout_secs = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --ready-timeout")?;
            }
            _ => {
                // 未知参数，忽略
            }
        }
        i += 1;
    }

    let deployment = deployment.ok_or("缺少 --deployment 参数")?;
    let wav = wav.ok_or("缺少 --wav 参数")?;
    let mode = mode.ok_or("缺少 --mode 参数（latency 或 lifecycle）")?;
    let output = output.ok_or("缺少 --output 参数")?;

    Ok(HarnessArgs {
        deployment,
        wav,
        mode,
        output,
        rounds,
        lifecycle_count,
        kill_count,
        ready_timeout_secs,
    })
}

/// 从 CLI 参数运行 gate harness。
///
/// 返回 exit code（0=成功，1=失败）。
pub fn run_from_args(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stt-gate-harness: 参数解析失败: {e}");
            eprintln!(
                "用法: blink stt-gate-harness --deployment <dir> --wav <path> \
                 --mode <latency|lifecycle> --output <path> \
                 [--rounds N] [--lifecycle-count N] [--kill-count N] \
                 [--ready-timeout secs]"
            );
            return 1;
        }
    };

    // 初始化 stderr tracing
    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();

    // 创建 tokio runtime
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("stt-gate-harness: 创建 tokio runtime 失败: {e}");
            return 1;
        }
    };

    runtime.block_on(async move {
        match parsed.mode {
            HarnessMode::Latency => run_latency_harness(&parsed).await,
            HarnessMode::Lifecycle => run_lifecycle_harness(&parsed).await,
        }
    })
}

// ── WAV 解码（非 cfg(test) 版本）──────────────────────────────────────────

/// 解析 WAV 文件为 f32 PCM 样本（16-bit, 16kHz, mono）。
///
/// 与 `domain::stt::wav::parse_wav_to_f32` 逻辑相同，
/// 但该函数只在 `#[cfg(test)]` 下可用，此处需要生产版本。
fn parse_wav_to_f32(data: &[u8]) -> Result<Vec<f32>, String> {
    if data.len() < 44 || &data[0..4] != b"RIFF" || &data[8..12] != b"WAVE" {
        return Err("不是有效的 WAV 文件".to_string());
    }

    let mut offset = 12;
    let mut samples = Vec::new();

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = u32::from_le_bytes([
            data[offset + 4],
            data[offset + 5],
            data[offset + 6],
            data[offset + 7],
        ]) as usize;

        if chunk_id == b"data" {
            let data_start = offset + 8;
            let data_end = (data_start + chunk_size).min(data.len());
            let pcm_bytes = &data[data_start..data_end];

            samples = pcm_bytes
                .chunks_exact(2)
                .map(|chunk| {
                    let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
                    sample as f32 / 32768.0
                })
                .collect();
            break;
        }

        offset += 8 + chunk_size;
        if chunk_size % 2 == 1 {
            offset += 1;
        }
    }

    if samples.is_empty() {
        return Err("WAV 中未找到 PCM data".to_string());
    }

    Ok(samples)
}

// ── 内存快照 ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemorySnapshot {
    /// 主进程工作集（MB）
    main_process_mb: f64,
    /// worker 子进程工作集（MB），无则为 None
    worker_process_mb: Option<f64>,
    /// 快照时间戳 / 标签
    timestamp: String,
}

#[cfg(windows)]
fn get_process_memory_mb(pid: u32) -> Option<f64> {
    use windows::Win32::System::ProcessStatus::GetProcessMemoryInfo;
    use windows::Win32::System::ProcessStatus::PROCESS_MEMORY_COUNTERS;

    unsafe {
        let handle = windows::Win32::System::Threading::OpenProcess(
            windows::Win32::System::Threading::PROCESS_QUERY_INFORMATION
                | windows::Win32::System::Threading::PROCESS_VM_READ,
            false,
            pid,
        )
        .ok()?;

        let mut counters = PROCESS_MEMORY_COUNTERS::default();
        let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(handle, &mut counters, size).ok()?;

        Some(counters.WorkingSetSize as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(not(windows))]
fn get_process_memory_mb(_pid: u32) -> Option<f64> {
    None
}

fn take_memory_snapshot(worker_pid: Option<u32>, label: &str) -> MemorySnapshot {
    let main_pid = std::process::id();
    let main_process_mb = get_process_memory_mb(main_pid).unwrap_or(0.0);
    let worker_process_mb = worker_pid.and_then(get_process_memory_mb);

    MemorySnapshot {
        main_process_mb,
        worker_process_mb,
        timestamp: label.to_string(),
    }
}

/// 简单时间戳（不引入 chrono 依赖）。
fn now_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("t+{}ms", now.as_millis())
}

// ── 统计辅助 ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct PercentileStats {
    count: usize,
    min: f64,
    max: f64,
    mean: f64,
    p50: f64,
    p95: f64,
}

fn compute_percentiles(values: &[f64]) -> Option<PercentileStats> {
    if values.is_empty() {
        return None;
    }

    let mut sorted = values.to_vec();
    sorted.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));

    let n = sorted.len();
    let min = sorted[0];
    let max = sorted[n - 1];
    let mean = sorted.iter().sum::<f64>() / n as f64;

    let percentile = |p: f64| -> f64 {
        if n == 1 {
            return sorted[0];
        }
        let idx = ((p / 100.0) * (n - 1) as f64).floor() as usize;
        sorted[idx.min(n - 1)]
    };

    Some(PercentileStats {
        count: n,
        min,
        max,
        mean,
        p50: percentile(50.0),
        p95: percentile(95.0),
    })
}

// ── 元数据 ───────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct HarnessMetadata {
    version: String,
    wav_path: String,
    deployment_dir: String,
    rounds: usize,
    ready_timeout_secs: u64,
    mode: String,
    repro_command: String,
    generated_at: String,
}

// ── 延迟 Harness ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LatencyRoundResult {
    round: usize,
    /// 从第一个有效语音样本进入 STT port 到收到非空 Partial 的时间（ms）
    first_partial_ms: Option<u64>,
    /// 从 End 信号到收到非空 Final 的时间（ms）
    final_after_release_ms: Option<u64>,
    /// 本轮音频时长（ms）
    audio_duration_ms: u64,
    /// 本轮推理总时间（ms）——从 begin 到 Final 到达
    inference_wall_ms: u64,
    /// RTF = inference_wall_ms / audio_duration_ms
    rtf: f64,
    /// 队列峰值（Busy 事件数）
    busy_count: u32,
    /// 收到的 Partial 文本
    partial_text: Option<String>,
    /// 收到的 Final 文本
    final_text: Option<String>,
    /// 错误（如有）
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LatencyHarnessReport {
    metadata: HarnessMetadata,
    rounds: Vec<LatencyRoundResult>,
    first_partial_stats: Option<PercentileStats>,
    final_after_release_stats: Option<PercentileStats>,
    rtf_stats: Option<PercentileStats>,
    total_busy: u32,
    total_errors: u32,
    memory_snapshots: Vec<MemorySnapshot>,
    timing_notes: String,
}

async fn run_latency_harness(args: &HarnessArgs) -> i32 {
    let deployment_dir = &args.deployment;
    let wav_path = &args.wav;
    let output_path = &args.output;
    let ready_timeout = Duration::from_secs(args.ready_timeout_secs);

    tracing::info!(
        deployment = %deployment_dir.display(),
        wav = %wav_path.display(),
        rounds = args.rounds,
        "stt-gate-harness: 启动延迟 harness"
    );

    // 读取 WAV
    let wav_bytes = match std::fs::read(wav_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(%e, "读取 WAV 文件失败");
            return 1;
        }
    };

    let samples = match parse_wav_to_f32(&wav_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "解析 WAV 失败");
            return 1;
        }
    };

    let audio_duration_ms = (samples.len() / 16) as u64;
    tracing::info!(
        samples = samples.len(),
        duration_ms = audio_duration_ms,
        "WAV 解码成功"
    );

    // 标注：仅技术延迟，不作为 CER/质量语料
    tracing::info!("NOTE: 此 WAV 仅用于技术延迟测量，不作为 CER/质量语料");

    let mut rounds_results = Vec::with_capacity(args.rounds);
    let mut memory_snapshots = Vec::new();
    let mut total_busy = 0u32;
    let mut total_errors = 0u32;

    // 启动 worker
    tracing::info!("启动 ParaformerOnline worker...");
    let launch_start = Instant::now();
    let adapter = match ParaformerOnlineAdapter::launch(deployment_dir.clone(), ready_timeout).await
    {
        Ok(a) => {
            tracing::info!(
                launch_ms = launch_start.elapsed().as_millis(),
                "worker 启动成功"
            );
            a
        }
        Err(e) => {
            tracing::error!(%e, "worker 启动失败");
            return 1;
        }
    };

    memory_snapshots.push(take_memory_snapshot(None, "worker_ready_initial"));

    // 每轮延迟测量
    for round in 0..args.rounds {
        match run_latency_round(&adapter, &samples, audio_duration_ms, round).await {
            Ok(r) => {
                total_busy += r.busy_count;
                if r.error.is_some() {
                    total_errors += 1;
                }
                rounds_results.push(r);
            }
            Err(e) => {
                total_errors += 1;
                rounds_results.push(LatencyRoundResult {
                    round,
                    first_partial_ms: None,
                    final_after_release_ms: None,
                    audio_duration_ms,
                    inference_wall_ms: 0,
                    rtf: 0.0,
                    busy_count: 0,
                    partial_text: None,
                    final_text: None,
                    error: Some(e),
                });
            }
        }

        // 每轮后 reset
        let _ = adapter.reset().await;
    }

    memory_snapshots.push(take_memory_snapshot(None, "after_all_rounds"));

    // 停止 worker
    let _ = adapter.stop().await;
    tracing::info!("worker 已停止");

    // 计算统计
    let first_partial_values: Vec<f64> = rounds_results
        .iter()
        .filter_map(|r| r.first_partial_ms.map(|v| v as f64))
        .collect();
    let final_after_values: Vec<f64> = rounds_results
        .iter()
        .filter_map(|r| r.final_after_release_ms.map(|v| v as f64))
        .collect();
    let rtf_values: Vec<f64> = rounds_results
        .iter()
        .filter(|r| r.error.is_none() && r.rtf > 0.0)
        .map(|r| r.rtf)
        .collect();

    let first_partial_stats = compute_percentiles(&first_partial_values);
    let final_after_release_stats = compute_percentiles(&final_after_values);
    let rtf_stats = compute_percentiles(&rtf_values);

    let metadata = HarnessMetadata {
        version: "0.22.9-handoff-07b".to_string(),
        wav_path: wav_path.to_string_lossy().to_string(),
        deployment_dir: deployment_dir.to_string_lossy().to_string(),
        rounds: args.rounds,
        ready_timeout_secs: args.ready_timeout_secs,
        mode: "latency".to_string(),
        repro_command: format!(
            "blink.exe stt-gate-harness --deployment {} --wav {} \
             --mode latency --output {} --rounds {}",
            deployment_dir.display(),
            wav_path.display(),
            output_path.display(),
            args.rounds
        ),
        generated_at: now_timestamp(),
    };

    let timing_notes = "timing 计算说明：\
        \n  - first_partial_ms: 从第一个有效语音样本进入 production STT port \
          (begin_session 返回后第一次 push_audio) 到收到非空 Partial 的墙钟时间。\
        \n  - final_after_release_ms: 从调用 finish_session (End 信号) \
          到收到非空 Final 的墙钟时间。\
        \n  - RTF = inference_wall_ms / audio_duration_ms \
          (inference_wall_ms = 从 begin_session 到 Final 到达)。\
        \n  - 音频按 16kHz 实时时钟投喂（每 10ms 发送 160 samples），\
          使用 tokio::time::interval 校正累计漂移。\
        \n  - 此 WAV 仅用于技术延迟验证，不作为 CER/质量语料。"
        .to_string();

    let report = LatencyHarnessReport {
        metadata,
        rounds: rounds_results,
        first_partial_stats,
        final_after_release_stats,
        rtf_stats,
        total_busy,
        total_errors,
        memory_snapshots,
        timing_notes,
    };

    let json = match serde_json::to_string_pretty(&report) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(%e, "JSON 序列化失败");
            return 1;
        }
    };

    if let Err(e) = std::fs::write(output_path, json) {
        tracing::error!(%e, "写入输出文件失败");
        return 1;
    }

    tracing::info!(
        rounds = args.rounds,
        total_busy,
        total_errors,
        output = %output_path.display(),
        "延迟 harness 完成"
    );

    if let Some(ref stats) = report.first_partial_stats {
        println!(
            "first-partial: p50={:.0}ms p95={:.0}ms (n={})",
            stats.p50, stats.p95, stats.count
        );
    } else {
        println!("first-partial: 无数据");
    }
    if let Some(ref stats) = report.final_after_release_stats {
        println!(
            "final-after-release: p50={:.0}ms p95={:.0}ms (n={})",
            stats.p50, stats.p95, stats.count
        );
    } else {
        println!("final-after-release: 无数据");
    }
    if let Some(ref stats) = report.rtf_stats {
        println!(
            "RTF: p50={:.3} p95={:.3} (n={})",
            stats.p50, stats.p95, stats.count
        );
    } else {
        println!("RTF: 无数据");
    }

    0
}

/// 执行一轮延迟测量。
async fn run_latency_round(
    adapter: &ParaformerOnlineAdapter,
    samples: &[f32],
    audio_duration_ms: u64,
    round: usize,
) -> Result<LatencyRoundResult, String> {
    let begin_start = Instant::now();
    let generation = adapter
        .begin_session()
        .await
        .map_err(|e| format!("begin_session 失败: {e}"))?;

    let mut rx = adapter.events();

    // 实时投喂音频：每 10ms 投喂 160 samples
    let chunk_size = 160usize;
    let mut first_partial_time: Option<Instant> = None;
    let mut busy_count = 0u32;
    let mut partial_text = None;
    let mut feed_start: Option<Instant> = None;

    // 使用 tokio::time::interval 校正累计漂移
    let mut interval = tokio::time::interval(Duration::from_millis(10));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

    for chunk in samples.chunks(chunk_size) {
        interval.tick().await;

        if feed_start.is_none() {
            feed_start = Some(Instant::now());
        }

        if let Err(e) = adapter.push_audio(generation, chunk).await {
            tracing::warn!(round, %e, "push_audio 失败");
        }

        // 非阻塞检查是否有事件
        while let Ok(event) = rx.try_recv() {
            match event {
                SttEvent::Partial {
                    generation: evt_gen,
                    confirmed,
                    preview,
                } => {
                    if evt_gen == generation
                        && (!confirmed.is_empty() || !preview.is_empty())
                        && first_partial_time.is_none()
                    {
                        first_partial_time = Some(Instant::now());
                        partial_text = Some(format!("{confirmed}{preview}"));
                        tracing::info!(
                            round,
                            ms = feed_start.map(|s| s.elapsed().as_millis()).unwrap_or(0),
                            "first partial 到达"
                        );
                    }
                }
                SttEvent::Busy { .. } => {
                    busy_count += 1;
                }
                SttEvent::Error {
                    generation: evt_gen,
                    message,
                } if evt_gen == generation => {
                    return Err(format!("STT Error: {message}"));
                }
                _ => {}
            }
        }
    }

    // finish_session
    let finish_start = Instant::now();
    if let Err(e) = adapter.finish_session(generation).await {
        tracing::warn!(round, %e, "finish_session 失败");
    }

    // 等待 Final 事件（带超时）
    let mut final_text = None;
    let mut final_arrived = false;
    let deadline = Instant::now() + Duration::from_secs(30);

    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            break;
        }

        match tokio::time::timeout(remaining, rx.recv()).await {
            Ok(Some(event)) => match event {
                SttEvent::Final {
                    generation: evt_gen,
                    text,
                } if evt_gen == generation => {
                    final_text = Some(text);
                    final_arrived = true;
                    break;
                }
                SttEvent::Busy { .. } => {
                    busy_count += 1;
                }
                SttEvent::Error {
                    generation: evt_gen,
                    message,
                } if evt_gen == generation => {
                    return Ok(LatencyRoundResult {
                        round,
                        first_partial_ms: first_partial_time
                            .zip(feed_start)
                            .map(|(fp, fs)| fp.duration_since(fs).as_millis() as u64),
                        final_after_release_ms: None,
                        audio_duration_ms,
                        inference_wall_ms: begin_start.elapsed().as_millis() as u64,
                        rtf: 0.0,
                        busy_count,
                        partial_text,
                        final_text: None,
                        error: Some(format!("STT Error: {message}")),
                    });
                }
                _ => {}
            },
            Ok(None) => break,
            Err(_) => break,
        }
    }

    let finish_elapsed = finish_start.elapsed();
    let inference_wall = begin_start.elapsed();

    let first_partial_ms = first_partial_time
        .zip(feed_start)
        .map(|(fp, fs)| fp.duration_since(fs).as_millis() as u64);

    let final_after_release_ms = if final_arrived {
        Some(finish_elapsed.as_millis() as u64)
    } else {
        None
    };

    let rtf = if audio_duration_ms > 0 {
        inference_wall.as_millis() as f64 / audio_duration_ms as f64
    } else {
        0.0
    };

    tracing::info!(
        round,
        first_partial_ms = first_partial_ms.unwrap_or(0),
        final_after_release_ms = final_after_release_ms.unwrap_or(0),
        rtf = rtf,
        busy = busy_count,
        "round 完成"
    );

    Ok(LatencyRoundResult {
        round,
        first_partial_ms,
        final_after_release_ms,
        audio_duration_ms,
        inference_wall_ms: inference_wall.as_millis() as u64,
        rtf,
        busy_count,
        partial_text,
        final_text,
        error: None,
    })
}

// ── 生命周期 Harness ─────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LifecycleRoundResult {
    round: usize,
    operation: String,
    success: bool,
    elapsed_ms: u64,
    error: Option<String>,
    orphan_detected: bool,
    stale_generation: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct KillRestartRoundResult {
    round: usize,
    kill_ms: u64,
    wait_ms: u64,
    restart_ms: u64,
    success: bool,
    error: Option<String>,
    orphan_pid: Option<u32>,
    memory_after_restart: Option<MemorySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LifecycleHarnessReport {
    metadata: HarnessMetadata,
    lifecycle_rounds: Vec<LifecycleRoundResult>,
    kill_restart_rounds: Vec<KillRestartRoundResult>,
    summary: LifecycleSummary,
    memory_snapshots: Vec<MemorySnapshot>,
    orphan_check: OrphanCheckResult,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LifecycleSummary {
    total_start_stop: usize,
    total_cancel: usize,
    total_end: usize,
    success_count: usize,
    failure_count: usize,
    orphan_count: usize,
    stale_generation_count: usize,
    deadlock_count: usize,
    reset_reproducible: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct OrphanCheckResult {
    method: String,
    orphan_pids: Vec<u32>,
    has_orphans: bool,
}

async fn run_lifecycle_harness(args: &HarnessArgs) -> i32 {
    let deployment_dir = &args.deployment;
    let wav_path = &args.wav;
    let output_path = &args.output;
    let ready_timeout = Duration::from_secs(args.ready_timeout_secs);

    tracing::info!(
        deployment = %deployment_dir.display(),
        lifecycle_count = args.lifecycle_count,
        kill_count = args.kill_count,
        "stt-gate-harness: 启动生命周期 harness"
    );

    // 读取 WAV
    let wav_bytes = match std::fs::read(wav_path) {
        Ok(b) => b,
        Err(e) => {
            tracing::error!(%e, "读取 WAV 文件失败");
            return 1;
        }
    };
    let samples = match parse_wav_to_f32(&wav_bytes) {
        Ok(s) => s,
        Err(e) => {
            tracing::error!(%e, "解析 WAV 失败");
            return 1;
        }
    };

    let mut lifecycle_rounds = Vec::with_capacity(args.lifecycle_count);
    let mut kill_restart_rounds = Vec::with_capacity(args.kill_count);
    let mut memory_snapshots = Vec::new();

    // ── Phase 1: N 次 start → Begin → Audio → Cancel/End → stop ──────
    tracing::info!(
        "Phase 1: 开始 {} 次 start/stop/cancel 循环",
        args.lifecycle_count
    );

    let mut adapter =
        match ParaformerOnlineAdapter::launch(deployment_dir.clone(), ready_timeout).await {
            Ok(a) => a,
            Err(e) => {
                tracing::error!(%e, "初始 worker 启动失败");
                return 1;
            }
        };

    memory_snapshots.push(take_memory_snapshot(None, "initial_worker_ready"));

    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut orphan_count = 0usize;
    let mut stale_gen_count = 0usize;
    let mut deadlock_count = 0usize;
    let mut total_start_stop = 0usize;
    let mut total_cancel = 0usize;
    let mut total_end = 0usize;

    // reset 可复现性检查
    let mut baseline_final_text: Option<String> = None;
    let mut reset_reproducible = true;

    for round in 0..args.lifecycle_count {
        let op_start = Instant::now();
        let use_cancel = round % 2 == 0;

        let operation = if use_cancel {
            total_cancel += 1;
            "start→Begin→Audio→Cancel→stop"
        } else {
            total_end += 1;
            "start→Begin→Audio→End→stop"
        };
        total_start_stop += 1;

        let result = run_lifecycle_round(
            &mut adapter,
            &samples,
            round,
            use_cancel,
            &mut baseline_final_text,
            &mut reset_reproducible,
        )
        .await;

        let elapsed_ms = op_start.elapsed().as_millis() as u64;

        match &result {
            Ok(r) => {
                if r.success {
                    success_count += 1;
                } else {
                    failure_count += 1;
                }
                if r.orphan_detected {
                    orphan_count += 1;
                }
                if r.stale_generation {
                    stale_gen_count += 1;
                }
                lifecycle_rounds.push(r.clone());
            }
            Err(e) => {
                failure_count += 1;
                tracing::warn!(round, %e, "lifecycle round 失败");
                lifecycle_rounds.push(LifecycleRoundResult {
                    round,
                    operation: operation.to_string(),
                    success: false,
                    elapsed_ms,
                    error: Some(e.clone()),
                    orphan_detected: false,
                    stale_generation: false,
                });
            }
        }

        if elapsed_ms > 60_000 {
            deadlock_count += 1;
            tracing::warn!(round, elapsed_ms, "检测到疑似死锁（单轮 > 60s）");
        }

        let _ = adapter.reset().await;
    }

    // ── Phase 2: N 次 worker kill → wait → restart → Ready ─────────────
    tracing::info!("Phase 2: 开始 {} 次 kill/restart 循环", args.kill_count);

    // 先停止当前 worker
    let _ = adapter.stop().await;

    for round in 0..args.kill_count {
        let result = run_kill_restart_round(deployment_dir, ready_timeout, round).await;

        match &result {
            Ok(r) => {
                if !r.success {
                    failure_count += 1;
                }
                if let Some(ref mem) = r.memory_after_restart {
                    memory_snapshots.push(mem.clone());
                }
                kill_restart_rounds.push(r.clone());
            }
            Err(e) => {
                failure_count += 1;
                tracing::warn!(round, %e, "kill/restart round 失败");
                kill_restart_rounds.push(KillRestartRoundResult {
                    round,
                    kill_ms: 0,
                    wait_ms: 0,
                    restart_ms: 0,
                    success: false,
                    error: Some(e.clone()),
                    orphan_pid: None,
                    memory_after_restart: None,
                });
            }
        }
    }

    // ── Orphan 检查 ────────────────────────────────────────────────────
    let orphan_check = check_orphans().await;

    memory_snapshots.push(take_memory_snapshot(None, "final"));

    let summary = LifecycleSummary {
        total_start_stop,
        total_cancel,
        total_end,
        success_count,
        failure_count,
        orphan_count,
        stale_generation_count: stale_gen_count,
        deadlock_count,
        reset_reproducible,
    };

    let metadata = HarnessMetadata {
        version: "0.22.9-handoff-07b".to_string(),
        wav_path: wav_path.to_string_lossy().to_string(),
        deployment_dir: deployment_dir.to_string_lossy().to_string(),
        rounds: args.lifecycle_count,
        ready_timeout_secs: args.ready_timeout_secs,
        mode: "lifecycle".to_string(),
        repro_command: format!(
            "blink.exe stt-gate-harness --deployment {} --wav {} \
             --mode lifecycle --output {} --lifecycle-count {} --kill-count {}",
            deployment_dir.display(),
            wav_path.display(),
            output_path.display(),
            args.lifecycle_count,
            args.kill_count
        ),
        generated_at: now_timestamp(),
    };

    let (
        success_count,
        failure_count,
        orphan_count,
        stale_gen_count,
        deadlock_count,
        reset_reproducible_val,
    ) = (
        summary.success_count,
        summary.failure_count,
        summary.orphan_count,
        summary.stale_generation_count,
        summary.deadlock_count,
        summary.reset_reproducible,
    );
    let report = LifecycleHarnessReport {
        metadata,
        lifecycle_rounds,
        kill_restart_rounds,
        summary,
        memory_snapshots,
        orphan_check,
    };

    let json = match serde_json::to_string_pretty(&report) {
        Ok(j) => j,
        Err(e) => {
            tracing::error!(%e, "JSON 序列化失败");
            return 1;
        }
    };

    if let Err(e) = std::fs::write(output_path, json) {
        tracing::error!(%e, "写入输出文件失败");
        return 1;
    }

    tracing::info!(
        success = success_count,
        failures = failure_count,
        orphans = orphan_count,
        output = %output_path.display(),
        "生命周期 harness 完成"
    );

    println!(
        "lifecycle: {}/{} success, {} failures, {} orphans, {} stale-gen, {} deadlocks, reset_reproducible={}",
        success_count,
        success_count + failure_count,
        failure_count,
        orphan_count,
        stale_gen_count,
        deadlock_count,
        reset_reproducible_val
    );

    0
}

/// 执行一轮生命周期测试。
///
/// 1. begin_session
/// 2. 投喂少量音频（不等待实时钟——生命周期测试不测量延迟）
/// 3. cancel 或 end
/// 4. 检查 stale generation
async fn run_lifecycle_round(
    adapter: &mut ParaformerOnlineAdapter,
    samples: &[f32],
    round: usize,
    use_cancel: bool,
    baseline_final_text: &mut Option<String>,
    reset_reproducible: &mut bool,
) -> Result<LifecycleRoundResult, String> {
    let op_start = Instant::now();
    let operation = if use_cancel {
        "start→Begin→Audio→Cancel→stop"
    } else {
        "start→Begin→Audio→End→stop"
    };

    // begin
    let generation = adapter
        .begin_session()
        .await
        .map_err(|e| format!("begin_session 失败: {e}"))?;

    let mut rx = adapter.events();

    // 投喂少量音频（取前 2 秒 = 32000 samples，快速发送不等待实时钟）
    let feed_samples = &samples[..samples.len().min(32000)];
    let chunk_size = 1600usize; // 100ms chunks
    let mut stale_generation = false;

    for chunk in feed_samples.chunks(chunk_size) {
        if let Err(e) = adapter.push_audio(generation, chunk).await {
            tracing::warn!(round, %e, "push_audio 失败");
        }
        // 检查是否有旧 generation 事件泄漏
        while let Ok(event) = rx.try_recv() {
            match &event {
                SttEvent::Partial {
                    generation: evt_gen,
                    ..
                }
                | SttEvent::Final {
                    generation: evt_gen,
                    ..
                } => {
                    if *evt_gen != generation {
                        tracing::warn!(
                            round,
                            expected_gen = generation,
                            got_gen = evt_gen,
                            "检测到旧 generation 结果泄漏"
                        );
                        stale_generation = true;
                    }
                }
                SttEvent::Busy { .. } => {}
                SttEvent::Error { .. } => {}
            }
        }
    }

    let success;
    if use_cancel {
        // cancel
        adapter
            .cancel_session(generation)
            .await
            .map_err(|e| format!("cancel_session 失败: {e}"))?;
        success = true;
    } else {
        // end
        adapter
            .finish_session(generation)
            .await
            .map_err(|e| format!("finish_session 失败: {e}"))?;

        // 等待 Final（短超时）
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut got_final = false;
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                break;
            }
            match tokio::time::timeout(remaining, rx.recv()).await {
                Ok(Some(SttEvent::Final {
                    generation: evt_gen,
                    text,
                })) => {
                    if evt_gen == generation {
                        got_final = true;
                        // 检查可复现性
                        if let Some(baseline) = baseline_final_text.as_ref() {
                            if baseline != &text {
                                tracing::warn!(round, "Final 文本与 baseline 不一致");
                                *reset_reproducible = false;
                            }
                        } else {
                            *baseline_final_text = Some(text);
                        }
                        break;
                    }
                }
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        success = got_final;
    }

    let elapsed_ms = op_start.elapsed().as_millis() as u64;

    Ok(LifecycleRoundResult {
        round,
        operation: operation.to_string(),
        success,
        elapsed_ms,
        error: None,
        orphan_detected: false,
        stale_generation,
    })
}

/// 执行一轮 kill → wait → restart → Ready 测试。
async fn run_kill_restart_round(
    deployment_dir: &std::path::Path,
    ready_timeout: Duration,
    round: usize,
) -> Result<KillRestartRoundResult, String> {
    // ── 1. 启动 worker ──────────────────────────────────────────────
    let adapter = ParaformerOnlineAdapter::launch(deployment_dir.to_path_buf(), ready_timeout)
        .await
        .map_err(|e| format!("launch 失败: {e}"))?;

    // ── 2. kill worker（通过 stop 强制回收）─────────────────────────
    let kill_start = Instant::now();
    adapter
        .stop()
        .await
        .map_err(|e| format!("stop 失败: {e}"))?;
    let kill_ms = kill_start.elapsed().as_millis() as u64;

    // ── 3. wait（确认进程退出）──────────────────────────────────────
    let wait_start = Instant::now();
    // adapter drop 触发 ManagedProcess 回收
    drop(adapter);
    // 短暂等待回收完成
    tokio::time::sleep(Duration::from_millis(500)).await;
    let wait_ms = wait_start.elapsed().as_millis() as u64;

    // ── 4. 检查 orphan ───────────────────────────────────────────────
    // 查找残留的 paraformer-worker 进程
    let orphan_pid = find_orphan_worker_pid().await;

    // ── 5. restart ───────────────────────────────────────────────────
    let restart_start = Instant::now();
    let _new_adapter = ParaformerOnlineAdapter::launch(deployment_dir.to_path_buf(), ready_timeout)
        .await
        .map_err(|e| format!("restart launch 失败: {e}"))?;
    let restart_ms = restart_start.elapsed().as_millis() as u64;

    // ── 6. 内存快照 ──────────────────────────────────────────────────
    let mem = take_memory_snapshot(None, &format!("kill_restart_round_{round}_after_ready"));

    let success = orphan_pid.is_none();

    Ok(KillRestartRoundResult {
        round,
        kill_ms,
        wait_ms,
        restart_ms,
        success,
        error: None,
        orphan_pid,
        memory_after_restart: Some(mem),
    })
}

/// 查找残留的 paraformer-worker 进程 PID。
#[cfg(windows)]
async fn find_orphan_worker_pid() -> Option<u32> {
    use std::process::Command;
    let output = Command::new("tasklist")
        .args(["/FI", "IMAGENAME eq blink.exe", "/FO", "CSV", "/NH"])
        .output()
        .ok()?;

    let stdout = String::from_utf8_lossy(&output.stdout);
    // 当前进程 PID
    let current_pid = std::process::id();

    for line in stdout.lines() {
        // CSV 格式: "blink.exe","1234","..."
        if line.contains("blink.exe") {
            let parts: Vec<&str> = line.split(',').collect();
            if parts.len() >= 2 {
                let pid_str = parts[1].trim_matches('"');
                if let Ok(pid) = pid_str.parse::<u32>()
                    && pid != current_pid
                {
                    return Some(pid);
                }
            }
        }
    }
    None
}

#[cfg(not(windows))]
async fn find_orphan_worker_pid() -> Option<u32> {
    None
}

/// 全局 orphan 检查。
async fn check_orphans() -> OrphanCheckResult {
    let orphan_pids = find_orphan_worker_pid()
        .await
        .map(|p| vec![p])
        .unwrap_or_default();

    OrphanCheckResult {
        method: "tasklist filter blink.exe (excluding current PID)".to_string(),
        has_orphans: !orphan_pids.is_empty(),
        orphan_pids,
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_wav_simple() {
        // 构造最小 WAV（16 samples = 1ms @ 16kHz）
        let samples = vec![0.0f32, 0.5, -0.5, 1.0, -1.0, 0.3, -0.3, 0.7];
        // 使用 domain wav 编码
        let wav = crate::domain::stt::wav::pcm_to_wav(&samples, 16000, 1);
        let parsed = parse_wav_to_f32(&wav).expect("解析失败");
        assert_eq!(parsed.len(), samples.len());
        for (i, (a, b)) in samples.iter().zip(parsed.iter()).enumerate() {
            assert!((a - b).abs() < 1e-4, "样本 {i} 不匹配: {a} vs {b}");
        }
    }

    #[test]
    fn percentile_stats_basic() {
        let values = vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0, 10.0];
        let stats = compute_percentiles(&values).expect("非空");
        assert_eq!(stats.count, 10);
        assert_eq!(stats.min, 1.0);
        assert_eq!(stats.max, 10.0);
        assert!((stats.mean - 5.5).abs() < 0.01);
        assert!((stats.p50 - 5.0).abs() < 0.01 || (stats.p50 - 6.0).abs() < 0.01);
    }

    #[test]
    fn percentile_stats_empty() {
        let values: Vec<f64> = vec![];
        assert!(compute_percentiles(&values).is_none());
    }

    #[test]
    fn parse_args_basic() {
        let args = vec![
            "--deployment".to_string(),
            "/tmp/deploy".to_string(),
            "--wav".to_string(),
            "/tmp/test.wav".to_string(),
            "--mode".to_string(),
            "latency".to_string(),
            "--output".to_string(),
            "/tmp/out.json".to_string(),
            "--rounds".to_string(),
            "5".to_string(),
        ];
        let parsed = parse_args(&args).expect("解析成功");
        assert_eq!(parsed.deployment, PathBuf::from("/tmp/deploy"));
        assert_eq!(parsed.wav, PathBuf::from("/tmp/test.wav"));
        assert_eq!(parsed.mode, HarnessMode::Latency);
        assert_eq!(parsed.rounds, 5);
    }

    #[test]
    fn parse_args_lifecycle() {
        let args = vec![
            "--deployment".to_string(),
            "/tmp/deploy".to_string(),
            "--wav".to_string(),
            "/tmp/test.wav".to_string(),
            "--mode".to_string(),
            "lifecycle".to_string(),
            "--output".to_string(),
            "/tmp/out.json".to_string(),
            "--lifecycle-count".to_string(),
            "10".to_string(),
            "--kill-count".to_string(),
            "2".to_string(),
        ];
        let parsed = parse_args(&args).expect("解析成功");
        assert_eq!(parsed.mode, HarnessMode::Lifecycle);
        assert_eq!(parsed.lifecycle_count, 10);
        assert_eq!(parsed.kill_count, 2);
    }

    #[test]
    fn parse_args_missing_required() {
        let args = vec!["--deployment".to_string(), "/tmp".to_string()];
        assert!(parse_args(&args).is_err());
    }

    #[test]
    fn parse_args_unknown_mode() {
        let args = vec![
            "--deployment".to_string(),
            "/tmp".to_string(),
            "--wav".to_string(),
            "/tmp/w.wav".to_string(),
            "--mode".to_string(),
            "bogus".to_string(),
            "--output".to_string(),
            "/tmp/o.json".to_string(),
        ];
        assert!(parse_args(&args).is_err());
    }
}
