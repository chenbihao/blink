//! 托管资源的归档解压入口。调用方管理 staging/原子提交，本层负责格式与读取边界。
//! 7z 必须提供可信发行清单的摘要，先校验再解析 header。
//! sevenz-rust2 的 header 解码尚无可配置字典限额，不能把任意用户摘要当作信任凭据。
mod seven_zip;
#[cfg(test)]
mod tests;
mod zip;

use sha2::{Digest, Sha256};
use std::{
    collections::HashSet,
    fs::File,
    io::{Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};
use tokio_util::sync::CancellationToken;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum ArchiveFormat {
    #[default]
    #[serde(rename = "zip")]
    Zip,
    #[serde(rename = "7z")]
    SevenZip,
}
impl ArchiveFormat {
    pub fn extension(self) -> &'static str {
        match self {
            Self::Zip => "zip",
            Self::SevenZip => "7z",
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub struct ExtractLimits {
    pub max_entries: usize,
    pub max_entry_bytes: u64,
    pub max_total_bytes: u64,
    /// 同一 7z block 中 LZMA/LZMA2 字典之和；不含固定解码器状态。
    pub max_dictionary_bytes: u64,
}
impl Default for ExtractLimits {
    fn default() -> Self {
        Self {
            max_entries: 4096,
            max_entry_bytes: 128 << 20,
            max_total_bytes: 512 << 20,
            max_dictionary_bytes: 256 << 20,
        }
    }
}
#[derive(Clone, Copy)]
pub struct ExtractSelection<'a> {
    /// 规范的 / 分隔前缀，末尾须带 /；files 为此前缀下的相对文件路径。
    pub prefix: &'a str,
    pub files: &'a [String],
}
pub struct ExtractOptions<'a> {
    pub format: ArchiveFormat,
    pub limits: ExtractLimits,
    pub selection: Option<ExtractSelection<'a>>,
    pub expected_sha256: Option<&'a str>,
}
#[derive(Debug, Default, PartialEq, Eq)]
pub struct ExtractReport {
    pub entry_count: usize,
    pub decoded_bytes: u64,
    pub written_bytes: u64,
}
#[derive(Debug, thiserror::Error)]
pub enum ArchiveError {
    #[error("操作已取消")]
    Cancelled,
    #[error("归档超过限额: {0}")]
    Limit(&'static str),
    #[error("归档路径无效或冲突")]
    UnsafePath,
    #[error("归档包含链接、加密或删除条目")]
    UnsupportedEntry,
    #[error("归档缺少指定产物")]
    MissingFile,
    #[error("归档 SHA-256 不匹配")]
    Checksum,
    #[error("7z 解压需要可信发行清单中的 SHA-256")]
    TrustedDigestRequired,
    #[error("归档损坏或格式不支持: {0}")]
    Invalid(String),
    #[error("归档 IO 失败: {0}")]
    Io(#[from] std::io::Error),
}

/// 同步阻塞入口，应在 spawn_blocking 中使用。dest 须为空且位于调用方私有 staging。
/// 全包审计/读取，selection 只减少落盘文件；失败的 staging 由调用方丢弃。
pub fn extract_archive(
    path: &Path,
    dest: &Path,
    options: &ExtractOptions<'_>,
    cancel: Option<&CancellationToken>,
) -> Result<ExtractReport, ArchiveError> {
    check_cancel(cancel)?;
    if options.format == ArchiveFormat::SevenZip && options.expected_sha256.is_none() {
        return Err(ArchiveError::TrustedDigestRequired);
    }
    let mut file = std::fs::OpenOptions::new();
    file.read(true);
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        file.share_mode(1);
    } // 校验与解析期间拒绝其他写入/删除句柄。
    let mut file = file.open(path)?;
    if let Some(expected) = options.expected_sha256 {
        if expected.len() != 64 || !expected.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(ArchiveError::Checksum);
        }
        let mut hash = Sha256::new();
        let mut buffer = [0u8; 65536];
        loop {
            check_cancel(cancel)?;
            let n = file.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            hash.update(&buffer[..n]);
        }
        if format!("{:x}", hash.finalize()) != expected.to_ascii_lowercase() {
            return Err(ArchiveError::Checksum);
        }
        file.seek(SeekFrom::Start(0))?;
    }
    if let Ok(meta) = std::fs::symlink_metadata(dest) {
        if is_link(&meta) || !meta.is_dir() || std::fs::read_dir(dest)?.next().is_some() {
            return Err(ArchiveError::UnsafePath);
        }
    }
    if let Some(selection) = options.selection {
        if !selection.prefix.ends_with('/')
            || selection.files.is_empty()
            || validate_name(selection.prefix.trim_end_matches('/'))? + "/" != selection.prefix
        {
            return Err(ArchiveError::UnsafePath);
        }
        let mut seen = HashSet::new();
        for file in selection.files {
            let canonical = validate_name(file)?;
            if canonical != *file || !seen.insert(canonical.to_lowercase()) {
                return Err(ArchiveError::UnsafePath);
            }
        }
    }
    let source = CancelReader {
        inner: file,
        cancel,
    };
    let mut state = ExtractState::new(dest, options, cancel);
    let result = match options.format {
        ArchiveFormat::Zip => zip::extract(source, &mut state),
        ArchiveFormat::SevenZip => seven_zip::extract(source, &mut state),
    };
    check_cancel(cancel)?;
    result?;
    state.finish()
}
pub fn validate_name(raw: &str) -> Result<String, ArchiveError> {
    if raw.len() > 4096 || raw.starts_with(['/', '\\']) || raw.contains('\0') {
        return Err(ArchiveError::UnsafePath);
    }
    let normalized = raw.replace('\\', "/");
    let mut parts = Vec::new();
    for part in normalized.split('/') {
        if part.is_empty() || part == "." {
            continue;
        }
        let base = part
            .split('.')
            .next()
            .unwrap_or_default()
            .to_ascii_uppercase();
        if part == ".."
            || part.contains(':')
            || part.ends_with(['.', ' '])
            || matches!(
                base.as_str(),
                "CON"
                    | "PRN"
                    | "AUX"
                    | "NUL"
                    | "CONIN$"
                    | "CONOUT$"
                    | "COM¹"
                    | "COM²"
                    | "COM³"
                    | "LPT¹"
                    | "LPT²"
                    | "LPT³"
                    | "COM1"
                    | "COM2"
                    | "COM3"
                    | "COM4"
                    | "COM5"
                    | "COM6"
                    | "COM7"
                    | "COM8"
                    | "COM9"
                    | "LPT1"
                    | "LPT2"
                    | "LPT3"
                    | "LPT4"
                    | "LPT5"
                    | "LPT6"
                    | "LPT7"
                    | "LPT8"
                    | "LPT9"
            )
        {
            return Err(ArchiveError::UnsafePath);
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(ArchiveError::UnsafePath);
    }
    Ok(parts.join("/"))
}
fn is_link(meta: &std::fs::Metadata) -> bool {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        if meta.file_attributes() & 0x400 != 0 {
            return true;
        }
    }
    meta.file_type().is_symlink()
}
fn check_cancel(cancel: Option<&CancellationToken>) -> Result<(), ArchiveError> {
    if cancel.is_some_and(CancellationToken::is_cancelled) {
        Err(ArchiveError::Cancelled)
    } else {
        Ok(())
    }
}
struct CancelReader<'a, R> {
    inner: R,
    cancel: Option<&'a CancellationToken>,
}
impl<R: Read> Read for CancelReader<'_, R> {
    fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
        if self.cancel.is_some_and(CancellationToken::is_cancelled) {
            return Err(std::io::Error::other("cancelled"));
        }
        self.inner.read(bytes)
    }
}
impl<R: Seek> Seek for CancelReader<'_, R> {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

struct ExtractState<'a, 'b> {
    dest: &'a Path,
    options: &'a ExtractOptions<'b>,
    cancel: Option<&'a CancellationToken>,
    names: HashSet<String>,
    selected: HashSet<String>,
    declared: u64,
    report: ExtractReport,
}
impl<'a, 'b> ExtractState<'a, 'b> {
    fn new(
        dest: &'a Path,
        options: &'a ExtractOptions<'b>,
        cancel: Option<&'a CancellationToken>,
    ) -> Self {
        Self {
            dest,
            options,
            cancel,
            names: HashSet::new(),
            selected: HashSet::new(),
            declared: 0,
            report: ExtractReport::default(),
        }
    }
    fn audit(
        &mut self,
        name: &str,
        directory: bool,
        size: u64,
        unsupported: bool,
    ) -> Result<(), ArchiveError> {
        check_cancel(self.cancel)?;
        if unsupported {
            return Err(ArchiveError::UnsupportedEntry);
        }
        let safe = validate_name(name)?;
        if !self.names.insert(safe.to_lowercase()) {
            return Err(ArchiveError::UnsafePath);
        }
        self.report.entry_count += 1;
        if self.report.entry_count > self.options.limits.max_entries {
            return Err(ArchiveError::Limit("条目数"));
        }
        if size > self.options.limits.max_entry_bytes {
            return Err(ArchiveError::Limit("单文件"));
        }
        self.declared = self
            .declared
            .checked_add(size)
            .filter(|n| *n <= self.options.limits.max_total_bytes)
            .ok_or(ArchiveError::Limit("总解压量"))?;
        if directory && size != 0 {
            return Err(ArchiveError::Invalid("目录包含数据".into()));
        }
        Ok(())
    }
    fn process(
        &mut self,
        name: &str,
        directory: bool,
        size: u64,
        reader: &mut dyn Read,
    ) -> Result<(), ArchiveError> {
        check_cancel(self.cancel)?;
        let safe = validate_name(name)?;
        let relative = if let Some(selection) = self.options.selection {
            safe.strip_prefix(selection.prefix)
                .filter(|path| selection.files.iter().any(|file| file == path))
                .map(str::to_owned)
        } else {
            Some(safe)
        };
        let mut output: Option<File> = None;
        if let Some(relative) = relative {
            let target = self.dest.join(PathBuf::from(&relative));
            if directory {
                std::fs::create_dir_all(target)?;
                return Ok(());
            }
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            output = Some(
                std::fs::OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .open(target)?,
            );
            self.selected.insert(relative);
        }
        let mut count = 0u64;
        let mut buffer = [0u8; 65536];
        loop {
            check_cancel(self.cancel)?;
            let n = reader.read(&mut buffer)?;
            if n == 0 {
                break;
            }
            count += n as u64;
            self.report.decoded_bytes += n as u64;
            if count > size || self.report.decoded_bytes > self.options.limits.max_total_bytes {
                return Err(ArchiveError::Limit("实际解压量"));
            }
            if let Some(output) = &mut output {
                output.write_all(&buffer[..n])?;
                self.report.written_bytes += n as u64;
            }
        }
        if count != size {
            return Err(ArchiveError::Invalid("文件实际长度不符".into()));
        }
        Ok(())
    }
    fn finish(self) -> Result<ExtractReport, ArchiveError> {
        check_cancel(self.cancel)?;
        if self
            .options
            .selection
            .is_some_and(|s| s.files.iter().any(|file| !self.selected.contains(file)))
        {
            return Err(ArchiveError::MissingFile);
        }
        Ok(self.report)
    }
}
