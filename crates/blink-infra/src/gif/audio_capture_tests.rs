use super::*;

fn append(out: &mut Vec<i16>, blocks: Vec<AudioBlock>, origin: u64) {
    for block in blocks {
        assert_eq!(
            block.start,
            origin + out.len() as u64 / 2,
            "PCM 必须连续，不能留零洞或覆盖"
        );
        out.extend(block.samples);
    }
}

#[test]
fn callback_timestamp_jitter_never_inserts_holes_or_overwrites_samples() {
    let mut signal = CaptureSignal::new(1, RATE);
    let mut pcm = Vec::new();
    for n in 0..500 {
        let jitter = [0.0, 3.0, -7.0, 32.0, -31.0][n % 5];
        assert_eq!(
            signal
                .push(&[0.25; 480], n as f64 * 480.0 + jitter)
                .unwrap(),
            None
        );
        append(
            &mut pcm,
            signal.drain(0, (n + 1) as u64 * 480, |_| false),
            0,
        );
    }
    signal.finish();
    append(&mut pcm, signal.drain(0, 240_000, |_| false), 0);
    assert!(pcm.len() / 2 >= 239_998);
    assert!(
        pcm.iter().all(|s| *s == 8192),
        "连续非零输入不能变成周期性静音"
    );
}

#[test]
fn future_boundary_is_deferred_and_mute_is_applied_when_samples_become_valid() {
    let mut signal = CaptureSignal::new(1, RATE);
    signal.push(&[0.5; 480], 0.0).unwrap();
    let mut pcm = Vec::new();
    append(&mut pcm, signal.drain(0, 478, |_| false), 0);
    assert_eq!(pcm.len(), 478 * 2);
    signal.push(&[0.5; 480], 480.0).unwrap();
    append(&mut pcm, signal.drain(0, 960, |ms| ms >= 10), 0);
    assert_eq!(pcm.len(), 960 * 2);
    assert!(pcm[..480 * 2].iter().all(|s| *s == 16384));
    assert!(pcm[480 * 2..].iter().all(|s| *s == 0));
}

#[test]
fn real_gaps_are_preserved_and_overlapping_packets_never_rewrite_saved_audio() {
    let mut signal = CaptureSignal::new(1, RATE);
    signal.push(&[0.25; 480], 0.0).unwrap();
    assert_eq!(signal.drain(0, 480, |_| false)[0].start, 0);
    assert_eq!(signal.push(&[0.5; 480], 1440.0).unwrap(), Some(960));
    let gap = signal.drain(0, 1920, |_| false);
    assert_eq!(gap[0].start, 1440);
    assert_eq!(gap[0].samples.len(), 960);
    assert_eq!(signal.push(&[0.75; 480], 1680.0).unwrap(), Some(-240));
    let overlap = signal.drain(0, 2160, |_| false);
    assert_eq!(overlap[0].start, 1920);
    assert_eq!(overlap[0].samples.len(), 480);
    assert!(overlap[0].samples.iter().all(|s| *s == 24575));
}

fn resample(rate: u32, chunks: &[usize]) -> Vec<i16> {
    let mut signal = CaptureSignal::new(2, rate);
    let input: Vec<f32> = (0..rate as usize)
        .flat_map(|n| {
            let v = (n as f32 * 0.025).sin() * 0.3;
            [v, -v]
        })
        .collect();
    let mut at = 0;
    let mut output = Vec::new();
    for count in chunks.iter().cycle() {
        let end = (at + count).min(rate as usize);
        signal
            .push(
                &input[at * 2..end * 2],
                at as f64 * RATE as f64 / rate as f64,
            )
            .unwrap();
        append(&mut output, signal.drain(0, RATE as u64, |_| false), 0);
        at = end;
        if at == rate as usize {
            break;
        }
    }
    signal.finish();
    append(&mut output, signal.drain(0, RATE as u64, |_| false), 0);
    output
}

#[test]
fn stereo_resampling_keeps_phase_across_arbitrary_callback_splits() {
    for rate in [8000, 44100, 48000, 96000, 192000] {
        let whole = resample(rate, &[rate as usize]);
        let split = resample(rate, &[7, 441, 13, 479, 1, 1024]);
        assert_eq!(whole.len(), 96_000, "rate={rate}");
        assert_eq!(split.len(), whole.len(), "rate={rate}");
        assert!(
            whole
                .iter()
                .zip(&split)
                .all(|(a, b)| (*a as i32 - *b as i32).abs() <= 1),
            "rate={rate}"
        );
        assert!(
            split
                .chunks_exact(2)
                .all(|f| (f[0] as i32 + f[1] as i32).abs() <= 1)
        );
    }
}

#[test]
fn long_capture_tracks_device_clock_drift_without_holes() {
    for drift in [-0.0001, 0.0001] {
        let mut signal = CaptureSignal::new(1, RATE);
        let mut end = 0;
        for n in 0..12_000 {
            signal
                .push(&[0.25; 480], n as f64 * 480.0 * (1.0 + drift))
                .unwrap();
            for block in signal.drain(0, u64::MAX, |_| false) {
                assert_eq!(block.start, end);
                assert!(block.samples.iter().all(|s| *s == 8192));
                end += block.samples.len() as u64 / 2;
            }
        }
        let expected = 12_000.0 * 480.0 * (1.0 + drift);
        assert!(
            (end as f64 - expected).abs() < 12.0,
            "end={end}, expected={expected}"
        );
        assert!(signal.pending.is_empty());
    }
}
