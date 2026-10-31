use super::*;
use std::io::Cursor;
fn digest(path: &Path) -> String {
    format!("{:x}", Sha256::digest(std::fs::read(path).unwrap()))
}
fn package(root: &Path, format: ArchiveFormat, files: &[(&str, &[u8])]) -> PathBuf {
    let path = root.join(format!("sample.{}", format.extension()));
    if format == ArchiveFormat::Zip {
        let mut writer = ::zip::ZipWriter::new(File::create(&path).unwrap());
        for (name, bytes) in files {
            writer
                .start_file(*name, ::zip::write::SimpleFileOptions::default())
                .unwrap();
            writer.write_all(bytes).unwrap();
        }
        writer.finish().unwrap();
    } else {
        let mut writer = sevenz_rust2::ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
        let entries = files
            .iter()
            .map(|(name, _)| sevenz_rust2::ArchiveEntry::new_file(name))
            .collect();
        let sources: Vec<sevenz_rust2::SourceReader<_>> = files
            .iter()
            .map(|(_, bytes)| Cursor::new(*bytes).into())
            .collect();
        writer.push_archive_entries(entries, sources).unwrap();
        writer.finish().unwrap();
    }
    path
}
fn options(format: ArchiveFormat, sha: &str) -> ExtractOptions<'_> {
    ExtractOptions {
        format,
        limits: ExtractLimits::default(),
        selection: None,
        expected_sha256: Some(sha),
    }
}
#[test]
fn zip_and_seven_zip_share_selection_audit_and_limits() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        let temp = tempfile::tempdir().unwrap();
        let source = package(
            temp.path(),
            format,
            &[("build/unused", b"discard"), ("build/bin/tool.exe", b"MZ")],
        );
        let hash = digest(&source);
        let files = vec!["bin/tool.exe".to_owned()];
        let mut opts = options(format, &hash);
        opts.selection = Some(ExtractSelection {
            prefix: "build/",
            files: &files,
        });
        let report = extract_archive(&source, &temp.path().join("ok"), &opts, None).unwrap();
        assert_eq!(
            report,
            ExtractReport {
                entry_count: 2,
                decoded_bytes: 9,
                written_bytes: 2
            }
        );
        assert_eq!(
            std::fs::read(temp.path().join("ok/bin/tool.exe")).unwrap(),
            b"MZ"
        );
        assert!(!temp.path().join("ok/unused").exists());
        for (id, limit) in [
            (
                "entries",
                ExtractLimits {
                    max_entries: 1,
                    ..Default::default()
                },
            ),
            (
                "size",
                ExtractLimits {
                    max_total_bytes: 8,
                    ..Default::default()
                },
            ),
            (
                "file",
                ExtractLimits {
                    max_entry_bytes: 1,
                    ..Default::default()
                },
            ),
        ] {
            opts.limits = limit;
            assert!(matches!(
                extract_archive(&source, &temp.path().join(id), &opts, None),
                Err(ArchiveError::Limit(_))
            ));
        }
        opts.limits = ExtractLimits::default();
        let missing = vec!["missing".into()];
        opts.selection = Some(ExtractSelection {
            prefix: "build/",
            files: &missing,
        });
        assert!(matches!(
            extract_archive(&source, &temp.path().join("missing"), &opts, None),
            Err(ArchiveError::MissingFile)
        ));
    }
}
#[test]
fn archive_digest_and_cancellation_are_checked_before_writes() {
    let temp = tempfile::tempdir().unwrap();
    let source = package(
        temp.path(),
        ArchiveFormat::SevenZip,
        &[("file", b"content")],
    );
    let mut opts = options(ArchiveFormat::SevenZip, "bad");
    let out = temp.path().join("out");
    assert!(matches!(
        extract_archive(&source, &out, &opts, None),
        Err(ArchiveError::Checksum)
    ));
    opts.expected_sha256 = None;
    assert!(matches!(
        extract_archive(&source, &out, &opts, None),
        Err(ArchiveError::TrustedDigestRequired)
    ));
    let token = CancellationToken::new();
    token.cancel();
    assert!(matches!(
        extract_archive(&source, &out, &opts, Some(&token)),
        Err(ArchiveError::Cancelled)
    ));
    assert!(!out.exists());
    let mut read = CancelReader {
        inner: Cursor::new(b"data"),
        cancel: Some(&token),
    };
    let error = read.read_exact(&mut [0u8; 1]).unwrap_err();
    assert_ne!(
        error.kind(),
        std::io::ErrorKind::Interrupted,
        "不能让 read_exact 无限重试取消"
    );
}
#[test]
fn both_formats_reject_escapes_devices_and_case_collisions() {
    for format in [ArchiveFormat::Zip, ArchiveFormat::SevenZip] {
        for names in [["a", "A"], ["a", "../escape"], ["a", "NUL.txt"]] {
            let temp = tempfile::tempdir().unwrap();
            let source = package(
                temp.path(),
                format,
                &[(names[0], b"one"), (names[1], b"two")],
            );
            let hash = digest(&source);
            assert!(matches!(
                extract_archive(
                    &source,
                    &temp.path().join("out"),
                    &options(format, &hash),
                    None
                ),
                Err(ArchiveError::UnsafePath)
            ));
            assert!(!temp.path().join("escape").exists());
        }
    }
}
#[test]
fn extraction_preserves_existing_destination_files() {
    let temp = tempfile::tempdir().unwrap();
    let source = package(temp.path(), ArchiveFormat::Zip, &[("keep", b"new")]);
    let out = temp.path().join("out");
    std::fs::create_dir(&out).unwrap();
    std::fs::write(out.join("keep"), b"old").unwrap();
    let hash = digest(&source);
    assert!(matches!(
        extract_archive(&source, &out, &options(ArchiveFormat::Zip, &hash), None),
        Err(ArchiveError::UnsafePath)
    ));
    assert_eq!(std::fs::read(out.join("keep")).unwrap(), b"old");
}
#[test]
fn path_policy_accepts_unicode_and_blocks_windows_aliases() {
    assert_eq!(validate_name("目录\\文件.txt").unwrap(), "目录/文件.txt");
    for name in [
        "/absolute",
        "C:/drive",
        "file:ads",
        "name.",
        "space ",
        "CON",
        "NUL.txt",
        "COM¹.txt",
        "CONOUT$",
        "a/../b",
        "\\\\server\\file",
    ] {
        assert!(validate_name(name).is_err(), "{name}");
    }
}

#[test]
fn seven_zip_rejects_link_attributes_and_dictionary_over_budget() {
    let temp = tempfile::tempdir().unwrap();
    let source = package(
        temp.path(),
        ArchiveFormat::SevenZip,
        &[("file", b"content")],
    );
    let hash = digest(&source);
    let mut opts = options(ArchiveFormat::SevenZip, &hash);
    opts.limits.max_dictionary_bytes = 0;
    assert!(matches!(
        extract_archive(&source, &temp.path().join("budget"), &opts, None),
        Err(ArchiveError::Limit("解码字典"))
    ));
    // ArchiveWriter 不写入 anti-item，链接样本使用它实际保留的属性。
    for (id, attrs) in [("reparse", 0x400), ("symlink", 0o120000 << 16)] {
        let path = temp.path().join(format!("{id}.7z"));
        let mut entry = sevenz_rust2::ArchiveEntry::new_file("link");
        entry.has_windows_attributes = true;
        entry.windows_attributes = attrs;
        let mut writer = sevenz_rust2::ArchiveWriter::new(File::create(&path).unwrap()).unwrap();
        writer
            .push_archive_entry(entry, Some(Cursor::new(b"target")))
            .unwrap();
        writer.finish().unwrap();
        let hash = digest(&path);
        assert!(
            matches!(
                extract_archive(
                    &path,
                    &temp.path().join(id),
                    &options(ArchiveFormat::SevenZip, &hash),
                    None
                ),
                Err(ArchiveError::UnsupportedEntry)
            ),
            "{id}"
        );
    }
}

#[test]
fn cancellation_and_actual_length_are_enforced_during_streaming() {
    struct Cancels {
        source: Cursor<Vec<u8>>,
        token: CancellationToken,
    }
    impl Read for Cancels {
        fn read(&mut self, bytes: &mut [u8]) -> std::io::Result<usize> {
            let n = self.source.read(bytes)?;
            self.token.cancel();
            Ok(n)
        }
    }
    let temp = tempfile::tempdir().unwrap();
    let opts = options(ArchiveFormat::Zip, "");
    let mut state = ExtractState::new(temp.path(), &opts, None);
    state.audit("file", false, 1, false).unwrap();
    assert!(matches!(
        state.process("file", false, 1, &mut Cursor::new(b"too long")),
        Err(ArchiveError::Limit(_))
    ));
    let token = CancellationToken::new();
    let dest = temp.path().join("cancel");
    let mut state = ExtractState::new(&dest, &opts, Some(&token));
    state.audit("file", false, 100000, false).unwrap();
    let mut source = Cancels {
        source: Cursor::new(vec![0; 100000]),
        token: token.clone(),
    };
    assert!(matches!(
        state.process("file", false, 100000, &mut source),
        Err(ArchiveError::Cancelled)
    ));
}

#[test]
#[ignore = "需要 BLINK_FFMPEG_TEST_ARCHIVE=7z 和 BLINK_FFMPEG_TEST_ZIP=旧 ZIP，仅核对解压后的文件，不执行 FFmpeg"]
fn real_locked_7z_has_identical_payload_to_previous_zip() {
    let source = PathBuf::from(std::env::var_os("BLINK_FFMPEG_TEST_ARCHIVE").unwrap());
    let previous = PathBuf::from(std::env::var_os("BLINK_FFMPEG_TEST_ZIP").unwrap());
    let lock = crate::managed_runtime::ffmpeg::lock().unwrap();
    let temp = tempfile::tempdir().unwrap();
    let opts = ExtractOptions {
        format: ArchiveFormat::SevenZip,
        limits: ExtractLimits {
            max_total_bytes: lock.max_expanded_bytes,
            ..Default::default()
        },
        selection: Some(ExtractSelection {
            prefix: &lock.archive_prefix,
            files: &lock.files,
        }),
        expected_sha256: Some(&lock.runtime.sha256),
    };
    let report = extract_archive(&source, &temp.path().join("new"), &opts, None).unwrap();
    let zip_opts = ExtractOptions {
        format: ArchiveFormat::Zip,
        expected_sha256: lock.compatible_archive_sha256.first().map(String::as_str),
        ..opts
    };
    extract_archive(&previous, &temp.path().join("old"), &zip_opts, None).unwrap();
    for file in &lock.files {
        assert_eq!(
            digest(&temp.path().join("new").join(file)),
            digest(&temp.path().join("old").join(file)),
            "{file}"
        );
    }
    assert!(!temp.path().join("new/bin/ffplay.exe").exists());
    eprintln!("same payload, extraction report: {report:?}");
}
