//! 类型化音频导出：源轨有界混合，连续编辑区间生成滤镜，保持音调并精确对齐输出时长。
use super::{MediaError, process::TempOutput};
use crate::gif::audio::{self, BYTES_PER_MS, RATE};
use std::{
    fs::File,
    io::{Read, Write},
    path::PathBuf,
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
        || input.spans.len() > 16384
        || input.spans.is_empty()
    {
        return Err(MediaError::InvalidPlan("音画区间或混音设置无效"));
    }
    let end = manifest.committed_ms;
    if end > audio::MAX_AUDIO_MS {
        return Err(MediaError::Budget("声音素材时长"));
    }
    let available = crate::platform::disk_space::query(&input.dir)?;
    if available.available_bytes
        < end
            .saturating_mul(BYTES_PER_MS)
            .saturating_add(crate::gif::space::MIN_RESERVE)
    {
        return Err(MediaError::Budget("声音处理中间文件所需空间不足"));
    }
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
    out.write_all(&audio::wav_header_long(end * BYTES_PER_MS))?;
    let mut left = end * BYTES_PER_MS;
    let mut buffers = [vec![0u8; 16384], vec![0u8; 16384]];
    while left > 0 {
        if cancel.is_cancelled() {
            return Err(MediaError::Cancelled);
        }
        let n = left.min(16384) as usize;
        for i in 0..2 {
            buffers[i][..n].fill(0);
            if let Some(file) = &mut files[i] {
                file.read_exact(&mut buffers[i][..n])?;
            }
        }
        let mut mixed = Vec::with_capacity(n);
        for pos in (0..n).step_by(2) {
            let v = (0..2)
                .map(|i| {
                    i16::from_le_bytes([buffers[i][pos], buffers[i][pos + 1]]) as f32
                        * input.gain[i]
                })
                .sum::<f32>();
            mixed.extend(((audio::soft_limit(v / 32768.0) * 32768.0).round() as i16).to_le_bytes());
        }
        out.write_all(&mixed)?;
        left -= n as u64;
    }
    out.flush()?;
    std::fs::write(&graph.0, filter_graph(&input.spans, end, output_ms, fps)?)?;
    Ok(Some(PreparedAudio { wave, graph }))
}
pub(crate) fn filter_graph(
    spans: &[Span],
    source_end: u64,
    output_ms: u64,
    fps: u32,
) -> Result<String, MediaError> {
    if spans.is_empty()
        || spans.len() > 16384
        || output_ms == 0
        || ![10, 15, 24, 30, 60].contains(&fps)
    {
        return Err(MediaError::InvalidPlan("音频区间预算"));
    }
    let mut graph = format!("[1:a]asplit={}", spans.len());
    for i in 0..spans.len() {
        graph.push_str(&format!("[s{i}]"));
    }
    graph.push(';');
    let mut total = 0;
    for (i, s) in spans.iter().enumerate() {
        if s.output_ms == 0
            || s.sound_ms == 0
            || s.source_ms
                .checked_add(s.sound_ms)
                .is_none_or(|end| end > source_end)
        {
            return Err(MediaError::InvalidPlan("音频原始区间越界"));
        }
        let before = s
            .sound_ms
            .checked_add(s.silence_ms)
            .ok_or(MediaError::InvalidPlan("音频区间溢出"))?;
        let mut speed = before as f64 / s.output_ms as f64;
        if !(0.20..=5.0).contains(&speed) {
            return Err(MediaError::InvalidPlan("声音变速超限"));
        }
        graph.push_str(&format!("[s{i}]atrim=start_sample={}:end_sample={},asetpts=N/SR/TB,apad=whole_len={},atrim=end_sample={}", s.source_ms * 48, (s.source_ms + s.sound_ms) * 48, before * 48, before * 48));
        while speed < 0.5 {
            graph.push_str(",atempo=0.5");
            speed *= 2.0;
        }
        while speed > 2.0 {
            graph.push_str(",atempo=2");
            speed /= 2.0;
        }
        graph.push_str(&format!(
            ",atempo={speed:.9},apad=whole_len={},atrim=end_sample={},asetpts=N/SR/TB[a{i}];",
            s.output_ms * 48,
            s.output_ms * 48
        ));
        total += s.output_ms;
    }
    // 视频按实际输出帧率向上补齐；声音仅补同一段尾部静音，不改变语速。
    let expected_ms = total
        .checked_mul(fps as u64)
        .ok_or(MediaError::InvalidPlan("音频时长溢出"))?
        .div_ceil(1000)
        .checked_mul(1000)
        .ok_or(MediaError::InvalidPlan("音频时长溢出"))?
        .div_ceil(fps as u64);
    if output_ms != expected_ms {
        return Err(MediaError::InvalidPlan("音视频有效时长不一致"));
    }
    for i in 0..spans.len() {
        graph.push_str(&format!("[a{i}]"));
    }
    graph.push_str(&format!(
        "concat=n={}:v=0:a=1,apad=whole_len={},atrim=end_sample={},asetpts=N/SR/TB[recorded_audio]",
        spans.len(),
        output_ms * RATE as u64 / 1000,
        output_ms * 48
    ));
    Ok(graph)
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
        assert!(graph.contains("start_sample=4800:end_sample=7200"));
        assert!(graph.contains("apad=whole_len=9600"));
        assert!(graph.contains("atempo=2.000000000"));
        assert!(!graph.contains("aloop"));
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
