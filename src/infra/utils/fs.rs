//! 文件系统工具：原子写入（0.23.2）。

use std::io;
use std::path::Path;

/// 原子写入文件（同目录临时文件 + 原子替换）。
///
/// Windows 使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`——目标文件在
/// 任何时刻都完整存在，进程崩溃不会留下半截文件。非 Windows 退化为
/// `std::fs::rename`（POSIX 语义天然原子）。写入方负责阻塞隔离
///（调用处应已在 `spawn_blocking` 内）。
///
/// 返回写入后的文件身份 `(size, mtime_ms)`，供调用方做后续冲突保护基线。
pub fn atomic_write_bytes(path: &Path, content: &[u8]) -> io::Result<(u64, i64)> {
    let parent = path
        .parent()
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "路径无父目录"))?;
    std::fs::create_dir_all(parent)?;

    let tmp_name = format!(
        ".blink-tmp-{}-{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let tmp_path = parent.join(tmp_name);

    // 写临时文件并落盘（write_through 由 rename 时的 WRITE_THROUGH 兜底，
    // 此处先 flush 确保临时文件内容完整）。
    {
        let mut file = std::fs::File::create(&tmp_path)?;
        io::Write::write_all(&mut file, content)?;
        io::Write::flush(&mut file)?;
    }

    replace_via_rename(&tmp_path, path)?;

    file_identity(path)
        .ok_or_else(|| io::Error::new(io::ErrorKind::Other, "写入后读取文件身份失败"))
}

/// 读取文件身份 `(size, mtime_ms)`；文件不存在或元数据不可读时返回 None。
pub fn file_identity(path: &Path) -> Option<(u64, i64)> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_millis() as i64;
    Some((meta.len(), mtime_ms))
}

#[cfg(windows)]
fn replace_via_rename(tmp: &Path, target: &Path) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let to_wide = |p: &Path| -> Vec<u16> {
        p.as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect()
    };
    let flags = MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0);

    // MoveFileExW 偶发"拒绝访问 (0x80070005)"——杀软实时扫描或并发短暂占用
    // 文件句柄，属暂态错误，有限次重试即可恢复（对齐 local_engine 先例）。
    const MAX_RETRIES: u32 = 5;
    for attempt in 0..MAX_RETRIES {
        let result = unsafe {
            MoveFileExW(
                PCWSTR(to_wide(tmp).as_ptr()),
                PCWSTR(to_wide(target).as_ptr()),
                flags,
            )
        };
        match result {
            Ok(()) => return Ok(()),
            Err(e) => {
                let transient = e.code() == windows::core::HRESULT::from_win32(0x80070005)
                    && attempt + 1 < MAX_RETRIES;
                if !transient {
                    let _ = std::fs::remove_file(tmp);
                    return Err(io::Error::other(e));
                }
                tracing::debug!(
                    attempt = attempt + 1,
                    max = MAX_RETRIES,
                    "MoveFileExW 暂态拒绝访问，重试"
                );
                std::thread::sleep(std::time::Duration::from_millis(10 * (1 << attempt)));
            }
        }
    }
    unreachable!("重试循环内要么返回要么继续")
}

#[cfg(not(windows))]
fn replace_via_rename(tmp: &Path, target: &Path) -> io::Result<()> {
    std::fs::rename(tmp, target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("blink-fs-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn atomic_write_creates_and_replaces() {
        let dir = temp_dir("create-replace");
        let path = dir.join("note.md");

        let (size, _) = atomic_write_bytes(&path, b"hello").unwrap();
        assert_eq!(size, 5);
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");

        // 覆盖已有文件：替换后内容完整，无临时文件残留
        let (size2, _) = atomic_write_bytes(&path, "hello world 中文".as_bytes()).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), "hello world 中文".as_bytes());
        assert_eq!(size2, path.metadata().unwrap().len());
        let leftovers: Vec<_> = std::fs::read_dir(&dir).unwrap().collect();
        assert_eq!(leftovers.len(), 1, "不应有临时文件残留");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn file_identity_none_for_missing_file() {
        let dir = temp_dir("missing");
        assert!(file_identity(&dir.join("ghost.txt")).is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn identity_changes_after_external_write() {
        let dir = temp_dir("identity");
        let path = dir.join("f.txt");
        let (size, mtime) = atomic_write_bytes(&path, b"v1").unwrap();
        let before = file_identity(&path).unwrap();
        assert_eq!(before, (size, mtime));

        // 外部修改后 size 变化可被 identity 检出
        std::fs::write(&path, b"v1-external-longer").unwrap();
        let after = file_identity(&path).unwrap();
        assert_ne!(after.0, size, "外部修改后 size 应不同");

        let _ = std::fs::remove_dir_all(&dir);
    }
}
