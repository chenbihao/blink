//! EnergyVad 的 `VadFrontend` 适配器（0.22.9 Handoff 06）。
//!
//! 将现有的 `EnergyVad`（纯 Rust RMS 能量 VAD）包装为 `VadFrontend` trait 实现，
//! 使伪流式 STT 管线可以通过依赖注入替换 VAD 种类。
//!
//! ## 设计
//!
//! `EnergyVad` 的 `process_chunk` 需要 `&mut self`，而 `VadFrontend` 要求 `&self`。
//! 使用 `std::sync::Mutex` 包装——EnergyVad 的 `process_chunk` 是纯同步计算
//!（< 1µs/chunk），Mutex 持有时间极短，不会成为瓶颈。
//!
//! ## 不降低能力
//!
//! 此适配器不修改 `EnergyVad` 的任何逻辑——参数、行为和测试全部保持不变。
//! 只是在其外面加了一层 trait 适配。

use std::sync::Mutex;

use crate::domain::stt::vad::{EnergyVad, VadEvent};
use crate::domain::stt::vad_port::VadFrontend;

/// `EnergyVad` 的 `VadFrontend` 适配器。
///
/// 包装 `EnergyVad` 使其满足 `VadFrontend` trait（`Send + Sync`）。
#[allow(dead_code)] // Handoff 06: gate-held, not yet wired into production
pub struct EnergyVadAdapter {
    inner: Mutex<EnergyVad>,
}

#[allow(dead_code)] // Handoff 06: gate-held
impl EnergyVadAdapter {
    /// 从 `VadConfig` 参数创建适配器。
    #[allow(dead_code)]
    pub fn new(
        sample_rate: u32,
        silence_threshold: f64,
        min_silence_ms: u32,
        min_sentence_ms: u32,
    ) -> Self {
        Self {
            inner: Mutex::new(EnergyVad::with_params(
                sample_rate,
                silence_threshold,
                min_silence_ms,
                min_sentence_ms,
            )),
        }
    }

    /// 从已有的 `EnergyVad` 实例创建适配器（测试用）。
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn from_vad(vad: EnergyVad) -> Self {
        Self {
            inner: Mutex::new(vad),
        }
    }
}

impl VadFrontend for EnergyVadAdapter {
    fn process_chunk(&self, samples: &[f32]) -> VadEvent {
        let mut vad = self.inner.lock().unwrap();
        vad.process_chunk(samples)
    }

    fn reset_sentence(&self) {
        let mut vad = self.inner.lock().unwrap();
        vad.reset_sentence();
    }

    fn reset(&self) {
        let mut vad = self.inner.lock().unwrap();
        vad.reset();
    }

    fn name(&self) -> &'static str {
        "energy"
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_RATE: u32 = 16000;

    fn generate_tone(duration_ms: u32, amplitude: f32) -> Vec<f32> {
        let n = (duration_ms as u64 * SAMPLE_RATE as u64 / 1000) as usize;
        (0..n)
            .map(|i| {
                let t = i as f32 / SAMPLE_RATE as f32;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * amplitude
            })
            .collect()
    }

    fn generate_silence(duration_ms: u32) -> Vec<f32> {
        let n = (duration_ms as u64 * SAMPLE_RATE as u64 / 1000) as usize;
        vec![0.0; n]
    }

    #[test]
    fn adapter_name_is_energy() {
        let vad = EnergyVadAdapter::new(SAMPLE_RATE, 0.005, 300, 800);
        assert_eq!(vad.name(), "energy");
    }

    #[test]
    fn adapter_detects_sentence_end() {
        let vad = EnergyVadAdapter::new(SAMPLE_RATE, 0.005, 300, 800);

        // 说话 1s
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }

        // 静默 400ms → 句尾
        let silence = generate_silence(400);
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(got_end, "适配器应正确检测句尾");
    }

    #[test]
    fn adapter_reset_clears_state() {
        let vad = EnergyVadAdapter::new(SAMPLE_RATE, 0.005, 300, 800);

        // 积累状态
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }

        // reset
        vad.reset();

        // reset 后纯静默不应触发句尾
        let silence = generate_silence(1000);
        let mut events = 0;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                events += 1;
            }
        }
        assert_eq!(events, 0, "reset 后不应有残留状态");
    }

    #[test]
    fn adapter_reset_sentence_after_end() {
        let vad = EnergyVadAdapter::new(SAMPLE_RATE, 0.005, 300, 800);

        // 句子1
        let speech = generate_tone(1000, 0.1);
        for chunk in speech.chunks(160) {
            vad.process_chunk(chunk);
        }
        let silence = generate_silence(400);
        let mut end_count = 0;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                end_count += 1;
                vad.reset_sentence();
            }
        }
        assert_eq!(end_count, 1);

        // 句子2——reset_sentence 后应能再次触发
        let speech2 = generate_tone(1000, 0.1);
        for chunk in speech2.chunks(160) {
            vad.process_chunk(chunk);
        }
        let silence2 = generate_silence(400);
        for chunk in silence2.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                end_count += 1;
                vad.reset_sentence();
            }
        }
        assert_eq!(end_count, 2, "应能多次检测句尾");
    }
}
