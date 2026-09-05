use super::*;
use crate::domain::local_engine::*;
use crate::infra::local_engine::deployment::DeploymentPointer;
use crate::infra::local_engine::runtime::{ArtifactId, ComputePreference, RuntimePlan};
use std::collections::HashMap;
use std::sync::Arc;

// ── 基础辅助 ──────────────────────────────────────────────────────────────

fn make_fake_adapter(id: &str, self_test_passes: bool) -> Arc<dyn LocalEngineAdapter> {
    make_fake_adapter_with_options(id, self_test_passes, true)
}

/// 可选关闭 managed model storage 的 fake adapter（start 冻结段测试用：
/// 跳过 model_storage manifest 读取，直接以 descriptor 契约为冻结模型）。
fn make_fake_adapter_with_options(
    id: &str,
    self_test_passes: bool,
    managed_model_storage: bool,
) -> Arc<dyn LocalEngineAdapter> {
    struct FakeAdapter {
        descriptor: EngineDefinition,
        self_test_passes: bool,
        managed_model_storage: bool,
    }

    impl FakeAdapter {
        fn new(id: &str, self_test_passes: bool, managed_model_storage: bool) -> Self {
            let artifact = ArtifactId::new("fake-artifact").unwrap();
            Self {
                descriptor: EngineDefinition {
                    engine_id: EngineId::new(id).unwrap(),
                    display: EngineDisplay {
                        name: format!("Fake {id}"),
                        description: "test adapter".to_string(),
                        icon: "cpu".to_string(),
                        version: "0.1.0".to_string(),
                    },
                    capability_kind: CapabilityKind::Stt,
                    runtime_kind: RuntimePlan::PythonVenv,
                    service_transport: ServiceTransport::Http,
                    install_plan: InstallPlanRef {
                        runtime_kind: RuntimePlan::PythonVenv,
                        artifact_ids: vec![artifact.clone()],
                        compute_candidates: vec![ComputeCandidate {
                            preference: ComputePreference::Cpu,
                            profile_id: "cpu-x64".to_string(),
                            artifact_id: artifact,
                        }],
                        schema_version: 1,
                    },
                    model_contract: crate::infra::local_engine::runtime::ModelContract {
                        model_id: "fake-model".to_string(),
                        revision: "v1".to_string(),
                        checksum_source:
                            crate::infra::local_engine::runtime::ChecksumSource::Unverified,
                    },
                    lifecycle: LifecyclePolicy::Manual,
                    timeouts: EngineTimeouts::default(),
                    resource_budget: ResourceBudget::default(),
                },
                self_test_passes,
                managed_model_storage,
            }
        }
    }

    impl LocalEngineAdapter for FakeAdapter {
        fn descriptor(&self) -> &EngineDefinition {
            &self.descriptor
        }

        fn prepare_launch(
            &self,
            ctx: &LaunchContext,
            _config: &AdapterConfig,
        ) -> Result<ResolvedLaunch, LocalEngineError> {
            if !self.descriptor.is_profile_allowed(&ctx.resolved_profile) {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Start,
                    "不支持的 profile",
                    format!(
                        "profile '{}' 不在 descriptor 声明范围内",
                        ctx.resolved_profile.profile_id
                    ),
                ));
            }
            Ok(ResolvedLaunch {
                profile: ctx.resolved_profile.clone(),
                launch: LaunchDescriptor {
                    executable: std::path::PathBuf::from("fake-executable"),
                    args: vec!["--serve".to_string()],
                    current_dir: None,
                    env: HashMap::new(),
                    label: self.descriptor.engine_id.to_string(),
                },
            })
        }

        fn map_health(&self, _raw: &serde_json::Value) -> HealthMapping {
            HealthMapping {
                service: ServiceHealth::Healthy,
                model: ModelHealth::Ready,
                environment: None,
                backend: None,
                model_id: None,
                model_revision: None,
                model_content_fingerprint: None,
            }
        }

        fn uses_managed_model_storage(&self) -> bool {
            self.managed_model_storage
        }

        fn self_test(&self) -> AdapterSelfTest {
            if self.self_test_passes {
                AdapterSelfTest::passed()
            } else {
                AdapterSelfTest::failed("fake self-test failure")
            }
        }

        fn diagnostics(&self) -> EngineDiagnostic {
            EngineDiagnostic {
                entries: vec![DiagnosticEntry {
                    key: "version".to_string(),
                    value: "0.1.0".to_string(),
                    label: "info".to_string(),
                }],
            }
        }
    }

    Arc::new(FakeAdapter::new(
        id,
        self_test_passes,
        managed_model_storage,
    ))
}

/// 构建测试用 manager（1 个 fake adapter + fake 模型目录 + fake worker）。
fn make_service(adapter_id: &str) -> Arc<EngineManager> {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        adapter_id, true,
    )]));
    EngineManager::new(registry, Arc::new(NoopEventPort))
}

fn unique_tag(prefix: &str) -> String {
    format!(
        "{prefix}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    )
}

/// 测试用模型目录（两个模型候选）。
fn make_model_registry(
    engine_id: &EngineId,
    m_a: &str,
    m_b: &str,
) -> super::super::model_installer::ModelRegistry {
    use super::super::model_installer::ModelRegistry;
    let mk = |model_id: &str| EngineModelDescriptor {
        engine_id: engine_id.clone(),
        model_id: model_id.to_string(),
        display_name: model_id.to_string(),
        description: "test".to_string(),
        revision: "v1".to_string(),
        checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
        estimated_size_mb: Some(1),
        compatibility_schema: 1,
        stt_capabilities: crate::domain::local_engine::SttModelCapabilities::default(),
        business: None,
    };
    ModelRegistry::new_with_models(vec![mk(m_a), mk(m_b)])
}

/// barrier 门控 installer——两个任务都进入下载后才放行（无 sleep 猜时序）。
struct BarrierInstaller {
    barrier: Arc<tokio::sync::Barrier>,
}

#[async_trait::async_trait]
impl super::super::model_installer::ModelInstallWorker for BarrierInstaller {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        _model_id: &str,
        _revision: &str,
        staging_payload_dir: &std::path::Path,
        _cancel_token: CancellationToken,
        _sink: Option<Arc<dyn super::super::model_installer::InstallSink>>,
    ) -> Result<
        super::super::model_installer::ModelDownloadOutcome,
        super::super::model_installer::ModelDownloadError,
    > {
        self.barrier.wait().await;
        std::fs::create_dir_all(staging_payload_dir).unwrap();
        std::fs::write(staging_payload_dir.join("model.bin"), b"payload").unwrap();
        Ok(super::super::model_installer::ModelDownloadOutcome {
            source: "fake".to_string(),
            checksum_source: super::super::model_installer::ModelDownloadChecksumSource::Sha256(
                "ab".repeat(32),
            ),
        })
    }
}

/// Semaphore 门控 installer——测试放行一个 permit 控制"下载完成"时机。
///
/// 用 Semaphore 而非 Notify：permit 会累积，release 早于 waiter 注册
/// 也不会丢信号（Notify::notify_waiters 只唤醒已注册的 waiter）。
/// `started` 提供确定性的"下载已进入"信号：测试在 spawn 前注册
/// `notified()` future，下载入口处 notify——不用轮询猜时序。
struct GatedInstaller {
    gate: Arc<tokio::sync::Semaphore>,
    started: Arc<tokio::sync::Notify>,
    fail: std::sync::atomic::AtomicBool,
}

impl GatedInstaller {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            gate: Arc::new(tokio::sync::Semaphore::new(0)),
            started: Arc::new(tokio::sync::Notify::new()),
            fail: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// 返回"下载已进入"通知的 future——必须在 spawn 安装任务前创建。
    fn started_signal(&self) -> tokio::sync::futures::Notified<'_> {
        self.started.notified()
    }

    /// 放行一次下载等待。
    fn release(&self) {
        self.gate.add_permits(1);
    }
}

#[async_trait::async_trait]
impl super::super::model_installer::ModelInstallWorker for GatedInstaller {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        _model_id: &str,
        _revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        _sink: Option<Arc<dyn super::super::model_installer::InstallSink>>,
    ) -> Result<
        super::super::model_installer::ModelDownloadOutcome,
        super::super::model_installer::ModelDownloadError,
    > {
        // 确定性信号：下载已进入（claim 必然已登记——claim 先于下载）
        self.started.notify_waiters();
        let _permit = tokio::select! {
            p = self.gate.acquire() => p,
            _ = cancel_token.cancelled() => {
                return Err(super::super::model_installer::ModelDownloadError::Cancelled);
            }
        };
        if self.fail.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(super::super::model_installer::ModelDownloadError::Failed {
                message: "gated failure".to_string(),
            });
        }
        std::fs::create_dir_all(staging_payload_dir).unwrap();
        std::fs::write(staging_payload_dir.join("model.bin"), b"payload").unwrap();
        Ok(super::super::model_installer::ModelDownloadOutcome {
            source: "fake".to_string(),
            checksum_source: super::super::model_installer::ModelDownloadChecksumSource::Sha256(
                "ab".repeat(32),
            ),
        })
    }
}

/// 注入 launch snapshot（模拟运行中实例——测试 active 语义）。
async fn inject_launch(entry: &Arc<EngineEntry>, model_id: &str, instance_id: &str) {
    let endpoint = crate::infra::local_engine::port::Endpoint::new(8100);
    let mut l = entry.launch.lock().await;
    *l = Some(LaunchSnapshot {
        identity: ServiceIdentityInput {
            engine_id: entry.adapter.descriptor().engine_id.to_string(),
            instance_id: instance_id.to_string(),
            token: format!("tok-{instance_id}"),
            endpoint,
        },
        profile: ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("fake-artifact").unwrap(),
            priority: 0,
        },
        deployment_install_id: "dep-test".to_string(),
        model: Some(FrozenModelIdentity {
            model_id: model_id.to_string(),
            revision: "v1".to_string(),
            fingerprint: None,
        }),
        implementation: None,
    });
}

async fn cleanup_models(engine_id: &EngineId, models: &[&str]) {
    for m in models {
        let ak = mstore::encode_asset_key(m);
        let _ = std::fs::remove_dir_all(mstore::asset_root(engine_id, &ak).unwrap());
    }
}

// ── 基础生命周期 ─────────────────────────────────────────────────────────

#[test]
fn lease_uses_service_instance_id_instead_of_process_generation_id() {
    let engine_id = EngineId::new("paddleocr").unwrap();
    let endpoint = crate::infra::local_engine::port::Endpoint::new(8100);
    let service_identity = ServiceIdentityInput {
        engine_id: engine_id.to_string(),
        instance_id: "inst-service".to_string(),
        token: "test-token".to_string(),
        endpoint,
    };
    let process_identity = ProcessIdentity {
        pid: 4242,
        executable: std::path::PathBuf::from("python.exe"),
        start_time_ms: 123_456,
        instance_id: "inst-process-generation".to_string(),
    };

    let lease = build_process_lease(
        &engine_id,
        &process_identity,
        &service_identity,
        &endpoint,
        "dep-test".to_string(),
    );

    assert_eq!(lease.instance_id, "inst-service");
    assert_ne!(lease.instance_id, process_identity.instance_id);
    assert_eq!(lease.pid, process_identity.pid);
    assert_eq!(lease.endpoint, "http://127.0.0.1:8100");
}

#[tokio::test]
async fn service_rejects_unknown_engine_id() {
    let svc = make_service("fake-known");
    let unknown = EngineId::new("fake-unknown").unwrap();
    assert!(svc.get_status(&unknown).await.is_err());
    assert!(svc.catalog().await.len() == 1);
}

#[tokio::test]
async fn catalog_and_status_lists_have_stable_engine_order() {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        make_fake_adapter("engine-z", true),
        make_fake_adapter("engine-a", true),
    ]));
    let svc = EngineManager::new(registry, Arc::new(NoopEventPort));

    let catalog_ids: Vec<_> = svc
        .catalog()
        .await
        .into_iter()
        .map(|item| item.engine_id.to_string())
        .collect();
    assert_eq!(catalog_ids, vec!["engine-a", "engine-z"]);

    let status_ids: Vec<_> = svc
        .get_all_status()
        .await
        .into_iter()
        .map(|item| item.engine_id.to_string())
        .collect();
    assert_eq!(status_ids, vec!["engine-a", "engine-z"]);
}

#[tokio::test]
async fn initial_status_is_stopped_unknown() {
    let svc = make_service("fake-initial");
    let eid = EngineId::new("fake-initial").unwrap();
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(snap.status.desired, DesiredState::Stopped);
    assert_eq!(snap.status.process, ProcessState::Stopped);
    assert_eq!(snap.status.environment, EnvironmentHealth::Missing);
}

#[tokio::test]
async fn install_marks_environment_ready_when_self_test_passes() {
    let svc = make_service("fake-install-ok");
    let eid = EngineId::new("fake-install-ok").unwrap();
    svc.install(&eid, AdapterConfig::new()).await.unwrap();
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(snap.status.environment, EnvironmentHealth::Ready);
}

#[tokio::test]
async fn install_fails_when_self_test_fails() {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-install-fail",
        false,
    )]));
    let svc = EngineManager::new(registry, Arc::new(NoopEventPort));
    let eid = EngineId::new("fake-install-fail").unwrap();
    let err = svc.install(&eid, AdapterConfig::new()).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::SelfTestFailed);
}

#[tokio::test]
async fn stop_is_idempotent_when_already_stopped() {
    let svc = make_service("fake-stop");
    let eid = EngineId::new("fake-stop").unwrap();
    svc.stop(&eid).await.unwrap();
    svc.stop(&eid).await.unwrap();
}

#[tokio::test]
async fn get_diagnostics_returns_adapter_diagnostics() {
    let svc = make_service("fake-diag");
    let eid = EngineId::new("fake-diag").unwrap();
    let diag = svc.get_diagnostics(&eid).await.unwrap();
    assert!(!diag.entries.is_empty());
}

#[tokio::test]
async fn revision_strictly_increases_after_status_changes() {
    let svc = make_service("fake-rev");
    let eid = EngineId::new("fake-rev").unwrap();
    let r1 = svc.get_status(&eid).await.unwrap().revision;
    svc.stop(&eid).await.unwrap();
    let r2 = svc.get_status(&eid).await.unwrap().revision;
    assert!(r2 > r1);
}

// ── 取消语义（coordinator）──────────────────────────────────────────────

#[tokio::test]
async fn cancel_returns_no_active_operation_when_idle() {
    let svc = make_service("fake-cancel-none");
    let eid = EngineId::new("fake-cancel-none").unwrap();
    let outcome = svc.cancel_operation(&eid, "op-x").await;
    assert_eq!(outcome, CancelOutcome::NoActiveOperation);
}

#[tokio::test]
async fn cancel_rejects_stale_operation_id() {
    let svc = make_service("fake-cancel-stale");
    let eid = EngineId::new("fake-cancel-stale").unwrap();
    let guard = svc.coordinator().try_claim(&eid, "op-current").unwrap();
    let outcome = svc.cancel_operation(&eid, "op-stale").await;
    assert_eq!(
        outcome,
        CancelOutcome::Mismatched {
            current_operation_id: "op-current".to_string()
        }
    );
    guard.release();
}

#[tokio::test]
async fn cancel_after_completion_is_no_active_operation() {
    let svc = make_service("fake-cancel-done");
    let eid = EngineId::new("fake-cancel-done").unwrap();
    let guard = svc.coordinator().try_claim(&eid, "op-1").unwrap();
    guard.release();
    let outcome = svc.cancel_operation(&eid, "op-1").await;
    assert_eq!(outcome, CancelOutcome::NoActiveOperation);
}

/// cancel 后旧 worker 尚未退出（guard 仍持有）——下一个操作必须仍被拒绝；
/// worker 真正结束后才允许下一个操作。
#[tokio::test]
async fn manager_cancel_gates_next_operation_until_worker_finishes() {
    let installer = GatedInstaller::new();
    let eid = EngineId::new("fake-cancel-gate").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-cancel-gate",
        true,
    )]));
    let tag = unique_tag("mgate");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &tag, &format!("{tag}-b")),
        installer.clone(),
    );

    // 确定性等待：spawn 前注册"下载已进入"信号（download 入口必然在 claim 之后）
    let started = installer.started_signal();

    // 模型安装进入下载（gated）
    let svc_c = svc.clone();
    let eid_c = eid.clone();
    let tag_c = tag.clone();
    let install_task = tokio::spawn(async move {
        svc_c
            .install_model(&eid_c, &tag_c, Some("op-gated".to_string()))
            .await
    });

    started.await;
    // 0.22.9 双槽协调：模型安装占 model_storage 槽——用 any 探测（mutating 槽为空）
    let op = svc
        .coordinator()
        .active_operation_any(&eid)
        .expect("下载已进入，claim 必然已登记");

    // cancel：token 触发，但 claim 仍由 worker 持有
    let outcome = svc.cancel_operation(&eid, &op).await;
    assert!(outcome.is_cancelled(), "应成功发出取消信号: {outcome:?}");
    assert!(svc.coordinator().active_operation_any(&eid).is_some());

    // worker 收到取消信号退出（select cancelled 分支 → 成功取消路径）
    let result = install_task.await.unwrap().unwrap();
    assert!(result.success);
    assert_eq!(result.final_stage, ModelOperationStage::Cancelled);

    // worker 结束后 claim 释放——下一个操作可 claim
    assert!(svc.coordinator().active_operation_any(&eid).is_none());
    let guard = svc.coordinator().try_claim(&eid, "op-next").unwrap();
    guard.release();

    cleanup_models(&eid, &[&tag, &format!("{tag}-b")]).await;
}

// ── 变更互斥（必测并发场景）────────────────────────────────────────────

/// 0.22.9 双槽协调语义：模型安装（model_storage 槽）不再阻塞
/// 进程级操作（mutating 槽）——下载进行中 stop / 环境修复照常执行；
/// 同槽的另一个模型操作仍被互斥。
#[tokio::test]
async fn model_install_does_not_block_stop_and_env_repair() {
    let installer = GatedInstaller::new();
    let eid = EngineId::new("fake-race").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-race",
        true,
    )]));
    let tag = unique_tag("race");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &tag, &format!("{tag}-b")),
        installer.clone(),
    );

    // 确定性等待：spawn 前注册"下载已进入"信号
    let started = installer.started_signal();

    let svc_c = svc.clone();
    let eid_c = eid.clone();
    let tag_c = tag.clone();
    let install_task = tokio::spawn(async move { svc_c.install_model(&eid_c, &tag_c, None).await });

    started.await;

    // 同槽（另一个模型安装）→ 仍互斥
    let err = svc.install_model(&eid, &format!("{tag}-b"), None).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

    // stop 不再被模型下载阻塞（mutating 槽空闲即可执行）
    svc.stop(&eid).await.unwrap();

    // 环境修复同样放行（self-test pass → 幂等完成）
    svc.repair(&eid).await.unwrap();

    // 放行安装完成
    installer.release();
    let result = install_task.await.unwrap().unwrap();
    assert!(result.success);

    cleanup_models(&eid, &[&tag, &format!("{tag}-b")]).await;
}

/// 两个模型同时安装（同引擎）——第二个必须被拒绝。
#[tokio::test]
async fn two_model_installs_same_engine_second_rejected() {
    let installer = GatedInstaller::new();
    let eid = EngineId::new("fake-two").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-two", true,
    )]));
    let tag_a = unique_tag("two-a");
    let tag_b = format!("{tag_a}-b");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &tag_a, &tag_b),
        installer.clone(),
    );

    // 确定性等待：spawn 前注册"下载已进入"信号
    let started = installer.started_signal();

    let svc1 = svc.clone();
    let (e1, t1) = (eid.clone(), tag_a.clone());
    let first = tokio::spawn(async move { svc1.install_model(&e1, &t1, None).await });

    started.await;

    // 第二个模型安装 → AlreadyRunning（key = engine_id，与 model_id 无关）
    let err = svc.install_model(&eid, &tag_b, None).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

    installer.release();
    let result = first.await.unwrap().unwrap();
    assert!(result.success);

    cleanup_models(&eid, &[&tag_a, &tag_b]).await;
}

/// 不同引擎并行——barrier 对齐后两个引擎的模型安装都成功。
///
/// 两引擎各有一个模型候选；Barrier(2) 保证两个下载同时进行
/// （若 coordinator 错误地全局串行化，这里会死锁/超时而非误通过）。
#[tokio::test]
async fn different_engines_install_models_concurrently() {
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let eid_a = EngineId::new("fake-par-a").unwrap();
    let eid_b = EngineId::new("fake-par-b").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        make_fake_adapter("fake-par-a", true),
        make_fake_adapter("fake-par-b", true),
    ]));
    let tag_a = unique_tag("par");
    let tag_b = format!("{tag_a}-x");
    // 目录同时覆盖两个引擎（每引擎一个待装模型）
    let reg_a = make_model_registry(&eid_a, &tag_a, &format!("{tag_a}-b"));
    let reg_b = make_model_registry(&eid_b, &tag_b, &format!("{tag_b}-b"));
    let mut models = Vec::new();
    // 重建跨引擎目录：make_model_registry 是单引擎的，这里借 list 展开
    for eid in [&eid_a, &eid_b] {
        let src = if *eid == eid_a { &reg_a } else { &reg_b };
        for m in src.list(eid) {
            models.push(m.clone());
        }
    }
    let catalog = super::super::model_installer::ModelRegistry::new_with_models(models);

    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        catalog,
        Arc::new(BarrierInstaller {
            barrier: barrier.clone(),
        }),
    );

    let svc1 = svc.clone();
    let (ea, ta) = (eid_a.clone(), tag_a.clone());
    let svc2 = svc.clone();
    let (eb, tb) = (eid_b.clone(), tag_b.clone());

    // 两个引擎的模型安装并行——都应成功
    let install_a = tokio::spawn(async move { svc1.install_model(&ea, &ta, None).await });
    let install_b = tokio::spawn(async move { svc2.install_model(&eb, &tb, None).await });

    let (ra, rb) = tokio::join!(install_a, install_b);
    assert!(ra.unwrap().unwrap().success);
    assert!(rb.unwrap().unwrap().success);

    cleanup_models(&eid_a, &[&tag_a, &format!("{tag_a}-b")]).await;
    cleanup_models(&eid_b, &[&tag_b, &format!("{tag_b}-b")]).await;
}

// ── selected / active / 删除冲突 ────────────────────────────────────────

/// selected 与 active 不同：list_models 投影两个独立标志。
#[tokio::test]
async fn selected_and_active_are_independent() {
    let eid = EngineId::new("fake-sel").unwrap();
    let tag = unique_tag("sel");
    let models = [tag.clone(), format!("{tag}-b")];
    // list_models 只投影目录内模型——需要带模型目录的 manager
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-sel", true,
    )]));
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &models[0], &models[1]),
        Arc::new(super::super::model_installer::FakeInstaller::success()),
    );

    // 无运行实例 → is_active 全 false
    let list = svc.list_models(&eid).await;
    assert!(list.iter().all(|m| !m.is_active));

    // 注入 launch snapshot（active = 第二个模型）
    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_launch(&entry, &models[1], "inst-sel").await;
    let list = svc.list_models(&eid).await;
    let active_m = list.iter().find(|m| m.model_id == models[1]).unwrap();
    let inactive_m = list.iter().find(|m| m.model_id == models[0]).unwrap();
    assert!(active_m.is_active);
    assert!(!inactive_m.is_active);

    // get_model_status 同样区分
    let st = svc.get_model_status(&eid, &models[1]).await.unwrap();
    assert!(st.is_active);
    let st = svc.get_model_status(&eid, &models[0]).await.unwrap();
    assert!(!st.is_active);
}

/// 删除实际 active 模型（launch snapshot 判定）→ 结构化冲突，
/// instance_id 来自 launch snapshot（非 "current" 占位符）。
#[tokio::test]
async fn delete_active_model_blocked_by_launch_snapshot() {
    let installer = super::super::model_installer::FakeInstaller::success();
    let eid = EngineId::new("fake-del").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-del", true,
    )]));
    let tag = unique_tag("del");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &tag, &format!("{tag}-b")),
        Arc::new(installer),
    );

    // 安装模型
    let r = svc.install_model(&eid, &tag, None).await.unwrap();
    assert!(r.success);

    // 注入 launch snapshot：运行中实例使用该模型
    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_launch(&entry, &tag, "inst-del-123").await;

    // 删除 → ActiveInRunningInstance（instance_id 真实来自 snapshot）
    let r = svc.delete_model(&eid, &tag, None).await.unwrap();
    assert!(!r.success);
    let err = r.error.expect("应有结构化冲突");
    assert_eq!(err.code, LocalEngineErrorCode::ArtifactReferenced);

    // 清除 launch snapshot（模拟停止）后可删除
    {
        let mut l = entry.launch.lock().await;
        *l = None;
    }
    let r = svc.delete_model(&eid, &tag, None).await.unwrap();
    assert!(r.success, "停止后删除应成功: {:?}", r.error);

    cleanup_models(&eid, &[&tag, &format!("{tag}-b")]).await;
}

/// descriptor 默认模型不构成删除保护——非 selected 非 active 可删除。
#[tokio::test]
async fn descriptor_default_model_is_deletable() {
    let eid = EngineId::new("fake-dd").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-dd", true,
    )]));
    let tag = unique_tag("dd");
    let default_like = format!("{tag}-default");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &default_like, &format!("{tag}-b")),
        Arc::new(super::super::model_installer::FakeInstaller::success()),
    );

    // "descriptor 默认模型"（registry 第一项，未 selected、无运行实例）
    let r = svc.install_model(&eid, &default_like, None).await.unwrap();
    assert!(r.success);
    let r = svc.delete_model(&eid, &default_like, None).await.unwrap();
    assert!(
        r.success,
        "descriptor 默认模型不构成永久删除保护: {:?}",
        r.error
    );

    cleanup_models(&eid, &[&default_like, &format!("{tag}-b")]).await;
}

/// 回归测试：同会话内 install_model 之后 repair_model 必须成功。
///
/// 曾因内存模型状态缓存（Installed → Downloading 非法转移）而失败；
/// 缓存删除后磁盘 manifest 是唯一模型状态真源。
#[tokio::test]
async fn repair_model_after_install_in_same_session_succeeds() {
    let eid = EngineId::new("fake-repair-model").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-repair-model",
        true,
    )]));
    let tag = unique_tag("repmodel");
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &tag, &format!("{tag}-b")),
        Arc::new(super::super::model_installer::FakeInstaller::success()),
    );

    let r = svc.install_model(&eid, &tag, None).await.unwrap();
    assert!(r.success);

    // 同会话内修复——重新下载 + 校验 + 提升，必须成功
    let r = svc.repair_model(&eid, &tag, None).await.unwrap();
    assert!(r.success, "repair after install 应成功: {:?}", r.error);

    // 磁盘真源：仍为已安装
    let status = svc.get_model_status(&eid, &tag).await.unwrap();
    assert_eq!(status.install_state, ModelInstallState::Installed);

    cleanup_models(&eid, &[&tag, &format!("{tag}-b")]).await;
}

#[tokio::test]
async fn delete_not_installed_returns_error() {
    let eid = EngineId::new("fake-del-none").unwrap();
    let tag = unique_tag("delnone");
    let models = [tag.clone(), format!("{tag}-b")];
    // 构造带目录的 manager（make_service 无模型目录）
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-del-none",
        true,
    )]));
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &models[0], &models[1]),
        Arc::new(super::super::model_installer::FakeInstaller::success()),
    );
    let err = svc.delete_model(&eid, &models[0], None).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::NotRunning);
}

// ── 模型身份解析 ────────────────────────────────────────────────────────

#[test]
fn resolve_identity_fails_when_not_installed() {
    let eid = EngineId::new("fake-rid").unwrap();
    let contract = ModelContract {
        model_id: "m".to_string(),
        revision: "v1".to_string(),
        checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
    };
    let result = resolve_expected_model_identity(&eid, None, &contract, true);
    assert!(result.is_err(), "managed 模式未安装应 fail-closed");
}

#[test]
fn resolve_identity_uses_descriptor_for_adapter_managed_model() {
    let eid = EngineId::new("fake-rid2").unwrap();
    let contract = ModelContract {
        model_id: "m".to_string(),
        revision: "v1".to_string(),
        checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
    };
    let (id, rev, fp) = resolve_expected_model_identity(&eid, None, &contract, false).unwrap();
    assert_eq!(id, "m");
    assert_eq!(rev, "v1");
    assert!(fp.is_none());
}

#[test]
fn model_fingerprint_requires_nonzero_lowercase_sha256_hex() {
    assert!(is_valid_model_fingerprint(&"a".repeat(64)));
    assert!(!is_valid_model_fingerprint(&"A".repeat(64)));
    assert!(!is_valid_model_fingerprint(&"0".repeat(64)));
    assert!(!is_valid_model_fingerprint("abc"));
}

// ── 状态提交 fail-closed ────────────────────────────────────────────────

#[tokio::test]
async fn commit_with_stale_operation_id_rejected() {
    let svc = make_service("fake-commit");
    let eid = EngineId::new("fake-commit").unwrap();
    let _guard = svc.coordinator().try_claim(&eid, "op-1").unwrap();
    let err = svc
        .commit_status_internal(&eid, Some("op-stale"), |_| {})
        .await
        .unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::Rejected);
}

#[tokio::test]
async fn commit_without_op_id_rejected_while_operation_active() {
    let svc = make_service("fake-commit2");
    let eid = EngineId::new("fake-commit2").unwrap();
    let _guard = svc.coordinator().try_claim(&eid, "op-1").unwrap();
    let err = svc
        .commit_status_internal(&eid, None, |_| {})
        .await
        .unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::Rejected);
}

#[tokio::test]
async fn commit_with_op_id_rejected_after_operation_finished() {
    let svc = make_service("fake-commit3");
    let eid = EngineId::new("fake-commit3").unwrap();
    let guard = svc.coordinator().try_claim(&eid, "op-1").unwrap();
    guard.release();
    let err = svc
        .commit_status_internal(&eid, Some("op-1"), |_| {})
        .await
        .unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::Rejected);
}

// ── 存储 / 清理 ─────────────────────────────────────────────────────────

#[tokio::test]
async fn scan_storage_rejects_unknown_engine() {
    let svc = make_service("fake-scan");
    let unknown = EngineId::new("fake-scan-unknown").unwrap();
    assert!(svc.scan_storage(&unknown).await.is_err());
}

#[tokio::test]
async fn scan_storage_returns_empty_when_nothing_installed() {
    let svc = make_service("fake-scan-empty");
    let eid = EngineId::new("fake-scan-empty").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let dto = svc.scan_storage(&eid).await.unwrap();
    assert!(dto.targets.is_empty());
}

#[tokio::test]
async fn cleanup_rejects_active_slot_and_unknown_targets() {
    let svc = make_service("fake-clean");
    let eid = EngineId::new("fake-clean").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let space = crate::infra::local_engine::deployment::DeploymentSpace::engine(&eid);

    // 造一个 active 指针 + slot
    std::fs::create_dir_all(space.slot_dir("slot-a")).unwrap();
    DeploymentStore::write_pointer(
        &space,
        &DeploymentPointer {
            install_id: "dep-1".to_string(),
            slot: "slot-a".to_string(),
            updated_at_ms: 0,
            schema_version:
                crate::infra::local_engine::deployment::DEPLOYMENT_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();

    let result = svc
        .cleanup_targets(&eid, &["environment:slot-a".to_string()], None)
        .await
        .unwrap();
    assert!(
        result
            .skipped_target_ids
            .contains(&"environment:slot-a".to_string())
    );
    assert!(result.cleaned_target_ids.is_empty());
    // active slot 仍在
    assert!(space.slot_dir("slot-a").exists());

    // 未知 target id
    let result = svc
        .cleanup_targets(&eid, &["bogus-target".to_string()], None)
        .await
        .unwrap();
    assert!(
        result
            .skipped_target_ids
            .contains(&"bogus-target".to_string())
    );

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

#[tokio::test]
async fn cleanup_removes_non_active_slot_and_staging() {
    let svc = make_service("fake-clean2");
    let eid = EngineId::new("fake-clean2").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let space = crate::infra::local_engine::deployment::DeploymentSpace::engine(&eid);

    // active = slot-a；残留 slot-b + 孤儿 staging
    std::fs::create_dir_all(space.slot_dir("slot-a")).unwrap();
    std::fs::create_dir_all(space.slot_dir("slot-b")).unwrap();
    std::fs::write(space.slot_dir("slot-b").join("data.bin"), b"x").unwrap();
    std::fs::create_dir_all(space.operation_staging_dir("op-orphan")).unwrap();
    DeploymentStore::write_pointer(
        &space,
        &DeploymentPointer {
            install_id: "dep-1".to_string(),
            slot: "slot-a".to_string(),
            updated_at_ms: 0,
            schema_version:
                crate::infra::local_engine::deployment::DEPLOYMENT_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();

    let result = svc
        .cleanup_targets(
            &eid,
            &[
                "environment:slot-b".to_string(),
                "cache:staging".to_string(),
            ],
            None,
        )
        .await
        .unwrap();
    assert!(
        result
            .cleaned_target_ids
            .contains(&"environment:slot-b".to_string())
    );
    assert!(
        result
            .cleaned_target_ids
            .contains(&"cache:staging".to_string())
    );
    assert!(!space.slot_dir("slot-b").exists());
    assert!(space.slot_dir("slot-a").exists(), "active 不可删");

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

#[tokio::test]
async fn scan_storage_targets_no_full_paths() {
    let svc = make_service("fake-scan-paths");
    let eid = EngineId::new("fake-scan-paths").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let space = crate::infra::local_engine::deployment::DeploymentSpace::engine(&eid);
    std::fs::create_dir_all(space.slot_dir("slot-a")).unwrap();
    DeploymentStore::write_pointer(
        &space,
        &DeploymentPointer {
            install_id: "dep-1".to_string(),
            slot: "slot-a".to_string(),
            updated_at_ms: 0,
            schema_version:
                crate::infra::local_engine::deployment::DEPLOYMENT_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();
    let dto = svc.scan_storage(&eid).await.unwrap();
    for t in &dto.targets {
        assert!(
            !t.target_id.contains('\\') && !t.target_id.contains(':')
                || t.target_id.starts_with("environment:")
                || t.target_id.starts_with("cache:")
                || t.target_id.starts_with("model:")
                || t.target_id.starts_with("shared_runtime:")
                || t.target_id.starts_with("shared_download_cache:")
                || t.target_id.starts_with("legacy:"),
            "target_id 不应包含完整路径: {}",
            t.target_id
        );
    }
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

// ── 孤儿与关停 ──────────────────────────────────────────────────────────

#[tokio::test]
async fn stop_orphan_engine_rejects_unknown_engine() {
    let svc = make_service("fake-orphan");
    let unknown = EngineId::new("fake-orphan-unknown").unwrap();
    assert!(svc.stop_orphan_engine(&unknown).await.is_err());
}

#[tokio::test]
async fn stop_orphan_engine_returns_lease_not_found_when_no_lease() {
    let svc = make_service("fake-orphan2");
    let eid = EngineId::new("fake-orphan2").unwrap();
    let result = svc.stop_orphan_engine(&eid).await.unwrap();
    assert!(!result.stopped);
    assert_eq!(result.reason, "lease_not_found");
}

#[tokio::test]
async fn shutdown_all_blocking_uses_process_registry() {
    // 无进程时调用不 panic
    let svc = make_service("fake-shutdown");
    svc.shutdown_all_blocking();
}

// ── repair ──────────────────────────────────────────────────────────────

#[tokio::test]
async fn repair_marks_environment_ready_when_self_test_passes() {
    let svc = make_service("fake-repair");
    let eid = EngineId::new("fake-repair").unwrap();
    svc.repair(&eid).await.unwrap();
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(snap.status.environment, EnvironmentHealth::Ready);
}

#[tokio::test]
async fn repair_fails_when_self_test_fails() {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-repair-fail",
        false,
    )]));
    let svc = EngineManager::new(registry, Arc::new(NoopEventPort));
    let eid = EngineId::new("fake-repair-fail").unwrap();
    let err = svc.repair(&eid).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::SelfTestFailed);
}

#[tokio::test]
async fn repair_ends_with_idle_operation_and_released_claim() {
    let svc = make_service("fake-repair-c");
    let eid = EngineId::new("fake-repair-c").unwrap();
    let (_op_id, end_state) = svc.repair(&eid).await.unwrap();
    assert_eq!(end_state, EnvOperationEndState::Completed);
    let snap = svc.get_status(&eid).await.unwrap();
    // 终态协议：操作结束后 active_operation 必须归位 Idle——
    // 不允许 kind=Repairing && stage=Completed 的混合状态驻留快照（前端会显示 busy）
    assert_eq!(snap.status.operation.kind, OperationKind::Idle);
    assert!(!snap.status.operation.is_active());
    // 完成后 claim 已释放
    assert!(svc.coordinator().active_operation(&eid).is_none());
}

/// install 结束后 operation 归位 Idle——completed operation 不再显示 busy。
#[tokio::test]
async fn install_ends_with_idle_operation() {
    let svc = make_service("fake-install-idle");
    let eid = EngineId::new("fake-install-idle").unwrap();
    let (_op_id, end_state) = svc.install(&eid, AdapterConfig::new()).await.unwrap();
    assert_eq!(end_state, EnvOperationEndState::Completed);
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(snap.status.operation.kind, OperationKind::Idle);
    assert!(!snap.status.operation.is_active());
    assert!(svc.coordinator().active_operation(&eid).is_none());
}

// ── InstallSinkAdapter ─────────────────────────────────────────────────

struct RecordingEventPort {
    install_logs: std::sync::Mutex<Vec<(String, u64, String)>>,
    stages: std::sync::Mutex<Vec<(String, String)>>,
}

impl RecordingEventPort {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            install_logs: std::sync::Mutex::new(Vec::new()),
            stages: std::sync::Mutex::new(Vec::new()),
        })
    }
}

impl EventPort for RecordingEventPort {
    fn emit_status(&self, _snapshot: &EngineStatusSnapshot) {}
    fn emit_log(
        &self,
        _engine_id: &EngineId,
        _instance_id: &str,
        _seq: u64,
        _level: super::super::dto::EngineLogLevel,
        _line: &str,
    ) {
    }
    fn emit_install_log(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        seq: u64,
        _level: super::super::dto::EngineLogLevel,
        text: &str,
    ) {
        self.install_logs.lock().unwrap().push((
            format!("{engine_id}/{operation_id}"),
            seq,
            text.to_string(),
        ));
    }
    fn emit_install_stage(&self, engine_id: &EngineId, operation_id: &str, stage: &str) {
        self.stages
            .lock()
            .unwrap()
            .push((format!("{engine_id}/{operation_id}"), stage.to_string()));
    }
}

#[tokio::test]
async fn model_staging_validation_failure_is_visible_in_operation_logs() {
    let port = RecordingEventPort::new();
    let eid = EngineId::new("fake-model-log").unwrap();
    let model_id = format!("gguf/{}", unique_tag("path-model"));
    let fallback_id = format!("{model_id}-fallback");
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        eid.as_str(),
        true,
    )]));
    let svc = EngineManager::new_with_providers(
        registry,
        port.clone(),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(&eid, &model_id, &fallback_id),
        Arc::new(super::super::model_installer::FakeInstaller::success()),
    );
    let unsafe_operation_id = format!("install-model-{model_id}-123");

    let result = svc
        .install_model(&eid, &model_id, Some(unsafe_operation_id.clone()))
        .await
        .unwrap();

    assert!(!result.success);
    {
        let logs = port.install_logs.lock().unwrap();
        assert!(logs.iter().any(|(key, _, text)| {
            key.ends_with(&unsafe_operation_id)
                && text.contains("staging 目录创建失败")
                && text.starts_with("[ERROR]")
        }));
    }
    {
        let stages = port.stages.lock().unwrap();
        assert!(
            stages
                .iter()
                .any(|(key, stage)| key.ends_with(&unsafe_operation_id) && stage == "failed")
        );
    }

    cleanup_models(&eid, &[&model_id, &fallback_id]).await;
}

#[test]
fn install_sink_adapter_seq_monotonic() {
    let port = RecordingEventPort::new();
    let sink = InstallSinkAdapter::new(
        port.clone(),
        EngineId::new("fake-sink").unwrap(),
        "op-1".to_string(),
    );
    for i in 0..5 {
        sink.on_log("info", &format!("line {i}"));
    }
    let logs = port.install_logs.lock().unwrap();
    let seqs: Vec<u64> = logs.iter().map(|(_, s, _)| *s).collect();
    let mut sorted = seqs.clone();
    sorted.sort();
    assert_eq!(seqs, sorted);
}

#[test]
fn install_sink_adapter_flood_protection_drops_excess() {
    let port = RecordingEventPort::new();
    let sink = InstallSinkAdapter::new(
        port.clone(),
        EngineId::new("fake-sink2").unwrap(),
        "op-1".to_string(),
    );
    for i in 0..200 {
        sink.on_log("info", &format!("line {i}"));
    }
    let count = port.install_logs.lock().unwrap().len();
    assert!(count <= 100, "洪泛保护应丢弃超额日志，实际 {}", count);
}

#[test]
fn install_sink_adapter_operation_id_isolation() {
    let port = RecordingEventPort::new();
    let s1 = InstallSinkAdapter::new(
        port.clone(),
        EngineId::new("fake-sink3").unwrap(),
        "op-1".to_string(),
    );
    let s2 = InstallSinkAdapter::new(
        port.clone(),
        EngineId::new("fake-sink3").unwrap(),
        "op-2".to_string(),
    );
    s1.on_log("info", "from-op-1");
    s2.on_log("info", "from-op-2");
    let logs = port.install_logs.lock().unwrap();
    assert!(
        logs.iter()
            .any(|(k, _, t)| k.ends_with("op-1") && t == "from-op-1")
    );
    assert!(
        logs.iter()
            .any(|(k, _, t)| k.ends_with("op-2") && t == "from-op-2")
    );
}

#[test]
fn install_sink_adapter_on_stage_emits_install_stage() {
    let port = RecordingEventPort::new();
    let sink = InstallSinkAdapter::new(
        port.clone(),
        EngineId::new("fake-sink4").unwrap(),
        "op-1".to_string(),
    );
    sink.on_stage("downloading");
    let stages = port.stages.lock().unwrap();
    assert_eq!(stages.len(), 1);
    assert_eq!(stages[0].1, "downloading");
}

// ── 日志分类 ────────────────────────────────────────────────────────────

#[test]
fn engine_log_level_uses_explicit_wrapper_prefixes() {
    use super::super::dto::EngineLogLevel;
    use crate::infra::local_engine::log_pipe::LogSource;
    assert_eq!(
        classify_engine_log(LogSource::Stderr, "[ERROR] boom"),
        EngineLogLevel::Error
    );
    assert_eq!(
        classify_engine_log(LogSource::Stdout, "[WARN] careful"),
        EngineLogLevel::Warn
    );
    assert_eq!(
        classify_engine_log(LogSource::Stdout, "[INFO] hi"),
        EngineLogLevel::Info
    );
}

#[test]
fn unclassified_engine_output_is_debug_not_stderr_warning() {
    use super::super::dto::EngineLogLevel;
    use crate::infra::local_engine::log_pipe::LogSource;
    assert_eq!(
        classify_engine_log(LogSource::Stderr, "random stderr noise"),
        EngineLogLevel::Debug
    );
}

// ── 0.22.6.1 requested/actual backend 语义 ──────────────────────────────

/// CPU profile + health 回报 actual=cpu → 通过。
#[test]
fn backend_ready_with_cpu_observation_passes() {
    let mapping = HealthMapping {
        service: ServiceHealth::Healthy,
        model: ModelHealth::Ready,
        environment: None,
        backend: Some(crate::domain::local_engine::BackendObservation {
            actual_backend: crate::domain::local_engine::ComputeBackend::Cpu,
            device_name: "CPU".to_string(),
            consistent: true,
        }),
        model_id: Some("iic/SenseVoiceSmall".to_string()),
        model_revision: Some("funasr-1.x".to_string()),
        model_content_fingerprint: None,
    };
    assert!(require_backend_when_ready(&mapping).is_ok());
}

/// CPU profile + health 回报 actual=cuda → 后端一致性校验拒绝（GPU↔CPU 交叉）。
#[test]
fn backend_cpu_profile_rejects_cuda_observation() {
    let profile_backend = crate::domain::local_engine::ComputeBackend::Cpu;
    let obs = crate::domain::local_engine::BackendObservation {
        actual_backend: crate::domain::local_engine::ComputeBackend::Cuda,
        device_name: "RTX 4060".to_string(),
        consistent: true,
    };
    let verification = crate::infra::local_engine::runtime::verify_backend_consistency(
        profile_backend,
        Some(&obs),
    );
    assert_eq!(
        verification.state,
        crate::domain::local_engine::BackendState::Error,
        "CPU profile + cuda 观测必须被拒绝"
    );
}

/// 模型 Loading（actual backend 尚不可观察）→ 允许 backend 缺失。
#[test]
fn backend_loading_without_observation_is_allowed() {
    let mapping = HealthMapping {
        service: ServiceHealth::Healthy,
        model: ModelHealth::Loading,
        environment: None,
        backend: None,
        model_id: None,
        model_revision: None,
        model_content_fingerprint: None,
    };
    assert!(require_backend_when_ready(&mapping).is_ok());
}

/// Model Ready 但 actual backend 缺失 → 拒绝（协议不完整）。
#[test]
fn backend_ready_without_observation_rejected() {
    let mapping = HealthMapping {
        service: ServiceHealth::Healthy,
        model: ModelHealth::Ready,
        environment: None,
        backend: None,
        model_id: Some("m".to_string()),
        model_revision: Some("r".to_string()),
        model_content_fingerprint: None,
    };
    let err = require_backend_when_ready(&mapping).unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::BackendMismatch);
}

// ── 0.22.9 implementation 冻结与只读投影 ─────────────────────────────────

/// 构造为 engine "paddleocr" 声明 in-process implementation 的注册表
/// （fake adapter 的 model_contract = "fake-model"）。
fn make_inprocess_impl_registry(engine_id: &EngineId) -> ImplementationRegistry {
    ImplementationRegistry::new_validated(
        vec![ImplementationDescriptor {
            id: ImplementationId::PaddleOcrOnnxInProcess,
            engine_id: engine_id.clone(),
            runtime_kind: RuntimePlan::OnnxRuntime,
            service_transport: ServiceTransport::InProcess,
            executor_topology: ExecutorTopology::InProcess,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::OnnxRuntime,
                artifact_ids: vec![ArtifactId::new("ort-test").unwrap()],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: vec!["fake-model".to_string()],
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }],
        vec![ImplementationBinding {
            engine_id: engine_id.clone(),
            model_id: "fake-model".to_string(),
            implementation: ImplementationId::PaddleOcrOnnxInProcess,
        }],
    )
    .expect("测试 implementation 声明必须合法")
}

/// start_inprocess 冻结 implementation 并投影到状态；stop_inprocess 清除。
#[tokio::test]
async fn inprocess_start_freezes_implementation_and_stop_clears() {
    let eid = EngineId::new("paddleocr").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "paddleocr",
        true,
    )]));
    let svc = EngineManager::new_with_providers_and_implementations(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::empty(),
        Arc::new(super::super::model_installer::NoopModelWorker),
        make_inprocess_impl_registry(&eid),
    );

    // 环境就绪（fake 部署无真实环境，直接提交 Ready）
    svc.commit_status_internal(&eid, None, |s| {
        s.environment = EnvironmentHealth::Ready;
    })
    .await
    .unwrap();

    svc.start_inprocess(&eid).await.unwrap();

    // 状态投影：active_implementation 来自 start 冻结
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(
        snap.status.active_implementation,
        Some(ImplementationId::PaddleOcrOnnxInProcess)
    );
    // in-process 引擎无 launch snapshot——snapshot 读取返回 None，
    // 投影真源是 EngineStatus.active_implementation
    assert_eq!(svc.get_current_implementation(&eid).await.unwrap(), None);

    // stop 清除 active implementation
    svc.stop_inprocess(&eid).await.unwrap();
    let snap = svc.get_status(&eid).await.unwrap();
    assert_eq!(snap.status.active_implementation, None);
}

/// 解析 fail-closed：绑定表内模型成功、未知模型拒绝、无声明引擎返回 None。
#[tokio::test]
async fn implementation_resolution_is_fail_closed() {
    let eid = EngineId::new("funasr").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "funasr", true,
    )]));
    // new_with_providers 默认装配 builtin implementation 注册表
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::empty(),
        Arc::new(super::super::model_installer::NoopModelWorker),
    );

    // fake adapter 的 contract model "fake-model" 不在 funasr 绑定表 → 拒绝
    //（生产路径中 start 会在冻结后失败，不静默换模）
    assert!(
        svc.resolve_implementation_for_model(&eid, Some("fake-model"))
            .is_err()
    );
    // 未知旧模型（Python 时代 id）同样拒绝
    assert!(
        svc.resolve_implementation_for_model(&eid, Some("iic/SenseVoiceSmall"))
            .is_err()
    );

    // 绑定表中的模型解析到 GGUF implementation
    assert_eq!(
        svc.resolve_implementation_for_model(
            &eid,
            Some(crate::app::local_engine::funasr::gguf::GGUF_SENSEVOICE_ID)
        )
        .unwrap(),
        Some(ImplementationId::FunasrGgufWorker)
    );
    assert_eq!(
        svc.resolve_implementation_for_model(
            &eid,
            Some(crate::app::local_engine::funasr::gguf::GGUF_NANO_ID)
        )
        .unwrap(),
        Some(ImplementationId::FunasrGgufWorker)
    );

    // 无 implementation 声明的引擎（测试 fake 场景）→ Ok(None)
    let other = EngineId::new("engine-without-impls").unwrap();
    assert_eq!(
        svc.resolve_implementation_for_model(&other, Some("anything"))
            .unwrap(),
        None
    );
}

/// launch snapshot 冻结的 implementation 与 selected 无关：
/// snapshot 注入后 get_current_implementation 只读 snapshot。
#[tokio::test]
async fn frozen_implementation_is_independent_of_selected() {
    let eid = EngineId::new("funasr").unwrap();
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "funasr", true,
    )]));
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::empty(),
        Arc::new(super::super::model_installer::NoopModelWorker),
    );

    // 未运行 → None
    assert_eq!(svc.get_current_implementation(&eid).await.unwrap(), None);

    // 注入运行中实例（start 冻结的模拟）：模型与 implementation 均已冻结
    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_launch(&entry, "frozen-model-a", "inst-frozen").await;
    // 手工置实现——模拟 start 冻结路径写入的 snapshot
    {
        let mut l = entry.launch.lock().await;
        if let Some(snapshot) = l.as_mut() {
            snapshot.implementation = Some(ImplementationId::FunasrGgufWorker);
        }
    }

    // 冻结值只读快照——配置 selected 变化不影响（selected 不经过 snapshot）
    assert_eq!(
        svc.get_current_implementation(&eid).await.unwrap(),
        Some(ImplementationId::FunasrGgufWorker)
    );
    assert_eq!(
        svc.get_current_model_id(&eid).await.unwrap(),
        Some("frozen-model-a".to_string())
    );
}

// ── per-implementation deployment（0.22.9 Handoff 02）─────────────────────

use crate::infra::local_engine::deployment::{DeploymentSpace, DeploymentStore};

/// fake 引擎绑定 ParaformerOnnxWorker（implementation 级空间）的注册表。
///
/// 与 in-process 变体相对：该 implementation 经闭合映射落在
/// `impl-paraformer_onnx_worker/` 空间，用于验证双 deployment 隔离。
fn make_onnx_worker_impl_registry(engine_id: &EngineId) -> ImplementationRegistry {
    ImplementationRegistry::new_validated(
        vec![ImplementationDescriptor {
            id: ImplementationId::ParaformerOnnxWorker,
            engine_id: engine_id.clone(),
            runtime_kind: RuntimePlan::ManagedBinary,
            service_transport: ServiceTransport::Http,
            executor_topology: ExecutorTopology::ManagedWorker,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_ids: vec![ArtifactId::new("onnx-worker-test").unwrap()],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: vec!["fake-model".to_string()],
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }],
        vec![ImplementationBinding {
            engine_id: engine_id.clone(),
            model_id: "fake-model".to_string(),
            implementation: ImplementationId::ParaformerOnnxWorker,
        }],
    )
    .expect("测试 implementation 声明必须合法")
}

/// 在指定部署空间写入完整 active 部署（manifest + 指针）。
fn write_full_deployment(space: &DeploymentSpace, install_id: &str) {
    use crate::infra::local_engine::deployment::DEPLOYMENT_POINTER_SCHEMA_VERSION;
    use crate::infra::local_engine::runtime::MANIFEST_SCHEMA_VERSION;

    let slot = "slot-a";
    std::fs::create_dir_all(space.slot_dir(slot)).unwrap();
    let manifest = crate::infra::local_engine::runtime::DeploymentManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        engine_id: space.engine_id().clone(),
        runtime_kind: RuntimePlan::PythonVenv,
        install_id: install_id.to_string(),
        requested_preference: ComputePreference::Cpu,
        resolved_profile: crate::infra::local_engine::runtime::ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: crate::infra::local_engine::runtime::ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("fake-artifact").unwrap(),
            priority: 0,
        },
        installed_at_ms: 0,
        artifact: crate::infra::local_engine::runtime::ArtifactIdentity {
            runtime_kind: RuntimePlan::PythonVenv,
            artifact_id: ArtifactId::new("fake-artifact").unwrap(),
            sha256: "ab".repeat(32),
        },
        model_contract: crate::infra::local_engine::runtime::ModelContract {
            model_id: "fake-model".to_string(),
            revision: "v1".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
        },
        fallback_reasons: Vec::new(),
        extension: crate::infra::local_engine::runtime::ManifestExtension::PythonVenv(
            crate::infra::local_engine::runtime::PythonManifestExt {
                python_version: "3.12.8".to_string(),
                python_artifact_id: ArtifactId::new("fake-artifact").unwrap(),
                packages: Vec::new(),
                uv_version: "0.6.10".to_string(),
                index_url: None,
                self_test_passed: true,
            },
        ),
    };
    std::fs::write(
        space.slot_manifest_path(slot),
        serde_json::to_vec_pretty(&manifest).unwrap(),
    )
    .unwrap();
    DeploymentStore::write_pointer(
        space,
        &DeploymentPointer {
            install_id: install_id.to_string(),
            slot: slot.to_string(),
            updated_at_ms: 0,
            schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();
}

/// start 从 **resolved implementation 的部署空间**读取 deployment：
/// engine 级旧 deployment 存在、impl 空间未安装时 fail-closed 拒绝启动；
/// impl 空间安装后才允许越过 deployment 读取（fake exe spawn 失败）。
#[tokio::test]
async fn start_reads_deployment_from_implementation_space_not_engine_root() {
    let eid = EngineId::new("fake-impl-space").unwrap();
    let engine_space = DeploymentSpace::engine(&eid);
    let impl_space = DeploymentSpace::resolve(&eid, ImplementationId::ParaformerOnnxWorker);
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));

    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        make_fake_adapter_with_options(eid.as_str(), true, false),
    ]));
    let svc = EngineManager::new_with_providers_and_implementations(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::empty(),
        Arc::new(super::super::model_installer::NoopModelWorker),
        make_onnx_worker_impl_registry(&eid),
    );

    // 环境状态置 Ready（probe 对 fake 部署的粗粒度投影；per-implementation
    // 就绪由 start 冻结段按空间 fail-closed 复核）
    svc.commit_status_internal(&eid, None, |s| {
        s.environment = EnvironmentHealth::Ready;
    })
    .await
    .unwrap();

    // engine 级旧 deployment 存在（模拟 0.22.7/0.22.8 engine-level 资产）
    write_full_deployment(&engine_space, "dep-engine-level");

    // impl 空间未安装 → start 必须 fail-closed（EnvironmentMissing），
    // 不能错误认领 engine 级 deployment
    let err = svc.start(&eid, AdapterConfig::new()).await.unwrap_err();
    assert_eq!(
        err.code,
        LocalEngineErrorCode::EnvironmentMissing,
        "impl 空间未安装时 start 应拒绝，实际: {err:?}"
    );

    // impl 空间安装后 → start 越过 deployment 读取
    //（fake exe 不存在，spawn 失败 SpawnFailed——证明冻结段已通过）
    write_full_deployment(&impl_space, "dep-impl-level");
    let err = svc.start(&eid, AdapterConfig::new()).await.unwrap_err();
    assert_eq!(
        err.code,
        LocalEngineErrorCode::SpawnFailed,
        "impl 空间部署就绪后应越过 deployment 检查，实际: {err:?}"
    );

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// GGUF 语义的兼容读取：engine 级 deployment 经 GGUF implementation 映射
/// 原样可读（无 implementation 声明的 fake 引擎也走 engine 级空间）。
#[tokio::test]
async fn legacy_engine_level_deployment_readable_without_migration() {
    let eid = EngineId::new("fake-legacy-read").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));

    // 无 implementation 声明的引擎（builtin 注册表对 fake 引擎无声明）
    // → deployment_space_for 返回 engine 级空间
    let space = super::lifecycle::deployment_space_for(&eid, None);
    assert_eq!(space.root(), runtime::engine_root(&eid));

    write_full_deployment(&space, "dep-legacy");
    let active = DeploymentStore::read_active(&space).unwrap().unwrap();
    assert_eq!(active.0.install_id, "dep-legacy");

    // 0.22.9 的 GGUF 映射解析到同一空间（路径字节一致——不搬迁）
    let gguf_space = DeploymentSpace::resolve(
        &EngineId::new("fake-legacy-read").unwrap(),
        ImplementationId::FunasrGgufWorker,
    );
    assert_eq!(gguf_space.pointer_path(), space.pointer_path());

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// 存储扫描 + 清理按 implementation 独立：两条空间的 slot 各自列出，
/// active 删除保护按空间生效，未知 implementation 空间 fail-closed 拒绝。
#[tokio::test]
async fn storage_scan_and_cleanup_are_implementation_scoped() {
    let svc = make_service("fake-impl-clean");
    let eid = EngineId::new("fake-impl-clean").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let engine_space = DeploymentSpace::engine(&eid);
    let impl_space = DeploymentSpace::resolve(&eid, ImplementationId::ParaformerOnnxWorker);

    // engine 级 active slot-a；impl 空间 active slot-a + 残留 slot-b
    write_full_deployment(&engine_space, "dep-engine");
    write_full_deployment(&impl_space, "dep-impl");
    std::fs::create_dir_all(impl_space.slot_dir("slot-b")).unwrap();

    // 扫描：两条空间的环境各自成目标，current 按空间标注
    let dto = svc.scan_storage(&eid).await.unwrap();
    let find = |id: &str| dto.targets.iter().find(|t| t.target_id == id);
    let engine_target = find("environment:slot-a").expect("engine 级 slot-a 应列出");
    assert!(engine_target.current, "engine 级 active 应标记 current");
    let impl_target =
        find("environment:impl-paraformer_onnx_worker:slot-a").expect("impl slot-a 应列出");
    assert!(
        impl_target.current,
        "impl active 应按 impl 空间标记 current"
    );
    assert!(
        find("environment:impl-paraformer_onnx_worker:slot-b").is_some(),
        "impl slot-b 应列出且可清理"
    );

    // engine 级 active 拒绝删除
    let result = svc
        .cleanup_targets(&eid, &["environment:slot-a".to_string()], None)
        .await
        .unwrap();
    assert!(
        result
            .skipped_target_ids
            .contains(&"environment:slot-a".into())
    );
    // impl 空间 active 同样拒绝（按 impl 空间指针判定）
    let result = svc
        .cleanup_targets(
            &eid,
            &["environment:impl-paraformer_onnx_worker:slot-a".to_string()],
            None,
        )
        .await
        .unwrap();
    assert!(
        result
            .skipped_target_ids
            .contains(&"environment:impl-paraformer_onnx_worker:slot-a".into())
    );
    assert!(impl_space.slot_dir("slot-a").exists(), "impl active 不可删");
    assert!(
        engine_space.slot_dir("slot-a").exists(),
        "engine active 不可删"
    );

    // impl 空间非 active slot-b 正常删除，engine 级 slot 不受影响
    let result = svc
        .cleanup_targets(
            &eid,
            &["environment:impl-paraformer_onnx_worker:slot-b".to_string()],
            None,
        )
        .await
        .unwrap();
    assert!(
        result
            .cleaned_target_ids
            .contains(&"environment:impl-paraformer_onnx_worker:slot-b".into())
    );
    assert!(!impl_space.slot_dir("slot-b").exists());
    assert!(engine_space.slot_dir("slot-a").exists());

    // 未知 implementation 空间 fail-closed 拒绝（不映射默认空间）
    let result = svc
        .cleanup_targets(
            &eid,
            &["environment:impl-unknown_impl:slot-a".to_string()],
            None,
        )
        .await
        .unwrap();
    assert!(
        result
            .skipped_target_ids
            .contains(&"environment:impl-unknown_impl:slot-a".into())
    );

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// install 的部署空间解析 fail-closed：引擎声明了 implementation 但契约
/// 模型未绑定 → install 拒绝（不猜测安装目标）。
#[tokio::test]
async fn install_fails_closed_when_contract_model_unbound() {
    let eid = EngineId::new("fake-unbound").unwrap();
    // 注册表声明了 implementation，但绑定表为空（fake-model 未绑定）
    let impl_registry = ImplementationRegistry::new_validated(
        vec![ImplementationDescriptor {
            id: ImplementationId::ParaformerOnnxWorker,
            engine_id: eid.clone(),
            runtime_kind: RuntimePlan::ManagedBinary,
            service_transport: ServiceTransport::Http,
            executor_topology: ExecutorTopology::ManagedWorker,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_ids: vec![ArtifactId::new("onnx-worker-test").unwrap()],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: vec!["other-model".to_string()],
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }],
        vec![],
    )
    .expect("测试 implementation 声明必须合法");

    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        "fake-unbound",
        true,
    )]));
    let svc = EngineManager::new_with_providers_and_implementations(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::empty(),
        Arc::new(super::super::model_installer::NoopModelWorker),
        impl_registry,
    );

    let err = svc.install(&eid, AdapterConfig::new()).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::InvalidConfig);
}

// ── 跨 runtime 模型切换事务与失败矩阵（Handoff 08）──────────────────────────

/// 内存 fake selected 存储（事务配置提交/回写端口）。
struct FakeSelectedStore {
    current: std::sync::Mutex<Option<String>>,
}

impl FakeSelectedStore {
    fn with_initial(model_id: &str) -> std::sync::Arc<Self> {
        std::sync::Arc::new(Self {
            current: std::sync::Mutex::new(Some(model_id.to_string())),
        })
    }

    fn selected(&self) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
}

#[async_trait::async_trait]
impl super::switch::SelectedModelStore for FakeSelectedStore {
    fn read_selected(&self) -> Option<String> {
        self.current.lock().unwrap().clone()
    }
    async fn commit_selected(&self, model_id: &str) -> Result<(), String> {
        *self.current.lock().unwrap() = Some(model_id.to_string());
        Ok(())
    }
}

/// 可承载两个模型的 fake ONNX worker implementation 注册表。
fn make_onnx_worker_impl_registry_two_models(engine_id: &EngineId) -> ImplementationRegistry {
    ImplementationRegistry::new_validated(
        vec![ImplementationDescriptor {
            id: ImplementationId::ParaformerOnnxWorker,
            engine_id: engine_id.clone(),
            runtime_kind: RuntimePlan::ManagedBinary,
            service_transport: ServiceTransport::Http,
            executor_topology: ExecutorTopology::ManagedWorker,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_ids: vec![ArtifactId::new("onnx-worker-test").unwrap()],
                compute_candidates: Vec::new(),
                schema_version: 1,
            },
            carried_models: vec!["fake-model".to_string(), "fake-model-2".to_string()],
            resource_budget: ResourceBudget::default(),
            timeouts: None,
        }],
        vec![
            ImplementationBinding {
                engine_id: engine_id.clone(),
                model_id: "fake-model".to_string(),
                implementation: ImplementationId::ParaformerOnnxWorker,
            },
            ImplementationBinding {
                engine_id: engine_id.clone(),
                model_id: "fake-model-2".to_string(),
                implementation: ImplementationId::ParaformerOnnxWorker,
            },
        ],
    )
    .expect("测试 implementation 声明必须合法")
}

/// 在指定部署空间写入带指定模型契约的完整 active 部署
/// （ONNX 扩展 + generation id，模拟 ParaformerOnline 安装产物）。
fn write_onnx_deployment(
    space: &DeploymentSpace,
    install_id: &str,
    model_id: &str,
    generation: &str,
) {
    use crate::infra::local_engine::deployment::DEPLOYMENT_POINTER_SCHEMA_VERSION;
    use crate::infra::local_engine::runtime::MANIFEST_SCHEMA_VERSION;

    let slot = "slot-a";
    std::fs::create_dir_all(space.slot_dir(slot)).unwrap();
    let manifest = crate::infra::local_engine::runtime::DeploymentManifest {
        schema_version: MANIFEST_SCHEMA_VERSION,
        engine_id: space.engine_id().clone(),
        runtime_kind: RuntimePlan::OnnxRuntime,
        install_id: install_id.to_string(),
        requested_preference: ComputePreference::Cpu,
        resolved_profile: ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("fake-artifact").unwrap(),
            priority: 0,
        },
        installed_at_ms: 0,
        artifact: crate::infra::local_engine::runtime::ArtifactIdentity {
            runtime_kind: RuntimePlan::OnnxRuntime,
            artifact_id: ArtifactId::new("fake-artifact").unwrap(),
            sha256: "cd".repeat(32),
        },
        model_contract: crate::infra::local_engine::runtime::ModelContract {
            model_id: model_id.to_string(),
            revision: "onnx-test".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
        },
        fallback_reasons: Vec::new(),
        extension: crate::infra::local_engine::runtime::ManifestExtension::OnnxRuntime(
            crate::infra::local_engine::runtime::OnnxRuntimeManifestExt {
                dll_artifact_id: ArtifactId::new("fake-artifact").unwrap(),
                dll_sha256: "cd".repeat(32),
                ort_version: "1.19.2".to_string(),
                dll_files: vec![],
                model_generation_id: generation.to_string(),
                execution_provider: "cpu".to_string(),
                inter_op: 1,
                intra_op: 4,
                self_test_passed: true,
            },
        ),
    };
    runtime::atomic_write_json(&space.slot_manifest_path(slot), &manifest).unwrap();
    DeploymentStore::write_pointer(
        space,
        &DeploymentPointer {
            install_id: install_id.to_string(),
            slot: slot.to_string(),
            updated_at_ms: 0,
            schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
        },
    )
    .unwrap();
}

fn onnx_impl_space(eid: &EngineId) -> DeploymentSpace {
    DeploymentSpace::resolve(eid, ImplementationId::ParaformerOnnxWorker)
}

/// 构造带注入 implementation 注册表 + 模型目录的测试 manager。
fn make_switch_manager(
    eid: &EngineId,
    impl_registry: ImplementationRegistry,
) -> Arc<EngineManager> {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        make_fake_adapter_with_options(eid.as_str(), true, false),
    ]));
    EngineManager::new_with_providers_and_implementations(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        make_model_registry(eid, "fake-model", "fake-model-2"),
        Arc::new(super::super::model_installer::NoopModelWorker),
        impl_registry,
    )
}

/// 注入 ONNX 实例 launch snapshot（active = model_id）。
async fn inject_onnx_launch(entry: &Arc<EngineEntry>, model_id: &str, instance_id: &str) {
    inject_launch(entry, model_id, instance_id).await;
    let mut l = entry.launch.lock().await;
    l.as_mut().unwrap().implementation = Some(ImplementationId::ParaformerOnnxWorker);
}

/// 未安装目标 → 事务在验证步失败（Target(ModelNotReady)），
/// 不停止实例、不提交 selected（事务第 2 步 fail-closed）。
#[tokio::test]
async fn switch_target_not_installed_fails_before_any_mutation() {
    let eid = EngineId::new("fake-switch-a").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry_two_models(&eid));
    let store = FakeSelectedStore::with_initial("fake-model");
    svc.set_selected_store(store.clone());

    // 运行中（注入 ONNX launch snapshot）
    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_onnx_launch(&entry, "fake-model", "inst-sw-a").await;

    // 目标 fake-model-2 未安装（impl 空间无部署）→ Target 失败
    let err = svc.switch_model(&eid, "fake-model-2").await.unwrap_err();
    match err {
        super::switch::SwitchModelFailure::Target(e) => {
            assert_eq!(e.code, LocalEngineErrorCode::ModelNotReady, "{e:?}");
        }
        other => panic!("应为目标失败，实际: {other:?}"),
    }

    // 引擎未被动：launch snapshot 仍在（未 stop）；selected 未变
    assert!(
        entry.current_launch().await.is_some(),
        "验证失败不应停止实例"
    );
    assert_eq!(store.selected().as_deref(), Some("fake-model"));
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// 引擎未运行 → 只提交 selected（CommittedSelectedOnly），不自动启动。
#[tokio::test]
async fn switch_when_engine_stopped_commits_selected_only() {
    let eid = EngineId::new("fake-switch-b").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry_two_models(&eid));
    let store = FakeSelectedStore::with_initial("fake-model");
    svc.set_selected_store(store.clone());

    // 目标已安装（impl 空间写入契约 fake-model-2 的部署）
    write_onnx_deployment(&onnx_impl_space(&eid), "dep-t2", "fake-model-2", "gen-2");

    let outcome = svc.switch_model(&eid, "fake-model-2").await.unwrap();
    assert!(
        matches!(
            outcome,
            super::switch::SwitchModelOutcome::CommittedSelectedOnly { .. }
        ),
        "{outcome:?}"
    );
    assert_eq!(store.selected().as_deref(), Some("fake-model-2"));
    // 引擎未被启动
    let status = svc.get_status(&eid).await.unwrap();
    assert_eq!(status.status.desired, DesiredState::Stopped);
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// 目标已是 active → 幂等（只同步 selected），实例保持运行。
#[tokio::test]
async fn switch_same_target_is_idempotent() {
    let eid = EngineId::new("fake-switch-c").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry_two_models(&eid));
    let store = FakeSelectedStore::with_initial("fake-model");
    svc.set_selected_store(store.clone());

    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_onnx_launch(&entry, "fake-model", "inst-sw-c").await;
    write_onnx_deployment(&onnx_impl_space(&eid), "dep-t3", "fake-model", "gen-3");

    let outcome = svc.switch_model(&eid, "fake-model").await.unwrap();
    assert!(
        matches!(outcome, super::switch::SwitchModelOutcome::Completed { .. }),
        "{outcome:?}"
    );
    assert_eq!(store.selected().as_deref(), Some("fake-model"));
    assert!(
        entry.current_launch().await.is_some(),
        "幂等切换不应停止实例"
    );
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// 目标 start 失败（fake exe 不存在）→ 回滚：恢复旧 selected，
/// 旧模型重启也失败 → RollbackFailed 双错误；active=None、desired=Stopped。
///
/// 覆盖 handoff 失败矩阵的"Ready 超时/worker early exit → start 失败"类
/// 与"回滚失败"分支（成功路径由真实 E2E 覆盖）。
#[tokio::test]
async fn switch_start_failure_with_failed_rollback_reports_both_errors() {
    let eid = EngineId::new("fake-switch-d").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry_two_models(&eid));
    let store = FakeSelectedStore::with_initial("fake-model");
    svc.set_selected_store(store.clone());

    // 运行中：active = fake-model（ONNX impl）
    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_onnx_launch(&entry, "fake-model", "inst-sw-d").await;

    // 目标已安装：契约 fake-model-2
    write_onnx_deployment(&onnx_impl_space(&eid), "dep-t4", "fake-model-2", "gen-4");

    // 事务：验证 ✓ → stop（幂等）→ commit B → start 失败 → 回滚重启也失败
    let err = svc.switch_model(&eid, "fake-model-2").await.unwrap_err();
    match err {
        super::switch::SwitchModelFailure::RollbackFailed {
            target_error,
            rollback_error,
        } => {
            assert_eq!(
                target_error.code,
                LocalEngineErrorCode::SpawnFailed,
                "fake exe 启动失败应为 SpawnFailed: {target_error:?}"
            );
            assert_eq!(rollback_error.code, LocalEngineErrorCode::SpawnFailed);
        }
        other => panic!("应为回滚也失败（双错误），实际: {other:?}"),
    }

    // selected 已恢复旧值；active=None；desired=Stopped
    assert_eq!(
        store.selected().as_deref(),
        Some("fake-model"),
        "回滚后 selected 应恢复旧模型"
    );
    let status = svc.get_status(&eid).await.unwrap();
    assert_eq!(status.status.desired, DesiredState::Stopped);
    assert_eq!(status.status.active_implementation, None);
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// selected store 未接线 → fail-closed（Target(InvalidConfig)）。
#[tokio::test]
async fn switch_without_selected_store_fails_closed() {
    let eid = EngineId::new("fake-switch-e").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry_two_models(&eid));

    let err = svc.switch_model(&eid, "fake-model").await.unwrap_err();
    match err {
        super::switch::SwitchModelFailure::Target(e) => {
            assert_eq!(e.code, LocalEngineErrorCode::InvalidConfig);
            assert!(e.detail.contains("selected store"), "{e:?}");
        }
        other => panic!("应为 store 未接线失败，实际: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// 未绑定 implementation 的模型（在目录但不在绑定表）→ fail-closed 不换模。
#[tokio::test]
async fn switch_unbound_model_fails_closed() {
    let eid = EngineId::new("fake-switch-f").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    // 注册表只绑定 fake-model；fake-model-2 在目录但未绑定
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry(&eid));
    let store = FakeSelectedStore::with_initial("fake-model");
    svc.set_selected_store(store);

    let err = svc.switch_model(&eid, "fake-model-2").await.unwrap_err();
    match err {
        super::switch::SwitchModelFailure::Target(e) => {
            assert_eq!(e.code, LocalEngineErrorCode::InvalidConfig, "{e:?}");
        }
        other => panic!("应为绑定缺失失败，实际: {other:?}"),
    }
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

/// get_connection 投影：ONNX 实例返回 streaming port + 冻结 implementation，
/// worker 通道为 None（与 GGUF 实例互斥）。
#[tokio::test]
async fn get_connection_projects_streaming_port_for_onnx_instance() {
    use crate::infra::local_engine::stream_worker_proto::StreamWorkerClient;
    use crate::infra::local_engine::streaming_stt_adapter::ParaformerOnlineAdapter;

    let eid = EngineId::new("fake-switch-g").unwrap();
    let svc = make_switch_manager(&eid, make_onnx_worker_impl_registry(&eid));

    let entry = svc.get_entry_internal(&eid).await.unwrap();
    inject_onnx_launch(&entry, "fake-model", "inst-sw-g").await;
    // 构造真实 StreamWorkerClient（tokio duplex 管道）+ 适配器
    let (client_side, _worker_side_in) = tokio::io::duplex(1024);
    let (_worker_side_out, host_side) = tokio::io::duplex(1024);
    let client = StreamWorkerClient::new(Box::new(client_side), Box::new(host_side));
    {
        let mut sp = entry.streaming_port.lock().await;
        *sp = Some(Arc::new(ParaformerOnlineAdapter::with_process(
            client,
            crate::infra::local_engine::process::ManagedProcess::with_defaults(),
        )));
    }

    let conn = svc
        .get_connection(&eid)
        .await
        .unwrap()
        .expect("运行中应有连接");
    assert_eq!(
        conn.implementation,
        Some(ImplementationId::ParaformerOnnxWorker)
    );
    assert!(conn.streaming.is_some(), "ONNX 实例应投影 streaming port");
    assert!(conn.worker.is_none(), "ONNX 实例无 GGUF worker 通道");
    assert!(
        conn.streaming.as_ref().unwrap().is_ready(),
        "duplex 管道未断开时应 ready"
    );

    // 销毁适配器后连接投影同步消失（stop/exit 路径的清理语义）
    {
        let mut sp = entry.streaming_port.lock().await;
        *sp = None;
    }
    let conn2 = svc.get_connection(&eid).await.unwrap().unwrap();
    assert!(conn2.streaming.is_none());
}

/// ParaformerOnline 模型状态投影：状态真源 = impl 部署空间
/// （generation 与 asset lock 一致 → Installed；不一致 → 不可选；
/// 无部署 → NotInstalled）。
#[tokio::test]
async fn paraformer_online_model_status_reflects_impl_deployment() {
    use crate::app::local_engine::funasr::paraformer_online;
    use crate::domain::local_engine::{ModelInstallState, ModelVerificationState};

    // 模型 descriptor 绑定 funasr 引擎——测试根目录已重定向到临时目录，
    // 使用真实 engine id 不会触碰用户资产
    let eid = EngineId::new("funasr").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
        eid.as_str(),
        true,
    )]));
    let svc = EngineManager::new_with_providers(
        registry,
        Arc::new(NoopEventPort),
        HashMap::new(),
        crate::infra::local_engine::providers::python::PythonVenvProvider::new(),
        super::super::model_installer::ModelRegistry::new_with_models(vec![
            paraformer_online::paraformer_online_model_descriptor(),
        ]),
        Arc::new(super::super::model_installer::NoopModelWorker),
    );

    // 未安装
    let models = svc.list_models(&eid).await;
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].install_state, ModelInstallState::NotInstalled);

    // 安装（generation 与 asset lock 一致）
    let generation = paraformer_online::expected_model_generation_id().unwrap();
    write_onnx_deployment(
        &onnx_impl_space(&eid),
        "dep-h",
        paraformer_online::PARAFORMER_ONLINE_ID,
        &generation,
    );
    let models = svc.list_models(&eid).await;
    assert_eq!(models[0].install_state, ModelInstallState::Installed);
    assert_eq!(
        models[0].verification_state,
        ModelVerificationState::Unverified
    );

    // generation 与 asset lock 不一致（上游更新）→ 不可用
    write_onnx_deployment(
        &onnx_impl_space(&eid),
        "dep-h2",
        paraformer_online::PARAFORMER_ONLINE_ID,
        "stale-generation",
    );
    let models = svc.list_models(&eid).await;
    assert_eq!(models[0].install_state, ModelInstallState::NotInstalled);
    assert_eq!(
        models[0].verification_state,
        ModelVerificationState::Corrupted
    );
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

// ── 0.22.9 回归：ensure_installed 的 implementation 种子解析 ────────────────

/// 复现 ensure_installed 的 implementation 解析链：引擎声明了 implementation
/// 且无用户级模型选择（paddleocr 的 selected 恒 None）时，种子模型必须
/// 退化到 descriptor 模型契约。修复前直接用 selected（None）解析，
/// fail-closed 报"无法解析 implementation"，OCR 无法启动。
#[tokio::test]
async fn ensure_seed_implementation_falls_back_to_contract_for_paddleocr() {
    let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
        crate::app::local_engine::paddleocr::make_paddleocr_adapter(),
    ]));
    let svc = EngineManager::new(registry, Arc::new(NoopEventPort));
    let eid =
        EngineId::new(crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID).unwrap();

    let resolved = svc
        .resolve_seed_implementation(&eid, &AdapterConfig::default())
        .await
        .expect("paddleocr 种子解析必须经 contract 兜底成功");
    assert_eq!(
        resolved,
        Some(ImplementationId::PaddleOcrOnnxInProcess),
        "paddleocr 必须解析到 ONNX in-process implementation"
    );
}

/// 种子模型 helper 纯逻辑：selected 优先；selected 为空视同未选择，
/// 回落 descriptor 模型契约（保证不因空字符串 fail-closed）。
#[test]
fn seed_model_id_prefers_selected_then_contract() {
    use super::lifecycle::seed_model_id_for_implementation;

    let funasr = EngineId::new(crate::app::local_engine::funasr::FUNASR_ENGINE_ID).unwrap();
    let contract = "gguf/sensevoice-small-q8";

    let selected = AdapterConfig {
        engine_config: serde_json::json!({ "funasr_model": "gguf/fun-asr-nano-q4km" }),
        ..Default::default()
    };
    assert_eq!(
        seed_model_id_for_implementation(&funasr, &selected, contract).as_deref(),
        Some("gguf/fun-asr-nano-q4km"),
        "selected 存在时优先用 selected"
    );

    let empty_selected = AdapterConfig {
        engine_config: serde_json::json!({ "funasr_model": "" }),
        ..Default::default()
    };
    assert_eq!(
        seed_model_id_for_implementation(&funasr, &empty_selected, contract).as_deref(),
        Some(contract),
        "空 selected 视同未选择，回落 contract"
    );

    // 非 funasr 引擎无用户级模型选择——恒回落 contract（paddleocr 场景）
    let paddleocr =
        EngineId::new(crate::app::local_engine::paddleocr::PADDLEOCR_ENGINE_ID).unwrap();
    assert_eq!(
        seed_model_id_for_implementation(&paddleocr, &AdapterConfig::default(), contract)
            .as_deref(),
        Some(contract)
    );
}
