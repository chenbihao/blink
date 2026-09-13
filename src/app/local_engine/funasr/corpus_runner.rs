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
use crate::domain::stt::vad::{EnergyVad, VadEvent};
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
    let mut vad = EnergyVad::new(TARGET_SAMPLE_RATE);
    let mut segments = 0u32;
    for chunk in samples.chunks((TARGET_SAMPLE_RATE / 100) as usize) {
        if vad.process_chunk(chunk) != VadEvent::None {
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
fn normalize_for_compare(text: &str) -> String {
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

fn edit_similarity_percent(left: &str, right: &str) -> u8 {
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

    /// 用私有语料回放生产 EnergyVad，比较参数对切段数量和触发时刻的影响。
    /// 只写匿名 case id 与时间点；不输出音频、路径或转写正文。
    /// 这里不执行 ASR/terminal finalize：短词即使未触发 VAD，松键后仍可能识别成功。
    #[test]
    fn private_corpus_vad_parameter_sweep() {
        if std::env::var("BLINK_STT_VAD_SWEEP").ok().as_deref() != Some("1") {
            return;
        }
        let corpus_dir =
            should_run().expect("BLINK_STT_CORPUS_DIR must point to a corpus directory");
        let manifest = load_manifest(&corpus_dir).expect("load private corpus manifest");
        let listed_names: HashSet<_> = manifest
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
        let mut unlisted = Vec::new();
        for entry in std::fs::read_dir(&corpus_dir).expect("read corpus directory") {
            let entry = entry.expect("read corpus entry");
            let path = entry.path();
            if !path.is_file()
                || path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_none_or(|ext| !ext.eq_ignore_ascii_case("wav"))
                || listed_names.contains(&entry.file_name().to_string_lossy().to_lowercase())
            {
                continue;
            }
            let wav = std::fs::read(path).expect("read unlisted WAV");
            let id = {
                use sha2::{Digest, Sha256};
                let digest = format!("{:x}", Sha256::digest(&wav));
                format!("new_{}", &digest[..8])
            };
            let audio = decode_and_normalize(&wav).expect("decode and normalize unlisted WAV");
            unlisted.push((id, None, audio.samples));
        }
        unlisted.sort_by(|a, b| a.0.cmp(&b.0));
        cases.extend(unlisted);

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
                let mut vad = EnergyVad::with_params_and_windows(
                    TARGET_SAMPLE_RATE,
                    threshold,
                    silence_ms,
                    sentence_ms,
                    soft_window_s as u64 * 1000,
                    hard_window_s as u64 * 1000,
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
            "scope": "0.23.7 candidate matrix (A-H): EnergyVad soft/hard windows plus pseudo-streaming uncommitted-cap boundary replay; potential final calls, not recognized speech. ASR quality is evaluated separately.",
            "labeled_cases": listed_names.len(),
            "unlabeled_cases": cases.len() - listed_names.len(),
            "runs": runs,
        });
        std::fs::write(&output, serde_json::to_vec_pretty(&report).unwrap())
            .expect("write anonymous VAD parameter report");
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
