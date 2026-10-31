use super::*;
pub(super) fn extract(
    source: CancelReader<'_, File>,
    state: &mut ExtractState<'_, '_>,
) -> Result<(), ArchiveError> {
    let mut archive =
        ::zip::ZipArchive::new(source).map_err(|e| ArchiveError::Invalid(e.to_string()))?;
    if archive.len() > state.options.limits.max_entries {
        return Err(ArchiveError::Limit("条目数"));
    }
    for i in 0..archive.len() {
        let entry = archive
            .by_index_raw(i)
            .map_err(|e| ArchiveError::Invalid(e.to_string()))?;
        state.audit(
            entry.name(),
            entry.is_dir(),
            entry.size(),
            entry.encrypted() || entry.unix_mode().is_some_and(|m| m & 0o170000 == 0o120000),
        )?;
    }
    for i in 0..archive.len() {
        check_cancel(state.cancel)?;
        let mut entry = archive
            .by_index(i)
            .map_err(|e| ArchiveError::Invalid(e.to_string()))?;
        let name = entry.name().to_owned();
        let directory = entry.is_dir();
        let size = entry.size();
        state.process(&name, directory, size, &mut entry)?;
    }
    Ok(())
}
