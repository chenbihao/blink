//! 可选 FFmpeg 托管运行时。安装产物不可变，修复也以新代次原子发布。
//! status 只读文件身份；编码/解码自检仅在显式操作中运行。
use super::{InstallReporter, ManagedRuntimeError as Error, RuntimeLockEntry};
use serde::{Deserialize, Serialize};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

mod install;
mod probe;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::test_archive_lease;
#[cfg(test)]
pub(crate) fn test_lease_at(root: &Path) -> RuntimeLease { acquire_at(root).unwrap() }

pub const KIND: &str = "ffmpeg";
pub const NOTICE: &str = include_str!("../../../../../resources/runtimes/ffmpeg-NOTICE.txt");
pub const LICENSE: &str = include_str!("../../../../../resources/runtimes/ffmpeg-LICENSE.txt");
const LOCK: &str = include_str!("../../../../../resources/runtimes/ffmpeg-asset-lock.json");

#[derive(Clone, Deserialize)]
pub struct MediaLock {
    pub schema_version: u32,
    pub runtime: RuntimeLockEntry,
    pub archive_prefix: String,
    pub fallback_url: String,
    pub files: Vec<String>,
    pub max_entries: usize,
    pub max_expanded_bytes: u64,
    pub license: String,
    pub homepage: String,
    pub source_url: String,
    pub build_source_url: String,
}

pub fn lock() -> Result<MediaLock, Error> {
    let lock: MediaLock =
        serde_json::from_str(LOCK).map_err(|e| Error::LockParse(e.to_string()))?;
    if lock.schema_version != 1
        || lock.runtime.kind != KIND
        || lock.runtime.sha256.len() != 64
        || lock.runtime.size_bytes == 0
        || lock.max_entries == 0
        || lock.max_expanded_bytes < lock.runtime.size_bytes
        || !lock.runtime.url.starts_with("https://")
        || !lock.fallback_url.starts_with("https://")
        || super::safe_entry_relpath(&lock.archive_prefix).is_none()
        || lock
            .files
            .iter()
            .any(|p| super::safe_entry_relpath(p).is_none())
    {
        return Err(Error::LockParse("FFmpeg 锁文件无效".into()));
    }
    Ok(lock)
}

pub fn root() -> PathBuf {
    super::root().join(KIND)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct InstalledFile {
    path: String,
    bytes: u64,
    modified: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Record {
    slot: String,
    version: String,
    build: String,
    files: Vec<InstalledFile>,
    #[serde(default)]
    verification_failed: bool,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RuntimeStatus {
    pub state: String,
    pub version: Option<String>,
    pub target_version: String,
    pub installed_bytes: u64,
    pub download_bytes: u64,
    pub software_h264: bool,
    pub license: String,
    pub source_url: String,
    pub message: Option<String>,
}

fn read_record(root: &Path) -> Result<Option<Record>, Error> {
    let path = root.join("active.json");
    if !path.exists() {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(64 * 1024 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > 64 * 1024 {
        return Err(Error::SelfTestFailed("FFmpeg 安装记录超限".into()));
    }
    let record: Record = serde_json::from_slice(&bytes)
        .map_err(|_| Error::SelfTestFailed("FFmpeg 安装记录损坏".into()))?;
    if record.slot.is_empty()
        || record.slot.starts_with('.')
        || !record
            .slot
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        || record
            .files
            .iter()
            .any(|f| super::safe_entry_relpath(&f.path).is_none())
    {
        return Err(Error::SelfTestFailed("FFmpeg 安装记录路径无效".into()));
    }
    Ok(Some(record))
}

fn status_at(root: &Path) -> Result<RuntimeStatus, Error> {
    let lock = lock()?;
    let mut result = RuntimeStatus {
        state: "missing".into(),
        version: None,
        target_version: lock.runtime.version.clone(),
        installed_bytes: installed_bytes(root),
        download_bytes: lock.runtime.size_bytes,
        software_h264: false,
        license: lock.license.clone(),
        source_url: lock.source_url.clone(),
        message: None,
    };
    let record = match read_record(root) {
        Ok(Some(record)) => record,
        Ok(None) => {
            if result.installed_bytes > 0 {
                result.state = "pending_remove".into();
            }
            return Ok(result);
        }
        Err(_) => {
            result.state = "corrupt".into();
            result.message = Some("安装记录损坏，请修复".into());
            return Ok(result);
        }
    };
    result.version = Some(record.version.clone());
    let dir = root.join("slots").join(&record.slot);
    let complete = lock
        .files
        .iter()
        .all(|p| record.files.iter().any(|f| &f.path == p))
        && record.files.iter().all(|f| {
            crate::utils::fs::file_identity(&dir.join(&f.path)) == Some((f.bytes, f.modified))
        });
    if !complete || record.verification_failed {
        result.state = "corrupt".into();
        result.message = Some("安装文件缺失或已变化，请修复".into());
    } else if record.build != lock.runtime.sha256 {
        result.state = "outdated".into();
    } else {
        result.state = "ready".into();
        result.software_h264 = true;
    }
    Ok(result)
}

pub fn status() -> Result<RuntimeStatus, Error> {
    status_at(&root())
}

pub fn resolve() -> Option<PathBuf> {
    let root = root();
    if status_at(&root).ok()?.state != "ready" {
        return None;
    }
    let record = read_record(&root).ok()??;
    Some(root.join("slots").join(record.slot).join("bin/ffmpeg.exe"))
}

pub fn license_path() -> Option<PathBuf> {
    let root = root();
    let record = read_record(&root).ok()??;
    let path = root.join("slots").join(record.slot).join("LICENSE");
    path.is_file().then_some(path)
}

fn operation_lock(root: &Path) -> Result<File, Error> {
    std::fs::create_dir_all(root)?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".operation.lock"))?;
    file.try_lock().map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => Error::AlreadyInstalling,
        std::fs::TryLockError::Error(e) => Error::Io(e),
    })?;
    Ok(file)
}

/// 后续媒体任务持有此租约；卸载不能移除任务正在使用的产物。
pub struct RuntimeLease {
    pub ffmpeg: PathBuf,
    pub ffprobe: PathBuf,
    _guard: File,
    slot: String,
}

impl RuntimeLease {
    pub fn build_id(&self) -> &str { &self.slot }
}

pub fn acquire() -> Result<RuntimeLease, Error> {
    acquire_at(&root())
}

fn acquire_at(root: &Path) -> Result<RuntimeLease, Error> {
    let _operation = operation_lock(root)?;
    let guard = usage_lock(root, false)?;
    if status_at(root)?.state != "ready" {
        return Err(Error::SelfTestFailed(
            "FFmpeg 尚未就绪，请安装或修复".into(),
        ));
    }
    let record = read_record(root)?.ok_or_else(|| Error::SelfTestFailed("FFmpeg 未安装".into()))?;
    let dir = root.join("slots").join(&record.slot);
    Ok(RuntimeLease {
        ffmpeg: dir.join("bin/ffmpeg.exe"),
        ffprobe: dir.join("bin/ffprobe.exe"),
        _guard: guard,
        slot: record.slot,
    })
}

fn usage_lock(root: &Path, exclusive: bool) -> Result<File, Error> {
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(root.join(".use.lock"))?;
    let result = if exclusive {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|e| match e {
        std::fs::TryLockError::WouldBlock => Error::InstanceBusy,
        std::fs::TryLockError::Error(e) => Error::Io(e),
    })?;
    Ok(file)
}

pub async fn install(
    repair: bool,
    cancel: &CancellationToken,
    reporter: &dyn InstallReporter,
) -> Result<(), Error> {
    install::install_at(root(), repair, cancel, reporter).await
}

pub async fn diagnose(
    cancel: &CancellationToken,
    reporter: &dyn InstallReporter,
) -> Result<(), Error> {
    let lease = acquire()?;
    reporter.on_stage("verifying");
    let dir = root().join(format!(".probe-{}", unique_id()));
    tokio::fs::create_dir_all(&dir).await?;
    let result = probe::verify(&lease.ffmpeg, &lease.ffprobe, &dir, cancel).await;
    let _ = tokio::fs::remove_dir_all(&dir).await;
    if result
        .as_ref()
        .is_err_and(|e| !matches!(e, Error::Cancelled))
    {
        let slot = lease.slot.clone();
        let _ = tokio::task::spawn_blocking(move || -> Result<(), Error> {
            let root = root();
            let _operation = operation_lock(&root)?;
            if let Some(mut record) = read_record(&root)?.filter(|r| r.slot == slot) {
                record.verification_failed = true;
                crate::utils::fs::atomic_write_bytes(
                    &root.join("active.json"),
                    &serde_json::to_vec(&record)
                        .map_err(|e| Error::InstallFailed(e.to_string()))?,
                )?;
            }
            Ok(())
        })
        .await;
    }
    result
}

fn installed_bytes(root: &Path) -> u64 {
    fn sum(path: &Path, remaining: &mut usize) -> u64 {
        let Ok(entries) = std::fs::read_dir(path) else {
            return 0;
        };
        let mut bytes = 0u64;
        for entry in entries.flatten() {
            if *remaining == 0 {
                break;
            }
            *remaining -= 1;
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            if kind.is_dir() {
                bytes = bytes.saturating_add(sum(&entry.path(), remaining));
            } else if kind.is_file() {
                bytes = bytes.saturating_add(entry.metadata().map(|m| m.len()).unwrap_or(0));
            }
        }
        bytes
    }
    sum(&root.join("slots"), &mut 4096)
}

pub fn uninstall() -> Result<(), Error> {
    uninstall_at(&root())
}
fn uninstall_at(root: &Path) -> Result<(), Error> {
    let _operation = operation_lock(root)?;
    let _usage = usage_lock(root, true)?;
    // 先撤销可用状态，失败的物理清理如实保留，重试仍能完成。
    let pointer = root.join("active.json");
    if pointer.exists() {
        std::fs::remove_file(pointer)?;
    }
    for name in ["slots", "staging"] {
        let path = root.join(name);
        if path.exists() {
            std::fs::remove_dir_all(path)?;
        }
    }
    tracing::info!(kind = KIND, "托管媒体运行时已卸载");
    Ok(())
}

fn unique_id() -> String {
    format!(
        "{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    )
}

pub fn sweep_staging() -> usize {
    let root = root();
    if !root.is_dir() {
        return 0;
    }
    let Ok(_guard) = operation_lock(&root) else {
        return 0;
    };
    let mut count = 0;
    if let Ok(entries) = std::fs::read_dir(root.join("staging")) {
        for entry in entries.flatten() {
            if entry.file_type().is_ok_and(|t| t.is_dir())
                && std::fs::remove_dir_all(entry.path()).is_ok()
            {
                count += 1;
            }
        }
    }
    // probe 持有共享使用锁；仅无人使用时清除崩溃遗留。
    if let Ok(_usage) = usage_lock(&root, true) {
        if let Ok(entries) = std::fs::read_dir(&root) {
            for entry in entries.flatten() {
                if entry.file_name().to_string_lossy().starts_with(".probe-")
                    && entry.file_type().is_ok_and(|t| t.is_dir())
                    && std::fs::remove_dir_all(entry.path()).is_ok()
                {
                    count += 1;
                }
            }
        }
    }
    count
}
