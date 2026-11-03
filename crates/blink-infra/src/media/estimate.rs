//! Approximate size ranges. Samples exclude audio; AAC's configured 192 kbit/s
//! is an upper allowance, since silence and simple sounds encode much smaller.
#[derive(Clone, Copy, Debug)]
pub struct SizeRange {
    pub lower: u64,
    pub upper: u64,
}

impl SizeRange {
    pub fn around(bytes: u64) -> Self {
        Self {
            lower: bytes / 2,
            upper: bytes.saturating_mul(2),
        }
    }
}

pub fn mp4_size_range(sample_bytes: u64, sample_ms: u64, total_ms: u64, audio: bool) -> SizeRange {
    let fixed = sample_bytes.min(1400);
    let factor = total_ms as f64 / sample_ms.max(1) as f64;
    let video =
        fixed.saturating_add(((sample_bytes - fixed) as f64 * factor).min(u64::MAX as f64) as u64);
    let mut range = SizeRange::around(video);
    if audio {
        // Allow for AAC packet priming and MP4 sample-table/container overhead.
        range.upper = range
            .upper
            .saturating_add(total_ms.saturating_mul(25))
            .saturating_add(32 << 10);
    }
    range
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn audio_increases_upper_allowance_without_imposing_a_false_minimum_bitrate() {
        let silent = mp4_size_range(2400, 1000, 30_000, false);
        let audio = mp4_size_range(2400, 1000, 30_000, true);
        assert_eq!(silent.lower, audio.lower);
        assert_eq!(audio.upper - silent.upper, 750_000 + (32 << 10));
        assert_eq!(silent.lower, 15_700);
    }
}
