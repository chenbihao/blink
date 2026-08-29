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
    EngineDiagnostic, EngineModelDescriptor, EngineModelStatus, EngineOperation,
    EngineOperationCoordinator, EngineStatus, EngineStatusSnapshot, EnvOperationEndState,
    EnvironmentHealth, ErrorPhase, HealthMapping, LaunchContext, LocalEngineAdapter,
    LocalEngineError, LocalEngineErrorCode, ModelCompatibility, ModelDeleteConflict, ModelHealth,
    ModelInstallState, ModelOperationKind, ModelOperationResult, ModelOperationStage,
    ModelVerificationState, OperationGuard, OperationKind, OperationStage, ProcessState,
    ServiceEpoch, ServiceHealth, transition_install_state,
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
    self, BackendState, ComputeBackend, ComputePreference, EngineId, ModelContract,
    ResolvedProfile, generate_install_id, generate_operation_id,
};
use crate::infra::local_engine::state::{ProcessIdentity, ProcessStatus};

use crate::infra::local_engine::providers::InstallSink;
use crate::infra::local_engine::providers::ProviderDescriptor;
use crate::infra::local_engine::providers::python::PythonVenvProvider;

use super::error_bridge::{from_process, from_runtime};
use super::registry::EngineRegistry;

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

/// 从子进程输出内容推断展示/tracing 级别。
///
/// stdout/stderr 只是传输通道，不等于日志级别：Paddle/PaddleX 会把下载进度写到
/// stderr，若直接映射为 warn 会产生大量伪告警。受信任 wrapper 的显式前缀优先，
/// 未分类输出降为 debug。
pub(super) fn classify_engine_log(
    _source: crate::infra::local_engine::log_pipe::LogSource,
    text: &str,
) -> super::dto::EngineLogLevel {
    use super::dto::EngineLogLevel;
    let trimmed = text.trim_start();
    if trimmed.starts_with("[ERROR]")
        || trimmed.starts_with("ERROR:")
        || trimmed.starts_with("Traceback ")
    {
        EngineLogLevel::Error
    } else if trimmed.starts_with("[WARN]") || trimmed.starts_with("WARNING:") {
        EngineLogLevel::Warn
    } else if trimmed.starts_with("[INFO]") || trimmed.starts_with("[STATE]") {
        EngineLogLevel::Info
    } else if trimmed.starts_with("[TRACE]") {
        EngineLogLevel::Trace
    } else {
        EngineLogLevel::Debug
    }
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

/// 单个清理目标的执行结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CleanupTargetOutcome {
    /// 已删除，释放 bytes。
    Cleaned(u64),
    /// Windows 占用等——已记 cleanup residue（非产品状态），等待后续清理。
    Deferred(u64),
}

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
    /// 模型状态缓存（manager 唯一持有；磁盘 manifest 是持久真源）。
    model_states: RwLock<HashMap<(EngineId, String), EngineModelStatus>>,
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
            model_states: RwLock::new(HashMap::new()),
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

    // ── 查询 API（可并发） ──────────────────────────────────────────────────

    /// 返回所有引擎的 catalog（描述符列表）。
    pub async fn catalog(&self) -> Vec<EngineDefinition> {
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
        Ok(entry.current_identity().await)
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

    /// 安装/更新引擎环境。
    ///
    /// **唯一真源**：通过 `InstallTransaction` 事务执行安装
    /// （slot + journal，见 `infra/local_engine/deployment`）。
    ///
    /// 事务流程（由 `InstallTransaction::execute` 编排）：
    /// 1. journal begin（fail-closed 前提）
    /// 2. resolve_profile → 解析 compute preference
    /// 3. provider.prepare_environment → uv venv + pip install + self-test
    /// 4. promote → staging → candidate slot
    /// 5. atomic switch → `deployment.json`
    /// 6. 切换后验证失败 → 自动回滚 previous
    /// 7. 成功 → 删除旧 slot（占用记 residue），清 journal
    ///
    /// 安装前先停止运行中的引擎实例（安装持有操作 claim，串行安全）。
    /// 安装后验证 adapter self_test，成功后标记 environment=Ready。
    ///
    /// **终态协议**：返回 `EnvOperationEndState`——`Completed` 或 `Cancelled`
    /// （取消是正常终态，不包装成错误）；失败走 `Err(LocalEngineError)`。
    /// 无论哪种结束方式，状态快照的 `operation` 都归位 Idle——
    /// 操作结果由本返回值 + status 事件表达，不留 busy 残留。
    pub async fn install(
        &self,
        engine_id: &EngineId,
        config: AdapterConfig,
    ) -> Result<(Option<String>, EnvOperationEndState), LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        // 等待后台探测完成，避免竞态重复安装
        self.await_probe(engine_id).await?;

        let entry = self.get_entry(engine_id).await?;

        // 先检查 adapter self_test——如果已通过，环境已就绪，无需重新安装。
        // self_test 可能等待 venv python 子进程——阻塞隔离到 spawn_blocking。
        let adapter = Arc::clone(&entry.adapter);
        let pre_test = tokio::task::spawn_blocking(move || adapter.self_test())
            .await
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Install,
                    "安装前检查失败",
                    format!("spawn_blocking join 错误: {e}"),
                )
            })?;
        if pre_test.passed {
            self.commit_status_internal(engine_id, None, |status| {
                status.environment = EnvironmentHealth::Ready;
            })
            .await?;
            tracing::info!(engine = %engine_id, "install 跳过（self-test 已通过，环境就绪）");
            return Ok((None, EnvOperationEndState::Completed));
        }

        // claim 进程级操作（原子 busy 检查 + 登记）
        let operation_id = generate_operation_id();
        let guard = self
            .coordinator
            .try_claim(engine_id, &operation_id)
            .map_err(|e| {
                tracing::info!(engine = %engine_id, %e, "install: 引擎操作进行中，拒绝");
                e
            })?;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Installing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 更新会切换 slot——先停止运行中的实例（复用当前 claim 的 operation_id）
        self.stop_internal(engine_id, &entry, &operation_id).await;

        let result = self
            .install_transaction_locked(
                engine_id,
                &entry,
                &config,
                &pre_test,
                &operation_id,
                &guard,
            )
            .await;

        match result {
            Ok(()) => {
                // guard 仍持有 claim——归位 Idle 并广播终态后随 guard drop 释放 claim
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.clear_operation();
                })
                .await?;
                tracing::info!(engine = %engine_id, "install 完成（InstallTransaction + self-test passed）");
                Ok((Some(operation_id), EnvOperationEndState::Completed))
            }
            Err(err) => {
                // 取消是正常终态——事务已回滚，环境保持原状，不记 last_error
                if guard.is_cancelled() || err.code == LocalEngineErrorCode::Cancelled {
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.clear_operation();
                    })
                    .await?;
                    tracing::info!(engine = %engine_id, op = %operation_id, "install 已取消（正常终态）");
                    return Ok((Some(operation_id), EnvOperationEndState::Cancelled));
                }
                // 安装失败——事务内部已回滚（old 部署不受影响），标记 Broken
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.environment = EnvironmentHealth::Broken;
                    status.clear_operation();
                })
                .await?;
                Err(err)
            }
        }
    }

    /// install/repair 共享的事务执行体（调用方持有 operation claim）。
    async fn install_transaction_locked(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        config: &AdapterConfig,
        pre_test: &crate::domain::local_engine::AdapterSelfTest,
        operation_id: &str,
        guard: &OperationGuard,
    ) -> Result<(), LocalEngineError> {
        let preference = config.compute_preference.unwrap_or(ComputePreference::Auto);

        // 查找此引擎的 ProviderDescriptor
        let provider_descriptor = match self.provider_descriptors.get(engine_id) {
            Some(d) => d,
            None => {
                // 无 ProviderDescriptor（测试/未接线场景）——直接返回 SelfTestFailed。
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::SelfTestFailed,
                    ErrorPhase::SelfTest,
                    "引擎 self-test 失败",
                    pre_test.failure_reason.clone().unwrap_or_default(),
                ));
            }
        };

        // 更新进度：正在安装
        self.commit_status_internal(engine_id, Some(operation_id), |status| {
            status.operation.stage = OperationStage::Downloading;
        })
        .await?;

        // 执行 InstallTransaction（slot + journal 部署事务）
        let transaction = crate::infra::local_engine::providers::InstallTransaction::new(
            provider_descriptor,
            &self.python_provider,
        );

        let sink_adapter = InstallSinkAdapter::new(
            self.event_port.clone(),
            engine_id.clone(),
            operation_id.to_string(),
        );
        let install_result = transaction
            .execute(
                operation_id,
                preference,
                Some(guard.cancel_token()),
                Some(&sink_adapter),
            )
            .await;

        match install_result {
            Ok(result) => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %result.install_id,
                    operation_id = %result.operation_id,
                    fell_back = result.fell_back,
                    "InstallTransaction 完成"
                );

                // 安装后再次验证 adapter self_test（可能等待 venv python 子进程——阻塞隔离）
                let adapter = Arc::clone(&entry.adapter);
                let self_test = tokio::task::spawn_blocking(move || adapter.self_test())
                    .await
                    .map_err(|e| {
                        LocalEngineError::with_detail(
                            LocalEngineErrorCode::Internal,
                            ErrorPhase::SelfTest,
                            "安装后验证失败",
                            format!("spawn_blocking join 错误: {e}"),
                        )
                    })?;
                if !self_test.passed {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::SelfTestFailed,
                        ErrorPhase::SelfTest,
                        "引擎 self-test 失败",
                        self_test.failure_reason.unwrap_or_default(),
                    ));
                }

                Ok(())
            }
            Err(e) => Err(from_runtime(
                ErrorPhase::Install,
                "环境安装失败（InstallTransaction）",
                &e,
            )),
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
    /// 环境 Ready 判定必须同时满足：
    /// 1. 事务 journal 已按 fail-closed 规则恢复（`DeploymentStore::recover`）
    /// 2. active 指针存在且指向可读 manifest
    /// 3. adapter `self_test` 通过
    /// 缺少任何一项都不标记 Ready。
    ///
    /// **阻塞隔离**：recover（journal 扫描）、read_active（磁盘 IO）、
    /// self_test（venv python 子进程等待）全部在 `spawn_blocking` 内执行，
    /// async 上下文只做状态提交。
    async fn do_probe(&self, engine_id: &EngineId, entry: &EngineEntry) -> Result<(), String> {
        let adapter = Arc::clone(&entry.adapter);
        let eid = engine_id.clone();

        let outcome = tokio::task::spawn_blocking(move || probe_blocking(&eid, &adapter))
            .await
            .map_err(|e| format!("probe spawn_blocking join 错误: {e}"))??;

        match outcome {
            ProbeBlockingOutcome::NoDeployment => {
                tracing::debug!(engine = %engine_id, "探测: 无 deployment.json → Missing");
            }
            ProbeBlockingOutcome::Ready { install_id, slot } => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %install_id,
                    slot = %slot,
                    "探测: active 部署有效 + self_test 通过 → Ready"
                );
                self.commit_status_internal(engine_id, None, |status| {
                    status.environment = EnvironmentHealth::Ready;
                })
                .await
                .map_err(|e| format!("提交 Ready 状态失败: {e}"))?;
            }
            ProbeBlockingOutcome::Broken { reason } => {
                tracing::warn!(
                    engine = %engine_id,
                    reason = %reason,
                    "探测: self_test 失败 → Broken"
                );
                let _ = self
                    .commit_status_internal(engine_id, None, |status| {
                        status.environment = EnvironmentHealth::Broken;
                    })
                    .await;
            }
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
    ///
    /// **阻塞隔离**：read_active（磁盘 IO）与 self_test（子进程等待）在
    /// `spawn_blocking` 内执行。
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

        // 环境未就绪——验证受管部署（deployment.json + manifest）+ self_test。
        // 不能仅凭 self_test 通过就标记 Ready。磁盘 IO 与子进程等待在 blocking 线程。
        let adapter = Arc::clone(&entry.adapter);
        let eid = engine_id.clone();
        let verification = tokio::task::spawn_blocking(move || {
            let has_managed_deployment = match DeploymentStore::read_active(&eid) {
                Ok(Some(_)) => true,
                Ok(None) => false,
                Err(e) => {
                    tracing::warn!(
                        engine = %eid,
                        error = %e,
                        "ensure_installed: 读取 deployment.json 失败"
                    );
                    false
                }
            };
            let self_test = adapter.self_test();
            (has_managed_deployment, self_test)
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "环境检查失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        let (has_managed_deployment, self_test) = verification;
        if has_managed_deployment && self_test.passed {
            // self_test 通过 + 受管 generation 存在 → 标记 Ready
            self.commit_status_internal(engine_id, None, |status| {
                status.environment = EnvironmentHealth::Ready;
            })
            .await?;
            return Ok(());
        }

        // 没有受管 generation 或 self_test 未通过——需要安装
        self.install(engine_id, config).await.map(|_| ())
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
        let ci = entry.current_identity().await;
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

        // claim 进程级操作（与 install/repair/模型操作互斥，key = engine_id）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

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

        // ── 冻结 launch snapshot ──
        // deployment identity + 模型身份在 start 时冻结；配置变化只改变
        // selected，不改变正在运行的 active。
        // read_active（磁盘 IO）+ resolve_expected_model_identity（manifest 读取）
        // 是阻塞操作——在 spawn_blocking 内执行。
        let adapter_for_freeze = Arc::clone(&entry.adapter);
        let eid_for_freeze = engine_id.clone();
        let contract = descriptor.model_contract.clone();
        let uses_managed = adapter_for_freeze.uses_managed_model_storage();
        let selected_model_id = if engine_id.as_str() == super::funasr::FUNASR_ENGINE_ID {
            Some(
                crate::app::stt_config::get_stt_config()
                    .local_engine
                    .funasr_model,
            )
        } else {
            None
        };
        let (deployment_install_id, frozen_model) = tokio::task::spawn_blocking(
            move || -> Result<(String, Option<FrozenModelIdentity>), LocalEngineError> {
                let active = DeploymentStore::read_active(&eid_for_freeze)
                    .map_err(|e| from_runtime(ErrorPhase::Start, "读取 active 部署失败", &e))?;
                let install_id = active
                    .as_ref()
                    .map(|(p, _)| p.install_id.clone())
                    .unwrap_or_default();

                // fail-closed：managed 模型未安装/损坏时不允许 start
                let frozen = match resolve_expected_model_identity(
                    &eid_for_freeze,
                    selected_model_id.as_deref(),
                    &contract,
                    uses_managed,
                ) {
                    Ok((model_id, revision, fingerprint)) => Some(FrozenModelIdentity {
                        model_id,
                        revision,
                        fingerprint,
                    }),
                    Err(reason) => {
                        return Err(LocalEngineError::with_detail(
                            LocalEngineErrorCode::ModelNotReady,
                            ErrorPhase::Start,
                            "模型未就绪，请先安装模型",
                            reason,
                        ));
                    }
                };
                Ok((install_id, frozen))
            },
        )
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Start,
                "冻结启动快照失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })??;

        // ── 启动尝试循环（bind race 有限重试）──
        //
        // probe 空闲与子进程 bind 之间存在竞争：探测后端口可能被其他进程抢走，
        // 子进程 bind 失败即退出。检测到**明确的** address-in-use（见
        // `is_explicit_address_in_use`）时重新分配端口重试，次数由
        // `ConflictRetryPolicy` 封顶；其他任何失败不重试；
        // **永不终止占用端口的未知进程**。
        let retry_policy = ConflictRetryPolicy::default();
        let preferred_port = config.preferred_port.unwrap_or(8100);
        let allocator = EndpointAllocator::with_defaults(preferred_port);
        let mut attempt: usize = 0;

        loop {
            attempt += 1;

            // 分配 endpoint（每次尝试重新探测——此前尝试可能留下新的占用者）
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

            // adapter prepare_launch（可能等待 venv python 子进程检查包——阻塞隔离）
            let adapter_for_launch = Arc::clone(&entry.adapter);
            let config_for_launch = config.clone();
            let resolved_launch = tokio::task::spawn_blocking(move || {
                adapter_for_launch.prepare_launch(&ctx, &config_for_launch)
            })
            .await
            .map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Start,
                    "启动参数准备失败",
                    format!("spawn_blocking join 错误: {e}"),
                )
            })??;

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
            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                status.desired = DesiredState::Running;
                status.process = ProcessState::Starting;
                status.service = ServiceHealth::Unknown;
                // 新一轮显式启动已经接管状态，旧实例的错误不应继续挂在界面上。
                // 本轮若失败，rollback 会写入新的 last_error。
                status.last_error = None;
                status.operation = EngineOperation {
                    kind: OperationKind::Idle,
                    operation_id: String::new(),
                    stage: OperationStage::Pending,
                    cancellable: false,
                };
            })
            .await?;

            // 保存 launch snapshot（identity + profile + deployment + 模型身份）+ 进程句柄
            {
                let mut l = entry.launch.lock().await;
                *l = Some(LaunchSnapshot {
                    identity: identity_input.clone(),
                    profile: resolved_launch.profile.clone(),
                    deployment_install_id: deployment_install_id.clone(),
                    model: frozen_model.clone(),
                });
            }
            {
                let mut mp = entry.managed_process.lock().await;
                *mp = Some(managed.clone());
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
            // 日志实例隔离——每次 start 创建新 CancellationToken，
            // stop/rollback/restart 时 cancel 旧 pump。
            // pump 每条日志 emit 前实时读取 launch snapshot 校验实例归属。
            let pump_token = CancellationToken::new();
            {
                // 先取消旧 pump（restart/retry 场景）
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
                    tracing::info!(engine = %engine_id, pid, attempt, "进程已 spawn，等待 health 验证");

                    // 更新 process=Running（但 service 仍为 Unknown）
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.process = ProcessState::Running { pid };
                        // service 保持 Unknown——需要 health 验证
                    })
                    .await?;

                    // spawn 成功后立即写 lease
                    // 此时 PID、executable、creation_time_ms 均已从 OS 获取，
                    // token_fingerprint 可从 identity_input 计算。
                    // 如果 Blink 在 health 验证期间崩溃，lease 已存在，
                    // 下次启动的恢复扫描能发现此遗留进程。
                    // health 验证失败时，rollback_started_instance 会清理此 lease。
                    self.write_lease_for_engine(
                        engine_id,
                        &managed,
                        &identity_input,
                        &endpoint,
                        &req,
                        &deployment_install_id,
                    )
                    .await;

                    // health 验证——只有 Model Ready 才返回 Ok
                    // 任何失败（timeout/mismatch/backend/ModelFailed/早退）执行统一 rollback
                    match self
                        .verify_engine_health(engine_id, &entry, &identity_input, &managed)
                        .await
                    {
                        Ok(mapping) => {
                            // health 验证通过 + Model Ready——进入 Healthy
                            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
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
                            tracing::info!(
                                engine = %engine_id,
                                instance_id = %instance_id,
                                deployment = %deployment_install_id,
                                "引擎 health 验证通过，Model Ready"
                            );

                            // spawn exit monitor——监听进程意外退出
                            // server crash 后状态必须收敛到 Exited/Unreachable/Failed
                            self.spawn_exit_monitor(
                                engine_id,
                                &managed,
                                &entry,
                                &instance_id,
                                &pkey,
                            );

                            return Ok(());
                        }
                        Err(StartAttemptFailure::BindRace { detail })
                            if retry_policy.should_retry(attempt) =>
                        {
                            // probe-then-bind race——换端口重试（有限次数）
                            let err = LocalEngineError::with_detail(
                                LocalEngineErrorCode::PortConflict,
                                ErrorPhase::Start,
                                "端口被占用，尝试其他端口",
                                detail,
                            );
                            tracing::warn!(
                                engine = %engine_id,
                                attempt,
                                port = endpoint.port(),
                                %err,
                                "bind race：重新分配端口后重试"
                            );
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            continue;
                        }
                        Err(StartAttemptFailure::BindRace { detail }) => {
                            // 重试次数耗尽——结构化 PortConflict 终态
                            let err = LocalEngineError::with_detail(
                                LocalEngineErrorCode::PortConflict,
                                ErrorPhase::Start,
                                "候选端口反复被占用，请检查是否有残留引擎进程",
                                detail,
                            );
                            tracing::error!(engine = %engine_id, attempt, %err, "bind race 重试耗尽");
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            return Err(err);
                        }
                        Err(StartAttemptFailure::Fatal(err)) => {
                            // 任何非 bind-race 失败——统一 rollback，不重试
                            tracing::warn!(engine = %engine_id, %err, "health 验证失败，执行 rollback");
                            self.rollback_started_instance(
                                engine_id,
                                &entry,
                                &pkey,
                                &instance_id,
                                &operation_id,
                                &err,
                            )
                            .await;
                            return Err(err);
                        }
                    }
                }
                Err(e) => {
                    // spawn 失败——直接 rollback（清理已设置的中间状态），不重试
                    let err = from_process(ErrorPhase::Start, "进程启动失败", &e);
                    tracing::warn!(engine = %engine_id, %err, "进程 spawn 失败，执行 rollback");
                    self.rollback_started_instance(
                        engine_id,
                        &entry,
                        &pkey,
                        &instance_id,
                        &operation_id,
                        &err,
                    )
                    .await;
                    return Err(err);
                }
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

        // claim 进程级操作（与其他变更操作互斥）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        self.stop_internal_with_status(engine_id, &entry, &operation_id)
            .await
    }

    /// 无 claim 的停止执行体——供已持有操作 claim 的路径
    /// （install/repair 先停引擎）复用，不产生二级 claim。
    ///
    /// **必须传入 claim 持有者的 operation_id**：状态提交的 operation 门
    /// 以协调器 claim 为真源，二级 id 会被判定为迟到操作而拒绝，
    /// 导致运行中实例实际未被停止。
    async fn stop_internal(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        operation_id: &str,
    ) {
        let _ = self
            .stop_internal_with_status(engine_id, entry, operation_id)
            .await;
    }

    /// 停止执行体（携带用于状态提交的 operation_id）。
    async fn stop_internal_with_status(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        operation_id: &str,
    ) -> Result<(), LocalEngineError> {
        // 幂等检查
        let managed = {
            let mp = entry.managed_process.lock().await;
            mp.clone()
        };

        match managed {
            Some(mp) => {
                // 标记 desired=Stopped, process=Stopping
                self.commit_status_internal(engine_id, Some(operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop().await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.last_error = None;
                        })
                        .await?;

                        // 清理运行实例状态（launch snapshot + pump + registry + lease）
                        self.clear_running_instance(engine_id, entry, true).await;

                        tracing::info!(engine = %engine_id, "引擎已停止");
                        Ok(())
                    }
                    Err(e) => {
                        let err = from_process(ErrorPhase::Stop, "停止失败", &e);
                        self.commit_status_internal(engine_id, Some(operation_id), |status| {
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
                self.commit_status_internal(engine_id, Some(operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    /// 清理运行实例状态：取消日志 pump、删除 lease、清 launch snapshot、
    /// 移除 process registry 条目。
    ///
    /// `remove_lease`: stop/exit 路径删除；`stop_if_current` 条件停止成功后
    /// 同样删除（与 stop 语义一致）。
    async fn clear_running_instance(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        remove_lease_flag: bool,
    ) {
        // 取消旧日志 pump——确保 stop 后旧实例日志不再投影
        {
            let mut lc = entry.log_pump_cancel.lock().await;
            if let Some(cancel) = lc.take() {
                tracing::debug!(engine = %engine_id, "clear_running_instance: 取消日志 pump");
                cancel.cancel();
            }
        }

        // 取出 instance_id 用于 lease 删除与 registry 移除
        let saved_instance_id = entry
            .current_identity()
            .await
            .map(|i| i.instance_id.clone());

        if remove_lease_flag {
            if let Some(ref inst_id) = saved_instance_id {
                if let Err(e) = remove_lease(&engine_id.to_string(), inst_id) {
                    tracing::warn!(
                        engine = %engine_id,
                        instance = %inst_id,
                        %e,
                        "清理实例: 删除 lease 失败（继续清理）"
                    );
                }
            }
        }

        // 清理 launch snapshot + 进程句柄
        {
            let mut l = entry.launch.lock().await;
            if let Some(snapshot) = l.take() {
                tracing::debug!(
                    engine = %engine_id,
                    deployment = %snapshot.deployment_install_id,
                    "清理实例: 释放 launch snapshot（start 冻结的部署绑定至此失效）"
                );
            }
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

        // claim 进程级操作（与其他变更操作互斥）
        let operation_id = generate_operation_id();
        let _guard = self.coordinator.try_claim(engine_id, &operation_id)?;

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
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    status.process = ProcessState::Stopping;
                })
                .await?;

                match mp.stop_if_current(instance_token).await {
                    Ok(()) => {
                        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                            status.process = ProcessState::Stopped;
                            status.service = ServiceHealth::Unknown;
                            status.model = ModelHealth::Unknown;
                            status.last_error = None;
                        })
                        .await?;

                        // 清理运行实例状态（含 lease——条件停止成功即实例终结）
                        self.clear_running_instance(engine_id, &entry, true).await;

                        tracing::info!(engine = %engine_id, "引擎已条件停止（token 匹配）");
                        Ok(())
                    }
                    Err(e) => {
                        let err = from_process(ErrorPhase::Stop, "条件停止失败", &e);
                        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
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
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.desired = DesiredState::Stopped;
                    if status.process == ProcessState::Starting {
                        status.process = ProcessState::Stopped;
                    }
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    // ── repair / cleanup / storage / cancel（0.22.5 H2）─────────────────────

    /// 修复/更新引擎环境。
    ///
    /// repair 是一个完整的部署事务（复用 install 事务体）：
    /// 1. claim 操作（与所有变更互斥）
    /// 2. 停止运行中的实例
    /// 3. 在 candidate slot 中按当前配置重建环境
    /// 4. self-test + 切换后验证；失败自动回滚 previous
    /// 5. 成功删除旧 slot（占用记 residue）
    ///
    /// 不通过原地覆盖 active 部署"修复"。
    ///
    /// **终态协议**：同 `install`——返回 `EnvOperationEndState`，
    /// 取消是正常终态，结束后 `operation` 归位 Idle。
    pub async fn repair(
        &self,
        engine_id: &EngineId,
    ) -> Result<(Option<String>, EnvOperationEndState), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // claim 进程级操作（原子 busy 检查 + 登记）
        let operation_id = generate_operation_id();
        let guard = self.coordinator.try_claim(engine_id, &operation_id)?;

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Repairing,
                operation_id: operation_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: true,
            };
        })
        .await?;

        // 读取当前配置
        let config = self.read_adapter_config_for_engine(engine_id);

        // 无 ProviderDescriptor 时退化为 self_test 验证
        // （self_test 可能等待 venv python 子进程——阻塞隔离）
        if self.provider_descriptors.get(engine_id).is_none() {
            let adapter = Arc::clone(&entry.adapter);
            let self_test = tokio::task::spawn_blocking(move || adapter.self_test())
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Repair,
                        "修复检查失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?;
            if !self_test.passed {
                let err = LocalEngineError::with_detail(
                    LocalEngineErrorCode::SelfTestFailed,
                    ErrorPhase::Repair,
                    "修复后 self-test 仍失败",
                    self_test.failure_reason.unwrap_or_default(),
                );
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.clear_operation();
                })
                .await?;
                return Err(err);
            }

            self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                status.environment = EnvironmentHealth::Ready;
                status.clear_operation();
            })
            .await?;
            tracing::info!(engine = %engine_id, "repair 完成（self-test 降级路径）");
            return Ok((None, EnvOperationEndState::Completed));
        }

        let pre_test = {
            let adapter = Arc::clone(&entry.adapter);
            tokio::task::spawn_blocking(move || adapter.self_test())
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Repair,
                        "修复检查失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?
        };

        // 更新会切换 slot——先停止运行中的实例（复用当前 claim 的 operation_id）
        self.stop_internal(engine_id, &entry, &operation_id).await;

        let result = self
            .install_transaction_locked(
                engine_id,
                &entry,
                &config,
                &pre_test,
                &operation_id,
                &guard,
            )
            .await;

        match result {
            Ok(()) => {
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.clear_operation();
                })
                .await?;
                tracing::info!(engine = %engine_id, "repair 完成（新部署已切换，旧 slot 已清理）");
                Ok((Some(operation_id), EnvOperationEndState::Completed))
            }
            Err(err) => {
                // 取消是正常终态——事务已回滚，不记 last_error
                if guard.is_cancelled() || err.code == LocalEngineErrorCode::Cancelled {
                    self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                        status.clear_operation();
                    })
                    .await?;
                    tracing::info!(engine = %engine_id, op = %operation_id, "repair 已取消（正常终态）");
                    return Ok((Some(operation_id), EnvOperationEndState::Cancelled));
                }
                // 事务内部已回滚——active 部署不受影响
                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.last_error = Some(err.clone());
                    status.clear_operation();
                })
                .await?;
                Err(err)
            }
        }
    }

    /// 清理引擎资产。
    ///
    /// 前端提交 `target_ids`，后端重新解析每个 target_id，不信任前端提交的路径/size/shared/current。
    ///
    /// 禁止提交任意路径。active 部署不可删除。
    /// 共享资产经过 active manifest 引用检查。
    ///
    /// 清理结束后 `operation` 归位 Idle——结果由本返回值表达。
    pub async fn cleanup_targets(
        &self,
        engine_id: &EngineId,
        target_ids: &[String],
        operation_id: Option<String>,
    ) -> Result<super::dto::CleanupResultDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        self.get_entry(engine_id).await?;

        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let _guard = self.coordinator.try_claim(engine_id, &op_id)?;

        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Cleaning,
                operation_id: op_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: false, // cleanup 进入删除阶段后不可取消
            };
        })
        .await?;

        // target 解析 + 磁盘删除（measure/execute_cleanup）都是阻塞 IO——
        // 整体放 spawn_blocking，claim 仍由 guard 持有。
        let eid = engine_id.clone();
        let targets: Vec<String> = target_ids.to_vec();
        let outcomes = tokio::task::spawn_blocking(move || {
            targets
                .into_iter()
                .map(|target_id| {
                    (
                        target_id.clone(),
                        resolve_and_cleanup_target_blocking(&eid, &target_id),
                    )
                })
                .collect::<Vec<_>>()
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Cleanup,
                "清理执行失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        let mut cleaned = Vec::new();
        let mut skipped = Vec::new();
        let mut deferred = Vec::new();
        let mut released: u64 = 0;
        let mut errors = Vec::new();

        for (target_id, outcome) in outcomes {
            match outcome {
                Ok(CleanupTargetOutcome::Cleaned(bytes)) => {
                    released += bytes;
                    cleaned.push(target_id);
                }
                Ok(CleanupTargetOutcome::Deferred(bytes)) => {
                    // Windows 文件占用等——slot 记 residue，等待后续清理
                    released += bytes;
                    deferred.push(target_id);
                }
                Err(e) => {
                    let reason = e.to_string();
                    tracing::warn!(
                        engine = %engine_id,
                        target = %target_id,
                        error = %reason,
                        "cleanup 跳过"
                    );
                    errors.push(format!("{target_id}: {reason}"));
                    skipped.push(target_id);
                }
            }
        }

        // 终态：归位 Idle——清理结果由返回值表达，不留 busy 残留
        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.clear_operation();
        })
        .await?;

        Ok(super::dto::CleanupResultDto {
            engine_id: engine_id.to_string(),
            operation_id: op_id,
            cleaned_target_ids: cleaned,
            skipped_target_ids: skipped,
            released_bytes: released,
            deferred_target_ids: deferred,
            error: if errors.is_empty() {
                None
            } else {
                Some(errors.join("; "))
            },
        })
    }

    /// 取消操作。
    ///
    /// 只取消完全匹配 `operation_id` 的活跃 claim token。
    /// **取消是正常协议语义**——返回 `CancelOutcome`，不用错误类型表达：
    /// - claim 由 worker 的 RAII guard 持有——cancel 后 claim 不释放，
    ///   直到 worker 真正结束才允许下一个操作；
    /// - 已完成的 operation 不再是 busy state → `NoActiveOperation`；
    /// - 错配的 operation_id → `Mismatched`，不触发任何 token。
    pub async fn cancel_operation(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> CancelOutcome {
        if let Err(e) = self.validate_engine_id(engine_id) {
            tracing::warn!(engine = %engine_id, %e, "取消请求 engine_id 无效");
            return CancelOutcome::NoActiveOperation;
        }

        let outcome = self.coordinator.cancel(engine_id, operation_id);
        if outcome.is_cancelled() {
            tracing::info!(
                engine = %engine_id,
                op = %operation_id,
                "操作取消信号已发送（worker 结束前 claim 不释放）"
            );
        } else {
            tracing::info!(
                engine = %engine_id,
                op = %operation_id,
                outcome = ?outcome,
                "取消请求未命中活跃操作"
            );
        }
        outcome
    }

    /// 扫描引擎存储——返回所有可诊断/可清理的存储目标。
    ///
    /// 在 `spawn_blocking` 中执行，不阻塞 Tauri 事件线程或启动主链路。
    pub async fn scan_storage(
        &self,
        engine_id: &EngineId,
    ) -> Result<super::dto::EngineStorageDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;

        let engine_id_owned = engine_id.clone();
        let result =
            tokio::task::spawn_blocking(move || scan_engine_storage_blocking(&engine_id_owned))
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Request,
                        "存储扫描失败",
                        format!("spawn_blocking panic: {e}"),
                    )
                })?;

        result.map_err(|e| from_runtime(ErrorPhase::Request, "存储扫描失败", &e))
    }

    /// 从配置真源读取 AdapterConfig。
    ///
    /// 真源在 [`super::config_source`]——commands/maintenance/wiring 与本服务
    /// 共用同一构造入口，避免归一化规则（如 funasr device=cuda→Cpu）漂移。
    fn read_adapter_config_for_engine(&self, engine_id: &EngineId) -> AdapterConfig {
        super::config_source::adapter_config_for_engine(engine_id)
            .unwrap_or_else(AdapterConfig::new)
    }

    /// 解析 target_id 并执行清理（阻塞——须在 spawn_blocking 中调用）。
    ///
    /// target_id 格式：
    /// - `slot:{slot}` — 非 active 部署 slot（residue 感知：占用记残留）
    /// - `staging` — 孤儿 staging
    /// - `model_cache` — 引擎模型缓存
    /// - `shared:{runtime_kind}:{artifact_id}` — provider 共享 artifact
    /// - `download_cache:{runtime_kind}` — provider 下载缓存
    /// - `legacy:{kind}` — 旧版遗留资产（拒绝自动清理）
    fn resolve_and_cleanup_target(
        &self,
        engine_id: &EngineId,
        target_id: &str,
    ) -> Result<CleanupTargetOutcome, crate::infra::local_engine::runtime::RuntimeError> {
        resolve_and_cleanup_target_blocking(engine_id, target_id)
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

    // ── 结构化日志（0.22.5 H1）──────────────────────────────────────────────

    /// 查询引擎结构化日志（含 instance_id + seq）。
    ///
    /// 返回 `Vec<StructuredLogEntry>`，每条包含 `engine_id`、`instance_id`、
    /// `seq`、`timestamp_ms`、`level`、`text`。
    ///
    /// 历史与 `LOCAL_ENGINE_LOG` 实时事件使用同一 shape。
    /// 如果引擎未运行但有 `last_managed_process`，从上一实例读取 bounded history。
    pub async fn get_logs_structured(
        &self,
        engine_id: &EngineId,
        max_lines: usize,
    ) -> Result<Vec<StructuredLogEntry>, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;

        // 获取当前实例的 instance_id
        let instance_id = entry
            .current_identity()
            .await
            .map(|i| i.instance_id.clone());

        // 优先从当前运行实例读取
        let mp = entry.managed_process.lock().await;
        if let Some(managed) = mp.as_ref() {
            let history = managed.log_history().await;
            let mut logs: Vec<StructuredLogEntry> = history
                .into_iter()
                .rev()
                .take(max_lines)
                .map(|entry| StructuredLogEntry {
                    engine_id: engine_id.to_string(),
                    instance_id: instance_id.clone().unwrap_or_default(),
                    seq: entry.seq,
                    timestamp_ms: entry.timestamp_ms,
                    level: classify_engine_log(entry.source, &entry.text).to_string(),
                    text: entry.text,
                })
                .collect();
            // ring buffer 是正序；先从尾部截取，再恢复为正序供 UI 与实时事件拼接。
            logs.reverse();
            return Ok(logs);
        }
        drop(mp);

        // fallback: 从上一实例读取
        let last_mp = entry.last_managed_process.lock().await;
        if let Some(managed) = last_mp.as_ref() {
            let history = managed.log_history().await;
            let mut logs: Vec<StructuredLogEntry> = history
                .into_iter()
                .rev()
                .take(max_lines)
                .map(|entry| StructuredLogEntry {
                    engine_id: engine_id.to_string(),
                    instance_id: instance_id.clone().unwrap_or_default(),
                    seq: entry.seq,
                    timestamp_ms: entry.timestamp_ms,
                    level: classify_engine_log(entry.source, &entry.text).to_string(),
                    text: entry.text,
                })
                .collect();
            logs.reverse();
            return Ok(logs);
        }

        Ok(Vec::new())
    }

    // ── provider descriptor / provider 访问（0.22.5 H1）─────────────────────

    /// 返回指定引擎的 `ProviderDescriptor` 引用。
    ///
    /// 用于 catalog 兼容性检查——commands 从 `ProviderDescriptor.profiles`
    /// + `RuntimeProvider::check_compatibility` 获取真源兼容性。
    pub fn provider_descriptor_for_engine(
        &self,
        engine_id: &EngineId,
    ) -> Option<&ProviderDescriptor> {
        self.provider_descriptors.get(engine_id)
    }

    /// 返回 `PythonVenvProvider` 引用。
    ///
    /// 用于 catalog 兼容性检查——commands 调用
    /// `RuntimeProvider::check_compatibility` 判定本机兼容性。
    pub fn python_provider(&self) -> &PythonVenvProvider {
        &self.python_provider
    }

    // ── mark_needs_rebuild（0.22.5 H2）──────────────────────────────────────

    /// 标记引擎环境为 `NeedsRebuild`。
    ///
    /// 当用户在偏好页面切换 compute profile（如 CPU → CUDA）时，
    /// 旧 generation 不能继续当作新 profile Ready。
    /// 此方法将环境投影为 `NeedsRebuild`，并广播状态事件。
    ///
    /// **不启动安装、不停止进程**——只投影状态。
    /// 用户点击修复/重建后走现有事务生成新 generation。
    pub async fn mark_needs_rebuild(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.commit_status_internal(engine_id, None, |status| {
            status.environment = EnvironmentHealth::NeedsRebuild;
        })
        .await
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
                    let err = from_process(ErrorPhase::Stop, "shutdown_all 回收失败", &e);
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

    // ── stop_orphan_engine（0.22.6.6）─────────────────────────────────────

    /// 手动停止孤儿引擎进程。
    ///
    /// 当 lease 恢复扫描发现遗留进程时，用户可通过设置页手动调用此方法终止。
    ///
    /// **安全策略**（fail-closed）：
    /// 1. 扫描 lease 文件，查找指定 engine 的 lease
    /// 2. 使用 `build_process_evidence` 查询 OS 进程身份
    /// 3. 使用 `probe_health_evidence` 探测 health 端点
    /// 4. 调用 `decide_recovery` 纯函数做恢复判定
    /// 5. 如果判定为 `Adoptable`，使用 `kill_process_tree_verified` 验证身份后终止
    /// 6. 终止后清除 lease 文件
    ///
    /// 证据不足时返回错误，不降级为仅 PID kill。
    pub async fn stop_orphan_engine(
        &self,
        engine_id: &EngineId,
    ) -> Result<super::dto::OrphanStopResultDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let engine_id_str = engine_id.to_string();

        // 1. 扫描 lease 文件，查找匹配的 lease
        let leases =
            tokio::task::spawn_blocking(|| crate::infra::local_engine::lease::scan_leases())
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Request,
                        "扫描 lease 失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?;

        let lease = leases
            .iter()
            .find(|l| l.engine_id == engine_id_str)
            .cloned();

        let lease = match lease {
            Some(l) => l,
            None => {
                return Ok(super::dto::OrphanStopResultDto {
                    engine_id: engine_id_str,
                    stopped: false,
                    reason: "lease_not_found".to_string(),
                    detail: Some("未找到该引擎的 lease 文件".to_string()),
                });
            }
        };

        // 2. 在 spawn_blocking 中构建进程证据
        let pid = lease.pid;
        let process_evidence = tokio::task::spawn_blocking(move || {
            crate::infra::local_engine::lease_recovery::build_process_evidence(pid)
        })
        .await
        .map_err(|e| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Request,
                "查询进程证据失败",
                format!("spawn_blocking join 错误: {e}"),
            )
        })?;

        // 3. 异步探测 health 端点
        let health_evidence =
            crate::infra::local_engine::lease_recovery::probe_health_evidence(&lease.endpoint)
                .await;

        // 4. 调用 decide_recovery 做恢复判定
        let decision = crate::infra::local_engine::lease::decide_recovery(
            &lease,
            &process_evidence,
            health_evidence.as_ref(),
        );

        let result = match &decision {
            crate::infra::local_engine::lease::RecoveryDecision::Adoptable { pid, .. } => {
                // 5. 使用 kill_process_tree_verified 验证身份后终止
                let expected_exe = std::path::PathBuf::from(&lease.executable);
                let expected_creation = lease.creation_time_ms;
                let pid_val = *pid;

                let kill_result = tokio::task::spawn_blocking(move || {
                    crate::infra::platform::process::kill_process_tree_verified(
                        pid_val,
                        &expected_exe,
                        expected_creation,
                    )
                })
                .await
                .map_err(|e| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Stop,
                        "终止进程失败",
                        format!("spawn_blocking join 错误: {e}"),
                    )
                })?;

                match kill_result {
                    Ok(()) => {
                        // 6. 清除 lease 文件
                        if let Err(e) =
                            crate::infra::local_engine::lease::remove_lease_force(&engine_id_str)
                        {
                            tracing::warn!(
                                engine = %engine_id_str,
                                %e,
                                "孤儿进程已终止但清除 lease 失败"
                            );
                        }

                        super::dto::OrphanStopResultDto {
                            engine_id: engine_id_str,
                            stopped: true,
                            reason: "adoptable_killed".to_string(),
                            detail: Some(format!("进程 {} 已验证身份并终止", lease.pid)),
                        }
                    }
                    Err(e) => super::dto::OrphanStopResultDto {
                        engine_id: engine_id_str,
                        stopped: false,
                        reason: "kill_failed".to_string(),
                        detail: Some(format!("终止进程失败: {e}")),
                    },
                }
            }
            crate::infra::local_engine::lease::RecoveryDecision::DoNotAdopt(diag) => {
                let reason_str = match &diag.reason {
                    crate::infra::local_engine::lease::RecoveryReason::PidNotFound => {
                        // 进程已退出，清除 stale lease
                        if let Err(e) =
                            crate::infra::local_engine::lease::remove_lease_force(&engine_id_str)
                        {
                            tracing::warn!(
                                engine = %engine_id_str,
                                %e,
                                "PID 不存在但清除 lease 失败"
                            );
                        }
                        "pid_not_exist".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::ExecutableMismatch {
                        ..
                    } => "executable_mismatch".to_string(),
                    crate::infra::local_engine::lease::RecoveryReason::CreationTimeMismatch {
                        ..
                    } => "creation_time_mismatch".to_string(),
                    crate::infra::local_engine::lease::RecoveryReason::CreationTimeMissing => {
                        "creation_time_missing".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::ProcessQueryFailed => {
                        "process_query_failed".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::TokenFingerprintMismatch => {
                        "token_fingerprint_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::InstanceIdMismatch => {
                        "instance_id_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::EngineIdMismatch => {
                        "engine_id_mismatch".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::HealthUnreachable => {
                        "health_unreachable".to_string()
                    }
                    crate::infra::local_engine::lease::RecoveryReason::SchemaVersion { .. } => {
                        "schema_version_mismatch".to_string()
                    }
                };

                super::dto::OrphanStopResultDto {
                    engine_id: engine_id_str,
                    stopped: false,
                    reason: reason_str,
                    detail: Some(diag.detail.clone()),
                }
            }
        };

        Ok(result)
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
    /// 2. 验证 operation_id（busy 真源 = `EngineOperationCoordinator` 的 claim）
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

        // 验证 operation_id（fail-closed）——活跃 claim 唯一真源是协调器
        let current_op_id = self.coordinator.active_operation(engine_id);
        match (&current_op_id, operation_id) {
            (Some(current), Some(submitted)) => {
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
            }
            // 有活跃操作但提交未携带 operation_id → 拒绝
            (Some(current), None) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作进行中，请等待",
                    format!("有活跃操作但提交未携带 operation_id (current={current})"),
                ));
            }
            // 无活跃操作但提交携带 operation_id → fail-closed 拒绝
            // 防止迟到的任务（已取消/已失败的 operation）覆写新状态
            (None, Some(submitted)) => {
                tracing::warn!(
                    engine = %engine_id,
                    submitted_op = %submitted,
                    "提交携带 operation_id 但无活跃操作，拒绝（fail-closed）"
                );
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作已过期",
                    format!(
                        "提交携带 operation_id={submitted} 但无活跃操作（可能已取消/完成/失败）"
                    ),
                ));
            }
            // 无活跃操作且提交不携带 operation_id → 允许（非操作状态转换）
            (None, None) => {}
        }

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

    /// 解析 compute profile（从 descriptor 声明的候选列表中选择）。
    fn resolve_profile(
        &self,
        descriptor: &EngineDefinition,
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
    ///
    /// **进程早退快速失败（0.22.6 phase B）**：每次轮询前检查进程状态——
    /// 已退出时不等满 start_timeout，按输出尾部分类：
    /// - 明确的 address-in-use → `StartAttemptFailure::BindRace`（可换端口重试）；
    /// - 其他任何退出 → `StartAttemptFailure::Fatal`（附输出尾部，便于诊断）。
    async fn verify_engine_health(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        identity_input: &ServiceIdentityInput,
        managed: &Arc<ManagedProcess>,
    ) -> Result<HealthMapping, StartAttemptFailure> {
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
                StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Health,
                    "HTTP client 构造失败",
                    format!("{e}"),
                ))
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

            // ── 进程早退快速失败 + bind race 识别 ──
            // 子进程 bind 失败（probe-then-bind race）或其他启动期崩溃会立即退出；
            // 等满 start_timeout 才报错会无谓拖慢失败路径。
            {
                let snapshot = managed.snapshot().await;
                if let ProcessStatus::Exited { reason } = snapshot.status {
                    let tail: Vec<String> = managed
                        .log_history()
                        .await
                        .into_iter()
                        .rev()
                        .take(30)
                        .map(|l| l.text)
                        .collect();
                    let reason_text = format!("{reason:?}");
                    let tail_text = tail.join("\n");
                    if is_explicit_address_in_use(&reason_text)
                        || is_explicit_address_in_use(&tail_text)
                    {
                        return Err(StartAttemptFailure::BindRace {
                            detail: format!(
                                "子进程退出（{reason_text:?}），输出包含明确的地址占用错误；输出尾部:\n{tail_text}"
                            ),
                        });
                    }
                    return Err(StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                        LocalEngineErrorCode::SpawnFailed,
                        ErrorPhase::Start,
                        "引擎进程启动后立即退出",
                        format!("退出原因: {reason_text}; 输出尾部:\n{tail_text}"),
                    )));
                }
            }

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
                return Err(StartAttemptFailure::Fatal(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Timeout,
                    ErrorPhase::Health,
                    "health 验证超时",
                    format!("{phase} 阶段在 {attempt} 次尝试后未通过"),
                )));
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
                            return Err(StartAttemptFailure::Fatal(err));
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
        // 期望身份来自 **start 时冻结的 launch snapshot**——配置变化（selected
        // 改变）不影响正在运行的 active；删除模型也不影响本次运行的校验合同。
        // adapter 自管模型的引擎（snapshot.model = None）使用编译期 descriptor
        // 身份，fingerprint 由 health 契约负责校验。
        let descriptor = entry.adapter.descriptor();
        let engine_id = &descriptor.engine_id;

        let launch = entry.current_launch().await;
        let (expected_model_id, expected_revision, expected_fingerprint) = match launch.as_ref() {
            Some(snap) => match &snap.model {
                Some(m) => (
                    m.model_id.clone(),
                    m.revision.clone(),
                    m.fingerprint.clone(),
                ),
                None => (
                    descriptor.model_contract.model_id.clone(),
                    descriptor.model_contract.revision.clone(),
                    None,
                ),
            },
            None => {
                // launch snapshot 缺失（不应发生——start 后由 claim 保护）——fail-closed
                let _ = engine_id;
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Health,
                    "运行实例状态缺失",
                    "health 验证期间 launch snapshot 不存在",
                ));
            }
        };

        if mapping.model == ModelHealth::Ready {
            match mapping.model_id.as_deref() {
                Some(health_model_id) if health_model_id == expected_model_id => {}
                Some(health_model_id) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_id 不匹配",
                        format!(
                            "health 报告 model_id='{health_model_id}'，期望='{expected_model_id}'"
                        ),
                    ));
                }
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 model_id",
                        "health 报告 model=Ready 但 model_id 为 None",
                    ));
                }
            }

            match mapping.model_revision.as_deref() {
                Some(health_revision) if health_revision == expected_revision => {}
                Some(health_revision) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_revision 不匹配",
                        format!(
                            "health 报告 model_revision='{health_revision}'，期望='{expected_revision}'"
                        ),
                    ));
                }
                None => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "模型 Ready 但缺少 model_revision",
                        "health 报告 model=Ready 但 model_revision 为 None",
                    ));
                }
            }
        } else {
            if let Some(ref health_model_id) = mapping.model_id {
                if health_model_id != &expected_model_id {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_id 不匹配",
                        format!(
                            "health 报告 model_id='{health_model_id}'，期望='{expected_model_id}'"
                        ),
                    ));
                }
            }

            if let Some(ref health_revision) = mapping.model_revision {
                if health_revision != &expected_revision {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "model_revision 不匹配",
                        format!(
                            "health 报告 model_revision='{health_revision}'，期望='{expected_revision}'"
                        ),
                    ));
                }
            }
        }

        // Ready 必须有合法 64-hex fingerprint；managed 模式还必须与 manifest 一致。
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
                Some(fp) if !is_valid_model_fingerprint(fp) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型 Ready 但 fingerprint 无效",
                        "health 报告 model=Ready 但 model_content_fingerprint 不是 64 位小写 hex",
                    ));
                }
                Some(fp)
                    if expected_fingerprint
                        .as_ref()
                        .is_some_and(|expected| fp != expected) =>
                {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::IdentityVerification,
                        ErrorPhase::Health,
                        "fingerprint 不匹配",
                        format!(
                            "health 报告 fingerprint='{fp}'，manifest 期望='{}'",
                            expected_fingerprint.as_deref().unwrap_or_default()
                        ),
                    ));
                }
                _ => {}
            }
        }

        // backend 一致性验证——期望来自 launch snapshot 冻结的 profile
        if let Some(ref obs) = mapping.backend {
            let profile = entry.current_profile().await;
            if let Some(ref profile) = profile {
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

    // ── lease 写入辅助（0.22.6.1） ─────────────────────────────────────────

    /// 为引擎实例写入持久化 lease。
    ///
    /// 在 spawn 成功后立即调用——从 `ManagedProcess` 获取进程身份
    /// （PID、可执行路径、创建时间），从 `ServiceIdentityInput` 获取
    /// token fingerprint；`deployment_id` 使用 start 时冻结的
    /// launch snapshot 中的部署 install_id。
    ///
    /// 写入失败只打 warn 日志，不影响 start 成功返回（lease 是辅助证据，
    /// 不是运行时强依赖）。
    async fn write_lease_for_engine(
        &self,
        engine_id: &EngineId,
        managed: &Arc<ManagedProcess>,
        identity_input: &ServiceIdentityInput,
        endpoint: &crate::infra::local_engine::port::Endpoint,
        #[allow(unused_variables)] req: &LaunchRequest,
        deployment_id: &str,
    ) {
        // 从 ManagedProcess 获取进程身份
        let snapshot = managed.snapshot().await;
        let identity = match &snapshot.identity {
            Some(id) => id,
            None => {
                tracing::warn!(
                    engine = %engine_id,
                    "write_lease: ManagedProcess 无 identity，跳过 lease 写入"
                );
                return;
            }
        };

        let lease = build_process_lease(
            engine_id,
            identity,
            identity_input,
            endpoint,
            deployment_id.to_string(),
        );

        // lease 文件写入是同步 IO——挪到 blocking 线程，不占 async worker。
        if let Err(e) = tokio::task::spawn_blocking(move || write_lease(&lease)).await {
            tracing::warn!(
                engine = %engine_id,
                instance = %identity.instance_id,
                error = %e,
                "write_lease: 写入 lease 失败（不影响运行时）"
            );
        }
    }

    // ── exit monitor（0.22.6.3）─────────────────────────────────────────────

    /// Spawn 进程退出监听 task。
    ///
    /// 在 health 验证通过后调用，监听 `ManagedProcess` 的状态变更。
    /// 当收到 `ProcessStatus::Exited` 时执行 `handle_process_exit`。
    ///
    /// **设计约束**：
    /// - task 内不持有 `&self`（生命周期不够），只持有 Arc 字段
    /// - task 内直接操作 `entry.status` 的 RwLock（等价于 commit_status_internal
    ///   但不检查 operation_id——exit 事件是异步到达的，不经过 op_gate）
    /// - 通过 `entry.managed_process` 的 instance_id 验证：如果已 restart，
    ///   旧 monitor 的 exit 事件不会覆盖新实例状态
    fn spawn_exit_monitor(
        &self,
        engine_id: &EngineId,
        managed: &Arc<ManagedProcess>,
        entry: &Arc<EngineEntry>,
        instance_id: &str,
        pkey: &ProcessKey,
    ) {
        let engine_id = engine_id.clone();
        let managed = Arc::clone(managed);
        let entry = Arc::clone(entry);
        let event_port = Arc::clone(&self.event_port);
        let epoch = self.epoch.clone();
        let instance_id = instance_id.to_string();
        let pkey = pkey.clone();
        // 0.22.6.4: 传入 process_registry 的 Arc 引用，使 exit monitor
        // 能在验证身份后移除对应 ProcessKey 条目，避免 registry 泄漏。
        // 不形成强引用环：registry 是 EngineManager 拥有的 Mutex<HashMap>，
        // 这里克隆的是 Arc 到同一 Mutex 的引用，不持有 EngineManager 自身。
        let process_registry = Arc::clone(&self.process_registry);

        tokio::spawn(async move {
            let mut rx = managed.subscribe_status();

            loop {
                if rx.changed().await.is_err() {
                    // sender dropped——进程已清理，退出 monitor
                    break;
                }

                let status = rx.borrow().clone();
                if !status.is_exited() {
                    continue;
                }

                tracing::warn!(
                    engine = %engine_id,
                    instance = %instance_id,
                    status = ?status,
                    "exit monitor: 收到进程退出事件"
                );

                // 0.22.6.3: 验证此 managed 仍是 entry 的当前实例
                // 如果已 restart（新 start 替换了 managed_process），旧 exit 事件不生效
                let is_current = {
                    let mp = entry.managed_process.lock().await;
                    if let Some(ref current) = *mp {
                        current
                            .is_current_token(&managed.current_token().await)
                            .await
                    } else {
                        false
                    }
                };

                if !is_current {
                    tracing::info!(
                        engine = %engine_id,
                        instance = %instance_id,
                        "exit monitor: managed 已不是当前实例（可能 restart），忽略旧 exit 事件"
                    );
                    break;
                }

                // 收到 exit 事件且验证为当前实例——执行状态收敛
                let exit_reason = match &status {
                    ProcessStatus::Exited { reason } => format!("{reason:?}"),
                    _ => unreachable!(),
                };

                // 取消旧日志 pump——确保退出后旧实例日志不再投影
                {
                    let mut lc = entry.log_pump_cancel.lock().await;
                    if let Some(cancel) = lc.take() {
                        tracing::debug!(engine = %engine_id, "exit monitor: 取消日志 pump");
                        cancel.cancel();
                    }
                }

                // 取出 instance_id 用于 lease 删除
                let saved_instance_id = entry
                    .current_identity()
                    .await
                    .map(|i| i.instance_id.clone());

                // 删除 lease
                if let Some(ref inst_id) = saved_instance_id {
                    if let Err(e) = remove_lease(&engine_id.to_string(), inst_id) {
                        tracing::warn!(
                            engine = %engine_id,
                            instance = %inst_id,
                            %e,
                            "exit monitor: 删除 lease 失败（继续清理）"
                        );
                    }
                }

                // 清理 launch snapshot + 进程句柄
                {
                    let mut l = entry.launch.lock().await;
                    *l = None;
                }
                {
                    let mut mp = entry.managed_process.lock().await;
                    *mp = None;
                }

                // 0.22.6.4: 从 process_registry 移除——exit monitor 持有
                // process_registry 的 Arc 引用，在验证身份后安全移除。
                // is_current 检查已确保不会误删新实例的条目。
                {
                    let mut reg = process_registry.lock().unwrap();
                    reg.remove(&pkey);
                    tracing::debug!(
                        engine = %engine_id,
                        instance = %instance_id,
                        "exit monitor: 已从 process_registry 移除"
                    );
                }

                // 置错误终态：process=Exited, service=Unreachable, model=Unknown
                {
                    let mut status_guard = entry.status.write().await;

                    // epoch 验证——新 epoch 重置状态
                    if status_guard.service_epoch != epoch {
                        *status_guard = EngineStatus {
                            service_epoch: epoch.clone(),
                            ..Default::default()
                        };
                    }

                    let new_revision = status_guard.revision + 1;
                    let exit_err = LocalEngineError::with_detail(
                        LocalEngineErrorCode::NotRunning,
                        ErrorPhase::Stop,
                        "进程意外退出",
                        exit_reason.clone(),
                    );

                    status_guard.desired = DesiredState::Stopped;
                    status_guard.process = ProcessState::Exited {
                        reason: exit_reason,
                    };
                    status_guard.service = ServiceHealth::Unreachable;
                    status_guard.model = ModelHealth::Unknown;
                    status_guard.last_error = Some(exit_err);
                    status_guard.revision = new_revision;

                    // 广播状态变更
                    let snapshot = EngineStatusSnapshot {
                        engine_id: engine_id.clone(),
                        service_epoch: epoch,
                        revision: new_revision,
                        status: status_guard.clone(),
                    };
                    event_port.emit_status(&snapshot);
                }

                tracing::warn!(
                    engine = %engine_id,
                    instance = %instance_id,
                    "exit monitor: 状态已收敛到 Exited/Unreachable，current identity 已清理"
                );

                // exit 事件只处理一次
                break;
            }
        });
    }

    // ── rollback ────────────────────────────────────────────────────────────

    /// 统一回滚已启动实例——start 失败时调用。
    ///
    /// 清理项：
    /// 1. 停止 ManagedProcess（如果存在）
    /// 2. 清理 launch snapshot / 日志 pump / lease / process registry
    /// 3. 置错误终态（process=Exited, service=Unreachable, last_error=err）
    ///
    /// **不回滚部署**：部署完整性由安装事务的切换后验证保证；
    /// 进程启动失败（端口冲突/超时等）是进程生命周期问题，
    /// 不构成部署回滚条件（旧 slot 已在事务成功时删除）。
    async fn rollback_started_instance(
        &self,
        engine_id: &EngineId,
        entry: &Arc<EngineEntry>,
        _pkey: &ProcessKey,
        instance_id: &str,
        operation_id: &str,
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

        // 清理运行实例状态（pump/lease/launch snapshot/registry）
        self.clear_running_instance(engine_id, entry, true).await;

        // 置错误终态。
        // 必须携带 start claim 的 operation_id 提交——start 的 claim 仍由
        // _guard 持有，不带 id（或带错 id）的提交会被 operation 门拒绝，
        // 导致 Exited/Unreachable 终态不落地、快照停留在 Running。
        let _ = self
            .commit_status_internal(engine_id, Some(operation_id), |status| {
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

    // ── 模型资产操作（从 ModelService 并入，单一业务真相）──────────────────
    //
    // 语义变化：
    // - 变更互斥由 EngineOperationCoordinator 承载（key = engine_id）——
    //   同一引擎的模型安装与环境安装/修复/启动/停止互斥；
    // - 删除冲突检查依据 **launch snapshot**（active）与配置真源（selected），
    //   不再用当前配置猜测运行中的模型；
    // - descriptor 默认模型只提供首次默认值，不构成删除保护。

    /// 读取引擎当前 selected 模型（配置真源）。
    fn read_selected_model(&self, engine_id: &EngineId) -> Option<String> {
        if engine_id.as_str() == super::funasr::FUNASR_ENGINE_ID {
            let m = crate::app::stt_config::get_stt_config()
                .local_engine
                .funasr_model;
            if m.is_empty() { None } else { Some(m) }
        } else {
            None
        }
    }

    /// 检查模型删除冲突（selected / active launch snapshot）。
    ///
    /// active 判定依据 launch snapshot 冻结的模型身份与 instance_id——
    /// 不根据当前配置猜测。descriptor 默认模型不构成冲突。
    async fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict> {
        let mut reasons = Vec::new();

        // selected（配置真源）
        if self.read_selected_model(engine_id).as_deref() == Some(model_id) {
            reasons.push(DeleteConflictReason::ReferencedByConfig {
                config_field: "funasr_model".to_string(),
                config_value: model_id.to_string(),
            });
        }

        // active（launch snapshot）
        if let Ok(entry) = self.get_entry(engine_id).await {
            if let Some(launch) = entry.current_launch().await {
                if let Some(ref m) = launch.model {
                    if m.model_id == model_id {
                        reasons.push(DeleteConflictReason::ActiveInRunningInstance {
                            instance_id: launch.identity.instance_id.clone(),
                        });
                    }
                }
            }
        }

        if reasons.is_empty() {
            None
        } else {
            Some(ModelDeleteConflict {
                engine_id: engine_id.clone(),
                model_id: model_id.to_string(),
                reasons,
            })
        }
    }

    /// 列出引擎的所有模型候选及其当前状态（只读查询，无副作用）。
    ///
    /// 状态从磁盘 manifest 结构恢复（不做全量 hash）；
    /// is_selected 来自配置，is_active 来自 launch snapshot。
    pub async fn list_models(&self, engine_id: &EngineId) -> Vec<EngineModelStatus> {
        let descriptors = self.model_registry.list(engine_id);
        let selected = self.read_selected_model(engine_id);
        let launch_model = match self.get_entry(engine_id).await {
            Ok(entry) => entry
                .current_launch()
                .await
                .and_then(|l| l.model.map(|m| m.model_id)),
            Err(_) => None,
        };

        descriptors
            .iter()
            .map(|desc| {
                let asset_key = mstore::encode_asset_key(&desc.model_id);
                let mut status = match mstore::restore_model_state(engine_id, &asset_key) {
                    Ok(mstore::RestoredModelState::Installed { manifest, .. }) => {
                        let mut st = EngineModelStatus::not_installed(desc);
                        st.install_state = ModelInstallState::Installed;
                        st.verification_state = ModelVerificationState::Unverified;
                        st.cache_size_bytes = Some(manifest.payload_size_bytes);
                        st
                    }
                    Ok(mstore::RestoredModelState::Corrupted { .. }) => {
                        let mut st = EngineModelStatus::not_installed(desc);
                        st.install_state = ModelInstallState::NotInstalled;
                        st.verification_state = ModelVerificationState::Corrupted;
                        st.compatibility = ModelCompatibility::Unknown;
                        st
                    }
                    _ => EngineModelStatus::not_installed(desc),
                };
                status.is_selected = selected.as_deref() == Some(desc.model_id.as_str());
                status.is_active = launch_model.as_deref() == Some(desc.model_id.as_str());
                status
            })
            .collect()
    }

    /// 列出引擎**可选**（已安装、校验可用、当前兼容）的模型。
    ///
    /// "什么模型可选"是业务规则——由 EngineManager（单一业务真相）过滤，
    /// STT 选择入口（command 层）只做参数适配与投影，不复制过滤规则。
    ///
    /// 返回 `(descriptor, status)` 对；`is_selected` 已按配置真源填充。
    pub async fn list_selectable_models(
        &self,
        engine_id: &EngineId,
    ) -> Result<Vec<(EngineModelDescriptor, EngineModelStatus)>, LocalEngineError> {
        let models = self.list_models(engine_id).await;
        let mut result = Vec::new();
        for status in models {
            if !status.is_usable() {
                continue;
            }
            if !matches!(
                status.compatibility,
                ModelCompatibility::Compatible | ModelCompatibility::Unknown
            ) {
                continue;
            }
            let desc = self
                .model_registry
                .find(engine_id, &status.model_id)
                .ok_or_else(|| {
                    LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Request,
                        "模型目录不一致",
                        format!(
                            "engine_id={}, model_id={} 有状态但无 descriptor",
                            engine_id, status.model_id
                        ),
                    )
                })?;
            result.push((desc.clone(), status));
        }
        Ok(result)
    }

    /// 获取单个模型状态。
    pub async fn get_model_status(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<EngineModelStatus, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let asset_key = mstore::encode_asset_key(model_id);
        let mut status = match mstore::restore_model_state(engine_id, &asset_key) {
            Ok(mstore::RestoredModelState::Installed { manifest, .. }) => {
                let mut st = EngineModelStatus::not_installed(desc);
                st.install_state = ModelInstallState::Installed;
                st.verification_state = ModelVerificationState::Unverified;
                st.cache_size_bytes = Some(manifest.payload_size_bytes);
                st
            }
            Ok(mstore::RestoredModelState::Corrupted { .. }) => {
                let mut st = EngineModelStatus::not_installed(desc);
                st.verification_state = ModelVerificationState::Corrupted;
                st.compatibility = ModelCompatibility::Unknown;
                st
            }
            _ => EngineModelStatus::not_installed(desc),
        };
        status.is_selected = self.read_selected_model(engine_id).as_deref() == Some(model_id);
        if let Ok(entry) = self.get_entry(engine_id).await {
            if let Some(launch) = entry.current_launch().await {
                if let Some(ref m) = launch.model {
                    status.is_active = m.model_id == model_id;
                }
            }
        }
        Ok(status)
    }

    /// 安装模型（真实事务：staging/下载/校验/提升）。
    ///
    /// 变更互斥：与同引擎其他变更操作（环境安装/修复/启停/其他模型操作）
    /// 通过 EngineOperationCoordinator 串行。
    pub async fn install_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.execute_model_install_or_repair(engine_id, model_id, operation_id, false)
            .await
    }

    /// 修复模型（重下载 + 完整校验；保留旧 payload 直至新 payload 提升成功）。
    pub async fn repair_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.execute_model_install_or_repair(engine_id, model_id, operation_id, true)
            .await
    }

    /// install/repair 共享事务体（差异只在 kind 与幂等短路）。
    async fn execute_model_install_or_repair(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
        is_repair: bool,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let kind = if is_repair {
            ModelOperationKind::Repair
        } else {
            ModelOperationKind::Install
        };

        // 已安装且非修复 → 幂等返回
        if !is_repair {
            let asset_key = mstore::encode_asset_key(model_id);
            if matches!(
                mstore::restore_model_state(engine_id, &asset_key),
                Ok(mstore::RestoredModelState::Installed { .. })
            ) {
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: operation_id.unwrap_or_default(),
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Done,
                    success: true,
                    error: None,
                });
            }
        }

        // claim 进程级操作（与同引擎所有变更互斥）
        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let guard = self.coordinator.try_claim(engine_id, &op_id)?;

        let install_id = generate_install_id();
        let asset_key = mstore::encode_asset_key(model_id);

        // 状态转移 → Downloading（缓存仅作 UI 投影，磁盘 manifest 是持久真源）
        self.transition_model_state(engine_id, model_id, ModelInstallState::Downloading)
            .await?;

        // 清理孤儿 staging（claim 已保证无活跃操作，删除安全）
        let orphan_cleaned = tokio::task::spawn_blocking({
            let eid = engine_id.clone();
            let ak = asset_key.clone();
            move || mstore::cleanup_orphan_staging(&eid, &ak)
        })
        .await
        .unwrap_or(0);
        if orphan_cleaned > 0 {
            tracing::info!(
                engine_id = %engine_id,
                model_id = %model_id,
                count = orphan_cleaned,
                "已清理孤儿 staging 残留"
            );
        }

        // staging payload 目录
        let staging_payload_dir =
            match mstore::model_operation_staging_payload_dir(engine_id, &asset_key, &op_id) {
                Ok(p) => p,
                Err(e) => {
                    return Ok(self
                        .model_op_failed(
                            engine_id,
                            model_id,
                            &op_id,
                            kind,
                            &asset_key,
                            format!("staging 目录创建失败: {e}"),
                        )
                        .await);
                }
            };
        if let Err(e) = tokio::fs::create_dir_all(&staging_payload_dir).await {
            return Ok(self
                .model_op_failed(
                    engine_id,
                    model_id,
                    &op_id,
                    kind,
                    &asset_key,
                    format!("staging 目录创建失败: {e}"),
                )
                .await);
        }

        // 下载（worker 执行；sink 实时广播日志 + 内存缓冲）
        let sink = std::sync::Arc::new(super::model_installer::BroadcastingInstallSink::new(
            super::model_installer::BoundedInstallSink::new(500),
            Arc::clone(&self.event_port) as Arc<dyn EventPort>,
            engine_id.clone(),
            op_id.clone(),
        ));
        use super::model_installer::InstallSink as _ModelInstallSink;
        sink.emit_stage("preparing");
        let download_result = self
            .model_worker
            .download_to_staging(
                engine_id,
                model_id,
                &desc.revision,
                &staging_payload_dir,
                guard.cancel_token().clone(),
                Some(Arc::clone(&sink) as Arc<dyn super::model_installer::InstallSink>),
            )
            .await;

        // 取消优先判定（claim 未释放——guard 仍持有）
        if guard.is_cancelled() {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                .await
                .ok();
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Cancelled,
                success: true,
                error: None,
            });
        }

        if let Err(e) = download_result {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition_model_state(engine_id, model_id, ModelInstallState::DownloadFailed)
                .await
                .ok();
            self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                .await
                .ok();
            let tail = sink.tail_lines(15);
            let detail = if tail.is_empty() {
                e.to_string()
            } else {
                format!("{e}\n最近日志:\n{}", tail.join("\n"))
            };
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    e.to_code(),
                    if is_repair {
                        ErrorPhase::Repair
                    } else {
                        ErrorPhase::Install
                    },
                    "模型下载失败",
                    detail,
                )),
            });
        }

        self.transition_model_state(engine_id, model_id, ModelInstallState::Staging)
            .await?;
        self.transition_model_state(engine_id, model_id, ModelInstallState::Verifying)
            .await?;

        // 完整 fingerprint 校验（GB 级 hash 在 blocking pool 执行）
        let fingerprint = match tokio::task::spawn_blocking({
            let dir = staging_payload_dir.clone();
            move || mstore::compute_content_fingerprint(&dir)
        })
        .await
        {
            Ok(Ok(fp)) => fp,
            Ok(Err(e)) => {
                let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                self.transition_model_state(
                    engine_id,
                    model_id,
                    ModelInstallState::VerificationFailed,
                )
                .await
                .ok();
                self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                    .await
                    .ok();
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ArtifactCorrupted,
                        ErrorPhase::Install,
                        "模型校验失败",
                        format!("{e}"),
                    )),
                });
            }
            Err(join_err) => {
                let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                self.transition_model_state(
                    engine_id,
                    model_id,
                    ModelInstallState::VerificationFailed,
                )
                .await
                .ok();
                self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                    .await
                    .ok();
                return Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: kind,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(LocalEngineError::with_detail(
                        LocalEngineErrorCode::Internal,
                        ErrorPhase::Install,
                        "模型校验失败",
                        format!("fingerprint join 错误: {join_err}"),
                    )),
                });
            }
        };

        // manifest（保留来源、revision、checksum provenance、fingerprint、兼容 schema）
        let (source, checksum_source) = match download_result.as_ref() {
            Ok(outcome) => {
                let s = outcome.source.clone();
                match &outcome.checksum_source {
                    super::model_installer::ModelDownloadChecksumSource::Sha256(sha) => (
                        s,
                        crate::domain::local_engine::ChecksumSource::Sha256(sha.clone()),
                    ),
                    super::model_installer::ModelDownloadChecksumSource::Unverified => {
                        (s, crate::domain::local_engine::ChecksumSource::Unverified)
                    }
                }
            }
            Err(_) => unreachable!("download_result 已在上面处理"),
        };

        let downloaded_at_ms = runtime::now_ms();
        let manifest = mstore::ModelManifest {
            schema_version: mstore::MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine_id.clone(),
            model_id: model_id.to_string(),
            revision: desc.revision.clone(),
            source: match checksum_source {
                crate::domain::local_engine::ChecksumSource::Sha256(ref sha) => {
                    mstore::ModelSource::Sha256 {
                        sha256: sha.clone(),
                        source,
                        downloaded_at_ms,
                    }
                }
                _ => mstore::ModelSource::Unverified {
                    source,
                    downloaded_at_ms,
                },
            },
            install_id: install_id.clone(),
            installed_at_ms: downloaded_at_ms,
            content_fingerprint_algorithm: mstore::CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fingerprint.fingerprint.clone(),
            payload_size_bytes: fingerprint.total_size_bytes,
            file_count: fingerprint.file_count,
            compatibility_schema: desc.compatibility_schema,
            model_contract_identity: mstore::ModelContractIdentity {
                model_id: model_id.to_string(),
                revision: desc.revision.clone(),
                checksum_source_kind: match &desc.checksum_source {
                    crate::domain::local_engine::ChecksumSource::Sha256(_) => "sha256",
                    crate::domain::local_engine::ChecksumSource::DownloadSource { .. } => {
                        "download_source"
                    }
                    crate::domain::local_engine::ChecksumSource::Unverified => "unverified",
                }
                .to_string(),
            },
        };

        // 提升：staging → generations/{install_id} + 原子切换 current.json
        if let Err(e) = mstore::promote_staging_to_generation(
            engine_id,
            &asset_key,
            &install_id,
            &op_id,
            &manifest,
        ) {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition_model_state(engine_id, model_id, ModelInstallState::VerificationFailed)
                .await
                .ok();
            self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                .await
                .ok();
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: kind,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(from_runtime(ErrorPhase::Install, "模型提升失败", &e)),
            });
        }

        // 稳定状态只保留一个 installed revision——提升成功后删除旧 generation
        let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
        let _ = mstore::cleanup_old_generations(engine_id, &asset_key, &install_id);

        self.transition_model_state(engine_id, model_id, ModelInstallState::Installed)
            .await?;
        {
            let mut states = self.model_states.write().await;
            let key = (engine_id.clone(), model_id.to_string());
            if let Some(st) = states.get_mut(&key) {
                st.cache_size_bytes = Some(manifest.payload_size_bytes);
                st.verification_state = ModelVerificationState::Verified;
                st.compatibility = ModelCompatibility::Compatible;
            }
        }

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: kind,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 模型操作早期失败的统一收尾（清 staging + 状态回 NotInstalled + 失败结果）。
    async fn model_op_failed(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        op_id: &str,
        kind: ModelOperationKind,
        asset_key: &str,
        message: String,
    ) -> ModelOperationResult {
        tracing::warn!(
            engine_id = %engine_id,
            model_id = %model_id,
            %message,
            "模型操作失败"
        );
        let _ = mstore::cleanup_staging(engine_id, asset_key, op_id);
        self.transition_model_state(engine_id, model_id, ModelInstallState::DownloadFailed)
            .await
            .ok();
        self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
            .await
            .ok();
        ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id.to_string(),
            operation_kind: kind,
            final_stage: ModelOperationStage::Failed,
            success: false,
            error: Some(LocalEngineError::with_detail(
                LocalEngineErrorCode::InstallFailed,
                ErrorPhase::Install,
                "模型操作失败",
                message,
            )),
        }
    }

    /// 删除模型资产。
    ///
    /// 冲突判定：
    /// - selected（配置真源）→ 结构化冲突；
    /// - active（launch snapshot 冻结的模型身份 + instance_id）→ 结构化冲突；
    /// - descriptor 默认模型**不构成删除保护**（只提供首次默认值）。
    pub async fn delete_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        self.model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;

        let asset_key = mstore::encode_asset_key(model_id);

        // 已安装检查（Corrupted 视为可删除——允许清理损坏资产）
        match mstore::restore_model_state(engine_id, &asset_key) {
            Ok(mstore::RestoredModelState::Installed { .. })
            | Ok(mstore::RestoredModelState::Corrupted { .. }) => {}
            Ok(mstore::RestoredModelState::NotInstalled) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "模型未安装，无需删除",
                    format!("engine_id={}, model_id={}", engine_id, model_id),
                ));
            }
            Err(e) => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Internal,
                    ErrorPhase::Request,
                    "模型状态读取失败",
                    format!("{e}"),
                ));
            }
        }

        // 冲突检查（selected / active launch snapshot）
        if let Some(conflict) = self.check_delete_conflict(engine_id, model_id).await {
            self.transition_model_state(engine_id, model_id, ModelInstallState::DeleteBlocked)
                .await
                .ok();
            self.transition_model_state(engine_id, model_id, ModelInstallState::Installed)
                .await
                .ok();
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: operation_id.unwrap_or_default(),
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(conflict.to_error()),
            });
        }

        // claim 进程级操作
        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        let _guard = self.coordinator.try_claim(engine_id, &op_id)?;

        self.transition_model_state(engine_id, model_id, ModelInstallState::Deleting)
            .await?;

        let delete_result = tokio::task::spawn_blocking({
            let eid = engine_id.clone();
            let ak = asset_key.clone();
            move || mstore::delete_model_generation(&eid, &ak)
        })
        .await;

        match delete_result {
            Ok(Ok(())) => {
                self.transition_model_state(engine_id, model_id, ModelInstallState::NotInstalled)
                    .await?;
                {
                    let mut states = self.model_states.write().await;
                    let key = (engine_id.clone(), model_id.to_string());
                    if let Some(st) = states.get_mut(&key) {
                        st.cache_size_bytes = None;
                        st.verification_state = ModelVerificationState::Unknown;
                        st.is_selected = false;
                        st.is_active = false;
                        st.compatibility = ModelCompatibility::Unknown;
                    }
                }
                Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: ModelOperationKind::Delete,
                    final_stage: ModelOperationStage::Done,
                    success: true,
                    error: None,
                })
            }
            Ok(Err(e)) => {
                // 删除失败不谎报 NotInstalled——回 Installed
                self.transition_model_state(engine_id, model_id, ModelInstallState::Installed)
                    .await
                    .ok();
                Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: op_id,
                    operation_kind: ModelOperationKind::Delete,
                    final_stage: ModelOperationStage::Failed,
                    success: false,
                    error: Some(from_runtime(ErrorPhase::Cleanup, "模型删除失败", &e)),
                })
            }
            Err(join_err) => Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Internal,
                ErrorPhase::Cleanup,
                "模型删除失败",
                format!("spawn_blocking join 错误: {join_err}"),
            )),
        }
    }

    /// 取消模型操作（只触发匹配 operation_id 的 claim token）。
    ///
    /// 取消成功返回 `Cancelled` 终态结果（正常语义）；未命中活跃操作或
    /// id 错配返回结构化错误。
    pub async fn cancel_model_operation(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: &str,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        match self.coordinator.cancel(engine_id, operation_id) {
            CancelOutcome::Cancelled => {
                tracing::info!(
                    engine = %engine_id,
                    model = %model_id,
                    op = %operation_id,
                    "模型操作取消信号已发送"
                );
                Ok(ModelOperationResult {
                    engine_id: engine_id.to_string(),
                    model_id: model_id.to_string(),
                    operation_id: operation_id.to_string(),
                    operation_kind: ModelOperationKind::Install,
                    final_stage: ModelOperationStage::Cancelled,
                    success: true,
                    error: None,
                })
            }
            other => {
                let detail = match &other {
                    CancelOutcome::NoActiveOperation => "当前没有进行中的模型操作".to_string(),
                    CancelOutcome::Mismatched {
                        current_operation_id,
                    } => format!(
                        "operation_id 不匹配: 当前={current_operation_id}, 请求={operation_id}"
                    ),
                    CancelOutcome::Cancelled => unreachable!("已在上一个分支处理"),
                };
                Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "取消请求未命中活跃操作",
                    detail,
                ))
            }
        }
    }

    /// 模型安装状态转移（缓存投影；磁盘 manifest 是持久真源）。
    async fn transition_model_state(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        target: ModelInstallState,
    ) -> Result<(), LocalEngineError> {
        let desc = self.model_registry.find(engine_id, model_id).cloned();
        let mut states = self.model_states.write().await;
        let key = (engine_id.clone(), model_id.to_string());
        let current = states
            .get(&key)
            .map(|s| s.install_state.clone())
            .unwrap_or(ModelInstallState::NotInstalled);
        let next = transition_install_state(&current, target)?;
        let st = states.entry(key).or_insert_with(|| {
            EngineModelStatus::not_installed(&desc.expect("registry 命中后才转移状态"))
        });
        st.install_state = next;
        Ok(())
    }

    /// 校验 health 回报的模型身份（commands 兼容入口）。
    pub fn verify_model_identity(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        health_model_id: Option<&str>,
        health_revision: Option<&str>,
        health_fingerprint: Option<&str>,
    ) -> Result<crate::domain::local_engine::ModelIdentityVerification, LocalEngineError> {
        let desc = self
            .model_registry
            .find(engine_id, model_id)
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Unsupported,
                    ErrorPhase::Request,
                    "未知模型",
                    format!(
                        "engine_id={}, model_id={} 不在 allowlist",
                        engine_id, model_id
                    ),
                )
            })?;
        desc.verify_health_identity(health_model_id, health_revision, health_fingerprint)
    }

    /// 读取已安装模型 manifest（commands 兼容入口）。
    pub fn get_installed_manifest(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<mstore::ModelManifest, LocalEngineError> {
        let asset_key = mstore::encode_asset_key(model_id);
        let pointer = mstore::read_model_current_pointer(engine_id, &asset_key)
            .map_err(|e| from_runtime(ErrorPhase::Request, "读取模型指针失败", &e))?
            .ok_or_else(|| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::ArtifactCorrupted,
                    ErrorPhase::Request,
                    "模型未安装",
                    "current.json 不存在",
                )
            })?;
        mstore::read_model_manifest(engine_id, &asset_key, &pointer.install_id)
            .map_err(|e| from_runtime(ErrorPhase::Request, "读取模型 manifest 失败", &e))
    }
}

/// 用服务身份与 OS 进程证据构造持久化 lease。
///
/// `ManagedProcess` 的 `ProcessIdentity::instance_id` 是 infra 状态机用于隔离
/// generation 的内部 token；health、回滚与恢复协议使用的是
/// `ServiceIdentityInput::instance_id`。lease 必须保存后者，否则 start 回滚时
/// 无法通过 instance 校验删除本次写入的 lease。
fn build_process_lease(
    engine_id: &EngineId,
    process_identity: &ProcessIdentity,
    service_identity: &ServiceIdentityInput,
    endpoint: &crate::infra::local_engine::port::Endpoint,
    generation_id: String,
) -> ProcessLease {
    ProcessLease::new(
        engine_id.to_string(),
        service_identity.instance_id.clone(),
        process_identity.pid,
        process_identity.start_time_ms,
        process_identity.executable.to_string_lossy().to_string(),
        endpoint.base_url(),
        service_identity.token_fingerprint(),
        generation_id,
    )
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
                        let current_instance_id = entry
                            .current_identity()
                            .await
                            .map(|i| i.instance_id.clone());

                        match current_instance_id {
                            Some(ref current) if current == &instance_id => {
                                // 身份匹配——同时进入 Blink tracing 与 UI 日志流。
                                // 未分类的第三方输出降为 debug，避免下载进度污染默认日志。
                                let level = classify_engine_log(log_entry.source, &log_entry.text);
                                match level {
                                    super::dto::EngineLogLevel::Error => tracing::error!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::dto::EngineLogLevel::Warn => tracing::warn!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::dto::EngineLogLevel::Info => tracing::info!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    super::dto::EngineLogLevel::Trace => tracing::trace!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                    _ => tracing::debug!(engine = %engine_id, instance = %instance_id, seq = log_entry.seq, output = %log_entry.text, "本地引擎输出"),
                                }
                                event_port.emit_log(
                                    &engine_id,
                                    &instance_id,
                                    log_entry.seq,
                                    level,
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

// ── 启动恢复探测（阻塞隔离，0.22.6 phase B）───────────────────────────────

/// probe 阻塞段的判定结果——async 上下文只负责按此提交状态。
///
/// 铁则：探测是**只读恢复**——只做 fail-closed 事务收尾和结构校验，
/// 不同步 hash GB 模型、不启动 Python/OCR 服务进程、不进入主链路。
#[derive(Debug, Clone, PartialEq, Eq)]
enum ProbeBlockingOutcome {
    /// 无 active 部署——保持默认 Missing。
    NoDeployment,
    /// active 部署有效 + self_test 通过 → Ready。
    Ready { install_id: String, slot: String },
    /// active 部署存在但 self_test 失败 → Broken。
    Broken { reason: String },
}

/// probe 的全部阻塞工作：fail-closed 事务恢复 + active 部署读取 + self_test。
///
/// 必须在 `spawn_blocking` 中调用——journal 遍历、JSON 读取和
/// self_test 的 venv python 子进程等待都是阻塞操作。
fn probe_blocking(
    engine_id: &EngineId,
    adapter: &Arc<dyn LocalEngineAdapter>,
) -> Result<ProbeBlockingOutcome, String> {
    // 1. 崩溃恢复：journal 存在即事务未收尾，按恢复表回滚/收尾（fail-closed）。
    let recovery = DeploymentStore::recover(engine_id).map_err(|e| format!("部署恢复失败: {e}"))?;
    match recovery {
        crate::infra::local_engine::deployment::RecoveryOutcome::Stable => {}
        other => {
            tracing::warn!(engine = %engine_id, outcome = ?other, "探测: 已恢复未收尾事务");
        }
    }

    // 2. 读 active 部署（结构校验，不做全量 hash）。
    let active = DeploymentStore::read_active(engine_id)
        .map_err(|e| format!("读取 deployment.json 失败: {e}"))?;
    let Some((pointer, _manifest)) = active else {
        return Ok(ProbeBlockingOutcome::NoDeployment);
    };

    // 3. self_test（venv python 子进程等待——阻塞，必须在 blocking 线程）。
    let self_test = adapter.self_test();
    if self_test.passed {
        Ok(ProbeBlockingOutcome::Ready {
            install_id: pointer.install_id,
            slot: pointer.slot,
        })
    } else {
        Ok(ProbeBlockingOutcome::Broken {
            reason: self_test
                .failure_reason
                .unwrap_or_else(|| "unknown".to_string()),
        })
    }
}

// ── 动态模型身份解析（0.22.6 B2）─────────────────────────────────────────

/// 从 model_storage manifest 动态解析当前安装的模型身份。
///
/// 返回 `(model_id, revision, fingerprint)` 三元组（如果模型已安装且有效）。
///
/// **asset_key 真源**：managed 模式下用 `selected_model_id`（配置选中的模型，
/// 如 funasr 的 `funasr_model`）查找 manifest；`fallback_contract.model_id`
/// 只是 descriptor 默认占位——用户可能安装/选择了其他模型（如装了
/// paraformer-zh 而 descriptor 默认 SenseVoiceSmall），按硬编码查找会
/// 误报"模型未安装"。
///
/// **0.22.6 B2 fail-closed 铁则**：模型未安装、损坏或恢复失败时返回 `Err`，
/// 不再回退到 descriptor 静态值。调用方必须将此视为启动/健康检查失败。
///
/// 这确保 health Ready 校验只与实际安装的 manifest 比对，
/// 而非与 descriptor 中编译期常量比对——防止
/// "下载了模型 A 但 health 期望模型 B" 的静默通过。
fn resolve_expected_model_identity(
    engine_id: &EngineId,
    selected_model_id: Option<&str>,
    fallback_contract: &ModelContract,
    uses_managed_model_storage: bool,
) -> Result<(String, String, Option<String>), String> {
    if !uses_managed_model_storage {
        return Ok((
            fallback_contract.model_id.clone(),
            fallback_contract.revision.clone(),
            None,
        ));
    }

    // 使用配置选中的 model_id 作为 asset_key 的来源
    let model_id_for_key = selected_model_id
        .filter(|m| !m.is_empty())
        .unwrap_or(&fallback_contract.model_id);
    let asset_key = mstore::encode_asset_key(model_id_for_key);
    match mstore::restore_model_state(engine_id, &asset_key) {
        Ok(mstore::RestoredModelState::Installed { manifest, .. }) => Ok((
            manifest.model_id,
            manifest.revision,
            Some(manifest.content_fingerprint),
        )),
        Ok(mstore::RestoredModelState::Corrupted { reason, .. }) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                reason = %reason,
                "模型状态 Corrupted——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态 Corrupted: {reason}"))
        }
        Ok(mstore::RestoredModelState::NotInstalled) => {
            tracing::debug!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                "模型未安装——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型未安装: {model_id_for_key}"))
        }
        Err(e) => {
            tracing::warn!(
                engine_id = %engine_id,
                model_id = %model_id_for_key,
                error = %e,
                "模型状态恢复失败——fail-closed，不回退到 descriptor 静态身份"
            );
            Err(format!("模型状态恢复失败: {e}"))
        }
    }
}

fn is_valid_model_fingerprint(fingerprint: &str) -> bool {
    fingerprint.len() == 64
        && fingerprint
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        && !fingerprint.bytes().all(|byte| byte == b'0')
}

// ── 存储扫描辅助（spawn_blocking 执行）────────────────────────────────────

/// 阻塞式存储扫描——在 `spawn_blocking` 中执行。
///
/// 扫描引擎的 generations、model cache、provider shared artifacts、
fn scan_engine_storage_blocking(
    engine_id: &EngineId,
) -> Result<super::dto::EngineStorageDto, crate::infra::local_engine::runtime::RuntimeError> {
    use crate::infra::local_engine::runtime::{ArtifactId, RuntimePlan};

    let mut targets = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut releasable_bytes: u64 = 0;

    // active 指针 + residue 记录
    let active = DeploymentStore::read_pointer(engine_id)?;
    let active_slot = active.as_ref().map(|p| p.slot.as_str());
    let residue = DeploymentStore::read_residue(engine_id)?;
    let residue_slots: Vec<&str> = residue.iter().map(|r| r.slot.as_str()).collect();

    // ── 1. 部署 slot（active 不可删；非 active = residue） ──
    for slot in ["slot-a", "slot-b"] {
        let dir = runtime::slot_dir(engine_id, slot);
        if !dir.exists() {
            continue;
        }
        let size = dir_size(&dir);
        total_bytes += size;

        let is_current = active_slot == Some(slot);
        let removable = !is_current;
        if removable {
            releasable_bytes += size;
        }

        let label_fallback = if is_current {
            "当前环境（不可删除）".to_string()
        } else if residue_slots.contains(&slot) {
            "清理残留环境（被占用）".to_string()
        } else {
            "残留环境".to_string()
        };

        targets.push(super::dto::StorageTargetDto {
            target_id: format!("slot:{slot}"),
            kind: super::dto::StorageTargetKindDto::EngineGeneration,
            engine_id: Some(engine_id.to_string()),
            label_key: "local_engine.storage.engine_generation".to_string(),
            label_fallback,
            size_bytes: size,
            current: is_current,
            previous: false,
            removable,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: if is_current {
                Some("current_generation".to_string())
            } else {
                None
            },
            affected_engine_ids: None,
            reference_count: None,
            path_display: None,
        });
    }

    // ── 2. 孤儿 staging ──
    let staging = runtime::staging_dir(engine_id);
    if staging.exists() {
        let has_entries = std::fs::read_dir(&staging)
            .map(|mut d| d.next().is_some())
            .unwrap_or(false);
        if has_entries {
            let size = dir_size(&staging);
            total_bytes += size;
            releasable_bytes += size;

            targets.push(super::dto::StorageTargetDto {
                target_id: "staging".to_string(),
                kind: super::dto::StorageTargetKindDto::EngineGeneration,
                engine_id: Some(engine_id.to_string()),
                label_key: "local_engine.storage.engine_staging".to_string(),
                label_fallback: "事务构建残留（staging）".to_string(),
                size_bytes: size,
                current: false,
                previous: false,
                removable: true,
                shared: false,
                requires_separate_confirmation: false,
                blocked_reason: None,
                affected_engine_ids: None,
                reference_count: None,
                path_display: None,
            });
        }
    }

    // ── 3. Engine model cache ──
    let model_cache_dir = runtime::engine_model_cache_dir(engine_id);
    if model_cache_dir.exists() {
        let size = dir_size(&model_cache_dir);
        total_bytes += size;
        releasable_bytes += size;

        targets.push(super::dto::StorageTargetDto {
            target_id: "model_cache".to_string(),
            kind: super::dto::StorageTargetKindDto::EngineModelCache,
            engine_id: Some(engine_id.to_string()),
            label_key: "local_engine.storage.engine_model_cache".to_string(),
            label_fallback: "模型缓存".to_string(),
            size_bytes: size,
            current: false,
            previous: false,
            removable: true,
            shared: false,
            requires_separate_confirmation: false,
            blocked_reason: None,
            affected_engine_ids: None,
            reference_count: None,
            path_display: None,
        });
    }

    // ── 4. Provider shared artifacts ──
    // 引用真源 = 各引擎 active 部署 manifest（无独立 refcount 数据）
    let shared_root = runtime::runtimes_root().join("shared");
    if shared_root.exists() {
        for provider_entry in std::fs::read_dir(&shared_root)? {
            let provider_entry = provider_entry?;
            if !provider_entry.file_type()?.is_dir() {
                continue;
            }
            let provider_name = provider_entry.file_name();
            let provider_str = provider_name.to_string_lossy().to_string();
            let runtime_kind = match provider_str.as_str() {
                "python_venv" => RuntimePlan::PythonVenv,
                "managed_binary" => RuntimePlan::ManagedBinary,
                _ => continue,
            };

            for artifact_entry in std::fs::read_dir(provider_entry.path())? {
                let artifact_entry = artifact_entry?;
                if !artifact_entry.file_type()?.is_dir() {
                    continue;
                }
                let artifact_name = match artifact_entry.file_name().to_str() {
                    Some(n) => n.to_string(),
                    None => continue,
                };

                let artifact_id = match ArtifactId::new(&artifact_name) {
                    Ok(a) => a,
                    Err(_) => continue,
                };

                let artifact_dir = artifact_entry.path();
                let size = dir_size(&artifact_dir);
                total_bytes += size;

                // 扫描 active 部署 manifest 引用
                let refs = runtime::scan_artifact_references(runtime_kind, &artifact_id)
                    .unwrap_or_default();
                let ref_count = refs.len() as u32;
                let affected: Vec<String> = refs
                    .iter()
                    .map(|r| r.engine_id.clone())
                    .collect::<std::collections::HashSet<_>>()
                    .into_iter()
                    .collect();
                let is_shared = affected.len() > 1 || !affected.contains(&engine_id.to_string());
                let removable = ref_count == 0 || !is_shared;
                let blocked = if !removable {
                    Some(format!("被 {} 个引擎引用", ref_count))
                } else {
                    None
                };

                if removable {
                    releasable_bytes += size;
                }

                let target_id = format!("shared:{}:{}", runtime_kind.provider_id(), artifact_name);

                targets.push(super::dto::StorageTargetDto {
                    target_id,
                    kind: super::dto::StorageTargetKindDto::ProviderSharedArtifact,
                    engine_id: None,
                    label_key: "local_engine.storage.provider_shared_artifact".to_string(),
                    label_fallback: format!("共享 artifact ({provider_str}/{artifact_name})"),
                    size_bytes: size,
                    current: false,
                    previous: false,
                    removable,
                    shared: true,
                    requires_separate_confirmation: true,
                    blocked_reason: blocked,
                    affected_engine_ids: Some(affected),
                    reference_count: Some(ref_count),
                    path_display: None,
                });
            }
        }
    }

    // ── 5. Provider download cache ──
    let uv_cache = runtime::uv_cache_dir();
    if uv_cache.exists() {
        let size = dir_size(&uv_cache);
        total_bytes += size;
        releasable_bytes += size;

        targets.push(super::dto::StorageTargetDto {
            target_id: "download_cache:python_venv".to_string(),
            kind: super::dto::StorageTargetKindDto::ProviderDownloadCache,
            engine_id: None,
            label_key: "local_engine.storage.provider_download_cache".to_string(),
            label_fallback: "uv 下载缓存".to_string(),
            size_bytes: size,
            current: false,
            previous: false,
            removable: true,
            shared: true,
            requires_separate_confirmation: true,
            blocked_reason: None,
            affected_engine_ids: None,
            reference_count: None,
            path_display: None,
        });
    }

    // ── 6. Legacy owned assets ──
    // 旧版 ModelScope 用户级公共缓存——仅在确有诊断价值时展示
    if engine_id.as_str() == "funasr" {
        if let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"))
        {
            if legacy_dir.exists() {
                let size = dir_size(&legacy_dir);
                if size > 0 {
                    total_bytes += size;

                    // legacy 资产不自动标记为 removable——需要单独确认
                    targets.push(super::dto::StorageTargetDto {
                        target_id: "legacy:modelscope".to_string(),
                        kind: super::dto::StorageTargetKindDto::LegacyOwnedAsset,
                        engine_id: Some(engine_id.to_string()),
                        label_key: "local_engine.storage.legacy_modelscope".to_string(),
                        label_fallback: "旧版 ModelScope 缓存残留".to_string(),
                        size_bytes: size,
                        current: false,
                        previous: false,
                        removable: false,
                        shared: true,
                        requires_separate_confirmation: true,
                        blocked_reason: Some("需单独确认和手动清理".to_string()),
                        affected_engine_ids: None,
                        reference_count: None,
                        path_display: None,
                    });
                }
            }
        }
    }

    Ok(super::dto::EngineStorageDto {
        engine_id: Some(engine_id.to_string()),
        targets,
        total_size_bytes: total_bytes,
        releasable_size_bytes: releasable_bytes,
    })
}

/// 测量 cleanup scope 的字节数（不执行删除）。
/// 解析 target_id 并执行清理（**阻塞**——磁盘删除，须在 spawn_blocking 中调用）。
///
/// target_id 格式：
/// - `slot:{slot}` — 非 active 部署 slot（residue 感知：占用记残留）
/// - `staging` — 孤儿 staging
/// - `model_cache` — 引擎模型缓存
/// - `shared:{runtime_kind}:{artifact_id}` — provider 共享 artifact
/// - `download_cache:{runtime_kind}` — provider 下载缓存
/// - `legacy:{kind}` — 旧版遗留资产（拒绝自动清理）
fn resolve_and_cleanup_target_blocking(
    engine_id: &EngineId,
    target_id: &str,
) -> Result<CleanupTargetOutcome, crate::infra::local_engine::runtime::RuntimeError> {
    use crate::infra::local_engine::providers::execute_cleanup;
    use crate::infra::local_engine::runtime::CleanupScope;

    if let Some(slot) = target_id.strip_prefix("slot:") {
        runtime::validate_slot_name(slot)?;

        // active slot 不可删除
        let active = DeploymentStore::read_pointer(engine_id)?;
        if active.as_ref().is_some_and(|p| p.slot == slot) {
            return Err(
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: "active 部署不可删除".to_string(),
                },
            );
        }

        let scope = CleanupScope::EngineDeploymentSlot {
            engine_id: engine_id.clone(),
            slot: slot.to_string(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        // delete_slot_if_not_active 占用时记 residue（Ok(false)）——
        // 查询 residue 判断是否残留
        let deferred = DeploymentStore::read_residue(engine_id)?
            .iter()
            .any(|r| r.slot == slot);
        if deferred {
            Ok(CleanupTargetOutcome::Deferred(size))
        } else {
            Ok(CleanupTargetOutcome::Cleaned(size))
        }
    } else if target_id == "staging" {
        let scope = CleanupScope::EngineStaging {
            engine_id: engine_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if target_id == "model_cache" {
        let scope = CleanupScope::EngineModelCache {
            engine_id: engine_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if let Some(rest) = target_id.strip_prefix("shared:") {
        // shared:{runtime_kind}:{artifact_id}
        let parts: Vec<&str> = rest.splitn(2, ':').collect();
        if parts.len() != 2 {
            return Err(
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: format!("无效的 shared target_id: {target_id}"),
                },
            );
        }
        let runtime_kind = match parts[0] {
            "python_venv" => crate::infra::local_engine::runtime::RuntimePlan::PythonVenv,
            "managed_binary" => crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary,
            _ => {
                return Err(
                    crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                        message: format!("未知的 runtime_kind: {}", parts[0]),
                    },
                );
            }
        };
        let artifact_id =
            crate::infra::local_engine::runtime::ArtifactId::new(parts[1]).map_err(|e| {
                crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message: format!("无效的 artifact id: {e}"),
                }
            })?;
        let scope = CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id: artifact_id.clone(),
        };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if let Some(kind) = target_id.strip_prefix("download_cache:") {
        let runtime_kind = match kind {
            "python_venv" => crate::infra::local_engine::runtime::RuntimePlan::PythonVenv,
            "managed_binary" => crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary,
            _ => {
                return Err(
                    crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                        message: format!("未知的 runtime_kind: {kind}"),
                    },
                );
            }
        };
        let scope = CleanupScope::ProviderDownloadCache { runtime_kind };
        let size = measure_cleanup_scope(&scope);
        execute_cleanup(&scope)?;
        Ok(CleanupTargetOutcome::Cleaned(size))
    } else if target_id.starts_with("legacy:") {
        // legacy 资产——只清理可证明归属的
        // 目前不自动清理 legacy，只标记
        Err(
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: "legacy 资产需要手动确认和单独清理".to_string(),
            },
        )
    } else {
        Err(
            crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                message: format!("未知/无效的 target_id: {target_id}"),
            },
        )
    }
}

fn measure_cleanup_scope(scope: &crate::infra::local_engine::runtime::CleanupScope) -> u64 {
    use crate::infra::local_engine::runtime::CleanupScope;

    match scope {
        CleanupScope::EngineDeploymentSlot { engine_id, slot } => dir_size(
            &crate::infra::local_engine::runtime::slot_dir(engine_id, slot),
        ),
        CleanupScope::EngineStaging { engine_id } => {
            dir_size(&crate::infra::local_engine::runtime::staging_dir(engine_id))
        }
        CleanupScope::EngineModelCache { engine_id } => {
            let dir = crate::infra::local_engine::runtime::engine_model_cache_dir(engine_id);
            dir_size(&dir)
        }
        CleanupScope::ProviderSharedArtifact {
            runtime_kind,
            artifact_id,
        } => {
            let dir = crate::infra::local_engine::runtime::shared_artifact_dir(
                *runtime_kind,
                artifact_id,
            );
            dir_size(&dir)
        }
        CleanupScope::ProviderDownloadCache { runtime_kind } => match runtime_kind {
            crate::infra::local_engine::runtime::RuntimePlan::PythonVenv => {
                let dir = crate::infra::local_engine::runtime::uv_cache_dir();
                dir_size(&dir)
            }
            crate::infra::local_engine::runtime::RuntimePlan::ManagedBinary => 0,
        },
    }
}

/// 递归计算目录大小（字节数）。
fn dir_size(path: &std::path::Path) -> u64 {
    if !path.exists() {
        return 0;
    }
    let mut total: u64 = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        match std::fs::read_dir(&dir) {
            Ok(entries) => {
                for entry in entries.flatten() {
                    let path = entry.path();
                    match entry.file_type() {
                        Ok(t) if t.is_dir() => stack.push(path),
                        Ok(t) if t.is_file() => {
                            total += std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                        }
                        _ => {}
                    }
                }
            }
            Err(_) => {}
        }
    }
    total
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
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
                        cleanup: CleanupPolicy::default(),
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
                checksum_source:
                    super::super::model_installer::ModelDownloadChecksumSource::Unverified,
            })
        }
    }

    /// Semaphore 门控 installer——测试放行一个 permit 控制"下载完成"时机。
    ///
    /// 用 Semaphore 而非 Notify：permit 会累积，release 早于 waiter 注册
    /// 也不会丢信号（Notify::notify_waiters 只唤醒已注册的 waiter）。
    struct GatedInstaller {
        gate: Arc<tokio::sync::Semaphore>,
        fail: std::sync::atomic::AtomicBool,
    }

    impl GatedInstaller {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                gate: Arc::new(tokio::sync::Semaphore::new(0)),
                fail: std::sync::atomic::AtomicBool::new(false),
            })
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
                checksum_source:
                    super::super::model_installer::ModelDownloadChecksumSource::Unverified,
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
            endpoint: endpoint.clone(),
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

        // 模型安装进入下载（gated）
        let svc_c = svc.clone();
        let eid_c = eid.clone();
        let tag_c = tag.clone();
        let install_task = tokio::spawn(async move {
            svc_c
                .install_model(&eid_c, &tag_c, Some("op-gated".to_string()))
                .await
        });

        // 等安装进入 claim（barrier 语义：轮询 coordinator 直到活跃——不用 sleep，
        // 通过 Notify 由 install 内部路径推进不可行，改为等待 claim 出现）
        let mut claimed_op = None;
        for _ in 0..200 {
            if let Some(op) = svc.coordinator().active_operation(&eid) {
                claimed_op = Some(op);
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
        let op = claimed_op.expect("模型安装应已 claim");

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

        let svc_c = svc.clone();
        let eid_c = eid.clone();
        let tag_c = tag.clone();
        let install_task =
            tokio::spawn(async move { svc_c.install_model(&eid_c, &tag_c, None).await });

        // 等待安装 claim 生效
        for _ in 0..200 {
            if svc.coordinator().active_operation(&eid).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

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

        let svc1 = svc.clone();
        let (e1, t1) = (eid.clone(), tag_a.clone());
        let first = tokio::spawn(async move { svc1.install_model(&e1, &t1, None).await });

        for _ in 0..200 {
            if svc.coordinator().active_operation(&eid).is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }

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
        let models = vec![tag.clone(), format!("{tag}-b")];
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

    #[tokio::test]
    async fn delete_not_installed_returns_error() {
        let eid = EngineId::new("fake-del-none").unwrap();
        let tag = unique_tag("delnone");
        let models = vec![tag.clone(), format!("{tag}-b")];
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
            .cleanup_targets(&eid, &["slot:slot-a".to_string()], None)
            .await
            .unwrap();
        assert!(
            result
                .skipped_target_ids
                .contains(&"slot:slot-a".to_string())
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
                &["slot:slot-b".to_string(), "staging".to_string()],
                None,
            )
            .await
            .unwrap();
        assert!(
            result
                .cleaned_target_ids
                .contains(&"slot:slot-b".to_string())
        );
        assert!(result.cleaned_target_ids.contains(&"staging".to_string()));
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
                    || t.target_id.starts_with("slot:")
                    || t.target_id.starts_with("shared:")
                    || t.target_id.starts_with("download_cache:")
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
}
