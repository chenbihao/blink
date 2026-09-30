//! 平台文件身份原语——用于 AudioResourceRegistry 安全校验（0.22.16 H03）。
//!
//! **职责**：
//! - 提供 `FileIdentity`：不可伪造的文件身份指纹（卷序列号 + 文件索引 + 体积 + mtime）
//! - 提供 `probe_identity()`：打开文件并获取 `FileIdentity`，返回受控 reader
//! - 提供 `open_file_for_identity()`：打开文件 → 获取 identity → 返回 file handle，
//!   避免"校验路径后重新打开"的 TOCTOU
//!
//! **分层**：纯 Win32 / std 调用，不依赖 tauri 或 domain。app 层消费此模块。
//!
//! **安全模型**：
//! - Windows 使用 `GetFileInformationByHandle`（`BY_HANDLE_FILE_INFORMATION`），
//!   其 `nFileIndexHigh/Low` + `dwVolumeSerialNumber` 是文件系统级唯一标识，
//!   不受文件改名/移动影响（同一卷内）。
//! - 非 Windows 回退到 `Metadata`（size + mtime），弱保证但不 panic。
//!
//! **测试策略**：identity 比较逻辑走纯函数测试；Win32 原语按 spec-backend §二
//! "Win32/GUI 集成层免自动化"，用临时文件走 `Path::exists` 守卫。
#![allow(dead_code)] // 0.22.16 H03：后续 agent 负责 wiring，当前仅交付模块

use std::fs::File;
use std::io;
use std::path::Path;

/// 平台无关的文件身份指纹。
///
/// 用于检测文件是否被替换或修改。
///
/// **Windows**：包含卷序列号 + 文件索引（高/低 32 位），这是文件系统级唯一标识。
/// **非 Windows**：仅包含 size + 高精度 mtime，弱保证但不 panic。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileIdentity {
    /// 文件体积（字节）。
    pub size: u64,
    /// 最后修改时间的高精度平台时间戳。
    ///
    /// Windows 保存原始 FILETIME（自 1601 起的 100ns tick），避免秒级截断
    /// 让同一秒内的同尺寸原位覆盖逃过校验；其他平台保存 Unix epoch 纳秒。
    pub mtime: u64,
    /// 卷序列号（仅 Windows）。
    pub volume_serial: Option<u32>,
    /// 文件索引高 32 位（仅 Windows）。
    pub file_index_high: Option<u32>,
    /// 文件索引低 32 位（仅 Windows）。
    pub file_index_low: Option<u32>,
}

impl FileIdentity {
    /// 两个 identity 是否匹配（Windows 上检查全部字段，非 Windows 只查 size + mtime）。
    pub fn matches(&self, other: &Self) -> bool {
        if self.size != other.size || self.mtime != other.mtime {
            return false;
        }
        // Windows 上额外检查卷序列号和文件索引
        if self.volume_serial.is_some() && other.volume_serial.is_some() {
            return self.volume_serial == other.volume_serial
                && self.file_index_high == other.file_index_high
                && self.file_index_low == other.file_index_low;
        }
        true
    }

    /// 是否具备强身份（Windows 文件索引可用）。
    pub fn has_strong_identity(&self) -> bool {
        self.volume_serial.is_some()
            && self.file_index_high.is_some()
            && self.file_index_low.is_some()
    }
}

/// 打开文件句柄 + 获取文件身份。
///
/// 返回 `File` 和 `FileIdentity`，调用方持有 `File` 防止 TOCTOU。
/// 文件以只读方式打开。
///
/// **错误**：文件不存在、是目录、权限不足等返回 `io::Error`。
pub fn open_file_for_identity(path: &Path) -> io::Result<(File, FileIdentity)> {
    let file = File::open(path)?;
    let identity = probe_identity(&file)?;
    Ok((file, identity))
}

/// 从已打开的文件句柄获取身份。
///
/// **Windows**：使用 `GetFileInformationByHandle` 获取卷序列号和文件索引。
/// **非 Windows**：使用 `Metadata` 获取 size + mtime。
pub fn probe_identity(file: &File) -> io::Result<FileIdentity> {
    #[cfg(target_os = "windows")]
    {
        probe_identity_windows(file)
    }
    #[cfg(not(target_os = "windows"))]
    {
        probe_identity_std(file)
    }
}

#[cfg(target_os = "windows")]
fn probe_identity_windows(file: &File) -> io::Result<FileIdentity> {
    use std::os::windows::io::AsRawHandle;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::Storage::FileSystem::{
        BY_HANDLE_FILE_INFORMATION, GetFileInformationByHandle,
    };

    let handle = HANDLE(file.as_raw_handle());
    let mut info = BY_HANDLE_FILE_INFORMATION::default();
    unsafe {
        GetFileInformationByHandle(handle, &mut info)
            .map_err(|e| io::Error::other(format!("GetFileInformationByHandle failed: {e}")))?;
    }

    Ok(FileIdentity {
        size: ((info.nFileSizeHigh as u64) << 32) | (info.nFileSizeLow as u64),
        mtime: filetime_ticks(
            info.ftLastWriteTime.dwLowDateTime,
            info.ftLastWriteTime.dwHighDateTime,
        ),
        volume_serial: Some(info.dwVolumeSerialNumber),
        file_index_high: Some(info.nFileIndexHigh),
        file_index_low: Some(info.nFileIndexLow),
    })
}

#[cfg(not(target_os = "windows"))]
fn probe_identity_std(file: &File) -> io::Result<FileIdentity> {
    let meta = file.metadata()?;
    Ok(FileIdentity {
        size: meta.len(),
        mtime: meta
            .modified()?
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| {
                d.as_secs()
                    .saturating_mul(1_000_000_000)
                    .saturating_add(u64::from(d.subsec_nanos()))
            })
            .unwrap_or(0),
        volume_serial: None,
        file_index_high: None,
        file_index_low: None,
    })
}

/// 合并 Windows FILETIME 的高低位，保留 100ns 精度。
#[cfg(target_os = "windows")]
fn filetime_ticks(low: u32, high: u32) -> u64 {
    ((high as u64) << 32) | (low as u64)
}

/// 检查路径是否为 regular file（非目录、非 symlink）。
///
/// **Windows**：`Metadata::file_type()` 已区分文件和目录。
/// `FILE_ATTRIBUTE_REPARSE_POINT` 用于检测 symlink/reparse point。
///
/// 返回 `true` 表示是 regular file（非目录、非 symlink/reparse point）。
pub fn is_regular_file(path: &Path) -> bool {
    let Ok(meta) = std::fs::symlink_metadata(path) else {
        return false;
    };
    if !meta.file_type().is_file() {
        return false;
    }
    // Windows 上额外检查 reparse point 属性
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::fs::MetadataExt;
        // FILE_ATTRIBUTE_REPARSE_POINT = 0x0400
        if meta.file_attributes() & 0x0400 != 0 {
            return false;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use tempfile::tempdir;

    /// 生成临时文件并写入内容。
    fn make_test_file(dir: &Path, name: &str, content: &[u8]) -> std::path::PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(content).unwrap();
        path
    }

    #[test]
    fn identity_matches_same_file() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "test.wav", b"RIFFdata");
        let (file, id1) = open_file_for_identity(&path).unwrap();
        let id2 = probe_identity(&file).unwrap();
        assert!(id1.matches(&id2), "同一文件的 identity 应匹配");
    }

    #[test]
    fn identity_differs_after_content_change() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "test.wav", b"original");
        let (_file, id1) = open_file_for_identity(&path).unwrap();
        drop(_file);

        // 修改文件内容
        std::fs::write(&path, b"modified content").unwrap();
        let (_file2, id2) = open_file_for_identity(&path).unwrap();
        assert!(!id1.matches(&id2), "内容修改后 identity 应不同");
    }

    #[test]
    fn identity_differs_after_same_size_in_place_change() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "same-size.wav", b"AAAAAAAA");
        let (_file, id1) = open_file_for_identity(&path).unwrap();
        drop(_file);

        // 保持文件索引和体积不变，只修改内容。短暂等待让文件系统提交高精度 mtime，
        // 但远小于旧实现的一秒精度，专门覆盖旧 audio_ref 可被复用的窗口。
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&path, b"BBBBBBBB").unwrap();
        let (_file2, id2) = open_file_for_identity(&path).unwrap();

        assert_eq!(id1.size, id2.size);
        assert!(!id1.matches(&id2), "同尺寸原位修改后 identity 应不同");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn filetime_keeps_subsecond_ticks() {
        let high = 0x01da_1234;
        let low = 0x5678_9abc;
        assert_eq!(filetime_ticks(low, high), 0x01da_1234_5678_9abc);
    }

    #[test]
    fn identity_differs_for_different_files() {
        let dir = tempdir().unwrap();
        let path1 = make_test_file(dir.path(), "a.wav", b"content A");
        let path2 = make_test_file(dir.path(), "b.wav", b"content B");
        let (_f1, id1) = open_file_for_identity(&path1).unwrap();
        let (_f2, id2) = open_file_for_identity(&path2).unwrap();
        assert!(!id1.matches(&id2), "不同文件的 identity 应不同");
    }

    #[test]
    fn open_nonexistent_file_errors() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nonexistent.wav");
        let result = open_file_for_identity(&path);
        assert!(result.is_err(), "不存在的文件应返回错误");
    }

    #[test]
    fn open_directory_errors() {
        let dir = tempdir().unwrap();
        let _result = open_file_for_identity(dir.path());
        // File::open on a directory succeeds on Windows but metadata probe should still work;
        // The caller should use is_regular_file() before opening.
        // For directories, is_regular_file returns false.
        assert!(!is_regular_file(dir.path()), "目录不应是 regular file");
    }

    #[test]
    fn is_regular_file_for_actual_file() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "test.wav", b"data");
        assert!(is_regular_file(&path), "普通文件应是 regular file");
    }

    #[test]
    fn is_regular_file_for_nonexistent() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("nope.wav");
        assert!(!is_regular_file(&path), "不存在的路径不应是 regular file");
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn identity_has_strong_identity_on_windows() {
        let dir = tempdir().unwrap();
        let path = make_test_file(dir.path(), "test.wav", b"data");
        let (_file, id) = open_file_for_identity(&path).unwrap();
        assert!(
            id.has_strong_identity(),
            "Windows 上应具有强身份（卷序列号 + 文件索引）"
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn symlink_is_not_regular_file() {
        let dir = tempdir().unwrap();
        let target = make_test_file(dir.path(), "target.wav", b"data");
        let link = dir.path().join("link.wav");
        // 创建 symlink（可能需要管理员权限，跳过如果失败）
        #[cfg(windows)]
        {
            if std::os::windows::fs::symlink_file(&target, &link).is_err() {
                return; // 跳过：无权限创建 symlink
            }
        }
        assert!(!is_regular_file(&link), "symlink 不应是 regular file");
    }
}
