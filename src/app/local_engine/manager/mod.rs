//! EngineManager — 本地引擎生命周期编排服务（0.22.3）。
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
    AdapterConfig, CancelOutcome, DeleteConflictReason, DesiredState, EngineDefinition,
    EngineDiagnostic, EngineModelDescriptor, EngineModelStatus, EngineOperation, EngineStatus,
    EngineStatusSnapshot, EnvOperationEndState, EnvironmentHealth, ErrorPhase, HealthMapping,
    LaunchContext, LocalEngineAdapter, LocalEngineError, LocalEngineErrorCode, ModelCompatibility,
    ModelDeleteConflict, ModelHealth, ModelInstallState, ModelOperationKind, ModelOperationResult,
    ModelOperationStage, ModelVerificationState, OperationKind, OperationStage, ProcessState,
    ServiceEpoch, ServiceHealth,
};
use crate::infra::local_engine::deployment::DeploymentStore;
use crate::infra::local_engine::lease::{ProcessLease, remove_lease, write_lease};
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::port::{
    ConflictRetryPolicy, EndpointAllocator, IdentityVerification, ServiceIdentityInput,
    ServiceIdentityResult, generate_service_token, is_explicit_address_in_use,
};
use crate::infra::local_engine::process::{LaunchRequest, ManagedProcess, ShutdownConfig};
use crate::infra::local_engine::runtime::{
    self, BackendState, ComputePreference, EngineId, ModelContract, ResolvedProfile,
    generate_install_id, generate_operation_id,
};
use crate::infra::local_engine::state::{ProcessIdentity, ProcessStatus};

use crate::infra::local_engine::providers::InstallSink;
use crate::infra::local_engine::providers::ProviderDescriptor;
use crate::infra::local_engine::providers::python::PythonVenvProvider;

use super::error_bridge::{from_process, from_runtime};
use super::operation_coordinator::{EngineOperationCoordinator, OperationGuard};
use super::registry::EngineRegistry;

mod deployment;
mod health;
mod lifecycle;
mod logs;
mod models;
mod recovery;
mod status;
mod storage;

#[cfg(test)]
mod tests;

// tests 通过 `use super::*` 消费以下跨用例模块的 helper（保持原测试代码不变）。
#[cfg(test)]
use health::{is_valid_model_fingerprint, require_backend_when_ready};
#[cfg(test)]
use lifecycle::{build_process_lease, resolve_expected_model_identity};
#[cfg(test)]
use logs::classify_engine_log;

// ── InstallSinkAdapter (0.22.6 H5) ──────────────────────────────────────

/// `InstallSink` 适配器——把 infra 层安装进度/日志桥接到 `EventPort`。
///
/// 持有 `EventPort` 引用、`engine_id`、`operation_id` 和日志序号计数器。
/// `on_stage` 通过 `emit_install_stage` 广播阶段变更给前端。
/// `on_log` 把安装日志行通过 `emit_install_log` 广播，以 `operation_id` 隔离。
///
/// **洪泛保护**：使用简单的速率限制——每秒最多 50 条日志。
/// **线程安全**：所有字段通过 `Mutex` 保护。
struct InstallSinkAdapter {
    event_port: Arc<dyn EventPort>,
    engine_id: EngineId,
    operation_id: String,
    log_seq: std::sync::Mutex<u64>,
    /// 上次速率限制窗口重置时间（毫秒）
    rate_window: std::sync::Mutex<(std::time::Instant, u32)>,
}

impl InstallSinkAdapter {
    fn new(event_port: Arc<dyn EventPort>, engine_id: EngineId, operation_id: String) -> Self {
        Self {
            event_port,
            engine_id,
            operation_id,
            log_seq: std::sync::Mutex::new(0),
            rate_window: std::sync::Mutex::new((std::time::Instant::now(), 0)),
        }
    }

    /// 速率限制检查——每秒最多 50 条日志。
    /// 返回 true 表示允许通过，false 表示被限流。
    fn check_rate_limit(&self) -> bool {
        let mut window = self.rate_window.lock().unwrap();
        let now = std::time::Instant::now();
        let elapsed = now.duration_since(window.0);
        if elapsed.as_secs() >= 1 {
            // 重置窗口
            *window = (now, 1);
            true
        } else if window.1 >= 50 {
            // 超过限制
            false
        } else {
            window.1 += 1;
            true
        }
    }
}

impl InstallSink for InstallSinkAdapter {
    fn on_stage(&self, stage: &str) {
        tracing::debug!(
            engine = %self.engine_id,
            op = %self.operation_id,
            stage = stage,
            "install sink: stage"
        );
        // 0.22.6 H4: 通过 emit_install_stage 广播阶段变更给前端
        self.event_port
            .emit_install_stage(&self.engine_id, &self.operation_id, stage);
    }

    fn on_log(&self, level: &str, text: &str) {
        // 速率限制
        if !self.check_rate_limit() {
            return;
        }

        let seq = {
            let mut s = self.log_seq.lock().unwrap();
            *s += 1;
            *s
        };

        // 安装器原始输出默认进入 debug；明确的 warn/error 保留级别。
        // UI 仍接收完整的受限流日志，后端排障时可通过 debug 日志查看同一条目。
        let log_level = super::dto::EngineLogLevel::from_str_lossy(level);
        match log_level {
            super::dto::EngineLogLevel::Error => {
                tracing::error!(engine = %self.engine_id, op = %self.operation_id, seq, output = text, "本地引擎安装输出")
            }
            super::dto::EngineLogLevel::Warn => {
                tracing::warn!(engine = %self.engine_id, op = %self.operation_id, seq, output = text, "本地引擎安装输出")
            }
            super::dto::EngineLogLevel::Trace => {
                tracing::trace!(engine = %self.engine_id, op = %self.operation_id, seq, output = text, "本地引擎安装输出")
            }
            _ => {
                tracing::debug!(engine = %self.engine_id, op = %self.operation_id, seq, output = text, "本地引擎安装输出")
            }
        }

        self.event_port
            .emit_install_log(&self.engine_id, &self.operation_id, seq, log_level, text);
    }
}

// ── StructuredLogEntry ─────────────────────────────────────────────────────

/// 结构化日志条目——由 service 层从 `LogEntry` 投影。
///
/// 包含 `engine_id`、`instance_id`、`seq`、`timestamp_ms`、`level`、`text`。
/// commands 层将其投影为 `EngineLogDto`（字符串化 seq + RFC 3339 timestamp）。
#[derive(Debug, Clone)]
pub struct StructuredLogEntry {
    /// 引擎 id。
    pub engine_id: String,
    /// 实例 id（用于按 instance 隔离日志）。
    pub instance_id: String,
    /// 序号（单调递增）。
    pub seq: u64,
    /// 时间戳（Unix 毫秒）。
    pub timestamp_ms: u64,
    /// 日志级别（"error" / "warn" / "info" / "debug" / "trace"）。
    pub level: String,
    /// 文本内容。
    pub text: String,
}

// ── LocalEngineConnection ─────────────────────────────────────────────────

/// 受限连接快照——由 `EngineManager` 产生，不可序列化给前端。
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

/// start 单次尝试的失败分类（0.22.6 phase B）。
///
/// 区分**可重试的 bind race** 与**致命失败**：
/// - `BindRace`：子进程输出包含明确的 address-in-use——probe 空闲与
///   bind 之间的竞争，可重新分配端口重试（次数由 ConflictRetryPolicy 封顶）；
/// - `Fatal`：其他一切失败——不重试，直接 rollback 并返回错误。
#[derive(Debug)]
enum StartAttemptFailure {
    BindRace { detail: String },
    Fatal(LocalEngineError),
}

// ── EngineEntry ───────────────────────────────────────────────────────────

/// start 时冻结的模型身份（"selected"之外的实际启动合同）。
#[derive(Debug, Clone)]
pub(super) struct FrozenModelIdentity {
    pub model_id: String,
    pub revision: String,
    /// manifest 内容指纹（adapter 自管模型时为 None，由 health 契约校验）。
    pub fingerprint: Option<String>,
}

/// 运行中实例的 launch snapshot——**start 时冻结，stop/exit 时清除**。
///
/// 冻结内容（任务铁则）：
/// - deployment identity（active 部署 install_id）
/// - model_id / revision（来自配置 selected + manifest，不来自当前配置猜测）
/// - resolved profile / backend
/// - instance identity（endpoint/token/instance_id）
///
/// 配置变化只改变 selected，不改变正在运行的 active——
/// 模型删除冲突检查以此 snapshot 为准。
#[derive(Debug, Clone)]
pub(super) struct LaunchSnapshot {
    /// 实例身份（endpoint/token/instance_id）。
    pub identity: ServiceIdentityInput,
    /// resolved profile（backend 期望来源）。
    pub profile: ResolvedProfile,
    /// 本次启动绑定的 active 部署 install_id。
    pub deployment_install_id: String,
    /// 本次启动冻结的模型身份（None = adapter 自管/无模型合同）。
    pub model: Option<FrozenModelIdentity>,
}

/// 单引擎的运行时状态。
///
/// 持有：
/// - adapter 引用
/// - 当前状态快照
/// - launch snapshot（start 冻结 / stop 清除）
/// - 受管实例跟踪
///
/// 变更操作互斥由 `EngineManager` 的进程级 `EngineOperationCoordinator`
/// 承载（key = engine_id），不再使用 per-entry gate。
pub(crate) struct EngineEntry {
    adapter: Arc<dyn LocalEngineAdapter>,
    /// 状态快照（读多写少，用 RwLock）。
    status: RwLock<EngineStatus>,
    /// 运行实例的 launch snapshot（Running 时存在）。
    launch: Mutex<Option<LaunchSnapshot>>,
    /// 受管实例（Running 时存在）。
    managed_process: Mutex<Option<Arc<ManagedProcess>>>,
    /// 上一实例的 ManagedProcess 引用——stop 后保留 bounded history 可查。
    #[allow(dead_code)]
    last_managed_process: Mutex<Option<Arc<ManagedProcess>>>,
    /// 日志 pump 的 cancellation token——每次 start 创建新 token，
    /// stop/rollback/restart 时 cancel 旧 pump，确保旧实例日志不再投影。
    log_pump_cancel: Mutex<Option<CancellationToken>>,
    /// 后台探测共享结果——确定性 probe 协调。
    ///
    /// 构造后 spawn 后台任务探测 active 部署，
    /// `ensure_installed`/`start` 在执行前 await 此信号，
    /// 确保不会在探测未完成时竞态重复安装。
    /// probe 完成（成功或失败）后所有等待者获得同一确定结果。
    probe_result: OnceCell<Result<(), LocalEngineError>>,
    /// probe 完成信号 watch sender——probe 完成后发送 true。
    probe_tx: watch::Sender<bool>,
    /// probe 完成信号 watch——用于确定性等待（不轮询）。
    probe_watch: watch::Receiver<bool>,
}

impl EngineEntry {
    /// 当前实例身份（None = 未运行）。
    async fn current_identity(&self) -> Option<ServiceIdentityInput> {
        self.launch
            .lock()
            .await
            .as_ref()
            .map(|l| l.identity.clone())
    }

    /// 当前 resolved profile（None = 未运行）。
    async fn current_profile(&self) -> Option<ResolvedProfile> {
        self.launch.lock().await.as_ref().map(|l| l.profile.clone())
    }

    /// 当前 launch snapshot 克隆。
    async fn current_launch(&self) -> Option<LaunchSnapshot> {
        self.launch.lock().await.clone()
    }
}

// ── EventPort ─────────────────────────────────────────────────────────────

/// 状态/日志事件的 app 投影出口。
///
/// 由 app 层（commands/wiring）实现，把领域事件桥接成 Tauri emit。
/// service 不直接持有 AppHandle——通过此 trait 解耦。
pub trait EventPort: Send + Sync {
    /// 广播引擎状态快照。
    fn emit_status(&self, snapshot: &EngineStatusSnapshot);

    /// 广播引擎日志条目（运行时日志，以 `instance_id` 隔离）。
    fn emit_log(
        &self,
        engine_id: &EngineId,
        instance_id: &str,
        seq: u64,
        level: super::dto::EngineLogLevel,
        line: &str,
    );

    /// 广播安装日志条目（安装时日志，以 `operation_id` 隔离）。
    ///
    /// `level` 是闭合枚举 `EngineLogLevel`。
    /// `text` 是已做 UTF-8 lossy + 长度截断的日志行。
    /// `seq` 在同一 `operation_id` 内单调递增。
    fn emit_install_log(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
        seq: u64,
        level: super::dto::EngineLogLevel,
        text: &str,
    );

    /// 广播安装阶段变更（0.22.6 H4）。
    ///
    /// 前端通过此事件实时显示安装进度（preparing/downloading/verifying/...）。
    /// `stage` 是稳定的 wire 字符串（对应 `OperationStage` 的 Display 值）。
    fn emit_install_stage(&self, engine_id: &EngineId, operation_id: &str, stage: &str);
}

/// 空实现（测试/无事件场景用）。
#[allow(dead_code)]
pub struct NoopEventPort;

impl EventPort for NoopEventPort {
    fn emit_status(&self, _snapshot: &EngineStatusSnapshot) {}
    fn emit_log(
        &self,
        _engine_id: &EngineId,
        _instance_id: &str,
        _seq: u64,
        _level: super::dto::EngineLogLevel,
        _line: &str,
    ) {
    }
    fn emit_install_log(
        &self,
        _engine_id: &EngineId,
        _operation_id: &str,
        _seq: u64,
        _level: super::dto::EngineLogLevel,
        _text: &str,
    ) {
    }
    fn emit_install_stage(&self, _engine_id: &EngineId, _operation_id: &str, _stage: &str) {}
}

// ── EngineManager ────────────────────────────────────────────────────

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

pub struct EngineManager {
    registry: Arc<EngineRegistry>,
    /// 模型目录（编译期 allowlist，从原 ModelService 并入）。
    model_registry: super::model_installer::ModelRegistry,
    epoch: ServiceEpoch,
    entries: RwLock<HashMap<EngineId, Arc<EngineEntry>>>,
    event_port: Arc<dyn EventPort>,
    /// 进程级唯一操作协调器——所有变更操作（安装/修复/启动/停止/清理/
    /// 模型资产操作）必须先 claim，key 只有 engine_id。
    coordinator: EngineOperationCoordinator,
    /// 模型安装 worker（下载执行器）。
    model_worker: Arc<dyn super::model_installer::ModelInstallWorker>,
    /// 每引擎对应的 ProviderDescriptor（infra 层安装事务用）。
    /// key = engine_id，value = 编译期声明的 provider descriptor。
    provider_descriptors: HashMap<EngineId, ProviderDescriptor>,
    /// Python venv provider 实例（安装事务用）。
    /// 目前只有 PythonVenv 引擎，单实例即可。
    python_provider: PythonVenvProvider,
    /// 同步 process registry——独立于 async entries 锁。
    /// `shutdown_all_blocking` 直接读取此字段，不访问 entries。
    /// start 在 spawn 后登记，stop/spawn失败/health失败后移除。
    /// 0.22.6.4: 使用 Arc 包装，使 exit monitor 能持有引用并在验证身份后
    /// 安全移除对应 ProcessKey 条目，避免 registry 泄漏。
    /// 不形成强引用环：Arc 指向 Mutex，不指向 EngineManager 自身。
    process_registry: Arc<std::sync::Mutex<HashMap<ProcessKey, Arc<ManagedProcess>>>>,
}

#[allow(dead_code)]
impl EngineManager {
    /// 创建服务实例。
    ///
    /// 每次创建生成新 `service_epoch`——新 epoch 初始 revision 不受旧快照影响。
    pub fn new(registry: Arc<EngineRegistry>, event_port: Arc<dyn EventPort>) -> Arc<Self> {
        Self::new_with_providers(
            registry,
            event_port,
            HashMap::new(),
            PythonVenvProvider::new(),
            super::model_installer::ModelRegistry::empty(),
            Arc::new(super::model_installer::NoopModelWorker),
        )
    }

    /// 创建服务实例（带 provider descriptors + python provider + 模型目录）。
    ///
    /// `provider_descriptors`：每引擎对应的 `ProviderDescriptor`，
    /// 由 wiring 层在构造时传入（如 `make_funasr_provider_descriptor()`）。
    /// `python_provider`：`PythonVenvProvider` 实例，用于 `InstallTransaction`。
    /// `model_registry` / `model_worker`：模型资产目录与下载执行器。
    pub fn new_with_providers(
        registry: Arc<EngineRegistry>,
        event_port: Arc<dyn EventPort>,
        provider_descriptors: HashMap<EngineId, ProviderDescriptor>,
        python_provider: PythonVenvProvider,
        model_registry: super::model_installer::ModelRegistry,
        model_worker: Arc<dyn super::model_installer::ModelInstallWorker>,
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
                    status: RwLock::new(initial_status),
                    launch: Mutex::new(None),
                    managed_process: Mutex::new(None),
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
            model_registry,
            epoch,
            entries: RwLock::new(entries),
            event_port,
            coordinator: EngineOperationCoordinator::new(),
            model_worker,
            provider_descriptors,
            python_provider,
            process_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
        });

        // 后台探测每个引擎的 active 部署（含事务 journal fail-closed 恢复）
        // 不阻塞主链路（Alt+Space），ensure_installed/start 会 await 探测结果
        // 探测结果经 commit_status_internal 统一提交（revision+1 并广播）
        // 确定性 probe 协调——使用 OnceCell 共享结果，不轮询
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

    /// 返回操作协调器引用（测试用）。
    pub fn coordinator(&self) -> &EngineOperationCoordinator {
        &self.coordinator
    }

    /// 返回模型目录引用（commands DTO 投影用）。
    pub fn model_registry(&self) -> &super::model_installer::ModelRegistry {
        &self.model_registry
    }

    /// 获取引擎 entry（测试注入 launch snapshot 用）。
    #[cfg(test)]
    pub(crate) async fn get_entry_internal(
        &self,
        engine_id: &EngineId,
    ) -> Result<Arc<EngineEntry>, LocalEngineError> {
        self.get_entry(engine_id).await
    }
}
