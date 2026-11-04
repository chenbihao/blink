//! 语音波形与录屏电平共用的能量测量；不做 VAD 或录音门控。
#[derive(Clone, Copy, Debug, Default)]
pub struct SignalLevel {
    pub rms: f64,
    pub peak: f32,
}

pub fn measure(samples: &[f32]) -> SignalLevel {
    if samples.is_empty() {
        return SignalLevel::default();
    }
    let mut sum = 0.0f64;
    let mut peak = 0.0f32;
    for &sample in samples {
        let value = if sample.is_finite() {
            sample.clamp(-1.0, 1.0)
        } else {
            0.0
        };
        sum += f64::from(value).powi(2);
        peak = peak.max(value.abs());
    }
    SignalLevel {
        rms: (sum / samples.len() as f64).sqrt(),
        peak,
    }
}

/// 统一视觉反馈曲线：保留真实 RMS，独立增强安静说话的可见程度。
pub fn feedback_level(rms: f64) -> f64 {
    const FLOOR: f64 = 0.001;
    if !rms.is_finite() || rms <= FLOOR {
        return 0.0;
    }
    ((rms - FLOOR) / (0.15 - FLOOR)).clamp(0.0, 1.0).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn known_energy_silence_and_invalid_samples() {
        let level = measure(&[0.5, -0.5]);
        assert_eq!(level.rms, 0.5);
        assert_eq!(level.peak, 0.5);
        assert_eq!(measure(&[]).rms, 0.0);
        assert_eq!(measure(&[f32::NAN, f32::INFINITY]).peak, 0.0);
        assert_eq!(feedback_level(0.0005), 0.0);
        assert!(feedback_level(0.01) > 0.2);
        assert_eq!(feedback_level(1.0), 1.0);
    }
}
