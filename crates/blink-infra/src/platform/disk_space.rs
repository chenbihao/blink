use std::{io, path::Path};

#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DiskSpace {
    pub volume: String,
    pub available_bytes: u64,
}

/// Query the caller's available bytes, including quotas and other applications.
#[cfg(windows)]
pub fn query(path: &Path) -> io::Result<DiskSpace> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{GetDiskFreeSpaceExW, GetVolumePathNameW},
        core::PCWSTR,
    };
    let wide: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut available = 0;
    let mut volume = vec![0u16; 32768];
    unsafe {
        GetDiskFreeSpaceExW(PCWSTR(wide.as_ptr()), Some(&mut available), None, None)
            .map_err(io::Error::other)?;
        GetVolumePathNameW(PCWSTR(wide.as_ptr()), &mut volume).map_err(io::Error::other)?;
    }
    let end = volume.iter().position(|c| *c == 0).unwrap_or(volume.len());
    Ok(DiskSpace {
        volume: String::from_utf16_lossy(&volume[..end]),
        available_bytes: available,
    })
}
#[cfg(not(windows))]
pub fn query(_path: &Path) -> io::Result<DiskSpace> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "磁盘空间查询不受支持",
    ))
}
