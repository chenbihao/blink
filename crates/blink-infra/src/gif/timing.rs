use crate::platform::mapped_file::ReadOnlyMapping;
use std::{fs::File, sync::Mutex};

/// Immutable duration source: legacy fixtures use a shared vector, v2 recordings
/// page their validated fixed-width index through the OS, without a full frame table.
pub struct Timings {
    data: Data,
}
enum Data {
    Memory(Vec<u32>),
    Mapped {
        pages: Mutex<IndexPages>,
        count: u32,
        last: u32,
        _usage: File,
    },
}
impl std::fmt::Debug for Timings {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Timings").field("len", &self.len()).finish()
    }
}
impl Timings {
    pub fn memory(durations: Vec<u32>) -> Self {
        Self {
            data: Data::Memory(durations),
        }
    }
    pub(super) fn mapped(file: File, count: u32, last: u32, usage: File) -> std::io::Result<Self> {
        if count == 0 {
            return Ok(Self::memory(Vec::new()));
        }
        if file.metadata()?.len() < count as u64 * 32 {
            return Err(std::io::Error::other("索引尾部缺失"));
        }
        let pages = IndexPages::new(file, count)?;
        Ok(Self {
            data: Data::Mapped {
                pages: Mutex::new(pages),
                count,
                last,
                _usage: usage,
            },
        })
    }
    pub fn len(&self) -> usize {
        match &self.data {
            Data::Memory(v) => v.len(),
            Data::Mapped { count, .. } => *count as usize,
        }
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn at(&self, index: usize) -> u32 {
        assert!(index < self.len(), "validated timing index");
        match &self.data {
            Data::Memory(v) => v[index],
            Data::Mapped {
                pages, count, last, ..
            } => {
                if index + 1 == *count as usize {
                    return *last;
                }
                let mut pages = pages.lock().unwrap_or_else(|e| e.into_inner());
                let first = pages.stamp(index).expect("validated leased index page");
                let next = pages.stamp(index + 1).expect("validated leased index page");
                next.saturating_sub(first).clamp(1, u32::MAX as u64) as u32
            }
        }
    }
}

// A full-file mapping would let a sequential scan grow the process working set
// with the recording length. Keep only two 256KiB views, aligned to Windows'
// allocation granularity, regardless of how large the index becomes.
const PAGE_FRAMES: usize = 8192;
struct IndexPages {
    file: File,
    count: u32,
    cache: Vec<(usize, ReadOnlyMapping)>,
}
impl IndexPages {
    fn new(file: File, count: u32) -> std::io::Result<Self> {
        let first = ReadOnlyMapping::open_range(
            file.try_clone()?,
            0,
            (count as usize).min(PAGE_FRAMES) * 32,
        )?;
        Ok(Self {
            file,
            count,
            cache: vec![(0, first)],
        })
    }
    fn stamp(&mut self, index: usize) -> std::io::Result<u64> {
        let first = index / PAGE_FRAMES * PAGE_FRAMES;
        let page = if let Some(page) = self.cache.iter().position(|p| p.0 == first) {
            page
        } else {
            let bytes = (self.count as usize - first).min(PAGE_FRAMES) * 32;
            let view =
                ReadOnlyMapping::open_range(self.file.try_clone()?, first as u64 * 32, bytes)?;
            if self.cache.len() == 2 {
                self.cache.remove(0);
            }
            self.cache.push((first, view));
            self.cache.len() - 1
        };
        let at = (index - first) * 32 + 24;
        Ok(u64::from_le_bytes(
            self.cache[page].1.bytes()[at..at + 8].try_into().unwrap(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    #[test]
    fn index_pages_cross_boundaries_and_bound_mapped_memory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("frames.idx");
        let mut file = File::create(&path).unwrap();
        for i in 0..30000u64 {
            let mut entry = [0u8; 32];
            entry[24..32].copy_from_slice(&(i * 17).to_le_bytes());
            file.write_all(&entry).unwrap();
        }
        drop(file);
        let usage = File::create(root.path().join("usage")).unwrap();
        let timings = Timings::mapped(File::open(path).unwrap(), 30000, 19, usage).unwrap();
        for i in [0, 8191, 8192, 16383, 29998, 3, 24576, 8192] {
            assert_eq!(timings.at(i), 17);
        }
        assert_eq!(timings.at(29999), 19);
        if let Data::Mapped { pages, .. } = &timings.data {
            let pages = pages.lock().unwrap();
            assert!(pages.cache.len() <= 2);
            assert!(pages.cache.iter().map(|p| p.1.bytes().len()).sum::<usize>() <= 512 << 10);
        } else {
            panic!("expected paged index")
        }
    }
}
