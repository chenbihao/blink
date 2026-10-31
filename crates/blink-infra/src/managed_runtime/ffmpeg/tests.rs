use super::*;
use std::io::Write;

fn fixture(root: &Path, id: &str) -> PathBuf {
    let lock = lock().unwrap();
    let payload = root.join(format!("fixture-{id}"));
    for path in lock.files.iter().map(String::as_str).chain(["NOTICE.txt"]) {
        let file = payload.join(path);
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(file, b"fixture-verified-artifact").unwrap();
    }
    install::publish(root, &payload, &lock, id, &CancellationToken::new()).unwrap();
    root.join("slots").join(format!("build-{id}"))
}

#[test]
fn lock_is_fixed_and_installs_only_codec_tools_and_notices() {
    let lock = lock().unwrap();
    assert_eq!(lock.runtime.size_bytes, 35430500);
    assert_eq!(
        lock.archive_format,
        crate::utils::archive::ArchiveFormat::SevenZip
    );
    assert!(!lock.runtime.url.contains("latest"));
    assert!(!lock.files.iter().any(|p| p.contains("ffplay")));
    assert!(lock.files.contains(&"bin/ffprobe.exe".into()));
    assert!(lock.files.contains(&"LICENSE".into()));
    assert!(
        lock.source_url
            .starts_with("https://github.com/FFmpeg/FFmpeg/commit/")
    );
}

#[test]
fn status_is_read_only_and_checks_all_artifact_identities() {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path().join("absent");
    assert_eq!(status_at(&root).unwrap().state, "missing");
    assert!(!root.exists(), "读取状态不得建目录");
    let slot = fixture(&root, "one");
    let first = status_at(&root).unwrap();
    assert_eq!(first.state, "ready");
    assert!(first.software_h264);
    assert!(first.installed_bytes > 0);
    std::fs::write(slot.join("bin/ffprobe.exe"), b"corrupt").unwrap();
    assert_eq!(status_at(&root).unwrap().state, "corrupt");
    assert!(acquire_at(&root).is_err());
}

#[test]
fn archive_format_switch_keeps_verified_zip_installations_ready() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), "zip");
    let mut record = read_record(temp.path()).unwrap().unwrap();
    record.build = lock().unwrap().compatible_archive_sha256[0].clone();
    let write = |record: &Record| {
        std::fs::write(
            temp.path().join("active.json"),
            serde_json::to_vec(record).unwrap(),
        )
        .unwrap();
    };
    write(&record);
    assert_eq!(status_at(temp.path()).unwrap().state, "ready");
    record.version = "old-version".into();
    write(&record);
    assert_eq!(status_at(temp.path()).unwrap().state, "outdated");
    record.version = lock().unwrap().runtime.version;
    record.build = "0".repeat(64);
    write(&record);
    assert_eq!(status_at(temp.path()).unwrap().state, "outdated");
}

#[test]
fn malformed_pointer_never_escapes_slots() {
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("active.json"),
        br#"{"slot":"../outside","version":"x","build":"x","files":[]}"#,
    )
    .unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "corrupt");
    assert!(read_record(temp.path()).is_err());
}

#[test]
fn cancelled_publish_keeps_the_old_active_generation() {
    let temp = tempfile::tempdir().unwrap();
    let original = fixture(temp.path(), "original");
    let payload = temp.path().join("prepared");
    std::fs::create_dir_all(&payload).unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(
        install::publish(
            temp.path(),
            &payload,
            &lock().unwrap(),
            "cancelled",
            &cancel
        )
        .is_err()
    );
    assert_eq!(
        read_record(temp.path()).unwrap().unwrap().slot,
        "build-original"
    );
    assert!(original.is_dir());
    assert_eq!(status_at(temp.path()).unwrap().state, "ready");
}

#[test]
fn repair_switches_generation_without_removing_a_leased_binary() {
    let temp = tempfile::tempdir().unwrap();
    let first = fixture(temp.path(), "one");
    let lease = acquire_at(temp.path()).unwrap();
    let second = fixture(temp.path(), "two");
    assert_ne!(first, second);
    assert!(lease.ffmpeg.is_file());
    assert_eq!(read_record(temp.path()).unwrap().unwrap().slot, "build-two");
    assert!(matches!(
        uninstall_at(temp.path()),
        Err(Error::InstanceBusy)
    ));
    assert!(status_at(temp.path()).unwrap().software_h264);
    drop(lease);
    uninstall_at(temp.path()).unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "missing");
    assert!(!first.exists());
    assert!(!second.exists());
}

#[test]
fn failed_capability_probe_is_not_reported_as_ready() {
    let temp = tempfile::tempdir().unwrap();
    fixture(temp.path(), "one");
    let mut record = read_record(temp.path()).unwrap().unwrap();
    record.verification_failed = true;
    std::fs::write(
        temp.path().join("active.json"),
        serde_json::to_vec(&record).unwrap(),
    )
    .unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "corrupt");
}

#[test]
fn metadata_requires_the_actual_expected_video() {
    let good = br#"{"streams":[{"codec_name":"h264","width":64,"height":64,"pix_fmt":"yuv420p","nb_frames":"3","duration":"0.300000"}]}"#;
    probe::verify_metadata(good).unwrap();
    assert!(probe::verify_metadata(br#"{"streams":[]}"#).is_err());
    let wrong = String::from_utf8(good.to_vec())
        .unwrap()
        .replace("h264", "hevc");
    assert!(probe::verify_metadata(wrong.as_bytes()).is_err());
}

fn zip(root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
    let file = root.join("sample.zip");
    let mut writer = zip::ZipWriter::new(File::create(&file).unwrap());
    for (name, contents) in files {
        writer
            .start_file(*name, zip::write::SimpleFileOptions::default())
            .unwrap();
        writer.write_all(contents).unwrap();
    }
    writer.finish().unwrap();
    file
}

#[test]
fn archive_selects_required_files_and_rejects_bombs_escapes_and_cancellation() {
    let temp = tempfile::tempdir().unwrap();
    let archive = zip(
        temp.path(),
        &[
            ("build/bin/ffmpeg.exe", b"MZ"),
            ("build/bin/ffplay.exe", b"unwanted"),
        ],
    );
    let files = vec!["bin/ffmpeg.exe".to_string()];
    super::super::archive::extract(
        &archive,
        &temp.path().join("ok"),
        10,
        100,
        Some(("build/", &files)),
        None,
    )
    .unwrap();
    assert!(temp.path().join("ok/bin/ffmpeg.exe").is_file());
    assert!(!temp.path().join("ok/bin/ffplay.exe").exists());
    assert!(
        super::super::archive::extract(&archive, &temp.path().join("size"), 10, 2, None, None)
            .is_err()
    );
    assert!(
        super::super::archive::extract(&archive, &temp.path().join("count"), 1, 100, None, None)
            .is_err()
    );
    let missing = vec!["missing.exe".to_string()];
    assert!(
        super::super::archive::extract(
            &archive,
            &temp.path().join("missing"),
            10,
            100,
            Some(("build/", &missing)),
            None
        )
        .is_err()
    );
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        super::super::archive::extract(
            &archive,
            &temp.path().join("cancel"),
            10,
            100,
            None,
            Some(&cancel)
        ),
        Err(Error::Cancelled)
    ));
    let escape = zip(temp.path(), &[("build/../outside", b"bad")]);
    assert!(
        super::super::archive::extract(&escape, &temp.path().join("escape"), 10, 100, None, None)
            .is_err()
    );
    assert!(!temp.path().join("outside").exists());
}

struct Silent;
pub(crate) async fn test_archive_lease(root: &Path) -> RuntimeLease {
    use sha2::{Digest, Sha256};
    let archive = PathBuf::from(std::env::var_os("BLINK_FFMPEG_TEST_ARCHIVE").expect("指定真实发行包"));
    let lock = lock().unwrap();
    let mut input = File::open(&archive).unwrap(); let mut hash = Sha256::new();
    std::io::copy(&mut input, &mut hash).unwrap();
    assert_eq!(format!("{:x}", hash.finalize()), lock.runtime.sha256);
    let staging = root.join("fixture-stage"); std::fs::create_dir_all(&staging).unwrap();
    let cancel = CancellationToken::new();
    install::prepare_archive(&lock, archive, &staging, &cancel, &Silent).await.unwrap();
    install::publish(root, &staging.join("payload"), &lock, "media-test", &cancel).unwrap();
    acquire_at(root).unwrap()
}
impl InstallReporter for Silent {
    fn on_stage(&self, stage: &str) {
        tracing::info!(stage, "媒体运行时冒烟阶段");
    }
    fn on_log(&self, _: &str, text: &str) {
        tracing::info!("{text}");
    }
}

#[tokio::test]
async fn cancellation_and_operation_conflicts_do_not_publish_artifacts() {
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(matches!(
        install::install_at(temp.path().into(), false, &cancel, &Silent).await,
        Err(Error::Cancelled)
    ));
    assert!(!temp.path().join("active.json").exists());
    let _guard = operation_lock(temp.path()).unwrap();
    assert!(matches!(
        install::install_at(
            temp.path().into(),
            false,
            &CancellationToken::new(),
            &Silent
        )
        .await,
        Err(Error::AlreadyInstalling)
    ));
}

#[tokio::test]
async fn download_enforces_locked_size_and_hash() {
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    for (expected_size, expected_hash, success) in [
        (
            3,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            true,
        ),
        (
            2,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            false,
        ),
        (
            4,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad",
            false,
        ),
        (
            3,
            "0000000000000000000000000000000000000000000000000000000000000000",
            false,
        ),
    ] {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/archive.zip", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buffer = [0u8; 2048];
            tokio::io::AsyncReadExt::read(&mut stream, &mut buffer)
                .await
                .unwrap();
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\nConnection: close\r\n\r\nabc")
                .await
                .unwrap();
        });
        let temp = tempfile::tempdir().unwrap();
        let mut entry = lock().unwrap().runtime;
        entry.size_bytes = expected_size;
        entry.sha256 = expected_hash.into();
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            super::super::download_once(
                &entry,
                &url,
                &temp.path().join("download.zip"),
                None,
                &Silent,
            ),
        )
        .await
        .unwrap();
        assert_eq!(result.is_ok(), success);
        server.await.unwrap();
    }
}

#[tokio::test]
async fn stalled_body_remains_cancellable() {
    use tokio::{io::AsyncWriteExt, net::TcpListener};
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/archive.zip", listener.local_addr().unwrap());
    let headers_sent = std::sync::Arc::new(tokio::sync::Notify::new());
    let signal = headers_sent.clone();
    let server = tokio::spawn(async move {
        let (mut stream, _) = listener.accept().await.unwrap();
        let mut buffer = [0u8; 2048];
        tokio::io::AsyncReadExt::read(&mut stream, &mut buffer)
            .await
            .unwrap();
        stream
            .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 3\r\n\r\n")
            .await
            .unwrap();
        signal.notify_one();
        std::future::pending::<()>().await;
    });
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    let trigger = async {
        headers_sent.notified().await;
        cancel.cancel();
    };
    let mut entry = lock().unwrap().runtime;
    entry.size_bytes = 3;
    let destination = temp.path().join("download.zip");
    let download = super::super::download_once(&entry, &url, &destination, Some(&cancel), &Silent);
    let (result, ()) = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        tokio::join!(download, trigger)
    })
    .await
    .unwrap();
    assert!(matches!(result, Err(Error::Cancelled)));
    server.abort();
}

/// 真下载→摘要→选择解压→实际编解码→提交→修复→卸载，所有产物留临时目录。
#[tokio::test]
#[ignore = "联网下载锁定的 Windows x64 FFmpeg，显式运行"]
async fn real_ffmpeg_install_smoke() {
    let _ = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::INFO)
        .try_init();
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    install::install_at(temp.path().into(), false, &cancel, &Silent)
        .await
        .unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "ready");
    let first = read_record(temp.path()).unwrap().unwrap().slot;
    let bytes = status_at(temp.path()).unwrap().installed_bytes;
    assert!(bytes > 200_000_000 && bytes < 250_000_000);
    let lease = acquire_at(temp.path()).unwrap();
    assert!(matches!(
        uninstall_at(temp.path()),
        Err(Error::InstanceBusy)
    ));
    install::install_at(temp.path().into(), true, &cancel, &Silent)
        .await
        .unwrap();
    assert_ne!(read_record(temp.path()).unwrap().unwrap().slot, first);
    assert!(lease.ffmpeg.exists());
    drop(lease);
    uninstall_at(temp.path()).unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "missing");
}

/// 可重用已独立下载且核对摘要的发行包，隔离验证解压/编解码/发布与更新事务。
#[tokio::test]
#[ignore = "需要 BLINK_FFMPEG_TEST_ARCHIVE 指向锁定发行包，显式运行"]
async fn real_ffmpeg_archive_smoke() {
    use sha2::{Digest, Sha256};
    let archive =
        PathBuf::from(std::env::var_os("BLINK_FFMPEG_TEST_ARCHIVE").expect("指定真实发行包"));
    let lock = lock().unwrap();
    let mut input = File::open(&archive).unwrap();
    let mut digest = Sha256::new();
    std::io::copy(&mut input, &mut digest).unwrap();
    assert_eq!(format!("{:x}", digest.finalize()), lock.runtime.sha256);
    let temp = tempfile::tempdir().unwrap();
    let cancel = CancellationToken::new();
    for id in ["first", "repair"] {
        let staging = temp.path().join(id);
        std::fs::create_dir_all(&staging).unwrap();
        install::prepare_archive(&lock, archive.clone(), &staging, &cancel, &Silent)
            .await
            .unwrap();
        install::publish(temp.path(), &staging.join("payload"), &lock, id, &cancel).unwrap();
        assert_eq!(status_at(temp.path()).unwrap().state, "ready");
    }
    assert_eq!(
        read_record(temp.path()).unwrap().unwrap().slot,
        "build-repair"
    );
    assert!(!temp.path().join("slots/build-first").exists());
    uninstall_at(temp.path()).unwrap();
    assert_eq!(status_at(temp.path()).unwrap().state, "missing");
}
