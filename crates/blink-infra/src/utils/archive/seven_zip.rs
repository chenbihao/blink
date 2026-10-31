use super::*;
use sevenz_rust2::{ArchiveReader, EncoderMethod, Password};

pub(super) fn extract(
    source: CancelReader<'_, File>,
    state: &mut ExtractState<'_, '_>,
) -> Result<(), ArchiveError> {
    let mut archive = ArchiveReader::new(source, Password::empty()).map_err(|e| {
        if state.cancel.is_some_and(CancellationToken::is_cancelled) {
            ArchiveError::Cancelled
        } else {
            ArchiveError::Invalid(e.to_string())
        }
    })?;
    archive.set_thread_count(1);
    if archive.archive().files.len() > state.options.limits.max_entries
        || archive.archive().blocks.len() > state.options.limits.max_entries
    {
        return Err(ArchiveError::Limit("条目/数据块数"));
    }
    for block in &archive.archive().blocks {
        let mut dictionaries = 0u64;
        if block.coders.len() > 8 || block.get_unpack_size() > state.options.limits.max_total_bytes
        {
            return Err(ArchiveError::Limit("解码数据块"));
        }
        for (i, coder) in block.coders.iter().enumerate() {
            if block.get_unpack_size_at_index(i) > state.options.limits.max_total_bytes {
                return Err(ArchiveError::Limit("解码数据流"));
            }
            dictionaries = dictionaries
                .checked_add(dictionary_bytes(
                    coder.encoder_method_id(),
                    coder.properties(),
                )?)
                .filter(|n| *n <= state.options.limits.max_dictionary_bytes)
                .ok_or(ArchiveError::Limit("解码字典"))?;
        }
    }
    for entry in &archive.archive().files {
        let attrs = entry.windows_attributes();
        state.audit(
            entry.name(),
            entry.is_directory(),
            entry.size(),
            entry.is_anti_item() || (attrs & 0x400) != 0 || ((attrs >> 16) & 0o170000) == 0o120000,
        )?;
    }
    let mut failure = None;
    let result = archive.for_each_entries(|entry, read| {
        match state.process(entry.name(), entry.is_directory(), entry.size(), read) {
            Ok(()) => Ok(true),
            Err(error) => {
                failure = Some(error);
                Err(sevenz_rust2::Error::Io(
                    std::io::Error::other("受限解压失败"),
                    "".into(),
                ))
            }
        }
    });
    if let Some(error) = failure {
        return Err(error);
    }
    check_cancel(state.cancel)?;
    result.map_err(|e| ArchiveError::Invalid(e.to_string()))
}

fn dictionary_bytes(method: &[u8], properties: &[u8]) -> Result<u64, ArchiveError> {
    if method == EncoderMethod::ID_LZMA2 {
        let p = *properties
            .first()
            .filter(|p| **p <= 40)
            .ok_or_else(|| ArchiveError::Invalid("LZMA2 参数".into()))? as u32;
        return Ok(if p == 40 {
            u32::MAX as u64
        } else {
            ((2 | (p & 1)) as u64) << (p / 2 + 11)
        });
    }
    if method == EncoderMethod::ID_LZMA {
        let bytes: [u8; 4] = properties
            .get(1..5)
            .and_then(|p| p.try_into().ok())
            .ok_or_else(|| ArchiveError::Invalid("LZMA 参数".into()))?;
        return Ok(u32::from_le_bytes(bytes).max(4096) as u64);
    }
    if method == EncoderMethod::ID_COPY || method == [3, 3, 1, 3] || method == [3, 3, 1, 0x1b] {
        return Ok(0);
    }
    Err(ArchiveError::Invalid("不支持此 7z 压缩方法".into()))
}
