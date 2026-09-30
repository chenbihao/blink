//! ProcessLease 自动化测试（0.22.6.1）。
//!
//! 测试范围：
//! - lease 写入/删除/扫描的原子性与安全性
//! - 恢复决策函数的全部场景：PID 重用、路径不符、creation time 不符、
//!   token/instance 不符、stale lease、匹配 lease
//! - 所有不匹配场景均断言"不接管、不终止"（DoNotAdopt）
//! - 正常 stop/spawn失败/health失败后无错误删除新实例 lease 的竞态

use super::*;

// ── ProcessLease 构造 ──────────────────────────────────────────────────────

#[test]
fn process_lease_new_has_correct_schema_version() {
    let lease = ProcessLease::new(
        "funasr",
        "inst-abc",
        12345,
        1700000000000,
        "C:/blink/python.exe",
        "127.0.0.1:8100",
        "fp:abcdef0123456789",
        "gen-001",
    );
    assert_eq!(lease.schema_version, LEASE_SCHEMA_VERSION);
    assert_eq!(lease.engine_id, "funasr");
    assert_eq!(lease.instance_id, "inst-abc");
    assert_eq!(lease.pid, 12345);
    assert_eq!(lease.creation_time_ms, 1700000000000);
}

// ── write_lease / remove_lease / scan_leases ───────────────────────────────

#[test]
fn write_lease_creates_file() {
    let lease = ProcessLease::new(
        "test-engine",
        "inst-write-001",
        4242,
        1700000000000,
        "C:/test/python.exe",
        "127.0.0.1:8100",
        "fp:abcdef0123456789",
        "gen-001",
    );
    write_lease(&lease).expect("写入 lease 应成功");

    // 验证文件存在
    let path = lease_path("test-engine");
    assert!(path.exists(), "lease 文件应存在");

    // 验证内容
    let content = std::fs::read_to_string(&path).unwrap();
    let read_back: ProcessLease = serde_json::from_str(&content).unwrap();
    assert_eq!(read_back, lease);

    // 清理
    remove_lease_force("test-engine").unwrap();
}

#[test]
fn write_lease_rejects_invalid_engine_id() {
    let lease = ProcessLease::new("../escape", "inst-001", 1, 0, "", "", "", "");
    let result = write_lease(&lease);
    assert!(matches!(result, Err(LeaseError::InvalidEngineId(_))));
}

#[test]
fn write_lease_rejects_empty_engine_id() {
    let lease = ProcessLease::new("", "inst-001", 1, 0, "", "", "", "");
    let result = write_lease(&lease);
    assert!(matches!(result, Err(LeaseError::InvalidEngineId(_))));
}

#[test]
fn remove_lease_instance_mismatch_refused() {
    let lease = ProcessLease::new(
        "test-mismatch",
        "inst-correct",
        5555,
        1700000000000,
        "C:/test/python.exe",
        "127.0.0.1:8100",
        "fp:abcdef0123456789",
        "gen-001",
    );
    write_lease(&lease).expect("写入 lease 应成功");

    // 尝试用错误的 instance_id 删除
    let result = remove_lease("test-mismatch", "inst-wrong");
    assert!(matches!(result, Err(LeaseError::InstanceMismatch { .. })));

    // 文件仍应存在
    let path = lease_path("test-mismatch");
    assert!(path.exists(), "instance_id 不匹配时 lease 不应被删除");

    // 正确 instance_id 可删除
    remove_lease("test-mismatch", "inst-correct").unwrap();
    assert!(!path.exists());
}

#[test]
fn remove_lease_idempotent_when_not_exist() {
    let result = remove_lease("nonexistent-engine", "inst-none");
    assert!(result.is_ok(), "删除不存在的 lease 应幂等返回 Ok");
}

#[test]
fn remove_lease_force_works_without_instance_check() {
    let lease = ProcessLease::new(
        "test-force",
        "inst-001",
        7777,
        1700000000000,
        "C:/test/python.exe",
        "127.0.0.1:8100",
        "fp:abcdef0123456789",
        "gen-001",
    );
    write_lease(&lease).unwrap();

    // 强制删除（不需要 instance_id）
    remove_lease_force("test-force").unwrap();
    let path = lease_path("test-force");
    assert!(!path.exists());
}

#[test]
fn scan_leases_returns_all_valid_leases() {
    // 清理可能存在的旧文件
    let _ = remove_lease_force("scan-1");
    let _ = remove_lease_force("scan-2");

    let lease1 = ProcessLease::new(
        "scan-1",
        "inst-scan-1",
        1111,
        1700000000001,
        "C:/test/python1.exe",
        "127.0.0.1:8101",
        "fp:aaaaaaaaaaaaaaaa",
        "gen-001",
    );
    let lease2 = ProcessLease::new(
        "scan-2",
        "inst-scan-2",
        2222,
        1700000000002,
        "C:/test/python2.exe",
        "127.0.0.1:8102",
        "fp:bbbbbbbbbbbbbbbb",
        "gen-002",
    );

    write_lease(&lease1).unwrap();
    write_lease(&lease2).unwrap();

    let leases = scan_leases();
    assert!(leases.len() >= 2, "应至少扫描到 2 个 lease");

    let has_1 = leases
        .iter()
        .any(|l| l.engine_id == "scan-1" && l.instance_id == "inst-scan-1");
    let has_2 = leases
        .iter()
        .any(|l| l.engine_id == "scan-2" && l.instance_id == "inst-scan-2");
    assert!(has_1, "应扫描到 scan-1");
    assert!(has_2, "应扫描到 scan-2");

    // 清理
    remove_lease_force("scan-1").unwrap();
    remove_lease_force("scan-2").unwrap();
}

#[test]
fn scan_leases_skips_corrupt_files() {
    let _ = remove_lease_force("corrupt-test");

    // 先写一个合法 lease
    let lease = ProcessLease::new(
        "corrupt-test",
        "inst-corrupt",
        3333,
        1700000000003,
        "C:/test/python.exe",
        "127.0.0.1:8103",
        "fp:cccccccccccccccc",
        "gen-003",
    );
    write_lease(&lease).unwrap();

    // 写一个损坏的 JSON 文件
    let dir = leases_dir();
    let corrupt_path = dir.join("corrupt-bad.json");
    std::fs::write(&corrupt_path, b"{ this is not valid json }").unwrap();

    let leases = scan_leases();
    // 损坏文件应被跳过，不 panic
    let has_valid = leases
        .iter()
        .any(|l| l.engine_id == "corrupt-test" && l.instance_id == "inst-corrupt");
    assert!(has_valid, "合法 lease 应被扫描到");

    // 清理
    remove_lease_force("corrupt-test").unwrap();
    let _ = std::fs::remove_file(&corrupt_path);
}

#[test]
fn write_lease_overwrites_previous() {
    let _ = remove_lease_force("overwrite-test");

    let lease1 = ProcessLease::new(
        "overwrite-test",
        "inst-v1",
        100,
        1700000000100,
        "C:/v1/python.exe",
        "127.0.0.1:8100",
        "fp:1111111111111111",
        "gen-v1",
    );
    write_lease(&lease1).unwrap();

    let lease2 = ProcessLease::new(
        "overwrite-test",
        "inst-v2",
        200,
        1700000000200,
        "C:/v2/python.exe",
        "127.0.0.1:8200",
        "fp:2222222222222222",
        "gen-v2",
    );
    write_lease(&lease2).unwrap();

    // 应读到 v2
    let path = lease_path("overwrite-test");
    let content = std::fs::read_to_string(&path).unwrap();
    let read_back: ProcessLease = serde_json::from_str(&content).unwrap();
    assert_eq!(read_back.instance_id, "inst-v2");
    assert_eq!(read_back.pid, 200);

    // v1 的 instance_id 不应能删除（已被 v2 替换）
    let result = remove_lease("overwrite-test", "inst-v1");
    assert!(matches!(result, Err(LeaseError::InstanceMismatch { .. })));

    // v2 的 instance_id 可删除
    remove_lease("overwrite-test", "inst-v2").unwrap();
    assert!(!path.exists());
}

// ── decide_recovery 恢复决策 ──────────────────────────────────────────────

/// 构造一个"全部匹配"的 lease + evidence 组合。
fn make_matching_case() -> (ProcessLease, ProcessEvidence, HealthEvidence) {
    let lease = ProcessLease::new(
        "funasr",
        "inst-match",
        12345,
        1700000000000,
        "C:/blink/python.exe",
        "127.0.0.1:8100",
        "fp:abcdef0123456789",
        "gen-001",
    );
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: Some("C:/blink/python.exe".to_string()),
        actual_creation_time_ms: Some(1700000000000),
    };
    let health = HealthEvidence {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-match".to_string()),
        token_fingerprint: Some("fp:abcdef0123456789".to_string()),
    };
    (lease, process, health)
}

#[test]
fn recovery_all_match_returns_adoptable() {
    let (lease, process, health) = make_matching_case();
    let decision = decide_recovery(&lease, &process, Some(&health));
    assert_eq!(
        decision,
        RecoveryDecision::Adoptable {
            engine_id: "funasr".to_string(),
            instance_id: "inst-match".to_string(),
            pid: 12345,
        }
    );
}

#[test]
fn recovery_pid_not_found_returns_do_not_adopt() {
    let (lease, _process, health) = make_matching_case();
    let process = ProcessEvidence {
        pid_exists: false,
        actual_executable: None,
        actual_creation_time_ms: None,
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::PidNotFound);
            assert_eq!(diag.pid, 12345);
        }
        RecoveryDecision::Adoptable { .. } => panic!("PID 不存在时不应 Adoptable"),
    }
}

#[test]
fn recovery_executable_mismatch_returns_do_not_adopt() {
    let (lease, _process, health) = make_matching_case();
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: Some("C:/totally/different.exe".to_string()),
        actual_creation_time_ms: Some(1700000000000),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert!(matches!(
                diag.reason,
                RecoveryReason::ExecutableMismatch { .. }
            ));
        }
        RecoveryDecision::Adoptable { .. } => panic!("路径不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_creation_time_mismatch_returns_do_not_adopt() {
    let (lease, _process, health) = make_matching_case();
    // 创建时间差 10 秒（远超 2 秒容差）
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: Some("C:/blink/python.exe".to_string()),
        actual_creation_time_ms: Some(1700000000000 + 10_000),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert!(matches!(
                diag.reason,
                RecoveryReason::CreationTimeMismatch { .. }
            ));
        }
        RecoveryDecision::Adoptable { .. } => panic!("creation time 不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_creation_time_within_tolerance_adoptable() {
    // 1.5 秒误差在 2 秒容差内
    let (lease, _process, health) = make_matching_case();
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: Some("C:/blink/python.exe".to_string()),
        actual_creation_time_ms: Some(1700000000000 + 1_500),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    assert!(
        matches!(decision, RecoveryDecision::Adoptable { .. }),
        "1.5s 误差在 2s 容差内应 Adoptable"
    );
}

#[test]
fn recovery_creation_time_zero_in_lease_returns_do_not_adopt() {
    let (mut lease, process, health) = make_matching_case();
    lease.creation_time_ms = 0;
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::CreationTimeMissing);
        }
        RecoveryDecision::Adoptable { .. } => panic!("creation_time_ms=0 时不应 Adoptable"),
    }
}

#[test]
fn recovery_process_query_failed_returns_do_not_adopt() {
    let (lease, _process, health) = make_matching_case();
    // PID 存在但无法查询可执行路径
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: None,
        actual_creation_time_ms: Some(1700000000000),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::ProcessQueryFailed);
        }
        RecoveryDecision::Adoptable { .. } => panic!("查询失败时不应 Adoptable"),
    }
}

#[test]
fn recovery_health_unreachable_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let decision = decide_recovery(&lease, &process, None);
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::HealthUnreachable);
        }
        RecoveryDecision::Adoptable { .. } => panic!("health 不可达时不应 Adoptable"),
    }
}

#[test]
fn recovery_token_fingerprint_mismatch_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-match".to_string()),
        token_fingerprint: Some("fp:wrongfingerprint".to_string()),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::TokenFingerprintMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("token 不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_instance_id_mismatch_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-wrong".to_string()),
        token_fingerprint: Some("fp:abcdef0123456789".to_string()),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::InstanceIdMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("instance 不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_engine_id_mismatch_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: Some("wrong-engine".to_string()),
        instance_id: Some("inst-match".to_string()),
        token_fingerprint: Some("fp:abcdef0123456789".to_string()),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::EngineIdMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("engine 不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_schema_version_mismatch_returns_do_not_adopt() {
    let (mut lease, process, health) = make_matching_case();
    lease.schema_version = 999;
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert!(matches!(diag.reason, RecoveryReason::SchemaVersion { .. }));
        }
        RecoveryDecision::Adoptable { .. } => panic!("schema 版本不符时不应 Adoptable"),
    }
}

#[test]
fn recovery_health_missing_engine_id_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: None,
        instance_id: Some("inst-match".to_string()),
        token_fingerprint: Some("fp:abcdef0123456789".to_string()),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::EngineIdMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("缺少 engine id 时不应 Adoptable"),
    }
}

#[test]
fn recovery_health_missing_instance_id_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: Some("funasr".to_string()),
        instance_id: None,
        token_fingerprint: Some("fp:abcdef0123456789".to_string()),
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::InstanceIdMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("缺少 instance id 时不应 Adoptable"),
    }
}

#[test]
fn recovery_health_missing_token_fingerprint_returns_do_not_adopt() {
    let (lease, process, _health) = make_matching_case();
    let health = HealthEvidence {
        engine_id: Some("funasr".to_string()),
        instance_id: Some("inst-match".to_string()),
        token_fingerprint: None,
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::TokenFingerprintMismatch);
        }
        RecoveryDecision::Adoptable { .. } => panic!("缺少 token fingerprint 时不应 Adoptable"),
    }
}

#[test]
fn recovery_creation_time_query_failed_returns_do_not_adopt() {
    let (lease, _process, health) = make_matching_case();
    // PID 存在，可执行路径匹配，但创建时间无法查询
    let process = ProcessEvidence {
        pid_exists: true,
        actual_executable: Some("C:/blink/python.exe".to_string()),
        actual_creation_time_ms: None,
    };
    let decision = decide_recovery(&lease, &process, Some(&health));
    match decision {
        RecoveryDecision::DoNotAdopt(diag) => {
            assert_eq!(diag.reason, RecoveryReason::ProcessQueryFailed);
        }
        RecoveryDecision::Adoptable { .. } => panic!("creation time 查询失败时不应 Adoptable"),
    }
}

// ── 路径归一化测试 ─────────────────────────────────────────────────────────

#[test]
fn paths_match_normalized_handles_separator_differences() {
    assert!(paths_match_normalized(
        "C:\\Users\\blink\\python.exe",
        "c:/users/blink/python.exe"
    ));
    assert!(paths_match_normalized(
        "C:/blink/python.exe",
        "C:\\blink\\python.exe"
    ));
    assert!(!paths_match_normalized(
        "C:/blink/python.exe",
        "C:/other/python.exe"
    ));
}

// ── engine_id 验证 ──────────────────────────────────────────────────────────

#[test]
fn validate_engine_id_accepts_valid_ids() {
    assert!(validate_engine_id("funasr"));
    assert!(validate_engine_id("paddleocr"));
    assert!(validate_engine_id("a"));
    assert!(validate_engine_id("engine-1"));
    assert!(validate_engine_id("abc-123-def"));
}

#[test]
fn validate_engine_id_rejects_invalid_ids() {
    assert!(!validate_engine_id(""));
    assert!(!validate_engine_id("FunASR")); // 大写
    assert!(!validate_engine_id("../escape"));
    assert!(!validate_engine_id("has space"));
    assert!(!validate_engine_id("has/slash"));
    assert!(!validate_engine_id("has:colon"));
    assert!(!validate_engine_id(&"a".repeat(65))); // 过长
}
