//! STT Gate Corpus 工具——录制、校验、标注模板（0.22.9 Handoff 07E）。
//!
//! 建立私有、可复现的真实中文 STT gate corpus 格式、校验器、录制工具和标注模板。
//!
//! ## 隐私铁则
//!
//! - 录音默认只保存在用户明确指定的本地目录
//! - 不上传任何外部服务
//! - 不进入 Git
//! - 不复制到发布资源
//! - 日志不得记录原始音频内容
//!
//! ## 使用
//!
//! ```bash
//! # 方式 A（推荐）：从公开数据集导入
//! # 下载 THCHS-30 (https://www.openslr.org/18/)，解压后：
//! blink.exe stt-corpus import --dataset-dir /path/to/thchs30/data --corpus-dir ./corpus --max-samples 50
//!
//! # 方式 B：手动录制（见 prompts 子命令）
//! blink.exe stt-corpus init --corpus-dir ./corpus
//! ```
//!
//! ## 一条录制命令
//!
//! ```bash
//! # 录制：用 Audacity 以 16kHz mono PCM 16-bit 录音，导出到 corpus/wavs/near_short_01.wav
//! # 然后在 manifest.json 中填入对应条目
//! ```
//!
//! ## 一条验证命令
//!
//! ```bash
//! blink.exe stt-corpus validate --corpus-dir ./corpus
//! ```

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

// ── Corpus Manifest Schema ──────────────────────────────────────────────

/// Corpus manifest——整个 corpus 的元数据文件。
///
/// 位于 corpus 根目录的 `manifest.json`。
/// 每条样本一个条目，包含所有 gate 所需的标注信息。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusManifest {
    /// Schema 版本。
    pub schema_version: u32,
    /// Corpus 名称。
    pub name: String,
    /// Corpus 描述。
    pub description: String,
    /// 创建时间（ISO 8601）。
    pub created_at: String,
    /// 样本列表。
    pub samples: Vec<CorpusSample>,
}

/// 单条 corpus 样本的完整标注。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CorpusSample {
    /// 样本唯一 ID（如 "near_short_01"）。
    pub sample_id: String,
    /// WAV 文件相对路径（相对于 corpus 根目录）。
    pub wav_path: String,
    /// 精确 reference text（人工标注的 ground truth）。
    pub reference_text: String,
    /// 场景分类（如 "near_short" / "far_field" / "keyboard_noise" 等）。
    pub scenario: String,
    /// 录音设备（如 "built_in_mic" / "usb_mic" / "headset"）。
    pub recording_device: String,
    /// 近讲 / 远场。
    pub mic_distance: MicDistance,
    /// 噪声类型。
    pub noise_type: NoiseType,
    /// 预期句子数量（用于多句样本）。
    pub expected_sentence_count: u32,
    /// 人工句界时间（秒）——每句的开始/结束时间。
    /// 格式: [(start_s, end_s), ...]
    #[serde(default)]
    pub sentence_boundaries: Vec<(f64, f64)>,
    /// 总时长（秒）。
    pub duration_s: f64,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 声道数。
    pub channels: u32,
    /// 可选说话人匿名 ID（如 "speaker_A"）。
    #[serde(default)]
    pub speaker_anon_id: Option<String>,
}

/// 近讲 / 远场。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MicDistance {
    /// 近讲（< 50cm）。
    Near,
    /// 远场（>= 50cm）。
    Far,
}

/// 噪声类型。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum NoiseType {
    /// 安静（环境噪声 < 30dB）。
    Quiet,
    /// 风扇 / 空调。
    FanAc,
    /// 键盘敲击。
    Keyboard,
    /// 背景音乐或环境声。
    Background,
    /// 纯静音（负例）。
    PureSilence,
    /// 纯噪声（负例）。
    PureNoise,
}

/// Corpus 校验结果。
#[derive(Debug)]
pub struct ValidationResult {
    /// 是否通过。
    pub valid: bool,
    /// 错误列表。
    pub errors: Vec<String>,
    /// 警告列表。
    pub warnings: Vec<String>,
    /// 样本总数。
    pub sample_count: usize,
    /// 场景覆盖统计。
    pub scenario_coverage: std::collections::HashMap<String, usize>,
}

// ── 校验器 ───────────────────────────────────────────────────────────────

/// 校验 corpus 目录。
///
/// 检查：
/// - manifest.json 存在且可解析
/// - 每条 sample 的 WAV 文件存在
/// - 采样率 = 16kHz
/// - 声道 = 1（mono）
/// - reference_text 非空（pure_silence/pure_noise 负例除外）
/// - sample_id 唯一（无重复）
/// - 句界不越界（不超过 duration_s）
/// - 场景覆盖
pub fn validate_corpus(corpus_dir: &Path) -> Result<ValidationResult, String> {
    let manifest_path = corpus_dir.join("manifest.json");
    if !manifest_path.exists() {
        return Err(format!("manifest.json 不存在: {}", manifest_path.display()));
    }

    let manifest_content =
        std::fs::read_to_string(&manifest_path).map_err(|e| format!("读取 manifest 失败: {e}"))?;
    let manifest: CorpusManifest = serde_json::from_str(&manifest_content)
        .map_err(|e| format!("manifest.json 解析失败: {e}"))?;

    let mut errors = Vec::new();
    let mut warnings = Vec::new();
    let mut sample_ids: HashSet<String> = HashSet::new();
    let mut scenario_coverage: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for (i, sample) in manifest.samples.iter().enumerate() {
        // sample_id 唯一性
        if !sample_ids.insert(sample.sample_id.clone()) {
            errors.push(format!(
                "sample[{}]: 重复 sample_id '{}'",
                i, sample.sample_id
            ));
        }

        // WAV 文件存在
        let wav_path = corpus_dir.join(&sample.wav_path);
        if !wav_path.exists() {
            errors.push(format!(
                "sample[{}] '{}': WAV 文件不存在: {}",
                i,
                sample.sample_id,
                wav_path.display()
            ));
        } else {
            // 采样率/声道检查
            if let Ok(spec) = read_wav_header(&wav_path) {
                if spec.0 != 16000 {
                    errors.push(format!(
                        "sample[{}] '{}': 采样率应为 16000，实际 {}",
                        i, sample.sample_id, spec.0
                    ));
                }
                if spec.1 != 1 {
                    errors.push(format!(
                        "sample[{}] '{}': 声道应为 1（mono），实际 {}",
                        i, sample.sample_id, spec.1
                    ));
                }
            } else {
                warnings.push(format!(
                    "sample[{}] '{}': WAV 文件无法读取 header",
                    i, sample.sample_id
                ));
            }
        }

        // reference_text 非空（负例除外）
        let is_negative = sample.noise_type == NoiseType::PureSilence
            || sample.noise_type == NoiseType::PureNoise;
        if !is_negative && sample.reference_text.is_empty() {
            errors.push(format!(
                "sample[{}] '{}': reference_text 为空（非负例不应为空）",
                i, sample.sample_id
            ));
        }

        // 句界不越界
        for (idx, (start, end)) in sample.sentence_boundaries.iter().enumerate() {
            if *start < 0.0 || *end < 0.0 {
                errors.push(format!(
                    "sample[{}] '{}': sentence_boundaries[{}] 时间为负数",
                    i, sample.sample_id, idx
                ));
            }
            if *end > sample.duration_s {
                errors.push(format!(
                    "sample[{}] '{}': sentence_boundaries[{}] end={} 超过 duration={}",
                    i, sample.sample_id, idx, end, sample.duration_s
                ));
            }
        }

        // 场景覆盖统计
        *scenario_coverage
            .entry(sample.scenario.clone())
            .or_insert(0) += 1;
    }

    // 场景覆盖检查
    let required_scenarios = [
        "near_short",
        "near_long",
        "far_field",
        "fan_ac_noise",
        "keyboard_noise",
        "background_noise",
        "mid_pause",
        "multi_sentence",
        "numbers_dates_english",
        "pure_silence",
        "pure_noise",
    ];
    for req in &required_scenarios {
        if !scenario_coverage.contains_key(*req) {
            warnings.push(format!("缺少场景覆盖: {req}"));
        }
    }

    // 最少 10 条
    if manifest.samples.len() < 10 {
        warnings.push(format!(
            "样本数 {} 少于推荐的 10 条",
            manifest.samples.len()
        ));
    }

    let valid = errors.is_empty();
    Ok(ValidationResult {
        valid,
        errors,
        warnings,
        sample_count: manifest.samples.len(),
        scenario_coverage,
    })
}

/// 读取 WAV 文件 header，返回 (sample_rate, channels)。
fn read_wav_header(path: &Path) -> Result<(u32, u32), String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header).map_err(|e| e.to_string())?;

    // RIFF header
    if &header[0..4] != b"RIFF" {
        return Err("不是 WAV 文件".to_string());
    }
    // fmt chunk
    if &header[12..16] != b"fmt " {
        return Err("WAV fmt chunk 不存在".to_string());
    }
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let channels = u16::from_le_bytes([header[22], header[23]]) as u32;
    Ok((sample_rate, channels))
}

/// 读取 WAV 文件 header，返回 (sample_rate, channels, duration_s)。
fn read_wav_full_header(path: &Path) -> Result<(u32, u32, f64), String> {
    use std::io::Read;
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut header = [0u8; 44];
    file.read_exact(&mut header).map_err(|e| e.to_string())?;

    if &header[0..4] != b"RIFF" {
        return Err("不是 WAV 文件".to_string());
    }
    if &header[12..16] != b"fmt " {
        return Err("WAV fmt chunk 不存在".to_string());
    }
    let sample_rate = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
    let channels = u16::from_le_bytes([header[22], header[23]]) as u32;
    let bits_per_sample = u16::from_le_bytes([header[34], header[35]]) as u32;
    let byte_rate = u32::from_le_bytes([header[28], header[29], header[30], header[31]]);
    let data_size = u32::from_le_bytes([header[40], header[41], header[42], header[43]]);
    let duration_s = if byte_rate > 0 {
        data_size as f64 / byte_rate as f64
    } else if sample_rate > 0 && channels > 0 && bits_per_sample > 0 {
        data_size as f64 / (sample_rate * channels * (bits_per_sample / 8)) as f64
    } else {
        0.0
    };
    Ok((sample_rate, channels, duration_s))
}

// ── 公开数据集导入 ──────────────────────────────────────────────────────

/// 转录文件格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TranscriptFormat {
    /// THCHS-30 原始 `.trn`：第一行空格分隔词语，需去空格拼接。
    Trn,
    /// HF 仓库 `.lab`：整句纯文本，直接使用。
    Lab,
}

/// 从 THCHS-30 数据集导入 corpus。
///
/// THCHS-30（清华中文 30 小时数据集，Apache 2.0）：
/// - 16kHz mono PCM 16-bit WAV
/// - 每条 WAV 配一个转录文件（.trn 或 .lab）
///
/// 支持两种目录结构：
///
/// **格式 A — 原始 THCHS-30（扁平 .trn）**：
/// ```text
/// data/
///   A11_001.wav
///   A11_001.trn    ← "东北军 的 一些 爱国 将士 ..." (空格分隔词语)
/// ```
///
/// **格式 B — HF 仓库（说话人子目录 .lab）**：
/// ```text
/// dev/
///   A34/A34_118.wav
///   A34/A34_118.lab  ← "碰上爱玩且能玩物成痴..." (整句纯文本)
///   B2/B2_268.wav
///   B2/B2_268.lab
/// ```
///
/// 导入流程：
/// 1. 递归扫描 `--dataset-dir` 中的 .wav 文件
/// 2. 对每个 .wav 查找同名 .trn 或 .lab 文件
/// 3. 解析转录文本（.trn 去空格拼接，.lab 直接使用）
/// 4. 读取 WAV header 获取采样率/声道/时长
/// 5. 生成 manifest.json，每条样本作为一个完整 segment
/// 6. 复制 WAV 到 corpus/wavs/
///
/// 许可证：Apache 2.0（可商用，需署名）
pub fn import_thchs30(
    dataset_dir: &Path,
    corpus_dir: &Path,
    max_samples: usize,
) -> Result<usize, String> {
    if !dataset_dir.exists() {
        return Err(format!("数据集目录不存在: {}", dataset_dir.display()));
    }

    // 递归收集所有 .wav 文件
    let mut wav_files = Vec::new();
    collect_wav_files(dataset_dir, &mut wav_files);

    if wav_files.is_empty() {
        return Err(format!(
            "数据集目录中未找到 .wav 文件: {}",
            dataset_dir.display()
        ));
    }

    // 创建 corpus 目录
    let wavs_dir = corpus_dir.join("wavs");
    std::fs::create_dir_all(&wavs_dir).map_err(|e| format!("创建 wavs 目录失败: {e}"))?;

    let mut samples = Vec::new();
    let mut imported = 0;

    for wav_path in &wav_files {
        if imported >= max_samples {
            break;
        }

        let stem = wav_path
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or("无效文件名")?;

        // 查找转录文件：优先 .lab（HF 格式），其次 .trn（原始格式）
        let lab_path = wav_path.with_extension("lab");
        let trn_path = wav_path.with_extension("trn");

        let (transcript_content, fmt) = if lab_path.exists() {
            (
                std::fs::read_to_string(&lab_path).map_err(|e| format!("读取 .lab 失败: {e}"))?,
                TranscriptFormat::Lab,
            )
        } else if trn_path.exists() {
            (
                std::fs::read_to_string(&trn_path).map_err(|e| format!("读取 .trn 失败: {e}"))?,
                TranscriptFormat::Trn,
            )
        } else {
            continue; // 跳过没有转录的 WAV
        };

        // 解析转录文本
        let reference_text = parse_transcript(&transcript_content, fmt);
        if reference_text.is_empty() {
            continue;
        }

        // 读取 WAV 信息
        let (sample_rate, channels, duration_s) = match read_wav_full_header(wav_path) {
            Ok(info) => info,
            Err(_) => continue,
        };

        // 跳过不符合要求的样本
        if sample_rate != 16000 || channels != 1 {
            continue;
        }

        let wav_filename = format!("{stem}.wav");
        let dest_wav = wavs_dir.join(&wav_filename);

        // 复制 WAV 文件
        if let Err(e) = std::fs::copy(wav_path, &dest_wav) {
            tracing::warn!(file = %stem, %e, "复制 WAV 失败，跳过");
            continue;
        }

        // 从子目录路径提取说话人 ID（如 B2/B2_268 → B2）
        let speaker_id = wav_path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        let sample = CorpusSample {
            sample_id: stem.to_string(),
            wav_path: format!("wavs/{wav_filename}"),
            reference_text,
            scenario: "thchs30_read".to_string(),
            recording_device: "unknown".to_string(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            sentence_boundaries: vec![(0.0, duration_s)],
            duration_s: (duration_s * 1000.0).round() / 1000.0,
            sample_rate,
            channels,
            speaker_anon_id: speaker_id,
        };
        samples.push(sample);
        imported += 1;
    }

    if samples.is_empty() {
        return Err("未找到有效的 .wav + (.trn|.lab) 对（检查采样率=16kHz、mono）".to_string());
    }

    // 写 manifest.json
    let manifest = CorpusManifest {
        schema_version: 1,
        name: "Blink STT Gate Corpus (THCHS-30)".to_string(),
        description: "从 THCHS-30 导入的中文 STT gate corpus (Apache 2.0, 清华大学)".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        samples,
    };

    let manifest_path = corpus_dir.join("manifest.json");
    let json = serde_json::to_string_pretty(&manifest).map_err(|e| format!("序列化失败: {e}"))?;
    std::fs::write(&manifest_path, json).map_err(|e| format!("写入 manifest 失败: {e}"))?;

    Ok(imported)
}

/// 递归收集目录下所有 .wav 文件。
fn collect_wav_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_wav_files(&path, out);
        } else if path.extension().and_then(|e| e.to_str()) == Some("wav") {
            out.push(path);
        }
    }
}

/// 解析转录文本。
///
/// - `.trn` 格式：第一行空格分隔词语，去空格拼接为完整句子
/// - `.lab` 格式：整句纯文本，直接取第一行
fn parse_transcript(content: &str, fmt: TranscriptFormat) -> String {
    let first_line = content.lines().next().unwrap_or("").trim();
    match fmt {
        TranscriptFormat::Trn => first_line
            .split_whitespace()
            .collect::<Vec<&str>>()
            .join(""),
        TranscriptFormat::Lab => first_line.to_string(),
    }
}

/// 生成 corpus summary（不运行模型）。
pub fn generate_summary(corpus_dir: &Path) -> Result<String, String> {
    let manifest_path = corpus_dir.join("manifest.json");
    let manifest_content =
        std::fs::read_to_string(&manifest_path).map_err(|e| format!("读取 manifest 失败: {e}"))?;
    let manifest: CorpusManifest =
        serde_json::from_str(&manifest_content).map_err(|e| format!("解析失败: {e}"))?;

    let total_duration: f64 = manifest.samples.iter().map(|s| s.duration_s).sum();
    let mut scenario_coverage: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut device_coverage: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();
    let mut noise_coverage: std::collections::HashMap<String, usize> =
        std::collections::HashMap::new();

    for s in &manifest.samples {
        *scenario_coverage.entry(s.scenario.clone()).or_insert(0) += 1;
        *device_coverage
            .entry(s.recording_device.clone())
            .or_insert(0) += 1;
        *noise_coverage
            .entry(format!("{:?}", s.noise_type))
            .or_insert(0) += 1;
    }

    let summary = serde_json::json!({
        "name": manifest.name,
        "description": manifest.description,
        "sample_count": manifest.samples.len(),
        "total_duration_s": (total_duration * 1000.0).round() / 1000.0,
        "scenario_coverage": scenario_coverage,
        "device_coverage": device_coverage,
        "noise_coverage": noise_coverage,
        "schema_version": manifest.schema_version,
    });

    Ok(serde_json::to_string_pretty(&summary).unwrap())
}

/// 生成空白 manifest 模板。
pub fn generate_template_manifest() -> CorpusManifest {
    CorpusManifest {
        schema_version: 1,
        name: "Blink STT Gate Corpus".to_string(),
        description: "私有、可复现的真实中文 STT gate corpus".to_string(),
        created_at: chrono::Utc::now().to_rfc3339(),
        samples: vec![CorpusSample {
            sample_id: "template_01".to_string(),
            wav_path: "wavs/template_01.wav".to_string(),
            reference_text: "在此填入精确的参考文本。".to_string(),
            scenario: "near_short".to_string(),
            recording_device: "built_in_mic".to_string(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            sentence_boundaries: vec![(0.0, 2.0)],
            duration_s: 2.0,
            sample_rate: 16000,
            channels: 1,
            speaker_anon_id: Some("speaker_A".to_string()),
        }],
    }
}

// ── 录制 Prompt 清单 ───────────────────────────────────────────────────

/// 录制 prompt——固定朗读文本和场景定义。
///
/// 用户按此清单逐条录制，覆盖所有要求的场景。
/// 每条覆盖一个不同场景，共 10 条。
#[derive(Debug, Clone)]
pub struct RecordingPrompt {
    /// sample_id（如 "near_short_01"）。
    pub sample_id: String,
    /// 场景分类。
    pub scenario: String,
    /// 朗读文本（纯静音/纯噪声负例为空）。
    pub reference_text: String,
    /// 近讲/远场。
    pub mic_distance: MicDistance,
    /// 噪声类型。
    pub noise_type: NoiseType,
    /// 预期句子数量。
    pub expected_sentence_count: u32,
    /// 录制提示（给用户的说明）。
    pub instructions: String,
}

/// 生成精简录制 prompt 清单（10 条，覆盖全部核心场景）。
///
/// 每条覆盖一个不同场景：
/// - 近讲短句
/// - 近讲长句
/// - 远场
/// - 风扇/空调噪声
/// - 键盘噪声
/// - 句中短停顿
/// - 多句连续
/// - 数字/英文夹杂
/// - 纯静音负例
/// - 纯噪声负例
///
/// 共 10 条
pub fn generate_recording_prompts() -> Vec<RecordingPrompt> {
    vec![
        // 1. 近讲短句
        RecordingPrompt {
            sample_id: "near_short_01".into(),
            scenario: "near_short".into(),
            reference_text: "搜索微信。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            instructions: "距麦克风 10-20cm，正常语速。".into(),
        },
        // 2. 近讲长句
        RecordingPrompt {
            sample_id: "near_long_01".into(),
            scenario: "near_long".into(),
            reference_text: "请帮我查找一下最近打开过的所有文档，然后按修改时间排序。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            instructions: "距麦克风 10-20cm，一口气读完。".into(),
        },
        // 3. 远场
        RecordingPrompt {
            sample_id: "far_field_01".into(),
            scenario: "far_field".into(),
            reference_text: "帮我打开浏览器。".into(),
            mic_distance: MicDistance::Far,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            instructions: "距麦克风 1-2 米，正常音量。".into(),
        },
        // 4. 风扇/空调噪声
        RecordingPrompt {
            sample_id: "fan_ac_01".into(),
            scenario: "fan_ac_noise".into(),
            reference_text: "帮我新建一个文件夹。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::FanAc,
            expected_sentence_count: 1,
            instructions: "近讲，开风扇或空调。".into(),
        },
        // 5. 键盘噪声
        RecordingPrompt {
            sample_id: "keyboard_01".into(),
            scenario: "keyboard_noise".into(),
            reference_text: "帮我搜索文件管理器。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Keyboard,
            expected_sentence_count: 1,
            instructions: "近讲，同时敲击键盘模拟打字噪声。".into(),
        },
        // 6. 句中短停顿
        RecordingPrompt {
            sample_id: "mid_pause_01".into(),
            scenario: "mid_pause".into(),
            reference_text: "先打开浏览器……然后访问百度首页。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 2,
            instructions: "近讲，句中停顿约 0.5-1 秒。".into(),
        },
        // 7. 多句连续
        RecordingPrompt {
            sample_id: "multi_sentence_01".into(),
            scenario: "multi_sentence".into(),
            reference_text: "搜索微信。打开第一个结果。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 2,
            instructions: "近讲，连续说两句话，中间短暂停顿。".into(),
        },
        // 8. 数字/英文夹杂
        RecordingPrompt {
            sample_id: "numbers_01".into(),
            scenario: "numbers_dates_english".into(),
            reference_text: "帮我打开 GitHub 看一下今天的提交。".into(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Quiet,
            expected_sentence_count: 1,
            instructions: "近讲，包含英文和数字。".into(),
        },
        // 9. 纯静音负例
        RecordingPrompt {
            sample_id: "pure_silence_01".into(),
            scenario: "pure_silence".into(),
            reference_text: String::new(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::PureSilence,
            expected_sentence_count: 0,
            instructions: "保持安静 3 秒，不说话。用于测试误触发。".into(),
        },
        // 10. 纯噪声负例
        RecordingPrompt {
            sample_id: "pure_noise_01".into(),
            scenario: "pure_noise".into(),
            reference_text: String::new(),
            mic_distance: MicDistance::Near,
            noise_type: NoiseType::Keyboard,
            expected_sentence_count: 0,
            instructions: "仅敲击键盘 3 秒，不说话。用于测试误触发。".into(),
        },
    ]
}

/// 生成人工标注模板说明（打印到 stdout）。
pub fn print_annotation_template() {
    println!("=== STT Gate Corpus 人工标注模板说明 ===");
    println!();
    println!("[sentence_boundaries 格式]");
    println!("  每条样本的 sentence_boundaries 是一个 JSON 数组，");
    println!("  每个元素是 [start_s, end_s] 表示一句话的开始和结束时间（秒）。");
    println!("  例如：[[0.0, 1.5], [2.0, 3.5]] 表示两句话。");
    println!();
    println!("[句界时间测量方法]");
    println!("  1. 使用音频编辑器（如 Audacity）打开 WAV 文件");
    println!("  2. 播放并观察波形/频谱图");
    println!("  3. 语音段的起始点 = 波形从静默变为有声的位置");
    println!("  4. 语音段的结束点 = 波形从有声回到静默的位置");
    println!("  5. 记录每句话的 [开始, 结束] 时间（秒，精度 0.01s）");
    println!();
    println!("[负例标注]");
    println!("  纯静音/纯噪声负例：reference_text 为空字符串，");
    println!("  sentence_boundaries 为空数组 []，expected_sentence_count = 0。");
    println!();
    println!("[speaker_anon_id]");
    println!("  可选字段。同一说话人录多条可填入匿名 ID（如 speaker_A）。");
    println!("  不同说话人使用不同 ID。不记录真实姓名。");
    println!();
    println!("[manifest.json 编辑流程]");
    println!("  1. blink.exe stt-corpus init --corpus-dir ./corpus");
    println!("  2. blink.exe stt-corpus prompts  # 查看 10 条录制 prompt 清单");
    println!("  3. 按 prompt 清单逐条录制 WAV 到 corpus/wavs/（16kHz mono PCM 16-bit）");
    println!("  4. 编辑 manifest.json，替换 samples 数组中的条目");
    println!("  5. 用音频编辑器（如 Audacity）测量句界时间，填入 sentence_boundaries");
    println!("  6. blink.exe stt-corpus validate --corpus-dir ./corpus");
    println!("  7. blink.exe stt-corpus summary --corpus-dir ./corpus");
    println!();
    println!("[当前样本缺口]");
    println!("  推荐至少 10 条，当前 prompt 清单提供 10 条。");
    println!("  当前已录制: 0 条（工具已就绪，等待真人录制）");
    println!("  缺口: 10 条（全部待录）");
    println!("  录制完成后运行 validate 检查是否有缺失场景。");
}

// ── CLI 入口 ─────────────────────────────────────────────────────────────

/// STT corpus CLI 主入口。
///
/// 子命令：
/// - `import` — 从公开数据集（THCHS-30）导入 corpus
/// - `init` — 生成空白 manifest 模板 + wavs/ 目录
/// - `prompts` — 打印 10 条录制 prompt 清单
/// - `annotation` — 打印人工标注模板说明
/// - `validate` — 校验 corpus
/// - `summary` — 生成 corpus summary
pub fn run_from_args(args: &[String]) -> i32 {
    if args.is_empty() {
        eprintln!(
            "用法: blink.exe stt-corpus <import|init|prompts|annotation|validate|summary> [options]"
        );
        return 1;
    }

    match args[0].as_str() {
        "import" => {
            let dataset_dir = match get_arg(args, "--dataset-dir") {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("缺少 --dataset-dir 参数");
                    return 1;
                }
            };
            let corpus_dir = match get_arg(args, "--corpus-dir") {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("缺少 --corpus-dir 参数");
                    return 1;
                }
            };
            let max_samples = get_arg(args, "--max-samples")
                .and_then(|s| s.parse().ok())
                .unwrap_or(50);
            match import_thchs30(&dataset_dir, &corpus_dir, max_samples) {
                Ok(n) => {
                    println!("✅ 导入完成: {n} 条样本");
                    println!("   corpus 目录: {}", corpus_dir.display());
                    println!(
                        "   运行 validate 检查: blink.exe stt-corpus validate --corpus-dir {}",
                        corpus_dir.display()
                    );
                    0
                }
                Err(e) => {
                    eprintln!("导入失败: {e}");
                    1
                }
            }
        }
        "init" => {
            let corpus_dir = match get_arg(args, "--corpus-dir") {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("缺少 --corpus-dir 参数");
                    return 1;
                }
            };
            let manifest = generate_template_manifest();
            let manifest_path = corpus_dir.join("manifest.json");
            if let Err(e) = std::fs::create_dir_all(&corpus_dir) {
                eprintln!("创建目录失败: {e}");
                return 1;
            }
            let json = serde_json::to_string_pretty(&manifest).unwrap();
            if let Err(e) = std::fs::write(&manifest_path, json) {
                eprintln!("写入 manifest 失败: {e}");
                return 1;
            }
            let wavs_dir = corpus_dir.join("wavs");
            let _ = std::fs::create_dir_all(&wavs_dir);
            println!("✅ 已生成 manifest 模板: {}", manifest_path.display());
            println!("   WAV 目录: {}", wavs_dir.display());
            println!("   请编辑 manifest.json 并放入 WAV 文件");
            0
        }
        "prompts" => {
            let prompts = generate_recording_prompts();
            println!("=== STT Gate Corpus 录制 Prompt 清单 ===");
            println!("共 {} 条\n", prompts.len());
            for p in &prompts {
                println!("── {} ──", p.sample_id);
                println!("  场景: {}", p.scenario);
                println!(
                    "  文本: {}",
                    if p.reference_text.is_empty() {
                        "（无——负例）"
                    } else {
                        &p.reference_text
                    }
                );
                println!("  近讲/远场: {:?}", p.mic_distance);
                println!("  噪声: {:?}", p.noise_type);
                println!("  预期句子数: {}", p.expected_sentence_count);
                println!("  说明: {}", p.instructions);
                println!();
            }
            println!("=== 录制要求 ===");
            println!("  - 采样率: 16000 Hz");
            println!("  - 声道: 1（mono）");
            println!("  - 格式: PCM 16-bit WAV");
            println!("  - 文件名: 使用 sample_id.wav（如 near_short_01.wav）");
            println!("  - 保存到: corpus/wavs/ 目录");
            println!();
            println!("=== 录制命令示例 ===");
            println!("  # 使用 Audacity 录制，导出为 16kHz mono WAV");
            println!("  # 或使用 ffmpeg 从其他格式转换:");
            println!(
                "  ffmpeg -i input.wav -ar 16000 -ac 1 -sample_fmt s16 corpus/wavs/near_short_01.wav"
            );
            0
        }
        "annotation" => {
            print_annotation_template();
            0
        }
        "validate" => {
            let corpus_dir = match get_arg(args, "--corpus-dir") {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("缺少 --corpus-dir 参数");
                    return 1;
                }
            };
            match validate_corpus(&corpus_dir) {
                Ok(result) => {
                    if result.valid {
                        println!("✅ Corpus 校验通过");
                        println!("样本数: {}", result.sample_count);
                        println!("场景覆盖: {:?}", result.scenario_coverage);
                        if !result.warnings.is_empty() {
                            println!("\n⚠️ 警告:");
                            for w in &result.warnings {
                                println!("  - {w}");
                            }
                        }
                        0
                    } else {
                        println!("❌ Corpus 校验失败");
                        println!("\n错误:");
                        for e in &result.errors {
                            println!("  - {e}");
                        }
                        if !result.warnings.is_empty() {
                            println!("\n⚠️ 警告:");
                            for w in &result.warnings {
                                println!("  - {w}");
                            }
                        }
                        1
                    }
                }
                Err(e) => {
                    eprintln!("校验失败: {e}");
                    1
                }
            }
        }
        "summary" => {
            let corpus_dir = match get_arg(args, "--corpus-dir") {
                Some(d) => PathBuf::from(d),
                None => {
                    eprintln!("缺少 --corpus-dir 参数");
                    return 1;
                }
            };
            match generate_summary(&corpus_dir) {
                Ok(json) => {
                    println!("{json}");
                    0
                }
                Err(e) => {
                    eprintln!("生成 summary 失败: {e}");
                    1
                }
            }
        }
        other => {
            eprintln!("未知子命令: {other}");
            eprintln!(
                "用法: blink.exe stt-corpus <import|init|prompts|annotation|validate|summary> [options]"
            );
            1
        }
    }
}

fn get_arg(args: &[String], flag: &str) -> Option<String> {
    let mut i = 0;
    while i < args.len() {
        if args[i] == flag && i + 1 < args.len() {
            return Some(args[i + 1].clone());
        }
        i += 1;
    }
    None
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_serializes_correctly() {
        let manifest = generate_template_manifest();
        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed: CorpusManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.schema_version, 1);
        assert_eq!(parsed.samples.len(), 1);
        assert_eq!(parsed.samples[0].sample_id, "template_01");
        assert_eq!(parsed.samples[0].sample_rate, 16000);
        assert_eq!(parsed.samples[0].channels, 1);
    }

    #[test]
    fn mic_distance_serializes_as_snake_case() {
        let near = serde_json::to_string(&MicDistance::Near).unwrap();
        assert_eq!(near, "\"near\"");
        let far = serde_json::to_string(&MicDistance::Far).unwrap();
        assert_eq!(far, "\"far\"");
    }

    #[test]
    fn noise_type_serializes_as_snake_case() {
        let quiet = serde_json::to_string(&NoiseType::Quiet).unwrap();
        assert_eq!(quiet, "\"quiet\"");
        let keyboard = serde_json::to_string(&NoiseType::Keyboard).unwrap();
        assert_eq!(keyboard, "\"keyboard\"");
    }

    #[test]
    fn validate_empty_dir_returns_error() {
        let result = validate_corpus(Path::new("nonexistent_corpus_dir"));
        assert!(result.is_err());
    }

    #[test]
    fn validate_missing_manifest_returns_error() {
        let temp = tempfile::tempdir().unwrap();
        let result = validate_corpus(temp.path());
        assert!(result.is_err());
    }

    #[test]
    fn validate_minimal_manifest_passes() {
        let temp = tempfile::tempdir().unwrap();

        // 生成一个简单的 WAV（16kHz mono，1s 静音）
        let wav_path = temp.path().join("wavs").join("test_01.wav");
        std::fs::create_dir_all(wav_path.parent().unwrap()).unwrap();
        write_silence_wav(&wav_path, 16000, 1.0);

        let manifest = CorpusManifest {
            schema_version: 1,
            name: "test".to_string(),
            description: "test".to_string(),
            created_at: "2026-09-03T00:00:00Z".to_string(),
            samples: vec![CorpusSample {
                sample_id: "test_01".to_string(),
                wav_path: "wavs/test_01.wav".to_string(),
                reference_text: "这是一段测试文本。".to_string(),
                scenario: "near_short".to_string(),
                recording_device: "built_in_mic".to_string(),
                mic_distance: MicDistance::Near,
                noise_type: NoiseType::Quiet,
                expected_sentence_count: 1,
                sentence_boundaries: vec![(0.0, 1.0)],
                duration_s: 1.0,
                sample_rate: 16000,
                channels: 1,
                speaker_anon_id: None,
            }],
        };

        let manifest_path = temp.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = validate_corpus(temp.path()).unwrap();
        assert!(result.valid, "应有 0 错误, got: {:?}", result.errors);
        assert_eq!(result.sample_count, 1);
    }

    #[test]
    fn validate_duplicate_id_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let wav_path = temp.path().join("wavs").join("test_01.wav");
        std::fs::create_dir_all(wav_path.parent().unwrap()).unwrap();
        write_silence_wav(&wav_path, 16000, 1.0);

        let manifest = CorpusManifest {
            schema_version: 1,
            name: "test".to_string(),
            description: "test".to_string(),
            created_at: "2026-09-03T00:00:00Z".to_string(),
            samples: vec![
                CorpusSample {
                    sample_id: "dup".to_string(),
                    wav_path: "wavs/test_01.wav".to_string(),
                    reference_text: "文本1".to_string(),
                    scenario: "near_short".to_string(),
                    recording_device: "mic".to_string(),
                    mic_distance: MicDistance::Near,
                    noise_type: NoiseType::Quiet,
                    expected_sentence_count: 1,
                    sentence_boundaries: vec![],
                    duration_s: 1.0,
                    sample_rate: 16000,
                    channels: 1,
                    speaker_anon_id: None,
                },
                CorpusSample {
                    sample_id: "dup".to_string(),
                    wav_path: "wavs/test_01.wav".to_string(),
                    reference_text: "文本2".to_string(),
                    scenario: "near_long".to_string(),
                    recording_device: "mic".to_string(),
                    mic_distance: MicDistance::Near,
                    noise_type: NoiseType::Quiet,
                    expected_sentence_count: 1,
                    sentence_boundaries: vec![],
                    duration_s: 1.0,
                    sample_rate: 16000,
                    channels: 1,
                    speaker_anon_id: None,
                },
            ],
        };

        let manifest_path = temp.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = validate_corpus(temp.path()).unwrap();
        assert!(!result.valid, "重复 ID 应为错误");
        assert!(result.errors.iter().any(|e| e.contains("重复")));
    }

    #[test]
    fn validate_boundary_out_of_range_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let wav_path = temp.path().join("wavs").join("test_01.wav");
        std::fs::create_dir_all(wav_path.parent().unwrap()).unwrap();
        write_silence_wav(&wav_path, 16000, 1.0);

        let manifest = CorpusManifest {
            schema_version: 1,
            name: "test".to_string(),
            description: "test".to_string(),
            created_at: "2026-09-03T00:00:00Z".to_string(),
            samples: vec![CorpusSample {
                sample_id: "test_01".to_string(),
                wav_path: "wavs/test_01.wav".to_string(),
                reference_text: "测试".to_string(),
                scenario: "near_short".to_string(),
                recording_device: "mic".to_string(),
                mic_distance: MicDistance::Near,
                noise_type: NoiseType::Quiet,
                expected_sentence_count: 1,
                sentence_boundaries: vec![(0.0, 2.0)], // end > duration
                duration_s: 1.0,
                sample_rate: 16000,
                channels: 1,
                speaker_anon_id: None,
            }],
        };

        let manifest_path = temp.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let result = validate_corpus(temp.path()).unwrap();
        assert!(!result.valid, "句界越界应为错误");
    }

    #[test]
    fn summary_works() {
        let temp = tempfile::tempdir().unwrap();
        let wav_path = temp.path().join("wavs").join("test_01.wav");
        std::fs::create_dir_all(wav_path.parent().unwrap()).unwrap();
        write_silence_wav(&wav_path, 16000, 1.0);

        let manifest = generate_template_manifest();
        let manifest_path = temp.path().join("manifest.json");
        std::fs::write(
            &manifest_path,
            serde_json::to_string_pretty(&manifest).unwrap(),
        )
        .unwrap();

        let summary = generate_summary(temp.path()).unwrap();
        assert!(summary.contains("sample_count"));
        assert!(summary.contains("scenario_coverage"));
    }

    // ── helpers ──

    fn write_silence_wav(path: &Path, sample_rate: u32, duration_s: f64) {
        use std::io::Write;
        let n_samples = (sample_rate as f64 * duration_s) as usize;
        let data_size = n_samples * 2; // i16 = 2 bytes
        let mut file = std::fs::File::create(path).unwrap();
        // RIFF header
        file.write_all(b"RIFF").unwrap();
        file.write_all(&(36 + data_size as u32).to_le_bytes())
            .unwrap();
        file.write_all(b"WAVE").unwrap();
        // fmt chunk
        file.write_all(b"fmt ").unwrap();
        file.write_all(&16u32.to_le_bytes()).unwrap();
        file.write_all(&1u16.to_le_bytes()).unwrap(); // PCM
        file.write_all(&1u16.to_le_bytes()).unwrap(); // mono
        file.write_all(&sample_rate.to_le_bytes()).unwrap();
        file.write_all(&(sample_rate * 2).to_le_bytes()).unwrap(); // byte_rate
        file.write_all(&2u16.to_le_bytes()).unwrap(); // block_align
        file.write_all(&16u16.to_le_bytes()).unwrap(); // bits_per_sample
        // data chunk
        file.write_all(b"data").unwrap();
        file.write_all(&(data_size as u32).to_le_bytes()).unwrap();
        // silence (zeros)
        let zeros = vec![0u8; data_size];
        file.write_all(&zeros).unwrap();
    }

    /// 写一个 mock THCHS-30 条目（.wav + .trn）到指定目录。
    fn write_thchs30_mock(dataset_dir: &Path, stem: &str, transcript: &str) {
        let wav_path = dataset_dir.join(format!("{stem}.wav"));
        let trn_path = dataset_dir.join(format!("{stem}.trn"));
        write_silence_wav(&wav_path, 16000, 1.0);
        // .trn 格式：第一行空格分隔词语，第二行拼音
        let trn = format!("{transcript}\npin_yin_here\n");
        std::fs::write(&trn_path, trn).unwrap();
    }

    #[test]
    fn import_thchs30_creates_manifest() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        // 写 3 条 mock 数据
        write_thchs30_mock(dataset.path(), "A11_001", "东北军 的 一些 爱国 将士");
        write_thchs30_mock(dataset.path(), "A11_002", "今天 天气 很好");
        write_thchs30_mock(dataset.path(), "A11_003", "帮助 我 打开 浏览器");

        let n = import_thchs30(dataset.path(), corpus.path(), 50).unwrap();
        assert_eq!(n, 3);

        // manifest.json 应存在
        let manifest_path = corpus.path().join("manifest.json");
        assert!(manifest_path.exists());

        // 验证 manifest 内容
        let manifest: CorpusManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.samples.len(), 3);
        assert_eq!(manifest.samples[0].sample_id, "A11_001");
        // 转录文本应去除了空格
        assert_eq!(manifest.samples[0].reference_text, "东北军的一些爱国将士");
        assert_eq!(manifest.samples[0].sample_rate, 16000);
        assert_eq!(manifest.samples[0].channels, 1);
        assert_eq!(manifest.samples[0].scenario, "thchs30_read");

        // WAV 文件应被复制
        assert!(corpus.path().join("wavs/A11_001.wav").exists());

        // 校验应通过
        let result = validate_corpus(corpus.path()).unwrap();
        assert!(result.valid, "errors: {:?}", result.errors);
    }

    #[test]
    fn import_thchs30_respects_max_samples() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        write_thchs30_mock(dataset.path(), "A11_001", "测试 一");
        write_thchs30_mock(dataset.path(), "A11_002", "测试 二");
        write_thchs30_mock(dataset.path(), "A11_003", "测试 三");

        let n = import_thchs30(dataset.path(), corpus.path(), 2).unwrap();
        assert_eq!(n, 2);
    }

    #[test]
    fn import_thchs30_skips_non_16k() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        // 写一条 44.1kHz 的 WAV（应被跳过）
        let wav_path = dataset.path().join("bad_01.wav");
        write_silence_wav(&wav_path, 44100, 1.0);
        std::fs::write(dataset.path().join("bad_01.trn"), "无效\n").unwrap();

        // 写一条有效的
        write_thchs30_mock(dataset.path(), "good_01", "有效 测试");

        let n = import_thchs30(dataset.path(), corpus.path(), 50).unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn import_thchs30_empty_dir_is_error() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        let result = import_thchs30(dataset.path(), corpus.path(), 50);
        assert!(result.is_err());
    }

    // ── .lab 格式测试（HF 仓库结构）──

    /// 写一个 mock HF 仓库条目（.wav + .lab）到说话人子目录。
    fn write_hf_lab_mock(dataset_dir: &Path, speaker: &str, stem: &str, transcript: &str) {
        let sub_dir = dataset_dir.join(speaker);
        std::fs::create_dir_all(&sub_dir).unwrap();
        let wav_path = sub_dir.join(format!("{stem}.wav"));
        let lab_path = sub_dir.join(format!("{stem}.lab"));
        write_silence_wav(&wav_path, 16000, 1.0);
        // .lab 格式：整句纯文本
        std::fs::write(&lab_path, transcript).unwrap();
    }

    #[test]
    fn import_lab_format_from_subdirs() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        // 模拟 HF 仓库结构：dev/A34/A34_118.wav + .lab
        write_hf_lab_mock(dataset.path(), "A34", "A34_118", "碰上爱玩且能玩物成痴的人");
        write_hf_lab_mock(dataset.path(), "B2", "B2_268", "今天天气很好");
        write_hf_lab_mock(dataset.path(), "B2", "B2_275", "帮助我打开浏览器");

        let n = import_thchs30(dataset.path(), corpus.path(), 50).unwrap();
        assert_eq!(n, 3);

        let manifest_path = corpus.path().join("manifest.json");
        let manifest: CorpusManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();
        assert_eq!(manifest.samples.len(), 3);

        // .lab 是整句文本，直接使用（不去空格）
        assert_eq!(
            manifest.samples[0].reference_text,
            "碰上爱玩且能玩物成痴的人"
        );
        assert_eq!(manifest.samples[0].sample_id, "A34_118");

        // 说话人 ID 应从子目录提取
        assert_eq!(manifest.samples[0].speaker_anon_id, Some("A34".to_string()));
        assert_eq!(manifest.samples[1].speaker_anon_id, Some("B2".to_string()));

        // WAV 文件应被复制
        assert!(corpus.path().join("wavs/A34_118.wav").exists());
        assert!(corpus.path().join("wavs/B2_268.wav").exists());
        assert!(corpus.path().join("wavs/B2_275.wav").exists());

        // 校验应通过
        let result = validate_corpus(corpus.path()).unwrap();
        assert!(result.valid, "errors: {:?}", result.errors);
    }

    #[test]
    fn import_lab_and_trn_mixed() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        // 同一目录下混用 .trn 和 .lab
        write_thchs30_mock(dataset.path(), "trn_01", "空格 分隔 文本");
        write_hf_lab_mock(dataset.path(), "B2", "lab_01", "整句纯文本");

        let n = import_thchs30(dataset.path(), corpus.path(), 50).unwrap();
        assert_eq!(n, 2);

        let manifest_path = corpus.path().join("manifest.json");
        let manifest: CorpusManifest =
            serde_json::from_str(&std::fs::read_to_string(&manifest_path).unwrap()).unwrap();

        // 按样本 ID 查找（不依赖遍历顺序）
        let trn_sample = manifest
            .samples
            .iter()
            .find(|s| s.sample_id == "trn_01")
            .expect("应找到 trn_01");
        assert_eq!(trn_sample.reference_text, "空格分隔文本");

        let lab_sample = manifest
            .samples
            .iter()
            .find(|s| s.sample_id == "lab_01")
            .expect("应找到 lab_01");
        assert_eq!(lab_sample.reference_text, "整句纯文本");
    }

    #[test]
    fn import_lab_respects_max_samples() {
        let dataset = tempfile::tempdir().unwrap();
        let corpus = tempfile::tempdir().unwrap();

        write_hf_lab_mock(dataset.path(), "A34", "A34_001", "文本一");
        write_hf_lab_mock(dataset.path(), "A34", "A34_002", "文本二");
        write_hf_lab_mock(dataset.path(), "A34", "A34_003", "文本三");

        let n = import_thchs30(dataset.path(), corpus.path(), 2).unwrap();
        assert_eq!(n, 2);
    }
}
