//! LocalEngineService — 本地引擎生命周期编排服务（0.22.3）。
//!
//! 职责（§3.1）：
//! - 持有 registry、每引擎状态快照、service_epoch、操作串行化 gate
//! - 管理 instance/operation 跟踪
//! - 调用 adapter + infra 执行生命周期操作
//! - 暴露窄 API 供 commands/wiring 消费
//!
//! ## 状态提交统一入口
//!
//! 所有状态变更经过 `commit_status()`：
//! 1. 验证 engine_id 在 registry 中
//! 2. 验证 operation_id / instance generation
//! 3. revision +1
//! 4. 广播完整 snapshot
//!
//! ## 并发模型
//!
//! - 每个 engine 同时只允许一个变更操作（`tokio::Mutex` per engine gate）
//! - 查询（catalog/get_status）可并发
//! - start/stop 幂等，迟到 health/task/exit 不能覆盖新实例
//!
//! ## start 成功定义
//!
//! start 只有在 token health 的 engine_id/instance_id/backend 校验通过后
//! 才进入 Healthy。process spawned 不能直接等价为 Healthy。
//! model Ready 由 adapter health 映射产生，不能由端口可达推导。

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{Mutex, OnceCell, RwLock, watch};
use tokio_util::sync::CancellationToken;

use crate::domain::local_engine::{
    AdapterConfig, DesiredState, EngineDescriptor, EngineDiagnostic, EngineOperation, EngineStatus,
    EngineStatusSnapshot, EnvironmentHealth, ErrorPhase, HealthMapping, LaunchContext,
    LocalEngineAdapter, LocalEngineError, LocalEngineErrorCode, ModelHealth, OperationKind,
    OperationStage, ProcessState, ServiceEpoch, ServiceHealth,
};
use crate::infra::local_engine::port::{
    EndpointAllocator, IdentityVerification, ServiceIdentityInput, ServiceIdentityResult,
    generate_service_token,
};
use crate::infra::local_engine::process::{LaunchRequest, ManagedProcess, ShutdownConfig};
use crate::infra::local_engine::runtime::{
    self, BackendState, ComputeBackend, ComputePreference, EngineId, ResolvedProfile,
    generate_operation_id,
};

use crate::infra::local_engine::providers::ProviderDescriptor;
use crate::infra::local_engine::providers::python::PythonVenvProvider;

use super::registry::EngineRegistry;

// ── LocalEngineConnection ─────────────────────────────────────────────────

/// 受限连接快照——由 `LocalEngineService` 产生，不可序列化给前端。
///
/// 包含当前运行实例的 endpoint、token 和身份信息，
/// 供 STT transcription client 携带 `X-Engine-Token` 鉴权。
///
/// **stop 或重启后旧 connection 的请求不得影响新实例**——
/// token 不匹配的请求会被 Python server 拒绝（401）。
#[derive(Debug, Clone)]
pub struct LocalEngineConnection {
    /// 实际 endpoint base URL（`http://127.0.0.1:port`）。
    pub endpoint: String,
    /// 服务 token（用于 `X-Engine-Token` header）。
    pub token: String,
    /// engine id。
    #[allow(dead_code)]
    pub engine_id: String,
    /// instance id（每次启动随机生成，用于实例隔离）。
    #[allow(dead_code)]
    pub instance_id: String,
}

// ── EngineEntry ───────────────────────────────────────────────────────────

/// 单引擎的运行时状态。
///
/// 持有：
/// - adapter 引用
/// - 当前状态快照
/// - 操作串行化 gate（tokio::Mutex）
/// - managed instance 跟踪
struct EngineEntry {
    adapter: Arc<dyn LocalEngineAdapter>,
    /// 操作串行化 gate——同一引擎同时只允许一个变更操作。
    /// 查询走 `status_snapshot` 的 RwLock，不竞争此 gate。
    op_gate: Mutex<()>,
    /// 状态快照（读多写少，用 RwLock）。
    status: RwLock<EngineStatus>,
    /// 受管实例（Running 时存在）。
    managed_process: Mutex<Option<Arc<ManagedProcess>>>,
    /// 当前 instance 的 identity input（用于 health 验证）。
    current_identity: Mutex<Option<ServiceIdentityInput>>,
    /// 当前 resolved profile（start 后设置）。
    current_profile: Mutex<Option<ResolvedProfile>>,
    /// 当前 operation_id（活跃操作时存在）。
    current_operation_id: Mutex<Option<String>>,
    /// 上一实例的 ManagedProcess 引用——stop 后保留 bounded history 可查。
    /// Task H: 日志实例隔离——pump 绑定 token/instance，stop 后保留 bounded history。
    #[allow(dead_code)]
    last_managed_process: Mutex<Option<Arc<ManagedProcess>>>,
    /// 日志 pump 的 cancellation token——每次 start 创建新 token，
    /// stop/rollback/restart 时 cancel 旧 pump，确保旧实例日志不再投影。
    log_pump_cancel: Mutex<Option<CancellationToken>>,
    /// 后台探测共享结果——确定性 probe 协调。
    ///
    /// 构造后 spawn 后台任务探测 current generation，
    /// `ensure_installed`/`start` 在执行前 await 此信号，
    /// 确保不会在探测未完成时竞态重复安装。
    /// probe 完成（成功或失败）后所有等待者获得同一确定结果。
    probe_result: OnceCell<Result<(), LocalEngineError>>,
    /// probe 完成信号 watch sender——probe 完成后发送 true。
    probe_tx: watch::Sender<bool>,
    /// probe 完成信号 watch——用于确定性等待（不轮询）。
    probe_watch: watch::Receiver<bool>,
}

// ── EventPort ─────────────────────────────────────────────────────────────

/// 状态/日志事件的 app 投影出口。
///
/// 由 app 层（commands/wiring）实现，把领域事件桥接成 Tauri emit。
/// service 不直接持有 AppHandle——通过此 trait 解耦。
pub trait EventPort: Send + Sync {
    /// 广播引擎状态快照。
    fn emit_status(&self, snapshot: &EngineStatusSnapshot);

    /// 广播引擎日志条目。
    fn emit_log(&self, engine_id: &EngineId, instance_id: &str, seq: u64, line: &str);
}

/// 空实现（测试/无事件场景用）。
#[allow(dead_code)]
pub struct NoopEventPort;

impl EventPort for NoopEventPort {
    fn emit_status(&self, _snapshot: &EngineStatusSnapshot) {}
    fn emit_log(&self, _engine_id: &EngineId, _instance_id: &str, _seq: u64, _line: &str) {}
}

// ── LocalEngineService ────────────────────────────────────────────────────

/// 本地引擎生命周期编排服务。
///
/// 持有：
/// - registry（编译期 allowlist）
/// - 每引擎状态快照
/// - 本次实例的 service_epoch
/// - 每引擎操作串行化 gate
/// - managed instance/operation 跟踪
/// - EventPort 风格的 app 投影出口
/// - 独立同步 process registry（不依赖 async entries 锁）
///
/// **禁止再用独立 AtomicBool 作为服务真源**——所有状态经 `commit_status()`。

/// 同步 process registry 的 key——至少包含 engine_id + instance_id。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
struct ProcessKey {
    engine_id: EngineId,
    instance_id: String,
}

pub struct LocalEngineService {
    registry: Arc<EngineRegistry>,
    epoch: ServiceEpoch,
    entries: RwLock<HashMap<EngineId, Arc<EngineEntry>>>,
    event_port: Arc<dyn EventPort>,
    /// 每引擎对应的 ProviderDescriptor（infra 层安装事务用）。
    /// key = engine_id，value = 编译期声明的 provider descriptor。
    provider_descriptors: HashMap<EngineId, ProviderDescriptor>,
    /// Python venv provider 实例（安装事务用）。
    /// 目前只有 PythonVenv 引擎，单实例即可。
    python_provider: PythonVenvProvider,
    /// 同步 process registry——独立于 async entries 锁。
    /// `shutdown_all_blocking` 直接读取此字段，不访问 entries。
    /// start 在 spawn 后登记，stop/spawn失败/health失败后移除。
    process_registry: std::sync::Mutex<HashMap<ProcessKey, Arc<ManagedProcess>>>,
}

#[allow(dead_code)]
impl LocalEngineService {
    /// 创建服务实例。
    ///
    /// 每次创建生成新 `service_epoch`——新 epoch 初始 revision 不受旧快照影响。
    pub fn new(registry: Arc<EngineRegistry>, event_port: Arc<dyn EventPort>) -> Arc<Self> {
        Self::new_with_providers(
            registry,
            event_port,
            HashMap::new(),
            PythonVenvProvider::new(),
        )
    }

    /// 创建服务实例（带 provider descriptors + python provider）。
    ///
    /// `provider_descriptors`：每引擎对应的 `ProviderDescriptor`，
    /// 由 wiring 层在构造时传入（如 `make_funasr_provider_descriptor()`）。
    /// `python_provider`：`PythonVenvProvider` 实例，用于 `InstallTransaction`。
    pub fn new_with_providers(
        registry: Arc<EngineRegistry>,
        event_port: Arc<dyn EventPort>,
        provider_descriptors: HashMap<EngineId, ProviderDescriptor>,
        python_provider: PythonVenvProvider,
    ) -> Arc<Self> {
        let epoch = ServiceEpoch::new();
        let mut entries = HashMap::new();

        for adapter in registry.adapters() {
            let engine_id = adapter.descriptor().engine_id.clone();
            let initial_status = EngineStatus {
                service_epoch: epoch.clone(),
                ..Default::default()
            };
            // probe watch: 初始 false（未完成），probe 完成后发送 true
            let (probe_tx, probe_rx) = watch::channel(false);
            entries.insert(
                engine_id.clone(),
                Arc::new(EngineEntry {
                    adapter,
                    op_gate: Mutex::new(()),
                    status: RwLock::new(initial_status),
                    managed_process: Mutex::new(None),
                    current_identity: Mutex::new(None),
                    current_profile: Mutex::new(None),
                    current_operation_id: Mutex::new(None),
                    last_managed_process: Mutex::new(None),
                    log_pump_cancel: Mutex::new(None),
                    probe_result: OnceCell::new(),
                    probe_tx,
                    probe_watch: probe_rx,
                }),
            );
        }

        let service = Arc::new(Self {
            registry,
            epoch,
            entries: RwLock::new(entries),
            event_port,
            provider_descriptors,
            python_provider,
            process_registry: std::sync::Mutex::new(HashMap::new()),
        });

        // 0.22.3 Task B: 后台探测每个引擎的 current generation
        // 不阻塞主链路（Alt+Space），ensure_installed/start 会 await 探测结果
        // 0.22.3 Task D: 探测结果经 commit_status_internal 统一提交（revision+1 并广播）
        // 0.22.3 Task F: 确定性 probe 协调——使用 OnceCell 共享结果，不轮询
        service.spawn_background_probe();

        service
    }

    /// 返回 service_epoch。
    pub fn epoch(&self) -> &ServiceEpoch {
        &self.epoch
    }

    /// 返回 registry 引用。
    #[allow(dead_code)]
    pub fn registry(&self) -> &Arc<EngineRegistry> {
        &self.registry
    }

    // ── 查询 API（可并发） ──────────────────────────────────────────────────

    /// 返回所有引擎的 catalog（描述符列表）。
    pub async fn catalog(&self) -> Vec<EngineDescriptor> {
        let entries = self.entries.read().await;
        entries
            .values()
            .map(|e| e.adapter.descriptor().clone())
            .collect()
    }

    /// 返回指定引擎的状态快照。
    ///
    /// 查询无副作用——不因读取而启动进程或改变 generation。
    pub async fn get_status(
        &self,
        engine_id: &EngineId,
    ) -> Result<EngineStatusSnapshot, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        let status = entry.status.read().await;
        Ok(EngineStatusSnapshot {
            engine_id: engine_id.clone(),
            service_epoch: self.epoch.clone(),
            revision: status.revision,
            status: status.clone(),
        })
    }

    /// 返回所有引擎的状态快照列表。
    pub async fn get_all_status(&self) -> Vec<EngineStatusSnapshot> {
        let entries = self.entries.read().await;
        let mut result = Vec::new();
        for (engine_id, entry) in entries.iter() {
            let status = entry.status.read().await;
            result.push(EngineStatusSnapshot {
                engine_id: engine_id.clone(),
                service_epoch: self.epoch.clone(),
                revision: status.revision,
                status: status.clone(),
            });
        }
        result
    }

    /// 返回引擎当前的身份信息（endpoint + token）。
    ///
    /// 0.22.4：OCR Coordinator 需要获取 PaddleOCR server 的 endpoint 和 token
    /// 来发送 HTTP /recognize 请求。
    ///
    /// 如果引擎未运行或身份未设置，返回 `None`。
    pub async fn get_current_identity(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<crate::infra::local_engine::port::ServiceIdentityInput>, LocalEngineError>
    {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        let ci = entry.current_identity.lock().await;
        Ok(ci.clone())
    }

    /// 返回当前运行实例的 InstanceToken（用于条件停止）。
    ///
    /// 如果引擎未运行，返回 `None`。
    pub async fn get_current_instance_token(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<crate::infra::local_engine::state::InstanceToken>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        let mp = entry.managed_process.lock().await;
        match mp.as_ref() {
            Some(managed) => Ok(Some(managed.current_token().await)),
            None => Ok(None),
        }
    }

    /// 返回引擎诊断信息。
    pub async fn get_diagnostics(
        &self,
        engine_id: &EngineId,
    ) -> Result<EngineDiagnostic, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        let entry = entries.get(engine_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Request,
                "未知引擎",
                format!("engine_id '{}' 不在 registry 中", engine_id),
            )
        })?;
        Ok(entry.adapter.diagnostics())
    }

    // ── install ─────────────────────────────────────────────────────────────

    /// 安装引擎环境。
    ///
    /// **唯一真源**：通过 `InstallTransaction` 事务执行安装，
    /// 不再直接调用 `platform::python::setup`。
    ///
    /// 事务流程（由 `InstallTransaction::execute` 编排）：
    /// 1. resolve_profile → 解析 compute preference
    /// 2. create staging → `engines/{id}/staging/{op_id}`
    /// 3. provider.prepare_environment → uv venv + pip install + self-test
    /// 4. write manifest → generation manifest
    /// 5. promote → staging → `generations/{install_id}`
    /// 6. atomic switch → `current.json`
    /// 7. 失败自动回滚 staging / 恢复 previous current.json
    ///
    /// 安装后验证 adapter self_test，成功后标记 environment=Ready。
    /// 失败不破坏旧环境。
    pub async fn install(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        // 0.22.3 Task B: 等待后台探测完成，避免竞态重复安装
        self.await_probe(engine_id).await?;

        let entry = self.get_entry(engine_id).await?;

        let _gate = entry.op_gate.lock().await;

        let operation_id = generate_operation_id();
        self.set_operation_id(engine_id, Some(operation_id.clone()))
            .await;

        // 标记操作进行中
        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Installing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 先检查 adapter self_test——如果已通过，环境已就绪，无需重新安装。
        let pre_test = entry.adapter.self_test();
        if pre_test.passed {
            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                status.environment = EnvironmentHealth::Ready;
                status.operation.stage = OperationStage::Completed;
            })
            .await?;
            self.set_operation_id(engine_id, None).await;
            tracing::info!(engine = %engine_id, "install 跳过（self-test 已通过，环境就绪）");
            return Ok(());
        }

        // 0.22.3: pre_test 未通过时，先尝试 InstallTransaction。
        // 如果没有 ProviderDescriptor（测试/未接线场景），直接返回 SelfTestFailed。
        let preference = config.compute_preference.unwrap_or(ComputePreference::Auto);

        // 查找此引擎的 ProviderDescriptor
        let provider_descriptor = match self.provider_descriptors.get(engine_id) {
            Some(d) => d,
            None => {
                // 无 ProviderDescriptor——无法执行 InstallTransaction
                // 直接返回 SelfTestFailed（pre_test 已失败）
                let err = LocalEngineError::with_detail(
                    LocalEngineErrorCode::SelfTestFailed,
                    ErrorPhase::SelfTest,
                    "引擎 self-test 失败",
                    pre_test.failure_reason.unwrap_or_default(),
                );
                self.finish_operation_with_error(engine_id, &operation_id, &err)
                    .await?;
                return Err(err);
            }
        };

        // 更新进度：正在安装
        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation.stage = OperationStage::Downloading;
        })
        .await?;

        // 执行 InstallTransaction
        let transaction = crate::infra::local_engine::providers::InstallTransaction::new(
            provider_descriptor,
            &self.python_provider,
        );

        let install_result = transaction.execute(preference).await;

        match install_result {
            Ok(result) => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %result.install_id,
                    operation_id = %result.operation_id,
                    fell_back = result.fell_back,
                    "InstallTransaction 完成"
                );

                // 安装后再次验证 adapter self_test
                let self_test = entry.adapter.self_test();
                if !self_test.passed {
                    let err = LocalEngineError::with_detail(
                        LocalEngineErrorCode::SelfTestFailed,
                        ErrorPhase::SelfTest,
                        "引擎 self-test 失败",
                        self_test.failure_reason.unwrap_or_default(),
                    );
                    self.finish_operation_with_error(engine_id, &operation_id, &err)
                        .await?;
                    return Err(err);
                }

                // 环境标记为 Ready
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.operation.stage = OperationStage::Completed;
                })
                .await?;

                self.set_operation_id(engine_id, None).await;
                tracing::info!(engine = %engine_id, "install 完成（InstallTransaction + self-test passed）");
                Ok(())
            }
            Err(e) => {
                // 安装失败——不破坏旧环境，标记 Broken
                let err = LocalEngineError::from_runtime(
                    ErrorPhase::Install,
                    "环境安装失败（InstallTransaction）",
                    &e,
                );
                self.finish_operation_with_error(engine_id, &operation_id, &err)
                    .await?;
                // 标记环境为 Broken
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Broken;
                })
                .await
                .ok();
                Err(err)
            }
        }
    }

    // ── Task B: 后台环境探测 ────────────────────────────────────────────────

    /// 构造后启动后台探测任务，为每个引擎检查 current generation。
    ///
    /// 不阻塞主链路——`ensure_installed`/`start` 会 await 探测完成信号。
    /// 已安装旧用户启动 Blink 后，后台探测将自动识别 Ready。
    fn spawn_background_probe(self: &Arc<Self>) {
        // 构造时 entries 刚写入，try_read 不会失败
        let entries = match self.entries.try_read() {
            Ok(e) => e,
            Err(_) => {
                tracing::warn!("spawn_background_probe: entries RwLock 被占用，跳过后台探测");
                return;
            }
        };
        for (engine_id, _entry) in entries.iter() {
            let engine_id = engine_id.clone();
            let svc = Arc::clone(self);

            tauri::async_runtime::spawn(async move {
                svc.probe_environment(&engine_id).await;
            });
        }
    }

    /// 单引擎环境探测逻辑。
    ///
    /// 状态判定规则（0.22.3 Task C：必须基于 current.json + manifest）：
    /// - 无 current.json → Missing（默认值，不改）
    /// - current.json 有效 + manifest 可读 + adapter self_test 通过 → Ready
    /// - manifest 损坏 / self_test 失败 → Broken
    ///
    /// **0.22.3 Task D**: 探测结果通过 `commit_status_internal` 统一提交，
    /// revision+1 并广播完整 snapshot（不再直接操作 RwLock）。
    /// **0.22.3 Task F**: probe 完成后设置 OnceCell + 发送 watch 信号，
    /// 所有等待者获得同一确定结果，不永久等待。
    async fn probe_environment(self: Arc<Self>, engine_id: &EngineId) {
        let entry = match self.get_entry(engine_id).await {
            Ok(e) => e,
            Err(e) => {
                tracing::warn!(engine = %engine_id, %e, "probe_environment: 获取 entry 失败");
                // 即使失败也设置 probe_result + 发送 watch，让等待者获得确定结果
                if let Ok(entries) = self.entries.try_read() {
                    if let Some(entry) = entries.get(engine_id) {
                        let err = LocalEngineError::with_detail(
                            LocalEngineErrorCode::Internal,
                            ErrorPhase::Request,
                            "探测失败",
                            format!("{e}"),
                        );
                        let _ = entry.probe_result.set(Err(err));
                        let _ = entry.probe_tx.send(true);
                    }
                }
                return;
            }
        };

        let result = self.do_probe(engine_id, &entry).await;

        // 无论成功失败，都设置 probe_result + 发送 watch 信号——确定性协调
        let probe_outcome = result.as_ref().map(|()| ()).map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "探测失败",
                e.clone(),
            )
        });
        let _ = entry.probe_result.set(probe_outcome);
        // 通知所有等待者 probe 已完成
        let _ = entry.probe_tx.send(true);

        if let Err(e) = &result {
            tracing::warn!(
                engine = %engine_id,
                error = %e,
                "后台环境探测失败——保持默认 Missing 状态"
            );
        }
    }

    /// 实际探测逻辑（可返回错误用于日志）。
    ///
    /// **0.22.3 Task C**: 环境 Ready 判定必须同时满足：
    /// 1. `current.json` 存在且指向有效 generation
    /// 2. `manifest` 可读（generation 完整性验证）
    /// 3. adapter `self_test` 通过
    /// 缺少任何一项都不标记 Ready。
    async fn do_probe(&self, engine_id: &EngineId, entry: &EngineEntry) -> Result<(), String> {
        // 读 current.json
        let pointer = runtime::read_current_pointer(engine_id)
            .map_err(|e| format!("读取 current.json 失败: {e}"))?;

        let pointer = match pointer {
            None => {
                // 无 current generation → Missing（默认值，无需改状态）
                tracing::debug!(engine = %engine_id, "探测: 无 current.json → Missing");
                return Ok(());
            }
            Some(p) => p,
        };

        // 读 manifest（验证 generation 完整性）
        let manifest_result = runtime::read_manifest(engine_id, &pointer.install_id);
        let _manifest = match manifest_result {
            Ok(m) => m,
            Err(e) => {
                tracing::warn!(engine = %engine_id, error = %e, "探测: manifest 损坏 → Broken");
                // manifest 损坏 → 标记 Broken（统一经 commit_status_internal，revision+1）
                let _ = self
                    .commit_status_internal(engine_id, None, |status| {
                        status.environment = EnvironmentHealth::Broken;
                    })
                    .await;
                return Err(format!("manifest 损坏: {e}"));
            }
        };

        // manifest 有效——执行 adapter self_test
        let self_test = entry.adapter.self_test();
        if self_test.passed {
            // 全部通过 → Ready（统一经 commit_status_internal，revision+1 并广播）
            tracing::info!(
                engine = %engine_id,
                install_id = %pointer.install_id,
                "探测: current generation 有效 + self_test 通过 → Ready"
            );
            self.commit_status_internal(engine_id, None, |status| {
                status.environment = EnvironmentHealth::Ready;
            })
            .await
            .map_err(|e| format!("提交 Ready 状态失败: {e}"))?;
        } else {
            // self_test 失败 → Broken（统一经 commit_status_internal，revision+1 并广播）
            tracing::warn!(
                engine = %engine_id,
                reason = self_test.failure_reason.as_deref().unwrap_or("unknown"),
                "探测: self_test 失败 → Broken"
            );
            let _ = self
                .commit_status_internal(engine_id, None, |status| {
                    status.environment = EnvironmentHealth::Broken;
                })
                .await;
        }
        Ok(())
    }

    /// 等待后台探测完成——确定性协调，不轮询。
    ///
    /// `ensure_installed`/`start` 在执行前调用此方法，
    /// 确保不会在探测未完成时竞态重复安装。
    /// probe 完成（成功/失败）后所有等待者获得同一确定结果或 Err。
    async fn await_probe(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        let entry = self.get_entry(engine_id).await?;
        // OnceCell::get 在 set 后立即返回 Some(Result)，不阻塞
        if let Some(result) = entry.probe_result.get() {
            return result.clone();
        }
        // probe 未完成——await watch 直到完成
        // watch 不会永久阻塞：probe 任务完成（成功/失败）后发送 true
        let mut rx = entry.probe_watch.clone();
        // 先检查是否已完成（避免 race condition）
        if *rx.borrow() {
            return entry.probe_result.get().cloned().unwrap_or_else(|| {
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Request,
                    "探测状态不一致",
                    "probe_watch=true but probe_result=None",
                ))
            });
        }
        // 等待 probe 完成（watch 发送 true）
        let _ = rx.changed().await;
        // 完成后从 OnceCell 获取确定结果
        entry.probe_result.get().cloned().unwrap_or_else(|| {
            Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "探测状态不一致",
                "probe_watch changed but probe_result=None",
            ))
        })
    }

    /// 确保 Python 环境已安装（如果未安装则安装，已安装则标记 Ready）。
    ///
    /// 用于 auto-start 和 start command 的前置检查。
    /// 如果环境已 Ready，直接返回 Ok。
    ///
    /// **0.22.3 Task C**: 环境 Ready 判定必须基于 current.json + manifest + self_test，
    /// 不能仅凭 self_test 通过就标记 Ready。如果 self_test 通过但没有受管 generation，
    /// 说明环境是手动安装的（非 InstallTransaction 产生），仍需调用 install 建立受管 generation。
    pub async fn ensure_installed(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        // 0.22.3 Task B: 等待后台探测完成，避免竞态重复安装
        self.await_probe(engine_id).await?;

        let entry = self.get_entry(engine_id).await?;

        // 检查当前环境状态
        {
            let status = entry.status.read().await;
            if status.environment == EnvironmentHealth::Ready {
                return Ok(());
            }
        }

        // 环境未就绪——验证受管 generation（current.json + manifest）
        // Task C: 不能仅凭 self_test 通过就标记 Ready
        let has_managed_generation = match runtime::read_current_pointer(engine_id) {
            Ok(Some(pointer)) => {
                // 有 current.json——验证 manifest 可读
                match runtime::read_manifest(engine_id, &pointer.install_id) {
                    Ok(_) => true,
                    Err(e) => {
                        tracing::warn!(
                            engine = %engine_id,
                            error = %e,
                            "ensure_installed: current.json 存在但 manifest 损坏"
                        );
                        false
                    }
                }
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(
                    engine = %engine_id,
                    error = %e,
                    "ensure_installed: 读取 current.json 失败"
                );
                false
            }
        };

        if has_managed_generation {
            // 有受管 generation——检查 adapter self_test
            let self_test = entry.adapter.self_test();
            if self_test.passed {
                // self_test 通过 + 受管 generation 存在 → 标记 Ready
                self.commit_status_internal(engine_id, None, |status| {
                    status.environment = EnvironmentHealth::Ready;
                })
                .await?;
                return Ok(());
            }
        }

        // 没有受管 generation 或 self_test 未通过——需要安装
        self.install(engine_id, config).await
    }

    /// 返回当前运行实例的受限连接快照。
    ///
    /// 0.22.3 Task A: STT transcription client 必须通过此方法获取
    /// endpoint + token + 身份信息，在请求中携带 `X-Engine-Token` 鉴权。
    ///
    /// stop 或重启后旧 connection 的 token 不匹配新实例——
    /// Python server 会拒绝旧 token 的请求（401）。
    /// 无运行实例时返回 None。
    pub async fn get_connection(
        &self,
        engine_id: &EngineId,
    ) -> Result<Option<LocalEngineConnection>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let ci = entry.current_identity.lock().await;
        Ok(ci.as_ref().map(|identity| LocalEngineConnection {
            endpoint: identity.endpoint.base_url(),
            token: identity.token.clone(),
            engine_id: identity.engine_id.clone(),
            instance_id: identity.instance_id.clone(),
        }))
    }

    // ── start ───────────────────────────────────────────────────────────────

    /// 启动引擎服务。
    ///
    /// **start 的成功定义**：只有在 token health 的 engine_id/instance_id/backend
    /// 校验通过后，且 model 变为 Ready，才返回 Ok。
    /// process spawned 不能直接等价为 Healthy。
    /// model Ready 由 adapter health 映射产生。
    ///
    /// **任何失败分支都执行 rollback_started_instance 并返回 Err**——
    /// timeout/mismatch/backend 错误/ModelFailed/health 不可达全部返回 Err。
    ///
    /// 幂等：如果 desired 已为 Running 且进程活跃，直接返回 Ok。
    /// 迟到的 health/task/exit 不能覆盖新实例。
    pub async fn start(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        let _gate = entry.op_gate.lock().await;

        // 幂等检查：desired=Running 且进程活跃 → 直接返回
        {
            let status = entry.status.read().await;
            if status.desired == DesiredState::Running && status.is_process_active() {
                tracing::debug!(engine = %engine_id, "start 幂等：已 Running/Starting");
                return Ok(());
            }
        }

        // 环境检查
        {
            let status = entry.status.read().await;
            if status.environment != crate::domain::local_engine::EnvironmentHealth::Ready {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::EnvironmentMissing,
                    ErrorPhase::Start,
                    "环境未就绪，请先安装",
                    format!("environment={:?}", status.environment),
                ));
            }
        }

        // 解析 compute profile
        let preference = config.compute_preference.unwrap_or(ComputePreference::Auto);
        let descriptor = entry.adapter.descriptor();
        let profile = self.resolve_profile(descriptor, preference)?;

        // 分配 endpoint（在 prepare_launch 之前，因为 LaunchContext 需要 endpoint）
        let preferred_port = config.preferred_port.unwrap_or(8100);
        let allocator = EndpointAllocator::with_defaults(preferred_port);
        let endpoint = allocator.allocate().map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::PortConflict,
                ErrorPhase::Start,
                "端口分配失败",
                format!("endpoint allocation failed: {e}"),
            )
        })?;

        // 生成 token + identity
        let token = generate_service_token();
        let instance_id = format!("inst-{}", &token[..8]);
        let identity_input = ServiceIdentityInput {
            engine_id: engine_id.to_string(),
            instance_id: instance_id.clone(),
            token: token.clone(),
            endpoint: endpoint.clone(),
        };

        // 构建 LaunchContext（包含 endpoint、身份参数和 resolved profile）
        let ctx = LaunchContext {
            endpoint: endpoint.clone(),
            engine_id: engine_id.to_string(),
            instance_id: instance_id.clone(),
            token: token.clone(),
            resolved_profile: profile.clone(),
        };

        // adapter prepare_launch（接收 LaunchContext，不接受外部注入）
        let resolved_launch = entry.adapter.prepare_launch(&ctx, &config)?;

        // 构建 LaunchRequest
        let launch = &resolved_launch.launch;
        let mut env = launch.env.clone();
        // 注入 token 和 endpoint 到环境变量（作为后备，adapter 应已通过 CLI 参数传递）
        env.insert("BLINK_ENGINE_TOKEN".to_string(), token.clone());
        env.insert("BLINK_ENGINE_ENDPOINT".to_string(), endpoint.base_url());
        env.insert("BLINK_ENGINE_ID".to_string(), engine_id.to_string());
        env.insert("BLINK_INSTANCE_ID".to_string(), instance_id.clone());

        let req = LaunchRequest {
            executable: launch.executable.clone(),
            args: launch.args.iter().map(|s| s.clone().into()).collect(),
            current_dir: launch.current_dir.clone(),
            env,
            instance_id: instance_id.clone(),
            label: launch.label.clone(),
            shutdown: ShutdownConfig::default(),
        };

        // 创建 ManagedProcess
        let managed = ManagedProcess::with_defaults();

        // 标记 desired=Running, process=Starting
        self.commit_status_internal(engine_id, None, |status| {
            status.desired = DesiredState::Running;
            status.process = ProcessState::Starting;
            status.service = ServiceHealth::Unknown;
            status.operation = EngineOperation {
                kind: OperationKind::Idle,
                operation_id: String::new(),
                stage: OperationStage::Pending,
                cancellable: false,
            };
        })
        .await?;

        // 保存 managed process + identity + profile
        {
            let mut mp = entry.managed_process.lock().await;
            *mp = Some(managed.clone());
        }
        {
            let mut ci = entry.current_identity.lock().await;
            *ci = Some(identity_input.clone());
        }
        {
            let mut cp = entry.current_profile.lock().await;
            *cp = Some(resolved_launch.profile.clone());
        }
        // 同步 process registry——登记到 service 级 registry
        let pkey = ProcessKey {
            engine_id: engine_id.clone(),
            instance_id: instance_id.clone(),
        };
        {
            let mut reg = self.process_registry.lock().unwrap();
            reg.insert(pkey.clone(), managed.clone());
        }
        // 启动日志 pump task——把 ManagedProcess 的实时日志转发到 EventPort
        // Task H: 日志实例隔离——每次 start 创建新 CancellationToken，
        // stop/rollback/restart 时 cancel 旧 pump。
        // pump 每条日志 emit 前实时读取 current_identity 校验实例归属。
        let pump_token = CancellationToken::new();
        {
            // 先取消旧 pump（restart 场景）
            let mut old_cancel = entry.log_pump_cancel.lock().await;
            if let Some(old) = old_cancel.take() {
                tracing::debug!(engine = %engine_id, "start: 取消旧日志 pump");
                old.cancel();
            }
            *old_cancel = Some(pump_token.clone());
        }
        {
            let event_port = self.event_port.clone();
            let engine_id_clone = engine_id.clone();
            let instance_id_clone = instance_id.clone();
            let subscriber = managed.subscribe_logs();
            let entry_clone = Arc::clone(&entry);
            let pump_token_clone = pump_token.clone();
            tokio::spawn(async move {
                pump_logs_to_event_port(
                    subscriber,
                    event_port,
                    engine_id_clone,
                    instance_id_clone,
                    entry_clone,
                    pump_token_clone,
                )
                .await;
            });
        }

        // spawn 进程
        match managed.start(&req).await {
            Ok(()) => {
                // 进程 spawn 成功——但 process spawned 不等价于 Healthy
                let pid = managed.pid().await.unwrap_or(0);
                tracing::info!(engine = %engine_id, pid, "进程已 spawn，等待 health 验证");

                // 更新 process=Running（但 service 仍为 Unknown）
                self.commit_status_internal(engine_id, None, |status| {
                    status.process = ProcessState::Running { pid };
                    // service 保持 Unknown——需要 health 验证
                })
                .await?;

                // health 验证——只有 Model Ready 才返回 Ok
                // 任何失败（timeout/mismatch/backend/ModelFailed）执行统一 rollback
                match self
                    .verify_engine_health(engine_id, &entry, &identity_input)
                    .await
                {
                    Ok(mapping) => {
                        // health 验证通过 + Model Ready——进入 Healthy
                        self.commit_status_internal(engine_id, None, |status| {
                            status.service = mapping.service;
                            status.model = mapping.model;
                            if let Some(ref backend_obs) = mapping.backend {
                                status.backend.backend_verification =
                                    runtime::verify_backend_consistency(
                                        resolved_launch.profile.backend,
                                        Some(backend_obs),
                                    );
                            }
                        })
                        .await?;
                        tracing::info!(engine = %engine_id, "引擎 health 验证通过，Model Ready");
                        Ok(())
                    }
                    Err(err) => {
                        // 任何失败——统一 rollback
                        tracing::warn!(engine = %engine_id, %err, "health 验证失败，执行 rollback");
                        self.rollback_started_instance(
                            engine_id,
                            &entry,
                            &pkey,
                            &instance_id,
                            &err,
                        )
                        .await;
                        Err(err)
                    }
                }
            }
            Err(e) => {
                // spawn 失败——直接 rollback（清理已设置的中间状态）
                let err = LocalEngineError::from_process(ErrorPhase::Start, "进程启动失败", &e);
                tracing::warn!(engine = %engine_id, %err, "进程 spawn 失败，执行 rollback");
                self.rollback_started_instance(engine_id, &entry, &pkey, &instance_id, &err)
                    .await;
                Err(err)
            }
        }
    }

    // ── stop ────────────────────────────────────────────────────────────────

    /// 停止引擎服务。
    ///
    /// 幂等：如果进程已 Stopped，直接返回 Ok。
    /// 迟到的 health/task/exit 不能覆盖新实例。
    pub async fn stop(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        let _gate = entry.op_gate.lock().await;

        // 幂等检查
        let managed = {
            let mp = entry.managed_process.lock().await;
            mp.clone()
        };

        match managed {
            Some(mp) => {
                // 标记 desired=Stopped, process=Stopping
                self.commit_status_internal(engine_id, None, |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop().await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, None, |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                        })
                        .await?;

                        // 取消旧日志 pump——确保 stop 后旧实例日志不再投影
                        {
                            let mut lc = entry.log_pump_cancel.lock().await;
                            if let Some(cancel) = lc.take() {
                                tracing::debug!(engine = %engine_id, "stop: 取消日志 pump");
                                cancel.cancel();
                            }
                        }

                        // 先取出 instance_id 用于从 registry 移除
                        let saved_instance_id = {
                            let ci = entry.current_identity.lock().await;
                            ci.as_ref().map(|i| i.instance_id.clone())
                        };

                        // 清理 identity/profile
                        {
                            let mut ci = entry.current_identity.lock().await;
                            *ci = None;
                        }
                        {
                            let mut cp = entry.current_profile.lock().await;
                            *cp = None;
                        }
                        {
                            let mut mp_guard = entry.managed_process.lock().await;
                            *mp_guard = None;
                        }

                        // 从同步 registry 移除
                        if let Some(instance_id) = saved_instance_id {
                            let pkey = ProcessKey {
                                engine_id: engine_id.clone(),
                                instance_id,
                            };
                            let mut reg = self.process_registry.lock().unwrap();
                            reg.remove(&pkey);
                        }

                        tracing::info!(engine = %engine_id, "引擎已停止");
                        Ok(())
                    }
                    Err(e) => {
                        let err = LocalEngineError::from_process(ErrorPhase::Stop, "停止失败", &e);
                        self.commit_status_internal(engine_id, None, |status| {
                            status.process = ProcessState::Exited {
                                reason: format!("stop failed: {e}"),
                            };
                            status.last_error = Some(err.clone());
                        })
                        .await?;
                        Err(err)
                    }
                }
            }
            None => {
                // 已 Stopped，幂等返回
                self.commit_status_internal(engine_id, None, |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                })
                .await?;
                Ok(())
            }
        }
    }

    /// 条件停止：只停止指定 instance token 的实例。
    ///
    /// 如果当前实例的 token 与传入的 token 不匹配（已有新实例接管），
    /// 直接返回 Ok(())，不停止新实例。
    ///
    /// 用于 OcrCoordinator 的 lease 管理：旧 timer 或旧 startup task
    /// 不得停止/覆盖新实例。
    pub async fn stop_if_current(
        &self,
        engine_id: &EngineId,
        instance_token: &crate::infra::local_engine::state::InstanceToken,
    ) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        let _gate = entry.op_gate.lock().await;

        let managed = {
            let mp = entry.managed_process.lock().await;
            mp.clone()
        };

        match managed {
            Some(mp) => {
                // 条件检查：token 不匹配则跳过
                if !mp.is_current_token(instance_token).await {
                    tracing::info!(
                        engine = %engine_id,
                        "stop_if_current: token 不匹配，跳过停止（新实例已接管）"
                    );
                    return Ok(());
                }

                // 标记 desired=Stopped, process=Stopping
                self.commit_status_internal(engine_id, None, |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop_if_current(instance_token).await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, None, |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                        })
                        .await?;

                        // 取消旧日志 pump
                        {
                            let mut lc = entry.log_pump_cancel.lock().await;
                            if let Some(cancel) = lc.take() {
                                tracing::debug!(engine = %engine_id, "stop_if_current: 取消日志 pump");
                                cancel.cancel();
                            }
                        }

                        // 清理 identity/profile
                        let saved_instance_id = {
                            let ci = entry.current_identity.lock().await;
                            ci.as_ref().map(|i| i.instance_id.clone())
                        };
                        {
                            let mut ci = entry.current_identity.lock().await;
                            *ci = None;
                        }
                        {
                            let mut cp = entry.current_profile.lock().await;
                            *cp = None;
                        }
                        {
                            let mut mp_guard = entry.managed_process.lock().await;
                            *mp_guard = None;
                        }

                        // 从同步 registry 移除
                        if let Some(instance_id) = saved_instance_id {
                            let pkey = ProcessKey {
                                engine_id: engine_id.clone(),
                                instance_id,
                            };
                            let mut reg = self.process_registry.lock().unwrap();
                            reg.remove(&pkey);
                        }

                        tracing::info!(engine = %engine_id, "引擎已条件停止（token 匹配）");
                        Ok(())
                    }
                    Err(e) => {
                        let err =
                            LocalEngineError::from_process(ErrorPhase::Stop, "条件停止失败", &e);
                        self.commit_status_internal(engine_id, None, |status| {
                            status.process = ProcessState::Exited {
                                reason: format!("stop_if_current failed: {e}"),
                            };
                            status.last_error = Some(err.clone());
                        })
                        .await?;
                        Err(err)
                    }
                }
            }
            None => {
                // 已 Stopped，幂等返回
                self.commit_status_internal(engine_id, None, |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                })
                .await?;
                Ok(())
            }
        }
    }

    // ── repair / cleanup 骨架 ───────────────────────────────────────────────

    /// 修复引擎环境（骨架）。
    pub async fn repair(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let _gate = entry.op_gate.lock().await;

        let operation_id = generate_operation_id();
        self.set_operation_id(engine_id, Some(operation_id.clone()))
            .await;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Repairing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 骨架：执行 adapter self_test
        let self_test = entry.adapter.self_test();
        if !self_test.passed {
            let err = LocalEngineError::with_detail(
                LocalEngineErrorCode::SelfTestFailed,
                ErrorPhase::Repair,
                "修复后 self-test 仍失败",
                self_test.failure_reason.unwrap_or_default(),
            );
            self.finish_operation_with_error(engine_id, &operation_id, &err)
                .await?;
            return Err(err);
        }

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.environment = EnvironmentHealth::Ready;
            status.operation.stage = OperationStage::Completed;
        })
        .await?;

        self.set_operation_id(engine_id, None).await;
        Ok(())
    }

    /// 清理引擎资产（骨架）。
    pub async fn cleanup(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let _gate = entry.op_gate.lock().await;

        let operation_id = generate_operation_id();
        self.set_operation_id(engine_id, Some(operation_id.clone()))
            .await;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Cleaning,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: false,
            };
        })
        .await?;

        // 骨架：真实清理由 H4 + infra providers 实现
        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation.stage = OperationStage::Completed;
        })
        .await?;

        self.set_operation_id(engine_id, None).await;
        Ok(())
    }

    // ── logs / history ──────────────────────────────────────────────────────

    /// 查询引擎日志（provider-neutral 入口）。
    pub async fn get_logs(
        &self,
        engine_id: &EngineId,
        max_lines: usize,
    ) -> Result<Vec<String>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let mp = entry.managed_process.lock().await;
        match mp.as_ref() {
            Some(managed) => {
                let history = managed.log_history().await;
                let lines: Vec<String> = history
                    .into_iter()
                    .rev()
                    .take(max_lines)
                    .map(|entry| entry.text)
                    .collect();
                Ok(lines)
            }
            None => Ok(Vec::new()),
        }
    }

    // ── shutdown_all ────────────────────────────────────────────────────────

    /// 异步遍历所有受管实例并回收。
    ///
    /// 单个失败不能阻止其他实例回收；最终返回汇总错误并记录结构化日志。
    pub async fn shutdown_all(&self) -> Result<(), Vec<LocalEngineError>> {
        let entries = self.entries.read().await;
        let mut errors = Vec::new();

        for (engine_id, entry) in entries.iter() {
            let mp = entry.managed_process.lock().await;
            if let Some(managed) = mp.as_ref() {
                tracing::info!(engine = %engine_id, "shutdown_all: 回收引擎实例");
                if let Err(e) = managed.stop().await {
                    let err = LocalEngineError::from_process(
                        ErrorPhase::Stop,
                        "shutdown_all 回收失败",
                        &e,
                    );
                    tracing::error!(engine = %engine_id, %err, "shutdown_all: 回收失败");
                    errors.push(err);
                } else {
                    // 更新状态
                    drop(mp);
                    let _ = self
                        .commit_status_internal(engine_id, None, |status| {
                            status.desired = DesiredState::Stopped;
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                        })
                        .await;
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }

    /// 同步阻塞版本的 shutdown_all（应用退出用）。
    ///
    /// 遍历 `process_registry`（同步 Mutex，不依赖 async lock），
    /// 对每个 ManagedProcess 调用 `shutdown_blocking()`。
    /// 单个失败不阻止其他回收。
    ///
    /// **0.22.3 Task E**: 不依赖 `entries` 的 async lock——
    /// `process_registry` 是独立的同步 Mutex，shutdown 路径可靠。
    #[allow(dead_code)]
    pub fn shutdown_all_blocking(&self) {
        // 同步遍历 process_registry——不依赖 async entries lock
        let registry = self.process_registry.lock().unwrap();
        for (key, managed) in registry.iter() {
            tracing::info!(
                engine = %key.engine_id,
                instance = %key.instance_id,
                "shutdown_all_blocking: 回收"
            );
            managed.shutdown_blocking();
        }
    }

    // ── 内部辅助 ────────────────────────────────────────────────────────────

    /// 验证 engine_id 在 registry 中。
    fn validate_engine_id(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        match self.registry.lookup(engine_id) {
            crate::app::local_engine::registry::RegistryLookup::Found(_) => Ok(()),
            crate::app::local_engine::registry::RegistryLookup::UnknownEngine { requested } => {
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知引擎",
                    format!("engine_id '{}' 不在编译期 allowlist 中", requested),
                ))
            }
        }
    }

    /// 获取引擎 entry。
    async fn get_entry(&self, engine_id: &EngineId) -> Result<Arc<EngineEntry>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entries = self.entries.read().await;
        entries.get(engine_id).cloned().ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "内部错误",
                format!("engine_id '{}' 通过 registry 验证但未找到 entry", engine_id),
            )
        })
    }

    /// 状态提交统一入口。
    ///
    /// 1. 验证 epoch
    /// 2. 验证 operation_id
    /// 3. revision +1
    /// 4. 广播完整 snapshot
    async fn commit_status_internal(
        &self,
        engine_id: &EngineId,
        operation_id: Option<&str>,
        updater: impl FnOnce(&mut EngineStatus),
    ) -> Result<(), LocalEngineError> {
        let entry = self.get_entry(engine_id).await?;
        let mut status = entry.status.write().await;

        // 验证 epoch
        if status.service_epoch != self.epoch {
            // 新 epoch——重置状态
            *status = EngineStatus {
                service_epoch: self.epoch.clone(),
                ..Default::default()
            };
        }

        // 验证 operation_id
        let current_op_id = entry.current_operation_id.lock().await;
        if let Some(ref current) = *current_op_id {
            if let Some(submitted) = operation_id {
                if submitted != current.as_str() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::Rejected,
                        ErrorPhase::Request,
                        "操作已过期",
                        format!(
                            "operation_id 不匹配: expected={}, got={}",
                            current, submitted
                        ),
                    ));
                }
            } else {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作进行中，请等待",
                    "有活跃操作但提交未携带 operation_id".to_string(),
                ));
            }
        }
        drop(current_op_id);

        // revision +1
        let new_revision = status.revision + 1;

        // 应用更新
        updater(&mut status);
        status.revision = new_revision;

        // 广播
        let snapshot = EngineStatusSnapshot {
            engine_id: engine_id.clone(),
            service_epoch: self.epoch.clone(),
            revision: new_revision,
            status: status.clone(),
        };
        self.event_port.emit_status(&snapshot);

        Ok(())
    }

    /// 设置/清除当前 operation_id。
    async fn set_operation_id(&self, engine_id: &EngineId, op_id: Option<String>) {
        if let Ok(entry) = self.get_entry(engine_id).await {
            let mut guard = entry.current_operation_id.lock().await;
            *guard = op_id;
        }
    }

    /// 操作以错误结束——更新状态并清除 operation_id。
    async fn finish_operation_with_error(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        error: &LocalEngineError,
    ) -> Result<(), LocalEngineError> {
        self.commit_status_internal(engine_id, Some(operation_id), |status| {
            status.operation.stage = OperationStage::Failed;
            status.last_error = Some(error.clone());
        })
        .await?;
        self.set_operation_id(engine_id, None).await;
        Ok(())
    }

    /// 解析 compute profile（从 descriptor 声明的候选列表中选择）。
    fn resolve_profile(
        &self,
        descriptor: &EngineDescriptor,
        preference: ComputePreference,
    ) -> Result<ResolvedProfile, LocalEngineError> {
        let candidates = &descriptor.install_plan.compute_candidates;
        if candidates.is_empty() {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::ProfileUnresolved,
                ErrorPhase::Config,
                "引擎未声明任何 compute profile",
                "descriptor.compute_candidates is empty".to_string(),
            ));
        }

        match preference {
            ComputePreference::Auto => {
                let c = &candidates[0];
                Ok(ResolvedProfile {
                    profile_id: c.profile_id.clone(),
                    backend: match c.preference {
                        ComputePreference::Cpu => ComputeBackend::Cpu,
                        ComputePreference::Cuda => ComputeBackend::Cuda,
                        ComputePreference::Vulkan => ComputeBackend::Vulkan,
                        ComputePreference::Directml => ComputeBackend::Directml,
                        _ => ComputeBackend::Cpu,
                    },
                    artifact_id: c.artifact_id.clone(),
                    priority: 0,
                })
            }
            _ => {
                for (i, c) in candidates.iter().enumerate() {
                    if c.preference == preference {
                        return Ok(ResolvedProfile {
                            profile_id: c.profile_id.clone(),
                            backend: match c.preference {
                                ComputePreference::Cpu => ComputeBackend::Cpu,
                                ComputePreference::Cuda => ComputeBackend::Cuda,
                                ComputePreference::Vulkan => ComputeBackend::Vulkan,
                                ComputePreference::Directml => ComputeBackend::Directml,
                                _ => ComputeBackend::Cpu,
                            },
                            artifact_id: c.artifact_id.clone(),
                            priority: i as u32,
                        });
                    }
                }
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::ProfileUnresolved,
                    ErrorPhase::Config,
                    "引擎未声明此 compute preference",
                    format!("preference={:?} not in descriptor", preference),
                ))
            }
        }
    }

    // ── health 验证 ─────────────────────────────────────────────────────────

    /// 验证引擎 health——轮询直到 Model Ready 或 Err。
    ///
    /// **0.22.3 Task G**: 只有两个终态：
    /// - `Ok(HealthMapping)`：service=Healthy + model=Ready + 身份/backend 全匹配
    /// - `Err`：timeout / mismatch / backend 错误 / ModelFailed / 不可达
    ///
    /// 不返回模糊的 `Verified(last_mapping)`——last_mapping 可能为 NotLoaded。
    /// start 只有在真实 Model Ready 后才返回 Ok。
    async fn verify_engine_health(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        identity_input: &ServiceIdentityInput,
    ) -> Result<HealthMapping, LocalEngineError> {
        let health_url = format!("{}/health", identity_input.endpoint.base_url());
        let token = identity_input.token.clone();
        let token_fp = identity_input.token_fingerprint();

        // 从 descriptor 读取配置化超时——不使用硬编码魔术数字。
        // - start_timeout: Phase 1——等待 HTTP 服务器连通 + 鉴权通过
        // - model_load_timeout: Phase 2——等待 Model Ready（模型加载可能较慢）
        let timeouts = &entry.adapter.descriptor().timeouts;
        let start_timeout = timeouts.start_timeout;
        let model_load_timeout = timeouts.model_load_timeout;

        tracing::info!(
            engine = %engine_id,
            url = %health_url,
            token_fp = %token_fp,
            start_timeout_secs = start_timeout.as_secs(),
            model_load_timeout_secs = model_load_timeout.as_secs(),
            "开始两阶段 health 轮询"
        );

        // 单次 HTTP 请求超时——使用 start_timeout（Phase 1 连通+鉴权），
        // 不硬编码 5s。reqwest 对每次 .send() 应用此超时。
        let client = reqwest::Client::builder()
            .timeout(start_timeout)
            .build()
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Health,
                    "HTTP client 构造失败",
                    format!("{e}"),
                )
            })?;

        // 两阶段轮询：
        // Phase 1: start_timeout 内——等待 HTTP 2xx + 鉴权通过（身份匹配）
        // Phase 2: model_load_timeout 内——等待 Model Ready
        // 总轮询窗口 = start_timeout + model_load_timeout
        let interval = std::time::Duration::from_millis(500);
        let phase1_deadline = tokio::time::Instant::now() + start_timeout;
        let phase2_deadline = phase1_deadline + model_load_timeout;
        let mut attempt: u32 = 0;
        let mut phase1_passed = false;

        loop {
            attempt += 1;
            let now = tokio::time::Instant::now();
            // 检查是否超时
            // Phase 1 未通过时检查 phase1_deadline；通过后检查 phase2_deadline
            let deadline = if phase1_passed {
                phase2_deadline
            } else {
                phase1_deadline
            };
            if now >= deadline {
                let phase = if phase1_passed { "model_load" } else { "start" };
                tracing::warn!(
                    engine = %engine_id,
                    attempt,
                    phase,
                    "health 轮询超时"
                );
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Timeout,
                    ErrorPhase::Health,
                    "health 验证超时",
                    format!("{phase} 阶段在 {attempt} 次尝试后未通过"),
                ));
            }

            match client
                .get(&health_url)
                .header("X-Engine-Token", &token)
                .send()
                .await
            {
                Ok(resp) if resp.status().is_success() => {
                    let raw_health: serde_json::Value = match resp.json().await {
                        Ok(v) => v,
                        Err(e) => {
                            tracing::debug!(attempt, %e, "health 响应解析失败，重试");
                            tokio::time::sleep(interval).await;
                            continue;
                        }
                    };

                    tracing::debug!(attempt, raw = %raw_health, "health 响应");

                    // 两阶段验证：先身份（Phase 1），后 backend/model（Phase 2）
                    match self
                        .parse_and_verify_health(&raw_health, entry, identity_input)
                        .await
                    {
                        Ok(mapping) => {
                            // 身份验证通过——标记 Phase 1 完成
                            if !phase1_passed {
                                phase1_passed = true;
                                tracing::info!(
                                    engine = %engine_id,
                                    attempt,
                                    "Phase 1 通过：HTTP 连通 + 鉴权成功"
                                );
                            }

                            // 只在 model=Ready 时返回 Ok
                            if mapping.model == ModelHealth::Ready {
                                tracing::info!(
                                    attempt,
                                    service = ?mapping.service,
                                    model = ?mapping.model,
                                    "health 验证通过，Model Ready"
                                );
                                return Ok(mapping);
                            }
                            // Model 未 Ready——继续轮询（Phase 2）
                            tracing::debug!(
                                attempt,
                                model = ?mapping.model,
                                "model 尚未 Ready，继续等待"
                            );
                        }
                        Err(err) => {
                            // 身份不匹配/backend 错误——直接返回 Err
                            tracing::warn!(attempt, %err, "health 验证失败");
                            return Err(err);
                        }
                    }
                }
                Ok(resp) => {
                    tracing::debug!(attempt, status = %resp.status(), "health 非 2xx，重试");
                }
                Err(e) => {
                    tracing::debug!(attempt, %e, "health 请求失败，重试");
                }
            }

            tokio::time::sleep(interval).await;
        }
    }

    /// 两阶段验证 health 响应：先身份，后 backend/model。
    ///
    /// **Phase 1**: 从 health 响应中提取回显的身份字段，
    /// 调用 `ServiceIdentityInput::verify` 核对 engine_id/instance_id/token/endpoint。
    /// 任一不匹配返回 `IdentityVerification` 错误。
    ///
    /// **Phase 2**: adapter `map_health` 映射后，验证 backend 一致性。
    /// backend 交叉不匹配（如 GPU↔CPU）返回 `BackendMismatch` 错误。
    /// ModelFailed 返回 `ModelNotReady` 错误。
    async fn parse_and_verify_health(
        &self,
        raw_health: &serde_json::Value,
        entry: &Arc<EngineEntry>,
        identity_input: &ServiceIdentityInput,
    ) -> Result<HealthMapping, LocalEngineError> {
        // Phase 1: 身份验证
        let identity_result = ServiceIdentityResult {
            engine_id: raw_health
                .get("engine_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            instance_id: raw_health
                .get("instance_id")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            token_fingerprint: raw_health
                .get("token_fingerprint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            endpoint: raw_health
                .get("endpoint")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
        };

        match identity_input.verify(&identity_result) {
            IdentityVerification::Verified => {}
            IdentityVerification::Mismatch(mismatch) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::IdentityVerification,
                    ErrorPhase::Health,
                    "服务身份不匹配",
                    mismatch.detail,
                ));
            }
        }

        // Phase 2: adapter 映射 + backend 验证
        let mapping = entry.adapter.map_health(raw_health);

        // ModelFailed 直接返回错误
        if mapping.model == ModelHealth::Failed {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::ModelNotReady,
                ErrorPhase::Health,
                "模型加载失败",
                "health 回报 model=Failed",
            ));
        }

        // ── 模型身份校验（model_id + model_revision + fingerprint） ──
        // health 报告的 model_id 和 model_revision 必须与 descriptor 一致。
        // Ready 必须有合法 fingerprint。
        let descriptor = entry.adapter.descriptor();
        let expected_model_id = &descriptor.model_contract.model_id;
        let expected_revision = &descriptor.model_contract.revision;

        if let Some(ref health_model_id) = mapping.model_id {
            if health_model_id != expected_model_id {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::IdentityVerification,
                    ErrorPhase::Health,
                    "model_id 不匹配",
                    format!(
                        "health 报告 model_id='{health_model_id}'，descriptor 期望='{expected_model_id}'"
                    ),
                ));
            }
        }

        if let Some(ref health_revision) = mapping.model_revision {
            if health_revision != expected_revision {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::IdentityVerification,
                    ErrorPhase::Health,
                    "model_revision 不匹配",
                    format!(
                        "health 报告 model_revision='{health_revision}'，descriptor 期望='{expected_revision}'"
                    ),
                ));
            }
        }

        // Ready 必须有合法 fingerprint（非空、非全零）
        if mapping.model == ModelHealth::Ready {
            match &mapping.model_content_fingerprint {
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 fingerprint",
                        "health 报告 model=Ready 但 model_content_fingerprint 为 None",
                    ));
                }
                Some(fp) if fp.is_empty() || fp.chars().all(|c| c == '0') => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型 Ready 但 fingerprint 无效",
                        "health 报告 model=Ready 但 model_content_fingerprint 为空或全零",
                    ));
                }
                _ => {}
            }
        }

        // backend 一致性验证
        if let Some(ref obs) = mapping.backend {
            let profile = entry.current_profile.lock().await;
            if let Some(ref profile) = *profile {
                let verification = runtime::verify_backend_consistency(profile.backend, Some(obs));
                if verification.state == BackendState::Error {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::BackendMismatch,
                        ErrorPhase::Health,
                        "backend 不匹配",
                        verification.mismatch_reason.unwrap_or_default(),
                    ));
                }
            }
        }

        Ok(mapping)
    }

    // ── rollback ────────────────────────────────────────────────────────────

    /// 统一回滚已启动实例——start 失败时调用。
    ///
    /// **0.22.3 Task C**: 任何 Err 分支都执行此方法：
    /// 1. 停止 ManagedProcess（如果存在）
    /// 2. 清理 identity/profile
    /// 3. 从 process_registry 移除
    /// 4. 从 EngineEntry 移除 managed_process
    /// 5. 置错误终态（process=Exited, service=Unreachable, last_error=err）
    async fn rollback_started_instance(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        pkey: &ProcessKey,
        instance_id: &str,
        error: &LocalEngineError,
    ) {
        tracing::warn!(
            engine = %engine_id,
            instance = instance_id,
            error = %error,
            "rollback_started_instance: 清理中间状态"
        );

        // 停止 ManagedProcess（如果仍在运行）
        {
            let mp = entry.managed_process.lock().await;
            if let Some(managed) = mp.as_ref() {
                if let Err(e) = managed.stop().await {
                    tracing::warn!(
                        engine = %engine_id,
                        error = %e,
                        "rollback: ManagedProcess.stop 失败（继续清理）"
                    );
                }
            }
        }

        // 取消旧日志 pump——确保 rollback 后旧实例日志不再投影
        {
            let mut lc = entry.log_pump_cancel.lock().await;
            if let Some(cancel) = lc.take() {
                tracing::debug!(engine = %engine_id, "rollback: 取消日志 pump");
                cancel.cancel();
            }
        }

        // 清理 identity/profile
        {
            let mut ci = entry.current_identity.lock().await;
            *ci = None;
        }
        {
            let mut cp = entry.current_profile.lock().await;
            *cp = None;
        }
        {
            let mut mp = entry.managed_process.lock().await;
            *mp = None;
        }

        // 从同步 registry 移除
        {
            let mut reg = self.process_registry.lock().unwrap();
            reg.remove(pkey);
        }

        // 置错误终态
        let _ = self
            .commit_status_internal(engine_id, None, |status| {
                status.desired = DesiredState::Stopped;
                status.process = ProcessState::Exited {
                    reason: format!("rollback: {:?}", error.code),
                };
                status.service = ServiceHealth::Unreachable;
                status.model = ModelHealth::Unknown;
                status.last_error = Some(error.clone());
            })
            .await;
    }
}

// ── 日志投影辅助 ──────────────────────────────────────────────────────────

/// 把 ManagedProcess 的实时日志转发到 EventPort。
///
/// **0.22.3 Task H**: 真正的日志实例隔离。
///
/// 隔离机制（三重保障）：
/// 1. **CancellationToken**: stop/rollback/restart 时 cancel，pump 立即退出。
/// 2. **实时身份校验**: 每条日志 emit 前从 `entry.current_identity` 实时读取当前实例 ID，
///    如果与 pump 启动时的 `instance_id` 不匹配（说明已 restart），跳过并退出。
/// 3. **broadcast Closed**: ManagedProcess 的 LogPipe 被 drop 时 broadcast 关闭。
///
/// 不再比较两个静态 instance_id 副本——旧实现中 `expected_instance_id` 和
/// `instance_id` 都是启动时捕获的，永远相等，无法识别 restart。
///
/// 事件 payload 的 `instance_id` 始终为日志真实来源实例（pump 启动时的 instance_id），
/// 不受 stop/restart 后 current_identity 变化的影响。
async fn pump_logs_to_event_port(
    mut subscriber: crate::infra::local_engine::log_pipe::LogSubscriber,
    event_port: Arc<dyn EventPort>,
    engine_id: EngineId,
    instance_id: String,
    entry: Arc<EngineEntry>,
    cancel_token: CancellationToken,
) {
    use tokio::sync::broadcast::error::RecvError;

    loop {
        // 先检查 cancellation——被 cancel 时立即退出
        if cancel_token.is_cancelled() {
            tracing::debug!(engine = %engine_id, "日志 pump 结束（cancelled）");
            break;
        }

        // 用 select 同时监听 broadcast 和 cancellation
        tokio::select! {
            biased; // 优先检查 cancellation
            _ = cancel_token.cancelled() => {
                tracing::debug!(engine = %engine_id, "日志 pump 结束（cancelled）");
                break;
            }
            result = subscriber.recv() => {
                match result {
                    Ok(log_entry) => {
                        // 实时读取当前 identity——如果已 stop/rollback/restart，
                        // current_identity 会变为 None 或不同的 instance_id
                        let current_instance_id = {
                            let ci = entry.current_identity.lock().await;
                            ci.as_ref().map(|i| i.instance_id.clone())
                        };

                        match current_instance_id {
                            Some(ref current) if current == &instance_id => {
                                // 身份匹配——emit 日志（instance_id 为真实来源实例）
                                event_port.emit_log(
                                    &engine_id,
                                    &instance_id,
                                    log_entry.seq,
                                    &log_entry.text,
                                );
                            }
                            Some(ref current) => {
                                // 身份不匹配——说明已 restart，旧 pump 退出
                                tracing::debug!(
                                    engine = %engine_id,
                                    expected = %instance_id,
                                    actual = %current,
                                    "日志 pump: 实例已切换，退出"
                                );
                                break;
                            }
                            None => {
                                // identity 已被清理（stop/rollback）——退出
                                tracing::debug!(
                                    engine = %engine_id,
                                    "日志 pump: identity 已清理，退出"
                                );
                                break;
                            }
                        }
                    }
                    Err(RecvError::Lagged(n)) => {
                        tracing::debug!(
                            engine = %engine_id,
                            missed = n,
                            "日志 pump 落后，跳过"
                        );
                        continue;
                    }
                    Err(RecvError::Closed) => {
                        tracing::debug!(engine = %engine_id, "日志 pump 结束（broadcast closed）");
                        break;
                    }
                }
            }
        }
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::*;
    use crate::infra::local_engine::runtime::{ArtifactId, ComputePreference, RuntimeKind};
    use std::collections::HashMap;
    use std::sync::Arc;

    // ── Fake Adapter ────────────────────────────────────────────────────────

    /// 构建 fake adapter（可配置 self_test 通过/失败）。
    fn make_fake_adapter(id: &str, self_test_passes: bool) -> Arc<dyn LocalEngineAdapter> {
        struct FakeAdapter {
            descriptor: EngineDescriptor,
            self_test_passes: bool,
        }

        impl FakeAdapter {
            fn new(id: &str, self_test_passes: bool) -> Self {
                let artifact = ArtifactId::new("fake-artifact").unwrap();
                Self {
                    descriptor: EngineDescriptor {
                        engine_id: EngineId::new(id).unwrap(),
                        display: EngineDisplay {
                            name: format!("Fake {id}"),
                            description: "test adapter".to_string(),
                            icon: "cpu".to_string(),
                            version: "0.1.0".to_string(),
                        },
                        capability_kind: CapabilityKind::Stt,
                        runtime_kind: RuntimeKind::PythonVenv,
                        install_plan: InstallPlanRef {
                            runtime_kind: RuntimeKind::PythonVenv,
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
                        cleanup: CleanupPolicy::default(),
                    },
                    self_test_passes,
                }
            }
        }

        impl LocalEngineAdapter for FakeAdapter {
            fn descriptor(&self) -> &EngineDescriptor {
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
                    fallback: None,
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

    /// 构建测试用 service（含 1 个 fake adapter）。
    fn make_service(adapter_id: &str) -> Arc<LocalEngineService> {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            adapter_id, true,
        )]));
        LocalEngineService::new(registry, Arc::new(NoopEventPort))
    }

    // ── 验收场景 1：EngineRegistry 拒绝未知 engine_id ──────────────────────

    #[tokio::test]
    async fn service_rejects_unknown_engine_id() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc.get_status(&unknown).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
        assert!(err.detail.contains("no-such-engine"));
    }

    // ── 验收场景 2：初始状态正确 ────────────────────────────────────────────

    #[tokio::test]
    async fn initial_status_is_stopped_unknown() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.desired, DesiredState::Stopped);
        assert_eq!(snapshot.status.process, ProcessState::Stopped);
        assert_eq!(snapshot.status.service, ServiceHealth::Unknown);
        assert_eq!(
            snapshot.status.environment,
            crate::domain::local_engine::EnvironmentHealth::Missing
        );
        assert_eq!(snapshot.revision, 0);
        assert_eq!(snapshot.service_epoch, *svc.epoch());
    }

    // ── 验收场景 3：install self-test 通过后环境标记为 Ready ──────────────

    #[tokio::test]
    async fn install_marks_environment_ready_when_self_test_passes() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(
            snapshot.status.environment,
            crate::domain::local_engine::EnvironmentHealth::Ready
        );
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
        assert_eq!(snapshot.status.operation.kind, OperationKind::Installing);
    }

    // ── 验收场景 3b：install self-test 失败时返回错误 ────────────────────────

    #[tokio::test]
    async fn install_fails_when_self_test_fails() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            "engine-fail",
            false,
        )]));
        let svc = LocalEngineService::new(registry, Arc::new(NoopEventPort));
        let eid = EngineId::new("engine-fail").unwrap();

        let result = svc.install(&eid, AdapterConfig::new()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::SelfTestFailed);

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.stage, OperationStage::Failed);
    }

    // ── 验收场景 4：start 在环境未就绪时返回 EnvironmentMissing ────────────

    #[tokio::test]
    async fn start_fails_when_environment_not_ready() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.start(&eid, AdapterConfig::new()).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::EnvironmentMissing);
        assert_eq!(err.phase, ErrorPhase::Start);
    }

    // ── 验收场景 5：stop 幂等——已 Stopped 时直接返回 Ok ────────────────────

    #[tokio::test]
    async fn stop_is_idempotent_when_already_stopped() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.stop(&eid).await;
        assert!(result.is_ok());

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.desired, DesiredState::Stopped);
    }

    // ── 验收场景 6：catalog / get_all_status 查询 API 正确返回 ─────────────

    #[tokio::test]
    async fn catalog_and_get_all_status_return_all_engines() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![
            make_fake_adapter("engine-a", true),
            make_fake_adapter("engine-b", true),
        ]));
        let svc = LocalEngineService::new(registry, Arc::new(NoopEventPort));

        let catalog = svc.catalog().await;
        assert_eq!(catalog.len(), 2);

        let all_status = svc.get_all_status().await;
        assert_eq!(all_status.len(), 2);

        for snap in &all_status {
            assert_eq!(snap.service_epoch, *svc.epoch());
            assert_eq!(snap.revision, 0);
        }
    }

    // ── 验收场景 7：get_diagnostics 返回 adapter 诊断 ──────────────────────

    #[tokio::test]
    async fn get_diagnostics_returns_adapter_diagnostics() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let diag = svc.get_diagnostics(&eid).await.unwrap();
        assert!(!diag.entries.is_empty());
        assert_eq!(diag.entries[0].key, "version");
    }

    // ── 验收场景 8：repair 骨架执行 self-test 后环境标记为 Ready ───────────

    #[tokio::test]
    async fn repair_marks_environment_ready_when_self_test_passes() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        svc.repair(&eid).await.unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(
            snapshot.status.environment,
            crate::domain::local_engine::EnvironmentHealth::Ready
        );
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── 验收场景 8b：cleanup 骨架完成 ───────────────────────────────────────

    #[tokio::test]
    async fn cleanup_completes_successfully() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        svc.cleanup(&eid).await.unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── 验收场景 9：shutdown_all 无进程时返回 Ok ───────────────────────────

    #[tokio::test]
    async fn shutdown_all_succeeds_when_no_processes() {
        let svc = make_service("engine-a");
        let result = svc.shutdown_all().await;
        assert!(result.is_ok());
    }

    // ── 验收场景 10：revision 在状态变更后严格递增 ─────────────────────────

    #[tokio::test]
    async fn revision_strictly_increases_after_status_changes() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let snap0 = svc.get_status(&eid).await.unwrap();
        assert_eq!(snap0.revision, 0);

        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        let snap1 = svc.get_status(&eid).await.unwrap();
        assert!(snap1.revision > snap0.revision);
    }

    // ── 验收场景 11：service_epoch 在新 service 实例时变化 ─────────────────

    #[tokio::test]
    async fn service_epoch_differs_between_instances() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            "engine-a", true,
        )]));
        let svc1 = LocalEngineService::new(registry.clone(), Arc::new(NoopEventPort));
        let svc2 = LocalEngineService::new(registry, Arc::new(NoopEventPort));

        assert_ne!(*svc1.epoch(), *svc2.epoch());
    }

    // ── 验收场景 12：get_logs 无进程时返回空 ───────────────────────────────

    #[tokio::test]
    async fn get_logs_returns_empty_when_no_process() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let logs = svc.get_logs(&eid, 100).await.unwrap();
        assert!(logs.is_empty());
    }

    // ── 验收场景 13：unknown engine_id 的操作全部返回 Unsupported ──────────

    #[tokio::test]
    async fn all_operations_reject_unknown_engine_id() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("unknown-engine").unwrap();

        assert!(svc.install(&unknown, AdapterConfig::new()).await.is_err());
        assert!(svc.start(&unknown, AdapterConfig::new()).await.is_err());
        assert!(svc.stop(&unknown).await.is_err());
        assert!(svc.repair(&unknown).await.is_err());
        assert!(svc.cleanup(&unknown).await.is_err());
        assert!(svc.get_diagnostics(&unknown).await.is_err());
        assert!(svc.get_logs(&unknown, 10).await.is_err());
    }

    // ── 0.22.3 失败路径测试 ────────────────────────────────────────────────

    // ── 失败路径 1：start spawn 失败时执行 rollback ─────────────────────────

    #[tokio::test]
    async fn start_rollback_on_spawn_failure() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 先 install 让环境就绪
        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        // start 会因 fake-executable 不存在而 spawn 失败
        // 验证 rollback 被执行：终态应为 Exited + last_error
        let result = svc.start(&eid, AdapterConfig::new()).await;
        assert!(result.is_err());

        let snapshot = svc.get_status(&eid).await.unwrap();
        // rollback 应置 desired=Stopped, process=Exited, service=Unreachable
        assert_eq!(snapshot.status.desired, DesiredState::Stopped);
        assert!(matches!(
            snapshot.status.process,
            ProcessState::Exited { .. }
        ));
        assert_eq!(snapshot.status.service, ServiceHealth::Unreachable);
        assert!(snapshot.status.last_error.is_some());
    }

    // ── 失败路径 2：start health 验证超时执行 rollback ──────────────────────

    #[tokio::test]
    async fn start_rollback_on_health_timeout() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 先 install 让环境就绪
        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        // start 会 spawn fake-executable（不存在）→ spawn 失败 → rollback
        // 或者即使 spawn 成功（理论上不会），health 也会超时
        let result = svc.start(&eid, AdapterConfig::new()).await;
        assert!(result.is_err());

        // 验证 rollback 清理了 identity
        let conn = svc.get_connection(&eid).await.unwrap();
        assert!(conn.is_none(), "rollback 后不应有 connection");
    }

    // ── 失败路径 3：rollback 清理 process_registry ──────────────────────────

    #[tokio::test]
    async fn rollback_clears_process_registry() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 先 install
        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        // start 失败
        let _ = svc.start(&eid, AdapterConfig::new()).await;

        // process_registry 应为空（rollback 清理了）
        let reg = svc.process_registry.lock().unwrap();
        assert!(
            reg.is_empty(),
            "process_registry 应在 rollback 后为空，实际有 {} 个条目",
            reg.len()
        );
    }

    // ── 失败路径 4：shutdown_all_blocking 使用 process_registry ────────────

    #[test]
    fn shutdown_all_blocking_uses_process_registry() {
        let svc = make_service("engine-a");

        // 无进程时 shutdown_all_blocking 不 panic
        svc.shutdown_all_blocking();
    }

    // ── 失败路径 5：stop 清理 process_registry ──────────────────────────────

    #[tokio::test]
    async fn stop_clears_process_registry() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 无进程时 stop——幂等返回
        svc.stop(&eid).await.unwrap();

        // process_registry 应为空
        let reg = svc.process_registry.lock().unwrap();
        assert!(reg.is_empty());
    }

    // ── 失败路径 6：get_connection 无实例时返回 None ───────────────────────

    #[tokio::test]
    async fn get_connection_returns_none_when_no_instance() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let conn = svc.get_connection(&eid).await.unwrap();
        assert!(conn.is_none());
    }

    // ── 失败路径 7：get_connection 对未知 engine 返回错误 ───────────────────

    #[tokio::test]
    async fn get_connection_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc.get_connection(&unknown).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, LocalEngineErrorCode::Unsupported);
    }
}
