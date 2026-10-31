//! 托管安装的薄适配；ZIP/7z 实现与限额检查统一在 utils::archive。
use super::ManagedRuntimeError as Error;
use crate::utils::archive::{
    self, ArchiveError, ArchiveFormat, ExtractLimits, ExtractOptions, ExtractSelection,
};
use std::path::Path;
use tokio_util::sync::CancellationToken;

pub(super) fn map_error(error: ArchiveError) -> Error {
    match error {
        ArchiveError::Cancelled => Error::Cancelled,
        ArchiveError::Io(error) => Error::Io(error),
        error => Error::InstallFailed(error.to_string()),
    }
}
pub(super) fn extract(
    archive_path: &Path,
    dest: &Path,
    max_entries: usize,
    max_bytes: u64,
    selection: Option<(&str, &[String])>,
    cancel: Option<&CancellationToken>,
) -> Result<(), Error> {
    archive::extract_archive(
        archive_path,
        dest,
        &ExtractOptions {
            format: ArchiveFormat::Zip,
            limits: ExtractLimits {
                max_entries,
                max_total_bytes: max_bytes,
                max_entry_bytes: max_bytes,
                ..Default::default()
            },
            selection: selection.map(|(prefix, files)| ExtractSelection { prefix, files }),
            expected_sha256: None,
        },
        cancel,
    )
    .map(|_| ())
    .map_err(map_error)
}
