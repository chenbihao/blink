//! PaddleOCR adapter 回归测试（自原单文件 `#[cfg(test)] mod tests` 整体迁移，断言不变）。

use super::locks::parse_locked_requirements;
use super::*;
use crate::domain::local_engine::{CapabilityKind, LifecyclePolicy, ModelHealth, ServiceHealth};
use crate::infra::local_engine::providers::{InstallPlan, PackageLock, PipExtraArg};
use crate::infra::local_engine::runtime::{ComputeBackend, ComputePreference};

#[test]
fn descriptor_has_correct_engine_id() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().engine_id.as_str(), PADDLEOCR_ENGINE_ID);
}

#[test]
fn descriptor_has_ocr_capability() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Ocr);
}

#[test]
fn descriptor_has_on_demand_lifecycle() {
    let adapter = PaddleocrAdapter::new();
    assert_eq!(adapter.descriptor().lifecycle, LifecyclePolicy::OnDemand);
}

#[test]
fn paddleocr_model_identity_is_managed_by_wrapper_manifest() {
    let adapter = PaddleocrAdapter::new();
    assert!(!adapter.uses_managed_model_storage());
}

#[test]
fn descriptor_has_cpu_profile_only() {
    let adapter = PaddleocrAdapter::new();
    let candidates = &adapter.descriptor().install_plan.compute_candidates;
    assert_eq!(candidates.len(), 1);
    assert_eq!(candidates[0].preference, ComputePreference::Cpu);
}

#[test]
fn provider_descriptor_packages_match_requirements() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        // 必须包含 paddlepaddle 和 paddleocr
        let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
        assert!(names.contains(&"paddlepaddle"));
        assert!(names.contains(&"paddleocr"));
        assert!(names.contains(&"fastapi"));
        assert!(names.contains(&"uvicorn"));
    } else {
        panic!("expected PythonVenv install plan");
    }
}

/// Task 3: 验证 production descriptor 中不存在空 hash。
///
/// 所有 PackageLock.sha256 必须为 Some(有效 64 位 hex)。
/// 如果 hash 未填充，render_hashed_requirements 会拒绝安装。
#[test]
fn provider_descriptor_no_empty_hashes() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            assert!(
                pkg.sha256.is_some(),
                "PackageLock {} 的 sha256 为 None，不允许空 hash 进入生产安装",
                pkg.name
            );
            let hash = pkg.sha256.as_ref().unwrap();
            assert_eq!(
                hash.len(),
                64,
                "PackageLock {} 的 sha256 长度不是 64: {}",
                pkg.name,
                hash
            );
            assert!(
                hash.bytes().all(|b| b.is_ascii_hexdigit()),
                "PackageLock {} 的 sha256 包含非 hex 字符: {}",
                pkg.name,
                hash
            );
        }
    } else {
        panic!("expected PythonVenv install plan");
    }
}

/// Task 5: 验证 production descriptor 中不存在占位全零 hash。
///
/// 全零 hash（sha256 = "000...0"）格式合法但不是真实 hash，
/// --require-hashes 会因 hash 不匹配而拒绝安装。
/// 生产 descriptor 中所有 hash 必须是真实的、从 PyPI 获取的值。
#[test]
fn provider_descriptor_no_placeholder_zero_hashes() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let zero_hash = "0".repeat(64);
        for pkg in &plan.packages {
            let hash = pkg.sha256.as_ref().unwrap();
            assert_ne!(
                hash, &zero_hash,
                "PackageLock {} 的 sha256 为全零占位值，生产 descriptor 不允许占位 hash",
                pkg.name
            );
        }
    } else {
        panic!("expected PythonVenv install plan");
    }
}

/// Task 5: 验证 production descriptor 中所有 hash 不相同（防重复）。
///
/// 不同包不应有相同的 SHA-256 hash（除非是同一 wheel 文件，
/// 但不同包的 wheel 永远不同）。
#[test]
fn provider_descriptor_hashes_are_distinct() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let hashes: Vec<&str> = plan
            .packages
            .iter()
            .map(|p| p.sha256.as_deref().unwrap())
            .collect();
        let unique: std::collections::HashSet<&str> = hashes.iter().copied().collect();
        assert_eq!(hashes.len(), unique.len(), "存在重复的 SHA-256 hash");
    }
}

/// Task 3: 验证 package name、version、hash 能稳定序列化。
#[test]
fn package_lock_stable_serialization() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            let json = serde_json::to_string(pkg).expect("PackageLock 序列化失败");
            let deserialized: PackageLock =
                serde_json::from_str(&json).expect("PackageLock 反序列化失败");
            assert_eq!(pkg.name, deserialized.name);
            assert_eq!(pkg.version, deserialized.version);
            assert_eq!(pkg.sha256, deserialized.sha256);
        }
    }
}

/// Task 3: 验证版本不是范围约束。
#[test]
fn provider_descriptor_versions_are_exact() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            assert!(
                !pkg.version
                    .chars()
                    .next()
                    .is_some_and(|ch| matches!(ch, '>' | '<' | '~' | '!')),
                "PackageLock {} 使用了非精确版本约束: {}",
                pkg.name,
                pkg.version
            );
        }
    }
}

#[test]
fn provider_descriptor_has_cpu_profile_only() {
    let pd = make_paddleocr_provider_descriptor();
    assert_eq!(pd.profiles.len(), 1);
    assert_eq!(pd.profiles[0].backend, ComputeBackend::Cpu);
}

#[test]
fn map_health_ready() {
    let raw = serde_json::json!({
        "protocol_version": "0.3.0",
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        "actual_backend": "cpu",
        "device_name": "CPU",
        "uptime_seconds": 12.34
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Ready);
    assert_eq!(
        mapping.model_id.as_deref(),
        Some("PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec")
    );
    assert_eq!(mapping.model_revision.as_deref(), Some("ppocrv6-tiny"));
    assert!(mapping.backend.as_ref().unwrap().consistent);
}

/// Task 4: 验证 model_content_fingerprint 被正确存入 HealthMapping
#[test]
fn map_health_ready_preserves_content_fingerprint() {
    let raw = serde_json::json!({
        "protocol_version": "0.3.0",
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        "actual_backend": "cpu",
        "device_name": "CPU",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model_content_fingerprint.as_deref(),
        Some("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90")
    );
}

/// Task 4: 验证缺少 model_content_fingerprint 时 Ready 降级为 Failed
#[test]
fn map_health_ready_without_fingerprint_degrades_to_failed() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        "model_revision": "ppocrv6-tiny",
        // 缺少 model_content_fingerprint
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(
        mapping.model,
        ModelHealth::Failed,
        "Ready 缺 fingerprint 应降级为 Failed"
    );
}

/// Task 4: 验证 fingerprint 格式错误时 Ready 降级为 Failed
#[test]
fn map_health_ready_with_invalid_fingerprint_degrades_to_failed() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "short-not-hex",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model,
        ModelHealth::Failed,
        "Ready 无效 fingerprint 应降级为 Failed"
    );
}

/// Task 4: 验证缺少 model_id 时 Ready 降级为 Failed
#[test]
fn map_health_ready_without_model_id_degrades_to_failed() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        // 缺少 model_id
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model,
        ModelHealth::Failed,
        "Ready 缺 model_id 应降级为 Failed"
    );
}

/// Task 4: 验证缺少 model_revision 时 Ready 降级为 Failed
#[test]
fn map_health_ready_without_model_revision_degrades_to_failed() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        // 缺少 model_revision
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model,
        ModelHealth::Failed,
        "Ready 缺 model_revision 应降级为 Failed"
    );
}

/// Task 4: 验证 health 报告的 model_id 与 descriptor 一致
#[test]
fn map_health_model_id_matches_descriptor() {
    let adapter = PaddleocrAdapter::new();
    let expected_model_id = &adapter.descriptor().model_contract.model_id;

    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": expected_model_id,
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model_id.as_deref(),
        Some(expected_model_id.as_str())
    );
}

/// Task 4: 验证 health 报告的 model_revision 与 descriptor 一致
#[test]
fn map_health_model_revision_matches_descriptor() {
    let adapter = PaddleocrAdapter::new();
    let expected_revision = &adapter.descriptor().model_contract.revision;

    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
        "model_revision": expected_revision,
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(
        mapping.model_revision.as_deref(),
        Some(expected_revision.as_str())
    );
}

/// Task 4: 验证 health 报告不一致的 model_id 时仍映射（由上层验证）
///
/// map_paddleocr_health 是纯映射函数，不做身份验证——
/// 身份验证由 EngineManager.parse_and_verify_health 负责。
/// 这里验证映射函数忠实传递 health 报告的值，不做静默修正。
#[test]
fn map_health_mismatched_model_id_passes_through() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Ready",
        "model_id": "WRONG_MODEL_ID",
        "model_revision": "ppocrv6-tiny",
        "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
    });
    let mapping = map_paddleocr_health(&raw);
    // 映射函数忠实传递值，不做静默修正
    assert_eq!(mapping.model_id.as_deref(), Some("WRONG_MODEL_ID"));
}

#[test]
fn map_health_loading() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Loading",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Healthy);
    assert_eq!(mapping.model, ModelHealth::Loading);
}

#[test]
fn map_health_failed() {
    let raw = serde_json::json!({
        "engine_id": "paddleocr",
        "instance_id": "test-uuid",
        "token_fingerprint": "fp:abc123def4560a1b",
        "endpoint": "http://127.0.0.1:9100",
        "service_state": "healthy",
        "model_state": "Failed",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(mapping.model, ModelHealth::Failed);
}

#[test]
fn map_health_missing_identity_degrades_to_unreachable() {
    // 缺少 token_fingerprint 和 endpoint → 降级为 Unreachable
    let raw = serde_json::json!({
        "service_state": "healthy",
        "model_state": "Ready",
    });
    let mapping = map_paddleocr_health(&raw);
    assert_eq!(mapping.service, ServiceHealth::Unreachable);
}

/// Task 4: 验证 model_id 和 model_revision 与 descriptor 一致
#[test]
fn descriptor_model_identity_matches() {
    let adapter = PaddleocrAdapter::new();
    let descriptor = adapter.descriptor();
    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
    assert_eq!(descriptor.model_contract.model_id, expected_model_id);
    assert_eq!(descriptor.model_contract.revision, "ppocrv6-tiny");
}

/// Task 4: 验证 provider descriptor 的 model identity 也一致
#[test]
fn provider_descriptor_model_identity_matches() {
    let pd = make_paddleocr_provider_descriptor();
    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
    assert_eq!(pd.model_contract.model_id, expected_model_id);
    assert_eq!(pd.model_contract.revision, "ppocrv6-tiny");
}

/// Task 4: model_revision 不应使用 cache_files:N 格式
#[test]
fn model_revision_not_cache_files_format() {
    let adapter = PaddleocrAdapter::new();
    let revision = &adapter.descriptor().model_contract.revision;
    assert!(
        !revision.starts_with("cache_files:"),
        "model_revision 不应使用 cache_files:N 格式，实际: {}",
        revision
    );
    assert_eq!(revision, "ppocrv6-tiny");
}

#[test]
fn engine_config_from_ocr_config_defaults_to_tiny() {
    let cfg = PaddleOcrEngineConfig::from_ocr_config();
    assert_eq!(cfg.model, "tiny");
}

#[test]
fn ensure_ocr_server_script_creates_file() {
    let tmp = tempfile::tempdir().unwrap();
    let path = ensure_ocr_server_script_in(tmp.path()).unwrap();
    assert!(path.exists());
    let content = std::fs::read_to_string(&path).unwrap();
    assert!(content.contains("blink_ocr_server"));
    // rect 归一化 seam 必须与 server 脚本同目录释放（server import 它）
    let rect_module = tmp.path().join("ocr_rect.py");
    assert!(rect_module.exists(), "ocr_rect.py 必须随 server 脚本释放");
    assert!(
        std::fs::read_to_string(&rect_module)
            .unwrap()
            .contains("def extract_results")
    );
}

#[test]
fn ensure_ocr_server_script_idempotent() {
    let tmp = tempfile::tempdir().unwrap();
    let path1 = ensure_ocr_server_script_in(tmp.path()).unwrap();
    let path2 = ensure_ocr_server_script_in(tmp.path()).unwrap();
    assert_eq!(path1, path2);
}

#[test]
fn embedded_ocr_server_echoes_canonical_endpoint_authority() {
    assert!(
        BLINK_OCR_SERVER_PY.contains("_ENDPOINT = f\"{args.host}:{args.port}\""),
        "PaddleOCR health endpoint 必须使用 host:port canonical authority"
    );
    assert!(
        !BLINK_OCR_SERVER_PY.contains("_ENDPOINT = f\"http://{args.host}:{args.port}\""),
        "身份 endpoint 不得混用 HTTP base URL"
    );
}

// ── 完整依赖锁测试 ──────────────────────────────────────────────────────

/// 验证从 locked-requirements.txt 解析的包列表包含全部传递依赖（>7 个直接包）。
#[test]
fn locked_packages_includes_transitive_deps() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        // 之前硬编码只有 7 个直接包；完整锁应有 70 个（含传递依赖）
        assert!(
            plan.packages.len() > 7,
            "locked-requirements.txt 应解析出 >7 个包（含传递依赖），实际: {}",
            plan.packages.len()
        );
        tracing::info!(
            "locked-requirements.txt 解析出 {} 个包",
            plan.packages.len()
        );
    }
}

/// 验证所有包的 all_hashes 非空（多平台 wheel hash）。
#[test]
fn locked_packages_all_hashes_non_empty() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        for pkg in &plan.packages {
            assert!(
                !pkg.all_hashes.is_empty(),
                "PackageLock {} 的 all_hashes 为空，--require-hashes 需要至少一个 hash",
                pkg.name
            );
            // 所有 hash 格式验证
            for h in &pkg.all_hashes {
                assert_eq!(
                    h.len(),
                    64,
                    "PackageLock {} 的 all_hashes 中有长度不为 64 的 hash",
                    pkg.name
                );
                assert!(
                    h.bytes().all(|b| b.is_ascii_hexdigit()),
                    "PackageLock {} 的 all_hashes 中有非 hex 字符",
                    pkg.name
                );
            }
        }
    }
}

/// 验证安装计划包含 --no-deps（禁止传递依赖自动解析）。
#[test]
fn provider_descriptor_has_no_deps() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        assert!(
            plan.extra_pip_args
                .iter()
                .any(|arg| matches!(arg, PipExtraArg::NoDeps)),
            "安装计划必须包含 --no-deps，禁止传递依赖自动解析"
        );
    }
}

/// 验证 locked-requirements.txt 中包含关键直接依赖。
#[test]
fn locked_packages_contains_key_deps() {
    let pd = make_paddleocr_provider_descriptor();
    if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
        let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
        // 直接依赖
        assert!(names.contains(&"paddlepaddle"), "缺少 paddlepaddle");
        assert!(names.contains(&"paddleocr"), "缺少 paddleocr");
        assert!(names.contains(&"fastapi"), "缺少 fastapi");
        assert!(names.contains(&"uvicorn"), "缺少 uvicorn");
        assert!(names.contains(&"pillow"), "缺少 pillow");
        assert!(names.contains(&"numpy"), "缺少 numpy");
        assert!(names.contains(&"pyarrow"), "缺少 pyarrow");
        // 关键传递依赖
        assert!(names.contains(&"aiohttp"), "缺少传递依赖 aiohttp");
        assert!(names.contains(&"starlette"), "缺少传递依赖 starlette");
    }
}

/// 验证 parse_locked_requirements 解析格式正确。
#[test]
fn parse_locked_requirements_correctness() {
    let sample = "# comment\naiohappyeyeballs==2.7.1 \\\n    --hash=sha256:065665c041c42a5938ed220bdcd7230f22527fbec085e1853d2402c8a3615d9d \\\n    --hash=sha256:9243213661e29250eb41368e5daa826fc017156c3b8a11440826b2e3ed376472\nfastapi==0.115.6 \\\n    --hash=sha256:e9240b29e36fa8f4bb7290316988e90c381e5092e0cbe84e7818cc3713bcf305\n";
    let packages = parse_locked_requirements(sample);
    assert_eq!(packages.len(), 2);
    assert_eq!(packages[0].name, "aiohappyeyeballs");
    assert_eq!(packages[0].version, "2.7.1");
    assert_eq!(packages[0].all_hashes.len(), 2);
    assert_eq!(
        packages[0].sha256.as_deref(),
        Some("065665c041c42a5938ed220bdcd7230f22527fbec085e1853d2402c8a3615d9d")
    );
    assert_eq!(packages[1].name, "fastapi");
    assert_eq!(packages[1].version, "0.115.6");
    assert_eq!(packages[1].all_hashes.len(), 1);
}

/// 验证嵌入的 LOCKED_REQUIREMENTS_TXT 不为空。
#[test]
fn embedded_locked_requirements_not_empty() {
    assert!(!LOCKED_REQUIREMENTS_TXT.is_empty());
    assert!(LOCKED_REQUIREMENTS_TXT.contains("paddlepaddle"));
    assert!(LOCKED_REQUIREMENTS_TXT.contains("paddleocr"));
}
