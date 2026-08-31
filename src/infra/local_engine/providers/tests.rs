use super::*;
use crate::infra::local_engine::deployment::TransactionJournal;
use crate::infra::local_engine::runtime::ArtifactId;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

// ── 测试用 FakeProvider ────────────────────────────────────────────────

struct FakeProvider {
    compatible: bool,
    /// true = CPU 候选（Always/CpuFeature）兼容、GPU 候选不兼容
    /// （表达 per-backend 差异；false = 所有候选统一返回 compatible）。
    cpu_only: bool,
    prepare_ok: bool,
    self_test_ok: bool,
    /// 写入不可信的 self_test 标记，模拟切换后 manifest 验证失败。
    post_switch_verification_fails: bool,
}

impl FakeProvider {
    fn new(compatible: bool, prepare_ok: bool, self_test_ok: bool) -> Self {
        Self {
            compatible,
            cpu_only: false,
            prepare_ok,
            self_test_ok,
            post_switch_verification_fails: false,
        }
    }

    /// CPU 兼容、GPU 不兼容（profile 回退测试用）。
    fn cpu_only(mut self) -> Self {
        self.cpu_only = true;
        self
    }

    fn with_post_switch_failure(mut self) -> Self {
        self.post_switch_verification_fails = true;
        self
    }
}

#[async_trait]
impl RuntimeProvider for FakeProvider {
    fn kind(&self) -> RuntimePlan {
        RuntimePlan::PythonVenv
    }

    fn check_compatibility(
        &self,
        compatibility: &CompatibilityCheck,
    ) -> Result<bool, RuntimeError> {
        if self.cpu_only {
            return Ok(matches!(
                compatibility,
                CompatibilityCheck::Always | CompatibilityCheck::RequiresCpuFeature { .. }
            ));
        }
        Ok(self.compatible)
    }

    async fn prepare_environment(
        &self,
        staging_dir: &std::path::Path,
        _plan: &InstallPlan,
        _resolved_profile: &ResolvedProfile,
        _cancel_token: Option<&tokio_util::sync::CancellationToken>,
        _sink: Option<&dyn InstallSink>,
    ) -> Result<PrepareResult, RuntimeError> {
        if !self.prepare_ok {
            return Err(RuntimeError::InstallFailed {
                message: "fake prepare failure".to_string(),
            });
        }
        std::fs::create_dir_all(staging_dir.join("venv")).unwrap();
        Ok(PrepareResult {
            artifact: runtime::ArtifactIdentity {
                runtime_kind: RuntimePlan::PythonVenv,
                artifact_id: fake_descriptor().profiles[0].artifact_id.clone(),
                sha256: "a".repeat(64),
            },
        })
    }

    async fn self_test(
        &self,
        _deployment_dir: &std::path::Path,
        _plan: &InstallPlan,
        _cancel_token: Option<&tokio_util::sync::CancellationToken>,
        _sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        if self.self_test_ok {
            Ok(())
        } else {
            Err(RuntimeError::SelfTestFailed {
                message: "fake self-test failure".to_string(),
            })
        }
    }

    fn build_manifest_extension(
        &self,
        _deployment_dir: &std::path::Path,
        _plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        Ok(ManifestExtension::PythonVenv(runtime::PythonManifestExt {
            python_version: "3.12.8".to_string(),
            python_artifact_id: fake_descriptor().profiles[0].artifact_id.clone(),
            packages: Vec::new(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            self_test_passed: !self.post_switch_verification_fails,
        }))
    }
}

fn fake_descriptor() -> ProviderDescriptor {
    let artifact = ArtifactId::new("python-3.12.8-fake").unwrap();
    ProviderDescriptor {
        engine_id: EngineId::new("fake-engine").unwrap(),
        runtime_kind: RuntimePlan::PythonVenv,
        display_name: "Fake".to_string(),
        // GPU 优先（priority 0），CPU 兜底——与真实 descriptor 语义一致，
        // 使 auto 回退路径可测。
        profiles: vec![
            ProfileCandidate {
                profile_id: "cuda-sm86".to_string(),
                backend: ComputeBackend::Cuda,
                artifact_id: artifact.clone(),
                compatibility: CompatibilityCheck::RequiresCuda { min_version: None },
            },
            ProfileCandidate {
                profile_id: "cpu-x64".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: artifact,
                compatibility: CompatibilityCheck::Always,
            },
        ],
        model_contract: ModelContract {
            model_id: "fake-model".to_string(),
            revision: "v1".to_string(),
            checksum_source: runtime::ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: ArtifactId::new("python-3.12.8-fake").unwrap(),
            packages: Vec::new(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            extra_pip_args: Vec::new(),
            self_test_script: "pass".to_string(),
        }),
    }
}

fn cleanup_engine(desc: &ProviderDescriptor) {
    let _ = std::fs::remove_dir_all(runtime::engine_root(&desc.engine_id));
}

fn unique_engine(tag: &str) -> ProviderDescriptor {
    let mut desc = fake_descriptor();
    desc.engine_id = EngineId::new(format!("fake-{tag}")).unwrap();
    desc
}

// ── profile 解析 ───────────────────────────────────────────────────────

#[tokio::test]
async fn resolve_auto_falls_back_to_cpu() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true).cpu_only(); // GPU 不兼容
    let tx = InstallTransaction::new(&desc, &provider);
    let (profile, fallbacks) = tx.resolve_profile(ComputePreference::Auto).unwrap();
    assert_eq!(profile.backend, ComputeBackend::Cpu);
    assert_eq!(fallbacks.len(), 1);
    assert_eq!(fallbacks[0].rejected_profile, "cuda-sm86");
}

#[tokio::test]
async fn resolve_explicit_cpu_no_fallback() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true).cpu_only();
    let tx = InstallTransaction::new(&desc, &provider);
    let (profile, _) = tx.resolve_profile(ComputePreference::Cpu).unwrap();
    assert_eq!(profile.backend, ComputeBackend::Cpu);
}

#[tokio::test]
async fn resolve_explicit_cuda_no_fallback_on_incompatible() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true).cpu_only();
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx.resolve_profile(ComputePreference::Cuda).unwrap_err();
    assert!(matches!(err, RuntimeError::ExplicitBackendFailed { .. }));
}

#[tokio::test]
async fn resolve_gpu_auto_skips_cpu() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &provider);
    let (profile, fallbacks) = tx.resolve_profile(ComputePreference::GpuAuto).unwrap();
    assert_eq!(profile.backend, ComputeBackend::Cuda);
    assert!(fallbacks.is_empty());
}

#[tokio::test]
async fn resolve_gpu_auto_fails_when_no_gpu_compatible() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true).cpu_only();
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx.resolve_profile(ComputePreference::GpuAuto).unwrap_err();
    assert!(matches!(err, RuntimeError::ProfileResolutionFailed { .. }));
}

#[tokio::test]
async fn resolve_explicit_backend_not_in_descriptor() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx.resolve_profile(ComputePreference::Vulkan).unwrap_err();
    assert!(matches!(err, RuntimeError::ExplicitBackendFailed { .. }));
}

#[tokio::test]
async fn resolve_auto_all_incompatible_fails() {
    let desc = fake_descriptor();
    let provider = FakeProvider::new(false, true, true);
    // auto 会回退到 CPU（Always 兼容不受 compatible flag 影响？）
    // —— FakeProvider::check_compatibility 对所有候选返回同一 flag，
    // CPU 候选也不兼容 → 全部失败
    let tx = InstallTransaction::new(&desc, &provider);
    // CPU 是 Always 但 fake provider 仍返回 false
    let result = tx.resolve_profile(ComputePreference::Auto);
    // Auto 在第一个兼容即返回；CPU 也 false → 报错
    assert!(result.is_err());
}

// ── 事务：成功 / 失败 / 回滚 ───────────────────────────────────────────

#[tokio::test]
async fn install_success_switches_pointer_and_deletes_old_slot() {
    let desc = unique_engine("ok");
    let provider = FakeProvider::new(true, true, true);

    // 第一次安装：slot-a active
    let tx = InstallTransaction::new(&desc, &provider);
    let r1 = tx
        .execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap();
    let (p1, m1) = DeploymentStore::read_active(&desc.engine_id)
        .unwrap()
        .unwrap();
    assert_eq!(p1.slot, "slot-a");
    assert_eq!(m1.install_id, r1.install_id);

    // 第二次安装（更新）：slot-b active，slot-a 删除
    let tx = InstallTransaction::new(&desc, &provider);
    let r2 = tx
        .execute("op-2", ComputePreference::Auto, None, None)
        .await
        .unwrap();
    let (p2, m2) = DeploymentStore::read_active(&desc.engine_id)
        .unwrap()
        .unwrap();
    assert_eq!(p2.slot, "slot-b");
    assert_eq!(m2.install_id, r2.install_id);
    // 稳定状态只保留 active slot
    assert!(!DeploymentSlot::A.dir(&desc.engine_id).exists());
    // journal 已清除
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );
    // staging 已清空
    assert!(!runtime::operation_staging_dir(&desc.engine_id, "op-1").exists());
    assert!(!runtime::operation_staging_dir(&desc.engine_id, "op-2").exists());

    cleanup_engine(&desc);
}

#[tokio::test]
async fn update_failure_preserves_active_and_clears_journal() {
    let desc = unique_engine("fail");
    let ok_provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &ok_provider);
    let r1 = tx
        .execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap();

    // 更新失败（prepare 失败）
    let fail_provider = FakeProvider::new(true, false, true);
    let tx = InstallTransaction::new(&desc, &fail_provider);
    let err = tx
        .execute("op-2", ComputePreference::Auto, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::InstallFailed { .. }));

    // old 仍是 active，journal/staging 清除
    let (p, m) = DeploymentStore::read_active(&desc.engine_id)
        .unwrap()
        .unwrap();
    assert_eq!(m.install_id, r1.install_id);
    assert_eq!(p.slot, "slot-a");
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );
    assert!(!runtime::staging_dir(&desc.engine_id).join("op-2").exists());

    cleanup_engine(&desc);
}

#[tokio::test]
async fn post_switch_verification_failure_rolls_back_to_previous() {
    let desc = unique_engine("rollback");
    let ok_provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &ok_provider);
    let r1 = tx
        .execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap();

    // 切换后验证失败 → 自动回滚
    let bad = Arc::new(FakeProvider::new(true, true, true).with_post_switch_failure());
    let tx = InstallTransaction::new(&desc, bad.as_ref());
    let err = tx
        .execute("op-2", ComputePreference::Auto, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::SelfTestFailed { .. }));

    // active 回到 old，candidate slot 删除，journal 清除
    let (p, m) = DeploymentStore::read_active(&desc.engine_id)
        .unwrap()
        .unwrap();
    assert_eq!(m.install_id, r1.install_id);
    assert_eq!(p.slot, "slot-a");
    assert!(!DeploymentSlot::B.dir(&desc.engine_id).exists());
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );

    cleanup_engine(&desc);
}

#[tokio::test]
async fn self_test_failure_in_staging_cleans_journal() {
    let desc = unique_engine("stfail");
    let provider = FakeProvider::new(true, true, false);
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx
        .execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::SelfTestFailed { .. }));

    assert!(
        DeploymentStore::read_pointer(&desc.engine_id)
            .unwrap()
            .is_none()
    );
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );

    cleanup_engine(&desc);
}

#[tokio::test]
async fn cancel_before_prepare_cleans_staging_and_journal() {
    let desc = unique_engine("cancel1");
    let provider = FakeProvider::new(true, true, true);
    let token = tokio_util::sync::CancellationToken::new();
    token.cancel();
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx
        .execute("op-1", ComputePreference::Auto, Some(&token), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::OperationCancelled { .. }));
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );
    assert!(!runtime::staging_dir(&desc.engine_id).join("op-1").exists());

    cleanup_engine(&desc);
}

#[tokio::test]
async fn cancel_token_check_at_each_stage() {
    // prepare 成功后、self-test 前取消——wrapper 在 prepare 返回前设置取消标记
    let desc = unique_engine("cancel2");
    let provider = FakeProvider::new(true, true, true);
    let token = tokio_util::sync::CancellationToken::new();
    let provider = CancelAfterPrepareProvider {
        inner: provider,
        token: token.clone(),
    };
    let tx = InstallTransaction::new(&desc, &provider);
    let err = tx
        .execute("op-1", ComputePreference::Auto, Some(&token), None)
        .await
        .unwrap_err();
    assert!(matches!(err, RuntimeError::OperationCancelled { .. }));
    assert!(
        DeploymentStore::read_journal(&desc.engine_id)
            .unwrap()
            .is_none()
    );

    cleanup_engine(&desc);
}

/// prepare 完成后设置取消标记的 wrapper provider。
struct CancelAfterPrepareProvider<P> {
    inner: P,
    token: tokio_util::sync::CancellationToken,
}

#[async_trait]
impl<P: RuntimeProvider + Sync> RuntimeProvider for CancelAfterPrepareProvider<P> {
    fn kind(&self) -> RuntimePlan {
        self.inner.kind()
    }
    fn check_compatibility(&self, c: &CompatibilityCheck) -> Result<bool, RuntimeError> {
        self.inner.check_compatibility(c)
    }
    async fn prepare_environment(
        &self,
        staging_dir: &std::path::Path,
        plan: &InstallPlan,
        profile: &ResolvedProfile,
        _cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<PrepareResult, RuntimeError> {
        let r = self
            .inner
            .prepare_environment(staging_dir, plan, profile, None, sink)
            .await?;
        // prepare 成功后立刻取消——下一阶段（self-test 前）检查点会命中
        self.token.cancel();
        Ok(r)
    }
    async fn self_test(
        &self,
        d: &std::path::Path,
        plan: &InstallPlan,
        ct: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        self.inner.self_test(d, plan, ct, sink).await
    }
    fn build_manifest_extension(
        &self,
        d: &std::path::Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        self.inner.build_manifest_extension(d, plan)
    }
}

// ── 共享 artifact 引用保护 ─────────────────────────────────────────────

#[tokio::test]
async fn cleanup_shared_artifact_with_active_reference_rejected() {
    let desc = unique_engine("shared");
    let provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &provider);
    tx.execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap();

    // active 部署引用的 artifact 拒绝清理
    let artifact_id = fake_descriptor().profiles[0].artifact_id.clone();
    let err = execute_cleanup(&CleanupScope::ProviderSharedArtifact {
        runtime_kind: RuntimePlan::PythonVenv,
        artifact_id: artifact_id.clone(),
    })
    .unwrap_err();
    assert!(matches!(err, RuntimeError::ArtifactStillReferenced { .. }));

    cleanup_engine(&desc);
}

#[tokio::test]
async fn cleanup_shared_artifact_without_reference_succeeds() {
    let artifact_id = ArtifactId::new("shared-unref-artifact-0001").unwrap();
    let dir = runtime::shared_artifact_dir(RuntimePlan::PythonVenv, &artifact_id);
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("payload.bin"), b"x").unwrap();

    execute_cleanup(&CleanupScope::ProviderSharedArtifact {
        runtime_kind: RuntimePlan::PythonVenv,
        artifact_id: artifact_id.clone(),
    })
    .unwrap();
    assert!(!dir.exists());
}

// ── slot 清理 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn cleanup_rejects_active_slot() {
    let desc = unique_engine("cleanup-active");
    let provider = FakeProvider::new(true, true, true);
    let tx = InstallTransaction::new(&desc, &provider);
    tx.execute("op-1", ComputePreference::Auto, None, None)
        .await
        .unwrap();

    let err = execute_cleanup(&CleanupScope::EngineDeploymentSlot {
        engine_id: desc.engine_id.clone(),
        slot: "slot-a".to_string(),
    })
    .unwrap_err();
    assert!(matches!(err, RuntimeError::CleanupFailed { .. }));

    cleanup_engine(&desc);
}

#[tokio::test]
async fn ensure_path_within_rejects_traversal_in_cleanup() {
    let desc = unique_engine("traversal");
    let engine_root = runtime::engine_root(&desc.engine_id);
    std::fs::create_dir_all(&engine_root).unwrap();
    let escape = engine_root.join("..").join("..").join("etc");
    assert!(runtime::ensure_path_within(&engine_root, &escape).is_err());
    let _ = std::fs::remove_dir_all(&engine_root);
}

// ── 错误分类 ───────────────────────────────────────────────────────────

#[test]
fn from_io_disk_space_detects_disk_full() {
    #[cfg(windows)]
    let disk_full = std::io::Error::from_raw_os_error(112);
    #[cfg(not(windows))]
    let disk_full = std::io::Error::from_raw_os_error(28);
    assert!(matches!(
        RuntimeError::from_io_disk_space(disk_full),
        RuntimeError::InsufficientDiskSpace { .. }
    ));

    let other = std::io::Error::from_raw_os_error(5);
    assert!(matches!(
        RuntimeError::from_io_disk_space(other),
        RuntimeError::Io(_)
    ));
}

// ── InstallSink 阶段序列 ───────────────────────────────────────────────

struct RecordingSink {
    stages: Mutex<Vec<String>>,
    logs: Mutex<Vec<String>>,
}

impl RecordingSink {
    fn new() -> Self {
        Self {
            stages: Mutex::new(Vec::new()),
            logs: Mutex::new(Vec::new()),
        }
    }
}

impl InstallSink for RecordingSink {
    fn on_stage(&self, stage: &str) {
        self.stages.lock().unwrap().push(stage.to_string());
    }
    fn on_log(&self, level: &str, text: &str) {
        self.logs.lock().unwrap().push(format!("[{level}] {text}"));
    }
}

#[tokio::test]
async fn install_sink_stage_sequence_is_correct() {
    let desc = unique_engine("sink");
    let provider = FakeProvider::new(true, true, true);
    let sink = RecordingSink::new();
    let tx = InstallTransaction::new(&desc, &provider);
    tx.execute("op-1", ComputePreference::Auto, None, Some(&sink))
        .await
        .unwrap();

    let stages = sink.stages.lock().unwrap().clone();
    assert_eq!(
        stages,
        vec![
            "preparing",
            "downloading",
            "verifying",
            "promoting",
            "switching",
            "validating",
            "completed"
        ]
    );

    cleanup_engine(&desc);
}

#[tokio::test]
async fn install_sink_stages_on_prepare_failure() {
    let desc = unique_engine("sinkfail");
    let provider = FakeProvider::new(true, false, true);
    let sink = RecordingSink::new();
    let tx = InstallTransaction::new(&desc, &provider);
    let _ = tx
        .execute("op-1", ComputePreference::Auto, None, Some(&sink))
        .await;
    let stages = sink.stages.lock().unwrap().clone();
    assert_eq!(stages, vec!["preparing", "downloading", "failed"]);

    cleanup_engine(&desc);
}

#[tokio::test]
async fn install_sink_logs_are_emitted_during_install() {
    let desc = unique_engine("sinklog");
    let provider = FakeProvider::new(true, true, true);
    let sink = RecordingSink::new();
    let tx = InstallTransaction::new(&desc, &provider);
    tx.execute("op-1", ComputePreference::Auto, None, Some(&sink))
        .await
        .unwrap();
    assert!(!sink.logs.lock().unwrap().is_empty());

    cleanup_engine(&desc);
}

#[tokio::test]
async fn noop_install_sink_does_not_break_install() {
    let desc = unique_engine("noop");
    let provider = FakeProvider::new(true, true, true);
    let sink = NoopInstallSink;
    let tx = InstallTransaction::new(&desc, &provider);
    assert!(
        tx.execute("op-1", ComputePreference::Auto, None, Some(&sink))
            .await
            .is_ok()
    );

    cleanup_engine(&desc);
}

// ── 并发：不同引擎可并行安装 ───────────────────────────────────────────

#[tokio::test]
async fn different_engines_install_concurrently() {
    let desc_a = unique_engine("conc-a");
    let desc_b = unique_engine("conc-b");
    let barrier = Arc::new(tokio::sync::Barrier::new(2));

    // provider 在 prepare 内等待 barrier——两引擎同时进入 prepare 才能完成
    struct BarrierProvider {
        _inner: FakeProvider,
        barrier: Arc<tokio::sync::Barrier>,
        entered: AtomicUsize,
    }

    #[async_trait]
    impl RuntimeProvider for BarrierProvider {
        fn kind(&self) -> RuntimePlan {
            RuntimePlan::PythonVenv
        }
        fn check_compatibility(&self, _c: &CompatibilityCheck) -> Result<bool, RuntimeError> {
            Ok(true)
        }
        async fn prepare_environment(
            &self,
            staging_dir: &std::path::Path,
            _plan: &InstallPlan,
            _profile: &ResolvedProfile,
            _ct: Option<&tokio_util::sync::CancellationToken>,
            _sink: Option<&dyn InstallSink>,
        ) -> Result<PrepareResult, RuntimeError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            self.barrier.wait().await; // 双方都进入才放行
            std::fs::create_dir_all(staging_dir.join("venv")).unwrap();
            Ok(PrepareResult {
                artifact: runtime::ArtifactIdentity {
                    runtime_kind: RuntimePlan::PythonVenv,
                    artifact_id: fake_descriptor().profiles[0].artifact_id.clone(),
                    sha256: "b".repeat(64),
                },
            })
        }
        async fn self_test(
            &self,
            _d: &std::path::Path,
            _plan: &InstallPlan,
            _ct: Option<&tokio_util::sync::CancellationToken>,
            _sink: Option<&dyn InstallSink>,
        ) -> Result<(), RuntimeError> {
            Ok(())
        }
        fn build_manifest_extension(
            &self,
            _d: &std::path::Path,
            _plan: &InstallPlan,
        ) -> Result<ManifestExtension, RuntimeError> {
            Ok(ManifestExtension::PythonVenv(runtime::PythonManifestExt {
                python_version: "3.12.8".to_string(),
                python_artifact_id: fake_descriptor().profiles[0].artifact_id.clone(),
                packages: Vec::new(),
                uv_version: "0.6.10".to_string(),
                index_url: None,
                self_test_passed: true,
            }))
        }
    }

    let pa = BarrierProvider {
        _inner: FakeProvider::new(true, true, true),
        barrier: barrier.clone(),
        entered: AtomicUsize::new(0),
    };
    let pb = BarrierProvider {
        _inner: FakeProvider::new(true, true, true),
        barrier,
        entered: AtomicUsize::new(0),
    };

    let tx_a = InstallTransaction::new(&desc_a, &pa);
    let tx_b = InstallTransaction::new(&desc_b, &pb);
    let (ra, rb) = tokio::join!(
        tx_a.execute("op-conc-a", ComputePreference::Auto, None, None),
        tx_b.execute("op-conc-b", ComputePreference::Auto, None, None)
    );
    assert!(ra.is_ok());
    assert!(rb.is_ok());

    cleanup_engine(&desc_a);
    cleanup_engine(&desc_b);
}

// ── journal 阶段语义（直接构造 journal 验证 recover 兼容）────────────

#[test]
fn transaction_journal_serde_roundtrip() {
    let j = TransactionJournal {
        schema_version: crate::infra::local_engine::deployment::TRANSACTION_JOURNAL_SCHEMA_VERSION,
        engine_id: "fake-x".to_string(),
        operation_id: "op-1".to_string(),
        candidate_slot: "slot-b".to_string(),
        candidate_install_id: "dep-1".to_string(),
        previous: Some(crate::infra::local_engine::deployment::PreviousDeployment {
            install_id: "dep-0".to_string(),
            slot: "slot-a".to_string(),
        }),
        phase: TransactionPhase::Switched,
        started_at_ms: 0,
    };
    let json = serde_json::to_string(&j).unwrap();
    let back: TransactionJournal = serde_json::from_str(&json).unwrap();
    assert_eq!(back, j);
}
