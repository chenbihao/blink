//! 音频导出：分块剪辑/混音 PCM，再统一保音调变速；滤镜图大小与编辑区间数无关。
use super::{MediaError, process::TempOutput};
use crate::gif::audio::{self, BYTES_PER_MS};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Debug)]
pub struct Span {
    pub source_ms: u64,
    pub sound_ms: u64,
    pub silence_ms: u64,
    pub output_ms: u64,
}
#[derive(Clone, Debug)]
pub struct AudioInput {
    pub dir: PathBuf,
    pub spans: Vec<Span>,
    pub gain: [f32; 2],
}
pub(crate) struct PreparedAudio {
    pub wave: TempOutput,
    pub graph: TempOutput,
}
pub(crate) fn prepare(
    input: AudioInput,
    output_ms: u64,
    fps: u32,
    cancel: &CancellationToken,
) -> Result<Option<PreparedAudio>, MediaError> {
    if input.gain.iter().all(|v| *v == 0.0) {
        return Ok(None);
    }
    let Some(manifest) = audio::manifest(&input.dir)? else {
        return Ok(None);
    };
    if input
        .gain
        .iter()
        .any(|v| !v.is_finite() || !(0.0..=2.0).contains(v))
    {
        return Err(MediaError::InvalidPlan("混音设置无效"));
    }
    let (before_ms, edited_ms) = edit_clock(&input.spans, manifest.committed_ms, output_ms, fps)?;
    let bytes = before_ms * BYTES_PER_MS;
    ensure_space(&input.dir, bytes)?;
    let wave = TempOutput::create(&input.dir.join("audio-mix.wav"))?;
    let graph = TempOutput::create(&input.dir.join("audio-filter.txt"))?;
    let mut files: [Option<File>; 2] = [None, None];
    for i in 0..2 {
        if manifest.enabled[i] && input.gain[i] > 0.0 {
            files[i] = Some(File::open(audio::track_path(&input.dir, i))?);
        }
    }
    if files.iter().all(Option::is_none) {
        return Ok(None);
    }
    let mut out = File::create(&wave.0)?;
    out.write_all(&audio::wav_header_long(bytes))?;
    let mut buffers = [vec![0u8; 16384], vec![0u8; 16384]];
    let mut mixed = vec![0u8; 16384];
    let mut remaining = bytes;
    let mut space_at = Instant::now();
    for span in &input.spans {
        // Cut only the requested original interval; extended holds append silence,
        // never repeat speech. Seeking also preserves the nonzero capture origin.
        for file in files.iter_mut().flatten() {
            file.seek(SeekFrom::Start(span.source_ms * BYTES_PER_MS))?;
        }
        for (ms, sound) in [(span.sound_ms, true), (span.silence_ms, false)] {
            let mut left = ms * BYTES_PER_MS;
            while left > 0 {
                if cancel.is_cancelled() {
                    return Err(MediaError::Cancelled);
                }
                if space_at.elapsed() >= Duration::from_millis(250) {
                    ensure_space(&input.dir, remaining)?;
                    space_at = Instant::now();
                }
                let n = left.min(mixed.len() as u64) as usize;
                mixed[..n].fill(0);
                if sound {
                    for i in 0..2 {
                        buffers[i][..n].fill(0);
                        if let Some(file) = &mut files[i] {
                            file.read_exact(&mut buffers[i][..n])?;
                        }
                    }
                    for pos in (0..n).step_by(2) {
                        let value = (0..2)
                            .map(|i| {
                                i16::from_le_bytes([buffers[i][pos], buffers[i][pos + 1]]) as f32
                                    * input.gain[i]
                            })
                            .sum::<f32>();
                        let sample = (audio::soft_limit(value / 32768.0) * 32768.0).round() as i16;
                        mixed[pos..pos + 2].copy_from_slice(&sample.to_le_bytes());
                    }
                }
                out.write_all(&mixed[..n])?;
                left -= n as u64;
                remaining -= n as u64;
            }
        }
    }
    out.flush()?;
    std::fs::write(&graph.0, tempo_graph(before_ms, edited_ms, output_ms))?;
    tracing::debug!(
        spans = input.spans.len(),
        before_ms,
        edited_ms,
        "录制声音已分块剪辑与混音"
    );
    Ok(Some(PreparedAudio { wave, graph }))
}
fn ensure_space(dir: &Path, remaining: u64) -> Result<(), MediaError> {
    if crate::platform::disk_space::query(dir)?.available_bytes
        < remaining.saturating_add(crate::gif::space::MIN_RESERVE)
    {
        return Err(MediaError::Budget("声音处理中间文件所需空间不足"));
    }
    Ok(())
}

// Validate the entire plan before creating a temporary file. Only arithmetic and
// source boundaries are limited; a long edited recording has no segment-count cap.
fn edit_clock(
    spans: &[Span],
    source_end: u64,
    output_ms: u64,
    fps: u32,
) -> Result<(u64, u64), MediaError> {
    if spans.is_empty()
        || output_ms == 0
        || output_ms > audio::MAX_AUDIO_MS
        || source_end > audio::MAX_AUDIO_MS
        || ![10, 15, 24, 30, 60].contains(&fps)
    {
        return Err(MediaError::InvalidPlan("音频区间或帧率无效"));
    }
    let (mut before, mut edited) = (0u64, 0u64);
    for span in spans {
        if span.output_ms == 0
            || span.sound_ms == 0
            || span
                .source_ms
                .checked_add(span.sound_ms)
                .is_none_or(|end| end > source_end)
        {
            return Err(MediaError::InvalidPlan("音频原始区间越界"));
        }
        let duration = span
            .sound_ms
            .checked_add(span.silence_ms)
            .ok_or(MediaError::InvalidPlan("音频区间溢出"))?;
        if !(0.20..=5.0).contains(&(duration as f64 / span.output_ms as f64)) {
            return Err(MediaError::InvalidPlan("声音变速超限"));
        }
        before = before
            .checked_add(duration)
            .filter(|v| *v <= audio::MAX_AUDIO_MS)
            .ok_or(MediaError::InvalidPlan("音频区间溢出"))?;
        edited = edited
            .checked_add(span.output_ms)
            .filter(|v| *v <= audio::MAX_AUDIO_MS)
            .ok_or(MediaError::InvalidPlan("音频时长溢出"))?;
    }
    // The sampled video rounds its tail up; append only that same tail silence.
    let expected_ms = (edited * fps as u64).div_ceil(1000) * 1000;
    if output_ms != expected_ms.div_ceil(fps as u64) {
        return Err(MediaError::InvalidPlan("音视频有效时长不一致"));
    }
    Ok((before, edited))
}
fn tempo_graph(before_ms: u64, edited_ms: u64, output_ms: u64) -> String {
    let mut graph = String::from("[1:a]asetpts=N/SR/TB");
    let mut speed = before_ms as f64 / edited_ms as f64;
    while speed < 0.5 {
        graph.push_str(",atempo=0.5");
        speed *= 2.0;
    }
    while speed > 2.0 {
        graph.push_str(",atempo=2");
        speed /= 2.0;
    }
    graph.push_str(&format!(
        ",atempo={speed:.9},apad=whole_len={},atrim=end_sample={},asetpts=N/SR/TB[recorded_audio]",
        output_ms * 48,
        output_ms * 48
    ));
    graph
}
#[cfg(test)]
fn filter_graph(
    spans: &[Span],
    source_end: u64,
    output_ms: u64,
    fps: u32,
) -> Result<String, MediaError> {
    let (before, edited) = edit_clock(spans, source_end, output_ms, fps)?;
    Ok(tempo_graph(before, edited, output_ms))
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hot_dual_source_mix_is_limited_without_changing_quiet_audio_or_source_pcm() {
        let dir = tempfile::tempdir().unwrap();
        audio::write_manifest(
            dir.path(),
            &audio::AudioManifest {
                version: 1,
                enabled: [true, true],
                committed_ms: 10,
                complete: true,
                ..Default::default()
            },
        )
        .unwrap();
        let samples: Vec<i16> = (0..960)
            .map(|n| match n % 6 {
                0 => 1000,
                1 => -1000,
                2 => 32767,
                3 => -32768,
                4 => 8000,
                _ => -8000,
            })
            .collect();
        let source: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
        for i in 0..2 {
            std::fs::write(audio::track_path(dir.path(), i), &source).unwrap();
        }
        let prepared = prepare(
            AudioInput {
                dir: dir.path().to_owned(),
                spans: vec![Span {
                    source_ms: 0,
                    sound_ms: 10,
                    silence_ms: 0,
                    output_ms: 10,
                }],
                gain: [2.0, 2.0],
            },
            34,
            30,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let wave = std::fs::read(&prepared.wave.0).unwrap();
        assert_eq!(wave.len(), source.len() + 44);
        let mixed: Vec<i16> = wave[44..]
            .chunks_exact(2)
            .map(|b| i16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(&mixed[..2], &[4000, -4000]);
        assert!(mixed.iter().all(|s| s.unsigned_abs() <= 32113));
        assert!(
            mixed[2] > mixed[4] && mixed[3] < mixed[5],
            "大峰值平滑压缩，不截成相同方波平台"
        );
        for i in 0..2 {
            assert_eq!(
                std::fs::read(audio::track_path(dir.path(), i)).unwrap(),
                source
            );
        }
    }

    #[test]
    fn preview_and_export_soft_limit_share_the_same_transfer_curve() {
        for (input, expected) in [
            (0.0, 0.0),
            (0.5, 0.5),
            (0.9, 0.9),
            (1.0, 0.957079616),
            (2.0, 0.979999915),
            (4.0, 0.98),
        ] {
            assert!((audio::soft_limit(input) - expected).abs() < 1e-7);
            assert!((audio::soft_limit(-input) + expected).abs() < 1e-7);
        }
    }
    #[test]
    fn audio_tail_matches_the_actual_video_sampling_rate() {
        use crate::media::{Budget, PlanFrame, SamplingPlan};
        for fps in [10, 15, 24, 30, 60] {
            for ms in [1, 10, 999, 1000, 1001, 2345] {
                let sampling = SamplingPlan::from_frames(
                    vec![PlanFrame {
                        src_idx: 0,
                        duration_ms: ms,
                    }]
                    .into(),
                    1,
                    &Budget::default(),
                    fps,
                )
                .unwrap();
                let spans = [Span {
                    source_ms: 0,
                    sound_ms: ms as u64,
                    silence_ms: 0,
                    output_ms: ms as u64,
                }];
                let output = sampling.duration_ms();
                let graph = filter_graph(&spans, ms as u64, output, fps).unwrap();
                assert!(graph.contains("atempo=1.000000000"));
                assert!(graph.ends_with(&format!(
                    "atrim=end_sample={},asetpts=N/SR/TB[recorded_audio]",
                    output * 48
                )));
                assert!(filter_graph(&spans, ms as u64, output + 1, fps).is_err());
                assert!(filter_graph(&spans, ms as u64, output, 0).is_err());
            }
        }
    }

    #[test]
    fn graph_cuts_pads_and_preserves_pitch_without_repeating_speech() {
        let graph = filter_graph(
            &[Span {
                source_ms: 100,
                sound_ms: 50,
                silence_ms: 150,
                output_ms: 100,
            }],
            200,
            100,
            30,
        )
        .unwrap();
        assert!(graph.contains("apad=whole_len=4800"));
        assert!(graph.contains("atempo=2.000000000"));
        assert!(!graph.contains("aloop"));
        assert!(!graph.contains("asplit"));
    }
    #[test]
    fn graph_rejects_missing_audio_and_mismatched_duration() {
        let s = Span {
            source_ms: 10,
            sound_ms: 100,
            silence_ms: 0,
            output_ms: 100,
        };
        assert!(filter_graph(&[s.clone()], 50, 100, 30).is_err());
        assert!(filter_graph(&[s], 200, 200, 30).is_err());
    }
    #[test]
    fn many_short_holds_are_streamed_with_exact_cuts_silence_and_constant_filter_size() {
        let dir = tempfile::tempdir().unwrap();
        let count = 20_000u64;
        let source_ms = count * 2 + 150;
        audio::write_manifest(
            dir.path(),
            &audio::AudioManifest {
                version: 1,
                enabled: [true, false],
                committed_ms: source_ms,
                complete: true,
                ..Default::default()
            },
        )
        .unwrap();
        // The first 150ms and every deleted millisecond must never enter output.
        let original: Vec<u8> = (0..source_ms)
            .flat_map(|ms| {
                let sample = if ms >= 150 && (ms - 150) % 2 == 0 {
                    1000i16
                } else {
                    9000i16
                };
                sample.to_le_bytes().repeat(96)
            })
            .collect();
        let path = audio::track_path(dir.path(), 0);
        std::fs::write(&path, &original).unwrap();
        let spans: Vec<_> = (0..count)
            .map(|i| Span {
                source_ms: 150 + i * 2,
                sound_ms: 1,
                silence_ms: 1,
                output_ms: 2,
            })
            .collect();
        let graph = filter_graph(&spans, source_ms, count * 2, 30).unwrap();
        assert!(graph.len() < 256);
        assert!(!graph.contains("asplit") && !graph.contains("concat"));
        let result = prepare(
            AudioInput {
                dir: dir.path().into(),
                spans,
                gain: [1.0, 0.0],
            },
            count * 2,
            30,
            &CancellationToken::new(),
        )
        .unwrap()
        .unwrap();
        let wave = std::fs::read(&result.wave.0).unwrap();
        assert_eq!(wave.len() as u64, 44 + count * 2 * BYTES_PER_MS);
        for (ms, samples) in wave[44..].chunks_exact(BYTES_PER_MS as usize).enumerate() {
            let expected = if ms % 2 == 0 { 1000i16 } else { 0 };
            assert!(
                samples
                    .chunks_exact(2)
                    .all(|s| i16::from_le_bytes([s[0], s[1]]) == expected)
            );
        }
        assert_eq!(std::fs::read(&path).unwrap(), original);
    }

    #[test]
    fn extended_holds_validate_temp_space_arithmetic_before_writing() {
        let span = Span {
            source_ms: 0,
            sound_ms: 1,
            silence_ms: audio::MAX_AUDIO_MS,
            output_ms: audio::MAX_AUDIO_MS,
        };
        assert!(edit_clock(&[span], 1, audio::MAX_AUDIO_MS, 30).is_err());
        let span = Span {
            source_ms: 0,
            sound_ms: 1,
            silence_ms: 3,
            output_ms: 2,
        };
        assert_eq!(edit_clock(&[span], 1, 34, 30).unwrap(), (4, 2));
    }
    #[test]
    fn source_mix_is_exact_and_cancelled_or_missing_audio_preserves_sources() {
        let dir = tempfile::tempdir().unwrap();
        let m = audio::AudioManifest {
            version: 1,
            enabled: [true, true],
            committed_ms: 10,
            complete: true,
            ..Default::default()
        };
        audio::write_manifest(dir.path(), &m).unwrap();
        let first: Vec<u8> = (0..960).flat_map(|_| 1000i16.to_le_bytes()).collect();
        let second: Vec<u8> = (0..960).flat_map(|_| 2000i16.to_le_bytes()).collect();
        std::fs::write(audio::track_path(dir.path(), 0), &first).unwrap();
        std::fs::write(audio::track_path(dir.path(), 1), &second).unwrap();
        let input = AudioInput {
            dir: dir.path().to_owned(),
            spans: vec![Span {
                source_ms: 0,
                sound_ms: 10,
                silence_ms: 0,
                output_ms: 10,
            }],
            gain: [1.0, 0.5],
        };
        let cancel = CancellationToken::new();
        let prepared = prepare(input.clone(), 34, 30, &cancel).unwrap().unwrap();
        let bytes = std::fs::read(&prepared.wave.0).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert!(
            bytes[44..]
                .chunks_exact(2)
                .all(|b| i16::from_le_bytes([b[0], b[1]]) == 2000)
        );
        let temp = prepared.wave.0.clone();
        drop(prepared);
        assert!(!temp.exists());
        cancel.cancel();
        assert!(matches!(
            prepare(input.clone(), 34, 30, &cancel),
            Err(MediaError::Cancelled)
        ));
        assert_eq!(
            std::fs::read(audio::track_path(dir.path(), 0)).unwrap(),
            first
        );
        std::fs::remove_file(audio::track_path(dir.path(), 1)).unwrap();
        assert!(matches!(
            prepare(input, 34, 30, &CancellationToken::new()),
            Err(MediaError::Io(_))
        ));
    }
}
