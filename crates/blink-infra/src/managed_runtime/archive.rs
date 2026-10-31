//! 有界 ZIP 解压。媒体运行时只提取锁定文件，审计仍覆盖完整归档。
use super::ManagedRuntimeError as Error;
use std::{
    collections::HashSet,
    io::{Read, Write},
    path::Path,
};
use tokio_util::sync::CancellationToken;

pub(super) fn extract(
    archive: &Path,
    dest: &Path,
    max_entries: usize,
    max_bytes: u64,
    selection: Option<(&str, &[String])>,
    cancel: Option<&CancellationToken>,
) -> Result<(), Error> {
    let mut zip = zip::ZipArchive::new(std::fs::File::open(archive)?)
        .map_err(|e| Error::InstallFailed(format!("ZIP 无效: {e}")))?;
    if zip.len() > max_entries {
        return Err(Error::InstallFailed("ZIP 文件数超限".into()));
    }
    let mut declared = 0u64;
    let mut written = 0u64;
    let mut seen = HashSet::new();
    let mut extracted = HashSet::new();
    for i in 0..zip.len() {
        check_cancel(cancel)?;
        let mut entry = zip
            .by_index(i)
            .map_err(|e| Error::InstallFailed(format!("ZIP entry 无效: {e}")))?;
        let name = entry.name().replace('\\', "/");
        let relative = super::safe_entry_relpath(&name)
            .ok_or_else(|| Error::InstallFailed("ZIP 路径越界".into()))?;
        if entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000)
            || !seen.insert(relative.to_string_lossy().to_lowercase())
        {
            return Err(Error::InstallFailed("ZIP 含链接或重复路径".into()));
        }
        declared = declared
            .checked_add(entry.size())
            .filter(|n| *n <= max_bytes)
            .ok_or_else(|| Error::InstallFailed("ZIP 解压大小超限".into()))?;
        let target = if let Some((prefix, files)) = selection {
            let Some(stripped) = name.strip_prefix(prefix) else {
                continue;
            };
            if !files.iter().any(|f| f == stripped) {
                continue;
            }
            extracted.insert(stripped.to_string());
            dest.join(
                super::safe_entry_relpath(stripped)
                    .ok_or_else(|| Error::InstallFailed("产物路径无效".into()))?,
            )
        } else {
            dest.join(relative)
        };
        if entry.is_dir() {
            std::fs::create_dir_all(&target)?;
            continue;
        }
        std::fs::create_dir_all(
            target
                .parent()
                .ok_or_else(|| Error::InstallFailed("产物无父目录".into()))?,
        )?;
        let mut out = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(target)?;
        let mut buffer = [0u8; 64 * 1024];
        loop {
            check_cancel(cancel)?;
            let count = entry.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            written = written
                .checked_add(count as u64)
                .filter(|n| *n <= max_bytes)
                .ok_or_else(|| Error::InstallFailed("ZIP 实际解压大小超限".into()))?;
            out.write_all(&buffer[..count])?;
        }
        out.flush()?;
    }
    if let Some((_, files)) = selection {
        if files.iter().any(|f| !extracted.contains(f)) {
            return Err(Error::InstallFailed("ZIP 缺少锁定产物".into()));
        }
    }
    Ok(())
}

fn check_cancel(cancel: Option<&CancellationToken>) -> Result<(), Error> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}
