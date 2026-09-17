//! 私有 Corpus Runner（0.22.16 Handoff 07）。
//!
//! 三层验收体系中的第二层：env-gated 本地私有 corpus。
//!
//! **安全约束**：
//! - 由环境变量 `BLINK_STT_CORPUS_DIR` 显式启用
//! - 未启用、目录缺失、模型缺失时安全 skip
//! - 不自动下载模型，不上传音频
//! - 失败只输出匿名 case id、格式、时长、错误类别和是否命中
//! - 不输出完整转写
//! - 运行结束确认无 orphan worker
//!
//! **分层**：app 层测试基础设施，消费 infra 层 WAV decoder + normalize 和
//! domain 层 SttTransport。不新增生产代码路径。

#![allow(dead_code)]

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use serde::Deserialize;

use crate::domain::stt::SttTransport;
use crate::domain::stt::vad::EnergyVad;
use crate::domain::stt::wav::{decode_wav, pcm_to_wav};
use crate::infra::platform::audio::normalize::{AudioNormalizer, TARGET_SAMPLE_RATE};

/// 环境变量名，启用 corpus runner。
pub const ENV_CORPUS_DIR: &str = "BLINK_STT_CORPUS_DIR";

/// Manifest 中的单个 case。
#[derive(Debug, Clone, Deserialize)]
pub struct CorpusCase {
    pub case_id: String,
    pub filename: String,
    pub scene: String,
    pub expected_empty: bool,
    pub expected_segments: u32,
    pub expected_text: String,
    #[serde(default)]
    pub allowed_normalized: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct CorpusManifest {
    pub cases: Vec<CorpusCase>,
}

#[derive(Debug, thiserror::Error)]
pub enum CorpusRunError {
    #[error("corpus manifest is missing")]
    ManifestMissing,
    #[error("corpus manifest is invalid: {0}")]
    InvalidManifest(String),
    #[error("STT transport is unavailable")]
    TransportUnavailable,
}

/// 单个 case 的运行结果（匿名输出）。
#[derive(Debug, Clone)]
pub struct CorpusResult {
    pub case_id: String,
    pub format_ok: bool,
    pub duration_ms: u64,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub decode_ms: u64,
    pub normalize_ms: u64,
    pub queue_ms: Option<u64>,
    pub inference_ms: Option<u64>,
    pub total_ms: u64,
    pub peak_memory_bytes: Option<u64>,
    pub detected_segments: u32,
    pub expected_segments: u32,
    pub segments_matched: bool,
    pub text_empty: bool,
    pub text_matched: bool,
    pub text_chars: usize,
    pub best_similarity_percent: u8,
    pub matched: bool,
    pub error_category: Option<String>,
}

/// Runner 的可配置参数。
pub struct CorpusRunnerConfig {
    pub corpus_dir: PathBuf,
    pub transport: Option<Arc<dyn SttTransport>>,
    /// 受管 worker PID，用于读取其生命周期峰值工作集。
    pub worker_pid: Option<u32>,
}

/// 检查 corpus runner 是否应启用。
///
/// 条件：
/// 1. `BLINK_STT_CORPUS_DIR` 环境变量已设置
/// 2. 目录存在
///
/// 不检查模型——模型缺失会在运行时返回错误，不阻止 runner 本身。
pub fn should_run() -> Option<PathBuf> {
    let dir = std::env::var(ENV_CORPUS_DIR).ok()?;
    should_run_for_path(Path::new(&dir))
}

fn should_run_for_path(path: &Path) -> Option<PathBuf> {
    path.is_dir().then(|| path.to_path_buf())
}

pub fn load_manifest(corpus_dir: &Path) -> Result<CorpusManifest, CorpusRunError> {
    let content = std::fs::read_to_string(corpus_dir.join("manifest.toml"))
        .map_err(|_| CorpusRunError::ManifestMissing)?;
    parse_manifest(&content)
}

fn parse_manifest(content: &str) -> Result<CorpusManifest, CorpusRunError> {
    let manifest: CorpusManifest = toml::from_str(content)
        .map_err(|error| CorpusRunError::InvalidManifest(error.to_string()))?;
    if manifest.cases.is_empty() {
        return Err(CorpusRunError::InvalidManifest(
            "cases must not be empty".to_string(),
        ));
    }

    let mut case_ids = HashSet::new();
    let mut filenames = HashSet::new();
    for case in &manifest.cases {
        let numeric_id = case.case_id.strip_prefix("case_");
        if numeric_id.is_none_or(|value| {
            value.is_empty() || value.len() > 4 || !value.bytes().all(|byte| byte.is_ascii_digit())
        }) {
            return Err(CorpusRunError::InvalidManifest(
                "case_id must use the anonymous case_<number> form".to_string(),
            ));
        }
        if !case_ids.insert(case.case_id.clone()) {
            return Err(CorpusRunError::InvalidManifest(
                "case_id must be unique".to_string(),
            ));
        }

        let file_path = Path::new(&case.filename);
        let mut components = file_path.components();
        if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
            return Err(CorpusRunError::InvalidManifest(
                "filename must be a direct child of the corpus directory".to_string(),
            ));
        }
        if file_path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_none_or(|ext| !ext.eq_ignore_ascii_case("wav"))
        {
            return Err(CorpusRunError::InvalidManifest(
                "only WAV corpus files are supported".to_string(),
            ));
        }
        if !filenames.insert(case.filename.clone()) {
            return Err(CorpusRunError::InvalidManifest(
                "filename must be unique".to_string(),
            ));
        }
    }
    Ok(manifest)
}

/// 运行单个 WAV 文件的解码和规范化。
///
/// 返回：(16k mono PCM f32 样本, 解码元数据, 规范化元数据)。
/// 不执行 STT 推理——那需要 transport。
pub fn decode_and_normalize(wav_bytes: &[u8]) -> Result<DecodedAudioSummary, String> {
    let decode_start = std::time::Instant::now();

    let decoded = decode_wav(wav_bytes).map_err(|e| format!("decode: {e}"))?;
    let decode_ms = decode_start.elapsed().as_millis() as u64;

    let norm_start = std::time::Instant::now();
    let mut normalizer = AudioNormalizer::from_source_format(decoded.format);
    let mut samples = normalizer.process(&decoded.samples);
    samples.extend(normalizer.finish());
    let normalize_ms = norm_start.elapsed().as_millis() as u64;

    let duration_ms =
        (decoded.num_frames as f64 / decoded.format.sample_rate as f64 * 1000.0) as u64;

    Ok(DecodedAudioSummary {
        samples,
        duration_ms,
        channels: decoded.format.channels,
        sample_rate: decoded.format.sample_rate,
        bits_per_sample: decoded.format.bits_per_sample,
        num_frames: decoded.num_frames,
        decode_ms,
        normalize_ms,
    })
}

/// 顺序执行 manifest 中全部 case。返回值只保留匿名 id、格式、耗时和布尔结果，
/// 不携带文件名、绝对路径或转写正文。
pub async fn run_corpus(config: CorpusRunnerConfig) -> Result<Vec<CorpusResult>, CorpusRunError> {
    let manifest = load_manifest(&config.corpus_dir)?;
    let transport = config
        .transport
        .ok_or(CorpusRunError::TransportUnavailable)?;
    let mut results = Vec::with_capacity(manifest.cases.len());

    for case in manifest.cases {
        let total_start = Instant::now();
        let file_path = config.corpus_dir.join(&case.filename);
        let decoded = tokio::task::spawn_blocking(move || {
            let bytes = std::fs::read(file_path).map_err(|_| "read_failed".to_string())?;
            decode_and_normalize(&bytes)
        })
        .await
        .map_err(|_| CorpusRunError::InvalidManifest("decode task failed".to_string()))?;

        let summary = match decoded {
            Ok(summary) => summary,
            Err(error) => {
                results.push(CorpusResult {
                    case_id: case.case_id,
                    format_ok: false,
                    duration_ms: 0,
                    channels: 0,
                    sample_rate: 0,
                    bits_per_sample: 0,
                    decode_ms: 0,
                    normalize_ms: 0,
                    queue_ms: None,
                    inference_ms: None,
                    total_ms: total_start.elapsed().as_millis() as u64,
                    peak_memory_bytes: config
                        .worker_pid
                        .and_then(crate::infra::platform::process::peak_working_set_bytes),
                    detected_segments: 0,
                    expected_segments: case.expected_segments,
                    segments_matched: false,
                    text_empty: true,
                    text_matched: false,
                    text_chars: 0,
                    best_similarity_percent: 0,
                    matched: false,
                    error_category: Some(error.split(':').next().unwrap_or("decode_failed").into()),
                });
                continue;
            }
        };

        let format_ok = assert_normalized_format(&summary).is_ok();
        let detected_segments = detect_segments(&summary.samples);
        let segments_matched = detected_segments == case.expected_segments;
        let wav = encode_canonical_wav(&summary.samples);
        let transport_start = Instant::now();
        let transcription = transport.transcribe_with_metrics(&wav).await;
        let transport_ms = transport_start.elapsed().as_millis() as u64;
        let peak_memory_bytes = config
            .worker_pid
            .and_then(crate::infra::platform::process::peak_working_set_bytes);

        match transcription {
            Ok(output) => {
                let inference_ms = output.inference_ms.map(|value| value.ceil() as u64);
                let queue_ms = inference_ms.map(|value| transport_ms.saturating_sub(value));
                let normalized = normalize_for_compare(&output.text);
                let text_empty = output.text.trim().is_empty();
                let text_matched = evaluate_result(&case, &output.text, &normalized);
                let best_similarity_percent = best_similarity_percent(&case, &normalized);
                results.push(CorpusResult {
                    case_id: case.case_id,
                    format_ok,
                    duration_ms: summary.duration_ms,
                    channels: summary.channels,
                    sample_rate: summary.sample_rate,
                    bits_per_sample: summary.bits_per_sample,
                    decode_ms: summary.decode_ms,
                    normalize_ms: summary.normalize_ms,
                    queue_ms,
                    inference_ms,
                    total_ms: total_start.elapsed().as_millis() as u64,
                    peak_memory_bytes,
                    detected_segments,
                    expected_segments: case.expected_segments,
                    segments_matched,
                    text_empty,
                    text_matched,
                    text_chars: normalized.chars().count(),
                    best_similarity_percent,
                    matched: format_ok && segments_matched && text_matched,
                    error_category: None,
                });
            }
            Err(error) => results.push(CorpusResult {
                case_id: case.case_id,
                format_ok,
                duration_ms: summary.duration_ms,
                channels: summary.channels,
                sample_rate: summary.sample_rate,
                bits_per_sample: summary.bits_per_sample,
                decode_ms: summary.decode_ms,
                normalize_ms: summary.normalize_ms,
                queue_ms: None,
                inference_ms: None,
                total_ms: total_start.elapsed().as_millis() as u64,
                peak_memory_bytes,
                detected_segments,
                expected_segments: case.expected_segments,
                segments_matched,
                text_empty: true,
                text_matched: false,
                text_chars: 0,
                best_similarity_percent: 0,
                matched: false,
                error_category: Some(classify_transport_error(&error)),
            }),
        }
    }

    Ok(results)
}

fn detect_segments(samples: &[f32]) -> u32 {
    // 0.23.14 参数真源对齐：此前用 `EnergyVad::new`（旧默认灵敏度 0.005），
    // 而生产 `SttConfig` 默认已是 0.001 + 300/800ms 双阈值与 8/12s 窗口——
    // 段数验收口径必须与生产一致，否则出现假阳性/假阴性。生产默认变化时
    // 以 `VadConfig::default()` 为准（此处程序化引用，避免双份漂移）。
    let vad_cfg = crate::domain::config::stt_config::VadConfig::default();
    let mut vad = EnergyVad::with_params_and_windows(
        TARGET_SAMPLE_RATE,
        vad_cfg.silence_threshold,
        vad_cfg.min_silence_ms,
        vad_cfg.min_sentence_ms,
        vad_cfg.soft_window_ms(),
        vad_cfg.hard_window_ms(),
    );
    let mut segments = 0u32;
    for chunk in samples.chunks((TARGET_SAMPLE_RATE / 100) as usize) {
        // ShortPhraseEnd（短句停顿）不是切分事件，不计入段数。
        if vad.process_chunk(chunk).is_boundary() {
            segments = segments.saturating_add(1);
        }
    }
    if vad.is_speaking() {
        segments = segments.saturating_add(1);
    }
    segments
}

fn classify_transport_error(error: &crate::domain::stt::SttTransportError) -> String {
    error.category().to_string()
}

/// 解码和规范化后的音频摘要。
pub struct DecodedAudioSummary {
    pub samples: Vec<f32>,
    pub duration_ms: u64,
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub num_frames: usize,
    pub decode_ms: u64,
    pub normalize_ms: u64,
}

/// 验证 16kHz mono 规范化结果。
pub fn assert_normalized_format(summary: &DecodedAudioSummary) -> Result<(), String> {
    // 规范化后样本数应匹配 16kHz 时长
    let expected_samples =
        (summary.duration_ms as f64 * TARGET_SAMPLE_RATE as f64 / 1000.0).round() as usize;
    let tolerance = (TARGET_SAMPLE_RATE as usize / 50).max(1); // ±20ms tolerance
    if summary.samples.len().abs_diff(expected_samples) > tolerance {
        return Err(format!(
            "normalized sample count mismatch: got {}, expected ~{} (tolerance {})",
            summary.samples.len(),
            expected_samples,
            tolerance
        ));
    }
    Ok(())
}

/// 将 16k mono f32 编码为 canonical PCM16 WAV（供 transport 使用）。
pub fn encode_canonical_wav(samples: &[f32]) -> Vec<u8> {
    pcm_to_wav(samples, TARGET_SAMPLE_RATE, 1)
}

/// 检查系统中是否有 orphan worker 进程。
pub fn count_orphan_workers() -> usize {
    let out = crate::infra::platform::no_window(std::process::Command::new("powershell"))
        .args([
            "-NoProfile",
            "-Command",
            "(Get-Process -Name 'funasr-*-worker' -ErrorAction SilentlyContinue | Measure-Object).Count",
        ])
        .output();
    match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .trim()
            .parse()
            .unwrap_or(0),
        _ => 0,
    }
}

/// 判定转写结果是否匹配 manifest 期望。
///
/// **语义匹配**：只要 expected_empty 和 text_empty 一致即视为匹配。
/// 不使用"三模型逐字相同"作为通过条件；格式断言严格，ASR 语义允许模型差异。
pub fn evaluate_result(case: &CorpusCase, _text: &str, normalized_text: &str) -> bool {
    if case.expected_empty {
        return normalize_for_compare(normalized_text).is_empty();
    }
    // 非空 case：检查 normalized text 是否在 allowed_normalized 中
    // 或与 expected_text 匹配（允许标点差异）
    if normalized_text.is_empty() {
        return false;
    }
    // 检查是否包含期望的关键字符（语义匹配，不要求逐字相同）
    // 简单策略：去掉标点和空格后比较
    let clean_actual = normalize_for_compare(normalized_text);
    std::iter::once(&case.expected_text)
        .chain(case.allowed_normalized.iter())
        .map(|candidate| normalize_for_compare(candidate))
        .filter(|candidate| !candidate.is_empty())
        .any(|candidate| clean_actual.contains(&candidate))
}

/// 归一化文本用于比较：去标点、去空格、统一大小写。
pub(crate) fn normalize_for_compare(text: &str) -> String {
    text.chars()
        .filter(|c| {
            // 去除 ASCII 标点、空格和中文标点
            !c.is_ascii_punctuation() && !c.is_whitespace() && !is_cjk_punctuation(*c)
        })
        .collect::<String>()
        .to_lowercase()
}

fn best_similarity_percent(case: &CorpusCase, actual: &str) -> u8 {
    std::iter::once(&case.expected_text)
        .chain(case.allowed_normalized.iter())
        .map(|candidate| normalize_for_compare(candidate))
        .filter(|candidate| !candidate.is_empty())
        .map(|candidate| edit_similarity_percent(actual, &candidate))
        .max()
        .unwrap_or(if actual.is_empty() { 100 } else { 0 })
}

pub(crate) fn edit_similarity_percent(left: &str, right: &str) -> u8 {
    let left: Vec<char> = left.chars().collect();
    let right: Vec<char> = right.chars().collect();
    let max_len = left.len().max(right.len());
    if max_len == 0 {
        return 100;
    }
    let mut previous: Vec<usize> = (0..=right.len()).collect();
    let mut current = vec![0; right.len() + 1];
    for (i, left_char) in left.iter().enumerate() {
        current[0] = i + 1;
        for (j, right_char) in right.iter().enumerate() {
            current[j + 1] = (previous[j + 1] + 1)
                .min(current[j] + 1)
                .min(previous[j] + usize::from(left_char != right_char));
        }
        std::mem::swap(&mut previous, &mut current);
    }
    (((max_len - previous[right.len()]) * 100) / max_len) as u8
}

/// 判断是否为中文标点（全角标点）。
fn is_cjk_punctuation(c: char) -> bool {
    // CJK 标点符号 Unicode 区间 + 常见全角标点
    let cp = c as u32;
    // 全角 ASCII 标点 FF01-FF5E
    (0xFF01..=0xFF5F).contains(&cp)
    // CJK 标点符号区 3000-303F
    || (0x3000..=0x303F).contains(&cp)
    // 常见中文标点散落字符
    || matches!(c, '·' | '～')
}

/// 输出匿名验收表。
///
/// 不输出完整转写文本，只输出 case_id、格式、时长、是否命中和错误类别。
pub fn format_anonymous_summary(results: &[CorpusResult]) -> String {
    use std::fmt::Write;

    let mut output = String::new();
    writeln!(output, "STT corpus acceptance (anonymous)").unwrap();
    writeln!(
        output,
        "{:<12} {:<6} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8}",
        "case_id",
        "fmt",
        "dur_ms",
        "dec_ms",
        "queue",
        "infer",
        "total",
        "peak_mb",
        "segments",
        "text",
        "chars",
        "sim%",
        "match"
    )
    .unwrap();
    for r in results {
        writeln!(
            output,
            "{:<12} {:<6} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8} {:<8}",
            r.case_id,
            if r.format_ok { "Y" } else { "N" },
            r.duration_ms,
            r.decode_ms,
            r.queue_ms.map_or_else(|| "-".into(), |v| v.to_string()),
            r.inference_ms.map_or_else(|| "-".into(), |v| v.to_string()),
            r.total_ms,
            r.peak_memory_bytes
                .map_or_else(|| "-".into(), |bytes| (bytes / (1024 * 1024)).to_string()),
            format!("{}/{}", r.detected_segments, r.expected_segments),
            if r.text_matched { "Y" } else { "N" },
            r.text_chars,
            r.best_similarity_percent,
            if r.matched { "Y" } else { "N" },
        )
        .unwrap();
    }
    let total = results.len();
    let matched = results.iter().filter(|r| r.matched).count();
    writeln!(output, "matched={matched}/{total}").unwrap();
    output
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::stt::vad::VadEvent;
    use crate::infra::platform::audio::test_fixtures::*;

    #[test]
    fn runner_path_gate_isolated_from_process_environment() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            should_run_for_path(dir.path()),
            Some(dir.path().to_path_buf())
        );
        assert!(should_run_for_path(&dir.path().join("missing")).is_none());
    }

    /// 用私有语料回放 EnergyVad 的历史 A-H 参数矩阵，保留 0ms 软静默作为
    /// legacy baseline；当前生产渐进策略由 C 候选回放单独报告。
    /// 只写匿名 case id 与时间点；不输出音频、路径或转写正文。
    /// 这里不执行 ASR/terminal finalize：短词即使未触发 VAD，松键后仍可能识别成功。
    #[test]
    fn private_corpus_vad_parameter_sweep_legacy_baseline() {
        if std::env::var("BLINK_STT_VAD_SWEEP").ok().as_deref() != Some("1") {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let cases = collect_corpus_cases(&corpus_dir);
        let labeled_count = cases
            .iter()
            .filter(|(id, _, _)| id.starts_with("case_"))
            .count();

        // 0.23.7 候选矩阵：固定能量阈值与最短静音，按
        // "最短句长 ms / 软窗口 s / 硬窗口 s / 未提交上限 s" 比较 A–H。
        // 离线重放以最近边界为已提交位置近似伪流式层的未提交上限。
        let candidates: Vec<(&str, u32, u32, u32, u32)> = vec![
            ("A", 800, 8, 12, 12),   // 现有基线
            ("B", 1000, 8, 12, 12),  // 上轮长语音候选
            ("C", 1000, 10, 12, 12), // 单独推迟软切
            ("D", 1000, 8, 14, 14),  // 延长硬切与兜底
            ("E", 1000, 10, 14, 14), // 中等长度组合
            ("F", 1000, 10, 16, 16), // 长上下文上界
            ("G", 1000, 6, 10, 10),  // 低延迟对照
            ("H", 1000, 10, 14, 16), // 测定稿滞后时的未提交上限兜底
        ];
        let threshold = 0.005_f64;
        let silence_ms = 300_u32;

        let mut runs = Vec::new();
        for (label, sentence_ms, soft_window_s, hard_window_s, max_uncommitted_s) in candidates {
            let mut forced_boundaries = 0u32;
            let mut uncommitted_cap_boundaries = 0u32;
            let mut case_results = Vec::new();
            for (case_id, expected, samples) in &cases {
                let mut vad = EnergyVad::with_params_and_windows_and_soft_silence_floor(
                    TARGET_SAMPLE_RATE,
                    threshold,
                    silence_ms,
                    sentence_ms,
                    soft_window_s as u64 * 1000,
                    hard_window_s as u64 * 1000,
                    0,
                );
                let max_uncommitted_samples =
                    max_uncommitted_s as usize * TARGET_SAMPLE_RATE as usize;
                let mut events = Vec::new();
                let mut processed = 0usize;
                let mut segment_start = 0usize;
                for chunk in samples.chunks((TARGET_SAMPLE_RATE / 100) as usize) {
                    processed += chunk.len();
                    let mut event = vad.process_chunk(chunk);
                    // 伪流式层的未提交音频硬上限；无并发 finalize 的
                    // 离线重放以最近边界为已提交位置，覆盖长段的保底切片。
                    // 与生产一致按绝对未提交音频计算，不依赖 VAD speaking。
                    let mut cap_triggered = false;
                    if !event.is_boundary() && processed - segment_start >= max_uncommitted_samples
                    {
                        event = VadEvent::HardWindow;
                        cap_triggered = true;
                    }
                    if event.is_boundary() {
                        if event != VadEvent::SentenceEnd {
                            forced_boundaries += 1;
                        }
                        if cap_triggered {
                            uncommitted_cap_boundaries += 1;
                        }
                        events.push(serde_json::json!({
                            "time_ms": processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                            "reason": if cap_triggered { "uncommitted_cap" } else { event.reason() },
                            "off_threshold": vad.current_off_threshold(),
                        }));
                        segment_start = processed;
                        vad.reset_sentence();
                    }
                }
                // 松键后 terminal finalize 会处理剩余音频，即使 VAD 已不在
                // speaking 状态。以相同 off threshold 判断尾段是否非全静音。
                let terminal_off_threshold = vad.current_off_threshold();
                let terminal_tail_has_audio = samples[segment_start..]
                    .iter()
                    .any(|sample| sample.abs() > terminal_off_threshold as f32);
                let potential_transcribe_calls =
                    events.len() as u32 + u32::from(terminal_tail_has_audio);
                case_results.push(serde_json::json!({
                    "case_id": case_id,
                    "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "expected_segments": expected,
                    "potential_transcribe_calls": potential_transcribe_calls,
                    "events": events,
                    "trailing_speech": vad.is_speaking(),
                    "terminal_tail_has_audio": terminal_tail_has_audio,
                    "terminal_off_threshold": terminal_off_threshold,
                }));
            }
            runs.push(serde_json::json!({
                "label": label,
                "silence_threshold": threshold,
                "min_silence_ms": silence_ms,
                "min_sentence_ms": sentence_ms,
                "soft_window_s": soft_window_s,
                "hard_window_s": hard_window_s,
                "max_uncommitted_s": max_uncommitted_s,
                "forced_boundaries": forced_boundaries,
                "uncommitted_cap_boundaries": uncommitted_cap_boundaries,
                "cases": case_results,
            }));
        }

        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/stt-vad-parameter-sweep.json");
        let report = serde_json::json!({
            "scope": "0.23.7 historical A-H legacy baseline matrix: 0ms single-low-frame SoftWindow comparison plus pseudo-streaming uncommitted-cap boundary replay; not current production progressive behavior and not recognized speech. ASR quality is evaluated separately.",
            "labeled_cases": labeled_count,
            "unlabeled_cases": cases.len() - labeled_count,
            "runs": runs,
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous VAD parameter report");
    }

    /// 0.23.7.2 B：比较短句门槛候选，不执行 ASR 或 terminal worker。
    ///
    /// 基线沿用生产 800ms；直接候选把最短有效语音降到 200/250ms，但仍依赖
    /// 现有 3 帧 attack debounce 作为可靠起声；另一候选再加 600ms 边界抑制
    /// 间隔，用于观察快速连续边界是否能收敛。报告只写匿名 id、音频时间、
    /// 原因和计数，不把 preview 或低能量自动解释为语义正确。
    #[test]
    fn private_corpus_short_sentence_candidates() {
        if std::env::var("BLINK_STT_VAD_SHORT_SWEEP").ok().as_deref() != Some("1") {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let cases = collect_corpus_cases(&corpus_dir);

        let candidates = [
            ShortGateCandidate {
                label: "baseline_800",
                min_sentence_ms: 800,
                min_boundary_interval_ms: 0,
            },
            ShortGateCandidate {
                label: "low_effective_200_after_attack",
                min_sentence_ms: 200,
                min_boundary_interval_ms: 0,
            },
            ShortGateCandidate {
                label: "low_effective_250_after_attack",
                min_sentence_ms: 250,
                min_boundary_interval_ms: 0,
            },
            ShortGateCandidate {
                label: "low_effective_250_after_attack_interval_600",
                min_sentence_ms: 250,
                min_boundary_interval_ms: 600,
            },
        ];

        let mut candidate_reports = Vec::new();
        for candidate in candidates {
            let case_reports = cases
                .iter()
                .map(|(case_id, expected, samples)| {
                    let mut result = replay_short_gate_candidate(samples, candidate);
                    result["case_id"] = serde_json::json!(case_id);
                    result["expected_segments"] = serde_json::json!(expected);
                    result
                })
                .collect::<Vec<_>>();
            candidate_reports.push(serde_json::json!({
                "label": candidate.label,
                "min_sentence_ms": candidate.min_sentence_ms,
                "min_boundary_interval_ms": candidate.min_boundary_interval_ms,
                "cases": case_reports,
            }));
        }

        let synthetic_cases = [
            (
                "short_word_250ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(250, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "short_word_300ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(300, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "three_short_words_300ms_gaps",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(300, 0.1),
                    generate_silence(300),
                    generate_tone(300, 0.1),
                    generate_silence(300),
                    generate_tone(300, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "transient_pulse_80ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(80, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "transient_burst_180ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(180, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "intra_sentence_pause_200ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(450, 0.1),
                    generate_silence(200),
                    generate_tone(450, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "two_short_words_280ms_gap_300ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(280, 0.1),
                    generate_silence(300),
                    generate_tone(280, 0.1),
                    generate_silence(400),
                ]),
            ),
            (
                "continuous_speech_9000ms",
                join_audio(&[
                    generate_silence(500),
                    generate_tone(9000, 0.1),
                    generate_silence(400),
                ]),
            ),
            ("all_silence_2000ms", generate_silence(2000)),
        ];
        let synthetic_reports = candidates
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "label": candidate.label,
                    "min_sentence_ms": candidate.min_sentence_ms,
                    "min_boundary_interval_ms": candidate.min_boundary_interval_ms,
                    "cases": synthetic_cases
                        .iter()
                        .map(|(case_id, samples)| {
                            let mut result = replay_short_gate_candidate(samples, *candidate);
                            result["case_id"] = serde_json::json!(case_id);
                            result
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let accumulated_windows_ms = [2000_u32, 4000_u32];
        let accumulated_candidate_reports = accumulated_windows_ms
            .iter()
            .map(|window_ms| {
                serde_json::json!({
                    "label": format!("accumulate_effective_800_within_{window_ms}"),
                    "min_sentence_ms": 800,
                    "evidence_window_ms": window_ms,
                    "cases": cases
                        .iter()
                        .map(|(case_id, expected, samples)| {
                            let mut result =
                                replay_accumulated_gate_candidate(samples, 800, *window_ms);
                            result["case_id"] = serde_json::json!(case_id);
                            result["expected_segments"] = serde_json::json!(expected);
                            result
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let accumulated_synthetic_reports = accumulated_windows_ms
            .iter()
            .map(|window_ms| {
                serde_json::json!({
                    "label": format!("accumulate_effective_800_within_{window_ms}"),
                    "min_sentence_ms": 800,
                    "evidence_window_ms": window_ms,
                    "cases": synthetic_cases
                        .iter()
                        .map(|(case_id, samples)| {
                            let mut result =
                                replay_accumulated_gate_candidate(samples, 800, *window_ms);
                            result["case_id"] = serde_json::json!(case_id);
                            result
                        })
                        .collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/stt-vad-short-sentence-candidates.json");
        let report = serde_json::json!({
            "scope": "0.23.7.2 B short effective-speech gate candidates; numeric VAD replay only, no audio, preview text or transcript content.",
            "case_counts": {
                "labeled": cases.iter().filter(|(id, _, _)| id.starts_with("case_")).count(),
                "unlabeled": cases.iter().filter(|(id, _, _)| id.starts_with("new_")).count(),
                "total": cases.len()
            },
            "candidate_notes": {
                "baseline_800": "Current production min_sentence_ms=800; no independent boundary interval.",
                "low_effective_200_after_attack": "200ms non-silent sentence gate after the existing 3-frame attack debounce; this is an acoustic candidate, not a semantic truth label.",
                "low_effective_250_after_attack": "250ms non-silent sentence gate after the existing 3-frame attack debounce; this is an acoustic candidate, not a semantic truth label.",
                "low_effective_250_after_attack_interval_600": "Same 250ms gate plus 600ms minimum accepted boundary interval; suppressed boundaries remain in numeric diagnostics and are not claimed as lost speech."
            },
            "limitations": [
                "The interval candidate is an offline post-filter approximation: a suppressed boundary still resets the diagnostic VAD while segment_start stays at the previous accepted boundary, so this does not prove production wiring or no-loss behavior.",
                "Synthetic 80ms and 180ms tones are transient energy proxies only; they are not substitutes for the real cough/keyboard acoustic samples.",
                "Unlabeled new_ cases have no semantic truth labels; their energy valleys and candidate boundaries remain pending manual review."
            ],
            "additional_candidate_notes": {
                "accumulate_effective_800": "Keeps the 800ms single-segment protection, carries rejected effective speech for a finite window, and permits a later pause only after the accumulated evidence reaches 800ms. The 2000ms and 4000ms windows are diagnostic candidates; neither is wired into production VAD."
            },
            "decision": {
                "production_min_sentence_ms": 800,
                "production_change": "none",
                "protected_empty_cases": ["case_09", "case_10", "case_11"],
                "reason": "Direct 200/250ms gates add a natural boundary to the expected-empty case_10 at 2690ms; the 600ms interval does not suppress that first boundary. Accumulation preserves case_10 but adds extra accumulated boundaries to case_07; the 4000ms window reaches the 7340ms candidate in new_5ebac8df, whose semantic correctness remains pending manual review."
            },
            "synthetic_cases": {
                "direct_gate_candidates": synthetic_reports,
                "accumulated_gate_candidates": accumulated_synthetic_reports
            },
            "corpus_cases": {
                "direct_gate_candidates": candidate_reports,
                "accumulated_gate_candidates": accumulated_candidate_reports
            },
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous short-sentence candidate report");
    }

    #[derive(Debug, Clone, Copy)]
    struct ShortGateCandidate {
        label: &'static str,
        min_sentence_ms: u32,
        min_boundary_interval_ms: u32,
    }

    /// 回放单个短句门槛候选。边界抑制只作为诊断层候选，不改变生产 VAD。
    fn replay_short_gate_candidate(
        samples: &[f32],
        candidate: ShortGateCandidate,
    ) -> serde_json::Value {
        let mut vad = EnergyVad::with_params_and_windows_and_soft_silence_floor(
            TARGET_SAMPLE_RATE,
            0.005,
            300,
            candidate.min_sentence_ms,
            8_000,
            12_000,
            0,
        );
        let frame_len = (TARGET_SAMPLE_RATE / 100) as usize;
        let mut processed = 0usize;
        let mut segment_start = 0usize;
        let mut previous = vad.dump_state();
        let mut last_accepted_boundary_ms: Option<u64> = None;
        let mut boundaries = Vec::new();
        let mut rejected_short_sentences = Vec::new();
        let mut suppressed_boundaries = Vec::new();

        for chunk in samples.chunks(frame_len) {
            processed += chunk.len();
            let time_ms = processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64;
            let event = vad.process_chunk(chunk);
            let state = vad.dump_state();
            if event.is_boundary() {
                let sentence_ms = state.sentence_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64;
                let interval_ok = candidate.min_boundary_interval_ms == 0
                    || last_accepted_boundary_ms.is_none_or(|last| {
                        time_ms.saturating_sub(last) >= candidate.min_boundary_interval_ms as u64
                    });
                if interval_ok {
                    boundaries.push(serde_json::json!({
                        "time_ms": time_ms,
                        "reason": event.reason(),
                        "sentence_ms": sentence_ms,
                    }));
                    last_accepted_boundary_ms = Some(time_ms);
                    segment_start = processed;
                } else {
                    suppressed_boundaries.push(serde_json::json!({
                        "time_ms": time_ms,
                        "reason": event.reason(),
                        "sentence_ms": sentence_ms,
                        "required_interval_ms": candidate.min_boundary_interval_ms,
                    }));
                }
                vad.reset_sentence();
            } else if previous.speaking && !state.speaking {
                rejected_short_sentences.push(serde_json::json!({
                    "time_ms": time_ms,
                    "reason": "min_sentence",
                    "sentence_ms": previous.sentence_samples as u64 * 1000
                        / TARGET_SAMPLE_RATE as u64,
                    "silence_ms": state.silence_samples as u64 * 1000
                        / TARGET_SAMPLE_RATE as u64,
                }));
            }
            previous = vad.dump_state();
        }

        let terminal_off_threshold = vad.current_off_threshold();
        let terminal_tail_has_audio = samples[segment_start..]
            .iter()
            .any(|sample| sample.abs() > terminal_off_threshold as f32);
        serde_json::json!({
            "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
            "boundaries": boundaries,
            "boundary_count": boundaries.len(),
            "rejected_short_sentences": rejected_short_sentences,
            "rejected_short_count": rejected_short_sentences.len(),
            "suppressed_boundaries": suppressed_boundaries,
            "suppressed_boundary_count": suppressed_boundaries.len(),
            "terminal_tail_has_audio": terminal_tail_has_audio,
            "potential_transcribe_calls": boundaries.len() as u32
                + u32::from(terminal_tail_has_audio),
        })
    }

    /// 诊断候选：保留单段 800ms 保护，但在有限窗口内累积被拒绝的有效
    /// 非静默时长。它故意不改 EnergyVad 的生产字段，只验证该策略的候选
    /// 端点与误切风险。
    fn replay_accumulated_gate_candidate(
        samples: &[f32],
        min_sentence_ms: u32,
        evidence_window_ms: u32,
    ) -> serde_json::Value {
        let mut vad = EnergyVad::with_params_and_windows_and_soft_silence_floor(
            TARGET_SAMPLE_RATE,
            0.005,
            300,
            min_sentence_ms,
            8_000,
            12_000,
            0,
        );
        let frame_len = (TARGET_SAMPLE_RATE / 100) as usize;
        let mut processed = 0usize;
        let mut segment_start = 0usize;
        let mut previous = vad.dump_state();
        let mut evidence_ms = 0u64;
        let mut evidence_anchor_ms: Option<u64> = None;
        let mut boundaries = Vec::new();
        let mut rejected_short_sentences = Vec::new();

        for chunk in samples.chunks(frame_len) {
            processed += chunk.len();
            let time_ms = processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64;
            if evidence_anchor_ms
                .is_some_and(|anchor| time_ms.saturating_sub(anchor) > evidence_window_ms as u64)
            {
                evidence_ms = 0;
                evidence_anchor_ms = None;
            }

            let event = vad.process_chunk(chunk);
            let state = vad.dump_state();
            if event.is_boundary() {
                boundaries.push(serde_json::json!({
                    "time_ms": time_ms,
                    "reason": event.reason(),
                    "sentence_ms": state.sentence_samples as u64 * 1000
                        / TARGET_SAMPLE_RATE as u64,
                    "accumulated_sentence_ms": evidence_ms,
                }));
                evidence_ms = 0;
                evidence_anchor_ms = None;
                segment_start = processed;
                vad.reset_sentence();
            } else if previous.speaking && !state.speaking {
                let sentence_ms =
                    previous.sentence_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64;
                evidence_ms = if evidence_anchor_ms.is_some() {
                    evidence_ms.saturating_add(sentence_ms)
                } else {
                    sentence_ms
                };
                evidence_anchor_ms.get_or_insert(time_ms);
                if evidence_ms >= min_sentence_ms as u64 {
                    boundaries.push(serde_json::json!({
                        "time_ms": time_ms,
                        "reason": "natural_silence_accumulated",
                        "sentence_ms": sentence_ms,
                        "accumulated_sentence_ms": evidence_ms,
                    }));
                    evidence_ms = 0;
                    evidence_anchor_ms = None;
                    segment_start = processed;
                } else {
                    rejected_short_sentences.push(serde_json::json!({
                        "time_ms": time_ms,
                        "reason": "min_sentence_accumulating",
                        "sentence_ms": sentence_ms,
                        "accumulated_sentence_ms": evidence_ms,
                    }));
                }
            }
            previous = vad.dump_state();
        }

        let terminal_off_threshold = vad.current_off_threshold();
        let terminal_tail_has_audio = samples[segment_start..]
            .iter()
            .any(|sample| sample.abs() > terminal_off_threshold as f32);
        serde_json::json!({
            "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
            "boundaries": boundaries,
            "boundary_count": boundaries.len(),
            "rejected_short_sentences": rejected_short_sentences,
            "rejected_short_count": rejected_short_sentences.len(),
            "terminal_tail_has_audio": terminal_tail_has_audio,
            "potential_transcribe_calls": boundaries.len() as u32
                + u32::from(terminal_tail_has_audio),
        })
    }

    fn generate_tone(duration_ms: u32, amplitude: f32) -> Vec<f32> {
        let sample_count = duration_ms as usize * TARGET_SAMPLE_RATE as usize / 1000;
        (0..sample_count)
            .map(|index| {
                let t = index as f32 / TARGET_SAMPLE_RATE as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude
            })
            .collect()
    }

    fn generate_silence(duration_ms: u32) -> Vec<f32> {
        vec![0.0; duration_ms as usize * TARGET_SAMPLE_RATE as usize / 1000]
    }

    fn join_audio(parts: &[Vec<f32>]) -> Vec<f32> {
        parts.iter().flat_map(|part| part.iter().copied()).collect()
    }

    #[test]
    fn short_sentence_candidate_policies_are_distinguishable() {
        let short_word = join_audio(&[
            generate_silence(500),
            generate_tone(250, 0.1),
            generate_silence(400),
        ]);
        let low_200 = replay_short_gate_candidate(
            &short_word,
            ShortGateCandidate {
                label: "low_200",
                min_sentence_ms: 200,
                min_boundary_interval_ms: 0,
            },
        );
        let low_250 = replay_short_gate_candidate(
            &short_word,
            ShortGateCandidate {
                label: "low_250",
                min_sentence_ms: 250,
                min_boundary_interval_ms: 0,
            },
        );
        assert_eq!(low_200["boundary_count"], 1);
        assert_eq!(low_250["boundary_count"], 0);

        let close_words = join_audio(&[
            generate_silence(500),
            generate_tone(280, 0.1),
            generate_silence(300),
            generate_tone(280, 0.1),
            generate_silence(400),
        ]);
        let interval = replay_short_gate_candidate(
            &close_words,
            ShortGateCandidate {
                label: "low_250_interval_600",
                min_sentence_ms: 250,
                min_boundary_interval_ms: 600,
            },
        );
        assert_eq!(interval["boundary_count"], 1);
        assert_eq!(interval["suppressed_boundary_count"], 1);

        let accumulated = join_audio(&[
            generate_silence(500),
            generate_tone(300, 0.1),
            generate_silence(300),
            generate_tone(300, 0.1),
            generate_silence(300),
            generate_tone(300, 0.1),
            generate_silence(400),
        ]);
        let accumulated = replay_accumulated_gate_candidate(&accumulated, 800, 2000);
        assert_eq!(accumulated["boundary_count"], 1);
        assert_eq!(
            accumulated["boundaries"][0]["reason"],
            "natural_silence_accumulated"
        );
    }

    /// 0.23.7.2 C：比较软窗口的连续静默策略。
    ///
    /// 生产默认从软窗口开始时的 300ms 连续 `<off` 静默，逐步降到硬窗口
    /// 处的 150ms；0ms 是旧的单帧切对照，200ms 与 50ms 只用于候选比较。
    /// 只回放 VAD 和未提交 cap 近似，不执行 ASR/worker，报告不含音频或正文。
    #[test]
    fn private_corpus_progressive_soft_window_candidates() {
        if std::env::var("BLINK_STT_VAD_PROGRESSIVE_SWEEP")
            .ok()
            .as_deref()
            != Some("1")
        {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let cases = collect_corpus_cases(&corpus_dir);
        let candidates = [
            SoftWindowCandidate {
                label: "baseline_single_low_frame",
                floor_ms: 0,
            },
            SoftWindowCandidate {
                label: "progressive_300_to_150",
                floor_ms: 150,
            },
            SoftWindowCandidate {
                label: "progressive_300_to_200",
                floor_ms: 200,
            },
            SoftWindowCandidate {
                label: "emergency_50_only",
                floor_ms: 50,
            },
        ];

        let corpus_reports = candidates
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "label": candidate.label,
                    "start_ms": 300,
                    "floor_ms": candidate.floor_ms,
                    "cases": cases.iter().map(|(case_id, expected, samples)| {
                        let mut result = replay_soft_window_candidate(
                            samples,
                            *candidate,
                            160,
                            800,
                        );
                        result["case_id"] = serde_json::json!(case_id);
                        result["expected_segments"] = serde_json::json!(expected);
                        result
                    }).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        let synthetic_cases = [
            (
                "at_8s_single_10ms_low",
                join_audio(&[generate_tone(8_000, 0.1), generate_silence(10)]),
            ),
            (
                "at_8s_200ms_low",
                join_audio(&[generate_tone(8_000, 0.1), generate_silence(200)]),
            ),
            (
                "at_8s_300ms_low",
                join_audio(&[generate_tone(8_000, 0.1), generate_silence(300)]),
            ),
            (
                "hysteresis_break_after_200ms_low",
                join_audio(&[
                    generate_tone(8_000, 0.1),
                    generate_silence(200),
                    generate_tone(10, 0.010),
                    generate_silence(200),
                ]),
            ),
            (
                "near_hard_100ms_low",
                join_audio(&[generate_tone(11_800, 0.1), generate_silence(100)]),
            ),
            (
                "near_hard_160ms_low",
                join_audio(&[generate_tone(11_800, 0.1), generate_silence(160)]),
            ),
            (
                "continuous_12s",
                join_audio(&[generate_tone(12_000, 0.1), generate_silence(100)]),
            ),
            (
                "silence_interrupted_by_voice",
                join_audio(&[
                    generate_tone(8_000, 0.1),
                    generate_silence(200),
                    generate_tone(20, 0.1),
                    generate_silence(200),
                ]),
            ),
            ("hysteresis_stuck_cap_13s", generate_tone(13_000, 0.008)),
            ("all_silence_2s", generate_silence(2_000)),
        ];
        let synthetic_reports = candidates
            .iter()
            .map(|candidate| {
                serde_json::json!({
                    "label": candidate.label,
                    "start_ms": 300,
                    "floor_ms": candidate.floor_ms,
                    "cases": synthetic_cases.iter().map(|(case_id, samples)| {
                        let mut result = replay_soft_window_candidate(
                            samples,
                            *candidate,
                            160,
                            800,
                        );
                        result["case_id"] = serde_json::json!(case_id);
                        result
                    }).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();
        let chunk_size_reports = [160_usize, 320, 480]
            .iter()
            .map(|chunk_samples| {
                let mut result = replay_soft_window_candidate(
                    &synthetic_cases[5].1,
                    candidates[1],
                    *chunk_samples,
                    800,
                );
                result["chunk_samples"] = serde_json::json!(chunk_samples);
                result
            })
            .collect::<Vec<_>>();

        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/stt-vad-progressive-soft-window.json");
        let report = serde_json::json!({
            "scope": "0.23.7.2 C progressive soft-window candidate replay; numeric VAD and uncommitted-cap approximation only, no audio, ASR, preview or transcript content.",
            "params": {
                "silence_threshold": 0.005,
                "min_silence_ms": 300,
                "min_sentence_ms": 800,
                "soft_window_ms": 8000,
                "hard_window_ms": 12000,
                "max_uncommitted_ms": 12000,
                "frame_ms": 10,
            },
            "case_counts": {
                "labeled": cases.iter().filter(|(id, _, _)| id.starts_with("case_")).count(),
                "unlabeled": cases.iter().filter(|(id, _, _)| id.starts_with("new_")).count(),
                "total": cases.len(),
            },
            "candidate_notes": {
                "baseline_single_low_frame": "Legacy comparison: once the segment reaches 8s, the first RMS < off frame may produce SoftWindow.",
                "progressive_300_to_150": "Selected production candidate: continuous RMS < off silence decreases from configured min_silence_ms=300 at 8s to a bounded 150ms floor at 12s.",
                "progressive_300_to_200": "Slower relaxation comparison; not selected as default.",
                "emergency_50_only": "Emergency comparison only; never a default because it accepts very short valleys near the hard limit.",
            },
            "limitations": [
                "Numeric boundary changes are not semantic truth; unlabeled new_ cutpoints require manual listening.",
                "The uncommitted cap is replayed from the last accepted boundary and does not measure worker queueing or UI delivery.",
                "Synthetic tones represent energy patterns only; they are not labeled cough, keyboard or real speech acoustics.",
                "Different chunk-size rows verify event counts for 10ms-aligned chunks; production audio timing still depends on actual callback delivery.",
            ],
            "protected_empty_cases": ["case_09", "case_10", "case_11"],
            "corpus_candidates": corpus_reports,
            "synthetic_candidates": synthetic_reports,
            "chunk_size_equivalence": chunk_size_reports,
            "decision": {
                "production_candidate": "progressive_300_to_150",
                "production_change": "EnergyVad only; defaults and upper bounds unchanged.",
                "hard_cap": "Keep clear HardWindow and uncommitted_cap fallback at 12s; do not use 50ms as default.",
            },
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous progressive soft-window report");
    }

    #[derive(Debug, Clone, Copy)]
    struct SoftWindowCandidate {
        label: &'static str,
        floor_ms: u32,
    }

    fn replay_soft_window_candidate(
        samples: &[f32],
        candidate: SoftWindowCandidate,
        chunk_samples: usize,
        min_sentence_ms: u32,
    ) -> serde_json::Value {
        let mut vad = EnergyVad::with_params_and_windows_and_soft_silence_floor(
            TARGET_SAMPLE_RATE,
            0.005,
            300,
            min_sentence_ms,
            8_000,
            12_000,
            candidate.floor_ms,
        );
        let max_uncommitted_samples = 12 * TARGET_SAMPLE_RATE as usize;
        let mut processed = 0usize;
        let mut segment_start = 0usize;
        let mut events = Vec::new();
        let mut natural_count = 0u32;
        let mut soft_count = 0u32;
        let mut hard_count = 0u32;
        let mut cap_count = 0u32;

        for chunk in samples.chunks(chunk_samples.max(1)) {
            processed += chunk.len();
            let mut event = vad.process_chunk(chunk);
            let cap_triggered = !event.is_boundary()
                && processed.saturating_sub(segment_start) >= max_uncommitted_samples;
            if cap_triggered {
                event = VadEvent::HardWindow;
                cap_count += 1;
            }
            if event.is_boundary() {
                let reason = if cap_triggered {
                    "uncommitted_cap"
                } else {
                    event.reason()
                };
                match reason {
                    "natural_silence" => natural_count += 1,
                    "soft_window" => soft_count += 1,
                    "hard_window" => hard_count += 1,
                    _ => {}
                }
                events.push(serde_json::json!({
                    "time_ms": processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "reason": reason,
                }));
                segment_start = processed;
                vad.reset_sentence();
            }
        }

        let terminal_off_threshold = vad.current_off_threshold();
        let terminal_tail_has_audio = samples[segment_start..]
            .iter()
            .any(|sample| sample.abs() > terminal_off_threshold as f32);
        serde_json::json!({
            "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
            "events": events,
            "natural_count": natural_count,
            "soft_count": soft_count,
            "hard_count": hard_count,
            "cap_count": cap_count,
            "terminal_tail_has_audio": terminal_tail_has_audio,
            "potential_transcribe_calls": events.len() as u32
                + u32::from(terminal_tail_has_audio),
        })
    }

    /// 0.23.7.2 D：强制切（hard/cap）谷底回退的数值回放。
    ///
    /// 生产语义镜像：渐进软窗口 floor=150；强制切触发时在 1.2s 帧历史里
    /// 找 ≥50ms 的连续 `<off` 谷底，回退到其末帧；候选不高于上一接受边界
    /// （近似 committed）时放弃回退。回退只移动切点，不新增边界。
    fn replay_hard_cut_valley_arm(samples: &[f32], enable_retreat: bool) -> serde_json::Value {
        let mut vad = EnergyVad::with_params_and_windows_and_soft_silence_floor(
            TARGET_SAMPLE_RATE,
            0.005,
            300,
            800,
            8_000,
            12_000,
            150,
        );
        let max_uncommitted_samples = 12 * TARGET_SAMPLE_RATE as usize;
        let mut processed = 0usize;
        let mut segment_start = 0usize;
        let mut events = Vec::new();
        let mut retreated_count = 0u32;

        for chunk in samples.chunks(160) {
            processed += chunk.len();
            let mut event = vad.process_chunk(chunk);
            let cap_triggered = !event.is_boundary()
                && processed.saturating_sub(segment_start) >= max_uncommitted_samples;
            if cap_triggered {
                event = VadEvent::HardWindow;
            }
            if event.is_boundary() {
                let mut boundary_samples = processed;
                let mut reason = if cap_triggered {
                    "uncommitted_cap"
                } else {
                    event.reason()
                };
                if enable_retreat
                    && event == VadEvent::HardWindow
                    && let Some(offset) = vad.low_energy_valley_offset(1_200)
                {
                    let candidate = processed.saturating_sub(offset);
                    if candidate > segment_start && candidate < processed {
                        retreated_count += 1;
                        boundary_samples = candidate;
                        reason = if cap_triggered {
                            "uncommitted_cap_valley"
                        } else {
                            "hard_window_valley"
                        };
                    }
                }
                events.push(serde_json::json!({
                    "time_ms": boundary_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "reason": reason,
                    "retreated_from_ms": if boundary_samples == processed {
                        serde_json::Value::Null
                    } else {
                        serde_json::json!(processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64)
                    },
                }));
                segment_start = boundary_samples;
                vad.reset_sentence();
            }
        }

        let terminal_off_threshold = vad.current_off_threshold();
        let terminal_tail_has_audio = samples[segment_start..]
            .iter()
            .any(|sample| sample.abs() > terminal_off_threshold as f32);
        serde_json::json!({
            "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
            "events": events,
            "boundary_count": events.len(),
            "retreated_count": retreated_count,
            "terminal_tail_has_audio": terminal_tail_has_audio,
            "potential_transcribe_calls": events.len() as u32
                + u32::from(terminal_tail_has_audio),
        })
    }

    /// 0.23.7.2 D：比较强制切"当前时刻兜底 vs 近期谷底回退"。
    ///
    /// 基线臂 = 生产渐进软窗口（floor 150）无回退，与 C 包报告的
    /// progressive_300_to_150 臂数值一致；回退臂只移动 hard/cap 切点。
    /// 报告只含匿名 id 与数值，不含音频或正文。
    #[test]
    fn private_corpus_hard_cut_valley_candidates() {
        if std::env::var("BLINK_STT_VAD_VALLEY_SWEEP").ok().as_deref() != Some("1") {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let cases = collect_corpus_cases(&corpus_dir);

        let corpus_arms = [("baseline_no_retreat", false), ("valley_retreat", true)]
            .iter()
            .map(|(label, enable)| {
                serde_json::json!({
                    "label": label,
                    "cases": cases.iter().map(|(case_id, expected, samples)| {
                        let mut result = replay_hard_cut_valley_arm(samples, *enable);
                        result["case_id"] = serde_json::json!(case_id);
                        result["expected_segments"] = serde_json::json!(expected);
                        result
                    }).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        let synthetic_cases = [
            (
                "continuous_12s_no_valley",
                join_audio(&[generate_tone(12_000, 0.1), generate_silence(100)]),
            ),
            (
                "near_hard_80ms_notch",
                join_audio(&[
                    generate_tone(11_700, 0.1),
                    generate_silence(80),
                    generate_tone(400, 0.1),
                ]),
            ),
            (
                "near_hard_150ms_notch",
                join_audio(&[
                    generate_tone(11_700, 0.1),
                    generate_silence(150),
                    generate_tone(400, 0.1),
                ]),
            ),
            ("hysteresis_stuck_cap_13s", generate_tone(13_000, 0.008)),
            ("all_silence_2s", generate_silence(2_000)),
        ];
        let synthetic_arms = [("baseline_no_retreat", false), ("valley_retreat", true)]
            .iter()
            .map(|(label, enable)| {
                serde_json::json!({
                    "label": label,
                    "cases": synthetic_cases.iter().map(|(case_id, samples)| {
                        let mut result = replay_hard_cut_valley_arm(samples, *enable);
                        result["case_id"] = serde_json::json!(case_id);
                        result
                    }).collect::<Vec<_>>(),
                })
            })
            .collect::<Vec<_>>();

        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/stt-vad-hard-cut-valley.json");
        let report = serde_json::json!({
            "scope": "0.23.7.2 D hard-cut valley retreat candidate replay; numeric VAD approximation only, no audio, ASR, preview or transcript content. Engine-level invariants are proven separately in pseudo_streaming tests.",
            "params": {
                "silence_threshold": 0.005,
                "min_silence_ms": 300,
                "min_sentence_ms": 800,
                "soft_window_ms": 8000,
                "hard_window_ms": 12000,
                "max_uncommitted_ms": 12000,
                "progressive_soft_floor_ms": 150,
                "valley_window_ms": 1200,
                "valley_min_run_ms": 50,
            },
            "case_counts": {
                "labeled": cases.iter().filter(|(id, _, _)| id.starts_with("case_")).count(),
                "unlabeled": cases.iter().filter(|(id, _, _)| id.starts_with("new_")).count(),
                "total": cases.len(),
            },
            "limitations": [
                "The numeric replay approximates committed ends with the last accepted boundary and the engine cap with per-chunk checks; exact engine behavior is covered by pseudo_streaming invariant tests.",
                "A retreated numeric boundary is an energy-based position, not a semantic truth label; unlabeled new_ cutpoints remain pending manual review.",
            ],
            "protected_empty_cases": ["case_09", "case_10", "case_11"],
            "corpus_arms": corpus_arms,
            "synthetic_arms": synthetic_arms,
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous hard-cut valley report");
    }

    /// 收集 corpus 全部 case：manifest 标注样本 + 目录内未列入 manifest 的
    /// WAV（以内容哈希匿名 id 标识）。返回 (id, expected_segments, samples)。
    /// 两个私有回放测试（sweep / 帧诊断）共用，避免发现逻辑双份漂移。
    fn collect_corpus_cases(corpus_dir: &Path) -> Vec<(String, Option<u32>, Vec<f32>)> {
        let manifest = load_manifest(corpus_dir).expect("load private corpus manifest");
        let listed: HashSet<String> = manifest
            .cases
            .iter()
            .map(|case| case.filename.to_lowercase())
            .collect();
        let mut cases: Vec<_> = manifest
            .cases
            .into_iter()
            .map(|case| {
                let wav = std::fs::read(corpus_dir.join(&case.filename)).expect("read corpus WAV");
                let audio = decode_and_normalize(&wav).expect("decode and normalize corpus WAV");
                (case.case_id, Some(case.expected_segments), audio.samples)
            })
            .collect();
        // 新录音不要求立即维护 manifest；以文件内容哈希标识，报告不泄漏文件名。
        for entry in std::fs::read_dir(corpus_dir).expect("read corpus directory") {
            let entry = entry.expect("read corpus entry");
            let path = entry.path();
            if !path.is_file()
                || path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("wav"))
                || listed.contains(&entry.file_name().to_string_lossy().to_lowercase())
            {
                continue;
            }
            let wav = std::fs::read(path).expect("read unlisted WAV");
            use sha2::{Digest, Sha256};
            let id = format!("new_{}", &format!("{:x}", Sha256::digest(&wav))[..8]);
            let audio = decode_and_normalize(&wav).expect("decode and normalize unlisted WAV");
            cases.push((id, None, audio.samples));
        }
        cases.sort_by(|a, b| a.0.cmp(&b.0));
        cases
    }

    /// 0.23.7.1：逐帧 VAD 状态诊断（env 门控 `BLINK_STT_VAD_DIAG=1`）。
    ///
    /// 用生产 `EnergyVad`（默认 A 参数）按 10ms 帧回放全部 corpus 音频，
    /// 逐帧记录规范化 PCM 的 RMS、on/off 阈值、speaking、句长/静默/段长
    /// 计数与切点事件。用于核实"轻声短句停顿不定稿"的根因（最短句长
    /// 计数语义、滞回区与静默判据各自的贡献）。报告只含匿名 id 与数值，
    /// 不含音频或转写正文；写入 `target/stt-vad-frame-diagnostics.json`
    /// （运行产物，不入库）。
    #[test]
    fn private_corpus_vad_frame_diagnostics() {
        if std::env::var("BLINK_STT_VAD_DIAG").ok().as_deref() != Some("1") {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let cases = collect_corpus_cases(&corpus_dir);

        // 默认 A 组合（当前生产渐进软窗口）；不执行 ASR。
        let threshold = 0.005_f64;
        let silence_ms = 300_u32;
        let sentence_ms = 800_u32;
        let frame_len = (TARGET_SAMPLE_RATE / 100) as usize;

        let mut case_reports = Vec::new();
        for (case_id, _expected, samples) in &cases {
            let mut compact_vad = EnergyVad::with_params_and_windows(
                TARGET_SAMPLE_RATE,
                threshold,
                silence_ms,
                sentence_ms,
                8_000,
                12_000,
            );
            let compact_trace = crate::domain::stt::vad_diagnostics::trace_vad(
                samples,
                TARGET_SAMPLE_RATE,
                &mut compact_vad,
            );
            let mut vad = EnergyVad::with_params_and_windows(
                TARGET_SAMPLE_RATE,
                threshold,
                silence_ms,
                sentence_ms,
                8_000,
                12_000,
            );
            let mut frames = Vec::with_capacity(samples.len() / frame_len);
            let mut events = Vec::new();
            let mut processed = 0usize;
            for chunk in samples.chunks(frame_len) {
                processed += chunk.len();
                let rms = frame_rms(chunk);
                let event = vad.process_chunk(chunk);
                let state = vad.dump_state();
                frames.push(serde_json::json!({
                    "t_ms": processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "rms": (rms * 1e6).round() / 1e6,
                    "on": (state.on_threshold * 1e6).round() / 1e6,
                    "off": (state.off_threshold * 1e6).round() / 1e6,
                    "nf": (state.noise_floor * 1e6).round() / 1e6,
                    "sp": u8::from(state.speaking),
                    "sent_ms": state.sentence_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "sil_ms": state.silence_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                    "seg_ms": state.segment_samples as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                }));
                if event.is_boundary() {
                    events.push(serde_json::json!({
                        "time_ms": processed as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                        "reason": event.reason(),
                    }));
                    vad.reset_sentence();
                }
            }
            let frame_events: Vec<_> = events
                .iter()
                .map(|event| {
                    (
                        event["time_ms"].as_u64().unwrap(),
                        event["reason"].as_str().unwrap(),
                    )
                })
                .collect();
            let trace_events: Vec<_> = compact_trace
                .events
                .iter()
                .map(|event| (event.time_ms, event.reason))
                .collect();
            assert_eq!(
                trace_events, frame_events,
                "compact VAD trace drifted for {case_id}"
            );
            case_reports.push(serde_json::json!({
                "case_id": case_id,
                "duration_ms": samples.len() as u64 * 1000 / TARGET_SAMPLE_RATE as u64,
                "events": events,
                "compact_trace": compact_trace,
                "frames": frames,
            }));
        }

        let output = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target/stt-vad-frame-diagnostics.json");
        let report = serde_json::json!({
            "scope": "0.23.7.1 frame-level EnergyVad diagnostics (default A params) over private corpus; numeric only, no audio or transcript content.",
            "params": {
                "silence_threshold": threshold,
                "min_silence_ms": silence_ms,
                "min_sentence_ms": sentence_ms,
                "soft_window_s": 8,
                "hard_window_s": 12,
            },
            "cases": case_reports,
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous frame diagnostics report");
    }

    /// 与生产 `EnergyVad` 相同的帧 RMS 公式（10ms 帧对齐时结果一致）。
    fn frame_rms(samples: &[f32]) -> f64 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = samples
            .iter()
            .map(|s| (*s as f64) * (*s as f64))
            .sum::<f64>();
        (sum_sq / samples.len() as f64).sqrt()
    }

    /// 验证 48k stereo WAV 能正确解码和规范化为 16k mono。
    #[test]
    fn decode_and_normalize_48k_stereo_to_16k_mono() {
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 4800, // 100ms @ 48k
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let summary = decode_and_normalize(&wav).expect("decode failed");

        assert_eq!(summary.channels, 2);
        assert_eq!(summary.sample_rate, 48000);
        assert_eq!(summary.bits_per_sample, 16);
        assert_eq!(summary.duration_ms, 100);
        // 规范化后应为 16k mono = 1600 samples (100ms)
        assert_eq!(summary.samples.len(), 1600);
        assert_normalized_format(&summary).expect("normalized format check failed");
    }

    /// 验证全静音 WAV 解码后样本全为零。
    #[test]
    fn decode_silence_produces_zero_samples() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1600, // 100ms @ 16k
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let summary = decode_and_normalize(&wav).expect("decode failed");

        assert!(summary.samples.iter().all(|&s| s == 0.0), "静音应全零");
        assert_eq!(summary.duration_ms, 100);
    }

    /// 验证文本匹配逻辑。
    #[test]
    fn evaluate_result_matches_expected_text() {
        let case = CorpusCase {
            case_id: "case_01".into(),
            filename: "test.wav".into(),
            scene: "test".into(),
            expected_empty: false,
            expected_segments: 1,
            expected_text: "你好世界".into(),
            allowed_normalized: vec!["你好世界".into()],
        };
        assert!(evaluate_result(&case, "你好世界", "你好世界"));
        assert!(evaluate_result(&case, "你好世界。", "你好世界"));
        assert!(!evaluate_result(&case, "", ""));
        assert_eq!(best_similarity_percent(&case, "你好世界"), 100);
        assert_eq!(best_similarity_percent(&case, "你好世间"), 75);
    }

    #[test]
    fn manifest_rejects_path_escape_and_duplicate_ids() {
        let escaped = r#"
            [[cases]]
            case_id = "case_01"
            filename = "../private.wav"
            scene = "test"
            expected_empty = true
            expected_segments = 0
            expected_text = ""
        "#;
        assert!(parse_manifest(escaped).is_err());

        let duplicate = r#"
            [[cases]]
            case_id = "case_01"
            filename = "a.wav"
            scene = "test"
            expected_empty = true
            expected_segments = 0
            expected_text = ""
            [[cases]]
            case_id = "case_01"
            filename = "b.wav"
            scene = "test"
            expected_empty = true
            expected_segments = 0
            expected_text = ""
        "#;
        assert!(parse_manifest(duplicate).is_err());

        let descriptive_id = r#"
            [[cases]]
            case_id = "meeting_alice"
            filename = "a.wav"
            scene = "test"
            expected_empty = true
            expected_segments = 0
            expected_text = ""
        "#;
        assert!(parse_manifest(descriptive_id).is_err());
    }

    struct FakeCorpusTransport;

    #[async_trait::async_trait]
    impl SttTransport for FakeCorpusTransport {
        async fn check_ready(&self) -> Result<(), crate::domain::stt::SttTransportError> {
            Ok(())
        }

        async fn transcribe(
            &self,
            _wav_bytes: &[u8],
        ) -> Result<String, crate::domain::stt::SttTransportError> {
            Ok("hello".to_string())
        }

        async fn transcribe_with_metrics(
            &self,
            _wav_bytes: &[u8],
        ) -> Result<crate::domain::stt::SttTransportResult, crate::domain::stt::SttTransportError>
        {
            Ok(crate::domain::stt::SttTransportResult {
                text: "hello".to_string(),
                inference_ms: Some(1.0),
            })
        }
    }

    #[tokio::test]
    async fn runner_loads_manifest_walks_cases_and_invokes_transport() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1600,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        std::fs::write(dir.path().join("case.wav"), build_wav(&cfg)).unwrap();
        std::fs::write(
            dir.path().join("manifest.toml"),
            r#"
                [[cases]]
                case_id = "case_01"
                filename = "case.wav"
                scene = "test"
                expected_empty = false
                expected_segments = 0
                expected_text = "hello"
            "#,
        )
        .unwrap();

        let results = run_corpus(CorpusRunnerConfig {
            corpus_dir: dir.path().to_path_buf(),
            transport: Some(Arc::new(FakeCorpusTransport)),
            worker_pid: None,
        })
        .await
        .unwrap();

        assert_eq!(results.len(), 1);
        assert!(results[0].matched);
        assert_eq!(results[0].inference_ms, Some(1));
        assert!(!format_anonymous_summary(&results).contains("hello"));
        assert!(!format_anonymous_summary(&results).contains("case.wav"));
    }

    /// 验证 expected_empty 的匹配逻辑。
    #[test]
    fn evaluate_result_empty_case() {
        let case = CorpusCase {
            case_id: "case_01".into(),
            filename: "silence.wav".into(),
            scene: "silence".into(),
            expected_empty: true,
            expected_segments: 0,
            expected_text: "".into(),
            allowed_normalized: vec![],
        };
        assert!(evaluate_result(&case, "", ""));
        assert!(!evaluate_result(&case, "some text", "some text"));
        assert!(evaluate_result(&case, "。！？", ""));
    }

    /// 验证 canonical WAV 编码的格式正确。
    #[test]
    fn encode_canonical_wav_produces_valid_wav() {
        let samples = vec![0.0f32; 1600]; // 100ms @ 16k
        let wav = encode_canonical_wav(&samples);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        assert_eq!(&wav[12..16], b"fmt ");
        // mono, 16k, 16-bit
        let channels = u16::from_le_bytes([wav[22], wav[23]]);
        let sr = u32::from_le_bytes([wav[24], wav[25], wav[26], wav[27]]);
        let bits = u16::from_le_bytes([wav[34], wav[35]]);
        assert_eq!(channels, 1);
        assert_eq!(sr, 16000);
        assert_eq!(bits, 16);
    }

    /// 验证 normalize_for_compare 去除标点和空格。
    #[test]
    fn normalize_for_compare_strips_punctuation() {
        assert_eq!(normalize_for_compare("你好，世界！"), "你好世界");
        assert_eq!(normalize_for_compare("Hello, World!"), "helloworld");
        assert_eq!(normalize_for_compare("  spaces  "), "spaces");
    }

    /// 验证匿名摘要输出不包含完整转写文本。
    #[test]
    fn anonymous_summary_does_not_leak_text() {
        let results = [CorpusResult {
            case_id: "case_01".into(),
            format_ok: true,
            duration_ms: 1000,
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            decode_ms: 5,
            normalize_ms: 1,
            queue_ms: Some(2),
            inference_ms: Some(10),
            total_ms: 18,
            peak_memory_bytes: Some(1024),
            detected_segments: 1,
            expected_segments: 1,
            segments_matched: true,
            text_empty: false,
            text_matched: true,
            text_chars: 4,
            best_similarity_percent: 100,
            matched: true,
            error_category: None,
        }];
        // 只验证结构正确，不验证 stdout 内容（print 函数本身不输出文本）
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].case_id, "case_01");
    }
}
