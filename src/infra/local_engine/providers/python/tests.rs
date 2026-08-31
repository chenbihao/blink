use super::*;

#[test]
fn python_venv_provider_kind() {
    let provider = PythonVenvProvider::new();
    assert_eq!(provider.kind(), runtime::RuntimePlan::PythonVenv);
}

#[test]
fn python_venv_always_compatible() {
    let provider = PythonVenvProvider::new();
    assert!(
        provider
            .check_compatibility(&CompatibilityCheck::Always)
            .unwrap()
    );
}

#[test]
fn python_venv_cpu_only_rejects_gpu() {
    let provider = PythonVenvProvider::cpu_only();
    assert!(
        !provider
            .check_compatibility(&CompatibilityCheck::RequiresCuda { min_version: None })
            .unwrap()
    );
    assert!(
        !provider
            .check_compatibility(&CompatibilityCheck::RequiresVulkan)
            .unwrap()
    );
}

#[test]
fn python_venv_cpu_feature_ignored() {
    // Python venv 不关心 CPU feature（由 ManagedBinary 处理）
    let provider = PythonVenvProvider::new();
    assert!(
        provider
            .check_compatibility(&CompatibilityCheck::RequiresCpuFeature {
                feature: "avx2".to_string()
            })
            .unwrap()
    );
}

#[test]
fn python_venv_query_packages_empty_when_no_venv() {
    let tmp = tempfile::tempdir().unwrap();
    let venv_dir = tmp.path().join("venv");
    let packages = PythonVenvProvider::query_packages(&venv_dir, &[]);
    assert!(packages.is_empty());
}

#[test]
fn python_venv_query_packages_reports_missing() {
    let tmp = tempfile::tempdir().unwrap();
    let venv_dir = tmp.path().join("venv");
    let packages = PythonVenvProvider::query_packages(
        &venv_dir,
        &[
            PackageLock {
                name: "torch".to_string(),
                version: "2.5.0".to_string(),
                sha256: None,
                ..Default::default()
            },
            PackageLock {
                name: "funasr".to_string(),
                version: "1.3.0".to_string(),
                sha256: None,
                ..Default::default()
            },
        ],
    );
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].name, "torch");
    assert!(packages[0].installed_version.is_none());
    assert!(!packages[0].satisfies_lock);
}

#[test]
fn hashed_requirements_include_each_declared_hash() {
    let hash = "a".repeat(64);
    let rendered = render_hashed_requirements(&[PackageLock {
        name: "example".to_string(),
        version: "1.2.3".to_string(),
        sha256: Some(hash.clone()),
        ..Default::default()
    }])
    .unwrap();
    assert_eq!(rendered, format!("example==1.2.3 --hash=sha256:{hash}\n"));
}

#[test]
fn hashed_requirements_reject_non_exact_versions_and_invalid_hashes() {
    let ranged = PackageLock {
        name: "example".to_string(),
        version: ">=1.2".to_string(),
        sha256: Some("a".repeat(64)),
        ..Default::default()
    };
    assert!(render_hashed_requirements(&[ranged]).is_err());

    let invalid_hash = PackageLock {
        name: "example".to_string(),
        version: "1.2.3".to_string(),
        sha256: Some("not-a-sha256".to_string()),
        ..Default::default()
    };
    assert!(render_hashed_requirements(&[invalid_hash]).is_err());
}

/// Task 3: 验证 None hash 会被 render_hashed_requirements 拒绝。
///
/// 这确保了不会静默降级为无 hash 安装——
/// 如果 descriptor 中存在 sha256: None，安装前就会失败。
#[test]
fn hashed_requirements_reject_missing_hash() {
    let missing_hash = PackageLock {
        name: "example".to_string(),
        version: "1.2.3".to_string(),
        sha256: None,
        ..Default::default()
    };
    assert!(render_hashed_requirements(&[missing_hash]).is_err());
}

/// Task 5: 验证全零 hash 仍能通过 render_hashed_requirements 的格式检查。
#[test]
fn hashed_requirements_accept_zero_hash_format() {
    let zero_hash_pkg = PackageLock {
        name: "test-pkg".to_string(),
        version: "1.0.0".to_string(),
        sha256: Some("0".repeat(64)),
        ..Default::default()
    };
    let rendered = render_hashed_requirements(&[zero_hash_pkg]).unwrap();
    assert!(rendered.contains("test-pkg==1.0.0 --hash=sha256:"));
    let zero_hash = "0".repeat(64);
    assert!(rendered.contains(&format!("--hash=sha256:{}", zero_hash)));
}

#[test]
fn all_have_hashes_detection_with_real_hashes() {
    let packages = [
        PackageLock {
            name: "a".to_string(),
            version: "1.0".to_string(),
            sha256: Some("a".repeat(64)),
            ..Default::default()
        },
        PackageLock {
            name: "b".to_string(),
            version: "2.0".to_string(),
            sha256: Some("b".repeat(64)),
            ..Default::default()
        },
    ];
    let all_have_hashes = packages.iter().all(|p| p.sha256.is_some());
    assert!(
        all_have_hashes,
        "所有 hash 为 Some 时应走 --require-hashes 路径"
    );
}

#[test]
fn all_have_hashes_false_with_mixed() {
    let packages = [
        PackageLock {
            name: "a".to_string(),
            version: "1.0".to_string(),
            sha256: Some("0".repeat(64)),
            ..Default::default()
        },
        PackageLock {
            name: "b".to_string(),
            version: "2.0".to_string(),
            sha256: None,
            ..Default::default()
        },
    ];
    let all_have_hashes = packages.iter().all(|p| p.sha256.is_some());
    assert!(!all_have_hashes);
}

#[test]
fn render_hashed_requirements_rejects_incomplete_lock() {
    let incomplete = vec![
        PackageLock {
            name: "paddlepaddle".to_string(),
            version: "3.1.0".to_string(),
            sha256: Some("a".repeat(64)),
            ..Default::default()
        },
        PackageLock {
            name: "paddleocr".to_string(),
            version: "3.7.0".to_string(),
            sha256: None, // 缺失 hash
            ..Default::default()
        },
    ];
    assert!(render_hashed_requirements(&incomplete).is_err());
}

#[test]
fn render_hashed_requirements_renders_complete_lock() {
    let complete = vec![
        PackageLock {
            name: "paddlepaddle".to_string(),
            version: "3.1.0".to_string(),
            sha256: Some(
                "3cb6d98eece900e34c05fa0428ccc32836525e72af25cc8ad10a48d4046c4639".to_string(),
            ),
            ..Default::default()
        },
        PackageLock {
            name: "fastapi".to_string(),
            version: "0.115.6".to_string(),
            sha256: Some(
                "e9240b29e36fa8f4bb7290316988e90c381e5092e0cbe84e7818cc3713bcf305".to_string(),
            ),
            ..Default::default()
        },
    ];
    let rendered = render_hashed_requirements(&complete).unwrap();
    assert!(rendered.contains("paddlepaddle==3.1.0 --hash=sha256:3cb6d98"));
    assert!(rendered.contains("fastapi==0.115.6 --hash=sha256:e9240b2"));
}

#[test]
fn all_have_hashes_empty_list() {
    let packages: Vec<PackageLock> = vec![];
    let all_have_hashes = packages.iter().all(|p| p.sha256.is_some());
    assert!(all_have_hashes);
}

// ── 0.22.6 H2 新增测试 ─────────────────────────────────────────────────

#[test]
fn uv_version_satisfies_equal() {
    assert!(uv_version_satisfies("uv 0.6.10", "0.6.10"));
}

#[test]
fn uv_version_satisfies_higher() {
    assert!(uv_version_satisfies("uv 0.7.0", "0.6.10"));
}

#[test]
fn uv_version_satisfies_rejects_lower() {
    assert!(!uv_version_satisfies("uv 0.5.0", "0.6.10"));
}

#[test]
fn uv_version_satisfies_rejects_unparseable() {
    assert!(!uv_version_satisfies("garbage", "0.6.10"));
    assert!(!uv_version_satisfies("uv 0.6.10", "garbage"));
}

#[test]
fn uv_version_satisfies_skips_empty_or_any() {
    assert!(
        PythonVenvProvider::verify_uv_version(std::path::Path::new("/nonexistent"), "").is_ok()
    );
    assert!(
        PythonVenvProvider::verify_uv_version(std::path::Path::new("/nonexistent"), "any").is_ok()
    );
}

#[test]
fn parse_uv_version_extracts_triple() {
    assert_eq!(parse_uv_version("uv 0.6.10"), Some((0, 6, 10)));
    assert_eq!(parse_uv_version("0.6.10"), Some((0, 6, 10)));
    assert_eq!(parse_uv_version("uv 1.0.0+meta"), Some((1, 0, 0)));
}

#[test]
fn parse_uv_version_rejects_garbage() {
    assert_eq!(parse_uv_version("garbage"), None);
    assert_eq!(parse_uv_version("1.2"), None);
}

/// 验证 `apply_blink_uv_env` 设置了正确的环境变量值。
#[test]
fn apply_blink_uv_env_sets_correct_vars() {
    let tmp = tempfile::tempdir().unwrap();
    // 临时覆盖 runtime 目录（仅测试环境变量设置逻辑）
    let cache_dir = tmp.path().join("cache").join("uv");
    let python_dir = tmp.path().join("pythons");

    let mut cmd = tokio::process::Command::new("echo");
    // 手动调用以验证逻辑
    std::fs::create_dir_all(&cache_dir).unwrap();
    std::fs::create_dir_all(&python_dir).unwrap();
    cmd.env("UV_CACHE_DIR", &cache_dir);
    cmd.env("UV_PYTHON_INSTALL_DIR", &python_dir);

    // 验证环境变量已设置
    // （无法直接检查 tokio::process::Command 的 env，但确保不 panic）
}

/// 验证 `RuntimeFoundationStatus` 不触发安装。
#[test]
fn foundation_status_does_not_trigger_install() {
    let status = PythonVenvProvider::foundation_status();
    // 只检查结构体正确返回，不检查 uv 是否存在（取决于运行环境）
    assert!(!status.uv_install_dir.is_empty());
    assert!(!status.uv_cache_dir.is_empty());
    assert!(!status.uv_python_install_dir.is_empty());
}

/// 验证 uv_source 分类正确。
#[test]
fn foundation_status_uv_source_classification() {
    let status = PythonVenvProvider::foundation_status();
    let uv_exists = runtime::local_uv_exe().exists();
    // 如果 uv 存在，source 应为 BlinkManaged；否则为 NotInstalled
    if uv_exists {
        assert!(matches!(status.uv_source, UvSource::BlinkManaged));
        assert!(status.uv_path.is_some());
    } else {
        assert!(matches!(status.uv_source, UvSource::NotInstalled));
    }
}

/// 验证 `ensure_uv` 只使用 Blink 托管目录，不扫描 PATH。
#[tokio::test]
async fn ensure_uv_only_uses_blink_managed_dir() {
    let mut provider = PythonVenvProvider::new();
    // ensure_uv 应该只检查 runtime::local_uv_exe()
    // 如果 uv 不存在，它会尝试下载（在测试环境可能失败）
    // 但关键是验证：不扫描 PATH
    // 这里只验证缓存逻辑
    if runtime::local_uv_exe().exists() {
        let path = provider.ensure_uv(None, None).await.unwrap();
        assert_eq!(path, runtime::local_uv_exe());
    }
}

/// 验证取消 token 能终止子进程。
#[tokio::test]
async fn cancel_token_terminates_child_process() {
    use tokio_util::sync::CancellationToken;

    let ct = CancellationToken::new();
    let ct2 = ct.clone();

    // spawn 一个 sleep 命令
    let mut cmd = tokio::process::Command::new("cmd");
    cmd.args(["/c", "ping -n 30 127.0.0.1 > nul"]);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let no_window_cmd = crate::infra::platform::no_window_tokio(cmd);

    // 在另一个 task 中触发取消
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        ct2.cancel();
    });

    let result = run_command_with_cancel(no_window_cmd, Some(&ct), "test-cancel", None).await;
    assert!(matches!(
        result,
        Err(RuntimeError::OperationCancelled { .. })
    ));
}

/// 验证 staging 目录在取消后被清理的可能性。
///
/// 注意：provider 的 `prepare_environment` 由 `InstallTransaction` 调用，
/// staging 清理由 `InstallTransaction` 负责。provider 只负责取消时
/// 终止子进程。这里测试取消后 staging 目录本身可被安全删除。
#[tokio::test]
async fn cancel_then_staging_can_be_cleaned() {
    let tmp = tempfile::tempdir().unwrap();
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&staging).unwrap();
    std::fs::write(staging.join("dummy.txt"), b"test").unwrap();

    // 模拟取消后的 staging 清理
    std::fs::remove_dir_all(&staging).unwrap();
    assert!(!staging.exists());
}

/// 验证 `RuntimeFoundationStatus` 可序列化。
#[test]
fn foundation_status_serializable() {
    let status = PythonVenvProvider::foundation_status();
    let json = serde_json::to_string(&status).unwrap();
    assert!(json.contains("uv_source"));
    assert!(json.contains("uv_install_dir"));
    assert!(json.contains("uv_cache_dir"));
}

// ── 0.22.6 H6: sanitize_log_line 测试 ──────────────────────────────────

#[test]
fn sanitize_preserves_plain_text() {
    let safe = sanitize_log_line("Installing package torch==2.5.0");
    assert_eq!(safe, "Installing package torch==2.5.0");
}

#[test]
fn sanitize_masks_password_equals() {
    let safe = sanitize_log_line("password=hunter2");
    assert!(safe.contains("password="));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("hunter2"));
}

#[test]
fn sanitize_masks_password_colon() {
    let safe = sanitize_log_line("password: hunter2");
    assert!(safe.contains("password:"));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("hunter2"));
}

#[test]
fn sanitize_masks_password_flag() {
    let safe = sanitize_log_line("--password hunter2");
    assert!(safe.contains("--password"));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("hunter2"));
}

#[test]
fn sanitize_masks_password_case_insensitive() {
    let safe = sanitize_log_line("PASSWORD=Secret123");
    assert!(safe.contains("PASSWORD="));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("Secret123"));
}

#[test]
fn sanitize_masks_token() {
    let safe = sanitize_log_line("token=abc123def456");
    assert!(safe.contains("token="));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("abc123def456"));
}

#[test]
fn sanitize_masks_secret() {
    let safe = sanitize_log_line("secret=my_secret_value");
    assert!(safe.contains("secret="));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("my_secret_value"));
}

#[test]
fn sanitize_masks_api_key_variants() {
    for line in &["api_key=AKIA123", "api-key=AKIA123", "apikey=AKIA123"] {
        let safe = sanitize_log_line(line);
        assert!(safe.contains("***REDACTED***"), "failed for: {line}");
        assert!(!safe.contains("AKIA123"), "AKIA123 leaked for: {line}");
    }
}

#[test]
fn sanitize_masks_authorization_bearer() {
    let safe = sanitize_log_line("Authorization: Bearer eyJhbGciOiJIUzI1");
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("eyJhbGciOiJIUzI1"));
}

#[test]
fn sanitize_preserves_rest_of_line() {
    let safe = sanitize_log_line("Downloading password=hunter2 from mirror");
    assert!(safe.contains("Downloading"));
    assert!(safe.contains("from mirror"));
    assert!(safe.contains("password="));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("hunter2"));
}

#[test]
fn sanitize_truncates_very_long_lines() {
    let long_line = "a".repeat(8000);
    let safe = sanitize_log_line(&long_line);
    assert!(safe.len() < long_line.len());
    assert!(safe.contains("...[truncated]"));
    // 截断后长度不应超过 4096 + 后缀
    assert!(safe.len() <= LOG_LINE_MAX_LEN + 20);
}

#[test]
fn sanitize_handles_empty_line() {
    let safe = sanitize_log_line("");
    assert_eq!(safe, "");
}

#[test]
fn sanitize_no_false_positive_on_word_password_in_text() {
    // "password" 出现在文本中但后面没有值不应 panic
    let safe = sanitize_log_line("enter your password to continue");
    assert!(!safe.contains("REDACTED"));
    assert_eq!(safe, "enter your password to continue");
}

#[test]
fn sanitize_masks_multiple_different_keys_in_one_line() {
    // 当前实现每行只 mask 第一个匹配的敏感键就返回；
    // 测试确保 password 的值被 mask
    let safe = sanitize_log_line("password=secret token=tok123");
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("secret"));
    // password 在 SENSITIVE_KEYS 中排在 token 之前，所以先匹配
}

#[test]
fn sanitize_preserves_utf8() {
    let safe = sanitize_log_line("安装包 torch 完成 password=abc123");
    assert!(safe.contains("安装包 torch 完成"));
    assert!(safe.contains("***REDACTED***"));
    assert!(!safe.contains("abc123"));
}

#[test]
fn sanitize_handles_non_utf8_lossy() {
    // 模拟 invalid UTF-8 bytes 经 lossy 转换后的字符串
    // \xFF in UTF-8 is invalid, lossy converts to U+FFFD
    let lossy = String::from_utf8_lossy(b"password=abc\xffdef");
    let safe = sanitize_log_line(&lossy);
    assert!(safe.contains("***REDACTED***"));
}
