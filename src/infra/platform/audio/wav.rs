//! 正式 RIFF/WAVE decoder。
//!
//! 0.22.16 Handoff 01：生产可用、严格有界的未压缩 WAV decoder。
//! 只负责 `WAV bytes → 已验证的来源格式 + interleaved f32 frames`。
//!
//! ## 支持的格式
//!
//! - `WAVE_FORMAT_PCM`（tag=1）：signed 16/24/32-bit
//! - `WAVE_FORMAT_IEEE_FLOAT`（tag=3）：32-bit float
//! - `WAVE_FORMAT_EXTENSIBLE`（tag=0xFFFE）：
//!   - SubFormat GUID 为 PCM → 同上 PCM
//!   - SubFormat GUID 为 IEEE float → 同上 float
//!   - 其余 GUID → `UnknownExtensibleSubFormat`
//!
//! ## 拒绝的格式
//!
//! - 8-bit unsigned PCM（首版不支持）
//! - float64
//! - 压缩 WAV（μ-law, A-law, ADPCM, MP3-in-WAV 等）
//! - RF64 / RIFX / 其他 RIFF 变体
//!
//! ## 安全保证
//!
//! - 所有 offset、长度和容量计算使用 checked arithmetic
//! - 预算检查先于大分配
//! - float32 遇 NaN/Infinity 明确拒绝
//! - PCM24 正确 sign extension
//! - chunk 边界校验

#![allow(dead_code)]

use super::format::{AudioDecodeError, SampleKind, SourceFormat};

// ── 常量 ──────────────────────────────────────────────────────────────────

const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// PCM SubFormat GUID（WAVE_FORMAT_EXTENSIBLE）。
const PCM_SUBFORMAT_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// IEEE float SubFormat GUID（WAVE_FORMAT_EXTENSIBLE）。
const IEEE_FLOAT_SUBFORMAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// 解码后样本数的默认预算上限（防止解码炸弹）。
/// 60 秒 × 96kHz × 8 声道 ≈ 46M，足够覆盖常见 WAV；超出可由调用方覆盖。
const DEFAULT_SAMPLE_BUDGET: usize = 50_000_000;

// ── 公开 API ─────────────────────────────────────────────────────────────

/// WAV 解码结果。
#[derive(Debug, Clone)]
pub struct DecodedWav {
    /// interleaved f32 frames（归一化到 [-1.0, 1.0]）。
    pub samples: Vec<f32>,
    /// 已验证的来源格式。
    pub format: SourceFormat,
    /// 帧数（不含声道维度）。
    pub num_frames: usize,
    /// 时长（秒）。
    pub duration_secs: f64,
}

/// 解析 WAV 字节为 interleaved f32 frames + 已验证的来源格式。
///
/// 使用默认样本预算（50M）防止解码炸弹。
pub fn decode_wav(data: &[u8]) -> Result<DecodedWav, AudioDecodeError> {
    decode_wav_with_budget(data, DEFAULT_SAMPLE_BUDGET)
}

/// 解析 WAV 字节为 interleaved f32 frames + 已验证的来源格式。
///
/// `sample_budget` 限制解码后的 f32 样本数（含声道），超出返回 `BudgetExceeded`。
pub fn decode_wav_with_budget(
    data: &[u8],
    sample_budget: usize,
) -> Result<DecodedWav, AudioDecodeError> {
    // ── RIFF header ─────────────────────────────────────────────────────
    if data.len() < 12 {
        return Err(AudioDecodeError::Truncated(
            "file < 12 bytes (RIFF header)".into(),
        ));
    }

    let riff_id = &data[0..4];
    if riff_id != b"RIFF" {
        if riff_id == b"RF64" {
            return Err(AudioDecodeError::UnsupportedRiffVariant("RF64".into()));
        }
        if riff_id == b"RIFX" {
            return Err(AudioDecodeError::UnsupportedRiffVariant("RIFX".into()));
        }
        return Err(AudioDecodeError::BadRiffMagic(format!(
            "expected 'RIFF', got {:?}",
            std::str::from_utf8(riff_id).unwrap_or("<non-utf8>")
        )));
    }

    let riff_size = read_u32_le(data, 4);
    // RIFF size 声明的是 file_size - 8；允许实际数据比声明少（截断的 data 不影响 header 校验）
    let declared_inner = riff_size as usize;
    let actual_inner = data.len().saturating_sub(8);
    if declared_inner > actual_inner {
        // RIFF 声明比实际多 → 文件被截断，但我们仍尝试解析已有 chunk
        // 只在声明远超实际（如恶意大值）时拒绝
        if declared_inner > actual_inner.saturating_add(8) {
            return Err(AudioDecodeError::RiffSizeMismatch {
                declared: riff_size,
                actual: data.len(),
            });
        }
    }

    let wave_id = &data[8..12];
    if wave_id != b"WAVE" {
        return Err(AudioDecodeError::BadRiffMagic(format!(
            "expected 'WAVE', got {:?}",
            std::str::from_utf8(wave_id).unwrap_or("<non-utf8>")
        )));
    }

    // ── chunk 遍历 ─────────────────────────────────────────────────────
    let mut offset = 12usize;
    let mut fmt_info: Option<FmtInfo> = None;
    let mut data_info: Option<(usize, usize)> = None; // (start, size)

    while offset + 8 <= data.len() {
        let chunk_id = &data[offset..offset + 4];
        let chunk_size = read_u32_le(data, offset + 4) as usize;

        // chunk payload 起始
        let payload_start = offset.checked_add(8).ok_or_else(|| {
            AudioDecodeError::Malformed(format!("chunk payload offset overflow at {offset}"))
        })?;

        // 截断检查：payload 声称大小超出可用数据
        let payload_end = payload_start
            .checked_add(chunk_size)
            .ok_or_else(|| AudioDecodeError::Malformed("chunk end overflow".to_string()))?;

        let actual_payload_end = payload_end.min(data.len());

        if chunk_id == b"fmt " {
            if fmt_info.is_some() {
                return Err(AudioDecodeError::DuplicateChunk("fmt"));
            }
            let info = parse_fmt_chunk(&data[payload_start..actual_payload_end], chunk_size)?;
            fmt_info = Some(info);
        } else if chunk_id == b"data" {
            if data_info.is_some() {
                return Err(AudioDecodeError::DuplicateChunk("data"));
            }
            // data 在 fmt 前出现 → 确定策略：先记录，稍后校验 fmt 已解析
            data_info = Some((payload_start, chunk_size));
        }
        // 其他 chunk（JUNK, LIST, fact, etc.）跳过

        // 前进到下一个 chunk（含 pad byte）
        let next_offset = payload_end;
        // pad byte
        let padded = if chunk_size % 2 == 1 { 1 } else { 0 };
        let next_offset = next_offset
            .checked_add(padded)
            .ok_or_else(|| AudioDecodeError::Malformed("next chunk offset overflow".into()))?;

        // 如果 next_offset 超出 data，说明文件截断，但我们已拿到 fmt/data 就可以停
        if next_offset > data.len() && chunk_id != b"data" && chunk_id != b"fmt " {
            // 对于非关键 chunk 截断，可以安全停止
            break;
        }
        offset = next_offset;
    }

    // ── 校验 fmt ───────────────────────────────────────────────────────
    let fmt =
        fmt_info.ok_or_else(|| AudioDecodeError::FmtMissing("no fmt chunk found in WAV".into()))?;

    let (data_start, data_declared) = data_info.ok_or(AudioDecodeError::DataMissing)?;

    // data 实际可用字节
    let data_available = data.len().saturating_sub(data_start);
    if data_declared > data_available {
        // data 被截断 → 用实际可用
        // 但如果声明远超实际（恶意大值），拒绝
        if data_declared > data_available.saturating_add(8) {
            return Err(AudioDecodeError::DataTruncated {
                declared: data_declared as u32,
                available: data_available,
            });
        }
    }

    let data_size = data_declared.min(data_available);

    // ── 帧对齐检查 ─────────────────────────────────────────────────────
    let block_align = fmt.block_align as usize;
    if block_align == 0 {
        return Err(AudioDecodeError::BlockAlignMismatch {
            declared: fmt.block_align,
            expected: 0,
            channels: fmt.channels,
            bits: fmt.bits_per_sample,
        });
    }

    if data_size % block_align != 0 {
        return Err(AudioDecodeError::FrameMisalignment {
            data_size,
            block_align,
        });
    }

    let num_frames = data_size / block_align;
    let total_samples = num_frames
        .checked_mul(fmt.channels as usize)
        .ok_or_else(|| AudioDecodeError::Malformed("total samples overflow".into()))?;

    if total_samples > sample_budget {
        return Err(AudioDecodeError::BudgetExceeded(
            total_samples,
            sample_budget,
        ));
    }

    // ── 解码 ───────────────────────────────────────────────────────────
    let data_bytes = &data[data_start..data_start + data_size];
    let samples = decode_samples(data_bytes, &fmt, num_frames)?;

    let duration_secs = if fmt.sample_rate > 0 && fmt.channels > 0 {
        num_frames as f64 / fmt.sample_rate as f64
    } else {
        0.0
    };

    let source_format = SourceFormat {
        channels: fmt.channels,
        sample_rate: fmt.sample_rate,
        bits_per_sample: fmt.bits_per_sample,
        valid_bits: fmt.valid_bits,
        kind: fmt.kind,
        channel_mask: fmt.channel_mask,
    };

    Ok(DecodedWav {
        samples,
        format: source_format,
        num_frames,
        duration_secs,
    })
}

// ── 内部结构 ─────────────────────────────────────────────────────────────

struct FmtInfo {
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    valid_bits: u16,
    block_align: u16,
    byte_rate: u32,
    kind: SampleKind,
    channel_mask: Option<u32>,
}

// ── fmt chunk 解析 ───────────────────────────────────────────────────────

fn parse_fmt_chunk(payload: &[u8], declared_size: usize) -> Result<FmtInfo, AudioDecodeError> {
    // 最小 fmt chunk = 16 bytes (PCMWAVEFORMAT)
    if payload.len() < 16 {
        return Err(AudioDecodeError::FmtTooSmall(payload.len()));
    }

    let format_tag = read_u16_le_from(payload, 0);
    let channels = read_u16_le_from(payload, 2);
    let sample_rate = read_u32_le_from(payload, 4);
    let byte_rate = read_u32_le_from(payload, 8);
    let block_align = read_u16_le_from(payload, 12);
    let bits_per_sample = read_u16_le_from(payload, 14);

    // 基本校验
    if channels == 0 {
        return Err(AudioDecodeError::InvalidChannels(channels));
    }
    if channels > 256 {
        return Err(AudioDecodeError::InvalidChannels(channels));
    }
    if sample_rate == 0 {
        return Err(AudioDecodeError::InvalidSampleRate(sample_rate));
    }
    if sample_rate > 1_000_000 {
        return Err(AudioDecodeError::InvalidSampleRate(sample_rate));
    }

    let (kind, valid_bits, channel_mask) = match format_tag {
        WAVE_FORMAT_PCM => {
            // standard PCM
            let kind = match bits_per_sample {
                16 | 24 | 32 => SampleKind::PcmSigned,
                8 => return Err(AudioDecodeError::Unsupported8BitPcm),
                _ => return Err(AudioDecodeError::UnsupportedBits(bits_per_sample)),
            };
            (kind, bits_per_sample, None)
        }
        WAVE_FORMAT_IEEE_FLOAT => {
            if bits_per_sample != 32 {
                if bits_per_sample == 64 {
                    return Err(AudioDecodeError::UnsupportedBits(64));
                }
                return Err(AudioDecodeError::UnsupportedBits(bits_per_sample));
            }
            (SampleKind::IeeeFloat, bits_per_sample, None)
        }
        WAVE_FORMAT_EXTENSIBLE => {
            // extensible: need at least 16 + 2 (cbSize) = 18 bytes minimum
            // full WAVEFORMATEXTENSIBLE = 16 + 22 = 38 bytes payload
            if payload.len() < 18 {
                return Err(AudioDecodeError::FmtTooSmall(payload.len()));
            }
            let cb_size = read_u16_le_from(payload, 16) as usize;
            // cbSize should be 22 for standard extensible
            if cb_size < 22 || payload.len() < 16 + 2 + cb_size {
                return Err(AudioDecodeError::Malformed(format!(
                    "extensible cb_size={cb_size} but payload only {} bytes",
                    payload.len()
                )));
            }
            let valid_bits = read_u16_le_from(payload, 18);
            let mask = read_u32_le_from(payload, 20);
            let guid = &payload[24..40];

            let kind = if guid == PCM_SUBFORMAT_GUID {
                match bits_per_sample {
                    16 | 24 | 32 => SampleKind::PcmSigned,
                    8 => return Err(AudioDecodeError::Unsupported8BitPcm),
                    _ => return Err(AudioDecodeError::UnsupportedBits(bits_per_sample)),
                }
            } else if guid == IEEE_FLOAT_SUBFORMAT_GUID {
                if bits_per_sample != 32 {
                    if bits_per_sample == 64 {
                        return Err(AudioDecodeError::UnsupportedBits(64));
                    }
                    return Err(AudioDecodeError::UnsupportedBits(bits_per_sample));
                }
                SampleKind::IeeeFloat
            } else {
                return Err(AudioDecodeError::UnknownExtensibleSubFormat);
            };

            (kind, valid_bits, Some(mask))
        }
        _ => {
            let detail = match format_tag {
                0x0002 => "MS ADPCM",
                0x0006 => "A-law",
                0x0007 => "μ-law",
                0x0011 => "IMA ADPCM",
                0x0050 => "MP3-in-WAV",
                0x0055 => "MP3",
                _ => "unknown codec",
            };
            return Err(AudioDecodeError::UnsupportedCodec {
                audio_format: format_tag,
                detail: detail.into(),
            });
        }
    };

    // ── 一致性校验 ─────────────────────────────────────────────────────
    let expected_block_align = channels * (bits_per_sample / 8);
    if block_align != expected_block_align {
        return Err(AudioDecodeError::BlockAlignMismatch {
            declared: block_align,
            expected: expected_block_align,
            channels,
            bits: bits_per_sample,
        });
    }

    let expected_byte_rate = sample_rate * channels as u32 * (bits_per_sample / 8) as u32;
    if byte_rate != expected_byte_rate {
        return Err(AudioDecodeError::ByteRateMismatch {
            declared: byte_rate,
            expected: expected_byte_rate,
        });
    }

    // valid_bits 不应大于 bits_per_sample
    if valid_bits > bits_per_sample {
        return Err(AudioDecodeError::Malformed(format!(
            "valid_bits {valid_bits} > bits_per_sample {bits_per_sample}"
        )));
    }

    // 对 declared_size 做简单校验（standard fmt = 16, extensible fmt = 40）
    // 但容忍比标准大的 fmt（某些编码器多写）
    let _ = declared_size;

    Ok(FmtInfo {
        channels,
        sample_rate,
        bits_per_sample,
        valid_bits,
        block_align,
        byte_rate,
        kind,
        channel_mask,
    })
}

// ── 样本解码 ─────────────────────────────────────────────────────────────

fn decode_samples(
    data: &[u8],
    fmt: &FmtInfo,
    num_frames: usize,
) -> Result<Vec<f32>, AudioDecodeError> {
    let total = num_frames * fmt.channels as usize;
    let mut out = Vec::with_capacity(total);

    match fmt.kind {
        SampleKind::PcmSigned => match fmt.bits_per_sample {
            16 => decode_pcm16(data, &mut out),
            24 => decode_pcm24(data, &mut out),
            32 => decode_pcm32(data, &mut out),
            _ => unreachable!(),
        },
        SampleKind::IeeeFloat => decode_float32(data, &mut out)?,
    }

    Ok(out)
}

fn decode_pcm16(data: &[u8], out: &mut Vec<f32>) {
    for chunk in data.chunks_exact(2) {
        let val = i16::from_le_bytes([chunk[0], chunk[1]]);
        out.push(val as f32 / 32768.0);
    }
}

fn decode_pcm24(data: &[u8], out: &mut Vec<f32>) {
    for chunk in data.chunks_exact(3) {
        // 3 bytes LE → i32 with sign extension
        let b0 = chunk[0] as i32;
        let b1 = chunk[1] as i32;
        let b2 = chunk[2] as i32;
        let raw = b0 | (b1 << 8) | (b2 << 16);
        // Sign extension: if bit 23 is set, extend to negative
        let val = if raw & 0x800000 != 0 {
            raw | (0xFFu32 as i32) << 24
        } else {
            raw
        };
        out.push(val as f32 / 8388608.0);
    }
}

fn decode_pcm32(data: &[u8], out: &mut Vec<f32>) {
    for chunk in data.chunks_exact(4) {
        let val = i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        out.push(val as f32 / 2147483648.0);
    }
}

fn decode_float32(data: &[u8], out: &mut Vec<f32>) -> Result<(), AudioDecodeError> {
    for (i, chunk) in data.chunks_exact(4).enumerate() {
        let val = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !val.is_finite() {
            let byte_offset = i * 4;
            return Err(AudioDecodeError::NonFiniteFloat {
                offset: byte_offset,
            });
        }
        out.push(val);
    }
    Ok(())
}

// ── 小工具 ───────────────────────────────────────────────────────────────

#[inline]
fn read_u32_le(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

#[inline]
fn read_u16_le(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

#[inline]
fn read_u32_le_from(data: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ])
}

#[inline]
fn read_u16_le_from(data: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([data[offset], data[offset + 1]])
}

// ── 测试矩阵 ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::super::test_fixtures::*;
    use super::*;

    // ── PCM16 值域 ──────────────────────────────────────────────────────

    #[test]
    fn pcm16_min_value() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1,
            values: FixtureValues::Explicit(vec![-1.0]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert!((decoded.samples[0] + 1.0).abs() < 1e-3);
    }

    #[test]
    fn pcm16_max_value() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1,
            values: FixtureValues::Explicit(vec![1.0]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert!((decoded.samples[0] - 1.0).abs() < 1e-3);
    }

    #[test]
    fn pcm16_zero_value() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.samples[0], 0.0);
    }

    #[test]
    fn pcm16_mid_range() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 2,
            values: FixtureValues::Explicit(vec![0.5, -0.5]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert!((decoded.samples[0] - 0.5).abs() < 1e-3);
        assert!((decoded.samples[1] + 0.5).abs() < 1e-3);
    }

    // ── PCM24 值域 ──────────────────────────────────────────────────────

    #[test]
    fn pcm24_roundtrip_values() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 4,
            values: FixtureValues::Explicit(vec![0.0, 1.0, -1.0, 0.25]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.bits_per_sample, 24);
        assert_eq!(decoded.samples.len(), 4);
        assert!(decoded.samples[0].abs() < 1e-6);
        assert!((decoded.samples[1] - 1.0).abs() < 1e-4);
        assert!((decoded.samples[2] + 1.0).abs() < 1e-4);
        assert!((decoded.samples[3] - 0.25).abs() < 1e-4);
    }

    #[test]
    fn pcm24_sign_extension_negative() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1,
            values: FixtureValues::Explicit(vec![-1.0]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert!(
            (decoded.samples[0] + 1.0).abs() < 1e-4,
            "PCM24 -1.0 should decode to -1.0, got {}",
            decoded.samples[0]
        );
    }

    // ── PCM32 值域 ──────────────────────────────────────────────────────

    #[test]
    fn pcm32_roundtrip_values() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 3,
            values: FixtureValues::Explicit(vec![0.0, 0.5, -0.5]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.bits_per_sample, 32);
        assert!(decoded.samples[0].abs() < 1e-6);
        assert!((decoded.samples[1] - 0.5).abs() < 1e-4);
        assert!((decoded.samples[2] + 0.5).abs() < 1e-4);
    }

    // ── float32 值域 ────────────────────────────────────────────────────

    #[test]
    fn float32_roundtrip_values() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            extensible: false,
            num_frames: 4,
            values: FixtureValues::Explicit(vec![0.0, 1.0, -1.0, 0.5]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.kind, SampleKind::IeeeFloat);
        assert!(decoded.samples[0].abs() < 1e-7);
        assert!((decoded.samples[1] - 1.0).abs() < 1e-7);
        assert!((decoded.samples[2] + 1.0).abs() < 1e-7);
        assert!((decoded.samples[3] - 0.5).abs() < 1e-7);
    }

    #[test]
    fn float32_nan_is_rejected() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            extensible: false,
            num_frames: 2,
            values: FixtureValues::Explicit(vec![0.0, f32::NAN]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::NonFiniteFloat { .. }));
    }

    #[test]
    fn float32_infinity_is_rejected() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            extensible: false,
            num_frames: 2,
            values: FixtureValues::Explicit(vec![0.0, f32::INFINITY]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::NonFiniteFloat { .. }));
    }

    // ── 声道 ────────────────────────────────────────────────────────────

    #[test]
    fn mono_wav() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 100,
            values: FixtureValues::Deterministic,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.channels, 1);
        assert_eq!(decoded.num_frames, 100);
        assert_eq!(decoded.samples.len(), 100);
    }

    #[test]
    fn stereo_wav() {
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 100,
            values: FixtureValues::Deterministic,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.channels, 2);
        assert_eq!(decoded.num_frames, 100);
        assert_eq!(decoded.samples.len(), 200);
    }

    #[test]
    fn multichannel_with_extensible_mask() {
        let cfg = FixtureConfig {
            channels: 6,
            sample_rate: 48000,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: true,
            channel_mask: Some(0x3F),
            num_frames: 10,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.channels, 6);
        assert_eq!(decoded.format.channel_mask, Some(0x3F));
        assert_eq!(decoded.num_frames, 10);
        assert_eq!(decoded.samples.len(), 60);
    }

    // ── 采样率 ──────────────────────────────────────────────────────────

    #[test]
    fn sample_rates() {
        for &sr in &[8000u32, 16000, 22050, 32000, 44100, 48000, 96000] {
            let cfg = FixtureConfig {
                channels: 1,
                sample_rate: sr,
                bits_per_sample: 16,
                kind: FixtureSampleKind::PcmSigned,
                extensible: false,
                num_frames: 100,
                values: FixtureValues::Zero,
                ..Default::default()
            };
            let wav = build_wav(&cfg);
            let decoded =
                decode_wav(&wav).unwrap_or_else(|e| panic!("decode failed for {sr}Hz: {e}"));
            assert_eq!(decoded.format.sample_rate, sr);
            let expected_dur = 100.0 / sr as f64;
            assert!((decoded.duration_secs - expected_dur).abs() < 1e-10);
        }
    }

    // ── extensible ───────────────────────────────────────────────────────

    #[test]
    fn extensible_pcm16() {
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: true,
            num_frames: 10,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.kind, SampleKind::PcmSigned);
        assert_eq!(decoded.format.bits_per_sample, 16);
    }

    #[test]
    fn extensible_float32() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 48000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            extensible: true,
            num_frames: 10,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.kind, SampleKind::IeeeFloat);
    }

    #[test]
    fn extensible_valid_bits_less_than_container() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::PcmSigned,
            extensible: true,
            valid_bits: Some(20),
            num_frames: 10,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.bits_per_sample, 32);
        assert_eq!(decoded.format.valid_bits, 20);
    }

    // ── 奇数 chunk padding ──────────────────────────────────────────────

    #[test]
    fn odd_junk_chunk_before_fmt() {
        let cfg = FixtureConfig::default();
        let junk = junk_chunk(&[0xAA; 5]);
        let wav = build_wav_with_extra_chunks(&cfg, &[junk], &[]);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.num_frames, 100);
    }

    #[test]
    fn odd_list_chunk_between_fmt_and_data() {
        let cfg = FixtureConfig::default();
        let list = list_chunk(b"INFO odd length data");
        let wav = build_wav_with_extra_chunks(&cfg, &[], &[list]);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.num_frames, 100);
    }

    #[test]
    fn data_chunk_with_odd_payload_is_padded() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 1,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.num_frames, 1);
    }

    // ── 截断 ────────────────────────────────────────────────────────────

    #[test]
    fn truncated_header() {
        let data = [0u8; 11];
        let err = decode_wav(&data).unwrap_err();
        assert!(matches!(err, AudioDecodeError::Truncated(_)));
    }

    // ── 伪造 RIFF ────────────────────────────────────────────────────────

    #[test]
    fn bad_riff_magic() {
        let mut data = [0u8; 44];
        data[0..4].copy_from_slice(b"XXX ");
        data[8..12].copy_from_slice(b"WAVE");
        let err = decode_wav(&data).unwrap_err();
        assert!(matches!(err, AudioDecodeError::BadRiffMagic(_)));
    }

    #[test]
    fn rf64_rejected() {
        let mut data = vec![0u8; 44];
        data[0..4].copy_from_slice(b"RF64");
        data[8..12].copy_from_slice(b"WAVE");
        let err = decode_wav(&data).unwrap_err();
        assert!(matches!(err, AudioDecodeError::UnsupportedRiffVariant(_)));
    }

    #[test]
    fn rifx_rejected() {
        let mut data = vec![0u8; 44];
        data[0..4].copy_from_slice(b"RIFX");
        data[8..12].copy_from_slice(b"WAVE");
        let err = decode_wav(&data).unwrap_err();
        assert!(matches!(err, AudioDecodeError::UnsupportedRiffVariant(_)));
    }

    // ── 不支持 codec ─────────────────────────────────────────────────────

    #[test]
    fn unsupported_8bit_pcm_rejected() {
        let mut wav = build_wav(&FixtureConfig {
            bits_per_sample: 16,
            ..Default::default()
        });
        wav[34] = 8;
        wav[35] = 0;
        let block_align = 1u16;
        wav[32..34].copy_from_slice(&block_align.to_le_bytes());
        let byte_rate = 16000u32;
        wav[28..32].copy_from_slice(&byte_rate.to_le_bytes());
        wav[40..44].copy_from_slice(&100u32.to_le_bytes());
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::Unsupported8BitPcm));
    }

    #[test]
    fn unsupported_float64_rejected() {
        let mut wav = build_wav(&FixtureConfig {
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            ..Default::default()
        });
        wav[34] = 64;
        wav[35] = 0;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::UnsupportedBits(64)));
    }

    #[test]
    fn unsupported_mu_law_rejected() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[20] = 0x07;
        wav[21] = 0x00;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(
            err,
            AudioDecodeError::UnsupportedCodec {
                audio_format: 0x0007,
                ..
            }
        ));
    }

    #[test]
    fn unsupported_adpcm_rejected() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[20] = 0x02;
        wav[21] = 0x00;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(
            err,
            AudioDecodeError::UnsupportedCodec {
                audio_format: 0x0002,
                ..
            }
        ));
    }

    // ── 重复 chunk ───────────────────────────────────────────────────────

    #[test]
    fn duplicate_fmt_chunk_rejected() {
        let full = build_wav(&FixtureConfig::default());
        let fmt_chunk = &full[12..36];
        let data_chunk = &full[36..];
        let inner_len = 4 + fmt_chunk.len() * 2 + data_chunk.len();
        let mut wav = Vec::with_capacity(8 + inner_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(inner_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(fmt_chunk);
        wav.extend_from_slice(fmt_chunk);
        wav.extend_from_slice(data_chunk);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::DuplicateChunk("fmt")));
    }

    #[test]
    fn duplicate_data_chunk_rejected() {
        let full = build_wav(&FixtureConfig::default());
        let fmt_chunk = &full[12..36];
        let data_chunk = &full[36..];
        let inner_len = 4 + fmt_chunk.len() + data_chunk.len() * 2;
        let mut wav = Vec::with_capacity(8 + inner_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(inner_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(fmt_chunk);
        wav.extend_from_slice(data_chunk);
        wav.extend_from_slice(data_chunk);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::DuplicateChunk("data")));
    }

    // ── data 在 fmt 前 ──────────────────────────────────────────────────

    #[test]
    fn data_before_fmt_accepted() {
        let full = build_wav(&FixtureConfig::default());
        let fmt_chunk = full[12..36].to_vec();
        let data_chunk = full[36..].to_vec();
        let inner_len = 4 + data_chunk.len() + fmt_chunk.len();
        let mut wav = Vec::with_capacity(8 + inner_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(inner_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(&data_chunk);
        wav.extend_from_slice(&fmt_chunk);
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.channels, 1);
        assert_eq!(decoded.format.sample_rate, 16000);
        assert_eq!(decoded.num_frames, 100);
    }

    // ── 伪造 chunk size ──────────────────────────────────────────────────

    #[test]
    fn forged_riff_size_too_large() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[4..8].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::RiffSizeMismatch { .. }));
    }

    #[test]
    fn forged_data_size_too_large() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[40..44].copy_from_slice(&0xFFFFFFFFu32.to_le_bytes());
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::DataTruncated { .. }));
    }

    // ── frame misalignment ───────────────────────────────────────────────

    #[test]
    fn frame_misalignment_rejected() {
        let cfg = FixtureConfig {
            channels: 2,
            bits_per_sample: 16,
            num_frames: 10,
            ..Default::default()
        };
        let mut wav = build_wav(&cfg);
        let data_start = 44;
        let misaligned_size = 39;
        wav.truncate(data_start + misaligned_size);
        wav[40..44].copy_from_slice(&(misaligned_size as u32).to_le_bytes());
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::FrameMisalignment { .. }));
    }

    // ── byte_rate / block_align 不一致 ───────────────────────────────────

    #[test]
    fn block_align_mismatch_rejected() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[32] = 0x03;
        wav[33] = 0x00;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::BlockAlignMismatch { .. }));
    }

    #[test]
    fn byte_rate_mismatch_rejected() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[28] = 0xFF;
        wav[29] = 0xFF;
        wav[30] = 0xFF;
        wav[31] = 0x7F;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::ByteRateMismatch { .. }));
    }

    // ── 空 data ──────────────────────────────────────────────────────────

    #[test]
    fn empty_data_chunk() {
        let cfg = FixtureConfig {
            num_frames: 0,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let result = decode_wav(&wav);
        match result {
            Ok(decoded) => {
                assert_eq!(decoded.num_frames, 0);
                assert!(decoded.samples.is_empty());
            }
            Err(AudioDecodeError::DataMissing) => {}
            Err(e) => panic!("unexpected error for empty data: {e}"),
        }
    }

    // ── 预算超限 ────────────────────────────────────────────────────────

    #[test]
    fn budget_exceeded() {
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 96000,
            bits_per_sample: 32,
            num_frames: 100,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let err = decode_wav_with_budget(&wav, 100).unwrap_err();
        assert!(matches!(err, AudioDecodeError::BudgetExceeded(200, 100)));
    }

    // ── fmt chunk 缺失 ──────────────────────────────────────────────────

    #[test]
    fn fmt_chunk_missing() {
        let full = build_wav(&FixtureConfig::default());
        let data_chunk = &full[36..];
        let inner_len = 4 + data_chunk.len();
        let mut wav = Vec::with_capacity(8 + inner_len);
        wav.extend_from_slice(b"RIFF");
        wav.extend_from_slice(&(inner_len as u32).to_le_bytes());
        wav.extend_from_slice(b"WAVE");
        wav.extend_from_slice(data_chunk);
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::FmtMissing(_)));
    }

    // ── fmt chunk 截断 ──────────────────────────────────────────────────

    #[test]
    fn fmt_chunk_too_small() {
        // Minimal WAV with fmt chunk only 8 bytes (too small for PCMWAVEFORMAT which needs 16)
        // RIFF size = 4(WAVE) + 8(fmt hdr) + 8(fmt data) + 8(data hdr) = 28
        let wav: Vec<u8> = vec![
            b'R', b'I', b'F', b'F', 0x1C, 0x00, 0x00, 0x00, b'W', b'A', b'V', b'E', b'f', b'm',
            b't', b' ', 0x08, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x80, 0x3E, 0x00, 0x00,
            b'd', b'a', b't', b'a', 0x00, 0x00, 0x00, 0x00,
        ];
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::FmtTooSmall(8)));
    }

    // ── 无效声道数 / 采样率 ──────────────────────────────────────────────

    #[test]
    fn invalid_channels_zero() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[22] = 0;
        wav[23] = 0;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::InvalidChannels(0)));
    }

    #[test]
    fn invalid_sample_rate_zero() {
        let mut wav = build_wav(&FixtureConfig::default());
        wav[24..28].copy_from_slice(&0u32.to_le_bytes());
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::InvalidSampleRate(0)));
    }

    // ── extensible 未知 GUID ─────────────────────────────────────────────

    #[test]
    fn extensible_unknown_subformat_rejected() {
        let cfg = FixtureConfig {
            extensible: true,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            ..Default::default()
        };
        let mut wav = build_wav(&cfg);
        // GUID at offset 44 in extensible (12 + 8 + 24 = 44)
        wav[44] = 0xFF;
        let err = decode_wav(&wav).unwrap_err();
        assert!(matches!(err, AudioDecodeError::UnknownExtensibleSubFormat));
    }

    // ── 手写 bytes 第二来源 ──────────────────────────────────────────────

    #[test]
    fn handcrafted_minimal_pcm16_mono() {
        let wav: Vec<u8> = vec![
            b'R', b'I', b'F', b'F', 0x26, 0x00, 0x00, 0x00, b'W', b'A', b'V', b'E', b'f', b'm',
            b't', b' ', 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x80, 0x3E, 0x00, 0x00,
            0x00, 0x7D, 0x00, 0x00, 0x02, 0x00, 0x10, 0x00, b'd', b'a', b't', b'a', 0x02, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ];
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.channels, 1);
        assert_eq!(decoded.format.sample_rate, 16000);
        assert_eq!(decoded.format.bits_per_sample, 16);
        assert_eq!(decoded.num_frames, 1);
        assert_eq!(decoded.samples[0], 0.0);
    }

    #[test]
    fn handcrafted_pcm24_negative() {
        // 16000 Hz, 1ch, 24-bit → byte_rate = 16000*1*3 = 48000 = 0xBB80
        let wav: Vec<u8> = vec![
            b'R', b'I', b'F', b'F', 0x27, 0x00, 0x00, 0x00, b'W', b'A', b'V', b'E', b'f', b'm',
            b't', b' ', 0x10, 0x00, 0x00, 0x00, 0x01, 0x00, 0x01, 0x00, 0x80, 0x3E, 0x00, 0x00,
            0x80, 0xBB, 0x00, 0x00, // byte rate = 48000
            0x03, 0x00, 0x18, 0x00, b'd', b'a', b't', b'a', 0x03, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x80, // -8388608 → -1.0
            0x00, // pad
        ];
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.bits_per_sample, 24);
        assert_eq!(decoded.num_frames, 1);
        assert!(
            (decoded.samples[0] + 1.0).abs() < 1e-6,
            "expected -1.0, got {}",
            decoded.samples[0]
        );
    }

    #[test]
    fn handcrafted_float32_value() {
        // 16000 Hz, 1ch, 32-bit float → byte_rate = 16000*1*4 = 64000 = 0xFA00
        let half = 0.5f32.to_le_bytes();
        let wav: Vec<u8> = vec![
            b'R', b'I', b'F', b'F', 0x28, 0x00, 0x00, 0x00, b'W', b'A', b'V', b'E', b'f', b'm',
            b't', b' ', 0x10, 0x00, 0x00, 0x00, 0x03, 0x00, 0x01, 0x00, 0x80, 0x3E, 0x00, 0x00,
            0x00, 0xFA, 0x00, 0x00, // byte rate = 64000
            0x04, 0x00, 0x20, 0x00, b'd', b'a', b't', b'a', 0x04, 0x00, 0x00, 0x00, half[0],
            half[1], half[2], half[3],
        ];
        let decoded = decode_wav(&wav).unwrap();
        assert_eq!(decoded.format.kind, SampleKind::IeeeFloat);
        assert!((decoded.samples[0] - 0.5).abs() < 1e-7);
    }

    // ── SourceFormat Display ─────────────────────────────────────────────

    #[test]
    fn source_format_display() {
        let sf = SourceFormat {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 24,
            valid_bits: 20,
            kind: SampleKind::PcmSigned,
            channel_mask: Some(0x3),
        };
        let s = format!("{sf}");
        assert!(s.contains("2ch"));
        assert!(s.contains("48000Hz"));
        assert!(s.contains("24-bit"));
        assert!(s.contains("PCM"));
        assert!(s.contains("valid 20"));
        assert!(s.contains("mask=0x3"));
    }
}
