//! Read-only mapping of an immutable, leased recording index.
use std::{fs::File, io};

#[cfg(windows)]
pub(crate) struct ReadOnlyMapping {
    _file: File,
    handle: windows::Win32::Foundation::HANDLE,
    view: windows::Win32::System::Memory::MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
}
#[cfg(windows)]
impl ReadOnlyMapping {
    pub(crate) fn open_range(file: File, offset: u64, len: usize) -> io::Result<Self> {
        use std::os::windows::io::AsRawHandle;
        use windows::{
            Win32::{
                Foundation::{CloseHandle, HANDLE},
                System::Memory::*,
            },
            core::PCWSTR,
        };
        if len == 0
            || offset
                .checked_add(len as u64)
                .is_none_or(|end| end > file.metadata().map(|m| m.len()).unwrap_or(0))
        {
            return Err(io::Error::other("索引映射范围无效"));
        }
        let handle = unsafe {
            CreateFileMappingW(
                HANDLE(file.as_raw_handle()),
                None,
                PAGE_READONLY,
                0,
                0,
                PCWSTR::null(),
            )
        }
        .map_err(io::Error::other)?;
        let view = unsafe {
            MapViewOfFile(
                handle,
                FILE_MAP_READ,
                (offset >> 32) as u32,
                offset as u32,
                len,
            )
        };
        if view.Value.is_null() {
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(io::Error::last_os_error());
        }
        Ok(Self {
            _file: file,
            handle,
            view,
            len,
        })
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        // The owned file/mapping remain alive; callers retain the recording's usage
        // lease and never mutate/truncate this index while a timeline is open.
        unsafe { std::slice::from_raw_parts(self.view.Value.cast(), self.len) }
    }
}
#[cfg(windows)]
// All access is read-only; the view is immutable and independent of thread affinity.
unsafe impl Send for ReadOnlyMapping {}
#[cfg(windows)]
unsafe impl Sync for ReadOnlyMapping {}
#[cfg(windows)]
impl Drop for ReadOnlyMapping {
    fn drop(&mut self) {
        unsafe {
            let _ = windows::Win32::System::Memory::UnmapViewOfFile(self.view);
            let _ = windows::Win32::Foundation::CloseHandle(self.handle);
        }
    }
}

#[cfg(not(windows))]
pub(crate) struct ReadOnlyMapping(Vec<u8>);
#[cfg(not(windows))]
impl ReadOnlyMapping {
    pub(crate) fn open_range(mut file: File, offset: u64, len: usize) -> io::Result<Self> {
        use std::io::{Read, Seek, SeekFrom};
        file.seek(SeekFrom::Start(offset))?;
        let mut bytes = vec![0; len];
        file.read_exact(&mut bytes)?;
        Ok(Self(bytes))
    }
    pub(crate) fn bytes(&self) -> &[u8] {
        &self.0
    }
}
