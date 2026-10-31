use super::*;

pub(super) async fn install_at(
    root: PathBuf,
    repair: bool,
    cancel: &CancellationToken,
    reporter: &dyn InstallReporter,
) -> Result<(), Error> {
    if !cfg!(all(target_os = "windows", target_arch = "x86_64")) {
        return Err(Error::InstallFailed(
            "此 FFmpeg 构建仅支持 Windows x64".into(),
        ));
    }
    let _operation = operation_lock(&root)?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    if !repair && status_at(&root)?.state == "ready" {
        return Ok(());
    }
    let lock = lock()?;
    let id = unique_id();
    let staging = root.join("staging").join(&id);
    tokio::fs::create_dir_all(&staging).await?;
    let result = prepare(&lock, &staging, cancel, reporter).await;
    let result = match result {
        Ok(()) => {
            reporter.on_stage("promoting");
            let root = root.clone();
            let payload = staging.join("payload");
            let cancel = cancel.clone();
            tokio::task::spawn_blocking(move || publish(&root, &payload, &lock, &id, &cancel))
                .await
                .map_err(|e| Error::InstallFailed(format!("安装提交线程失败: {e}")))?
        }
        Err(error) => Err(error),
    };
    // 等待解压线程/子进程退出以后再清理，不与在途写入互相竞争。
    let _ = tokio::fs::remove_dir_all(&staging).await;
    if result.is_ok() {
        tracing::info!(kind = KIND, "FFmpeg 托管安装已提交");
        reporter.on_stage("done");
    }
    result
}

async fn prepare(
    lock: &MediaLock,
    staging: &Path,
    cancel: &CancellationToken,
    reporter: &dyn InstallReporter,
) -> Result<(), Error> {
    reporter.on_stage("downloading");
    let archive = staging.join(format!("archive.{}", lock.archive_format.extension()));
    let download = async {
        match super::super::download_archive(&lock.runtime, &archive, Some(cancel), reporter).await
        {
            Ok(()) => Ok(()),
            Err(error @ (Error::Cancelled | Error::ChecksumMismatch { .. })) => Err(error),
            Err(_) => {
                reporter.on_log("info", "主下载源不可用，尝试同摘要的官方 GitHub 发行源");
                let mut fallback = lock.runtime.clone();
                fallback.url = lock.fallback_url.clone();
                super::super::download_archive(&fallback, &archive, Some(cancel), reporter).await
            }
        }
    };
    tokio::time::timeout(std::time::Duration::from_secs(900), download)
        .await
        .map_err(|_| Error::Download("媒体运行时下载超过总时限".into()))??;
    prepare_archive(lock, archive, staging, cancel, reporter).await
}

pub(super) async fn prepare_archive(
    lock: &MediaLock,
    archive: PathBuf,
    staging: &Path,
    cancel: &CancellationToken,
    reporter: &dyn InstallReporter,
) -> Result<(), Error> {
    reporter.on_stage("extracting");
    let payload = staging.join("payload");
    let owned_lock = lock.clone();
    let destination = payload.clone();
    let token = cancel.clone();
    tokio::task::spawn_blocking(move || {
        use crate::utils::archive::{
            ExtractLimits, ExtractOptions, ExtractSelection, extract_archive,
        };
        extract_archive(
            &archive,
            &destination,
            &ExtractOptions {
                format: owned_lock.archive_format,
                limits: ExtractLimits {
                    max_entries: owned_lock.max_entries,
                    max_total_bytes: owned_lock.max_expanded_bytes,
                    ..Default::default()
                },
                selection: Some(ExtractSelection {
                    prefix: &owned_lock.archive_prefix,
                    files: &owned_lock.files,
                }),
                expected_sha256: Some(&owned_lock.runtime.sha256),
            },
            Some(&token),
        )
        .map(|_| ())
        .map_err(super::super::archive::map_error)
    })
    .await
    .map_err(|e| Error::InstallFailed(format!("解压线程失败: {e}")))??;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    tokio::fs::write(payload.join("NOTICE.txt"), NOTICE).await?;
    reporter.on_stage("verifying");
    let payload = tokio::fs::canonicalize(&payload).await?;
    probe::verify(
        &payload.join("bin/ffmpeg.exe"),
        &payload.join("bin/ffprobe.exe"),
        staging,
        cancel,
    )
    .await
}

pub(super) fn publish(
    root: &Path,
    payload: &Path,
    lock: &MediaLock,
    id: &str,
    cancel: &CancellationToken,
) -> Result<(), Error> {
    let mut files = Vec::new();
    for path in lock.files.iter().map(String::as_str).chain(["NOTICE.txt"]) {
        let (bytes, modified) = crate::utils::fs::file_identity(&payload.join(path))
            .ok_or_else(|| Error::SelfTestFailed("提交前产物缺失".into()))?;
        files.push(InstalledFile {
            path: path.into(),
            bytes,
            modified,
        });
    }
    let slot = format!("build-{id}");
    let target = root.join("slots").join(&slot);
    std::fs::create_dir_all(root.join("slots"))?;
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    std::fs::rename(payload, &target)?;
    let record = Record {
        slot,
        version: lock.runtime.version.clone(),
        build: lock.runtime.sha256.clone(),
        files,
        verification_failed: false,
    };
    let json =
        serde_json::to_vec_pretty(&record).map_err(|e| Error::InstallFailed(e.to_string()))?;
    // 切换点是 active.json 的原子替换；旧代次此前始终保持可用。
    if cancel.is_cancelled() {
        let _ = std::fs::remove_dir_all(&target);
        return Err(Error::Cancelled);
    }
    if let Err(error) = crate::utils::fs::atomic_write_bytes(&root.join("active.json"), &json) {
        let _ = std::fs::remove_dir_all(&target);
        return Err(Error::Io(error));
    }
    // 使用者持有共享租约时，保留旧文件；后续修复/更新或卸载可重试清理。
    if let Ok(_usage) = usage_lock(root, true) {
        if let Ok(entries) = std::fs::read_dir(root.join("slots")) {
            for entry in entries.flatten() {
                if entry.path() != target && entry.file_type().is_ok_and(|t| t.is_dir()) {
                    if let Err(error) = std::fs::remove_dir_all(entry.path()) {
                        tracing::debug!(%error, "旧媒体运行时产物待清理");
                    }
                }
            }
        }
    }
    Ok(())
}
