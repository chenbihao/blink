//! 程序生成的无语义 WAV 测试 fixtures。
//!
//! 0.22.16 Handoff 01：decoder 测试矩阵的合成波形来源。
//! 所有 fixture 使用确定性随机或公式化波形，不含任何语音或语义内容。
//!
//! **绝不包含真实录音或私有 corpus**。

#![allow(dead_code)]

// ── 常量 ──────────────────────────────────────────────────────────────────

/// PCM 16-bit 的 `wFormatTag`。
const WAVE_FORMAT_PCM: u16 = 0x0001;
/// IEEE float 的 `wFormatTag`。
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
/// extensible 的 `wFormatTag`。
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// PCM SubFormat GUID（16 字节）。
const PCM_SUBFORMAT_GUID: [u8; 16] = [
    0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// IEEE float SubFormat GUID（16 字节）。
const IEEE_FLOAT_SUBFORMAT_GUID: [u8; 16] = [
    0x03, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B, 0x71,
];

/// Fixure builder 的配置参数。
#[derive(Debug, Clone)]
pub struct FixtureConfig {
    pub channels: u16,
    pub sample_rate: u32,
    pub bits_per_sample: u16,
    pub kind: FixtureSampleKind,
    pub extensible: bool,
    /// extensible 时可指定 valid_bits < bits_per_sample。
    pub valid_bits: Option<u16>,
    /// extensible 时可指定 channel mask。
    pub channel_mask: Option<u32>,
    /// 生成的帧数。
    pub num_frames: usize,
    /// 样本值策略。
    pub values: FixtureValues,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FixtureSampleKind {
    PcmSigned,
    IeeeFloat,
}

#[derive(Debug, Clone)]
pub enum FixtureValues {
    /// 使用确定性伪随机波形（无语义）。
    Deterministic,
    /// 指定每帧每声道的 f32 值（按帧×声道排列）。
    Explicit(Vec<f32>),
    /// 全零。
    Zero,
    /// PCM 正负最大值交替。
    MaxMin,
    /// PCM 正负中间值交替。
    MidRange,
}

impl Default for FixtureConfig {
    fn default() -> Self {
        Self {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            valid_bits: None,
            channel_mask: None,
            num_frames: 100,
            values: FixtureValues::Deterministic,
        }
    }
}

// ── 构建器 ───────────────────────────────────────────────────────────────

/// 从配置构建完整的 WAV 字节。
///
/// 标准输出：`RIFF` / `WAVE` / `fmt ` / `data`。
/// 奇数 chunk 自动 pad 一字节。
pub fn build_wav(cfg: &FixtureConfig) -> Vec<u8> {
    let samples_f32 = generate_samples(cfg);
    let pcm_bytes = encode_samples(&samples_f32, cfg);

    let bits = cfg.bits_per_sample;
    let channels = cfg.channels;
    let block_align = channels * (bits / 8);
    let byte_rate = cfg.sample_rate * channels as u32 * (bits / 8) as u32;
    let data_size = pcm_bytes.len() as u32;

    let fmt_chunk = build_fmt_chunk(cfg, bits, channels, block_align, byte_rate);
    let fmt_padded = pad_chunk(fmt_chunk);

    // data chunk header + payload
    let mut data_chunk = Vec::with_capacity(8 + pcm_bytes.len());
    data_chunk.extend_from_slice(b"data");
    data_chunk.extend_from_slice(&data_size.to_le_bytes());
    data_chunk.extend_from_slice(&pcm_bytes);
    let data_padded = pad_chunk(data_chunk);

    let inner_len = 4 + fmt_padded.len() + data_padded.len(); // "WAVE" + fmt + data
    let file_size = inner_len as u32;

    let mut wav = Vec::with_capacity(12 + inner_len);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_size.to_le_bytes());
    wav.extend_from_slice(b"WAVE");
    wav.extend_from_slice(&fmt_padded);
    wav.extend_from_slice(&data_padded);
    wav
}

/// 构建一个带自定义 JUNK / LIST chunk 的 WAV（用于测试奇数长度和 chunk 间穿插）。
pub fn build_wav_with_extra_chunks(
    cfg: &FixtureConfig,
    extra_before_fmt: &[Vec<u8>],
    extra_before_data: &[Vec<u8>],
) -> Vec<u8> {
    let samples_f32 = generate_samples(cfg);
    let pcm_bytes = encode_samples(&samples_f32, cfg);

    let bits = cfg.bits_per_sample;
    let channels = cfg.channels;
    let block_align = channels * (bits / 8);
    let byte_rate = cfg.sample_rate * channels as u32 * (bits / 8) as u32;
    let data_size = pcm_bytes.len() as u32;

    let fmt_chunk = build_fmt_chunk(cfg, bits, channels, block_align, byte_rate);
    let fmt_padded = pad_chunk(fmt_chunk);

    let mut data_chunk = Vec::with_capacity(8 + pcm_bytes.len());
    data_chunk.extend_from_slice(b"data");
    data_chunk.extend_from_slice(&data_size.to_le_bytes());
    data_chunk.extend_from_slice(&pcm_bytes);
    let data_padded = pad_chunk(data_chunk);

    let mut inner = Vec::new();
    inner.extend_from_slice(b"WAVE");
    for ch in extra_before_fmt {
        inner.extend_from_slice(&pad_chunk(ch.clone()));
    }
    inner.extend_from_slice(&fmt_padded);
    for ch in extra_before_data {
        inner.extend_from_slice(&pad_chunk(ch.clone()));
    }
    inner.extend_from_slice(&data_padded);

    let file_size = inner.len() as u32;
    let mut wav = Vec::with_capacity(8 + inner.len());
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&file_size.to_le_bytes());
    wav.extend_from_slice(&inner);
    wav
}

/// 构建一个 JUNK chunk（用于测试穿插）。
pub fn junk_chunk(payload: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(8 + payload.len());
    chunk.extend_from_slice(b"JUNK");
    chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    chunk.extend_from_slice(payload);
    chunk
}

/// 构建一个 LIST chunk（用于测试穿插）。
pub fn list_chunk(payload: &[u8]) -> Vec<u8> {
    let mut chunk = Vec::with_capacity(8 + payload.len());
    chunk.extend_from_slice(b"LIST");
    chunk.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    chunk.extend_from_slice(payload);
    chunk
}

// ── 内部 ─────────────────────────────────────────────────────────────────

fn build_fmt_chunk(
    cfg: &FixtureConfig,
    bits: u16,
    channels: u16,
    block_align: u16,
    byte_rate: u32,
) -> Vec<u8> {
    if cfg.extensible {
        let valid_bits = cfg.valid_bits.unwrap_or(bits);
        let mask = cfg.channel_mask.unwrap_or(0);
        let mut chunk = Vec::with_capacity(8 + 40);
        chunk.extend_from_slice(b"fmt ");
        chunk.extend_from_slice(&40u32.to_le_bytes()); // cbSize for extensible = 22, total fmt data = 40
        chunk.extend_from_slice(&WAVE_FORMAT_EXTENSIBLE.to_le_bytes());
        chunk.extend_from_slice(&channels.to_le_bytes());
        chunk.extend_from_slice(&cfg.sample_rate.to_le_bytes());
        chunk.extend_from_slice(&byte_rate.to_le_bytes());
        chunk.extend_from_slice(&block_align.to_le_bytes());
        chunk.extend_from_slice(&bits.to_le_bytes());
        // cbSize (extension size)
        chunk.extend_from_slice(&22u16.to_le_bytes());
        // valid_bits
        chunk.extend_from_slice(&valid_bits.to_le_bytes());
        // channel mask
        chunk.extend_from_slice(&mask.to_le_bytes());
        // SubFormat GUID
        let guid = match cfg.kind {
            FixtureSampleKind::PcmSigned => &PCM_SUBFORMAT_GUID,
            FixtureSampleKind::IeeeFloat => &IEEE_FLOAT_SUBFORMAT_GUID,
        };
        chunk.extend_from_slice(guid);
        chunk
    } else {
        let format_tag = match cfg.kind {
            FixtureSampleKind::PcmSigned => WAVE_FORMAT_PCM,
            FixtureSampleKind::IeeeFloat => WAVE_FORMAT_IEEE_FLOAT,
        };
        let mut chunk = Vec::with_capacity(8 + 16);
        chunk.extend_from_slice(b"fmt ");
        chunk.extend_from_slice(&16u32.to_le_bytes());
        chunk.extend_from_slice(&format_tag.to_le_bytes());
        chunk.extend_from_slice(&channels.to_le_bytes());
        chunk.extend_from_slice(&cfg.sample_rate.to_le_bytes());
        chunk.extend_from_slice(&byte_rate.to_le_bytes());
        chunk.extend_from_slice(&block_align.to_le_bytes());
        chunk.extend_from_slice(&bits.to_le_bytes());
        chunk
    }
}

fn pad_chunk(mut chunk: Vec<u8>) -> Vec<u8> {
    // chunk[4..8] is the size; payload starts at 8
    let payload_len = chunk.len() - 8;
    if payload_len % 2 == 1 {
        chunk.push(0);
    }
    chunk
}

fn generate_samples(cfg: &FixtureConfig) -> Vec<f32> {
    let total = cfg.num_frames * cfg.channels as usize;
    match &cfg.values {
        FixtureValues::Zero => vec![0.0; total],
        FixtureValues::MaxMin => {
            let mut v = Vec::with_capacity(total);
            for i in 0..total {
                v.push(if i % 2 == 0 { 1.0 } else { -1.0 });
            }
            v
        }
        FixtureValues::MidRange => {
            let mut v = Vec::with_capacity(total);
            for i in 0..total {
                v.push(if i % 2 == 0 { 0.5 } else { -0.5 });
            }
            v
        }
        FixtureValues::Explicit(vals) => {
            assert_eq!(vals.len(), total, "Explicit values count mismatch");
            vals.clone()
        }
        FixtureValues::Deterministic => {
            // 简单确定性公式：sin(2πf t) + 混合，无语义
            let mut v = Vec::with_capacity(total);
            let freq = 440.0f32;
            let sr = cfg.sample_rate as f32;
            for frame in 0..cfg.num_frames {
                let t = frame as f32 / sr;
                let base = (2.0 * std::f32::consts::PI * freq * t).sin() * 0.3;
                for _ch in 0..cfg.channels {
                    v.push(base);
                }
            }
            v
        }
    }
}

fn encode_samples(samples_f32: &[f32], cfg: &FixtureConfig) -> Vec<u8> {
    match cfg.kind {
        FixtureSampleKind::PcmSigned => match cfg.bits_per_sample {
            16 => encode_pcm16(samples_f32),
            24 => encode_pcm24(samples_f32),
            32 => encode_pcm32(samples_f32),
            _ => panic!("unsupported PCM bits: {}", cfg.bits_per_sample),
        },
        FixtureSampleKind::IeeeFloat => match cfg.bits_per_sample {
            32 => encode_float32(samples_f32),
            _ => panic!("unsupported float bits: {}", cfg.bits_per_sample),
        },
    }
}

fn encode_pcm16(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 2);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 32767.0).round() as i16;
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

fn encode_pcm24(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 3);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        let val = (clamped * 8388607.0).round() as i32;
        let bytes = val.to_le_bytes();
        out.extend_from_slice(&bytes[0..3]);
    }
    out
}

fn encode_pcm32(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        let clamped = s.clamp(-1.0, 1.0);
        // -1.0 maps to i32::MIN (-2147483648), +1.0 maps to i32::MAX (2147483647)
        let val = if clamped == -1.0 {
            i32::MIN
        } else {
            (clamped * 2147483647.0).round() as i32
        };
        out.extend_from_slice(&val.to_le_bytes());
    }
    out
}

fn encode_float32(samples: &[f32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples.len() * 4);
    for &s in samples {
        // Don't clamp — write raw f32 bits so NaN/Inf are preserved
        // (decoder will reject non-finite values)
        out.extend_from_slice(&s.to_le_bytes());
    }
    out
}

// ── fixture builder 自身的 header/layout 测试 ────────────────────────────

#[cfg(test)]
mod builder_tests {
    use super::*;

    fn read_u16_le(data: &[u8], offset: usize) -> u16 {
        u16::from_le_bytes([data[offset], data[offset + 1]])
    }

    fn read_u32_le(data: &[u8], offset: usize) -> u32 {
        u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ])
    }

    #[test]
    fn builder_produces_valid_riff_header() {
        let cfg = FixtureConfig::default();
        let wav = build_wav(&cfg);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        let declared = read_u32_le(&wav, 4) as usize;
        assert_eq!(declared, wav.len() - 8);
    }

    #[test]
    fn builder_fmt_chunk_is_correct_standard_pcm16() {
        let cfg = FixtureConfig {
            channels: 2,
            sample_rate: 48000,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 10,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        assert_eq!(&wav[12..16], b"fmt ");
        let fmt_size = read_u32_le(&wav, 16);
        assert_eq!(fmt_size, 16);
        assert_eq!(read_u16_le(&wav, 20), 0x0001); // PCM
        assert_eq!(read_u16_le(&wav, 22), 2); // channels
        assert_eq!(read_u32_le(&wav, 24), 48000); // sample rate
        assert_eq!(read_u32_le(&wav, 28), 48000 * 2 * 2); // byte rate
        assert_eq!(read_u16_le(&wav, 32), 4); // block align
        assert_eq!(read_u16_le(&wav, 34), 16); // bits
    }

    #[test]
    fn builder_extensible_fmt_chunk_has_guid() {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: 16000,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: true,
            valid_bits: Some(20),
            channel_mask: Some(0x4), // mono
            num_frames: 5,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        assert_eq!(&wav[12..16], b"fmt ");
        let fmt_size = read_u32_le(&wav, 16);
        assert_eq!(fmt_size, 40);
        assert_eq!(read_u16_le(&wav, 20), 0xFFFE); // extensible
        assert_eq!(read_u16_le(&wav, 34), 24); // bits
        let cb_size = read_u16_le(&wav, 36);
        assert_eq!(cb_size, 22);
        let valid_bits = read_u16_le(&wav, 38);
        assert_eq!(valid_bits, 20);
        let mask = read_u32_le(&wav, 40);
        assert_eq!(mask, 0x4);
        // GUID at offset 44..60
        assert_eq!(&wav[44..60], &PCM_SUBFORMAT_GUID);
    }

    #[test]
    fn builder_float32_standard_tag() {
        let cfg = FixtureConfig {
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            extensible: false,
            num_frames: 3,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        assert_eq!(read_u16_le(&wav, 20), 0x0003); // IEEE float
        assert_eq!(read_u16_le(&wav, 34), 32); // bits
    }

    #[test]
    fn builder_odd_data_chunk_gets_padded() {
        // PCM 24-bit mono, 1 frame = 3 bytes (odd)
        let cfg = FixtureConfig {
            channels: 1,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            num_frames: 1,
            values: FixtureValues::Zero,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        // data chunk at offset after fmt (12 + 8 + 16 = 36)
        let data_offset = 36;
        assert_eq!(&wav[data_offset..data_offset + 4], b"data");
        let data_size = read_u32_le(&wav, data_offset + 4) as usize;
        assert_eq!(data_size, 3);
        // pad byte
        assert_eq!(wav[data_offset + 8 + 3], 0);
    }

    #[test]
    fn builder_extra_chunks_are_inserted() {
        let cfg = FixtureConfig::default();
        let junk = junk_chunk(&[0xAA; 5]); // odd → padded to 6
        let list = list_chunk(b"info extra data here");
        let wav = build_wav_with_extra_chunks(&cfg, &[junk], &[list]);
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..12], b"WAVE");
        // JUNK right after WAVE
        let off = 12;
        assert_eq!(&wav[off..off + 4], b"JUNK");
        // fmt after junk (12 + 8 + 6 = 26)
        let fmt_off = 26;
        assert_eq!(&wav[fmt_off..fmt_off + 4], b"fmt ");
        // data after fmt + list
        // fmt chunk = 8 + 16 = 24
        // list chunk = 8 + 20 = 28
        // data_off = 26 + 24 + 28 = 78
        let data_off = fmt_off + 24 + 28;
        assert_eq!(&wav[data_off..data_off + 4], b"data");
    }

    #[test]
    fn builder_pcm32_roundtrips_values() {
        let cfg = FixtureConfig {
            bits_per_sample: 32,
            kind: FixtureSampleKind::PcmSigned,
            num_frames: 4,
            channels: 1,
            values: FixtureValues::MaxMin,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        // data starts at offset 36 (12 + 8 + 16)
        let data_off = 36 + 8;
        let v0 = i32::from_le_bytes(wav[data_off..data_off + 4].try_into().unwrap());
        let v1 = i32::from_le_bytes(wav[data_off + 4..data_off + 8].try_into().unwrap());
        assert_eq!(v0, 2147483647); // max
        assert_eq!(v1, -2147483648); // min (i32::MIN)
    }

    #[test]
    fn builder_float32_roundtrips_values() {
        let cfg = FixtureConfig {
            bits_per_sample: 32,
            kind: FixtureSampleKind::IeeeFloat,
            num_frames: 2,
            channels: 1,
            values: FixtureValues::Explicit(vec![0.25, -0.75]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let data_off = 36 + 8;
        let v0 = f32::from_le_bytes(wav[data_off..data_off + 4].try_into().unwrap());
        let v1 = f32::from_le_bytes(wav[data_off + 4..data_off + 8].try_into().unwrap());
        assert!((v0 - 0.25).abs() < 1e-6);
        assert!((v1 - (-0.75)).abs() < 1e-6);
    }

    #[test]
    fn builder_stereo_interleaves_channels() {
        let cfg = FixtureConfig {
            channels: 2,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            num_frames: 2,
            values: FixtureValues::Explicit(vec![0.1, 0.2, 0.3, 0.4]),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let data_off = 36 + 8;
        let v0 = i16::from_le_bytes(wav[data_off..data_off + 2].try_into().unwrap());
        let v1 = i16::from_le_bytes(wav[data_off + 2..data_off + 4].try_into().unwrap());
        let v2 = i16::from_le_bytes(wav[data_off + 4..data_off + 6].try_into().unwrap());
        let v3 = i16::from_le_bytes(wav[data_off + 6..data_off + 8].try_into().unwrap());
        // 0.1 * 32767 ≈ 3277
        assert!((v0 as f32 / 32767.0 - 0.1).abs() < 0.01);
        assert!((v1 as f32 / 32767.0 - 0.2).abs() < 0.01);
        assert!((v2 as f32 / 32767.0 - 0.3).abs() < 0.01);
        assert!((v3 as f32 / 32767.0 - 0.4).abs() < 0.01);
    }
}
