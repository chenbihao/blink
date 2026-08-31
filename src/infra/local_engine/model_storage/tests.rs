use super::*;
use crate::infra::local_engine::runtime::EngineId;

// ── asset_key 编码 ──────────────────────────────────────────────────

#[test]
fn encode_iic_sensevoice_small() {
    assert!(encode_asset_key("iic/SenseVoiceSmall").starts_with("iic-sensevoicesmall-"));
}

#[test]
fn encode_paraformer_zh() {
    assert!(encode_asset_key("paraformer-zh").starts_with("paraformer-zh-"));
}

#[test]
fn encode_with_underscores() {
    assert!(encode_asset_key("my_model_v2").starts_with("my-model-v2-"));
}

#[test]
fn encode_with_dots() {
    assert!(encode_asset_key("model.v2.0").starts_with("model-v2-0-"));
}

#[test]
fn encode_empty_falls_back_to_model() {
    assert!(encode_asset_key("///").starts_with("model-"));
}

#[test]
fn encode_uppercase_to_lowercase() {
    assert!(encode_asset_key("HelloWorld").starts_with("helloworld-"));
}

#[test]
fn encode_compresses_double_hyphens() {
    assert!(encode_asset_key("a//b").starts_with("a-b-"));
}

#[test]
fn encode_trims_leading_trailing_hyphens() {
    assert!(encode_asset_key("/a/b/").starts_with("a-b-"));
}

#[test]
fn asset_keys_do_not_collide_when_slugs_match() {
    assert_ne!(encode_asset_key("a/b"), encode_asset_key("a-b"));
}

#[test]
fn validate_asset_key_rejects_empty() {
    assert!(validate_asset_key("").is_err());
}

#[test]
fn validate_asset_key_rejects_uppercase() {
    assert!(validate_asset_key("HelloWorld").is_err());
}

#[test]
fn validate_asset_key_rejects_slash() {
    assert!(validate_asset_key("a/b").is_err());
}

#[test]
fn validate_asset_key_rejects_double_hyphen() {
    assert!(validate_asset_key("a--b").is_err());
}

#[test]
fn validate_asset_key_accepts_valid() {
    assert!(validate_asset_key("iic-sensevoicesmall").is_ok());
    assert!(validate_asset_key("paraformer-zh").is_ok());
}

// ── fingerprint 算法 ─────────────────────────────────────────────────

/// 创建测试 fixture 目录
fn make_fixture(name: &str) -> PathBuf {
    let base = std::env::temp_dir()
        .join("blink-model-storage-tests")
        .join(format!("{}-{}", std::process::id(), name));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(&base).unwrap();
    base
}

fn write_file(dir: &Path, rel: &str, content: &[u8]) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).unwrap();
    }
    std::fs::write(&path, content).unwrap();
}

#[test]
fn fingerprint_empty_dir() {
    let dir = make_fixture("fingerprint_empty");
    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 0);
    assert_eq!(fp.total_size_bytes, 0);
    // 空 SHA-256
    assert_eq!(
        fp.fingerprint,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprint_single_file() {
    let dir = make_fixture("fingerprint_single");
    write_file(&dir, "model.bin", b"hello world");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 1);
    assert_eq!(fp.total_size_bytes, 11);
    assert_eq!(fp.fingerprint.len(), 64);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprint_deterministic_same_content() {
    // 两个相同内容的目录应有相同 fingerprint
    let dir1 = make_fixture("fp_det_1");
    let dir2 = make_fixture("fp_det_2");

    write_file(&dir1, "a.bin", b"content_a");
    write_file(&dir1, "b.bin", b"content_b");
    write_file(&dir2, "a.bin", b"content_a");
    write_file(&dir2, "b.bin", b"content_b");

    let fp1 = compute_content_fingerprint(&dir1).unwrap();
    let fp2 = compute_content_fingerprint(&dir2).unwrap();

    assert_eq!(fp1.fingerprint, fp2.fingerprint);

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn fingerprint_content_change_detected() {
    let dir1 = make_fixture("fp_change_1");
    let dir2 = make_fixture("fp_change_2");

    write_file(&dir1, "a.bin", b"content_a");
    write_file(&dir2, "a.bin", b"content_b"); // 不同内容

    let fp1 = compute_content_fingerprint(&dir1).unwrap();
    let fp2 = compute_content_fingerprint(&dir2).unwrap();

    assert_ne!(fp1.fingerprint, fp2.fingerprint);

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn fingerprint_path_change_detected() {
    let dir1 = make_fixture("fp_path_1");
    let dir2 = make_fixture("fp_path_2");

    write_file(&dir1, "a.bin", b"same");
    write_file(&dir2, "b.bin", b"same"); // 不同路径

    let fp1 = compute_content_fingerprint(&dir1).unwrap();
    let fp2 = compute_content_fingerprint(&dir2).unwrap();

    assert_ne!(fp1.fingerprint, fp2.fingerprint);

    let _ = std::fs::remove_dir_all(&dir1);
    let _ = std::fs::remove_dir_all(&dir2);
}

#[test]
fn fingerprint_empty_file() {
    let dir = make_fixture("fp_empty_file");
    write_file(&dir, "empty.bin", b"");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 1);
    assert_eq!(fp.total_size_bytes, 0);
    assert_eq!(fp.fingerprint.len(), 64);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprint_unicode_filename() {
    let dir = make_fixture("fp_unicode");
    write_file(&dir, "模型.bin", b"data");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 1);
    assert_eq!(fp.total_size_bytes, 4);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprint_excludes_manifest() {
    let dir = make_fixture("fp_exclude_manifest");

    write_file(&dir, "model.bin", b"model_data");
    // manifest.json 应被排除
    write_file(&dir, "manifest.json", b"should_be_excluded");
    // active.json 应被排除
    write_file(&dir, "active.json", b"should_be_excluded");
    // 临时文件应被排除
    write_file(&dir, ".tmp_file", b"should_be_excluded");
    // 下载锁应被排除
    write_file(&dir, ".download_lock", b"should_be_excluded");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 1); // 只有 model.bin
    assert_eq!(fp.total_size_bytes, 10); // "model_data"

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn fingerprint_nested_directories() {
    let dir = make_fixture("fp_nested");

    write_file(&dir, "a.bin", b"aaa");
    write_file(&dir, "sub/b.bin", b"bbb");
    write_file(&dir, "sub/deep/c.bin", b"ccc");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 3);
    assert_eq!(fp.total_size_bytes, 9);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 流式哈希与全量读取算法逐字节等价（大文件跨 1MB 缓冲边界）。
///
/// 参考实现 = Python 侧 `blink_model_installer.py::compute_content_fingerprint`
/// 的直译（全量 read 后 update），两者必须产出相同 hex。
#[test]
fn fingerprint_streaming_equivalent_to_full_read() {
    use sha2::{Digest, Sha256};

    let dir = make_fixture("fp_streaming");
    // 2.5MB 大文件（跨块边界）+ 小文件，确定性伪随机内容
    let mut big = Vec::with_capacity(2 * 1024 * 1024 + 512 * 1024);
    let mut x: u32 = 0x1234_5678;
    for _ in 0..(2 * 1024 * 1024 + 512 * 1024) / 4 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
        big.extend_from_slice(&x.to_le_bytes());
    }
    write_file(&dir, "big.bin", &big);
    write_file(&dir, "sub/small.bin", b"small");

    let fp = compute_content_fingerprint(&dir).unwrap();

    // 全量读取参考实现（Python 算法直译）
    let mut hasher = Sha256::new();
    for (rel, content) in [
        ("big.bin", big.as_slice()),
        ("sub/small.bin", b"small".as_slice()),
    ] {
        let rel_bytes = rel.as_bytes();
        hasher.update((rel_bytes.len() as u64).to_le_bytes());
        hasher.update(rel_bytes);
        hasher.update((content.len() as u64).to_le_bytes());
        hasher.update(content);
    }
    let expected = format!("{:x}", hasher.finalize());

    assert_eq!(fp.fingerprint, expected, "流式与全量算法必须产出相同指纹");
    assert_eq!(fp.file_count, 2);
    assert_eq!(fp.total_size_bytes, (big.len() + 5) as u64);

    let _ = std::fs::remove_dir_all(&dir);
}

// ── Golden fingerprint 测试（Rust/Python 共享规范）──────────────────────
//
// 以下测试使用固定内容的文件，产生确定的指纹值。
// Python 侧 `test_fingerprint_golden.py` 使用完全相同的 fixture 内容，
// 验证两边产生完全一致的 hex SHA-256。
//
// 如果此处的 expected 值需要变更，必须同时更新 Python 侧 golden test。

/// Golden fixture 1：单个文件，内容 "hello world"
/// 使用确定性验证：两次计算同一 fixture 必须得到相同值。
/// 跨语言一致性由 Python golden test 验证（使用完全相同的 fixture 内容）。
#[test]
fn golden_fingerprint_single_file() {
    let dir = make_fixture("golden_single");
    write_file(&dir, "model.bin", b"hello world");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 1);
    assert_eq!(fp.total_size_bytes, 11);

    // 确定性：同一 fixture 两次计算必相同
    let dir2 = make_fixture("golden_single_2");
    write_file(&dir2, "model.bin", b"hello world");
    let fp2 = compute_content_fingerprint(&dir2).unwrap();
    assert_eq!(fp.fingerprint, fp2.fingerprint);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// Golden fixture 2：两个嵌套文件 + 排序验证
#[test]
fn golden_fingerprint_nested_sorted() {
    let dir = make_fixture("golden_nested");

    write_file(&dir, "b/model.pt", b"model_b_data");
    write_file(&dir, "a/model.pt", b"model_a_data");
    write_file(&dir, "config.json", b"{\"version\":1}");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 3);
    assert_eq!(fp.total_size_bytes, 37); // 12 + 12 + 13

    // 确定性：同一 fixture 两次计算必相同
    let dir2 = make_fixture("golden_nested_2");
    write_file(&dir2, "b/model.pt", b"model_b_data");
    write_file(&dir2, "a/model.pt", b"model_a_data");
    write_file(&dir2, "config.json", b"{\"version\":1}");
    let fp2 = compute_content_fingerprint(&dir2).unwrap();
    assert_eq!(fp.fingerprint, fp2.fingerprint);

    eprintln!("golden_fingerprint_nested_sorted = {}", fp.fingerprint);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

/// Golden fixture 3：空目录 + manifest 排除
/// 空 SHA-256 是 e3b0c442...（已在上面的 fingerprint_empty_dir 测试中验证）
#[test]
fn golden_fingerprint_empty_with_manifest_excluded() {
    let dir = make_fixture("golden_empty_meta");

    // 只写 manifest.json + active.json（都应被排除）
    write_file(&dir, "manifest.json", b"{\"test\":true}");
    write_file(&dir, "active.json", b"{\"slot_id\":\"test\"}");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 0);
    assert_eq!(fp.total_size_bytes, 0);

    // 空目录的 SHA-256 = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
    assert_eq!(
        fp.fingerprint,
        "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// Golden fixture 4：混合内容（模拟真实模型目录结构）
#[test]
fn golden_fingerprint_model_like() {
    let dir = make_fixture("golden_model_like");

    // 模拟 FunASR 模型目录结构
    write_file(
        &dir,
        "model.pt",
        b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
    );
    write_file(
        &dir,
        "configuration.json",
        b"{\"model\":\"SenseVoice\",\"language\":\"zh\"}",
    );
    write_file(&dir, "examples/sample.wav", b"WAVE\x12\x34\x56\x78");
    write_file(&dir, "subdir/weights.bin", b"\xff\xfe\xfd\xfc");

    let fp = compute_content_fingerprint(&dir).unwrap();
    assert_eq!(fp.file_count, 4);
    assert_eq!(fp.total_size_bytes, 66); // 16 + 38 + 8 + 4

    // 确定性验证
    let dir2 = make_fixture("golden_model_like_2");
    write_file(
        &dir2,
        "model.pt",
        b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
    );
    write_file(
        &dir2,
        "configuration.json",
        b"{\"model\":\"SenseVoice\",\"language\":\"zh\"}",
    );
    write_file(&dir2, "examples/sample.wav", b"WAVE\x12\x34\x56\x78");
    write_file(&dir2, "subdir/weights.bin", b"\xff\xfe\xfd\xfc");
    let fp2 = compute_content_fingerprint(&dir2).unwrap();
    assert_eq!(fp.fingerprint, fp2.fingerprint);

    eprintln!("golden_fingerprint_model_like = {}", fp.fingerprint);

    let _ = std::fs::remove_dir_all(&dir);
    let _ = std::fs::remove_dir_all(&dir2);
}

// ── restore_model_state ──────────────────────────────────────────────

#[test]
fn restore_not_installed_when_no_pointer() {
    let engine = EngineId::new("funasr").unwrap();
    let dir = make_fixture("restore_not_installed");
    // 保存 runtimes_root 测试根，确保指向临时目录
    // models_root() 在 test 模式下返回 runtimes_root()/models
    // runtimes_root() 在 test 模式下返回 temp_dir()/blink-runtime-tests-{pid}
    // 所以 model_storage 的路径自动隔离

    let asset_key = "test-restore-not-installed";

    // 没有任何文件 → NotInstalled
    let state = restore_model_state(&engine, asset_key).unwrap();
    assert_eq!(state, RestoredModelState::NotInstalled);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn restore_installed_when_valid() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-restore-installed";

    let slot_id = "slot-test-0001";
    let payload_dir = model_payload_dir(&engine, asset_key, slot_id).unwrap();
    std::fs::create_dir_all(&payload_dir).unwrap();
    write_file(&payload_dir, "model.bin", b"model_data");

    let fp = compute_content_fingerprint(&payload_dir).unwrap();

    let manifest = ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test-model".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: fp.fingerprint.clone(),
        payload_size_bytes: fp.total_size_bytes,
        file_count: fp.file_count,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    };

    write_model_manifest(&engine, asset_key, slot_id, &manifest).unwrap();

    let pointer = ModelActivePointer {
        slot_id: slot_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
    };
    write_model_active_pointer(&engine, asset_key, &pointer).unwrap();

    // 恢复 → Installed
    let state = restore_model_state(&engine, asset_key).unwrap();
    match state {
        RestoredModelState::Installed {
            slot_id: restored_slot,
            manifest: m,
        } => {
            assert_eq!(restored_slot, slot_id);
            assert_eq!(m.model_id, "test-model");
            assert_eq!(m.content_fingerprint, fp.fingerprint);
        }
        other => panic!("expected Installed, got {:?}", other),
    }

    // 清理
    let root = asset_root(&engine, asset_key).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_structural_ok_but_explicit_verify_catches_fingerprint_mismatch() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-restore-corrupted-fp";

    let slot_id = "slot-test-corrupt-0001";
    let payload_dir = model_payload_dir(&engine, asset_key, slot_id).unwrap();
    std::fs::create_dir_all(&payload_dir).unwrap();
    write_file(&payload_dir, "model.bin", b"model_data");

    let manifest = ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test-model".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: "wrong_fingerprint".to_string(), // 故意错误
        payload_size_bytes: 10,
        file_count: 1,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    };

    write_model_manifest(&engine, asset_key, slot_id, &manifest).unwrap();

    let pointer = ModelActivePointer {
        slot_id: slot_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
    };
    write_model_active_pointer(&engine, asset_key, &pointer).unwrap();

    // restore 只做结构校验 → Installed（不做 GB hash）
    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { .. } => {}
        other => panic!("expected Installed, got {:?}", other),
    }

    // 显式完整校验 → fingerprint 不匹配被抓到
    let err = verify_model_payload(&engine, asset_key, &manifest).unwrap_err();
    assert!(err.contains("fingerprint 不匹配"), "unexpected: {err}");

    // 清理
    let root = asset_root(&engine, asset_key).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn restore_corrupted_when_payload_missing() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-restore-corrupted-payload";

    let slot_id = "slot-test-corrupt-0002";
    // 不创建 payload 目录

    let manifest = ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test-model".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: "any".to_string(),
        payload_size_bytes: 0,
        file_count: 0,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    };

    write_model_manifest(&engine, asset_key, slot_id, &manifest).unwrap();

    let pointer = ModelActivePointer {
        slot_id: slot_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
    };
    write_model_active_pointer(&engine, asset_key, &pointer).unwrap();

    // 恢复 → Corrupted（payload 不存在）
    let state = restore_model_state(&engine, asset_key).unwrap();
    match state {
        RestoredModelState::Corrupted { reason, .. } => {
            assert!(reason.contains("payload"));
        }
        other => panic!("expected Corrupted, got {:?}", other),
    }

    // 清理
    let root = asset_root(&engine, asset_key).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

// ── promote + delete ─────────────────────────────────────────────────

#[test]
fn promote_staging_commits_single_active_slot() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-promote";
    let operation_id = "op-test-promote-0001";
    let slot_id = "slot-test-promote-0001";

    // 创建 staging payload
    let staging_payload =
        model_operation_staging_payload_dir(&engine, asset_key, operation_id).unwrap();
    std::fs::create_dir_all(&staging_payload).unwrap();
    write_file(&staging_payload, "model.bin", b"model_data");

    let fp = compute_content_fingerprint(&staging_payload).unwrap();

    let manifest = ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test-model".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: fp.fingerprint.clone(),
        payload_size_bytes: fp.total_size_bytes,
        file_count: fp.file_count,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    };

    promote_staging_to_active_slot(&engine, asset_key, slot_id, operation_id, &manifest).unwrap();

    let slot_payload = model_payload_dir(&engine, asset_key, slot_id).unwrap();
    assert!(slot_payload.exists());
    assert!(slot_payload.join("model.bin").exists());

    let pointer = read_model_active_pointer(&engine, asset_key).unwrap();
    assert!(pointer.is_some());
    assert_eq!(pointer.unwrap().slot_id, slot_id);

    // 清理
    let root = asset_root(&engine, asset_key).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_active_model_removes_pointer_and_slot() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-delete";

    let slot_id = "slot-test-delete-0001";
    let payload_dir = model_payload_dir(&engine, asset_key, slot_id).unwrap();
    std::fs::create_dir_all(&payload_dir).unwrap();
    write_file(&payload_dir, "model.bin", b"data");

    let manifest = ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: "fake".to_string(),
        payload_size_bytes: 4,
        file_count: 1,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    };

    write_model_manifest(&engine, asset_key, slot_id, &manifest).unwrap();
    let pointer = ModelActivePointer {
        slot_id: slot_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
    };
    write_model_active_pointer(&engine, asset_key, &pointer).unwrap();

    // 删除
    delete_active_model(&engine, asset_key).unwrap();

    // 验证
    let pointer_path = model_active_pointer_path(&engine, asset_key).unwrap();
    assert!(!pointer_path.exists());
    let slot_dir = model_slot_dir(&engine, asset_key, slot_id).unwrap();
    assert!(!slot_dir.exists());

    // 清理
    let root = asset_root(&engine, asset_key).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn delete_active_model_fails_when_no_pointer() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "test-delete-no-pointer";

    let result = delete_active_model(&engine, asset_key);
    assert!(result.is_err());
}

// ── 单 active slot 事务：提交与崩溃恢复 ─────────────────────────────

/// 构造带真实 fingerprint 的测试 manifest。
fn test_manifest(engine: &EngineId, slot_id: &str, fp: &ContentFingerprint) -> ModelManifest {
    ModelManifest {
        schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
        engine_id: engine.clone(),
        model_id: "test-model".to_string(),
        revision: "v1".to_string(),
        source: ModelSource::Unverified {
            source: "test".to_string(),
            downloaded_at_ms: now_ms(),
        },
        slot_id: slot_id.to_string(),
        installed_at_ms: now_ms(),
        content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
        content_fingerprint: fp.fingerprint.clone(),
        payload_size_bytes: fp.total_size_bytes,
        file_count: fp.file_count,
        compatibility_schema: 1,
        model_contract_identity: ModelContractIdentity {
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            checksum_source_kind: "unverified".to_string(),
        },
    }
}

/// 通过 staging → promote 完整安装一个 slot 并置为 active。
fn install_slot(engine: &EngineId, asset_key: &str, slot_id: &str, op_id: &str) {
    let staging = model_operation_staging_payload_dir(engine, asset_key, op_id).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    write_file(
        &staging,
        "model.bin",
        format!("payload-{slot_id}").as_bytes(),
    );
    let fp = compute_content_fingerprint(&staging).unwrap();
    let manifest = test_manifest(engine, slot_id, &fp);
    promote_staging_to_active_slot(engine, asset_key, slot_id, op_id, &manifest).unwrap();
}

/// 在磁盘上手工构造「candidate slot 已就位但未切指针」的中间态。
fn materialize_candidate(engine: &EngineId, asset_key: &str, slot_id: &str) {
    let payload = model_payload_dir(engine, asset_key, slot_id).unwrap();
    std::fs::create_dir_all(&payload).unwrap();
    write_file(
        &payload,
        "model.bin",
        format!("payload-{slot_id}").as_bytes(),
    );
    let fp = compute_content_fingerprint(&payload).unwrap();
    let manifest = test_manifest(engine, slot_id, &fp);
    write_model_manifest(engine, asset_key, slot_id, &manifest).unwrap();
}

/// 手工写 journal（模拟崩溃现场）。
fn write_journal(
    engine: &EngineId,
    asset_key: &str,
    candidate: &str,
    previous: Option<&str>,
    phase: ModelTransactionPhase,
) {
    let tx = ModelTransaction {
        schema_version: MODEL_TRANSACTION_SCHEMA_VERSION,
        operation_id: "op-crash-sim".to_string(),
        candidate_slot_id: candidate.to_string(),
        previous_slot_id: previous.map(str::to_string),
        phase,
    };
    write_transaction(engine, asset_key, &tx).unwrap();
}

fn journal_exists(engine: &EngineId, asset_key: &str) -> bool {
    transaction_path(engine, asset_key).unwrap().exists()
}

#[test]
fn update_promote_deletes_previous_and_keeps_single_active() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-update-single-active";
    install_slot(&engine, asset_key, "slot-old-0001", "op-update-0001");
    install_slot(&engine, asset_key, "slot-new-0002", "op-update-0002");

    // 稳定状态只剩一个 active slot
    let pointer = read_model_active_pointer(&engine, asset_key)
        .unwrap()
        .unwrap();
    assert_eq!(pointer.slot_id, "slot-new-0002");
    assert!(
        !model_slot_dir(&engine, asset_key, "slot-old-0001")
            .unwrap()
            .exists()
    );
    assert!(
        model_slot_dir(&engine, asset_key, "slot-new-0002")
            .unwrap()
            .exists()
    );
    assert!(!journal_exists(&engine, asset_key));

    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => {
            assert_eq!(slot_id, "slot-new-0002");
        }
        other => panic!("expected Installed, got {:?}", other),
    }

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 崩溃点：journal=Preparing 且指针未切换（仍指向旧 active）→ 回滚删 candidate。
#[test]
fn recovery_preparing_before_pointer_rolls_back_to_old_active() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-crash-preparing-rollback";
    install_slot(&engine, asset_key, "slot-old-0001", "op-crash-0001");
    materialize_candidate(&engine, asset_key, "slot-cand-0002");
    write_journal(
        &engine,
        asset_key,
        "slot-cand-0002",
        Some("slot-old-0001"),
        ModelTransactionPhase::Preparing,
    );

    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => {
            assert_eq!(slot_id, "slot-old-0001", "回滚后旧 active 保持");
        }
        other => panic!("expected Installed, got {:?}", other),
    }
    // candidate 已回滚删除；journal 已消费
    assert!(
        !model_slot_dir(&engine, asset_key, "slot-cand-0002")
            .unwrap()
            .exists()
    );
    assert!(!journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 崩溃窗口：active.json 已切到 candidate 但 journal 仍是 Preparing
/// （指针写入与 journal 更新之间崩溃）→ 按已提交处理，绝不删除
/// 指针已指向的 candidate，只完成旧 slot 清理。
#[test]
fn recovery_preparing_after_pointer_write_rolls_forward() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-crash-window-rollforward";
    install_slot(&engine, asset_key, "slot-old-0001", "op-crash-0001");
    materialize_candidate(&engine, asset_key, "slot-cand-0002");
    write_journal(
        &engine,
        asset_key,
        "slot-cand-0002",
        Some("slot-old-0001"),
        ModelTransactionPhase::Preparing,
    );
    // 模拟提交点已越过：指针已指向 candidate
    write_model_active_pointer(
        &engine,
        asset_key,
        &ModelActivePointer {
            slot_id: "slot-cand-0002".to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();

    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => {
            assert_eq!(slot_id, "slot-cand-0002", "指针指向的 candidate 必须存活");
        }
        other => panic!("expected Installed, got {:?}", other),
    }
    // 旧 slot 完成清理；candidate 仍在；journal 已消费
    assert!(
        !model_slot_dir(&engine, asset_key, "slot-old-0001")
            .unwrap()
            .exists()
    );
    assert!(
        model_slot_dir(&engine, asset_key, "slot-cand-0002")
            .unwrap()
            .exists()
    );
    assert!(!journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 崩溃点：journal=Committed、旧 slot 尚未删除 → 完成已提交清理。
#[test]
fn recovery_committed_finishes_previous_cleanup() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-crash-committed-cleanup";
    install_slot(&engine, asset_key, "slot-old-0001", "op-crash-0001");
    materialize_candidate(&engine, asset_key, "slot-cand-0002");
    write_model_active_pointer(
        &engine,
        asset_key,
        &ModelActivePointer {
            slot_id: "slot-cand-0002".to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();
    write_journal(
        &engine,
        asset_key,
        "slot-cand-0002",
        Some("slot-old-0001"),
        ModelTransactionPhase::Committed,
    );

    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => {
            assert_eq!(slot_id, "slot-cand-0002");
        }
        other => panic!("expected Installed, got {:?}", other),
    }
    assert!(
        !model_slot_dir(&engine, asset_key, "slot-old-0001")
            .unwrap()
            .exists()
    );
    assert!(!journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 崩溃点：首次安装 Committed（无 previous）→ 只消费 journal。
#[test]
fn recovery_committed_first_install_without_previous() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-crash-committed-first";
    materialize_candidate(&engine, asset_key, "slot-first-0001");
    write_model_active_pointer(
        &engine,
        asset_key,
        &ModelActivePointer {
            slot_id: "slot-first-0001".to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();
    write_journal(
        &engine,
        asset_key,
        "slot-first-0001",
        None,
        ModelTransactionPhase::Committed,
    );

    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => {
            assert_eq!(slot_id, "slot-first-0001");
        }
        other => panic!("expected Installed, got {:?}", other),
    }
    assert!(!journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// journal 声称 Committed，但 active 仍指向 previous：事务事实不一致，
/// 必须 fail-closed，保留两个 slot 与 journal 供显式恢复。
#[test]
fn recovery_committed_pointer_mismatch_preserves_all_data() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-committed-pointer-mismatch";
    install_slot(&engine, asset_key, "slot-old-0001", "op-mismatch-0001");
    materialize_candidate(&engine, asset_key, "slot-cand-0002");
    write_journal(
        &engine,
        asset_key,
        "slot-cand-0002",
        Some("slot-old-0001"),
        ModelTransactionPhase::Committed,
    );

    let error = recover_model_transaction(&engine, asset_key).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::TransactionJournalInvalid { .. }
    ));
    assert_eq!(
        read_model_active_pointer(&engine, asset_key)
            .unwrap()
            .unwrap()
            .slot_id,
        "slot-old-0001"
    );
    assert!(
        model_slot_dir(&engine, asset_key, "slot-old-0001")
            .unwrap()
            .exists(),
        "当前 active 不得被删除"
    );
    assert!(
        model_slot_dir(&engine, asset_key, "slot-cand-0002")
            .unwrap()
            .exists(),
        "不一致事务的 candidate 也应保留供显式恢复"
    );
    assert!(journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// active pointer 无法解析时不能猜测提交状态，更不能删除任何 slot。
#[test]
fn recovery_corrupted_pointer_preserves_all_data_and_journal() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-corrupted-pointer-preserve";
    install_slot(&engine, asset_key, "slot-old-0001", "op-corrupt-0001");
    materialize_candidate(&engine, asset_key, "slot-cand-0002");
    write_journal(
        &engine,
        asset_key,
        "slot-cand-0002",
        Some("slot-old-0001"),
        ModelTransactionPhase::Committed,
    );
    std::fs::write(
        model_active_pointer_path(&engine, asset_key).unwrap(),
        b"{not-json",
    )
    .unwrap();

    let error = recover_model_transaction(&engine, asset_key).unwrap_err();
    assert!(matches!(
        error,
        RuntimeError::CurrentPointerParseFailed { .. }
    ));
    assert!(
        model_slot_dir(&engine, asset_key, "slot-old-0001")
            .unwrap()
            .exists()
    );
    assert!(
        model_slot_dir(&engine, asset_key, "slot-cand-0002")
            .unwrap()
            .exists()
    );
    assert!(journal_exists(&engine, asset_key));

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 指针切换失败（active.json 被只读阻塞）→ 旧 active 保持，candidate 回收。
#[test]
fn pointer_switch_failure_keeps_old_active() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-pointer-switch-fail";
    install_slot(&engine, asset_key, "slot-old-0001", "op-psf-0001");

    // 阻塞 active.json 的原子替换（MoveFileEx 不能替换只读文件）
    let pointer_path = model_active_pointer_path(&engine, asset_key).unwrap();
    let mut perms = std::fs::metadata(&pointer_path).unwrap().permissions();
    perms.set_readonly(true);
    std::fs::set_permissions(&pointer_path, perms).unwrap();

    let staging = model_operation_staging_payload_dir(&engine, asset_key, "op-psf-0002").unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    write_file(&staging, "model.bin", b"payload-new");
    let fp = compute_content_fingerprint(&staging).unwrap();
    let manifest = test_manifest(&engine, "slot-new-0002", &fp);
    let result = promote_staging_to_active_slot(
        &engine,
        asset_key,
        "slot-new-0002",
        "op-psf-0002",
        &manifest,
    );
    assert!(result.is_err(), "只读 active.json 必须使指针切换失败");

    // 旧 active 未被破坏
    let pointer = read_model_active_pointer(&engine, asset_key)
        .unwrap()
        .unwrap();
    assert_eq!(pointer.slot_id, "slot-old-0001");
    assert!(!journal_exists(&engine, asset_key));
    assert!(
        !model_slot_dir(&engine, asset_key, "slot-new-0002")
            .unwrap()
            .exists(),
        "失败的 candidate 应被回收"
    );

    // 解除阻塞后旧 active 仍可正常恢复
    #[allow(clippy::permissions_set_readonly_false)] // Windows 测试：需要解除只读以验证恢复
    {
        let mut perms = std::fs::metadata(&pointer_path).unwrap().permissions();
        perms.set_readonly(false);
        std::fs::set_permissions(&pointer_path, perms).unwrap();
    }
    match restore_model_state(&engine, asset_key).unwrap() {
        RestoredModelState::Installed { slot_id, .. } => assert_eq!(slot_id, "slot-old-0001"),
        other => panic!("expected Installed, got {:?}", other),
    }

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// cancellation 只清理匹配 operation 的 staging。
#[test]
fn cleanup_staging_only_removes_matching_operation() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-staging-scope";
    for op in ["op-a-0001", "op-b-0002"] {
        let dir = model_operation_staging_payload_dir(&engine, asset_key, op).unwrap();
        std::fs::create_dir_all(&dir).unwrap();
        write_file(&dir, "model.bin", b"data");
    }

    cleanup_staging(&engine, asset_key, "op-a-0001").unwrap();
    assert!(
        !model_operation_staging_dir(&engine, asset_key, "op-a-0001")
            .unwrap()
            .exists(),
        "匹配 operation 的 staging 已清理"
    );
    assert!(
        model_operation_staging_dir(&engine, asset_key, "op-b-0002")
            .unwrap()
            .exists(),
        "其他 operation 的 staging 不受影响"
    );

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}

/// 旧 slot 删除失败（文件被占用）记为 residue；重试成功后 residue 收敛清除。
#[test]
fn residue_recorded_when_locked_and_cleared_after_retry() {
    let engine = EngineId::new("funasr").unwrap();
    let asset_key = "tx-residue-retry";
    install_slot(&engine, asset_key, "slot-active-0001", "op-res-0001");

    // 构造暂时无法删除的非 active slot：以无 FILE_SHARE_DELETE 的句柄
    // 占住 payload 文件（模拟杀软扫描/进程占用——Rust std 的 POSIX 语义
    // remove_dir_all 无法删除被此类句柄占用的文件）
    let locked_slot = "slot-locked-0002";
    let payload = model_payload_dir(&engine, asset_key, locked_slot).unwrap();
    std::fs::create_dir_all(&payload).unwrap();
    write_file(&payload, "model.bin", b"locked");
    let locked_file = payload.join("model.bin");
    let held = {
        use std::os::windows::ffi::OsStrExt;
        use windows::Win32::Foundation::GENERIC_READ;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        };
        use windows::core::PCWSTR;
        let wide: Vec<u16> = locked_file
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        // SAFETY: wide 以 NUL 结尾；句柄随后用 CloseHandle 释放
        unsafe {
            CreateFileW(
                PCWSTR(wide.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE, // 不含 FILE_SHARE_DELETE
                None,
                OPEN_EXISTING,
                Default::default(),
                None,
            )
        }
        .unwrap()
    };

    let cleaned = cleanup_inactive_slots(&engine, asset_key, "slot-active-0001").unwrap();
    assert!(cleaned.is_empty(), "暂时无法删除的 slot 不应被清理");
    assert!(
        model_slot_dir(&engine, asset_key, locked_slot)
            .unwrap()
            .exists()
    );
    // residue 已记录
    let residue_file = residue_path(&engine, asset_key).unwrap();
    let residues: Vec<CleanupResidue> =
        serde_json::from_str(&std::fs::read_to_string(&residue_file).unwrap()).unwrap();
    assert_eq!(residues.len(), 1);
    assert_eq!(residues[0].slot_id, locked_slot);

    // 解除占用后重试 → slot 删除 + residue 记录收敛清除
    {
        use windows::Win32::Foundation::CloseHandle;
        // SAFETY: held 由本测试的 CreateFileW 创建，仅关闭一次
        let _ = unsafe { CloseHandle(held) };
    }
    let cleaned = cleanup_inactive_slots(&engine, asset_key, "slot-active-0001").unwrap();
    assert_eq!(cleaned, vec![locked_slot.to_string()]);
    assert!(
        !model_slot_dir(&engine, asset_key, locked_slot)
            .unwrap()
            .exists()
    );
    assert!(!residue_file.exists(), "重试成功后 residue 记录应被清除");

    let _ = std::fs::remove_dir_all(asset_root(&engine, asset_key).unwrap());
}
