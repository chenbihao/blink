use super::*;
use crate::domain::local_engine::*;
use crate::infra::local_engine::deployment::DeploymentPointer;
use crate::infra::local_engine::runtime::{ArtifactId, ComputePreference, RuntimePlan};
use std::collections::HashMap;
use std::sync::Arc;

// ── 基础辅助 ──────────────────────────────────────────────────────────────

fn make_fake_adapter(id: &str, self_test_passes: bool) -> Arc<dyn LocalEngineAdapter> {
    struct FakeAdapter {
        descriptor: EngineDefinition,
        self_test_passes: bool,
    }

    impl FakeAdapter {
        fn new(id: &str, self_test_passes: bool) -> Self {
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

    Arc::new(FakeAdapter::new(id, self_test_passes))
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
    let op = svc
        .coordinator()
        .active_operation(&eid)
        .expect("下载已进入，claim 必然已登记");

    // cancel：token 触发，但 claim 仍由 worker 持有
    let outcome = svc.cancel_operation(&eid, &op).await;
    assert!(outcome.is_cancelled(), "应成功发出取消信号: {outcome:?}");
    assert!(svc.coordinator().active_operation(&eid).is_some());

    // worker 收到取消信号退出（select cancelled 分支 → 成功取消路径）
    let result = install_task.await.unwrap().unwrap();
    assert!(result.success);
    assert_eq!(result.final_stage, ModelOperationStage::Cancelled);

    // worker 结束后 claim 释放——下一个操作可 claim
    assert!(svc.coordinator().active_operation(&eid).is_none());
    let guard = svc.coordinator().try_claim(&eid, "op-next").unwrap();
    guard.release();

    cleanup_models(&eid, &[&tag, &format!("{tag}-b")]).await;
}

// ── 变更互斥（必测并发场景）────────────────────────────────────────────

/// 模型安装与环境修复竞争：模型安装进行中，repair 必须被拒绝。
#[tokio::test]
async fn model_install_races_env_repair() {
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

    // repair 同引擎 → AlreadyRunning
    let err = svc.repair(&eid).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

    // start/stop 同样被互斥
    let err = svc.stop(&eid).await.unwrap_err();
    assert_eq!(err.code, LocalEngineErrorCode::AlreadyRunning);

    // 放行安装完成
    installer.release();
    let result = install_task.await.unwrap().unwrap();
    assert!(result.success);

    // 安装结束后 repair 可执行（self-test pass 降级路径）
    svc.repair(&eid).await.unwrap();

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

    // 造一个 active 指针 + slot
    std::fs::create_dir_all(runtime::slot_dir(&eid, "slot-a")).unwrap();
    DeploymentStore::write_pointer(
        &eid,
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
    assert!(runtime::slot_dir(&eid, "slot-a").exists());

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

    // active = slot-a；残留 slot-b + 孤儿 staging
    std::fs::create_dir_all(runtime::slot_dir(&eid, "slot-a")).unwrap();
    std::fs::create_dir_all(runtime::slot_dir(&eid, "slot-b")).unwrap();
    std::fs::write(runtime::slot_dir(&eid, "slot-b").join("data.bin"), b"x").unwrap();
    std::fs::create_dir_all(runtime::operation_staging_dir(&eid, "op-orphan")).unwrap();
    DeploymentStore::write_pointer(
        &eid,
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
    assert!(!runtime::slot_dir(&eid, "slot-b").exists());
    assert!(runtime::slot_dir(&eid, "slot-a").exists(), "active 不可删");

    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
}

#[tokio::test]
async fn scan_storage_targets_no_full_paths() {
    let svc = make_service("fake-scan-paths");
    let eid = EngineId::new("fake-scan-paths").unwrap();
    let _ = std::fs::remove_dir_all(runtime::engine_root(&eid));
    std::fs::create_dir_all(runtime::slot_dir(&eid, "slot-a")).unwrap();
    DeploymentStore::write_pointer(
        &eid,
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
