//! 录屏音频素材：固定 48kHz 双声道 PCM16；独立源轨、共同有效时钟。
//! 音频不进入 STT，不通过 JSON IPC 传样本。文件随 spool 使用租约一起保护。
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Instant,
};

pub const RATE: u32 = 48_000;
pub const BYTES_PER_MS: u64 = 192;
/// Arithmetic bound, independent of product duration.
pub const MAX_AUDIO_MS: u64 = u64::MAX / BYTES_PER_MS;

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioOptions {
    #[serde(default)]
    pub mode: AudioMode,
    pub microphone: Option<String>,
    pub system: Option<String>,
}
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioMode {
    #[default]
    Silent,
    Microphone,
    System,
    Both,
}
impl AudioOptions {
    pub fn enabled(&self, source: usize) -> bool {
        matches!(
            (self.mode, source),
            (AudioMode::Both, 0 | 1) | (AudioMode::Microphone, 0) | (AudioMode::System, 1)
        )
    }
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TrackInfo {
    pub source: usize,
    pub device_id: String,
    pub name: String,
    pub native_rate: u32,
    pub native_channels: u16,
}
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioManifest {
    pub version: u32,
    pub enabled: [bool; 2],
    pub committed_ms: u64,
    pub segments: Vec<(u64, Vec<TrackInfo>)>,
    pub muted: [Vec<(u64, u64)>; 2],
    pub complete: bool,
}
#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AudioStatus {
    pub tracks: Vec<TrackInfo>,
    pub muted: [bool; 2],
    pub rms: [f32; 2],
    pub peak: [f32; 2],
    pub error: Option<String>,
    pub active: bool,
}
pub struct Gate {
    pub meter_updated: [Option<Instant>; 2],
    pub start: Option<Instant>,
    pub offset_ms: u64,
    pub preview: bool,
    pub status: AudioStatus,
    pub options: AudioOptions,
    /// 时间边界静音：回调迟到时仍按样本所属时间门控。
    pub mute_edges: [Vec<(u64, bool)>; 2],
}
impl Gate {
    pub fn snapshot(&mut self) -> AudioStatus {
        for i in 0..2 {
            if self.meter_updated[i].is_none_or(|at| at.elapsed().as_millis() > 200) {
                self.status.rms[i] = 0.0;
                self.status.peak[i] = 0.0;
            }
        }
        let status = self.status.clone();
        self.status.peak = [0.0; 2];
        status
    }
    pub fn time_ms(&self, now: Instant) -> u64 {
        self.offset_ms
            + self
                .start
                .map(|s| now.saturating_duration_since(s).as_millis() as u64)
                .unwrap_or(0)
    }
    /// PCM 排他边界保留采样精度，避免逐块截断到毫秒形成周期性静音空洞。
    pub fn frame_end(&self, now: Instant) -> u64 {
        let elapsed = self.start
            .map(|start| now.saturating_duration_since(start).as_nanos())
            .unwrap_or(0);
        (u128::from(self.offset_ms) * 48 + (elapsed * u128::from(RATE)).div_ceil(1_000_000_000))
            .min(u128::from(MAX_AUDIO_MS) * 48) as u64
    }
    pub fn muted_at(&self, source: usize, ms: u64) -> bool {
        self.mute_edges[source]
            .iter()
            .rev()
            .find(|(at, _)| *at <= ms)
            .is_some_and(|(_, m)| *m)
    }
    pub fn fail(&mut self, message: &str) {
        self.status.error = Some(message.into());
        self.status.active = false;
        self.status.rms = [0.0; 2];
        self.status.peak = [0.0; 2];
    }
}
pub type SharedAudio = Arc<Mutex<Gate>>;
pub fn shared(options: AudioOptions) -> SharedAudio {
    Arc::new(Mutex::new(Gate {
        start: None,
        offset_ms: 0,
        preview: false,
        status: AudioStatus::default(),
        options,
        mute_edges: Default::default(),
        meter_updated: [None; 2],
    }))
}
pub fn levels(samples: &[f32]) -> (f32, f32) {
    let level = crate::platform::audio::signal::measure(samples);
    (level.rms as f32, level.peak)
}
/// 样本级时钟/隐私门控。只接受本轮有效区间内且已经发生的声音；排队前抹掉静音样本。
pub fn canonical_samples(
    input: &[f32],
    channels: u16,
    rate: u32,
    begin: i64,
    lower: u64,
    upper: u64,
    muted: impl Fn(u64) -> bool,
) -> (u64, Vec<i16>, (f32, f32)) {
    let count = input.len() / channels.max(1) as usize;
    if count == 0 || rate == 0 || channels == 0 {
        return (lower, Vec::new(), (0.0, 0.0));
    }
    let output_count = ((count as f64 * RATE as f64 / rate as f64).round() as usize).max(1);
    let start = begin.max(lower.min(i64::MAX as u64) as i64).max(0) as u64;
    let mut samples = Vec::with_capacity(output_count.min(RATE as usize) * 2);
    let mut meter = Vec::with_capacity(samples.capacity());
    let skip = (start as i128 - begin as i128)
        .max(0)
        .min(output_count as i128) as usize;
    for n in skip..output_count {
        let frame = begin + n as i64;
        if frame < 0 || frame as u64 >= upper {
            break;
        }
        let idx = n as f64 * rate as f64 / RATE as f64;
        let a = (idx.floor() as usize).min(count - 1);
        let b = (a + 1).min(count - 1);
        let f = idx.fract() as f32;
        for c in 0..2 {
            let channel = c.min(channels as usize - 1);
            let at = |frame: usize| {
                if channels <= 2 {
                    input[frame * channels as usize + channel]
                } else {
                    input[frame * channels as usize..(frame + 1) * channels as usize]
                        .iter()
                        .sum::<f32>()
                        / channels as f32
                }
            };
            let v = if muted(frame as u64 / 48) {
                0.0
            } else {
                at(a) * (1.0 - f) + at(b) * f
            };
            let v = if v.is_finite() {
                v.clamp(-1.0, 1.0)
            } else {
                0.0
            };
            meter.push(v);
            samples.push((v * 32767.0).round() as i16);
        }
    }
    (start, samples, levels(&meter))
}
pub fn track_path(dir: &Path, source: usize) -> PathBuf {
    dir.join(if source == 0 {
        "microphone.pcm"
    } else {
        "system.pcm"
    })
}
pub fn write_manifest(dir: &Path, manifest: &AudioManifest) -> std::io::Result<()> {
    crate::utils::fs::atomic_write_bytes(&dir.join("audio.json"), &serde_json::to_vec(manifest)?)
        .map(|_| ())
}
pub fn manifest(dir: &Path) -> std::io::Result<Option<AudioManifest>> {
    let path = dir.join("audio.json");
    match std::fs::read(path) {
        Ok(bytes) => {
            let m: AudioManifest = serde_json::from_slice(&bytes)?;
            if m.version != 1 || m.committed_ms > MAX_AUDIO_MS {
                return Err(std::io::Error::other("音频素材版本或时长无效"));
            }
            Ok(Some(m))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            if (0..2).any(|i| track_path(dir, i).exists()) {
                return Err(std::io::Error::other("声音元数据缺失"));
            }
            Ok(None)
        }
        Err(e) => Err(e),
    }
}
/// 恢复共同前缀；缺失/半写声音不可伪装成正常静音。
pub fn recovery_end(dir: &Path, video_ms: u64) -> std::io::Result<(u64, bool)> {
    let Some(m) = manifest(dir)? else {
        return Ok((video_ms, false));
    };
    let mut end = video_ms.min(m.committed_ms);
    for (i, enabled) in m.enabled.iter().enumerate() {
        if *enabled {
            end = end.min(
                std::fs::metadata(track_path(dir, i))
                    .map(|v| v.len() / BYTES_PER_MS)
                    .unwrap_or(0),
            );
        }
    }
    Ok((end, !m.complete || end < video_ms))
}
/// 读取已提交的源轨 WAV（短素材预算）；二进制 IPC，不做 base64/JSON。
pub fn read_wav(dir: &Path, source: usize, end_ms: u64) -> std::io::Result<Vec<u8>> {
    if source > 1 || end_ms > MAX_AUDIO_MS {
        return Err(std::io::Error::other("音轨读取超限"));
    }
    let size = u32::try_from(end_ms * BYTES_PER_MS)
        .map_err(|_| std::io::Error::other("整轨读取超出内存预算，请使用 Range"))?;
    if size > 16 << 20 {
        return Err(std::io::Error::other("整轨读取超出内存预算，请使用 Range"));
    }
    let mut file = File::open(track_path(dir, source))?;
    let mut out = wav_header(size);
    Read::by_ref(&mut file)
        .take(size as u64)
        .read_to_end(&mut out)?;
    if out.len() != size as usize + 44 {
        return Err(std::io::Error::other("音轨尾部缺失"));
    }
    Ok(out)
}
/// 浏览器 Range 读取只驻留请求页；无 Range 的兼容请求仍受短素材大小上限保护。
pub fn read_wav_range(
    dir: &Path,
    source: usize,
    end_ms: u64,
    range: Option<&str>,
) -> std::io::Result<(Vec<u8>, u64, u64, u64)> {
    if source > 1 || end_ms > MAX_AUDIO_MS {
        return Err(std::io::Error::other("音轨读取超限"));
    }
    let pcm_size = end_ms * BYTES_PER_MS;
    let header = wav_header_long(pcm_size);
    let header_len = header.len() as u64;
    let total = pcm_size
        .checked_add(header_len)
        .ok_or_else(|| std::io::Error::other("音轨长度溢出"))?;
    let (start, end) = wav_range(range, total)?;
    let mut bytes = Vec::with_capacity((end - start + 1) as usize);
    if start < header_len {
        bytes.extend(&header[start as usize..(end + 1).min(header_len) as usize]);
    }
    if end >= header_len {
        let mut file = File::open(track_path(dir, source))?;
        if file.metadata()?.len() < pcm_size {
            return Err(std::io::Error::other("音轨尾部缺失"));
        }
        let pcm_start = start.max(header_len) - header_len;
        file.seek(SeekFrom::Start(pcm_start))?;
        Read::by_ref(&mut file)
            .take(end - start.max(header_len) + 1)
            .read_to_end(&mut bytes)?;
    }
    if bytes.len() as u64 != end - start + 1 {
        return Err(std::io::Error::other("音轨读取不完整"));
    }
    Ok((bytes, start, end, total))
}
fn wav_range(range: Option<&str>, total: u64) -> std::io::Result<(u64, u64)> {
    let invalid = || std::io::Error::other("音轨 Range 无效");
    let Some(range) = range else {
        if total > 1 << 20 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "长音轨请求必须使用 Range",
            ));
        }
        return Ok((0, total - 1));
    };
    let (from, to) = range
        .strip_prefix("bytes=")
        .ok_or_else(invalid)?
        .split_once('-')
        .ok_or_else(invalid)?;
    let start = if from.is_empty() {
        total.saturating_sub(to.parse::<u64>().map_err(|_| invalid())?)
    } else {
        from.parse::<u64>().map_err(|_| invalid())?
    };
    let end = if from.is_empty() || to.is_empty() {
        total - 1
    } else {
        to.parse::<u64>().map_err(|_| invalid())?.min(total - 1)
    };
    if start >= total || end < start {
        return Err(invalid());
    }
    Ok((start, end.min(start.saturating_add((1 << 20) - 1))))
}
pub fn wav_header(size: u32) -> Vec<u8> {
    let mut b = Vec::with_capacity(44);
    b.extend(b"RIFF");
    b.extend(size.saturating_add(36).to_le_bytes());
    b.extend(b"WAVEfmt ");
    b.extend(16u32.to_le_bytes());
    b.extend(1u16.to_le_bytes());
    b.extend(2u16.to_le_bytes());
    b.extend(RATE.to_le_bytes());
    b.extend((RATE * 4).to_le_bytes());
    b.extend(4u16.to_le_bytes());
    b.extend(16u16.to_le_bytes());
    b.extend(b"data");
    b.extend(size.to_le_bytes());
    b
}
/// RF64 removes RIFF's 4GiB length ceiling while retaining canonical PCM.
pub fn wav_header_long(size: u64) -> Vec<u8> {
    if size <= (u32::MAX - 36) as u64 {
        return wav_header(size as u32);
    }
    let mut b = Vec::with_capacity(80);
    b.extend(b"RF64");
    b.extend(u32::MAX.to_le_bytes());
    b.extend(b"WAVEds64");
    b.extend(28u32.to_le_bytes());
    b.extend(size.saturating_add(72).to_le_bytes());
    b.extend(size.to_le_bytes());
    b.extend((size / 4).to_le_bytes());
    b.extend(0u32.to_le_bytes());
    let classic = wav_header(0);
    b.extend_from_slice(&classic[12..36]);
    b.extend(b"data");
    b.extend(u32::MAX.to_le_bytes());
    b
}
pub fn write_packet(file: &mut File, start_frame: u64, samples: &[i16]) -> std::io::Result<()> {
    if samples.len() > RATE as usize * 2
        || start_frame
            .checked_add((samples.len() / 2) as u64)
            .is_none_or(|end| end > u64::MAX / 4)
    {
        return Err(std::io::Error::other("录制音轨达到时长上限"));
    }
    file.seek(SeekFrom::Start(start_frame * 4))?;
    let bytes: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    file.write_all(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rf64_keeps_u64_sizes_and_large_reads_require_bounded_ranges() {
        let size = u32::MAX as u64 + 1;
        let header = wav_header_long(size);
        assert_eq!(header.len(), 80);
        assert_eq!(&header[..4], b"RF64");
        assert_eq!(u64::from_le_bytes(header[28..36].try_into().unwrap()), size);
        assert_eq!(
            u64::from_le_bytes(header[36..44].try_into().unwrap()),
            size / 4
        );
        assert!(wav_range(None, 2 << 20).is_err());
        assert_eq!(
            wav_range(Some("bytes=0-"), 10 << 20).unwrap(),
            (0, (1 << 20) - 1)
        );
        assert_eq!(
            wav_range(Some("bytes=1048576-"), 10 << 20).unwrap(),
            (1 << 20, (2 << 20) - 1)
        );
    }

    #[test]
    fn levels_are_numeric_and_silence_is_zero() {
        assert_eq!(levels(&[0.0; 64]), (0.0, 0.0));
        assert_eq!(levels(&[0.5, -0.5]), (0.5, 0.5));
        assert!(levels(&[f32::NAN, f32::INFINITY]).0.is_finite());
    }
    #[test]
    fn late_packets_follow_mute_edges() {
        let state = shared(AudioOptions::default());
        let mut g = state.lock().unwrap();
        g.mute_edges[0] = vec![(20, true), (40, false)];
        assert!(!g.muted_at(0, 19));
        assert!(g.muted_at(0, 20));
        assert!(g.muted_at(0, 39));
        assert!(!g.muted_at(0, 40));
    }
    #[test]
    fn missing_or_half_written_track_truncates_common_prefix() {
        let dir = tempfile::tempdir().unwrap();
        write_manifest(
            dir.path(),
            &AudioManifest {
                version: 1,
                enabled: [true, true],
                committed_ms: 100,
                complete: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(recovery_end(dir.path(), 100).unwrap(), (0, true));
        std::fs::write(track_path(dir.path(), 0), vec![0; 100 * 192]).unwrap();
        std::fs::write(track_path(dir.path(), 1), vec![0; 75 * 192 + 2]).unwrap();
        assert_eq!(recovery_end(dir.path(), 100).unwrap(), (75, true));
    }
    #[test]
    fn mute_removes_inflight_samples_before_storage_and_unmute_only_restores_future() {
        let state = shared(AudioOptions::default());
        let mut g = state.lock().unwrap();
        g.mute_edges[0] = vec![(20, true), (40, false)];
        let (_, pcm, _) = canonical_samples(&vec![0.5; 2880], 1, RATE, 0, 0, 2880, |ms| {
            g.muted_at(0, ms)
        });
        assert!(pcm[..20 * 48 * 2].iter().all(|v| *v > 0));
        assert!(pcm[20 * 48 * 2..40 * 48 * 2].iter().all(|v| *v == 0));
        assert!(pcm[40 * 48 * 2..].iter().all(|v| *v > 0));
        let (_, silent, level) = canonical_samples(&[1.0; 480], 1, RATE, 0, 0, 480, |_| true);
        assert!(silent.iter().all(|v| *v == 0));
        assert_eq!(level, (0.0, 0.0));
    }
    #[test]
    fn callback_frame_boundary_keeps_submillisecond_samples() {
        let state = shared(AudioOptions::default());
        let mut gate = state.lock().unwrap();
        let start = Instant::now();
        gate.start = Some(start);
        gate.offset_ms = 100;
        let now = start + std::time::Duration::from_micros(200_751);
        assert_eq!(gate.time_ms(now), 300);
        assert_eq!(gate.frame_end(now), 14_437);
        let begin = ((100.0 + 200.751 - 10.0) * 48.0_f64).round() as i64;
        let (_, pcm, level) = canonical_samples(&[0.5; 480], 1, RATE, begin, 4800, gate.frame_end(now), |_| false);
        assert_eq!(pcm.len(), 960, "the entire occurred 10ms packet must survive clock gating");
        assert_eq!(level, (0.5, 0.5));
    }
    #[test]
    fn sample_clock_clips_before_resume_and_future_buffers() {
        let (start, pcm, _) = canonical_samples(&[0.5; 480], 1, RATE, 0, 120, 360, |_| false);
        assert_eq!(start, 120);
        assert_eq!(pcm.len(), (360 - 120) * 2);
    }
    #[test]
    fn wav_ranges_cover_header_pcm_and_bound_open_ranges() {
        let dir = tempfile::tempdir().unwrap();
        let pcm = vec![12u8; 1920];
        std::fs::write(track_path(dir.path(), 0), &pcm).unwrap();
        let full = read_wav(dir.path(), 0, 10).unwrap();
        for range in ["bytes=0-43", "bytes=32-63", "bytes=44-", "bytes=-40"] {
            let (bytes, start, end, total) =
                read_wav_range(dir.path(), 0, 10, Some(range)).unwrap();
            assert_eq!(bytes, full[start as usize..=end as usize]);
            assert_eq!(total, 1964);
        }
        assert_eq!(
            wav_range(Some("bytes=0-"), 20 << 20).unwrap(),
            (0, (1 << 20) - 1)
        );
        assert!(wav_range(Some("bytes=9-3"), 20).is_err());
        assert!(wav_range(Some("bytes=0-3,5-7"), 20).is_err());
    }
}
