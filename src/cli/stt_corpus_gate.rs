//! STT Corpus Gate Runner（0.22.9 Handoff 07E-R）。
//!
//! 用真实 corpus 运行 VAD+STT 组合的 gate 测试，输出 JSON + CSV + Markdown 报告。
//!
//! ## 矩阵（EnergyVad 下）
//!
//! 必测：
//! - Fun-ASR-Nano GGUF（CER baseline）
//! - SenseVoice GGUF（既有路径回归）
//! - Paraformer-zh GGUF（既有路径回归）
//! - ParaformerOnline ONNX（新增候选）
//!
//! ## 口径铁则
//!
//! - GGUF 直接 spawn worker + NDJSON 标记为 **worker-protocol path**，
//!   非 production path——production path 需经 EngineManager/ManagedProcess。
//! - ONNX 实时投喂路径的 RTF 包含实时等待，与 GGUF 纯推理不可直接比较。
//! - 所有组合使用完全相同、顺序固定的样本集合。
//! - 正式 gate 数字必须来自 release build。
//!
//! ## 禁止
//!
//! - 不降低阈值
//! - 不注册模型
//! - 不改变默认模型/VAD
//! - 不改 UI
//! - 未测不得写通过
//! - 不修改文档

#![allow(clippy::collapsible_if)]

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::cli::stt_corpus::CorpusManifest;

// ── CER 计算 ─────────────────────────────────────────────────────────────

/// 字符级编辑距离（CER = edit_distance / ref_len）。
fn char_edit_distance(hyp: &str, ref_text: &str) -> usize {
    let h: Vec<char> = hyp.chars().collect();
    let r: Vec<char> = ref_text.chars().collect();
    let m = h.len();
    let n = r.len();
    if n == 0 {
        return m;
    }
    if m == 0 {
        return n;
    }
    let mut prev: Vec<usize> = (0..=n).collect();
    let mut curr = vec![0usize; n + 1];
    for i in 1..=m {
        curr[0] = i;
        for j in 1..=n {
            let cost = if h[i - 1] == r[j - 1] { 0 } else { 1 };
            curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut curr);
    }
    prev[n]
}

/// CER 归一化：Unicode NFKC + 去空白和标点 + 英文小写。
///
/// 不自动把中文数字和阿拉伯数字视为等价。
fn normalize_for_cer(text: &str) -> String {
    // Unicode NFKC（兼容性分解）
    let nfkc = unicode_normalization::UnicodeNormalization::nfkc(text);
    let mut result = String::new();
    for c in nfkc {
        // 去空白
        if c.is_whitespace() {
            continue;
        }
        // 去标点（中英文）
        if c.is_ascii_punctuation() || is_cjk_punctuation(c) {
            continue;
        }
        // 英文小写
        result.extend(c.to_lowercase());
    }
    result
}

/// 判断是否是 CJK 标点。
fn is_cjk_punctuation(c: char) -> bool {
    let cp = c as u32;
    // CJK 标点区 U+3000..U+303F
    (0x3000..=0x303F).contains(&cp)
    // 全角 ASCII 标点 U+FF00..U+FFEF
    || (0xFF00..=0xFFEF).contains(&cp)
    // 中文引号 U+2018, U+2019, U+201C, U+201D
    || matches!(cp, 0x2018 | 0x2019 | 0x201C | 0x201D | 0x2014 | 0x2026 | 0x2013)
}

/// 计算归一化后的 CER（Character Error Rate）。
fn calculate_cer(hypothesis_raw: &str, reference_raw: &str) -> f64 {
    let hyp = normalize_for_cer(hypothesis_raw);
    let r = normalize_for_cer(reference_raw);
    let ref_len = r.chars().count();
    if ref_len == 0 {
        return if hyp.is_empty() { 0.0 } else { 1.0 };
    }
    let dist = char_edit_distance(&hyp, &r);
    dist as f64 / ref_len as f64
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

// ── WAV 解码 ──────────────────────────────────────────────────────────────

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
            samples = data[data_start..data_end]
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

// ── VAD 预处理：WAV → speech segments ─────────────────────────────────────

/// 用 EnergyVad 把 PCM samples 切分成 speech segments。
///
/// 按 160 samples (10ms @16kHz) chunk 遍历，调 `EnergyVad::process_chunk`。
/// 收到 `SentenceEnd` 时，把当前累积的 segment 产出并开始新段。
/// 遍历结束后，如果残余 segment 足够长也产出。
///
/// 返回 `(segments, total_speech_samples)`。
///
/// **设计要点**：
/// - 不修改 EnergyVad 任何逻辑，只做外部切分
/// - 若 VAD 从未触发 SentenceEnd，则返回整段作为一个 segment（等价于 asr_only）
/// - segment 间不插入静音 padding
fn vad_segment_audio(
    samples: &[f32],
    vad: &mut crate::domain::stt::vad::EnergyVad,
) -> (Vec<Vec<f32>>, usize) {
    let chunk_size = 160usize; // 10ms @ 16kHz
    let mut segments: Vec<Vec<f32>> = Vec::new();
    let mut current: Vec<f32> = Vec::new();
    let mut total_speech = 0usize;

    for chunk in samples.chunks(chunk_size) {
        current.extend_from_slice(chunk);
        let event = vad.process_chunk(chunk);
        if matches!(event, crate::domain::stt::vad::VadEvent::SentenceEnd) {
            if !current.is_empty() {
                total_speech += current.len();
                segments.push(std::mem::take(&mut current));
            }
        }
    }

    // 残余段
    if !current.is_empty() {
        total_speech += current.len();
        segments.push(current);
    }

    (segments, total_speech)
}

/// FSMN-VAD 切分：用 `FsmnVadRunner` 跑完整音频，收集 segments，按时间戳切分 PCM。
///
/// 返回 `(segments, total_speech_samples)`。
///
/// **设计要点**：
/// - FSMN-VAD 的 `forward()` 内部维护 `total_samples` 跟踪时间戳
/// - 先跑一遍完整音频收集 `(start_s, end_s)` segment 列表
/// - 然后按时间戳切分原始 PCM samples
/// - 若无 segment 产生，返回整段（等价于 asr_only）
fn fsmn_vad_load_runner(
    fsmn_vad_dir: &Path,
) -> Result<crate::infra::stt::fsmn_vad_runner::FsmnVadRunner, String> {
    use crate::infra::stt::fsmn_vad_runner::FsmnVadRunner;

    let model_path = fsmn_vad_dir.join("model_quant.onnx");
    let mvn_path = fsmn_vad_dir.join("am.mvn");

    if !model_path.exists() {
        return Err(format!("FSMN-VAD 模型不存在: {}", model_path.display()));
    }
    if !mvn_path.exists() {
        return Err(format!("FSMN-VAD am.mvn 不存在: {}", mvn_path.display()));
    }

    // 初始化 ORT（如果尚未初始化）
    let dll_path = fsmn_vad_dir.join("onnxruntime.dll");
    if dll_path.exists() {
        match ort::init_from(&dll_path) {
            Ok(builder) => {
                let _ = builder.commit();
            }
            Err(e) => {
                return Err(format!("FSMN-VAD ORT 初始化失败: {e}"));
            }
        }
    }

    FsmnVadRunner::new(&model_path, &mvn_path)
}

/// FSMN-VAD 切分（复用组合级 runner）。
///
/// runner 由调用方在组合开始时创建一次，这里 `reset()` 清状态后切分，
/// 避免每个样本重新加载 ONNX 模型/重建 ORT Session（arena 内存反复增长）。
fn fsmn_vad_segment_audio(
    runner: &mut crate::infra::stt::fsmn_vad_runner::FsmnVadRunner,
    samples: &[f32],
) -> Result<(Vec<Vec<f32>>, usize), String> {
    runner.reset();

    // 分块投喂 FSMN-VAD（chunk_size 与 EnergyVad 一致：160 = 10ms@16kHz）
    let chunk_size = 160usize;
    for chunk in samples.chunks(chunk_size) {
        let _ = runner.forward(chunk, false);
    }
    // final flush
    let _ = runner.forward(&[], true);

    // 提取 segments: (start_s, end_s)
    let segments_ts = runner.segments();
    let sr = 16000f64;

    let mut pcm_segments: Vec<Vec<f32>> = Vec::new();
    let mut total_speech = 0usize;

    for (start_s, end_s) in segments_ts {
        let start_sample = ((*start_s * sr).round() as usize).min(samples.len());
        let end_sample = ((*end_s * sr).round() as usize).min(samples.len());
        if end_sample > start_sample {
            let seg = samples[start_sample..end_sample].to_vec();
            total_speech += seg.len();
            pcm_segments.push(seg);
        }
    }

    // 若无 segment 产生，返回整段
    if pcm_segments.is_empty() && !samples.is_empty() {
        total_speech = samples.len();
        pcm_segments.push(samples.to_vec());
    }

    Ok((pcm_segments, total_speech))
}

/// 将 f32 PCM samples 写成 16-bit WAV 文件（GGUF VAD 路径用）。
fn write_pcm_to_wav(path: &Path, samples: &[f32], sample_rate: u32) -> Result<(), String> {
    use std::io::Write;
    let num_samples = samples.len();
    let data_size = num_samples * 2;
    let file = std::fs::File::create(path).map_err(|e| format!("创建 WAV 失败: {e}"))?;
    let mut w = std::io::BufWriter::new(file);
    // RIFF header
    w.write_all(b"RIFF").map_err(|e| e.to_string())?;
    w.write_all(&(36 + data_size as u32).to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(b"WAVE").map_err(|e| e.to_string())?;
    // fmt chunk
    w.write_all(b"fmt ").map_err(|e| e.to_string())?;
    w.write_all(&16u32.to_le_bytes())
        .map_err(|e| e.to_string())?; // PCM
    w.write_all(&1u16.to_le_bytes())
        .map_err(|e| e.to_string())?; // PCM format
    w.write_all(&1u16.to_le_bytes())
        .map_err(|e| e.to_string())?; // mono
    w.write_all(&sample_rate.to_le_bytes())
        .map_err(|e| e.to_string())?;
    w.write_all(&(sample_rate * 2).to_le_bytes())
        .map_err(|e| e.to_string())?; // byte rate
    w.write_all(&2u16.to_le_bytes())
        .map_err(|e| e.to_string())?; // block align
    w.write_all(&16u16.to_le_bytes())
        .map_err(|e| e.to_string())?; // bits per sample
    // data chunk
    w.write_all(b"data").map_err(|e| e.to_string())?;
    w.write_all(&(data_size as u32).to_le_bytes())
        .map_err(|e| e.to_string())?;
    for s in samples {
        let v = (s.clamp(-1.0, 1.0) * 32767.0) as i16;
        w.write_all(&v.to_le_bytes()).map_err(|e| e.to_string())?;
    }
    w.flush().map_err(|e| e.to_string())?;
    Ok(())
}

// ── SHA-256 ──────────────────────────────────────────────────────────────

fn sha256_file(path: &Path) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    if let Ok(mut file) = std::fs::File::open(path) {
        use std::io::Read;
        let mut buf = [0u8; 8192];
        loop {
            match file.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buf[..n]),
                Err(_) => break,
            }
        }
    }
    let bytes = hasher.finalize();
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn manifest_hash(corpus_dir: &Path) -> String {
    let manifest_path = corpus_dir.join("manifest.json");
    sha256_file(&manifest_path)
}

// ── 内存快照 ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct MemorySnapshot {
    main_process_mb: f64,
    worker_process_mb: Option<f64>,
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

// ── 时间戳 ───────────────────────────────────────────────────────────────

fn now_timestamp() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    format!("unix:{}", now.as_secs())
}

// ── 测试结果结构 ─────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SampleResult {
    sample_id: String,
    reference_raw: String,
    hypothesis_raw: String,
    reference_normalized: String,
    hypothesis_normalized: String,
    cer: f64,
    /// 准确率 = 1 - CER
    accuracy: f64,
    first_partial_ms: Option<u64>,
    final_after_release_ms: Option<u64>,
    audio_duration_ms: u64,
    /// 包含实时投喂等待时间的 wall clock（ONNX 路径含 10ms tick 等待）
    inference_wall_ms: u64,
    /// 仅推理耗时（不含投喂等待），由 worker 返回的 _inf_ms 累计
    #[serde(skip_serializing_if = "Option::is_none")]
    inference_only_ms: Option<u64>,
    /// RTF = inference_wall_ms / audio_duration_ms（含实时投喂等待）
    rtf: f64,
    /// RTF_infer = inference_only_ms / audio_duration_ms（纯推理）
    #[serde(skip_serializing_if = "Option::is_none")]
    rtf_infer: Option<f64>,
    busy_count: u32,
    error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CombinationResult {
    name: String,
    vad: String,
    stt_engine: String,
    /// "worker-protocol" 或 "production"
    path_type: String,
    /// 模型/worker identity（model_id 或 worker exe 名）
    model_identity: String,
    samples: Vec<SampleResult>,
    cer_stats: Option<PercentileStats>,
    accuracy_stats: Option<PercentileStats>,
    first_partial_stats: Option<PercentileStats>,
    final_after_release_stats: Option<PercentileStats>,
    rtf_stats: Option<PercentileStats>,
    total_busy: u32,
    total_errors: u32,
    memory_snapshots: Vec<MemorySnapshot>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct LifecycleResult {
    total_start_stop: usize,
    success_count: usize,
    failure_count: usize,
    orphan_count: usize,
    stale_generation_count: usize,
    deadlock_count: usize,
    reset_reproducible: bool,
    kill_restart_count: usize,
    kill_restart_success: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct GateReport {
    metadata: ReportMetadata,
    combinations: Vec<CombinationResult>,
    lifecycle: Option<LifecycleResult>,
    paraformer_verdict: String,
    fsmn_verdict: String,
    default_model_recommendation: String,
    corpus_manifest_hash: String,
    repro_commands: Vec<String>,
    failed_items: Vec<String>,
    next_steps: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ReportMetadata {
    version: String,
    generated_at: String,
    corpus_dir: String,
    corpus_sample_count: usize,
    corpus_total_duration_s: f64,
}

// ── CLI 入口 ─────────────────────────────────────────────────────────────

struct GateArgs {
    corpus_dir: PathBuf,
    deployment_dir: PathBuf,
    output_dir: PathBuf,
    max_samples: Option<usize>,
    skip_lifecycle: bool,
    skip_fsmn: bool,
    ready_timeout_secs: u64,
    /// GGUF worker exe 目录（resources/bin/funasr-worker/）
    worker_dir: PathBuf,
    /// GGUF 模型文件目录（target/gguf-models/）
    model_dir: PathBuf,
    /// 每个样本重复测量次数（默认 1，建议 3 做 warm 测量）
    repeats: usize,
    /// 测量轮次间的冷却间隔（毫秒），避免长时间满载压垮机器
    sample_gap_ms: u64,
}

pub fn run_from_args(args: &[String]) -> i32 {
    let parsed = match parse_args(args) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stt-corpus-gate: 参数解析失败: {e}");
            eprintln!(
                "用法: blink.exe stt-corpus-gate --corpus-dir <dir> --deployment <dir> \
                 --output-dir <dir> [--worker-dir <dir>] [--model-dir <dir>] \
                 [--max-samples N] [--repeats N] [--skip-lifecycle] \
                 [--skip-fsmn] [--ready-timeout secs] [--sample-gap-ms N]"
            );
            return 1;
        }
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter("info")
        .with_writer(std::io::stderr)
        .try_init();

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("stt-corpus-gate: 创建 tokio runtime 失败: {e}");
            return 1;
        }
    };

    runtime.block_on(async move { run_gate(&parsed).await })
}

fn parse_args(args: &[String]) -> Result<GateArgs, String> {
    let mut corpus_dir = None;
    let mut deployment_dir = None;
    let mut output_dir = None;
    let mut max_samples = None;
    let mut skip_lifecycle = false;
    let mut skip_fsmn = false;
    let mut ready_timeout_secs = 120u64;
    let mut worker_dir = None;
    let mut model_dir = None;
    let mut repeats = 1usize;
    let mut sample_gap_ms = 2000u64;

    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--corpus-dir" => {
                i += 1;
                corpus_dir = args.get(i).map(PathBuf::from);
            }
            "--deployment" => {
                i += 1;
                deployment_dir = args.get(i).map(PathBuf::from);
            }
            "--output-dir" => {
                i += 1;
                output_dir = args.get(i).map(PathBuf::from);
            }
            "--max-samples" => {
                i += 1;
                max_samples = args.get(i).and_then(|s| s.parse().ok());
            }
            "--skip-lifecycle" => {
                skip_lifecycle = true;
            }
            "--skip-fsmn" => {
                skip_fsmn = true;
            }
            "--ready-timeout" => {
                i += 1;
                ready_timeout_secs = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --ready-timeout")?;
            }
            "--worker-dir" => {
                i += 1;
                worker_dir = args.get(i).map(PathBuf::from);
            }
            "--model-dir" => {
                i += 1;
                model_dir = args.get(i).map(PathBuf::from);
            }
            "--repeats" => {
                i += 1;
                repeats = args.get(i).and_then(|s| s.parse().ok()).unwrap_or(1);
                if repeats == 0 {
                    repeats = 1;
                }
            }
            "--sample-gap-ms" => {
                i += 1;
                sample_gap_ms = args
                    .get(i)
                    .and_then(|s| s.parse().ok())
                    .ok_or("无效 --sample-gap-ms")?;
            }
            _ => {}
        }
        i += 1;
    }

    Ok(GateArgs {
        corpus_dir: corpus_dir.ok_or("缺少 --corpus-dir")?,
        deployment_dir: deployment_dir.ok_or("缺少 --deployment")?,
        output_dir: output_dir.ok_or("缺少 --output-dir")?,
        max_samples,
        skip_lifecycle,
        skip_fsmn,
        ready_timeout_secs,
        sample_gap_ms,
        repeats,
        worker_dir: worker_dir.unwrap_or_else(|| PathBuf::from("resources/bin/funasr-worker")),
        model_dir: model_dir.unwrap_or_else(|| PathBuf::from("target/gguf-models")),
    })
}

// ── 核心 gate 逻辑 ───────────────────────────────────────────────────────

async fn run_gate(args: &GateArgs) -> i32 {
    // 1. 加载 corpus
    let manifest_path = args.corpus_dir.join("manifest.json");
    let manifest_content = match std::fs::read_to_string(&manifest_path) {
        Ok(c) => c,
        Err(e) => {
            tracing::error!(%e, "读取 manifest 失败");
            return 1;
        }
    };
    let manifest: CorpusManifest = match serde_json::from_str(&manifest_content) {
        Ok(m) => m,
        Err(e) => {
            tracing::error!(%e, "解析 manifest 失败");
            return 1;
        }
    };

    let corpus_hash = manifest_hash(&args.corpus_dir);
    let total_duration: f64 = manifest.samples.iter().map(|s| s.duration_s).sum();

    tracing::info!(
        samples = manifest.samples.len(),
        total_duration_s = total_duration,
        hash = %corpus_hash,
        "corpus 已加载"
    );

    let samples: Vec<_> = if let Some(max) = args.max_samples {
        manifest.samples.into_iter().take(max).collect()
    } else {
        manifest.samples.into_iter().collect()
    };
    let sample_count = samples.len();

    // 2. 运行矩阵
    let mut combinations = Vec::new();
    let mut repro_commands = Vec::new();

    let repro_cmd = format!(
        "blink.exe stt-corpus-gate --corpus-dir {} --deployment {} --output-dir {} --worker-dir {} --model-dir {}",
        args.corpus_dir.display(),
        args.deployment_dir.display(),
        args.output_dir.display(),
        args.worker_dir.display(),
        args.model_dir.display()
    );
    repro_commands.push(repro_cmd);

    // 必测 A: ASR-only + ParaformerOnline ONNX
    // 诚实标记：gate runner 直接将完整 WAV 按 10ms tick 投喂给 ASR，
    // 未经任何 VAD 切分。这是 ASR 引擎诊断，不是 VAD+ASR 组合测试。
    let combo_a = "asr_only + paraformer_onnx";
    tracing::info!("▶ 运行组合: {combo_a}");
    match run_combination(
        &samples,
        &args.corpus_dir,
        &args.deployment_dir,
        "asr_only",
        "paraformer_onnx",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_a, %e, "组合失败");
            combinations.push(error_combination(
                combo_a,
                "asr_only",
                "paraformer_onnx",
                &e,
            ));
        }
    }

    // 必测 B: ASR-only + Fun-ASR-Nano GGUF — CER baseline（不得省略）
    let combo_b = "asr_only + nano_gguf";
    tracing::info!("▶ 运行组合: {combo_b}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "nano",
        "asr_only",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_b, %e, "组合失败");
            combinations.push(error_combination(combo_b, "asr_only", "nano_gguf", &e));
        }
    }

    // 必测 C: ASR-only + SenseVoice GGUF — 既有路径回归
    let combo_c = "asr_only + sensevoice_gguf";
    tracing::info!("▶ 运行组合: {combo_c}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "sensevoice",
        "asr_only",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_c, %e, "组合失败");
            combinations.push(error_combination(
                combo_c,
                "asr_only",
                "sensevoice_gguf",
                &e,
            ));
        }
    }

    // 必测 D: ASR-only + Paraformer-zh GGUF
    let combo_d = "asr_only + paraformer_gguf";
    tracing::info!("▶ 运行组合: {combo_d}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "paraformer",
        "asr_only",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_d, %e, "组合失败");
            combinations.push(error_combination(
                combo_d,
                "asr_only",
                "paraformer_gguf",
                &e,
            ));
        }
    }

    // ── VAD × ASR 矩阵：EnergyVad × 4 ASR ──────────────────────────────
    // 与 combo A-D 的区别：先用 EnergyVad 切分 WAV 再投喂 ASR，
    // 而非整段 WAV 直接投喂。

    // 必测 E: EnergyVad + ParaformerOnline ONNX
    let combo_e = "energy_vad + paraformer_onnx";
    tracing::info!("▶ 运行组合: {combo_e}");
    match run_combination(
        &samples,
        &args.corpus_dir,
        &args.deployment_dir,
        "energy_vad",
        "paraformer_onnx",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_e, %e, "组合失败");
            combinations.push(error_combination(
                combo_e,
                "energy_vad",
                "paraformer_onnx",
                &e,
            ));
        }
    }

    // 必测 F: EnergyVad + Fun-ASR-Nano GGUF
    let combo_f = "energy_vad + nano_gguf";
    tracing::info!("▶ 运行组合: {combo_f}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "nano",
        "energy_vad",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_f, %e, "组合失败");
            combinations.push(error_combination(combo_f, "energy_vad", "nano_gguf", &e));
        }
    }

    // 必测 G: EnergyVad + SenseVoice GGUF
    let combo_g = "energy_vad + sensevoice_gguf";
    tracing::info!("▶ 运行组合: {combo_g}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "sensevoice",
        "energy_vad",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_g, %e, "组合失败");
            combinations.push(error_combination(
                combo_g,
                "energy_vad",
                "sensevoice_gguf",
                &e,
            ));
        }
    }

    // 必测 H: EnergyVad + Paraformer-zh GGUF
    let combo_h = "energy_vad + paraformer_gguf";
    tracing::info!("▶ 运行组合: {combo_h}");
    match run_gguf_combination(
        &samples,
        &args.corpus_dir,
        &args.worker_dir,
        &args.model_dir,
        "paraformer",
        "energy_vad",
        args.ready_timeout_secs,
        args.sample_gap_ms,
        args.repeats,
        &args.deployment_dir,
    )
    .await
    {
        Ok(r) => combinations.push(r),
        Err(e) => {
            tracing::error!(combo = combo_h, %e, "组合失败");
            combinations.push(error_combination(
                combo_h,
                "energy_vad",
                "paraformer_gguf",
                &e,
            ));
        }
    }

    // ── FSMN-VAD × ASR 矩阵 ──────────────────────────────────────────────
    // 与 EnergyVad × ASR 的区别：用 FSMN 神经网络 VAD 切分 WAV 再投喂 ASR。
    // 资产隔离：FSMN-VAD 模型在 deployment_dir/fsmn-vad/ 子目录。
    if !args.skip_fsmn {
        let fsmn_model = args
            .deployment_dir
            .join("fsmn-vad")
            .join("model_quant.onnx");
        if fsmn_model.exists() {
            // 必测 I: FSMN-VAD + ParaformerOnline ONNX
            let combo_i = "fsmn_vad + paraformer_onnx";
            tracing::info!("▶ 运行组合: {combo_i}");
            match run_combination(
                &samples,
                &args.corpus_dir,
                &args.deployment_dir,
                "fsmn_vad",
                "paraformer_onnx",
                args.ready_timeout_secs,
                args.sample_gap_ms,
                args.repeats,
            )
            .await
            {
                Ok(r) => combinations.push(r),
                Err(e) => {
                    tracing::error!(combo = combo_i, %e, "组合失败");
                    combinations.push(error_combination(
                        combo_i,
                        "fsmn_vad",
                        "paraformer_onnx",
                        &e,
                    ));
                }
            }

            // 必测 J: FSMN-VAD + Fun-ASR-Nano GGUF
            let combo_j = "fsmn_vad + nano_gguf";
            tracing::info!("▶ 运行组合: {combo_j}");
            match run_gguf_combination(
                &samples,
                &args.corpus_dir,
                &args.worker_dir,
                &args.model_dir,
                "nano",
                "fsmn_vad",
                args.ready_timeout_secs,
                args.sample_gap_ms,
                args.repeats,
                &args.deployment_dir,
            )
            .await
            {
                Ok(r) => combinations.push(r),
                Err(e) => {
                    tracing::error!(combo = combo_j, %e, "组合失败");
                    combinations.push(error_combination(combo_j, "fsmn_vad", "nano_gguf", &e));
                }
            }

            // 必测 K: FSMN-VAD + SenseVoice GGUF
            let combo_k = "fsmn_vad + sensevoice_gguf";
            tracing::info!("▶ 运行组合: {combo_k}");
            match run_gguf_combination(
                &samples,
                &args.corpus_dir,
                &args.worker_dir,
                &args.model_dir,
                "sensevoice",
                "fsmn_vad",
                args.ready_timeout_secs,
                args.sample_gap_ms,
                args.repeats,
                &args.deployment_dir,
            )
            .await
            {
                Ok(r) => combinations.push(r),
                Err(e) => {
                    tracing::error!(combo = combo_k, %e, "组合失败");
                    combinations.push(error_combination(
                        combo_k,
                        "fsmn_vad",
                        "sensevoice_gguf",
                        &e,
                    ));
                }
            }

            // 必测 L: FSMN-VAD + Paraformer-zh GGUF
            let combo_l = "fsmn_vad + paraformer_gguf";
            tracing::info!("▶ 运行组合: {combo_l}");
            match run_gguf_combination(
                &samples,
                &args.corpus_dir,
                &args.worker_dir,
                &args.model_dir,
                "paraformer",
                "fsmn_vad",
                args.ready_timeout_secs,
                args.sample_gap_ms,
                args.repeats,
                &args.deployment_dir,
            )
            .await
            {
                Ok(r) => combinations.push(r),
                Err(e) => {
                    tracing::error!(combo = combo_l, %e, "组合失败");
                    combinations.push(error_combination(
                        combo_l,
                        "fsmn_vad",
                        "paraformer_gguf",
                        &e,
                    ));
                }
            }
        } else {
            tracing::warn!(
                "FSMN-VAD 模型不存在 ({}), 跳过 FSMN 组合",
                fsmn_model.display()
            );
        }
    }

    // 3. Lifecycle 测试
    let lifecycle = if !args.skip_lifecycle {
        tracing::info!("▶ 运行 lifecycle 测试");
        run_lifecycle_test(&args.deployment_dir, args.ready_timeout_secs)
            .await
            .ok()
    } else {
        tracing::info!("跳过 lifecycle 测试");
        None
    };

    // 4. 判定
    let (paraformer_verdict, fsmn_verdict, default_rec, failed_items, next_steps) =
        evaluate_verdicts(&combinations, &lifecycle);

    // 5. 生成报告
    let report = GateReport {
        metadata: ReportMetadata {
            version: "0.22.9-handoff-07f".to_string(),
            generated_at: now_timestamp(),
            corpus_dir: args.corpus_dir.to_string_lossy().to_string(),
            corpus_sample_count: sample_count,
            corpus_total_duration_s: (total_duration * 1000.0).round() / 1000.0,
        },
        combinations,
        lifecycle,
        paraformer_verdict,
        fsmn_verdict,
        default_model_recommendation: default_rec,
        corpus_manifest_hash: corpus_hash,
        repro_commands,
        failed_items,
        next_steps,
    };

    // 写 JSON
    std::fs::create_dir_all(&args.output_dir).ok();
    let json_path = args.output_dir.join("gate_report.json");
    let json = serde_json::to_string_pretty(&report).unwrap_or_else(|e| format!("序列化失败: {e}"));
    if let Err(e) = std::fs::write(&json_path, &json) {
        tracing::error!(%e, "写入 JSON 失败");
        return 1;
    }

    // 写 CSV
    let csv_path = args.output_dir.join("gate_results.csv");
    let csv = generate_csv(&report);
    if let Err(e) = std::fs::write(&csv_path, &csv) {
        tracing::warn!(%e, "写入 CSV 失败");
    }

    // 写 Markdown
    let md_path = args.output_dir.join("gate_report.md");
    let md = generate_markdown(&report);
    if let Err(e) = std::fs::write(&md_path, &md) {
        tracing::warn!(%e, "写入 Markdown 失败");
    }

    tracing::info!(json = %json_path.display(), "报告已生成");
    println!("Gate 报告已生成: {}", json_path.display());
    println!("  Paraformer: {}", report.paraformer_verdict);
    println!("  FSMN: {}", report.fsmn_verdict);

    0
}

#[allow(dead_code)]
fn empty_combination(name: &str, vad: &str, stt_engine: &str) -> CombinationResult {
    CombinationResult {
        name: name.to_string(),
        vad: vad.to_string(),
        stt_engine: stt_engine.to_string(),
        path_type: "not_tested".to_string(),
        model_identity: String::new(),
        samples: vec![],
        cer_stats: None,
        accuracy_stats: None,
        first_partial_stats: None,
        final_after_release_stats: None,
        rtf_stats: None,
        total_busy: 0,
        total_errors: 0,
        memory_snapshots: vec![],
    }
}

fn error_combination(name: &str, vad: &str, stt_engine: &str, _e: &str) -> CombinationResult {
    CombinationResult {
        name: name.to_string(),
        vad: vad.to_string(),
        stt_engine: stt_engine.to_string(),
        path_type: "error".to_string(),
        model_identity: String::new(),
        samples: vec![],
        cer_stats: None,
        accuracy_stats: None,
        first_partial_stats: None,
        final_after_release_stats: None,
        rtf_stats: None,
        total_busy: 0,
        total_errors: 1,
        memory_snapshots: vec![],
    }
}

/// 运行单个 VAD+STT 组合。
#[allow(clippy::too_many_arguments)] // gate runner 矩阵展开参数（诊断工具）
async fn run_combination(
    samples: &[crate::cli::stt_corpus::CorpusSample],
    corpus_dir: &Path,
    deployment_dir: &Path,
    vad: &str,
    stt_engine: &str,
    ready_timeout_secs: u64,
    sample_gap_ms: u64,
    repeats: usize,
) -> Result<CombinationResult, String> {
    use crate::domain::stt::StreamingSttPort;
    use crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter;

    let ready_timeout = Duration::from_secs(ready_timeout_secs);

    if stt_engine != "paraformer_onnx" {
        return Err(format!("STT 引擎 {stt_engine} 的 production path 未就绪"));
    }

    // FSMN runner 组合级复用：只加载一次模型/Session，样本间用 reset() 清状态
    let mut fsmn_runner = if vad == "fsmn_vad" {
        Some(fsmn_vad_load_runner(&deployment_dir.join("fsmn-vad"))?)
    } else {
        None
    };

    eprintln!(
        "[gate] 启动 ParaformerOnline worker (deployment={})...",
        deployment_dir.display()
    );
    let launch_start = Instant::now();
    let adapter = ParaformerOnlineAdapter::launch(deployment_dir.to_path_buf(), ready_timeout)
        .await
        .map_err(|e| format!("worker 启动失败: {e}"))?;
    eprintln!(
        "[gate] worker 启动成功 ({}ms)",
        launch_start.elapsed().as_millis()
    );

    let mut memory_snapshots = Vec::new();
    memory_snapshots.push(take_memory_snapshot(None, "worker_ready"));

    let mut sample_results = Vec::new();
    let mut total_busy = 0u32;
    let mut total_errors = 0u32;

    for (idx, sample) in samples.iter().enumerate() {
        let wav_path = corpus_dir.join(&sample.wav_path);
        tracing::info!(
            idx,
            sample_id = %sample.sample_id,
            wav = %wav_path.display(),
            repeats = repeats,
            "处理样本"
        );

        // 重复测量：取最佳（最低 CER）结果
        let mut best_result: Option<SampleResult> = None;
        for round in 0..repeats {
            let result = if vad == "energy_vad" {
                match process_sample_vad(&adapter, &wav_path, &sample.reference_text, round).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(idx, round, %e, "VAD 样本处理失败");
                        SampleResult {
                            sample_id: sample.sample_id.clone(),
                            reference_raw: sample.reference_text.clone(),
                            hypothesis_raw: String::new(),
                            reference_normalized: normalize_for_cer(&sample.reference_text),
                            hypothesis_normalized: String::new(),
                            cer: 1.0,
                            accuracy: 0.0,
                            first_partial_ms: None,
                            final_after_release_ms: None,
                            audio_duration_ms: 0,
                            inference_wall_ms: 0,
                            inference_only_ms: None,
                            rtf: 0.0,
                            rtf_infer: None,
                            busy_count: 0,
                            error: Some(e),
                        }
                    }
                }
            } else if vad == "fsmn_vad" {
                let runner = fsmn_runner
                    .as_mut()
                    .expect("fsmn_vad 组合已确保 runner 存在");
                match process_sample_fsmn_vad(
                    &adapter,
                    &wav_path,
                    &sample.reference_text,
                    round,
                    runner,
                )
                .await
                {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(idx, round, %e, "FSMN-VAD 样本处理失败");
                        SampleResult {
                            sample_id: sample.sample_id.clone(),
                            reference_raw: sample.reference_text.clone(),
                            hypothesis_raw: String::new(),
                            reference_normalized: normalize_for_cer(&sample.reference_text),
                            hypothesis_normalized: String::new(),
                            cer: 1.0,
                            accuracy: 0.0,
                            first_partial_ms: None,
                            final_after_release_ms: None,
                            audio_duration_ms: 0,
                            inference_wall_ms: 0,
                            inference_only_ms: None,
                            rtf: 0.0,
                            rtf_infer: None,
                            busy_count: 0,
                            error: Some(e),
                        }
                    }
                }
            } else {
                match process_sample(&adapter, &wav_path, &sample.reference_text, round).await {
                    Ok(r) => r,
                    Err(e) => {
                        tracing::warn!(idx, round, %e, "样本处理失败");
                        SampleResult {
                            sample_id: sample.sample_id.clone(),
                            reference_raw: sample.reference_text.clone(),
                            hypothesis_raw: String::new(),
                            reference_normalized: normalize_for_cer(&sample.reference_text),
                            hypothesis_normalized: String::new(),
                            cer: 1.0,
                            accuracy: 0.0,
                            first_partial_ms: None,
                            final_after_release_ms: None,
                            audio_duration_ms: 0,
                            inference_wall_ms: 0,
                            inference_only_ms: None,
                            rtf: 0.0,
                            rtf_infer: None,
                            busy_count: 0,
                            error: Some(e),
                        }
                    }
                }
            };

            let mut result = result;
            result.sample_id = sample.sample_id.clone();

            if repeats > 1 {
                tracing::info!(
                    idx,
                    round,
                    cer = format!("{:.4}", result.cer),
                    "重复测量轮次完成"
                );
            }

            // 取最佳结果（最低 CER）
            match &best_result {
                None => best_result = Some(result),
                Some(prev) if result.cer < prev.cer => best_result = Some(result),
                _ => {}
            }

            let _ = adapter.reset().await;

            // 轮次间冷却：给 CPU/内存留喘息窗口，避免长时间满载
            if sample_gap_ms > 0 {
                tokio::time::sleep(Duration::from_millis(sample_gap_ms)).await;
            }
        }

        let result = best_result.unwrap(); // 至少跑一轮

        total_busy += result.busy_count;
        if result.error.is_some() {
            total_errors += 1;
        }
        sample_results.push(result);
    }

    memory_snapshots.push(take_memory_snapshot(None, "after_all_samples"));
    let _ = adapter.stop().await;

    let cer_values: Vec<f64> = sample_results.iter().map(|r| r.cer).collect();
    let accuracy_values: Vec<f64> = cer_values.iter().map(|c| 1.0 - c).collect();
    let first_partial_values: Vec<f64> = sample_results
        .iter()
        .filter_map(|r| r.first_partial_ms.map(|v| v as f64))
        .collect();
    let final_after_values: Vec<f64> = sample_results
        .iter()
        .filter_map(|r| r.final_after_release_ms.map(|v| v as f64))
        .collect();
    let rtf_values: Vec<f64> = sample_results
        .iter()
        .filter(|r| r.error.is_none() && r.rtf > 0.0)
        .map(|r| r.rtf)
        .collect();

    Ok(CombinationResult {
        name: format!("{vad} + {stt_engine}"),
        vad: vad.to_string(),
        stt_engine: stt_engine.to_string(),
        path_type: "production".to_string(),
        model_identity: "paraformer-online-onnx".to_string(),
        samples: sample_results,
        cer_stats: compute_percentiles(&cer_values),
        accuracy_stats: compute_percentiles(&accuracy_values),
        first_partial_stats: compute_percentiles(&first_partial_values),
        final_after_release_stats: compute_percentiles(&final_after_values),
        rtf_stats: compute_percentiles(&rtf_values),
        total_busy,
        total_errors,
        memory_snapshots,
    })
}

// ── GGUF worker (NDJSON v1) gate 逻辑 ─────────────────────────────────────

/// GGUF worker 的模型配置。
struct GgufModelConfig {
    /// worker exe 文件名
    worker_exe: &'static str,
    /// model_id（环境变量 BLINK_MODEL_ID）
    model_id: &'static str,
    /// 模型文件名（在 model_dir 中查找）
    model_files: &'static [&'static str],
    /// argv 前缀（--enc / -m 等）
    argv_prefix: &'static [&'static str],
}

/// 查找 GGUF 模型配置。
fn gguf_config(model: &str) -> Result<GgufModelConfig, String> {
    match model {
        "sensevoice" => Ok(GgufModelConfig {
            worker_exe: "funasr-sensevoice-worker.exe",
            model_id: "gguf/sensevoice-small-q8",
            model_files: &["sensevoice-small-q8.gguf"],
            argv_prefix: &["-m"],
        }),
        "paraformer" => Ok(GgufModelConfig {
            worker_exe: "funasr-paraformer-worker.exe",
            model_id: "gguf/paraformer-zh-q8",
            model_files: &["paraformer-q8.gguf"],
            argv_prefix: &["-m"],
        }),
        "nano" => Ok(GgufModelConfig {
            worker_exe: "funasr-nano-worker.exe",
            model_id: "gguf/fun-asr-nano-q4km",
            model_files: &["funasr-encoder-f16.gguf", "qwen3-0.6b-q4km.gguf"],
            argv_prefix: &["--enc", "", "-m", ""],
        }),
        other => Err(format!("未知 GGUF 模型: {other}")),
    }
}

/// 运行 GGUF worker 组合（NDJSON v1 协议，非流式整段推理）。
///
/// GGUF worker 是"整段 WAV → 整段输出"的一次性推理——不像 ParaformerOnline
/// 那样有流式 partial。因此测试流程更简单：
/// 1. spawn worker exe → wait_ready → hello
/// 2. 对每个样本：写 WAV 到临时目录 → transcribe → 记录 CER/延迟/RTF
/// 3. shutdown
#[allow(clippy::too_many_arguments)] // gate runner 矩阵展开参数（诊断工具）
async fn run_gguf_combination(
    samples: &[crate::cli::stt_corpus::CorpusSample],
    corpus_dir: &Path,
    worker_dir: &Path,
    model_dir: &Path,
    model: &str,
    vad: &str,
    ready_timeout_secs: u64,
    sample_gap_ms: u64,
    repeats: usize,
    deployment_dir: &Path,
) -> Result<CombinationResult, String> {
    use crate::infra::local_engine::worker_proto::NdjsonWorkerClient;
    use std::collections::HashMap;

    let cfg = gguf_config(model)?;
    let ready_timeout = Duration::from_secs(ready_timeout_secs);
    let stt_engine = format!("{}_gguf", model);

    // 1. 构造 worker argv
    let exe = worker_dir.join(cfg.worker_exe);
    if !exe.is_file() {
        return Err(format!(
            "worker exe 不存在: {}（请先 cargo xtask funasr-worker 构建）",
            exe.display()
        ));
    }

    let mut args: Vec<String> = Vec::new();
    // 对 nano 做特殊处理
    if model == "nano" {
        args.push("--enc".to_string());
        args.push(
            model_dir
                .join("funasr-encoder-f16.gguf")
                .display()
                .to_string(),
        );
        args.push("-m".to_string());
        args.push(model_dir.join("qwen3-0.6b-q4km.gguf").display().to_string());
    } else {
        for prefix in cfg.argv_prefix {
            args.push(prefix.to_string());
        }
        args.push(model_dir.join(cfg.model_files[0]).display().to_string());
    }
    args.push("--stdin-server".to_string());

    // 2. 构造环境变量
    let audio_tmp = std::env::temp_dir().join(format!("blink-gguf-gate-{}", std::process::id()));
    std::fs::create_dir_all(&audio_tmp).map_err(|e| format!("创建音频临时目录失败: {e}"))?;

    let mut env: HashMap<String, String> = HashMap::new();
    env.insert("BLINK_ENGINE_ID".to_string(), "funasr".to_string());
    env.insert(
        "BLINK_INSTANCE_ID".to_string(),
        format!("gate-gguf-{}", std::process::id()),
    );
    env.insert(
        "BLINK_ENGINE_TOKEN".to_string(),
        "gate-token-0123456789abcdef".to_string(),
    );
    env.insert("BLINK_MODEL_ID".to_string(), cfg.model_id.to_string());
    env.insert(
        "BLINK_MODEL_REVISION".to_string(),
        "gguf-v0.2.6".to_string(),
    );
    env.insert(
        "BLINK_MODEL_PAYLOAD_DIR".to_string(),
        model_dir.display().to_string(),
    );
    env.insert(
        "BLINK_AUDIO_DIR".to_string(),
        audio_tmp.display().to_string(),
    );

    // 3. spawn worker
    eprintln!(
        "[gate] 启动 GGUF worker: {} (model={})...",
        cfg.worker_exe, cfg.model_id
    );
    let launch_start = Instant::now();

    let mut cmd = crate::infra::platform::no_window_tokio(tokio::process::Command::new(&exe));
    cmd.args(&args)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("spawn worker 失败: {e}"))?;
    let worker_pid = child.id();

    let stdin = child.stdin.take().ok_or("无法取走 stdin")?;
    let stdout = child.stdout.take().ok_or("无法取走 stdout")?;
    let stderr = child.stderr.take();

    // 启动 stderr reader task 持续消费 worker stderr 输出。
    //
    // **铁则**：如果 stderr 被 piped 但不消费，管道缓冲区（Windows 默认 4KB）
    // 会被 worker 的大量模型加载日志填满，导致 worker 进程阻塞在 stderr 写入上，
    // 永远无法到达 stdout ready 输出——表现为 ready 超时。
    // nano worker（qwen3 + encoder）的 stderr 约 64KB，尤其容易触发。
    if let Some(stderr) = stderr {
        tokio::spawn(async move {
            use tokio::io::AsyncReadExt;
            let mut reader = tokio::io::BufReader::new(stderr);
            let mut buf = [0u8; 4096];
            let mut lines = 0u32;
            loop {
                match reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        lines += 1;
                        // 仅在 debug 级别逐行记录，避免日志爆炸
                        let preview = String::from_utf8_lossy(&buf[..n])
                            .lines()
                            .last()
                            .unwrap_or("")
                            .chars()
                            .take(80)
                            .collect::<String>();
                        tracing::debug!(lines, %preview, "worker stderr");
                    }
                    Err(e) => {
                        tracing::debug!(%e, lines, "worker stderr read error");
                        break;
                    }
                }
            }
            tracing::info!(lines, "worker stderr reader: EOF");
        });
    }

    let client = NdjsonWorkerClient::new(stdin, stdout);

    // 4. 等待 ready（模型加载）
    eprintln!(
        "[gate] 等待 GGUF worker ready (timeout={}s)...",
        ready_timeout_secs
    );
    let ready_result = client.wait_ready(ready_timeout).await;
    match &ready_result {
        Ok(ready) => {
            eprintln!(
                "[gate] GGUF worker ready ({}ms): model={} backend={}",
                launch_start.elapsed().as_millis(),
                ready
                    .get("model_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("?"),
                ready.get("backend").and_then(|v| v.as_str()).unwrap_or("?"),
            );
        }
        Err(e) => {
            // 读 stderr 帮助调试
            let stderr = child.stderr.take();
            if let Some(mut stderr) = stderr {
                use tokio::io::AsyncReadExt;
                let mut buf = vec![0u8; 4096];
                let _ =
                    tokio::time::timeout(Duration::from_millis(500), stderr.read(&mut buf)).await;
                let stderr_text = String::from_utf8_lossy(&buf);
                eprintln!("[gate] worker stderr: {stderr_text}");
            }
            return Err(format!("GGUF worker ready 失败: {e}"));
        }
    }

    // 5. hello 握手
    client
        .hello(Duration::from_secs(10))
        .await
        .map_err(|e| format!("GGUF worker hello 失败: {e}"))?;

    // FSMN runner 组合级复用（与 production 路径同理）
    let mut fsmn_runner = if vad == "fsmn_vad" {
        Some(fsmn_vad_load_runner(&deployment_dir.join("fsmn-vad"))?)
    } else {
        None
    };

    let mut memory_snapshots = Vec::new();
    memory_snapshots.push(take_memory_snapshot(worker_pid, "worker_ready"));

    // 6. 处理每个样本
    let mut sample_results = Vec::new();
    let mut total_busy = 0u32;
    let mut total_errors = 0u32;

    for (idx, sample) in samples.iter().enumerate() {
        let wav_path = corpus_dir.join(&sample.wav_path);
        tracing::info!(
            idx,
            sample_id = %sample.sample_id,
            wav = %wav_path.display(),
            repeats = repeats,
            "GGUF 处理样本"
        );

        // 重复测量：取最佳（最低 CER）结果
        let mut best_result: Option<SampleResult> = None;
        for round in 0..repeats {
            let mut result = match process_gguf_sample(
                &client,
                &wav_path,
                &sample.reference_text,
                &audio_tmp,
                vad,
                round,
                fsmn_runner.as_mut(),
            )
            .await
            {
                Ok(r) => r,
                Err(e) => {
                    tracing::warn!(idx, round, %e, "GGUF 样本处理失败");
                    SampleResult {
                        sample_id: sample.sample_id.clone(),
                        reference_raw: sample.reference_text.clone(),
                        hypothesis_raw: String::new(),
                        reference_normalized: normalize_for_cer(&sample.reference_text),
                        hypothesis_normalized: String::new(),
                        cer: 1.0,
                        accuracy: 0.0,
                        first_partial_ms: None,
                        final_after_release_ms: None,
                        audio_duration_ms: 0,
                        inference_wall_ms: 0,
                        inference_only_ms: None,
                        rtf: 0.0,
                        rtf_infer: None,
                        busy_count: 0,
                        error: Some(e),
                    }
                }
            };

            result.sample_id = sample.sample_id.clone();

            if repeats > 1 {
                tracing::info!(
                    idx,
                    round,
                    cer = format!("{:.4}", result.cer),
                    "GGUF 重复测量轮次完成"
                );
            }

            // 取最佳结果（最低 CER）
            match &best_result {
                None => best_result = Some(result),
                Some(prev) if result.cer < prev.cer => best_result = Some(result),
                _ => {}
            }

            // 轮次间冷却：给 CPU/内存留喘息窗口，避免长时间满载
            if sample_gap_ms > 0 {
                tokio::time::sleep(Duration::from_millis(sample_gap_ms)).await;
            }
        }

        let result = best_result.unwrap(); // 至少跑一轮

        total_busy += result.busy_count;
        if result.error.is_some() {
            total_errors += 1;
        }
        sample_results.push(result);
    }

    memory_snapshots.push(take_memory_snapshot(worker_pid, "after_all_samples"));

    // 7. shutdown
    client.request_shutdown().await;
    let _ = tokio::time::timeout(Duration::from_secs(10), child.wait()).await;

    // 清理临时目录
    let _ = std::fs::remove_dir_all(&audio_tmp);

    // 8. 统计
    let cer_values: Vec<f64> = sample_results.iter().map(|r| r.cer).collect();
    let accuracy_values: Vec<f64> = cer_values.iter().map(|c| 1.0 - c).collect();
    let final_after_values: Vec<f64> = sample_results
        .iter()
        .filter_map(|r| r.final_after_release_ms.map(|v| v as f64))
        .collect();
    let rtf_values: Vec<f64> = sample_results
        .iter()
        .filter(|r| r.error.is_none() && r.rtf > 0.0)
        .map(|r| r.rtf)
        .collect();

    Ok(CombinationResult {
        name: format!("{vad} + {stt_engine}"),
        vad: vad.to_string(),
        stt_engine,
        path_type: "worker-protocol".to_string(),
        model_identity: cfg.model_id.to_string(),
        samples: sample_results,
        cer_stats: compute_percentiles(&cer_values),
        accuracy_stats: compute_percentiles(&accuracy_values),
        first_partial_stats: None, // GGUF 非流式，无 partial
        final_after_release_stats: compute_percentiles(&final_after_values),
        rtf_stats: compute_percentiles(&rtf_values),
        total_busy,
        total_errors,
        memory_snapshots,
    })
}

/// 处理单个 GGUF 样本：写 WAV → transcribe → 计算指标。
#[allow(clippy::unnecessary_literal_unwrap)]
async fn process_gguf_sample(
    client: &crate::infra::local_engine::worker_proto::NdjsonWorkerClient,
    wav_path: &Path,
    reference_text: &str,
    audio_dir: &Path,
    vad: &str,
    round: usize,
    fsmn_runner: Option<&mut crate::infra::stt::fsmn_vad_runner::FsmnVadRunner>,
) -> Result<SampleResult, String> {
    use crate::domain::stt::vad::EnergyVad;
    use crate::infra::local_engine::worker_proto::TranscribeOptions;

    let wav_bytes = std::fs::read(wav_path).map_err(|e| format!("读取 WAV 失败: {e}"))?;
    let samples = parse_wav_to_f32(&wav_bytes).map_err(|e| format!("解析 WAV 失败: {e}"))?;
    let audio_duration_ms = (samples.len() / 16) as u64;

    // 根据 VAD 模式决定是否切分
    let segments: Vec<Vec<f32>> = if vad == "energy_vad" {
        let mut vad_inst = EnergyVad::new(16000);
        let (segs, _speech) = vad_segment_audio(&samples, &mut vad_inst);
        tracing::info!(round, segments = segs.len(), "VAD 切分完成 (GGUF 路径)");
        segs
    } else if vad == "fsmn_vad" {
        let runner = fsmn_runner.ok_or("fsmn_vad 组合缺少 runner（内部错误）")?;
        let (segs, _speech) = fsmn_vad_segment_audio(runner, &samples)?;
        tracing::info!(
            round,
            segments = segs.len(),
            "FSMN-VAD 切分完成 (GGUF 路径)"
        );
        segs
    } else {
        vec![samples.clone()]
    };

    if segments.is_empty() {
        return Ok(SampleResult {
            sample_id: String::new(),
            reference_raw: reference_text.to_string(),
            hypothesis_raw: String::new(),
            reference_normalized: normalize_for_cer(reference_text),
            hypothesis_normalized: String::new(),
            cer: 1.0,
            accuracy: 0.0,
            first_partial_ms: None,
            final_after_release_ms: None,
            audio_duration_ms,
            inference_wall_ms: 0,
            inference_only_ms: None,
            rtf: 0.0,
            rtf_infer: None,
            busy_count: 0,
            error: Some("VAD 切分后无 speech segment".into()),
        });
    }

    let infer_start = Instant::now();
    let mut hypothesis_text = String::new();
    let mut transcribe_errors: Vec<String> = Vec::new();

    for (seg_idx, segment) in segments.iter().enumerate() {
        // 写 segment 到临时 WAV 文件
        let file_name = format!(
            "gate-{}-seg{}-{}.wav",
            round,
            seg_idx,
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0),
        );
        let dest_path = audio_dir.join(&file_name);
        write_pcm_to_wav(&dest_path, segment, 16000)
            .map_err(|e| format!("写入临时 WAV 失败: {e}"))?;

        let canonical = dest_path
            .canonicalize()
            .map_err(|e| format!("canonicalize 失败: {e}"))?;

        let options = TranscribeOptions::default();
        let result = client
            .transcribe(&canonical, &options, Duration::from_secs(120))
            .await;

        let _ = std::fs::remove_file(&dest_path);

        match result {
            Ok(output) => {
                if !hypothesis_text.is_empty() && !output.text.is_empty() {
                    hypothesis_text.push_str("");
                }
                hypothesis_text.push_str(&output.text);
            }
            Err(e) => {
                tracing::warn!(round, seg_idx, %e, "GGUF segment transcribe 失败");
                transcribe_errors.push(format!("segment {seg_idx}: {e}"));
            }
        }
    }

    let inference_wall = infer_start.elapsed();

    let cer = calculate_cer(&hypothesis_text, reference_text);
    let final_after_release_ms = Some(inference_wall.as_millis() as u64);
    let rtf = if audio_duration_ms > 0 {
        inference_wall.as_millis() as f64 / audio_duration_ms as f64
    } else {
        0.0
    };

    let preview: String = hypothesis_text.chars().take(60).collect();
    tracing::info!(
        round,
        cer = format!("{cer:.3}"),
        final_ms = final_after_release_ms.unwrap_or(0),
        rtf = format!("{rtf:.3}"),
        %preview,
        "GGUF 样本完成"
    );

    Ok(SampleResult {
        sample_id: String::new(), // 调用者填充
        reference_raw: reference_text.to_string(),
        hypothesis_raw: hypothesis_text.clone(),
        reference_normalized: normalize_for_cer(reference_text),
        hypothesis_normalized: normalize_for_cer(&hypothesis_text),
        cer,
        accuracy: 1.0 - cer,
        first_partial_ms: None, // GGUF 非流式，无 partial
        final_after_release_ms,
        audio_duration_ms,
        inference_wall_ms: inference_wall.as_millis() as u64,
        // GGUF 路径无 10ms tick 等待——整段音频一次性发给 worker，
        // inference_wall_ms 即为纯推理时间。
        inference_only_ms: Some(inference_wall.as_millis() as u64),
        rtf,
        rtf_infer: if audio_duration_ms > 0 {
            Some(inference_wall.as_millis() as f64 / audio_duration_ms as f64)
        } else {
            None
        },
        busy_count: 0,
        error: if transcribe_errors.is_empty() {
            None
        } else {
            Some(transcribe_errors.join("; "))
        },
    })
}

/// 处理单条 corpus 样本——按 16kHz 实时时钟投喂音频。
async fn process_sample(
    adapter: &crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter,
    wav_path: &Path,
    reference_text: &str,
    round: usize,
) -> Result<SampleResult, String> {
    use crate::domain::stt::{StreamingSttPort, SttEvent};

    let wav_bytes = std::fs::read(wav_path).map_err(|e| format!("读取 WAV 失败: {e}"))?;
    let samples = parse_wav_to_f32(&wav_bytes).map_err(|e| format!("解析 WAV 失败: {e}"))?;
    let audio_duration_ms = (samples.len() / 16) as u64;

    let begin_start = Instant::now();
    let generation = adapter
        .begin_session()
        .await
        .map_err(|e| format!("begin_session 失败: {e}"))?;

    let mut rx = adapter.events();

    let chunk_size = 160usize;
    let mut first_partial_time: Option<Instant> = None;
    let mut busy_count = 0u32;
    let mut feed_start: Option<Instant> = None;

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
                    }
                }
                SttEvent::Busy { .. } => busy_count += 1,
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

    let finish_start = Instant::now();
    if let Err(e) = adapter.finish_session(generation).await {
        tracing::warn!(round, %e, "finish_session 失败");
    }

    let mut hypothesis_text = String::new();
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
                    hypothesis_text = text;
                    final_arrived = true;
                    break;
                }
                SttEvent::Busy { .. } => busy_count += 1,
                SttEvent::Error {
                    generation: evt_gen,
                    message,
                } if evt_gen == generation => {
                    return Ok(SampleResult {
                        sample_id: String::new(), // 调用者填充
                        reference_raw: reference_text.to_string(),
                        hypothesis_raw: String::new(),
                        reference_normalized: normalize_for_cer(reference_text),
                        hypothesis_normalized: String::new(),
                        cer: 1.0,
                        accuracy: 0.0,
                        first_partial_ms: first_partial_time
                            .zip(feed_start)
                            .map(|(fp, fs)| fp.duration_since(fs).as_millis() as u64),
                        final_after_release_ms: None,
                        audio_duration_ms,
                        inference_wall_ms: begin_start.elapsed().as_millis() as u64,
                        inference_only_ms: None,
                        rtf: 0.0,
                        rtf_infer: None,
                        busy_count,
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
    let cer = calculate_cer(&hypothesis_text, reference_text);

    tracing::info!(
        round,
        cer = format!("{cer:.3}"),
        first_partial_ms = first_partial_ms.unwrap_or(0),
        final_after_release_ms = final_after_release_ms.unwrap_or(0),
        rtf = format!("{rtf:.3}"),
        "样本完成"
    );

    Ok(SampleResult {
        sample_id: String::new(), // 调用者填充
        reference_raw: reference_text.to_string(),
        hypothesis_raw: hypothesis_text.clone(),
        reference_normalized: normalize_for_cer(reference_text),
        hypothesis_normalized: normalize_for_cer(&hypothesis_text),
        cer,
        accuracy: 1.0 - cer,
        first_partial_ms,
        final_after_release_ms,
        audio_duration_ms,
        inference_wall_ms: inference_wall.as_millis() as u64,
        // inference_only_ms: 用 final_after_release_ms 近似——
        // 这是投喂完毕后 worker final flush 的纯推理时间（不含 10ms tick 等待）。
        // 精确的累计 _inf_ms 需协议扩展，当前作为近似值。
        inference_only_ms: final_after_release_ms,
        rtf,
        rtf_infer: final_after_release_ms.and_then(|ms| {
            if audio_duration_ms > 0 {
                Some(ms as f64 / audio_duration_ms as f64)
            } else {
                None
            }
        }),
        busy_count,
        error: None,
    })
}

/// 处理单条 corpus 样本——VAD 预处理后逐段投喂 ASR。
///
/// 与 `process_sample` 的区别：
/// - 先用 EnergyVad 把 WAV 切分成 speech segments
/// - 对每个 segment 独立 begin→push→finish
/// - 拼接所有 segment 的 Final 文本作为最终 hypothesis
///
/// **指标口径**：
/// - `audio_duration_ms`：原始 WAV 全长（含静音），与 asr_only 一致
/// - `first_partial_ms`：首个 segment 的首个 Partial 延迟
/// - `final_after_release_ms`：最后一个 segment 的 finish→Final 延迟
/// - `rtf`：总推理 wall / 原始音频时长
async fn process_sample_vad(
    adapter: &crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter,
    wav_path: &Path,
    reference_text: &str,
    round: usize,
) -> Result<SampleResult, String> {
    use crate::domain::stt::vad::EnergyVad;
    use crate::domain::stt::{StreamingSttPort, SttEvent};

    let wav_bytes = std::fs::read(wav_path).map_err(|e| format!("读取 WAV 失败: {e}"))?;
    let all_samples = parse_wav_to_f32(&wav_bytes).map_err(|e| format!("解析 WAV 失败: {e}"))?;
    let audio_duration_ms = (all_samples.len() / 16) as u64;

    // VAD 切分
    let mut vad = EnergyVad::new(16000);
    let (segments, total_speech_samples) = vad_segment_audio(&all_samples, &mut vad);
    let total_speech_ms = (total_speech_samples / 16) as u64;

    tracing::info!(
        round,
        segments = segments.len(),
        total_speech_ms,
        audio_duration_ms,
        "VAD 切分完成"
    );

    if segments.is_empty() {
        return Ok(SampleResult {
            sample_id: String::new(),
            reference_raw: reference_text.to_string(),
            hypothesis_raw: String::new(),
            reference_normalized: normalize_for_cer(reference_text),
            hypothesis_normalized: String::new(),
            cer: 1.0,
            accuracy: 0.0,
            first_partial_ms: None,
            final_after_release_ms: None,
            audio_duration_ms,
            inference_wall_ms: 0,
            inference_only_ms: None,
            rtf: 0.0,
            rtf_infer: None,
            busy_count: 0,
            error: Some("VAD 切分后无 speech segment".into()),
        });
    }

    let begin_start = Instant::now();
    let mut hypothesis_text = String::new();
    let mut first_partial_time: Option<Instant> = None;
    let mut feed_start: Option<Instant> = None;
    let mut busy_count = 0u32;
    let mut last_finish_elapsed: Option<Duration> = None;
    let mut segment_errors: Vec<String> = Vec::new();

    let chunk_size = 160usize; // 10ms @ 16kHz

    for (seg_idx, segment) in segments.iter().enumerate() {
        let generation = match adapter.begin_session().await {
            Ok(g) => g,
            Err(e) => {
                segment_errors.push(format!("segment {seg_idx} begin_session: {e}"));
                continue;
            }
        };

        let mut rx = adapter.events();
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

        for chunk in segment.chunks(chunk_size) {
            interval.tick().await;
            if feed_start.is_none() {
                feed_start = Some(Instant::now());
            }
            if let Err(e) = adapter.push_audio(generation, chunk).await {
                tracing::warn!(round, seg_idx, %e, "push_audio 失败");
            }
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
                        }
                    }
                    SttEvent::Busy { .. } => busy_count += 1,
                    SttEvent::Error {
                        generation: evt_gen,
                        message,
                    } if evt_gen == generation => {
                        segment_errors.push(format!("segment {seg_idx}: {message}"));
                        break;
                    }
                    _ => {}
                }
            }
        }

        let finish_start = Instant::now();
        if let Err(e) = adapter.finish_session(generation).await {
            tracing::warn!(round, seg_idx, %e, "finish_session 失败");
        }

        // 收集 Final
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
                        if !hypothesis_text.is_empty() && !text.is_empty() {
                            hypothesis_text.push_str("");
                        }
                        hypothesis_text.push_str(&text);
                        break;
                    }
                    SttEvent::Busy { .. } => busy_count += 1,
                    SttEvent::Error {
                        generation: evt_gen,
                        message,
                    } if evt_gen == generation => {
                        segment_errors.push(format!("segment {seg_idx}: {message}"));
                        break;
                    }
                    _ => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        last_finish_elapsed = Some(finish_start.elapsed());

        let _ = adapter.reset().await;
    }

    let inference_wall = begin_start.elapsed();
    let first_partial_ms = first_partial_time
        .zip(feed_start)
        .map(|(fp, fs)| fp.duration_since(fs).as_millis() as u64);
    let final_after_release_ms = last_finish_elapsed.map(|d| d.as_millis() as u64);
    let rtf = if audio_duration_ms > 0 {
        inference_wall.as_millis() as f64 / audio_duration_ms as f64
    } else {
        0.0
    };
    let cer = calculate_cer(&hypothesis_text, reference_text);

    let error = if segment_errors.is_empty() {
        None
    } else {
        Some(segment_errors.join("; "))
    };

    tracing::info!(
        round,
        cer = format!("{cer:.3}"),
        first_partial_ms = first_partial_ms.unwrap_or(0),
        final_after_release_ms = final_after_release_ms.unwrap_or(0),
        rtf = format!("{rtf:.3}"),
        segments = segments.len(),
        "VAD 样本完成"
    );

    Ok(SampleResult {
        sample_id: String::new(),
        reference_raw: reference_text.to_string(),
        hypothesis_raw: hypothesis_text.clone(),
        reference_normalized: normalize_for_cer(reference_text),
        hypothesis_normalized: normalize_for_cer(&hypothesis_text),
        cer,
        accuracy: 1.0 - cer,
        first_partial_ms,
        final_after_release_ms,
        audio_duration_ms,
        inference_wall_ms: inference_wall.as_millis() as u64,
        inference_only_ms: final_after_release_ms,
        rtf,
        rtf_infer: final_after_release_ms.and_then(|ms| {
            if audio_duration_ms > 0 {
                Some(ms as f64 / audio_duration_ms as f64)
            } else {
                None
            }
        }),
        busy_count,
        error,
    })
}

/// FSMN-VAD 路径的 ONNX 样本处理：用 FSMN-VAD 切分 → 逐段 begin/push/finish → 拼接文本。
///
/// 与 `process_sample_vad` 的区别仅在于切分方式——EnergyVad 是逐 chunk 事件驱动，
/// FSMN-VAD 是先跑完整音频收集时间戳再按时间戳切分。
async fn process_sample_fsmn_vad(
    adapter: &crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter,
    wav_path: &Path,
    reference_text: &str,
    round: usize,
    runner: &mut crate::infra::stt::fsmn_vad_runner::FsmnVadRunner,
) -> Result<SampleResult, String> {
    use crate::domain::stt::{StreamingSttPort, SttEvent};

    let wav_bytes = std::fs::read(wav_path).map_err(|e| format!("读取 WAV 失败: {e}"))?;
    let all_samples = parse_wav_to_f32(&wav_bytes).map_err(|e| format!("解析 WAV 失败: {e}"))?;
    let audio_duration_ms = (all_samples.len() / 16) as u64;

    // FSMN-VAD 切分（runner 为组合级复用实例）
    let (segments, total_speech_samples) = fsmn_vad_segment_audio(runner, &all_samples)?;
    let total_speech_ms = (total_speech_samples / 16) as u64;

    tracing::info!(
        round,
        segments = segments.len(),
        total_speech_ms,
        audio_duration_ms,
        "FSMN-VAD 切分完成"
    );

    if segments.is_empty() {
        return Ok(SampleResult {
            sample_id: String::new(),
            reference_raw: reference_text.to_string(),
            hypothesis_raw: String::new(),
            reference_normalized: normalize_for_cer(reference_text),
            hypothesis_normalized: String::new(),
            cer: 1.0,
            accuracy: 0.0,
            first_partial_ms: None,
            final_after_release_ms: None,
            audio_duration_ms,
            inference_wall_ms: 0,
            inference_only_ms: None,
            rtf: 0.0,
            rtf_infer: None,
            busy_count: 0,
            error: Some("FSMN-VAD 切分后无 speech segment".into()),
        });
    }

    let begin_start = Instant::now();
    let mut hypothesis_text = String::new();
    let mut first_partial_time: Option<Instant> = None;
    let mut feed_start: Option<Instant> = None;
    let mut busy_count = 0u32;
    let mut last_finish_elapsed: Option<Duration> = None;
    let mut segment_errors: Vec<String> = Vec::new();

    let chunk_size = 160usize; // 10ms @ 16kHz

    for (seg_idx, segment) in segments.iter().enumerate() {
        let generation = match adapter.begin_session().await {
            Ok(g) => g,
            Err(e) => {
                segment_errors.push(format!("segment {seg_idx} begin_session: {e}"));
                continue;
            }
        };

        let mut rx = adapter.events();
        let mut interval = tokio::time::interval(Duration::from_millis(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Burst);

        for chunk in segment.chunks(chunk_size) {
            interval.tick().await;
            if feed_start.is_none() {
                feed_start = Some(Instant::now());
            }
            if let Err(e) = adapter.push_audio(generation, chunk).await {
                tracing::warn!(round, seg_idx, %e, "push_audio 失败");
            }
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
                        }
                    }
                    SttEvent::Busy { .. } => busy_count += 1,
                    SttEvent::Error {
                        generation: evt_gen,
                        message,
                    } if evt_gen == generation => {
                        segment_errors.push(format!("segment {seg_idx}: {message}"));
                        break;
                    }
                    _ => {}
                }
            }
        }

        let finish_start = Instant::now();
        if let Err(e) = adapter.finish_session(generation).await {
            tracing::warn!(round, seg_idx, %e, "finish_session 失败");
        }

        // 收集 Final
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
                        if !hypothesis_text.is_empty() && !text.is_empty() {
                            hypothesis_text.push_str("");
                        }
                        hypothesis_text.push_str(&text);
                        break;
                    }
                    SttEvent::Busy { .. } => busy_count += 1,
                    SttEvent::Error {
                        generation: evt_gen,
                        message,
                    } if evt_gen == generation => {
                        segment_errors.push(format!("segment {seg_idx}: {message}"));
                        break;
                    }
                    _ => {}
                },
                Ok(None) => break,
                Err(_) => break,
            }
        }

        last_finish_elapsed = Some(finish_start.elapsed());

        let _ = adapter.reset().await;
    }

    let inference_wall = begin_start.elapsed();
    let first_partial_ms = first_partial_time
        .zip(feed_start)
        .map(|(fp, fs)| fp.duration_since(fs).as_millis() as u64);
    let final_after_release_ms = last_finish_elapsed.map(|d| d.as_millis() as u64);
    let rtf = if audio_duration_ms > 0 {
        inference_wall.as_millis() as f64 / audio_duration_ms as f64
    } else {
        0.0
    };
    let cer = calculate_cer(&hypothesis_text, reference_text);

    let error = if segment_errors.is_empty() {
        None
    } else {
        Some(segment_errors.join("; "))
    };

    tracing::info!(
        round,
        cer = format!("{cer:.3}"),
        first_partial_ms = first_partial_ms.unwrap_or(0),
        final_after_release_ms = final_after_release_ms.unwrap_or(0),
        rtf = format!("{rtf:.3}"),
        segments = segments.len(),
        "FSMN-VAD 样本完成"
    );

    Ok(SampleResult {
        sample_id: String::new(),
        reference_raw: reference_text.to_string(),
        hypothesis_raw: hypothesis_text.clone(),
        reference_normalized: normalize_for_cer(reference_text),
        hypothesis_normalized: normalize_for_cer(&hypothesis_text),
        cer,
        accuracy: 1.0 - cer,
        first_partial_ms,
        final_after_release_ms,
        audio_duration_ms,
        inference_wall_ms: inference_wall.as_millis() as u64,
        inference_only_ms: final_after_release_ms,
        rtf,
        rtf_infer: final_after_release_ms.and_then(|ms| {
            if audio_duration_ms > 0 {
                Some(ms as f64 / audio_duration_ms as f64)
            } else {
                None
            }
        }),
        busy_count,
        error,
    })
}

/// 生命周期测试（100 次 start/stop + 10 次 kill/restart）。
async fn run_lifecycle_test(
    deployment_dir: &Path,
    ready_timeout_secs: u64,
) -> Result<LifecycleResult, String> {
    use crate::domain::stt::StreamingSttPort;
    use crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter;

    let ready_timeout = Duration::from_secs(ready_timeout_secs);
    let lifecycle_count = 100usize;
    let kill_count = 10usize;

    let adapter = ParaformerOnlineAdapter::launch(deployment_dir.to_path_buf(), ready_timeout)
        .await
        .map_err(|e| format!("初始 worker 启动失败: {e}"))?;

    let mut success_count = 0usize;
    let mut failure_count = 0usize;
    let mut orphan_count = 0usize;
    let mut stale_gen_count = 0usize;
    let mut deadlock_count = 0usize;

    for round in 0..lifecycle_count {
        let round_start = Instant::now();
        let use_cancel = round % 2 == 0;

        match adapter.begin_session().await {
            Ok(generation) => {
                let mut rx = adapter.events();
                let dummy = vec![0.0f32; 3200];
                let _ = adapter.push_audio(generation, &dummy).await;

                while let Ok(event) = rx.try_recv() {
                    if let crate::domain::stt::SttEvent::Partial {
                        generation: evt_gen,
                        ..
                    }
                    | crate::domain::stt::SttEvent::Final {
                        generation: evt_gen,
                        ..
                    } = &event
                    {
                        if *evt_gen != generation {
                            stale_gen_count += 1;
                        }
                    }
                }

                if use_cancel {
                    let _ = adapter.cancel_session(generation).await;
                    success_count += 1;
                } else {
                    let _ = adapter.finish_session(generation).await;
                    let deadline = Instant::now() + Duration::from_secs(15);
                    loop {
                        let remaining = deadline.saturating_duration_since(Instant::now());
                        if remaining.is_zero() {
                            failure_count += 1;
                            break;
                        }
                        match tokio::time::timeout(remaining, rx.recv()).await {
                            Ok(Some(crate::domain::stt::SttEvent::Final { .. })) => {
                                success_count += 1;
                                break;
                            }
                            Ok(Some(_)) => {}
                            Ok(None) | Err(_) => {
                                failure_count += 1;
                                break;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                failure_count += 1;
                tracing::warn!(round, %e, "begin_session 失败");
            }
        }

        let _ = adapter.reset().await;
        if round_start.elapsed().as_millis() > 60_000 {
            deadlock_count += 1;
            tracing::warn!(round, "检测到疑似死锁（单轮 > 60s）");
        }
    }

    let _ = adapter.stop().await;
    drop(adapter);
    tokio::time::sleep(Duration::from_millis(500)).await;

    // kill/restart
    let mut kr_success = 0usize;
    for round in 0..kill_count {
        match ParaformerOnlineAdapter::launch(deployment_dir.to_path_buf(), ready_timeout).await {
            Ok(a) => {
                let _ = a.stop().await;
                drop(a);
                tokio::time::sleep(Duration::from_millis(300)).await;
                kr_success += 1;
            }
            Err(e) => {
                tracing::warn!(round, %e, "kill/restart: launch 失败");
            }
        }
    }

    // orphan 检查
    #[cfg(windows)]
    {
        let output = std::process::Command::new("tasklist")
            .args(["/FI", "IMAGENAME eq blink.exe", "/FO", "CSV", "/NH"])
            .output()
            .ok();
        if let Some(out) = output {
            let stdout = String::from_utf8_lossy(&out.stdout);
            let current_pid = std::process::id();
            for line in stdout.lines() {
                if line.contains("blink.exe") {
                    let parts: Vec<&str> = line.split(',').collect();
                    if parts.len() >= 2 {
                        let pid_str = parts[1].trim_matches('"');
                        if let Ok(pid) = pid_str.parse::<u32>() {
                            if pid != current_pid {
                                orphan_count += 1;
                            }
                        }
                    }
                }
            }
        }
    }

    Ok(LifecycleResult {
        total_start_stop: lifecycle_count,
        success_count,
        failure_count,
        orphan_count,
        stale_generation_count: stale_gen_count,
        deadlock_count,
        reset_reproducible: failure_count == 0,
        kill_restart_count: kill_count,
        kill_restart_success: kr_success,
    })
}

// ── 判定逻辑 ─────────────────────────────────────────────────────────────

#[allow(clippy::collapsible_if)]
#[allow(clippy::unnecessary_unwrap)]
#[allow(clippy::if_same_then_else)]
fn evaluate_verdicts(
    combinations: &[CombinationResult],
    lifecycle: &Option<LifecycleResult>,
) -> (String, String, String, Vec<String>, Vec<String>) {
    let mut failed_items = Vec::new();
    let mut next_steps = Vec::new();

    // ── Paraformer 注册门 ──────────────────────────────────────────────
    // 阈值：
    //   first_partial p50 ≤ 400ms, p95 < 800ms
    //   final_after_release p95 ≤ 800ms
    //   RTF p95 < 0.8
    //   CER 相对 Nano 恶化不超过 1 个百分点（无 Nano baseline 时，绝对 CER 记录但不判 GO/NO_GO）
    //   lifecycle: 零 orphan、零死锁、零旧 generation 泄漏

    let paraformer_combo = combinations
        .iter()
        .find(|c| c.stt_engine == "paraformer_onnx" && c.vad == "asr_only");

    let mut paraformer_go = true;

    if let Some(combo) = paraformer_combo {
        if combo.samples.is_empty() {
            paraformer_go = false;
            failed_items.push("Paraformer: 无样本数据（未测试）".into());
        } else {
            // first_partial p50 ≤ 400ms, p95 < 800ms
            if let Some(ref stats) = combo.first_partial_stats {
                if stats.p50 > 400.0 {
                    paraformer_go = false;
                    failed_items.push(format!(
                        "Paraformer: first_partial p50={:.0}ms > 400ms",
                        stats.p50
                    ));
                }
                if stats.p95 >= 800.0 {
                    paraformer_go = false;
                    failed_items.push(format!(
                        "Paraformer: first_partial p95={:.0}ms >= 800ms",
                        stats.p95
                    ));
                }
            } else {
                paraformer_go = false;
                failed_items.push("Paraformer: 无 first_partial 数据".into());
            }

            // final_after_release p95 ≤ 800ms
            if let Some(ref stats) = combo.final_after_release_stats {
                if stats.p95 > 800.0 {
                    paraformer_go = false;
                    failed_items.push(format!(
                        "Paraformer: final_after_release p95={:.0}ms > 800ms",
                        stats.p95
                    ));
                }
            }

            // RTF p95 < 0.8
            if let Some(ref stats) = combo.rtf_stats {
                if stats.p95 >= 0.8 {
                    paraformer_go = false;
                    failed_items.push(format!("Paraformer: RTF p95={:.3} >= 0.8", stats.p95));
                }
            }

            // 1.2s 有效语音在句尾前产生非空 Partial
            let no_partial_count = combo
                .samples
                .iter()
                .filter(|s| s.first_partial_ms.is_none() && s.error.is_none())
                .count();
            if no_partial_count > 0 {
                paraformer_go = false;
                failed_items.push(format!(
                    "Paraformer: {no_partial_count} 条样本无 Partial（应 <1.2s 产生）"
                ));
            }

            // §九: 推理错误样本不得放行
            if combo.total_errors > 0 {
                paraformer_go = false;
                failed_items.push(format!(
                    "Paraformer: {}/{} 条样本有推理错误",
                    combo.total_errors,
                    combo.samples.len()
                ));
            }

            // CER 记录（无 Nano baseline 不判 GO/NO_GO）
            // 注意：gate runner 当前为 ASR-only 诊断——未经 VAD 切分，
            // 音频按 10ms tick 直接投喂至 ASR 引擎。
            // VAD × ASR 矩阵留待后续扩展。
            if let Some(ref stats) = combo.cer_stats {
                tracing::info!(
                    "Paraformer CER: mean={:.3} p50={:.3} p95={:.3}",
                    stats.mean,
                    stats.p50,
                    stats.p95
                );
            }
        }
    } else {
        paraformer_go = false;
        failed_items.push("Paraformer: 组合未运行".into());
    }

    // lifecycle 检查
    if let Some(lc) = lifecycle {
        if lc.orphan_count > 0 {
            paraformer_go = false;
            failed_items.push(format!(
                "Paraformer lifecycle: {} orphan 进程",
                lc.orphan_count
            ));
        }
        if lc.deadlock_count > 0 {
            paraformer_go = false;
            failed_items.push(format!("Paraformer lifecycle: {} 死锁", lc.deadlock_count));
        }
        if lc.stale_generation_count > 0 {
            paraformer_go = false;
            failed_items.push(format!(
                "Paraformer lifecycle: {} 旧 generation 泄漏",
                lc.stale_generation_count
            ));
        }
    } else {
        paraformer_go = false;
        failed_items.push("Paraformer lifecycle: 未测试".into());
    }

    let paraformer_verdict = if paraformer_go {
        "REGISTER_GO".to_string()
    } else {
        // 07E-R: 修复并完成全量 release gate 前，Paraformer 状态只能是
        // REGISTER_NOT_EVALUATED——不得基于不完整/无效测试判定 NO_GO
        "REGISTER_NOT_EVALUATED".to_string()
    };

    // ── FSMN 采用门 ────────────────────────────────────────────────────
    // 阈值：
    //   下游 CER 相对 EnergyVad 恶化不超过 0.5 个百分点
    //   句界 F1 不低于 EnergyVad 超过 0.02（当前无句界标注，记录但不判）
    //   executor 不积压、不阻塞 audio callback

    let fsmn_combo = combinations
        .iter()
        .find(|c| c.stt_engine == "paraformer_onnx" && c.vad == "fsmn_vad");

    let fsmn_verdict = if fsmn_combo.is_none() {
        "NOT_TESTED".to_string()
    } else if paraformer_combo.is_none() {
        "NOT_TESTED".to_string()
    } else {
        let fsmn = fsmn_combo.unwrap();
        let energy = paraformer_combo.unwrap();

        if fsmn.samples.is_empty() {
            "NOT_TESTED".to_string()
        } else {
            let mut fsmn_go = true;

            // CER 恶化 ≤ 0.5 个百分点
            if let (Some(f_cer), Some(e_cer)) = (&fsmn.cer_stats, &energy.cer_stats) {
                let delta = f_cer.mean - e_cer.mean;
                if delta > 0.005 {
                    fsmn_go = false;
                    failed_items.push(format!(
                        "FSMN: CER 恶化 {delta:.4} > 0.005 (fsmn={:.3} vs energy={:.3})",
                        f_cer.mean, e_cer.mean
                    ));
                }
            }

            // Busy 对比（executor 不积压）
            if fsmn.total_busy > energy.total_busy * 2 + 10 {
                fsmn_go = false;
                failed_items.push(format!(
                    "FSMN: busy 事件 {} 远超 energy {} (>2x+10)",
                    fsmn.total_busy, energy.total_busy
                ));
            }

            if fsmn_go {
                "ADOPT".to_string()
            } else {
                "KEEP_ENERGY".to_string()
            }
        }
    };

    // ── 默认模型建议 ───────────────────────────────────────────────────
    let default_rec = if paraformer_verdict == "REGISTER_GO" {
        if fsmn_verdict == "ADOPT" {
            "具备讨论 fresh-install 默认 ParaformerOnline + FSMN-VAD 的资格".to_string()
        } else if fsmn_verdict == "KEEP_ENERGY" {
            "具备讨论 fresh-install 默认 ParaformerOnline + EnergyVad 的资格".to_string()
        } else {
            "具备讨论 fresh-install 默认 ParaformerOnline + EnergyVad 的资格（FSMN 未测试）"
                .to_string()
        }
    } else {
        "不具备讨论 fresh-install 默认模型的资格——Paraformer 未通过注册门".to_string()
    };

    // ── 下一步 ─────────────────────────────────────────────────────────
    if paraformer_verdict == "REGISTER_NOT_EVALUATED" {
        next_steps.push("Paraformer 尚未完成有效评测——需修复后重跑全量 release gate".into());
    }
    if fsmn_verdict == "KEEP_ENERGY" {
        next_steps.push("FSMN 未达标，保持 EnergyVad，修复后重测".into());
    }
    if fsmn_verdict == "NOT_TESTED" && !failed_items.is_empty() {
        next_steps.push("FSMN 未测试——部署模型后补充测试".into());
    }
    // GGUF 对比分析
    let gguf_combos: Vec<_> = combinations
        .iter()
        .filter(|c| c.stt_engine.contains("gguf") && !c.samples.is_empty())
        .collect();
    if !gguf_combos.is_empty() {
        next_steps.push("GGUF 组合已测试——对比 CER/RTF 与 ParaformerOnline ONNX 的差异".into());
    }

    (
        paraformer_verdict,
        fsmn_verdict,
        default_rec,
        failed_items,
        next_steps,
    )
}

// ── CSV 生成 ─────────────────────────────────────────────────────────────

fn generate_csv(report: &GateReport) -> String {
    let mut csv = String::new();
    csv.push_str("combination,vad,stt_engine,path_type,model_identity,sample_id,cer,first_partial_ms,final_after_release_ms,inference_wall_ms,inference_only_ms,rtf,rtf_infer,busy_count,error,reference_raw,hypothesis_raw\n");

    for combo in &report.combinations {
        if combo.samples.is_empty() {
            csv.push_str(&format!(
                "{},{},{},(no_samples),,,,,,,\n",
                combo.name, combo.vad, combo.stt_engine
            ));
            continue;
        }
        for s in &combo.samples {
            csv.push_str(&format!(
                "{},{},{},{},{},{},{:.4},{},{},{},{},{:.3},{},{},{},\"{}\",\"{}\"\n",
                combo.name,
                combo.vad,
                combo.stt_engine,
                combo.path_type,
                combo.model_identity,
                s.sample_id,
                s.cer,
                s.first_partial_ms
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                s.final_after_release_ms
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                s.inference_wall_ms,
                s.inference_only_ms
                    .map(|v| v.to_string())
                    .unwrap_or_default(),
                s.rtf,
                s.rtf_infer.map(|v| format!("{v:.3}")).unwrap_or_default(),
                s.busy_count,
                s.error.as_deref().unwrap_or(""),
                s.reference_raw.replace('"', "'"),
                s.hypothesis_raw.replace('"', "'"),
            ));
        }
    }

    csv
}

// ── Markdown 报告生成 ───────────────────────────────────────────────────

fn generate_markdown(report: &GateReport) -> String {
    let mut md = String::new();

    md.push_str("# STT Production Gate Report (Handoff 07E-R)\n\n");
    md.push_str(&format!(
        "**Generated**: {}\n\n",
        report.metadata.generated_at
    ));
    md.push_str(&format!("**Version**: {}\n\n", report.metadata.version));
    md.push_str(&format!(
        "**Corpus**: {} ({} samples, {:.1}s total)\n\n",
        report.metadata.corpus_dir,
        report.metadata.corpus_sample_count,
        report.metadata.corpus_total_duration_s
    ));
    md.push_str(&format!(
        "**Corpus manifest SHA-256**: `{}`\n\n",
        report.corpus_manifest_hash
    ));

    // ── 结论 ──
    md.push_str("## 结论\n\n");
    md.push_str("| 项目 | 结论 |\n|---|---|\n");
    md.push_str(&format!(
        "| Paraformer 注册门 | **{}** |\n",
        report.paraformer_verdict
    ));
    md.push_str(&format!("| FSMN 采用门 | **{}** |\n", report.fsmn_verdict));
    md.push_str(&format!(
        "| 默认模型资格 | {} |\n\n",
        report.default_model_recommendation
    ));

    // ── 矩阵结果 ──
    md.push_str("## 矩阵结果\n\n");
    for combo in &report.combinations {
        md.push_str(&format!("### {}\n\n", combo.name));
        md.push_str(&format!(
            "**Path**: `{}` | **Model**: `{}`\n\n",
            combo.path_type, combo.model_identity
        ));

        if combo.samples.is_empty() {
            if combo.total_errors > 0 {
                md.push_str("**状态**: ERROR\n\n");
            } else {
                md.push_str("**状态**: NOT TESTED (production path 未就绪)\n\n");
            }
            continue;
        }

        md.push_str("| 指标 | mean | p50 | p95 | min | max | n |\n");
        md.push_str("|---|---|---|---|---|---|---|\n");

        if let Some(ref s) = combo.cer_stats {
            md.push_str(&format!(
                "| CER | {:.4} | {:.4} | {:.4} | {:.4} | {:.4} | {} |\n",
                s.mean, s.p50, s.p95, s.min, s.max, s.count
            ));
        } else {
            md.push_str("| CER | - | - | - | - | - | - |\n");
        }

        if let Some(ref s) = combo.accuracy_stats {
            md.push_str(&format!(
                "| Accuracy | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {:.2}% | {} |\n",
                s.mean, s.p50, s.p95, s.min, s.max, s.count
            ));
        } else {
            md.push_str("| Accuracy | - | - | - | - | - | - |\n");
        }

        if let Some(ref s) = combo.first_partial_stats {
            md.push_str(&format!(
                "| first_partial (ms) | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {} |\n",
                s.mean, s.p50, s.p95, s.min, s.max, s.count
            ));
        } else {
            md.push_str("| first_partial (ms) | - | - | - | - | - | - |\n");
        }

        if let Some(ref s) = combo.final_after_release_stats {
            md.push_str(&format!(
                "| final_after_release (ms) | {:.0} | {:.0} | {:.0} | {:.0} | {:.0} | {} |\n",
                s.mean, s.p50, s.p95, s.min, s.max, s.count
            ));
        } else {
            md.push_str("| final_after_release (ms) | - | - | - | - | - | - |\n");
        }

        if let Some(ref s) = combo.rtf_stats {
            md.push_str(&format!(
                "| RTF | {:.3} | {:.3} | {:.3} | {:.3} | {:.3} | {} |\n",
                s.mean, s.p50, s.p95, s.min, s.max, s.count
            ));
        } else {
            md.push_str("| RTF | - | - | - | - | - | - |\n");
        }

        md.push_str(&format!(
            "\n**总 Busy 事件**: {} | **总错误**: {}\n\n",
            combo.total_busy, combo.total_errors
        ));

        // 内存快照
        if !combo.memory_snapshots.is_empty() {
            md.push_str("**内存快照**:\n\n");
            md.push_str("| timestamp | main (MB) | worker (MB) |\n|---|---|---|\n");
            for m in &combo.memory_snapshots {
                md.push_str(&format!(
                    "| {} | {:.1} | {} |\n",
                    m.timestamp,
                    m.main_process_mb,
                    m.worker_process_mb
                        .map(|v| format!("{v:.1}"))
                        .unwrap_or("-".into())
                ));
            }
            md.push('\n');
        }

        // 失败样本
        let failed: Vec<_> = combo.samples.iter().filter(|s| s.error.is_some()).collect();
        if !failed.is_empty() {
            md.push_str("**失败样本**:\n\n");
            for s in failed {
                md.push_str(&format!(
                    "- `{}`: {}\n",
                    s.sample_id,
                    s.error.as_deref().unwrap_or("unknown")
                ));
            }
            md.push('\n');
        }
    }

    // ── Lifecycle ──
    if let Some(ref lc) = report.lifecycle {
        md.push_str("## Lifecycle 测试\n\n");
        md.push_str("| 指标 | 值 |\n|---|---|\n");
        md.push_str(&format!(
            "| start/stop 总次数 | {} |\n",
            lc.total_start_stop
        ));
        md.push_str(&format!("| 成功 | {} |\n", lc.success_count));
        md.push_str(&format!("| 失败 | {} |\n", lc.failure_count));
        md.push_str(&format!("| orphan 进程 | {} |\n", lc.orphan_count));
        md.push_str(&format!(
            "| 旧 generation 泄漏 | {} |\n",
            lc.stale_generation_count
        ));
        md.push_str(&format!("| 死锁 | {} |\n", lc.deadlock_count));
        md.push_str(&format!("| reset 可复现 | {} |\n", lc.reset_reproducible));
        md.push_str(&format!(
            "| kill/restart 次数 | {} |\n",
            lc.kill_restart_count
        ));
        md.push_str(&format!(
            "| kill/restart 成功 | {} |\n\n",
            lc.kill_restart_success
        ));
    } else {
        md.push_str("## Lifecycle 测试\n\n跳过（--skip-lifecycle）\n\n");
    }

    // ── 阈值说明 ──
    md.push_str("## 阈值\n\n");
    md.push_str("### Paraformer 注册门\n\n");
    md.push_str("| 指标 | 阈值 |\n|---|---|\n");
    md.push_str("| first_partial p50 | ≤ 400ms |\n");
    md.push_str("| first_partial p95 | < 800ms |\n");
    md.push_str("| final_after_release p95 | ≤ 800ms |\n");
    md.push_str("| RTF p95 | < 0.8 |\n");
    md.push_str("| CER 相对 Nano 恶化 | ≤ 1 个百分点 |\n");
    md.push_str("| lifecycle orphan | 零 |\n");
    md.push_str("| lifecycle 死锁 | 零 |\n");
    md.push_str("| lifecycle 旧 generation 泄漏 | 零 |\n\n");
    md.push_str("\n> **注意**: gate runner 当前为 ASR-only 诊断——未经 VAD 切分，\n> 音频按 10ms tick 实时投喂至 ASR 引擎。VAD × ASR 矩阵留待后续扩展。\n> RTF 包含实时投喂等待时间，RTF_infer 仅含纯推理耗时。\n\n");
    md.push_str("### FSMN 采用门\n\n");
    md.push_str("| 指标 | 阈值 |\n|---|---|\n");
    md.push_str("| CER 相对 EnergyVad 恶化 | ≤ 0.5 个百分点 |\n");
    md.push_str("| 句界 F1 | 不低于 EnergyVad 超过 0.02 |\n");
    md.push_str("| executor 积压/阻塞 | 不积压、不阻塞 |\n\n");

    // ── p50/p95 计算方法 ──
    md.push_str("## p50/p95 计算方法\n\n");
    md.push_str("1. 收集所有有效样本的指标值（排除 error 样本）\n");
    md.push_str("2. 升序排序\n");
    md.push_str("3. p50 = `sorted[floor(0.50 * (n-1))]`\n");
    md.push_str("4. p95 = `sorted[floor(0.95 * (n-1))]`\n\n");

    // ── 复现命令 ──
    md.push_str("## 复现命令\n\n");
    for cmd in &report.repro_commands {
        md.push_str(&format!("```bash\n{cmd}\n```\n\n"));
    }

    // ── 未通过项 ──
    if !report.failed_items.is_empty() {
        md.push_str("## 未通过项\n\n");
        for item in &report.failed_items {
            md.push_str(&format!("- {item}\n"));
        }
        md.push('\n');
    }

    // ── 下一步 ──
    if !report.next_steps.is_empty() {
        md.push_str("## 下一步\n\n");
        for step in &report.next_steps {
            md.push_str(&format!("- {step}\n"));
        }
        md.push('\n');
    }

    md
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── §A2: 文本契约定向测试 ──────────────────────────────────────────

    /// §A2.6: 静音得到合法空文本——空 hypothesis 的 CER 应为 1.0（全错），
    /// 不应被替换为 "(empty)" 字面量。
    #[test]
    fn empty_hypothesis_is_not_replaced_with_literal() {
        let cer = calculate_cer("", "测试文本");
        assert_eq!(
            cer, 1.0,
            "空 hypothesis 的 CER 应为 1.0，不应被替换为字面量"
        );
    }

    /// §A2: 空 reference 的空 hypothesis CER 应为 0.0（正确）。
    #[test]
    fn empty_hypothesis_empty_reference_is_zero_cer() {
        let cer = calculate_cer("", "");
        assert_eq!(cer, 0.0, "空对空 CER 应为 0.0");
    }

    /// §A2: 归一化去除标点和空白——中文标点、英文标点、全角标点。
    #[test]
    fn normalization_strips_punctuation_and_whitespace() {
        let normalized = normalize_for_cer("你好，世界！Hello, World.");
        assert_eq!(
            normalized, "你好世界helloworld",
            "归一化应去除所有标点和空白，英文小写"
        );
    }

    /// §A2: 归一化不去除中文数字和阿拉伯数字的语义等价（不替换）。
    #[test]
    fn normalization_preserves_numbers() {
        let n1 = normalize_for_cer("三个");
        let n2 = normalize_for_cer("3个");
        assert_ne!(n1, n2, "归一化不应将中文数字和阿拉伯数字视为等价");
    }

    /// §A2: CER 可以大于 1.0，不得强行截断。
    #[test]
    fn cer_can_exceed_one() {
        // hypothesis 比 reference 长，CER 应 > 1.0
        let cer = calculate_cer("你好世界测试", "好");
        assert!(
            cer > 1.0,
            "CER 应 > 1.0（hypothesis 比 reference 长得多），实际: {cer}"
        );
    }

    /// §A2: 非空 hypothesis 与空 reference 的 CER 应为 1.0。
    #[test]
    fn nonempty_hypothesis_empty_reference_is_one() {
        let cer = calculate_cer("识别文本", "");
        assert_eq!(cer, 1.0, "空 reference 的非空 hypothesis CER 应为 1.0");
    }

    /// §A2: CER 计算正确性——精确编辑距离验证。
    #[test]
    fn cer_exact_edit_distance() {
        // "你好" vs "你好世界" → 2 个插入，ref_len=2, CER=2/2=1.0
        let cer = calculate_cer("你好", "你好世界");
        // 等等，ref="你好世界"(4), hyp="你好"(2) → 2 个删除, CER=2/4=0.5
        assert!(
            (cer - 0.5).abs() < 0.001,
            "CER 应为 0.5（2 删除 / 4 ref chars），实际: {cer}"
        );
    }

    /// §A2: NFKC 归一化——全角括号和半角括号在归一化后一致。
    #[test]
    fn nfkc_normalization_fullwidth_halfwidth() {
        // 同一段文本，分别用全角括号和半角括号包裹
        // 归一化后括号都被去除，结果应一致
        let n1 = normalize_for_cer("测试（内容）");
        let n2 = normalize_for_cer("测试(内容)");
        assert_eq!(n1, n2, "全角和半角括号归一化后应一致");
        assert_eq!(n1, "测试内容");
    }

    // ── §九: 判定器防误放行测试 ──────────────────────────────────────────

    /// 构造一个"完美"的 Paraformer 组合结果（所有指标达标）。
    fn make_passing_paraformer_combo() -> CombinationResult {
        CombinationResult {
            name: "asr_only + paraformer_onnx".into(),
            vad: "asr_only".into(),
            stt_engine: "paraformer_onnx".into(),
            path_type: "worker-protocol".into(),
            model_identity: "test".into(),
            samples: vec![SampleResult {
                sample_id: "test_1".into(),
                reference_raw: "测试文本".into(),
                hypothesis_raw: "测试文本".into(),
                reference_normalized: "测试文本".into(),
                hypothesis_normalized: "测试文本".into(),
                cer: 0.0,
                accuracy: 1.0,
                first_partial_ms: Some(100),
                final_after_release_ms: Some(200),
                audio_duration_ms: 5000,
                inference_wall_ms: 100,
                inference_only_ms: Some(100),
                rtf: 0.02,
                rtf_infer: Some(0.02),
                busy_count: 0,
                error: None,
            }],
            cer_stats: Some(PercentileStats {
                count: 1,
                min: 0.0,
                max: 0.0,
                mean: 0.0,
                p50: 0.0,
                p95: 0.0,
            }),
            accuracy_stats: Some(PercentileStats {
                count: 1,
                min: 1.0,
                max: 1.0,
                mean: 1.0,
                p50: 1.0,
                p95: 1.0,
            }),
            first_partial_stats: Some(PercentileStats {
                count: 1,
                min: 100.0,
                max: 100.0,
                mean: 100.0,
                p50: 100.0,
                p95: 100.0,
            }),
            final_after_release_stats: Some(PercentileStats {
                count: 1,
                min: 200.0,
                max: 200.0,
                mean: 200.0,
                p50: 200.0,
                p95: 200.0,
            }),
            rtf_stats: Some(PercentileStats {
                count: 1,
                min: 0.02,
                max: 0.02,
                mean: 0.02,
                p50: 0.02,
                p95: 0.02,
            }),
            total_busy: 0,
            total_errors: 0,
            memory_snapshots: vec![],
        }
    }

    /// §九: 完美组合 + 完美 lifecycle → REGISTER_GO。
    #[test]
    fn verdict_pass_when_all_metrics_pass() {
        let combos = vec![make_passing_paraformer_combo()];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_eq!(para_verdict, "REGISTER_GO", "所有指标达标时应 GO");
        assert!(failed.is_empty(), "不应有 failed_items: {failed:?}");
    }

    /// §九: 缺 baseline（无 Paraformer 组合）→ 不能 GO。
    #[test]
    fn verdict_no_go_when_paraformer_combo_missing() {
        let combos: Vec<CombinationResult> = vec![];
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &None);
        assert_ne!(para_verdict, "REGISTER_GO", "缺 Paraformer 组合时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("组合未运行")),
            "应有'组合未运行'失败项: {failed:?}"
        );
    }

    /// §九: 缺 lifecycle → 不能 GO。
    #[test]
    fn verdict_no_go_when_lifecycle_missing() {
        let combos = vec![make_passing_paraformer_combo()];
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &None);
        assert_ne!(para_verdict, "REGISTER_GO", "缺 lifecycle 测试时不得放行");
        assert!(
            failed
                .iter()
                .any(|f| f.contains("lifecycle") && f.contains("未测试")),
            "应有'lifecycle 未测试'失败项: {failed:?}"
        );
    }

    /// §九: 空语料（samples 为空）→ 不能 GO。
    #[test]
    fn verdict_no_go_when_corpus_empty() {
        let mut combo = make_passing_paraformer_combo();
        combo.samples.clear();
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "空语料时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("无样本数据")),
            "应有'无样本数据'失败项: {failed:?}"
        );
    }

    /// §九: 无有效 Partial → 不能 GO。
    #[test]
    fn verdict_no_go_when_no_partial_data() {
        let mut combo = make_passing_paraformer_combo();
        // 移除 first_partial 数据
        combo.first_partial_stats = None;
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(
            para_verdict, "REGISTER_GO",
            "无 first_partial 数据时不得放行"
        );
        assert!(
            failed.iter().any(|f| f.contains("无 first_partial")),
            "应有'无 first_partial 数据'失败项: {failed:?}"
        );
    }

    /// §九: 样本有推理错误 → 不能 GO。
    #[test]
    fn verdict_no_go_when_sample_has_error() {
        let mut combo = make_passing_paraformer_combo();
        combo.samples[0].error = Some("forward panic".into());
        combo.samples[0].first_partial_ms = None;
        combo.total_errors = 1;
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "有推理错误时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("推理错误")),
            "应有推理错误失败项: {failed:?}"
        );
    }

    /// §九: RTF 超标 → 不能 GO。
    #[test]
    fn verdict_no_go_when_rtf_exceeds_threshold() {
        let mut combo = make_passing_paraformer_combo();
        combo.rtf_stats = Some(PercentileStats {
            count: 1,
            min: 0.9,
            max: 0.9,
            mean: 0.9,
            p50: 0.9,
            p95: 0.9,
        });
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "RTF p95 >= 0.8 时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("RTF")),
            "应有 RTF 超标失败项: {failed:?}"
        );
    }

    /// §九: first_partial 超标 → 不能 GO。
    #[test]
    fn verdict_no_go_when_first_partial_exceeds_threshold() {
        let mut combo = make_passing_paraformer_combo();
        combo.first_partial_stats = Some(PercentileStats {
            count: 1,
            min: 500.0,
            max: 500.0,
            mean: 500.0,
            p50: 500.0,
            p95: 900.0,
        });
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "first_partial 超标时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("first_partial")),
            "应有 first_partial 超标失败项: {failed:?}"
        );
    }

    /// §九: lifecycle 有 orphan 进程 → 不能 GO。
    #[test]
    fn verdict_no_go_when_lifecycle_has_orphans() {
        let combos = vec![make_passing_paraformer_combo()];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 2,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "有 orphan 进程时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("orphan")),
            "应有 orphan 失败项: {failed:?}"
        );
    }

    /// §九: lifecycle 有死锁 → 不能 GO。
    #[test]
    fn verdict_no_go_when_lifecycle_has_deadlocks() {
        let combos = vec![make_passing_paraformer_combo()];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 1,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert_ne!(para_verdict, "REGISTER_GO", "有死锁时不得放行");
        assert!(
            failed.iter().any(|f| f.contains("死锁")),
            "应有死锁失败项: {failed:?}"
        );
    }

    /// §九: 样本无 Partial 且无 Error → 不能 GO（应检测到缺失 Partial）。
    #[test]
    fn verdict_flags_missing_partial_in_clean_samples() {
        let mut combo = make_passing_paraformer_combo();
        // 设置一个样本没有 first_partial 也没有 error
        combo.samples[0].first_partial_ms = None;
        combo.samples[0].error = None;
        let combos = vec![combo];
        let lc = LifecycleResult {
            total_start_stop: 3,
            success_count: 3,
            failure_count: 0,
            orphan_count: 0,
            stale_generation_count: 0,
            deadlock_count: 0,
            reset_reproducible: true,
            kill_restart_count: 0,
            kill_restart_success: 0,
        };
        let (_para_verdict, _fsmn, _rec, failed, _steps) = evaluate_verdicts(&combos, &Some(lc));
        assert!(
            failed.iter().any(|f| f.contains("无 Partial")),
            "应检测到样本无 Partial: {failed:?}"
        );
    }
}
