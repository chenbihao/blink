//! v2: fixed width disk index + independently decodable, bounded data segments.
//! Readers never rewrite a recording; a checkpoint exposes only a common A/V prefix.
use super::*;
use std::sync::Mutex;

const ENTRY_BYTES: u64 = 32;
const SEGMENT_BYTES: u64 = 64 << 20;

#[derive(Clone, Copy)]
struct Entry {
    frame: u32,
    segment: u32,
    offset: u64,
    compressed: u32,
    raw: u32,
    time: u64,
}
impl Entry {
    fn bytes(self) -> [u8; 32] {
        let mut out = [0; 32];
        out[0..4].copy_from_slice(&self.frame.to_le_bytes());
        out[4..8].copy_from_slice(&self.segment.to_le_bytes());
        out[8..16].copy_from_slice(&self.offset.to_le_bytes());
        out[16..20].copy_from_slice(&self.compressed.to_le_bytes());
        out[20..24].copy_from_slice(&self.raw.to_le_bytes());
        out[24..32].copy_from_slice(&self.time.to_le_bytes());
        out
    }
    fn read(file: &mut File, frame: u32) -> Result<Self, SpoolError> {
        file.seek(SeekFrom::Start(frame as u64 * ENTRY_BYTES))?;
        let mut b = [0; 32];
        file.read_exact(&mut b)?;
        Ok(Self {
            frame: u32::from_le_bytes(b[0..4].try_into().unwrap()),
            segment: u32::from_le_bytes(b[4..8].try_into().unwrap()),
            offset: u64::from_le_bytes(b[8..16].try_into().unwrap()),
            compressed: u32::from_le_bytes(b[16..20].try_into().unwrap()),
            raw: u32::from_le_bytes(b[20..24].try_into().unwrap()),
            time: u64::from_le_bytes(b[24..32].try_into().unwrap()),
        })
    }
}
fn segment_path(dir: &Path, n: u32) -> PathBuf {
    dir.join(format!("segment-{n:08}.bin"))
}
#[derive(Serialize, Deserialize)]
struct Checkpoint {
    version: u32,
    frames: u32,
    end_ms: u64,
    #[serde(default)]
    bytes: u64,
}

pub(super) struct Writer {
    compressor: zstd::bulk::Compressor<'static>,
    dir: PathBuf,
    index: File,
    data: File,
    segment: u32,
    offset: u64,
    count: u32,
    bytes: u64,
    segment_bytes: u64,
    last: Option<u64>,
    checkpoint_at: std::time::Instant,
}
impl Writer {
    pub fn create(dir: &Path) -> Result<Self, SpoolError> {
        Ok(Self {
            compressor: zstd::bulk::Compressor::new(ZSTD_LEVEL)?,
            dir: dir.into(),
            index: File::create(dir.join("frames.idx"))?,
            data: File::create(segment_path(dir, 0))?,
            segment: 0,
            offset: 0,
            count: 0,
            bytes: 0,
            segment_bytes: SEGMENT_BYTES,
            last: None,
            checkpoint_at: std::time::Instant::now(),
        })
    }
    pub fn count(&self) -> u32 {
        self.count
    }
    pub fn bytes(&self) -> u64 {
        self.bytes + self.count as u64 * ENTRY_BYTES
    }
    pub fn last_ts(&self) -> Option<u64> {
        self.last
    }
    pub fn append(&mut self, time: u64, bgra: &[u8], expected: usize) -> Result<u64, SpoolError> {
        if bgra.len() != expected
            || expected > 64 << 20
            || self.count == u32::MAX
            || self.last.is_some_and(|last| time <= last)
        {
            return Err(SpoolError::CorruptFrame(self.count));
        }
        let bytes = self.compressor.compress(bgra)?;
        if self.offset > 0 && self.offset + bytes.len() as u64 > self.segment_bytes {
            self.data.sync_data()?;
            self.segment = self
                .segment
                .checked_add(1)
                .ok_or(SpoolError::CorruptFrame(self.count))?;
            self.data = File::create(segment_path(&self.dir, self.segment))?;
            self.offset = 0;
        }
        let entry = Entry {
            frame: self.count,
            segment: self.segment,
            offset: self.offset,
            compressed: bytes.len() as u32,
            raw: expected as u32,
            time,
        };
        // Material always precedes its index; the checkpoint follows durable A/V material.
        self.data.write_all(&bytes)?;
        self.index.write_all(&entry.bytes())?;
        self.count += 1;
        self.offset += bytes.len() as u64;
        self.bytes += bytes.len() as u64;
        self.last = Some(time);
        if self.checkpoint_at.elapsed().as_secs() >= 1 {
            self.checkpoint(time)?;
            self.checkpoint_at = std::time::Instant::now();
        }
        Ok(bytes.len() as u64)
    }
    pub fn publish_progress(&mut self, end: u64) -> Result<(), SpoolError> {
        if self.last.is_some_and(|last| end > last) && self.checkpoint_at.elapsed().as_secs() >= 1 {
            self.checkpoint(end)?;
            self.checkpoint_at = std::time::Instant::now();
        }
        Ok(())
    }
    fn checkpoint(&mut self, end: u64) -> Result<(), SpoolError> {
        self.data.sync_data()?;
        self.index.sync_data()?;
        let (end, _) = super::super::audio::recovery_end(&self.dir, end)?;
        crate::utils::fs::atomic_write_bytes(
            &self.dir.join("checkpoint.json"),
            &serde_json::to_vec(&Checkpoint {
                version: 1,
                frames: self.count,
                end_ms: end,
                bytes: self.bytes(),
            })?,
        )?;
        Ok(())
    }
    pub fn finish(&mut self, last_duration: u32) -> Result<(), SpoolError> {
        self.checkpoint(self.last.unwrap_or(0).saturating_add(if self.count > 0 {
            last_duration as u64
        } else {
            0
        }))
    }
}

pub(super) struct Index {
    file: Mutex<File>,
    count: u32,
    end: u64,
    first: u64,
}
impl Index {
    pub fn open(
        dir: &Path,
        fallback: u32,
        requested: Option<u32>,
    ) -> Result<(Self, u32, bool), SpoolError> {
        let mut file = File::open(dir.join("frames.idx"))?;
        let available = file.metadata()?.len() / ENTRY_BYTES;
        let checkpoint = match fs::read(dir.join("checkpoint.json")) {
            Ok(bytes) => serde_json::from_slice::<Checkpoint>(&bytes)
                .ok()
                .filter(|c| c.version == 1),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(e.into()),
        };
        let limit = checkpoint
            .as_ref()
            .map_or(available, |c| available.min(c.frames as u64))
            .min(u32::MAX as u64)
            .min(requested.map_or(u64::MAX, |n| n as u64));
        let mut count = 0u32;
        let mut previous: Option<Entry> = None;
        let mut segment = u32::MAX;
        let mut segment_len = 0;
        // Streaming validation: only the current entry and segment length are resident.
        while (count as u64) < limit {
            let e = Entry::read(&mut file, count)?;
            if e.segment != segment {
                segment = e.segment;
                segment_len = fs::metadata(segment_path(dir, segment))
                    .map(|v| v.len())
                    .unwrap_or(0);
            }
            if e.frame != count
                || e.raw == 0
                || e.raw > 64 << 20
                || e.compressed == 0
                || e.offset
                    .checked_add(e.compressed as u64)
                    .is_none_or(|end| end > segment_len)
                || previous.is_some_and(|p| {
                    e.time <= p.time
                        || e.segment < p.segment
                        || (e.segment == p.segment && e.offset != p.offset + p.compressed as u64)
                        || (e.segment != p.segment && (e.segment != p.segment + 1 || e.offset != 0))
                })
            {
                break;
            }
            if checkpoint.as_ref().is_some_and(|c| e.time >= c.end_ms) {
                break;
            }
            count += 1;
            previous = Some(e);
        }
        let video_end = previous.map_or(0, |e| e.time.saturating_add(fallback as u64));
        let published_end = checkpoint.as_ref().map_or(video_end, |c| {
            if count as u64 == limit {
                c.end_ms
            } else {
                c.end_ms.min(video_end)
            }
        });
        let (end, audio_recovered) = super::super::audio::recovery_end(dir, published_end)?;
        while count > 0 && Entry::read(&mut file, count - 1)?.time >= end {
            count -= 1;
        }
        let last_duration = if count > 0 {
            end.saturating_sub(Entry::read(&mut file, count - 1)?.time)
                .min(u32::MAX as u64) as u32
        } else {
            0
        };
        let recovered = audio_recovered || count as u64 != available || checkpoint.is_none();
        let first = if count > 0 {
            Entry::read(&mut file, 0)?.time
        } else {
            end
        };
        Ok((
            Self {
                file: Mutex::new(file),
                count,
                end,
                first,
            },
            last_duration,
            recovered,
        ))
    }
    pub fn count(&self) -> u32 {
        self.count
    }
    pub fn total_ms(&self) -> u64 {
        self.end.saturating_sub(self.first)
    }
    pub fn start_ms(&self) -> u64 {
        self.first
    }
    fn entry(&self, n: u32) -> Result<Entry, SpoolError> {
        if n >= self.count {
            return Err(SpoolError::FrameOutOfRange(n, self.count));
        }
        Entry::read(&mut self.file.lock().unwrap_or_else(|e| e.into_inner()), n)
    }
    pub fn timestamps(&self) -> Vec<u64> {
        // Legacy duration-table adapter; paged consumers use timing_page directly.
        (0..self.count)
            .map(|n| self.entry(n).expect("validated leased index").time)
            .collect()
    }
    pub fn read_frame(&self, dir: &Path, n: u32, max: usize) -> Result<Vec<u8>, SpoolError> {
        let e = self.entry(n)?;
        if e.raw as usize > max || e.compressed as usize > max.saturating_add(1 << 20) {
            return Err(SpoolError::CorruptFrame(n));
        }
        let mut file = File::open(segment_path(dir, e.segment))?;
        file.seek(SeekFrom::Start(e.offset))?;
        let mut decoder = zstd::stream::read::Decoder::new(file.take(e.compressed as u64))
            .map_err(|_| SpoolError::CorruptFrame(n))?;
        decoder
            .window_log_max(26)
            .map_err(|_| SpoolError::CorruptFrame(n))?;
        let mut raw = Vec::with_capacity(e.raw as usize);
        decoder.take(max as u64 + 1).read_to_end(&mut raw)?;
        if raw.len() != e.raw as usize {
            return Err(SpoolError::CorruptFrame(n));
        }
        Ok(raw)
    }
}
pub(super) fn summary(dir: &Path, fallback: u32) -> Result<(u32, u64, u64), SpoolError> {
    let mut file = File::open(dir.join("frames.idx"))?;
    let available = (file.metadata()?.len() / ENTRY_BYTES).min(u32::MAX as u64) as u32;
    let checkpoint = fs::read(dir.join("checkpoint.json"))
        .ok()
        .and_then(|b| serde_json::from_slice::<Checkpoint>(&b).ok())
        .filter(|c| c.version == 1);
    let mut count = checkpoint
        .as_ref()
        .map_or(available, |c| available.min(c.frames));
    let end = checkpoint.as_ref().map_or_else(
        || {
            if count == 0 {
                Ok(0)
            } else {
                Ok::<_, SpoolError>(
                    Entry::read(&mut file, count - 1)?
                        .time
                        .saturating_add(fallback as u64),
                )
            }
        },
        |c| Ok(c.end_ms),
    )?;
    // Lightweight summaries use binary search over fixed entries, never decompress
    // or validate every segment. Opening the chosen session performs full recovery.
    let mut low = 0;
    let mut high = count;
    while low < high {
        let middle = low + (high - low) / 2;
        if Entry::read(&mut file, middle)?.time < end {
            low = middle + 1;
        } else {
            high = middle;
        }
    }
    count = low;
    let first = if count > 0 {
        Entry::read(&mut file, 0)?.time
    } else {
        end
    };
    Ok((
        count,
        end.saturating_sub(first),
        checkpoint.map_or(0, |c| c.bytes),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn segments_are_random_access_and_partial_index_tail_is_ignored() {
        let dir = tempfile::tempdir().unwrap();
        let mut writer = Writer::create(dir.path()).unwrap();
        writer.segment_bytes = 32;
        for i in 0..20 {
            writer.append(i * 1000, &vec![i as u8; 1024], 1024).unwrap();
        }
        writer.finish(1000).unwrap();
        drop(writer);
        OpenOptions::new()
            .append(true)
            .open(dir.path().join("frames.idx"))
            .unwrap()
            .write_all(&[1, 2, 3])
            .unwrap();
        let (index, last, _) = Index::open(dir.path(), 1000, None).unwrap();
        assert_eq!(index.count(), 20);
        assert_eq!(last, 1000);
        assert_eq!(
            index.read_frame(dir.path(), 19, 1024).unwrap(),
            vec![19; 1024]
        );
        assert_eq!(
            index.read_frame(dir.path(), 0, 1024).unwrap(),
            vec![0; 1024]
        );
        assert!(segment_path(dir.path(), 1).exists());
        assert!(index.read_frame(dir.path(), 19, 512).is_err());
    }
    #[test]
    fn common_checkpoint_bounds_video_ahead_of_audio_and_preserves_long_hold() {
        let dir = tempfile::tempdir().unwrap();
        let m = super::super::super::audio::AudioManifest {
            version: 1,
            enabled: [true, false],
            committed_ms: 1500,
            complete: true,
            ..Default::default()
        };
        super::super::super::audio::write_manifest(dir.path(), &m).unwrap();
        File::create(super::super::super::audio::track_path(dir.path(), 0))
            .unwrap()
            .set_len(1500 * 192)
            .unwrap();
        let mut writer = Writer::create(dir.path()).unwrap();
        writer.append(0, &[5; 64], 64).unwrap();
        writer.append(1000, &[6; 64], 64).unwrap();
        writer.append(2000, &[7; 64], 64).unwrap();
        writer.finish(1000).unwrap();
        drop(writer);
        let (index, last, recovered) = Index::open(dir.path(), 1000, None).unwrap();
        assert_eq!(index.count(), 2);
        assert_eq!(last, 500);
        assert!(recovered);
        assert_eq!(summary(dir.path(), 1000).unwrap().0, 2);
        assert_eq!(summary(dir.path(), 1000).unwrap().1, 1500);
        assert!(summary(dir.path(), 1000).unwrap().2 > 0);
        // Read-only recovery does not truncate either source file or index.
        assert_eq!(
            fs::metadata(dir.path().join("frames.idx")).unwrap().len(),
            96
        );
    }
    #[test]
    fn missing_segment_and_half_checkpoint_recover_complete_prefix() {
        let dir = tempfile::tempdir().unwrap();
        let mut w = Writer::create(dir.path()).unwrap();
        w.segment_bytes = 1;
        for i in 0..4 {
            w.append(i * 100, &[i as u8; 64], 64).unwrap();
        }
        w.finish(100).unwrap();
        drop(w);
        fs::remove_file(segment_path(dir.path(), 2)).unwrap();
        fs::write(dir.path().join("checkpoint.json"), b"{\"frames\":").unwrap();
        let (index, last, recovered) = Index::open(dir.path(), 100, None).unwrap();
        assert_eq!(index.count(), 2);
        assert_eq!(last, 100);
        assert!(recovered);
        assert_eq!(index.read_frame(dir.path(), 1, 64).unwrap(), vec![1; 64]);
    }
}
