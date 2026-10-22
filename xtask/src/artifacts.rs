use std::io;
use std::path::Path;

/// 发布资源的内容不变时保留 mtime，避免触发 Tauri 的 rerun-if-changed。
pub fn copy_if_changed(source: &Path, destination: &Path) -> io::Result<bool> {
    let source_bytes = std::fs::read(source)?;
    write_if_changed(destination, &source_bytes)
}

pub fn write_if_changed(destination: &Path, bytes: &[u8]) -> io::Result<bool> {
    match std::fs::read(destination) {
        Ok(previous) if previous == bytes => return Ok(false),
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    std::fs::write(destination, bytes)?;
    Ok(true)
}

/// 时间戳描述本批产物，不能让一次无改动的发布刷新它并使主程序缓存失效。
pub fn write_worker_manifest(path: &Path, mut manifest: serde_json::Value) -> io::Result<bool> {
    let previous = match std::fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => None,
        Err(error) => return Err(error),
    };
    if let Some(mut previous) = previous {
        if let Some(object) = previous.as_object_mut() {
            object.remove("built_at_unix_ms");
        }
        if previous == manifest {
            return Ok(false);
        }
    }
    manifest["built_at_unix_ms"] = serde_json::json!(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as u64
    );
    let bytes = serde_json::to_vec_pretty(&manifest)?;
    write_if_changed(path, &bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn publication_preserves_unchanged_files_and_repairs_changed_content() {
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("source.exe");
        let destination = dir.path().join("published.exe");
        std::fs::write(&source, b"worker-v1").unwrap();
        assert!(copy_if_changed(&source, &destination).unwrap());
        let original = std::fs::metadata(&destination).unwrap().modified().unwrap();
        assert!(!copy_if_changed(&source, &destination).unwrap());
        assert_eq!(
            std::fs::metadata(&destination).unwrap().modified().unwrap(),
            original
        );

        // 相同长度的损坏文件也必须被修复，不能只比较大小/时间。
        std::fs::write(&destination, b"corrupted").unwrap();
        assert!(copy_if_changed(&source, &destination).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), b"worker-v1");
        std::fs::write(&source, b"worker-v2").unwrap();
        assert!(copy_if_changed(&source, &destination).unwrap());
        assert_eq!(std::fs::read(&destination).unwrap(), b"worker-v2");
    }

    #[test]
    fn manifest_preserves_timestamp_but_tracks_artifact_and_source_changes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("manifest.json");
        let original = serde_json::json!({
            "schema": 1, "funasr_commit": "source-a",
            "files": {"worker.exe": {"sha256": "hash-a", "size_bytes": 12}}
        });
        assert!(write_worker_manifest(&path, original.clone()).unwrap());
        let bytes = std::fs::read(&path).unwrap();
        let modified = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert!(!write_worker_manifest(&path, original.clone()).unwrap());
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        assert_eq!(
            std::fs::metadata(&path).unwrap().modified().unwrap(),
            modified
        );

        let mut changed = original.clone();
        changed["files"]["worker.exe"]["sha256"] = "hash-b".into();
        assert!(write_worker_manifest(&path, changed.clone()).unwrap());
        changed["funasr_commit"] = "source-b".into();
        assert!(write_worker_manifest(&path, changed).unwrap());
        std::fs::write(&path, b"invalid JSON").unwrap();
        assert!(write_worker_manifest(&path, original).unwrap());
    }
}
