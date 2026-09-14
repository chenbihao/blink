//! 对一份规范化 PCM 做只读 VAD 回放，供设置页诊断与语料测试共用。
//! 数值曲线有界；不返回 PCM、文件名或转写内容。

use super::vad::{EnergyVad, VadState};

#[derive(Debug, Clone, serde::Serialize)]
pub struct VadTracePoint {
    pub time_ms: u64,
    pub rms: f64,
    pub on: f64,
    pub off: f64,
    pub speaking: bool,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VadTraceEvent {
    pub time_ms: u64,
    pub reason: &'static str,
    pub sentence_ms: u64,
    pub silence_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VadQuietSpan {
    pub start_ms: u64,
    pub end_ms: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct VadTrace {
    pub duration_ms: u64,
    pub points: Vec<VadTracePoint>,
    pub events: Vec<VadTraceEvent>,
    pub rejected_short_sentences: Vec<VadTraceEvent>,
    pub quiet_spans: Vec<VadQuietSpan>,
}

/// 与生产 VAD 使用相同 10ms 输入和状态机。曲线最多约 1200 点，事件保留原时间。
pub fn trace_vad(samples: &[f32], sample_rate: u32, vad: &mut EnergyVad) -> VadTrace {
    let frame_samples = (sample_rate as usize / 100).max(1);
    let duration_ms = samples.len() as u64 * 1000 / sample_rate as u64;
    let bucket_ms = (duration_ms / 1200).div_ceil(10).saturating_mul(10).max(50);
    let bucket_samples = bucket_ms as usize * sample_rate as usize / 1000;
    let mut trace = VadTrace {
        duration_ms,
        points: Vec::new(),
        events: Vec::new(),
        rejected_short_sentences: Vec::new(),
        quiet_spans: Vec::new(),
    };
    let mut processed = 0usize;
    let mut bucket_start = 0usize;
    let mut bucket_energy = 0.0;
    let mut bucket_count = 0usize;
    let mut quiet_start: Option<u64> = None;
    let mut previous: VadState = vad.dump_state();

    for frame in samples.chunks(frame_samples) {
        let sum_squares: f64 = frame.iter().map(|sample| f64::from(*sample).powi(2)).sum();
        let rms = (sum_squares / frame.len() as f64).sqrt();
        processed += frame.len();
        let time_ms = processed as u64 * 1000 / sample_rate as u64;
        let event = vad.process_chunk(frame);
        let state = vad.dump_state();

        if rms < state.off_threshold {
            quiet_start.get_or_insert(
                time_ms.saturating_sub(frame.len() as u64 * 1000 / sample_rate as u64),
            );
        } else if let Some(start_ms) = quiet_start.take() {
            if time_ms.saturating_sub(start_ms) >= 50 {
                trace.quiet_spans.push(VadQuietSpan {
                    start_ms,
                    end_ms: time_ms,
                });
            }
        }

        if event.is_boundary() {
            trace.events.push(VadTraceEvent {
                time_ms,
                reason: event.reason(),
                sentence_ms: state.sentence_samples as u64 * 1000 / sample_rate as u64,
                silence_ms: state.silence_samples as u64 * 1000 / sample_rate as u64,
            });
            vad.reset_sentence();
        } else if previous.speaking && !state.speaking {
            trace.rejected_short_sentences.push(VadTraceEvent {
                time_ms,
                reason: "min_sentence",
                sentence_ms: previous.sentence_samples as u64 * 1000 / sample_rate as u64,
                silence_ms: state.silence_samples as u64 * 1000 / sample_rate as u64,
            });
        }
        previous = vad.dump_state();

        bucket_energy += sum_squares;
        bucket_count += frame.len();
        if processed - bucket_start >= bucket_samples || processed == samples.len() {
            trace.points.push(VadTracePoint {
                time_ms,
                rms: (bucket_energy / bucket_count as f64).sqrt(),
                on: state.on_threshold,
                off: state.off_threshold,
                speaking: state.speaking,
            });
            bucket_start = processed;
            bucket_energy = 0.0;
            bucket_count = 0;
        }
    }
    if let Some(start_ms) = quiet_start
        && duration_ms.saturating_sub(start_ms) >= 50
    {
        trace.quiet_spans.push(VadQuietSpan {
            start_ms,
            end_ms: duration_ms,
        });
    }
    trace
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn soft_speech_short_sentence_is_visible_as_rejected_pause() {
        let mut vad = EnergyVad::new(16_000);
        let mut samples = vec![0.0; 16_000 / 2];
        samples.extend(vec![0.03; 16_000 / 4]);
        samples.extend(vec![0.0; 16_000 / 2]);
        let trace = trace_vad(&samples, 16_000, &mut vad);
        assert!(trace.events.is_empty());
        assert_eq!(trace.rejected_short_sentences.len(), 1);
        assert!(trace.rejected_short_sentences[0].sentence_ms < 800);
        assert!(trace.points.len() <= 1201);
        assert!(
            trace
                .quiet_spans
                .iter()
                .any(|span| span.end_ms - span.start_ms >= 300)
        );
    }
}
