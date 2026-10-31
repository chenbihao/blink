//! 素材 TTL 与使用租约。清理只操作 sessions 下的普通会话目录。
use super::*;
use std::{fs::TryLockError, io};

pub(super) fn usage_lock(dir: &Path, exclusive: bool) -> io::Result<File> {
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(dir.join(".usage.lock"))?;
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|error| match error {
        TryLockError::WouldBlock => {
            io::Error::new(io::ErrorKind::WouldBlock, "素材正在使用或清理中")
        }
        TryLockError::Error(error) => error,
    })?;
    Ok(file)
}

pub fn cleanup_expired(root: &Path, ttl: std::time::Duration) -> usize {
    cleanup_expired_except(root, ttl, &[])
}

/// 过期以元数据 mtime 为准；未完成初始化的目录按目录 mtime 保留一个完整 TTL。
/// 编辑会话显式排除，跨进程读写器持共享租约，清理取得独占租约后才删除。
pub fn cleanup_expired_except(
    root: &Path,
    ttl: std::time::Duration,
    protected: &[String],
) -> usize {
    let deadline = SystemTime::now().checked_sub(ttl).unwrap_or(UNIX_EPOCH);
    let sessions = root.join("sessions");
    let Ok(root_meta) = fs::symlink_metadata(&sessions) else {
        return 0;
    };
    if !ordinary_directory(&root_meta) {
        return 0;
    }
    let mut removed = 0;
    for id in list_session_ids(root) {
        if !is_valid_session_id(&id) || protected.contains(&id) {
            continue;
        }
        let dir = session_dir(root, &id);
        let Ok(meta) = fs::symlink_metadata(&dir) else {
            continue;
        };
        if !ordinary_directory(&meta) {
            continue;
        }
        let modified = || match fs::symlink_metadata(dir.join("session.json")) {
            Ok(meta) if meta.is_file() && !meta.file_type().is_symlink() => meta.modified(),
            Ok(_) => Err(io::Error::other("素材元数据不是普通文件")),
            Err(error) if error.kind() == io::ErrorKind::NotFound => meta.modified(),
            Err(error) => Err(error),
        };
        if !modified().is_ok_and(|time| time <= deadline) {
            continue;
        }
        let _lease = match usage_lock(&dir, true) {
            Ok(lease) => lease,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => continue,
            Err(error) => {
                tracing::warn!(%error, session_id = %id, "素材清理租约失败");
                continue;
            }
        };
        // 元数据检查与拿锁之间，写入器可能刚完成定稿；拿锁后再次确认，不能删掉它。
        if !modified().is_ok_and(|time| time <= deadline) {
            continue;
        }
        match fs::remove_dir_all(&dir) {
            Ok(()) => removed += 1,
            Err(error) => tracing::warn!(%error, session_id = %id, "录制素材 TTL 清理失败"),
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn expire(dir: &Path) {
        File::options()
            .write(true)
            .open(dir.join("session.json"))
            .unwrap()
            .set_modified(SystemTime::now() - std::time::Duration::from_secs(8 * 86400))
            .unwrap();
    }

    #[test]
    fn cleanup_preserves_live_readers_writers_editor_and_new_partial_directory() {
        let root = tempfile::tempdir().unwrap();
        let ttl = std::time::Duration::from_secs(3 * 86400);
        let meta = super::super::tests::meta(8, 8);
        let id = meta.id.clone();
        let writer = SpoolWriter::create(root.path(), meta).unwrap();
        let dir = session_dir(root.path(), &id);
        expire(&dir);
        assert_eq!(cleanup_expired(root.path(), ttl), 0, "在录制的写入器不能删");
        writer.finalize("user").unwrap();
        expire(&dir);
        let first = SpoolReader::open(root.path(), &id).unwrap();
        let second = SpoolReader::open(root.path(), &id).unwrap();
        assert_eq!(
            cleanup_expired(root.path(), ttl),
            0,
            "导出/预览读取器不能删"
        );
        drop(first);
        assert_eq!(cleanup_expired(root.path(), ttl), 0, "第二个读租约仍有效");
        drop(second);
        assert_eq!(
            cleanup_expired_except(root.path(), ttl, &[id]),
            0,
            "空闲编辑会话仍受保护"
        );
        let partial = root.path().join("sessions/rec-partial");
        fs::create_dir(&partial).unwrap();
        assert_eq!(cleanup_expired(root.path(), ttl), 1, "只删除释放后的旧会话");
        assert!(!dir.exists());
        assert!(partial.exists(), "新建未完成的会话有完整 TTL 宽限");
    }
}

fn ordinary_directory(meta: &fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return false;
        }
    }
    meta.is_dir() && !meta.file_type().is_symlink()
}
