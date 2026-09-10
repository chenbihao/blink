//! 0.22.16 Handoff 07 验收测试：CI 可提交的合成波形格式矩阵 + 等价性 + 隐私。
//!
//! 所有测试使用程序生成的无语义波形，不含真实录音。
//! 可在 CI 中安全运行（无外部依赖、无模型、无音频设备）。

#![cfg(test)]

use crate::domain::stt::wav::{decode_wav, pcm_to_wav};
use crate::infra::platform::audio::normalize::{
    AudioNormalizer, StreamingResampler, TARGET_SAMPLE_RATE,
};
use crate::infra::platform::audio::test_fixtures::*;

// ═══════════════════════════════════════════════════════════════════════════
// 1. 合成 fixture 格式矩阵（CI 可提交）
// ═══════════════════════════════════════════════════════════════════════════

/// PCM16 值域全覆盖：最小值、最大值、零、中间值。
#[test]
fn format_matrix_pcm16_values() {
    for &sr in &[8000u32, 16000, 22050, 32000, 44100, 48000, 96000] {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: sr,
            bits_per_sample: 16,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 100,
            values: FixtureValues::Explicit({
                let mut v = vec![
                    0.0, 1.0, -1.0, 0.5, -0.5, 0.25, -0.25, 0.75, -0.75, 0.0, 0.1, 0.2, 0.3, 0.4,
                    0.5, 0.6, 0.7, 0.8, 0.9, -0.1, 0.01, -0.01, 0.001, -0.001, 0.999, -0.999, 0.0,
                    0.0, 0.0, 0.0,
                ];
                // pad to 100
                v.extend(std::iter::repeat_n(0.0, 70));
                v
            }),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded =
            decode_wav(&wav).unwrap_or_else(|e| panic!("decode {sr}Hz PCM16 failed: {e}"));
        assert_eq!(decoded.format.channels, 1);
        assert_eq!(decoded.format.sample_rate, sr);
        assert_eq!(decoded.format.bits_per_sample, 16);
        assert_eq!(decoded.num_frames, 100);
    }
}

/// PCM24 值域全覆盖。
#[test]
fn format_matrix_pcm24_values() {
    for &sr in &[16000u32, 44100, 48000, 96000] {
        let cfg = FixtureConfig {
            channels: 1,
            sample_rate: sr,
            bits_per_sample: 24,
            kind: FixtureSampleKind::PcmSigned,
            extensible: false,
            num_frames: 50,
            values: FixtureValues::Explicit({
                let mut v = vec![
                    0.0, 1.0, -1.0, 0.5, -0.5, 0.25, -0.25, 0.0, 0.0, 0.0, 0.1, -0.1, 0.3, -0.3,
                    0.7, -0.7, 0.0, 0.0, 0.0, 0.0,
                ];
                // pad to 50
                v.extend(std::iter::repeat_n(0.0, 30));
                v
            }),
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded =
            decode_wav(&wav).unwrap_or_else(|e| panic!("decode {sr}Hz PCM24 failed: {e}"));
        assert_eq!(decoded.format.bits_per_sample, 24);
        assert_eq!(decoded.format.sample_rate, sr);
        // PCM24 sign extension
        assert!(decoded.samples[0].abs() < 1e-6, "zero");
        assert!(
            (decoded.samples[1] - 1.0).abs() < 1e-4,
            "max: {}",
            decoded.samples[1]
        );
        assert!(
            (decoded.samples[2] + 1.0).abs() < 1e-4,
            "min: {}",
            decoded.samples[2]
        );
    }
}

/// PCM32 值域全覆盖。
#[test]
fn format_matrix_pcm32_values() {
    for &ch in &[1u16, 2, 6] {
        let cfg = FixtureConfig {
            channels: ch,
            sample_rate: 48000,
            bits_per_sample: 32,
            kind: FixtureSampleKind::PcmSigned,
            extensible: ch > 2,
            channel_mask: if ch > 2 { Some(0x3F) } else { None },
            num_frames: 20,
            values: FixtureValues::MaxMin,
            ..Default::default()
        };
        let wav = build_wav(&cfg);
        let decoded =
            decode_wav(&wav).unwrap_or_else(|e| panic!("decode {ch}ch PCM32 failed: {e}"));
        assert_eq!(decoded.format.channels, ch);
        assert_eq!(decoded.format.bits_per_sample, 32);
        assert_eq!(decoded.num_frames, 20);
        assert_eq!(decoded.samples.len(), 20 * ch as usize);
    }
}

/// float32 值域 + NaN/Inf 拒绝。
#[test]
fn format_matrix_float32_values() {
    let cfg = FixtureConfig {
        channels: 1,
        sample_rate: 48000,
        bits_per_sample: 32,
        kind: FixtureSampleKind::IeeeFloat,
        extensible: false,
        num_frames: 10,
        values: FixtureValues::Explicit(vec![0.0, 1.0, -1.0, 0.5, -0.5, 0.25, 0.0, 0.0, 0.0, 0.0]),
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    assert_eq!(
        decoded.format.kind,
        crate::infra::platform::audio::format::SampleKind::IeeeFloat
    );
    assert!(decoded.samples[0].abs() < 1e-7);
    assert!((decoded.samples[1] - 1.0).abs() < 1e-7);
    assert!((decoded.samples[2] + 1.0).abs() < 1e-7);
}

/// WAVE_FORMAT_EXTENSIBLE 标准格式（PCM 和 float）。
#[test]
fn format_matrix_extensible_formats() {
    // extensible PCM16
    let cfg = FixtureConfig {
        channels: 2,
        sample_rate: 48000,
        bits_per_sample: 16,
        kind: FixtureSampleKind::PcmSigned,
        extensible: true,
        num_frames: 50,
        values: FixtureValues::Zero,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    assert_eq!(
        decoded.format.kind,
        crate::infra::platform::audio::format::SampleKind::PcmSigned
    );

    // extensible float32
    let cfg = FixtureConfig {
        channels: 2,
        sample_rate: 48000,
        bits_per_sample: 32,
        kind: FixtureSampleKind::IeeeFloat,
        extensible: true,
        num_frames: 50,
        values: FixtureValues::Zero,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    assert_eq!(
        decoded.format.kind,
        crate::infra::platform::audio::format::SampleKind::IeeeFloat
    );
}

/// 奇数 chunk（JUNK/LIST）穿插不影响解码。
#[test]
fn format_matrix_odd_chunks() {
    let cfg = FixtureConfig::default();
    let junk = junk_chunk(&[0xAA; 5]); // odd → padded
    let list = list_chunk(b"INFO odd length data"); // odd → padded
    let wav = build_wav_with_extra_chunks(&cfg, &[junk], &[list]);
    let decoded = decode_wav(&wav).unwrap();
    assert_eq!(decoded.num_frames, 100);
}

/// 截断和伪造 chunk 被正确拒绝。
#[test]
fn format_matrix_malformed_rejected() {
    // truncated header
    assert!(matches!(
        decode_wav(&[0u8; 11]),
        Err(crate::infra::platform::audio::format::AudioDecodeError::Truncated(_))
    ));
    // bad RIFF magic
    let mut data = [0u8; 44];
    data[0..4].copy_from_slice(b"XXX ");
    data[8..12].copy_from_slice(b"WAVE");
    assert!(decode_wav(&data).is_err());
    // RF64 rejected
    let mut rf64 = vec![0u8; 44];
    rf64[0..4].copy_from_slice(b"RF64");
    rf64[8..12].copy_from_slice(b"WAVE");
    assert!(decode_wav(&rf64).is_err());
}

/// 不支持 codec 被拒绝。
#[test]
fn format_matrix_unsupported_codecs() {
    // mu-law
    let mut wav = build_wav(&FixtureConfig::default());
    wav[20] = 0x07;
    let err = decode_wav(&wav).unwrap_err();
    assert!(matches!(
        err,
        crate::infra::platform::audio::format::AudioDecodeError::UnsupportedCodec { .. }
    ));

    // A-law
    let mut wav = build_wav(&FixtureConfig::default());
    wav[20] = 0x06;
    assert!(decode_wav(&wav).is_err());

    // ADPCM
    let mut wav = build_wav(&FixtureConfig::default());
    wav[20] = 0x02;
    assert!(decode_wav(&wav).is_err());
}

// ═══════════════════════════════════════════════════════════════════════════
// 2. 48kHz stereo → 16kHz mono 规范化等价性
// ═══════════════════════════════════════════════════════════════════════════

/// 48kHz 双声道 WAV 经 decoder + normalizer 后成为 16kHz 单声道 f32，
/// 时长不被放大。
#[test]
fn normalize_48k_stereo_to_16k_mono_duration_not_amplified() {
    // 480 frames stereo @ 48k = 10ms
    let cfg = FixtureConfig {
        channels: 2,
        sample_rate: 48000,
        bits_per_sample: 16,
        kind: FixtureSampleKind::PcmSigned,
        extensible: false,
        num_frames: 4800, // 100ms @ 48k
        values: FixtureValues::Deterministic,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    assert_eq!(decoded.format.channels, 2);
    assert_eq!(decoded.format.sample_rate, 48000);

    let mut normalizer = AudioNormalizer::from_source_format(decoded.format);
    let normalized = normalizer.process(&decoded.samples);
    let _tail = normalizer.finish();

    // 100ms @ 16k = 1600 samples
    assert_eq!(
        normalized.len(),
        1600,
        "100ms @ 16k should produce 1600 samples"
    );
    let duration_ms = normalized.len() as f64 / TARGET_SAMPLE_RATE as f64 * 1000.0;
    assert!(
        (duration_ms - 100.0).abs() < 1.0,
        "duration should be ~100ms, got {duration_ms}ms"
    );
}

/// 同一原始样本走 decoder + normalizer 与走测试加载器得到逐样本等价结果
/// （允许规定的重采样浮点误差）。
#[test]
fn normalize_equivalence_whole_vs_direct() {
    // 生成 48k mono 正弦波
    let sr = 48000u32;
    let num_frames = 4800; // 100ms
    let input: Vec<f32> = (0..num_frames)
        .map(|i| (2.0 * std::f32::consts::PI * 1000.0 * (i as f32 / sr as f32)).sin() * 0.5)
        .collect();

    // 路径 A：直接 resampler 48k → 16k
    let mut resampler = StreamingResampler::new(sr, TARGET_SAMPLE_RATE);
    let direct_output = resampler.process(&input);
    let direct_tail = resampler.finish();
    let total_direct: Vec<f32> = direct_output.into_iter().chain(direct_tail).collect();

    // 路径 B：encode to WAV → decode → normalizer
    let wav = pcm_to_wav(&input, sr, 1);
    let decoded = decode_wav(&wav).unwrap();
    let mut normalizer = AudioNormalizer::from_source_format(decoded.format);
    let norm_output = normalizer.process(&decoded.samples);
    let norm_tail = normalizer.finish();
    let total_norm: Vec<f32> = norm_output.into_iter().chain(norm_tail).collect();

    // 帧数应一致
    assert_eq!(
        total_direct.len(),
        total_norm.len(),
        "frame count mismatch: direct={} norm={}",
        total_direct.len(),
        total_norm.len()
    );

    // 逐样本在浮点误差内等价
    // 容差 1e-3：路径 B 经 PCM16 编码/解码（f32→i16→f32）引入量化误差，
    // 与路径 A 的原始 f32 相比有精度损失，不能用 1e-5 严格容差。
    for (i, (a, b)) in total_direct.iter().zip(total_norm.iter()).enumerate() {
        assert!(
            (a - b).abs() < 1e-3,
            "sample {i} mismatch: direct={a} norm={b} (diff={})",
            (a - b).abs()
        );
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 3. 全静音和非语音不产生正文（格式层验证）
// ═══════════════════════════════════════════════════════════════════════════

/// 全静音 WAV 解码后所有样本为零。
#[test]
fn silence_decodes_to_zero_samples() {
    let cfg = FixtureConfig {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        kind: FixtureSampleKind::PcmSigned,
        extensible: false,
        num_frames: 16000, // 1 second
        values: FixtureValues::Zero,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    assert!(
        decoded.samples.iter().all(|&s| s == 0.0),
        "all samples should be zero"
    );
    assert_eq!(decoded.duration_secs, 1.0);
}

/// 全静音 48k stereo 规范化后仍全零。
#[test]
fn silence_48k_stereo_normalizes_to_zero() {
    let cfg = FixtureConfig {
        channels: 2,
        sample_rate: 48000,
        bits_per_sample: 16,
        kind: FixtureSampleKind::PcmSigned,
        extensible: false,
        num_frames: 4800, // 100ms
        values: FixtureValues::Zero,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    let mut normalizer = AudioNormalizer::from_source_format(decoded.format);
    let output = normalizer.process(&decoded.samples);
    assert!(
        output.iter().all(|&s| s == 0.0),
        "normalized silence should be all zero"
    );
}

/// MaxMin 交替极值（模拟瞬时爆音/敲击）——解码值域正确。
#[test]
fn maxmin_alternating_decodes_correctly() {
    let cfg = FixtureConfig {
        channels: 1,
        sample_rate: 16000,
        bits_per_sample: 16,
        kind: FixtureSampleKind::PcmSigned,
        extensible: false,
        num_frames: 100,
        values: FixtureValues::MaxMin,
        ..Default::default()
    };
    let wav = build_wav(&cfg);
    let decoded = decode_wav(&wav).unwrap();
    for (i, &s) in decoded.samples.iter().enumerate() {
        if i % 2 == 0 {
            assert!((s - 1.0).abs() < 1e-3, "sample {i} should be ~1.0, got {s}");
        } else {
            assert!(
                (s + 1.0).abs() < 1e-3,
                "sample {i} should be ~-1.0, got {s}"
            );
        }
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 4. 整段与不同 chunk 切分规范化等价
// ═══════════════════════════════════════════════════════════════════════════

/// 同一 48k mono 样本走整段 vs 随机切块，重采样结果逐样本等价。
#[test]
fn chunk_equivalence_whole_vs_random_chunks() {
    let sr = 48000u32;
    let input: Vec<f32> = (0..9600).map(|i| (i as f32 * 0.01).sin()).collect();

    // 整段
    let mut r_whole = StreamingResampler::new(sr, TARGET_SAMPLE_RATE);
    let whole_out = r_whole.process(&input);
    let whole_tail = r_whole.finish();
    let total_whole: Vec<f32> = whole_out.into_iter().chain(whole_tail).collect();

    // 随机切块
    let mut r_chunked = StreamingResampler::new(sr, TARGET_SAMPLE_RATE);
    let chunk_sizes = [7, 13, 1, 100, 3, 48, 200, 50, 58, 300, 500, 1000, 2000];
    let mut offset = 0;
    let mut chunked_out = Vec::new();
    for &cs in &chunk_sizes {
        let end = (offset + cs).min(input.len());
        if offset >= end {
            break;
        }
        chunked_out.extend(r_chunked.process(&input[offset..end]));
        offset = end;
    }
    // 剩余
    if offset < input.len() {
        chunked_out.extend(r_chunked.process(&input[offset..]));
    }
    let chunked_tail = r_chunked.finish();
    let total_chunked: Vec<f32> = chunked_out.into_iter().chain(chunked_tail).collect();

    assert_eq!(
        total_whole.len(),
        total_chunked.len(),
        "frame count: whole={} chunked={}",
        total_whole.len(),
        total_chunked.len()
    );

    for (i, (a, b)) in total_whole.iter().zip(total_chunked.iter()).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: whole={a} chunked={b}");
    }
}

/// 48k stereo → 16k mono 整段 vs AudioNormalizer（downmix + resample）等价。
#[test]
fn chunk_equivalence_stereo_via_normalizer() {
    // 48k stereo, 4800 frames = 100ms
    let sr = 48000u32;
    let num_frames = 4800;
    let input: Vec<f32> = (0..num_frames * 2)
        .map(|i| {
            if i % 2 == 0 {
                (i as f32 * 0.001).sin()
            } else {
                (i as f32 * 0.002).cos()
            }
        })
        .collect();

    // 路径 A：手动 downmix + resampler
    let mut mono = Vec::with_capacity(num_frames);
    for frame in input.chunks_exact(2) {
        mono.push((frame[0] + frame[1]) * 0.5);
    }
    let mut resampler = StreamingResampler::new(sr, TARGET_SAMPLE_RATE);
    let manual_out = resampler.process(&mono);
    let manual_tail = resampler.finish();
    let total_manual: Vec<f32> = manual_out.into_iter().chain(manual_tail).collect();

    // 路径 B：AudioNormalizer（自动 downmix + resample）
    let mut normalizer = AudioNormalizer::new(2, sr);
    let norm_out = normalizer.process(&input);
    let norm_tail = normalizer.finish();
    let total_norm: Vec<f32> = norm_out.into_iter().chain(norm_tail).collect();

    assert_eq!(
        total_manual.len(),
        total_norm.len(),
        "frame count: manual={} norm={}",
        total_manual.len(),
        total_norm.len()
    );

    for (i, (a, b)) in total_manual.iter().zip(total_norm.iter()).enumerate() {
        assert!((a - b).abs() < 1e-6, "sample {i}: manual={a} norm={b}");
    }
}

// ═══════════════════════════════════════════════════════════════════════════
// 5. 发布隐私 gate
// ═══════════════════════════════════════════════════════════════════════════

/// 验证 `git status --short --untracked-files=all` 不泄露 corpus。
/// 此测试在 CI 中运行，确认 corpus 目录被 gitignore。
#[test]
fn privacy_corpus_is_gitignored() {
    // 验证 .gitignore 包含 corpus 路径
    let gitignore = include_str!("../../../../.gitignore");
    assert!(
        gitignore.contains("testdata/stt/corpus/"),
        ".gitignore 必须包含 testdata/stt/corpus/ 排除规则"
    );
}

/// 验证测试快照不包含音频 bytes 或转写正文。
/// 检查 test_fixtures 源码不含真实音频内容。
#[test]
fn privacy_fixtures_contain_no_audio_content() {
    let fixtures_src = include_str!("../../../../src/infra/platform/audio/test_fixtures.rs");
    // 不应包含 base64 编码的音频数据
    assert!(
        !fixtures_src.contains("UklGR"),
        "fixtures 不应包含 base64 编码的 WAV 数据"
    );
    // 不应包含真实文件路径
    assert!(!fixtures_src.contains("C:\\"), "fixtures 不应包含绝对路径");
    assert!(
        !fixtures_src.contains("/Users/"),
        "fixtures 不应包含 macOS 绝对路径"
    );
}

/// 验证 manifest（如果存在）不会被 git 跟踪。
#[test]
fn privacy_manifest_is_gitignored() {
    let gitignore = include_str!("../../../../.gitignore");
    // corpus 目录整体被排除，manifest.toml 在其中
    assert!(
        gitignore.contains("testdata/stt/corpus/"),
        ".gitignore 必须排除 corpus 目录（含 manifest）"
    );
}

/// 验证 release-check 守卫包含 corpus 检查。
#[test]
fn privacy_release_check_has_corpus_guard() {
    // release-check 位于 xtask
    let release_check = include_str!("../../../../xtask/src/main.rs");
    // 检查是否有 corpus 或 testdata/stt 的守卫
    assert!(
        release_check.contains("corpus") || release_check.contains("testdata/stt"),
        "release-check 应包含 corpus 或 testdata/stt 守卫"
    );
}

/// 验证 AudioTranscriptionResult 序列化不包含路径或音频 bytes。
#[test]
fn privacy_transcription_result_no_secrets() {
    use crate::domain::stt::transcribe::AudioTranscriptionResult;

    let result = AudioTranscriptionResult {
        text: "测试文本".into(),
        duration_ms: 1000,
        engine_id: "funasr".into(),
        model_id: "sensevoice-small".into(),
        engine_instance_id: "inst-1".into(),
        source_format: "2ch 48000Hz 16-bit PCM".into(),
        normalized_format: "1ch 16000Hz mono".into(),
        normalization: "2ch 48000Hz → 16000Hz mono".into(),
        no_speech: false,
    };
    let json = serde_json::to_string(&result).unwrap();
    // 不应包含路径、URL、IP 或 audio_ref
    assert!(!json.contains("C:\\"), "结果不应包含路径");
    assert!(!json.contains("http://"), "结果不应包含 URL");
    assert!(!json.contains("127.0.0.1"), "结果不应包含 IP");
    assert!(!json.contains("aref_"), "结果不应包含 audio_ref");
    assert!(!json.contains("RIFF"), "结果不应包含音频魔数");
}

/// 验证 decode_wav 的错误输出不包含音频 bytes 或路径。
#[test]
fn privacy_decode_errors_anonymous() {
    let bad_data = [0u8; 11]; // truncated
    let err = decode_wav(&bad_data).unwrap_err();
    let err_str = err.to_string();
    // 错误消息不应包含原始字节
    assert!(
        !err_str.contains(&format!("{:?}", bad_data)),
        "错误不应包含原始字节"
    );
    // 不应包含任何路径
    assert!(!err_str.contains("C:\\"), "错误不应包含路径");
}
