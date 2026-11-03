//! Space protection is a live resource decision, independent of recording length.
use serde::Serialize;
pub const MIN_RESERVE: u64 = 256 << 20;
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub disk: Option<crate::platform::disk_space::DiskSpace>,
    pub material_bytes: u64,
    pub audio_bytes: u64,
    pub bytes_per_second: Option<u64>,
    pub reserve_bytes: u64,
    pub warning: bool,
    pub pause_reason: Option<&'static str>,
}
/// Keep enough for in-flight frames, checkpoint publication and recent growth.
pub fn reserve(queued_bytes: u64, bytes_per_second: u64) -> u64 {
    MIN_RESERVE.max(
        queued_bytes
            .saturating_mul(2)
            .saturating_add(bytes_per_second.saturating_mul(5)),
    )
}
pub fn pause_reason(available: Option<u64>, reserve: u64) -> Option<&'static str> {
    match available {
        None => Some("space_unavailable"),
        Some(n) if n < reserve => Some("low_space"),
        _ => None,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unknown_space_and_sudden_other_process_use_pause_without_discarding() {
        let r = reserve(64 << 20, 80 << 20);
        assert!(r >= 528 << 20);
        assert_eq!(pause_reason(Some(r), r), None);
        assert_eq!(pause_reason(Some(r - 1), r), Some("low_space"));
        assert_eq!(pause_reason(None, r), Some("space_unavailable"));
        assert_eq!(reserve(u64::MAX, u64::MAX), u64::MAX);
    }
}
