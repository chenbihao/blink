//! WAV 来源格式描述与解码错误类型。
//!
//! 0.22.16 Handoff 01：正式 RIFF/WAVE decoder 的结构化输出。
//! decoder 只负责 `WAV bytes → 已验证的来源格式 + interleaved f32 frames`，
//! 不做 downmix、重采样或 STT 调用。

#![allow(dead_code)]

use std::fmt;

// ── 来源格式 ─────────────────────────────────────────────────────────────

/// WAV 样本的容器位深类型。
///
/// 对应 `fmt` chunk 中的 `wBitsPerSample` 和 `WAVE_FORMAT_EXTENSIBLE` 的
/// `SubFormat` GUID。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleKind {
    /// 有符号整数 PCM（16/24/32-bit）。
    PcmSigned,
    /// IEEE 754 单精度浮点（32-bit）。
    IeeeFloat,
}

/// 已验证的 WAV 来源格式摘要。
///
/// 由 decoder 在成功解析 `fmt` chunk 后产出，描述原始容器的真实参数。
/// 调用方据此决定后续规范化策略（downmix、重采样等）。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SourceFormat {
    /// 声道数。
    pub channels: u16,
    /// 采样率（Hz）。
    pub sample_rate: u32,
    /// 容器位深（bits per sample，即 `wBitsPerSample`）。
    pub bits_per_sample: u16,
    /// 有效位深（`WAVE_FORMAT_EXTENSIBLE` 的 `wValidBitsPerSample`）；
    /// standard fmt 中等于 `bits_per_sample`。
    pub valid_bits: u16,
    /// 样本类型。
    pub kind: SampleKind,
    /// `WAVE_FORMAT_EXTENSIBLE` 的 channel mask（如有）。
    pub channel_mask: Option<u32>,
}

impl SourceFormat {
    /// 每帧字节数（`block_align`）。
    pub fn block_align(&self) -> usize {
        (self.channels as usize) * (self.bits_per_sample as usize / 8)
    }

    /// 字节率（`byte_rate`）。
    pub fn byte_rate(&self) -> u32 {
        self.sample_rate * self.channels as u32 * (self.bits_per_sample / 8) as u32
    }
}

impl fmt::Display for SourceFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let kind_str = match self.kind {
            SampleKind::PcmSigned => "PCM",
            SampleKind::IeeeFloat => "float",
        };
        write!(
            f,
            "{}ch {}Hz {}-bit {}",
            self.channels, self.sample_rate, self.bits_per_sample, kind_str
        )?;
        if self.valid_bits != self.bits_per_sample {
            write!(f, " (valid {})", self.valid_bits)?;
        }
        if let Some(mask) = self.channel_mask {
            write!(f, " mask=0x{:X}", mask)?;
        }
        Ok(())
    }
}

// ── 解码错误 ─────────────────────────────────────────────────────────────

/// WAV 解码错误分类。
///
/// 不返回裸字符串——调用方可按 `kind` 分类处理（重试 / 提示用户 / 记录）。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AudioDecodeError {
    /// RIFF 魔数不匹配（非 WAV 文件或格式错误）。
    #[error("not a valid RIFF/WAVE file: {0}")]
    BadRiffMagic(String),

    /// RIFF 长度字段与实际数据不一致。
    #[error("RIFF size mismatch: declared {declared} bytes, actual {actual} bytes")]
    RiffSizeMismatch { declared: u32, actual: usize },

    /// `fmt` chunk 缺失或出现位置不合理。
    #[error("fmt chunk missing or not found before data: {0}")]
    FmtMissing(String),

    /// `fmt` chunk 尺寸不足（截断或畸形）。
    #[error("fmt chunk too small: {0} bytes (need >= 16)")]
    FmtTooSmall(usize),

    /// 不支持的音频格式（codec）。
    ///
    /// `audio_format` 为 `fmt` chunk 中的 `wFormatTag` 值。
    #[error("unsupported audio format: format_tag={audio_format}, {detail}")]
    UnsupportedCodec { audio_format: u16, detail: String },

    /// 不支持的位深。
    #[error("unsupported bits per sample: {0}")]
    UnsupportedBits(u16),

    /// 声道数不合法（零或超出预算）。
    #[error("invalid channel count: {0}")]
    InvalidChannels(u16),

    /// 采样率不合法。
    #[error("invalid sample rate: {0}")]
    InvalidSampleRate(u32),

    /// block_align 与 channels × bits 不一致。
    #[error(
        "block_align mismatch: declared {declared}, expected {expected} (ch={channels} bps={bits})"
    )]
    BlockAlignMismatch {
        declared: u16,
        expected: u16,
        channels: u16,
        bits: u16,
    },

    /// byte_rate 与 sample_rate × channels × bytes_per_sample 不一致。
    #[error("byte_rate mismatch: declared {declared}, expected {expected}")]
    ByteRateMismatch { declared: u32, expected: u32 },

    /// data chunk 缺失。
    #[error("data chunk not found")]
    DataMissing,

    /// data chunk 声明大小超出实际数据。
    #[error("data chunk truncated: declared {declared} bytes, available {available}")]
    DataTruncated { declared: u32, available: usize },

    /// 帧不对齐（data 大小不是 block_align 的整数倍）。
    #[error("data size {data_size} not aligned to block_align {block_align}")]
    FrameMisalignment {
        data_size: usize,
        block_align: usize,
    },

    /// 截断：文件/chunk header 不完整。
    #[error("truncated: {0}")]
    Truncated(String),

    /// float32 样本中遇到 NaN 或 Infinity。
    #[error("NaN or Infinity in float32 samples at offset {offset}")]
    NonFiniteFloat { offset: usize },

    /// 解码后样本数超出预算（防止解码炸弹）。
    #[error("decoded sample budget exceeded: {0} samples (limit {1})")]
    BudgetExceeded(usize, usize),

    /// 重复 chunk（第二个 `fmt` 或第二个 `data`）。
    #[error("duplicate {0} chunk")]
    DuplicateChunk(&'static str),

    /// `WAVE_FORMAT_EXTENSIBLE` 的 SubFormat GUID 不匹配已知类型。
    #[error("extensible SubFormat GUID is not PCM or IEEE float")]
    UnknownExtensibleSubFormat,

    /// 8-bit PCM 不受首版支持。
    #[error("8-bit PCM unsigned is not supported in this version")]
    Unsupported8BitPcm,

    /// RF64 / RIFX 等非标准 RIFF 变体不受支持。
    #[error("unsupported RIFF variant: {0}")]
    UnsupportedRiffVariant(String),

    /// 其他畸形输入。
    #[error("malformed WAV: {0}")]
    Malformed(String),
}
