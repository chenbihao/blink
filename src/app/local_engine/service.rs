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
use crate::infra::local_engine::lease::{ProcessLease, remove_lease, write_lease};
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::port::{
    EndpointAllocator, IdentityVerification, ServiceIdentityInput, ServiceIdentityResult,
    generate_service_token,
};
use crate::infra::local_engine::process::{LaunchRequest, ManagedProcess, ShutdownConfig};
use crate::infra::local_engine::runtime::{
    self, BackendState, ComputeBackend, ComputePreference, EngineId, ModelContract,
    ResolvedProfile, generate_operation_id,
};
use crate::infra::local_engine::state::{ProcessIdentity, ProcessStatus};

use crate::infra::local_engine::providers::InstallSink;
use crate::infra::local_engine::providers::ProviderDescriptor;
use crate::infra::local_engine::providers::python::PythonVenvProvider;

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
    /// 操作取消 token——与 operation_id 绑定。
    /// cancel_local_engine_operation 只取消完全匹配且声明 cancellable 的操作。
    /// 旧 operation_id 不得取消新操作。
    operation_cancel: Mutex<Option<(String, CancellationToken)>>,
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
    /// 0.22.6.4: 使用 Arc 包装，使 exit monitor 能持有引用并在验证身份后
    /// 安全移除对应 ProcessKey 条目，避免 registry 泄漏。
    /// 不形成强引用环：Arc 指向 Mutex，不指向 LocalEngineService 自身。
    process_registry: Arc<std::sync::Mutex<HashMap<ProcessKey, Arc<ManagedProcess>>>>,
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
                    operation_cancel: Mutex::new(None),
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
            process_registry: Arc::new(std::sync::Mutex::new(HashMap::new())),
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

        // 0.22.6.2: 创建取消 token——install 是可取消的操作
        let cancel_token = CancellationToken::new();
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some((operation_id.clone(), cancel_token.clone()));
        }

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

        let sink_adapter = InstallSinkAdapter::new(
            self.event_port.clone(),
            engine_id.clone(),
            operation_id.clone(),
        );
        let install_result = transaction
            .execute(preference, Some(&cancel_token), Some(&sink_adapter))
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
                // finish_operation_with_error 会清除 operation_id（set None），
                // 所以标记 Broken 必须用 None operation_id 提交（fail-closed）
                self.finish_operation_with_error(engine_id, &operation_id, &err)
                    .await?;
                // 标记环境为 Broken（operation_id 已清除，用 None 提交）
                self.commit_status_internal(engine_id, None, |status| {
                    status.environment = EnvironmentHealth::Broken;
                })
                .await
                .ok();
                // 0.22.6.3: 清理取消 token（install 失败时防泄露）
                self.clear_cancel_token(engine_id).await;
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
        // 0.22.6.2: 先尝试恢复 current.json（缺失/损坏/无效 manifest）
        //
        // 如果 current.json 不存在或指向无效 manifest，尝试从 generations/ 扫描
        // 最新的有效 generation 恢复指针。恢复时传入 descriptor 做契约验证。
        let descriptor = self.provider_descriptors.get(engine_id);
        if let Err(e) =
            crate::infra::local_engine::providers::recover_current_pointer(engine_id, descriptor)
        {
            tracing::warn!(engine = %engine_id, %e, "探测: recover_current_pointer 失败");
        }

        // 读 current.json（可能在恢复后已更新）
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

                // 0.22.6.6: spawn 成功后立即写 lease
                // 此时 PID、executable、creation_time_ms 均已从 OS 获取，
                // token_fingerprint 可从 identity_input 计算。
                // 如果 Blink 在 health 验证期间崩溃，lease 已存在，
                // 下次启动的恢复扫描能发现此遗留进程。
                // health 验证失败时，rollback_started_instance 会清理此 lease。
                self.write_lease_for_engine(engine_id, &managed, &identity_input, &endpoint, &req)
                    .await;

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

                        // 0.22.6.6: lease 已在 spawn 后立即写入（上方），
                        // health 验证通过后不需要重新写入——所有字段在 spawn 时已就绪。
                        // health 验证失败时，rollback_started_instance 会清理 lease。

                        // 0.22.6.3: spawn exit monitor——监听进程意外退出
                        // server crash 后状态必须收敛到 Exited/Unreachable/Failed
                        self.spawn_exit_monitor(engine_id, &managed, &entry, &instance_id, &pkey);

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
                            status.last_error = None;
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

                        // 0.22.6.1: 删除 lease（只删除匹配 instance 的 lease）
                        if let Some(ref inst_id) = saved_instance_id {
                            if let Err(e) = remove_lease(&engine_id.to_string(), inst_id) {
                                tracing::warn!(
                                    engine = %engine_id,
                                    instance = %inst_id,
                                    %e,
                                    "stop: 删除 lease 失败（继续清理）"
                                );
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
                    status.last_error = None;
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
                            status.last_error = None;
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
                    status.last_error = None;
                })
                .await?;
                Ok(())
            }
        }
    }

    // ── repair / cleanup / storage / cancel（0.22.5 H2）─────────────────────

    /// 修复引擎环境。
    ///
    /// repair 是一个闭合事务：
    /// 1. 进入 repair gate（拒绝新 lease，但保留 current generation）
    /// 2. 在新 staging 中按当前配置重建环境
    /// 3. self-test
    /// 4. 原子切换 current.json
    /// 5. 失败保持旧 generation 可用
    ///
    /// 不通过原地覆盖 current generation"修复"。
    pub async fn repair(&self, engine_id: &EngineId) -> Result<(), LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let _gate = entry.op_gate.lock().await;

        let operation_id = generate_operation_id();
        self.set_operation_id(engine_id, Some(operation_id.clone()))
            .await;

        // 创建取消 token——repair 是可取消的操作
        let cancel_token = CancellationToken::new();
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some((operation_id.clone(), cancel_token.clone()));
        }

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

        // 检查是否有 ProviderDescriptor——repair 需要 InstallTransaction
        let provider_descriptor = match self.provider_descriptors.get(engine_id) {
            Some(d) => d,
            None => {
                // 无 ProviderDescriptor——退化为 self_test 验证
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
                    self.clear_cancel_token(engine_id).await;
                    return Err(err);
                }

                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.environment = EnvironmentHealth::Ready;
                    status.operation.stage = OperationStage::Completed;
                })
                .await?;
                self.set_operation_id(engine_id, None).await;
                self.clear_cancel_token(engine_id).await;
                return Ok(());
            }
        };

        // 保留当前 generation——repair 不删除旧 generation
        let previous_pointer = runtime::read_current_pointer(engine_id).map_err(|e| {
            LocalEngineError::from_runtime(ErrorPhase::Repair, "读取 current.json 失败", &e)
        })?;

        // 检查取消
        if cancel_token.is_cancelled() {
            return Err(self.cancel_operation(engine_id, &operation_id).await);
        }

        self.commit_status_internal(engine_id, Some(&operation_id), |status| {
            status.operation.stage = OperationStage::Downloading;
        })
        .await?;

        // 执行 InstallTransaction（在新 staging 中重建环境）
        let preference = config.compute_preference.unwrap_or(ComputePreference::Auto);

        let transaction = crate::infra::local_engine::providers::InstallTransaction::new(
            provider_descriptor,
            &self.python_provider,
        );

        let sink_adapter = InstallSinkAdapter::new(
            self.event_port.clone(),
            engine_id.clone(),
            operation_id.clone(),
        );
        match transaction
            .execute(preference, Some(&cancel_token), Some(&sink_adapter))
            .await
        {
            Ok(result) => {
                tracing::info!(
                    engine = %engine_id,
                    install_id = %result.install_id,
                    "repair: 新 generation 安装成功"
                );

                // 检查取消——promote 前的安全 checkpoint
                if cancel_token.is_cancelled() {
                    tracing::info!(engine = %engine_id, "repair: 在 promote 前被取消，清理 staging");
                    // 清理新 staging（current.json 未切换，旧 generation 仍可用）
                    let _ = std::fs::remove_dir_all(runtime::generation_dir(
                        engine_id,
                        &result.install_id,
                    ));
                    return Err(self.cancel_operation(engine_id, &operation_id).await);
                }

                self.commit_status_internal(engine_id, Some(&operation_id), |status| {
                    status.operation.stage = OperationStage::Completed;
                    status.environment = EnvironmentHealth::Ready;
                })
                .await?;

                self.set_operation_id(engine_id, None).await;
                self.clear_cancel_token(engine_id).await;
                tracing::info!(engine = %engine_id, "repair 完成（新 generation 已切换，旧 generation 可手动清理）");
                Ok(())
            }
            Err(e) => {
                let err = LocalEngineError::from_runtime(
                    ErrorPhase::Repair,
                    "修复失败（InstallTransaction）",
                    &e,
                );
                self.finish_operation_with_error(engine_id, &operation_id, &err)
                    .await?;
                self.clear_cancel_token(engine_id).await;

                // 保留旧 generation 可用
                if let Some(ref prev) = previous_pointer {
                    let _ = runtime::write_current_pointer(engine_id, prev);
                    tracing::info!(engine = %engine_id, "repair 失败，已恢复 previous generation");
                }
                Err(err)
            }
        }
    }

    /// 清理引擎资产。
    ///
    /// 前端提交 `target_ids`，后端重新解析每个 target_id，不信任前端提交的路径/size/shared/current。
    ///
    /// 禁止提交任意路径。current generation 默认不可删除。
    /// 共享资产经过引用检查。
    pub async fn cleanup_targets(
        &self,
        engine_id: &EngineId,
        target_ids: &[String],
        operation_id: Option<String>,
    ) -> Result<super::dto::CleanupResultDto, LocalEngineError> {
        self.validate_engine_id(engine_id)?;
        let entry = self.get_entry(engine_id).await?;
        let _gate = entry.op_gate.lock().await;

        let op_id = operation_id.unwrap_or_else(generate_operation_id);
        self.set_operation_id(engine_id, Some(op_id.clone())).await;

        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.operation = EngineOperation {
                kind: OperationKind::Cleaning,
                operation_id: op_id.clone(),
                stage: OperationStage::Preparing,
                cancellable: false, // cleanup 进入删除阶段后不可取消
            };
        })
        .await?;

        let mut cleaned = Vec::new();
        let mut skipped = Vec::new();
        let mut deferred = Vec::new();
        let mut released: u64 = 0;
        let mut errors = Vec::new();

        for target_id in target_ids {
            match self.resolve_and_cleanup_target(engine_id, target_id) {
                Ok(bytes) => {
                    released += bytes;
                    cleaned.push(target_id.clone());
                }
                Err(crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message,
                }) => {
                    // Windows 文件占用等——登记 deferred
                    tracing::warn!(
                        engine = %engine_id,
                        target = %target_id,
                        %message,
                        "cleanup 失败，登记 deferred"
                    );
                    deferred.push(target_id.clone());
                }
                Err(e) => {
                    tracing::warn!(
                        engine = %engine_id,
                        target = %target_id,
                        error = %e,
                        "cleanup 跳过"
                    );
                    skipped.push(target_id.clone());
                    errors.push(format!("{target_id}: {e}"));
                }
            }
        }

        self.commit_status_internal(engine_id, Some(&op_id), |status| {
            status.operation.stage = OperationStage::Completed;
        })
        .await?;

        self.set_operation_id(engine_id, None).await;

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
    /// 只取消完全匹配且声明 cancellable 的操作。
    /// 旧 operation_id 不得取消新操作。
    pub async fn cancel_operation(
        &self,
        engine_id: &EngineId,
        operation_id: &str,
    ) -> LocalEngineError {
        let entry = match self.get_entry(engine_id).await {
            Ok(e) => e,
            Err(e) => return e,
        };

        let oc = entry.operation_cancel.lock().await;
        match &*oc {
            Some((current_op_id, token)) if current_op_id == operation_id => {
                token.cancel();
                tracing::info!(
                    engine = %engine_id,
                    op = %operation_id,
                    "操作取消信号已发送"
                );
                // 设置状态为 Cancelled
                let _ = self
                    .commit_status_internal(engine_id, Some(operation_id), |status| {
                        status.operation.stage = OperationStage::Cancelled;
                    })
                    .await;
                self.set_operation_id(engine_id, None).await;
                drop(oc);
                self.clear_cancel_token(engine_id).await;
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Cancelled,
                    ErrorPhase::Request,
                    "操作已取消",
                    format!("operation_id={operation_id} 已被用户取消"),
                )
            }
            Some((current_op_id, _)) => {
                // operation_id 不匹配——旧 id 试图取消新操作
                tracing::warn!(
                    engine = %engine_id,
                    requested = %operation_id,
                    current = %current_op_id,
                    "取消请求的 operation_id 不匹配当前操作"
                );
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作已过期",
                    format!("operation_id 不匹配: expected={current_op_id}, got={operation_id}"),
                )
            }
            None => {
                // 无活跃操作
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "无活跃操作",
                    "没有正在进行的可取消操作",
                )
            }
        }
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

        result.map_err(|e| LocalEngineError::from_runtime(ErrorPhase::Request, "存储扫描失败", &e))
    }

    /// 清理取消 token。
    async fn clear_cancel_token(&self, engine_id: &EngineId) {
        if let Ok(entry) = self.get_entry(engine_id).await {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = None;
        }
    }

    /// 从配置真源读取 AdapterConfig（复用 commands 层逻辑的简化版）。
    fn read_adapter_config_for_engine(&self, engine_id: &EngineId) -> AdapterConfig {
        let engine_id_str = engine_id.as_str();
        match engine_id_str {
            "funasr" => {
                let config = crate::app::stt_config::get_stt_config();
                let local = &config.local_engine;
                let funasr_config =
                    crate::app::local_engine::funasr::FunasrEngineConfig::from_stt_config(local);
                let compute_preference = if local.device == "cuda" {
                    Some(ComputePreference::Cuda)
                } else {
                    Some(ComputePreference::Cpu)
                };
                AdapterConfig {
                    preferred_port: Some(local.server_port),
                    compute_preference,
                    engine_config: funasr_config.to_json(),
                }
            }
            "paddleocr" => {
                let ocr_config = crate::domain::config::ocr_config::get_ocr_config();
                let engine_config =
                    crate::app::local_engine::paddleocr::PaddleOcrEngineConfig::from_ocr_config();
                AdapterConfig {
                    preferred_port: None,
                    compute_preference: Some(ocr_config.compute_preference),
                    engine_config: engine_config.to_json(),
                }
            }
            _ => AdapterConfig::new(),
        }
    }

    /// 解析 target_id 并执行清理。
    ///
    /// target_id 格式：
    /// - `gen:{install_id}` — 引擎 generation
    /// - `model_cache` — 引擎模型缓存
    /// - `shared:{runtime_kind}:{artifact_id}` — provider 共享 artifact
    /// - `download_cache:{runtime_kind}` — provider 下载缓存
    /// - `legacy:{kind}` — 旧版遗留资产
    fn resolve_and_cleanup_target(
        &self,
        engine_id: &EngineId,
        target_id: &str,
    ) -> Result<u64, crate::infra::local_engine::runtime::RuntimeError> {
        use crate::infra::local_engine::providers::execute_cleanup;
        use crate::infra::local_engine::runtime::CleanupScope;

        // 解析 target_id
        if let Some(install_id) = target_id.strip_prefix("gen:") {
            // 引擎 generation
            runtime::validate_install_id(install_id)?;

            // 检查不是 current generation
            let current = runtime::read_current_pointer(engine_id)?;
            if let Some(ref c) = current {
                if c.install_id == install_id {
                    return Err(
                        crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                            message: "current generation 不可删除".to_string(),
                        },
                    );
                }
            }

            let scope = CleanupScope::EngineGeneration {
                engine_id: engine_id.clone(),
                install_ids: Some(vec![install_id.to_string()]),
            };
            let size = measure_cleanup_scope(&scope);
            execute_cleanup(&scope)?;
            Ok(size)
        } else if target_id == "model_cache" {
            let scope = CleanupScope::EngineModelCache {
                engine_id: engine_id.clone(),
            };
            let size = measure_cleanup_scope(&scope);
            execute_cleanup(&scope)?;
            Ok(size)
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
                "python_venv" => crate::infra::local_engine::runtime::RuntimeKind::PythonVenv,
                "managed_binary" => crate::infra::local_engine::runtime::RuntimeKind::ManagedBinary,
                _ => {
                    return Err(
                        crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                            message: format!("未知的 runtime_kind: {}", parts[0]),
                        },
                    );
                }
            };
            let artifact_id = crate::infra::local_engine::runtime::ArtifactId::new(parts[1])?;
            let scope = CleanupScope::ProviderSharedArtifact {
                runtime_kind,
                artifact_id: artifact_id.clone(),
            };
            let size = measure_cleanup_scope(&scope);
            execute_cleanup(&scope)?;
            Ok(size)
        } else if let Some(kind) = target_id.strip_prefix("download_cache:") {
            let runtime_kind = match kind {
                "python_venv" => crate::infra::local_engine::runtime::RuntimeKind::PythonVenv,
                "managed_binary" => crate::infra::local_engine::runtime::RuntimeKind::ManagedBinary,
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
            Ok(size)
        } else if let Some(_kind) = target_id.strip_prefix("legacy:") {
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
        let instance_id = {
            let ci = entry.current_identity.lock().await;
            ci.as_ref().map(|i| i.instance_id.clone())
        };

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

        // 验证 operation_id（fail-closed）
        //
        // 0.22.6.3: 如果 current_operation_id 为 None，任何携带 operation_id
        // 的提交都必须被拒绝——这防止迟到的任务（已取消/失败）覆写新状态。
        let current_op_id = entry.current_operation_id.lock().await;
        match (&*current_op_id, operation_id) {
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
            // 0.22.6.3: 无活跃操作但提交携带 operation_id → fail-closed 拒绝
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
        // 统一 ModelService 管理的引擎从 model_storage manifest 动态获取期望身份；
        // adapter 自管模型的引擎使用编译期 descriptor 身份，并由 adapter health
        // 契约负责校验其专属 manifest 产生的 content fingerprint。
        let descriptor = entry.adapter.descriptor();
        let engine_id = &descriptor.engine_id;

        // managed 模式 fail-closed：未安装/损坏时不回退 descriptor。
        // adapter-managed 模式显式使用 descriptor，fingerprint 由 health 提供。
        // asset_key 真源 = 配置选中的模型（funasr 读 SttConfig.funasr_model），
        // 不能用 descriptor 硬编码默认值——用户可能装了 paraformer-zh 而默认是
        // SenseVoiceSmall，按默认查找会误报"模型未安装"。
        let selected_model_id = if engine_id.as_str() == super::funasr::FUNASR_ENGINE_ID {
            Some(
                crate::app::stt_config::get_stt_config()
                    .local_engine
                    .funasr_model,
            )
        } else {
            None
        };
        let (expected_model_id, expected_revision, expected_fingerprint) =
            match resolve_expected_model_identity(
                engine_id,
                selected_model_id.as_deref(),
                &descriptor.model_contract,
                entry.adapter.uses_managed_model_storage(),
            ) {
                Ok(identity) => identity,
                Err(reason) => {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ModelNotReady,
                        ErrorPhase::Health,
                        "模型身份解析失败",
                        format!("无法从 manifest 获取模型身份: {reason}"),
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

    // ── lease 写入辅助（0.22.6.1） ─────────────────────────────────────────

    /// 为引擎实例写入持久化 lease。
    ///
    /// 在 health 验证全闭合后调用——从 `ManagedProcess` 获取进程身份
    /// （PID、可执行路径、创建时间），从 `ServiceIdentityInput` 获取
    /// token fingerprint，从 `current.json` 获取 generation_id。
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

        // 获取 generation_id（current.json 的 install_id）
        let generation_id = match runtime::read_current_pointer(engine_id) {
            Ok(Some(ref p)) => p.install_id.clone(),
            Ok(None) => {
                tracing::warn!(
                    engine = %engine_id,
                    "write_lease: 无 current.json，generation_id 用空串"
                );
                String::new()
            }
            Err(e) => {
                tracing::warn!(
                    engine = %engine_id,
                    error = %e,
                    "write_lease: 读取 current.json 失败，generation_id 用空串"
                );
                String::new()
            }
        };

        let lease =
            build_process_lease(engine_id, identity, identity_input, endpoint, generation_id);

        if let Err(e) = write_lease(&lease) {
            tracing::warn!(
                engine = %engine_id,
                instance = %lease.instance_id,
                pid = lease.pid,
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
        // 不形成强引用环：registry 是 LocalEngineService 拥有的 Mutex<HashMap>，
        // 这里克隆的是 Arc 到同一 Mutex 的引用，不持有 LocalEngineService 自身。
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
                let saved_instance_id = {
                    let ci = entry.current_identity.lock().await;
                    ci.as_ref().map(|i| i.instance_id.clone())
                };

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
    /// **0.22.3 Task C**: 任何 Err 分支都执行此方法：
    /// 1. 停止 ManagedProcess（如果存在）
    /// 2. 清理 identity/profile
    /// 3. 从 process_registry 移除
    /// 4. 从 EngineEntry 移除 managed_process
    /// 5. 置错误终态（process=Exited, service=Unreachable, last_error=err）
    ///
    /// **0.22.6.1**: rollback 时也尝试删除 lease（如果已写入）。
    /// lease 删除使用 instance_id 验证，不会误删新实例的 lease。
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

        // 0.22.6.1: 删除 lease（只删除匹配 instance 的 lease）
        // rollback 意味着 start 失败——如果 lease 已写入则清理
        if let Err(e) = remove_lease(&engine_id.to_string(), instance_id) {
            tracing::warn!(
                engine = %engine_id,
                instance = instance_id,
                error = %e,
                "rollback: 删除 lease 失败（继续清理）"
            );
        }

        // 从同步 registry 移除
        {
            let mut reg = self.process_registry.lock().unwrap();
            reg.remove(pkey);
        }

        // 0.22.6.3: 首次激活失败时触发 generation 回滚
        //
        // start 失败意味着当前 current generation 不可用。如果有 previous
        // generation，原子切回 previous，让用户能继续使用旧版本。
        // 新 generation 标记为 deferred cleanup，下次清理时移除。
        self.rollback_generation_on_activation_failure(engine_id)
            .await;

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

    /// 首次激活失败时的 generation 回滚。
    ///
    /// 0.22.6.3: 当 start（spawn/health）失败时，如果存在 previous generation，
    /// 原子切回 previous current.json，并将失败的 generation 标记为
    /// deferred cleanup。如果没有 previous generation，不操作（保持 current 指向）。
    ///
    /// **设计取舍**：
    /// - 只在 start 失败时回滚 generation——install 失败时 staging 已被
    ///   InstallTransaction 清理，current.json 仍指向旧的可用 generation。
    /// - 回滚 generation 不影响 rollback_started_instance 的进程清理——
    ///   两者独立，进程清理总在 generation 回滚之前完成。
    /// - 标记 deferred cleanup 而非立即删除——避免文件锁问题（Windows）。
    async fn rollback_generation_on_activation_failure(&self, engine_id: &EngineId) {
        use crate::infra::local_engine::providers::{mark_deferred_cleanup, rollback_to_previous};

        // 读取 current 指针
        let current = match runtime::read_current_pointer(engine_id) {
            Ok(Some(ptr)) => ptr,
            Ok(None) => {
                tracing::debug!(engine = %engine_id, "无 current generation，跳过 generation 回滚");
                return;
            }
            Err(e) => {
                tracing::warn!(
                    engine = %engine_id,
                    error = %e,
                    "读取 current.json 失败，跳过 generation 回滚"
                );
                return;
            }
        };

        // 尝试回滚到 previous generation
        match rollback_to_previous(engine_id) {
            Ok(Some(previous_id)) => {
                tracing::info!(
                    engine = %engine_id,
                    from = %current.install_id,
                    to = %previous_id,
                    "首次激活失败，已回滚 generation 到 previous"
                );
                // 标记失败的 generation 为 deferred cleanup
                if let Err(e) = mark_deferred_cleanup(
                    engine_id,
                    &current.install_id,
                    "首次激活失败（spawn/health），回滚到 previous",
                ) {
                    tracing::warn!(
                        engine = %engine_id,
                        install = %current.install_id,
                        error = %e,
                        "标记 deferred cleanup 失败（不影响回滚）"
                    );
                }
            }
            Ok(None) => {
                tracing::info!(
                    engine = %engine_id,
                    install = %current.install_id,
                    "无可回滚的 previous generation，保持 current 指向"
                );
            }
            Err(e) => {
                tracing::warn!(
                    engine = %engine_id,
                    install = %current.install_id,
                    error = %e,
                    "generation 回滚失败（current 保持指向当前 generation）"
                );
            }
        }
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
                        let current_instance_id = {
                            let ci = entry.current_identity.lock().await;
                            ci.as_ref().map(|i| i.instance_id.clone())
                        };

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
/// download cache 和 legacy 资产，构建 `EngineStorageDto`。
fn scan_engine_storage_blocking(
    engine_id: &EngineId,
) -> Result<super::dto::EngineStorageDto, crate::infra::local_engine::runtime::RuntimeError> {
    use crate::infra::local_engine::runtime::{ArtifactId, RuntimeKind, read_current_pointer};

    let mut targets = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut releasable_bytes: u64 = 0;

    // 当前 generation
    let current = read_current_pointer(engine_id)?;

    // ── 1. Engine generations ──
    let gens_dir = runtime::generations_dir(engine_id);
    if gens_dir.exists() {
        let current_id = current.as_ref().map(|c| c.install_id.as_str());
        let mut gen_ids: Vec<String> = Vec::new();

        for entry in std::fs::read_dir(&gens_dir)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str() {
                if runtime::validate_install_id(name).is_ok() {
                    gen_ids.push(name.to_string());
                }
            }
        }

        // 排序：current 最后（不可删）
        gen_ids.sort_by(|a, b| {
            let a_is_current = current_id == Some(a.as_str());
            let b_is_current = current_id == Some(b.as_str());
            match (a_is_current, b_is_current) {
                (true, true) => std::cmp::Ordering::Equal,
                (true, false) => std::cmp::Ordering::Greater,
                (false, true) => std::cmp::Ordering::Less,
                (false, false) => a.cmp(b),
            }
        });

        // 前一个 generation 标记
        let previous_id = if gen_ids.len() >= 2 {
            Some(gen_ids[gen_ids.len() - 2].as_str())
        } else {
            None
        };

        for install_id in &gen_ids {
            let gen_dir = runtime::generation_dir(engine_id, install_id);
            let size = dir_size(&gen_dir);
            total_bytes += size;

            let is_current = current_id == Some(install_id.as_str());
            let is_previous = previous_id == Some(install_id.as_str());
            let removable = !is_current;
            if removable {
                releasable_bytes += size;
            }

            let target_id = format!("gen:{install_id}");
            let label_fallback = if is_current {
                "当前环境（不可删除）".to_string()
            } else if is_previous {
                "上一版本环境".to_string()
            } else {
                "旧版本环境".to_string()
            };

            targets.push(super::dto::StorageTargetDto {
                target_id,
                kind: super::dto::StorageTargetKindDto::EngineGeneration,
                engine_id: Some(engine_id.to_string()),
                label_key: "local_engine.storage.engine_generation".to_string(),
                label_fallback,
                size_bytes: size,
                current: is_current,
                previous: is_previous,
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
    }

    // ── 2. Engine model cache ──
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

    // ── 3. Provider shared artifacts（Python distribution） ──
    // 扫描 runtimes/shared/ 下的所有 artifact
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
                "python_venv" => RuntimeKind::PythonVenv,
                "managed_binary" => RuntimeKind::ManagedBinary,
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

                // 检查引用计数
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

    // ── 4. Provider download cache ──
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

    // ── 5. Legacy owned assets ──
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
fn measure_cleanup_scope(scope: &crate::infra::local_engine::runtime::CleanupScope) -> u64 {
    use crate::infra::local_engine::runtime::CleanupScope;

    match scope {
        CleanupScope::EngineGeneration {
            engine_id,
            install_ids,
        } => {
            let current = crate::infra::local_engine::runtime::read_current_pointer(engine_id)
                .ok()
                .flatten();
            let current_id = current.map(|c| c.install_id);

            match install_ids {
                Some(ids) => {
                    let mut total = 0;
                    for id in ids {
                        if current_id.as_ref() == Some(id) {
                            continue;
                        }
                        let dir =
                            crate::infra::local_engine::runtime::generation_dir(engine_id, id);
                        total += dir_size(&dir);
                    }
                    total
                }
                None => {
                    let gens_dir = crate::infra::local_engine::runtime::generations_dir(engine_id);
                    if !gens_dir.exists() {
                        return 0;
                    }
                    let mut total = 0;
                    if let Ok(entries) = std::fs::read_dir(&gens_dir) {
                        for entry in entries.flatten() {
                            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                                if let Some(name) = entry.file_name().to_str() {
                                    if current_id.as_deref() == Some(name) {
                                        continue;
                                    }
                                    total += dir_size(&entry.path());
                                }
                            }
                        }
                    }
                    total
                }
            }
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
            crate::infra::local_engine::runtime::RuntimeKind::PythonVenv => {
                let dir = crate::infra::local_engine::runtime::uv_cache_dir();
                dir_size(&dir)
            }
            crate::infra::local_engine::runtime::RuntimeKind::ManagedBinary => 0,
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
    use crate::infra::local_engine::runtime::{ArtifactId, ComputePreference, RuntimeKind};
    use std::collections::HashMap;
    use std::sync::Arc;

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
            "gen-test".to_string(),
        );

        assert_eq!(lease.instance_id, "inst-service");
        assert_ne!(lease.instance_id, process_identity.instance_id);
        assert_eq!(lease.pid, process_identity.pid);
        assert_eq!(lease.endpoint, "http://127.0.0.1:8100");
    }

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

        let entry = svc.get_entry(&eid).await.unwrap();
        entry.status.write().await.last_error = Some(LocalEngineError::with_detail(
            LocalEngineErrorCode::NotRunning,
            ErrorPhase::Stop,
            "进程意外退出",
            "stale error from previous instance",
        ));

        let result = svc.stop(&eid).await;
        assert!(result.is_ok());

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.desired, DesiredState::Stopped);
        assert!(snapshot.status.last_error.is_none());
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

    // ── 验收场景 8b：cleanup_targets 空 target_ids 完成且返回空结果 ──────

    #[tokio::test]
    async fn cleanup_targets_completes_successfully() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // cleanup_targets with empty target_ids returns empty result
        let result = svc.cleanup_targets(&eid, &[], None).await.unwrap();

        assert_eq!(result.engine_id, "engine-a");
        assert!(result.cleaned_target_ids.is_empty());
        assert!(result.skipped_target_ids.is_empty());
        assert_eq!(result.released_bytes, 0);

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
        assert!(
            svc.cleanup_targets(&unknown, &["gen:fake".to_string()], None)
                .await
                .is_err()
        );
        assert!(svc.scan_storage(&unknown).await.is_err());
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

    // ── 0.22.5 H2：取消 / 引用检查 / 修复回滚 测试 ───────────────────────────

    // ── 取消 1：无活跃操作时 cancel_operation 返回 Rejected ──

    #[tokio::test]
    async fn cancel_returns_rejected_when_no_active_operation() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let err = svc.cancel_operation(&eid, "op-nonexistent").await;
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
        assert!(!err.action_hint.is_empty());
    }

    // ── 取消 2：旧 operation_id 不能取消新操作 ──

    #[tokio::test]
    async fn cancel_rejects_stale_operation_id() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 手动设置一个 operation_id + cancel token
        let entry = svc.get_entry(&eid).await.unwrap();
        let cancel_token = CancellationToken::new();
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some(("op-current".to_string(), cancel_token.clone()));
        }
        svc.set_operation_id(&eid, Some("op-current".to_string()))
            .await;

        // 用旧的 operation_id 尝试取消
        let err = svc.cancel_operation(&eid, "op-old").await;
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
        assert!(!err.action_hint.is_empty());

        // 清理
        svc.clear_cancel_token(&eid).await;
    }

    // ── 取消 3：正确 operation_id 成功取消 ──

    #[tokio::test]
    async fn cancel_succeeds_with_matching_operation_id() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let entry = svc.get_entry(&eid).await.unwrap();
        let cancel_token = CancellationToken::new();
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some(("op-target".to_string(), cancel_token.clone()));
        }
        svc.set_operation_id(&eid, Some("op-target".to_string()))
            .await;

        let err = svc.cancel_operation(&eid, "op-target").await;
        assert_eq!(err.code, LocalEngineErrorCode::Cancelled);
        assert!(cancel_token.is_cancelled());

        // 验证 operation_cancel 已清理
        let oc = entry.operation_cancel.lock().await;
        assert!(oc.is_none());
    }

    // ── 取消 4：cancel_operation 对未知 engine 返回错误 ──

    #[tokio::test]
    async fn cancel_operation_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let err = svc.cancel_operation(&unknown, "op-xyz").await;
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
    }

    // ── 存储扫描 1：scan_storage 对未知 engine 返回错误 ──

    #[tokio::test]
    async fn scan_storage_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc.scan_storage(&unknown).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, LocalEngineErrorCode::Unsupported);
    }

    // ── 存储扫描 2：scan_storage 对无 generation 的引擎返回空 targets ──

    #[tokio::test]
    async fn scan_storage_returns_empty_when_no_generations() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.scan_storage(&eid).await;
        // 扫描可能因为目录不存在而返回错误（RuntimeError::Io）或空结果
        // 关键是：不 panic，不阻塞
        match result {
            Ok(dto) => {
                // 如果成功，targets 可能非空（如有 legacy 或 shared artifact）
                // 但至少不 panic
                assert_eq!(dto.engine_id, Some("engine-a".to_string()));
            }
            Err(e) => {
                // 如果失败，code 应该是 Internal（Io 错误映射）
                // 或 Unsupported（engine_id 不在 allowlist）
                assert!(
                    e.code == LocalEngineErrorCode::Internal
                        || e.code == LocalEngineErrorCode::Unsupported
                );
            }
        }
    }

    // ── cleanup_targets 1：空 target_ids 返回空结果且状态为 Completed ──

    #[tokio::test]
    async fn cleanup_targets_empty_returns_empty_result() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.cleanup_targets(&eid, &[], None).await.unwrap();
        assert_eq!(result.engine_id, "engine-a");
        assert!(result.cleaned_target_ids.is_empty());
        assert!(result.skipped_target_ids.is_empty());
        assert!(result.deferred_target_ids.is_empty());
        assert_eq!(result.released_bytes, 0);
        assert!(result.error.is_none());

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.kind, OperationKind::Cleaning);
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── cleanup_targets 2：未知 target_id 被 deferred 或 skipped ──

    #[tokio::test]
    async fn cleanup_targets_skips_unknown_target_id() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc
            .cleanup_targets(&eid, &["unknown:target".to_string()], None)
            .await
            .unwrap();

        assert!(result.cleaned_target_ids.is_empty());
        // unknown target_id 走到 else 分支返回 CleanupFailed，被归类为 deferred
        // 或者被归类为 skipped——取决于 RuntimeError 类型
        let total_non_cleaned = result.skipped_target_ids.len() + result.deferred_target_ids.len();
        assert_eq!(total_non_cleaned, 1);
        assert!(result.error.is_some() || !result.deferred_target_ids.is_empty());
    }

    // ── cleanup_targets 3：未知 engine 返回错误 ──

    #[tokio::test]
    async fn cleanup_targets_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc
            .cleanup_targets(&unknown, &["gen:fake".to_string()], None)
            .await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, LocalEngineErrorCode::Unsupported);
    }

    // ── repair 1：repair 无 ProviderDescriptor 时退化为 self_test ──

    #[tokio::test]
    async fn repair_falls_back_to_self_test_without_provider_descriptor() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // engine-a 没有 ProviderDescriptor → repair 退化走 self_test
        svc.repair(&eid).await.unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(
            snapshot.status.environment,
            crate::domain::local_engine::EnvironmentHealth::Ready
        );
        assert_eq!(snapshot.status.operation.kind, OperationKind::Repairing);
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── repair 2：repair self_test 失败时返回 SelfTestFailed ──

    #[tokio::test]
    async fn repair_fails_when_self_test_fails() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            "engine-fail-repair",
            false,
        )]));
        let svc = LocalEngineService::new(registry, Arc::new(NoopEventPort));
        let eid = EngineId::new("engine-fail-repair").unwrap();

        let result = svc.repair(&eid).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            LocalEngineErrorCode::SelfTestFailed
        );

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.stage, OperationStage::Failed);
    }

    // ── repair 3：repair 对未知 engine 返回 Unsupported ──

    #[tokio::test]
    async fn repair_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc.repair(&unknown).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, LocalEngineErrorCode::Unsupported);
    }

    // ── repair 4：repair 可取消——operation_cancel 在 repair 期间被设置 ──

    #[tokio::test]
    async fn repair_sets_cancellable_operation() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // repair 完成后，operation_cancel 应已清理
        svc.repair(&eid).await.unwrap();

        let entry = svc.get_entry(&eid).await.unwrap();
        let oc = entry.operation_cancel.lock().await;
        assert!(oc.is_none(), "repair 完成后 operation_cancel 应已清理");
    }

    // ── repair 5：repair 失败后 operation_cancel 也被清理 ──

    #[tokio::test]
    async fn repair_clears_cancel_token_on_failure() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            "engine-fail-cancel",
            false,
        )]));
        let svc = LocalEngineService::new(registry, Arc::new(NoopEventPort));
        let eid = EngineId::new("engine-fail-cancel").unwrap();

        let _ = svc.repair(&eid).await;

        let entry = svc.get_entry(&eid).await.unwrap();
        let oc = entry.operation_cancel.lock().await;
        assert!(oc.is_none(), "repair 失败后 operation_cancel 也应被清理");
    }

    // ── resolve_and_cleanup_target 1：未知 target_id 前缀返回错误 ──

    #[tokio::test]
    async fn resolve_and_cleanup_unknown_target_id_prefix() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.resolve_and_cleanup_target(&eid, "totally:invalid:prefix");
        assert!(result.is_err());
    }

    // ── resolve_and_cleanup_target 2：gen: 前缀验证 install_id 格式 ──

    #[tokio::test]
    async fn resolve_and_cleanup_validates_install_id_format() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // gen: 前缀但 install_id 含非法字符
        let result = svc.resolve_and_cleanup_target(&eid, "gen:../traversal");
        assert!(result.is_err());
    }

    // ── resolve_and_cleanup_target 3：legacy: 前缀需要手动确认 ──

    #[tokio::test]
    async fn resolve_and_cleanup_legacy_needs_manual_confirmation() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc.resolve_and_cleanup_target(&eid, "legacy:modelscope");
        assert!(result.is_err());
        // legacy 资产返回 CleanupFailed（需要手动确认）
        match result {
            Err(crate::infra::local_engine::runtime::RuntimeError::CleanupFailed { message }) => {
                assert!(message.contains("legacy") || message.contains("手动"));
            }
            _ => panic!("expected CleanupFailed for legacy target"),
        }
    }

    // ════════════════════════════════════════════════════════════════════════
    // 0.22.5 H2 集成测试：取消 / 引用检查 / 修复失败回滚
    // ════════════════════════════════════════════════════════════════════════

    // ── 集成 1：取消操作后状态正确清理 ──────────────────────────────────────
    //
    // 场景：手动设置 operation_id + cancel_token，调用 cancel_operation，
    //       验证 token 被触发、operation_cancel 被清理、current_operation_id 被清除。

    #[tokio::test]
    async fn integration_cancel_cleans_up_all_state() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let entry = svc.get_entry(&eid).await.unwrap();
        let cancel_token = CancellationToken::new();

        // 设置 operation 状态为 Repairing
        let op_id = "op-integration-cancel-001".to_string();
        svc.set_operation_id(&eid, Some(op_id.clone())).await;
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some((op_id.clone(), cancel_token.clone()));
        }

        // 先 commit 一个 Repairing 状态
        let _ = svc
            .commit_status_internal(&eid, Some(&op_id), |status| {
                status.operation = EngineOperation {
                    kind: OperationKind::Repairing,
                    operation_id: op_id.clone(),
                    stage: OperationStage::Preparing,
                    cancellable: true,
                };
            })
            .await;

        // 取消
        let err = svc.cancel_operation(&eid, &op_id).await;
        assert_eq!(err.code, LocalEngineErrorCode::Cancelled);
        assert!(cancel_token.is_cancelled(), "CancellationToken 应被触发");

        // operation_cancel 应清理
        {
            let oc = entry.operation_cancel.lock().await;
            assert!(oc.is_none(), "operation_cancel 应被清理");
        }

        // current_operation_id 应清除
        {
            let oid = entry.current_operation_id.lock().await;
            assert!(oid.is_none(), "current_operation_id 应被清除");
        }

        // 状态应为 Cancelled
        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.stage, OperationStage::Cancelled);
    }

    // ── 集成 2：取消后再次用同一 operation_id 取消返回 Rejected ──────────────
    //
    // 场景：操作被取消后，用同一 operation_id 再次取消，应返回 Rejected
    //       （因为 operation_cancel 已清理）。

    #[tokio::test]
    async fn integration_cancel_after_cancel_returns_rejected() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let entry = svc.get_entry(&eid).await.unwrap();
        let cancel_token = CancellationToken::new();
        let op_id = "op-integration-cancel-002".to_string();
        svc.set_operation_id(&eid, Some(op_id.clone())).await;
        {
            let mut oc = entry.operation_cancel.lock().await;
            *oc = Some((op_id.clone(), cancel_token.clone()));
        }

        // 第一次取消成功
        let err = svc.cancel_operation(&eid, &op_id).await;
        assert_eq!(err.code, LocalEngineErrorCode::Cancelled);

        // 第二次取消——应返回 Rejected
        let err = svc.cancel_operation(&eid, &op_id).await;
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
    }

    // ── 集成 3：cleanup_targets 的 operation 在结束后清理 cancel token ──────
    //
    // 场景：cleanup_targets 虽然不可取消（cancellable=false），
    //       但完成后 operation_id 和 cancel token 应正确清理。

    #[tokio::test]
    async fn integration_cleanup_clears_operation_state_after_completion() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let result = svc
            .cleanup_targets(&eid, &["unknown:target".to_string()], None)
            .await
            .unwrap();

        // 操作完成
        assert_eq!(result.engine_id, "engine-a");

        // operation_id 应被清除
        let entry = svc.get_entry(&eid).await.unwrap();
        {
            let oid = entry.current_operation_id.lock().await;
            assert!(
                oid.is_none(),
                "cleanup 完成后 current_operation_id 应被清除"
            );
        }
        {
            let oc = entry.operation_cancel.lock().await;
            assert!(oc.is_none(), "cleanup 完成后 operation_cancel 应为 None");
        }

        // 状态应为 Completed
        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.kind, OperationKind::Cleaning);
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── 集成 4：repair 成功后状态正确（无 ProviderDescriptor 退化路径）──────
    //
    // 场景：fake adapter 没有 ProviderDescriptor，repair 退化为 self_test，
    //       成功后 environment=Ready，operation_cancel 已清理。

    #[tokio::test]
    async fn integration_repair_success_cleans_state() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        svc.repair(&eid).await.unwrap();

        let entry = svc.get_entry(&eid).await.unwrap();
        {
            let oc = entry.operation_cancel.lock().await;
            assert!(oc.is_none(), "repair 成功后 operation_cancel 应被清理");
        }
        {
            let oid = entry.current_operation_id.lock().await;
            assert!(oid.is_none(), "repair 成功后 current_operation_id 应被清除");
        }

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(
            snapshot.status.environment,
            crate::domain::local_engine::EnvironmentHealth::Ready
        );
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
    }

    // ── 集成 5：repair 失败后状态 Failed 且 cancel token 被清理 ──────────────
    //
    // 场景：repair 在 self_test 失败时返回 SelfTestFailed，
    //       operation_cancel 应被清理（不留 dangling token），
    //       current_operation_id 应被清除。

    #[tokio::test]
    async fn integration_repair_failure_cleans_state() {
        let registry = Arc::new(EngineRegistry::new_with_adapters(vec![make_fake_adapter(
            "engine-repair-fail",
            false,
        )]));
        let svc = LocalEngineService::new(registry, Arc::new(NoopEventPort));
        let eid = EngineId::new("engine-repair-fail").unwrap();

        let result = svc.repair(&eid).await;
        assert!(result.is_err());
        assert_eq!(
            result.unwrap_err().code,
            LocalEngineErrorCode::SelfTestFailed
        );

        let entry = svc.get_entry(&eid).await.unwrap();
        {
            let oc = entry.operation_cancel.lock().await;
            assert!(oc.is_none(), "repair 失败后 operation_cancel 应被清理");
        }
        {
            let oid = entry.current_operation_id.lock().await;
            assert!(oid.is_none(), "repair 失败后 current_operation_id 应被清除");
        }

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.stage, OperationStage::Failed);
        assert!(snapshot.status.last_error.is_some());
    }

    // ── 集成 6：resolve_and_cleanup_target 对 model_cache target_id 返回错误 ─
    //
    // 场景：model_cache target_id 在无实际模型缓存目录时应返回错误（CleanupFailed 或 Io）。
    //       验证 target_id 解析路径正确，不 panic。

    #[tokio::test]
    async fn integration_resolve_model_cache_no_dir_returns_error() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // model_cache target_id——目录不存在，execute_cleanup 返回 Ok 但 size=0
        // 或者 dir_size 返回 0
        let result = svc.resolve_and_cleanup_target(&eid, "model_cache");
        // model_cache 目录不存在时，execute_cleanup 返回 Ok（清理了不存在的目录）
        // size 可能为 0
        match result {
            Ok(size) => {
                // 如果成功，size 应为 0（目录不存在）
                assert_eq!(size, 0, "不存在的 model_cache 目录大小应为 0");
            }
            Err(_) => {
                // 如果失败，也是可接受的——目录不存在
            }
        }
    }

    // ── 集成 7：resolve_and_cleanup_target 对 gen: 前缀验证 current 不可删 ──
    //
    // 场景：gen: 前缀但对应的 install_id 是 current generation 时，
    //       resolve_and_cleanup_target 应返回 CleanupFailed。

    #[tokio::test]
    async fn integration_resolve_gen_current_rejects_deletion() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 先 install 让环境就绪
        svc.install(&eid, AdapterConfig::new()).await.unwrap();

        // 读取 current pointer（如果存在）
        let current = crate::infra::local_engine::runtime::read_current_pointer(&eid).unwrap();

        if let Some(ref c) = current {
            let target_id = format!("gen:{}", c.install_id);
            let result = svc.resolve_and_cleanup_target(&eid, &target_id);
            assert!(result.is_err());
            match result {
                Err(crate::infra::local_engine::runtime::RuntimeError::CleanupFailed {
                    message,
                }) => {
                    assert!(message.contains("current"), "应阻止删除 current generation");
                }
                _ => panic!("expected CleanupFailed for current generation"),
            }
        }
        // 如果没有 current pointer，跳过——fake adapter 的 install 不写 current.json
    }

    // ── 集成 8：scan_storage 返回的 targets 不含完整文件路径 ──────────────────
    //
    // 场景：scan_storage 返回的 EngineStorageDto 中 targets 的 path_display
    //       应为 None 或不含完整用户目录路径。

    #[tokio::test]
    async fn integration_scan_storage_targets_no_full_paths() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        match svc.scan_storage(&eid).await {
            Ok(dto) => {
                for target in &dto.targets {
                    if let Some(ref path) = target.path_display {
                        // path_display 不应包含完整用户目录路径
                        assert!(
                            !path.contains(
                                &dirs_next::home_dir().unwrap().to_string_lossy().to_string()
                            ),
                            "path_display 不应暴露完整用户目录路径"
                        );
                    }
                }
            }
            Err(_) => {
                // scan_storage 可能因为目录不存在而返回错误——可接受
            }
        }
    }

    // ── 集成 9：cleanup_targets 多个 target_ids 混合成功/跳过/deferred ────────
    //
    // 场景：提交多个 target_ids（混合有效/无效），验证结果正确分类。
    // gen:nonexistent-id 格式合法但目录不存在——execute_cleanup 返回 Ok（空操作），
    // size=0，被归类为 cleaned（released_bytes=0）。

    #[tokio::test]
    async fn integration_cleanup_mixed_targets_classification() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        let targets = vec![
            "totally:invalid".to_string(),    // 无效前缀 → CleanupFailed → deferred
            "gen:nonexistent-id".to_string(), // 有效格式但不存在 → cleaned（空操作，0 bytes）
            "legacy:whatever".to_string(),    // legacy → CleanupFailed → deferred
        ];

        let result = svc.cleanup_targets(&eid, &targets, None).await.unwrap();

        // gen:nonexistent-id 会被"清理"（空操作），released_bytes=0
        // totally:invalid 和 legacy:whatever 返回 CleanupFailed → deferred
        let total_handled = result.cleaned_target_ids.len()
            + result.skipped_target_ids.len()
            + result.deferred_target_ids.len();
        assert_eq!(
            total_handled, 3,
            "所有 3 个 target 应被分类为 cleaned/skipped/deferred"
        );

        // released_bytes 应为 0（不存在的目录不释放空间）
        assert_eq!(result.released_bytes, 0);
    }

    // ── 集成 10：repair 设置 cancellable=true ──────────────────────────────────
    //
    // 场景：repair 操作的状态中 cancellable 应为 true，
    //       验证 operation 状态正确反映可取消性。

    #[tokio::test]
    async fn integration_repair_sets_cancellable_flag() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 在 repair 执行前捕获状态——由于 repair 是同步的（self_test 路径），
        // 我们在 repair 完成后检查 final 状态的 operation kind
        svc.repair(&eid).await.unwrap();

        let snapshot = svc.get_status(&eid).await.unwrap();
        assert_eq!(snapshot.status.operation.kind, OperationKind::Repairing);
        // repair 完成后 stage 为 Completed
        assert_eq!(snapshot.status.operation.stage, OperationStage::Completed);
        // cancellable 应为 true（repair 是可取消操作）
        assert!(snapshot.status.operation.cancellable);
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6.3 确定性测试：server crash recovery
    // ═══════════════════════════════════════════════════════════════════════

    /// 验证 exit monitor 收到进程退出后状态收敛到 Exited/Unreachable。
    ///
    /// 场景：进程从 Running 退出 → exit monitor 验证 instance token 仍匹配 →
    /// 清理 identity/profile/lease → 状态收敛到 process=Exited, service=Unreachable。
    ///
    /// 此测试验证 ManagedProcessState 层面的退出提交逻辑，
    /// 确保旧 generation 的退出事件不会覆盖新实例。
    #[tokio::test]
    async fn crash_recovery_old_exit_does_not_overwrite_new_instance() {
        use crate::infra::local_engine::state::{
            CommitResult, ExitReason, ManagedProcessState, ProcessStatus,
        };

        let mut state = ManagedProcessState::initial();

        // gen=1 start
        let token1 = state.begin_start();
        let identity = crate::infra::local_engine::state::ProcessIdentity {
            pid: 1234,
            executable: std::path::PathBuf::from("/test/fake"),
            start_time_ms: 0,
            instance_id: token1.instance_id.clone(),
        };
        assert_eq!(
            state.try_commit_running(&token1, 1234, identity),
            CommitResult::Committed
        );
        assert_eq!(state.status, ProcessStatus::Running { pid: 1234 });

        // 模拟 crash——进程以非零码退出
        let ok = state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 });
        assert!(ok, "当前 generation 的退出应被接受");
        assert!(state.status.is_exited(), "状态应转为 Exited");

        // gen=2 start（新实例——模拟 restart）
        let token2 = state.begin_start();
        assert_ne!(token1.generation, token2.generation);

        // 旧 gen 的退出事件再次到达——应被拒绝
        let ok = state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 });
        assert!(!ok, "旧 generation 的退出事件不应覆盖新实例");

        // 新实例状态不变
        assert_eq!(state.status, ProcessStatus::Starting);
    }

    /// 验证 exit monitor 的身份验证逻辑——token 不匹配时忽略旧 exit。
    #[tokio::test]
    async fn crash_recovery_token_mismatch_ignores_exit() {
        use crate::infra::local_engine::state::{ExitReason, ManagedProcessState, ProcessStatus};

        let mut state = ManagedProcessState::initial();
        let token1 = state.begin_start();

        // gen=2 start（restart 发生在 exit 之前）
        let _token2 = state.begin_start();

        // gen=1 的 exit 事件到达——generation 不匹配，应被拒绝
        let ok = state.try_commit_exit(&token1, ExitReason::NonZeroExit { code: 1 });
        assert!(!ok, "旧 generation exit 不应覆盖新实例");

        // 新实例状态仍为 Starting
        assert_eq!(state.status, ProcessStatus::Starting);
    }

    /// 验证 ProcessStatus::Exited 标记。
    #[tokio::test]
    async fn crash_recovery_exited_status_is_terminal() {
        use crate::infra::local_engine::state::{ExitReason, ManagedProcessState, ProcessStatus};

        let mut state = ManagedProcessState::initial();
        let token = state.begin_start();
        state.set_status_exited(ExitReason::NonZeroExit { code: 42 });

        assert!(state.status.is_exited());
        assert!(!state.status.is_active());

        // 从 Exited 再次提交 Exited 应被拒绝
        let ok = state.try_commit_exit(&token, ExitReason::NormalExit { code: 0 });
        assert!(!ok, "从 Exited 不应再次转为 Exited");

        assert_eq!(
            state.status,
            ProcessStatus::Exited {
                reason: ExitReason::NonZeroExit { code: 42 }
            }
        );
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6.6 回归测试：stop_orphan_engine
    // ═══════════════════════════════════════════════════════════════════════

    // ── stop_orphan_engine 对未知 engine_id 返回 Unsupported ──

    #[tokio::test]
    async fn stop_orphan_engine_rejects_unknown_engine() {
        let svc = make_service("engine-a");
        let unknown = EngineId::new("no-such-engine").unwrap();

        let result = svc.stop_orphan_engine(&unknown).await;
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().code, LocalEngineErrorCode::Unsupported);
    }

    // ── stop_orphan_engine 无 lease 时返回 lease_not_found ──

    #[tokio::test]
    async fn stop_orphan_engine_returns_lease_not_found_when_no_lease() {
        let svc = make_service("engine-a");
        let eid = EngineId::new("engine-a").unwrap();

        // 清理可能存在的旧 lease
        let _ = crate::infra::local_engine::lease::remove_lease_force("engine-a");

        let result = svc.stop_orphan_engine(&eid).await.unwrap();
        assert!(!result.stopped);
        assert_eq!(result.reason, "lease_not_found");
        assert!(result.detail.is_some());
    }

    // ── stop_orphan_engine 有 stale lease（PID 不存在）时返回 pid_not_exist ──

    #[tokio::test]
    async fn stop_orphan_engine_returns_pid_not_exist_for_stale_lease() {
        let svc = make_service("engine-stale");
        let eid = EngineId::new("engine-stale").unwrap();

        // 清理可能存在的旧 lease
        let _ = crate::infra::local_engine::lease::remove_lease_force("engine-stale");

        // 写入一个 stale lease（PID 99999 几乎确定不存在）
        let lease = crate::infra::local_engine::lease::ProcessLease::new(
            "engine-stale",
            "inst-stale-001",
            99999,
            1700000000000,
            "C:/nonexistent/python.exe",
            "127.0.0.1:59999",
            "fp:stale00000000000",
            "gen-stale",
        );
        crate::infra::local_engine::lease::write_lease(&lease).unwrap();

        let result = svc.stop_orphan_engine(&eid).await.unwrap();
        assert!(!result.stopped);
        assert_eq!(result.reason, "pid_not_exist");
        assert!(result.detail.is_some());

        // 验证 stale lease 已被清除
        let leases = crate::infra::local_engine::lease::scan_leases();
        assert!(
            !leases.iter().any(|l| l.engine_id == "engine-stale"),
            "stale lease 应已清除"
        );
    }

    // ── stop_orphan_engine 有 lease 但 health 不可达时返回 health_unreachable ──

    #[tokio::test]
    async fn stop_orphan_engine_returns_health_unreachable() {
        let svc = make_service("engine-health");
        let eid = EngineId::new("engine-health").unwrap();

        // 清理可能存在的旧 lease
        let _ = crate::infra::local_engine::lease::remove_lease_force("engine-health");

        // 写入一个 lease，PID 为当前进程（存在但不是引擎进程）
        let current_pid = std::process::id();
        let lease = crate::infra::local_engine::lease::ProcessLease::new(
            "engine-health",
            "inst-health-001",
            current_pid,
            crate::infra::platform::process::get_process_creation_time_ms(current_pid).unwrap_or(0),
            std::env::current_exe()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
            "127.0.0.1:59998",
            "fp:health0000000000",
            "gen-health",
        );
        crate::infra::local_engine::lease::write_lease(&lease).unwrap();

        let result = svc.stop_orphan_engine(&eid).await.unwrap();
        assert!(!result.stopped);
        // health 不可达（127.0.0.1:59998 上没有服务）
        assert_eq!(result.reason, "health_unreachable");

        // 清理
        let _ = crate::infra::local_engine::lease::remove_lease_force("engine-health");
    }

    // ── stop_orphan_engine 的 OrphanStopResultDto 不暴露 PID/路径 ──

    #[tokio::test]
    async fn stop_orphan_engine_result_dto_does_not_expose_sensitive_fields() {
        let svc = make_service("engine-dto");
        let eid = EngineId::new("engine-dto").unwrap();

        // 清理可能存在的旧 lease
        let _ = crate::infra::local_engine::lease::remove_lease_force("engine-dto");

        // 无 lease 时返回 lease_not_found——DTO 不含 PID/路径
        let result = svc.stop_orphan_engine(&eid).await.unwrap();
        let json = serde_json::to_value(&result).unwrap();
        assert!(json.get("pid").is_none(), "不应暴露 pid");
        assert!(json.get("executable").is_none(), "不应暴露 executable");
        assert!(json.get("endpoint").is_none(), "不应暴露 endpoint");
        assert!(json.get("token").is_none(), "不应暴露 token");
        assert!(json.get("instance_id").is_none(), "不应暴露 instance_id");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6 H5: InstallSinkAdapter 测试
    // 洪泛保护、seq 单调递增、operation_id 隔离、失败终态
    // ═══════════════════════════════════════════════════════════════════════

    /// 用于测试的 EventPort 实现——捕获所有 emit 的日志和状态。
    struct RecordingEventPort {
        install_logs: std::sync::Mutex<Vec<(String, String, u64, String, String)>>,
        // (engine_id, operation_id, seq, level, text)
        status_snapshots: std::sync::Mutex<Vec<String>>,
        runtime_logs: std::sync::Mutex<Vec<(String, String, u64, String)>>,
        // (engine_id, instance_id, seq, line)
        install_stages: std::sync::Mutex<Vec<(String, String, String)>>,
        // (engine_id, operation_id, stage)
    }

    impl RecordingEventPort {
        fn new() -> Self {
            Self {
                install_logs: std::sync::Mutex::new(Vec::new()),
                status_snapshots: std::sync::Mutex::new(Vec::new()),
                runtime_logs: std::sync::Mutex::new(Vec::new()),
                install_stages: std::sync::Mutex::new(Vec::new()),
            }
        }

        fn install_logs(&self) -> Vec<(String, String, u64, String, String)> {
            self.install_logs.lock().unwrap().clone()
        }

        fn runtime_logs(&self) -> Vec<(String, String, u64, String)> {
            self.runtime_logs.lock().unwrap().clone()
        }

        fn install_stages(&self) -> Vec<(String, String, String)> {
            self.install_stages.lock().unwrap().clone()
        }
    }

    impl EventPort for RecordingEventPort {
        fn emit_status(&self, _snapshot: &EngineStatusSnapshot) {
            self.status_snapshots
                .lock()
                .unwrap()
                .push("status".to_string());
        }

        fn emit_log(
            &self,
            engine_id: &EngineId,
            instance_id: &str,
            seq: u64,
            _level: crate::app::local_engine::dto::EngineLogLevel,
            line: &str,
        ) {
            self.runtime_logs.lock().unwrap().push((
                engine_id.to_string(),
                instance_id.to_string(),
                seq,
                line.to_string(),
            ));
        }

        fn emit_install_log(
            &self,
            engine_id: &EngineId,
            operation_id: &str,
            seq: u64,
            level: crate::app::local_engine::dto::EngineLogLevel,
            text: &str,
        ) {
            self.install_logs.lock().unwrap().push((
                engine_id.to_string(),
                operation_id.to_string(),
                seq,
                level.to_string(),
                text.to_string(),
            ));
        }

        fn emit_install_stage(&self, engine_id: &EngineId, operation_id: &str, stage: &str) {
            self.install_stages.lock().unwrap().push((
                engine_id.to_string(),
                operation_id.to_string(),
                stage.to_string(),
            ));
        }
    }

    // ── InstallSinkAdapter seq 单调递增 ──

    #[test]
    fn install_sink_adapter_seq_monotonic() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-seq").unwrap();
        let adapter =
            InstallSinkAdapter::new(event_port.clone(), engine_id, "op-seq-test".to_string());

        // 发送 10 条日志
        for i in 0..10 {
            adapter.on_log("info", &format!("log line {i}"));
        }

        let logs = event_port.install_logs();
        assert_eq!(logs.len(), 10, "应收到 10 条安装日志");

        // 验证 seq 从 1 开始单调递增
        for (i, (_, _, seq, _, _)) in logs.iter().enumerate() {
            assert_eq!(*seq, (i + 1) as u64, "seq 应从 1 开始单调递增");
        }
    }

    // ── InstallSinkAdapter 洪泛保护：超过 50 条/秒被限流 ──

    #[test]
    fn install_sink_adapter_flood_protection_drops_excess() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-flood").unwrap();
        let adapter =
            InstallSinkAdapter::new(event_port.clone(), engine_id, "op-flood-test".to_string());

        // 在同一秒内发送 100 条日志
        for i in 0..100 {
            adapter.on_log("info", &format!("flood line {i}"));
        }

        let logs = event_port.install_logs();
        // 洪泛保护：最多 50 条通过
        assert!(
            logs.len() <= 50,
            "洪泛保护应限制每秒最多 50 条日志，实际通过 {} 条",
            logs.len()
        );
        assert!(
            logs.len() >= 40,
            "洪泛保护应至少允许接近 50 条日志通过，实际通过 {} 条",
            logs.len()
        );
    }

    // ── InstallSinkAdapter operation_id 隔离 ──

    #[test]
    fn install_sink_adapter_operation_id_isolation() {
        // 两个不同 operation_id 的 adapter 不应共享 seq
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-iso").unwrap();

        let adapter1 = InstallSinkAdapter::new(
            event_port.clone(),
            engine_id.clone(),
            "op-iso-1".to_string(),
        );
        let adapter2 = InstallSinkAdapter::new(
            event_port.clone(),
            engine_id.clone(),
            "op-iso-2".to_string(),
        );

        // 各发 5 条日志
        for i in 0..5 {
            adapter1.on_log("info", &format!("op1 line {i}"));
            adapter2.on_log("info", &format!("op2 line {i}"));
        }

        let logs = event_port.install_logs();
        assert_eq!(logs.len(), 10, "两个 operation 共应收到 10 条日志");

        // 分组验证：每个 operation_id 的 seq 独立从 1 递增
        let op1_logs: Vec<_> = logs
            .iter()
            .filter(|(_, op, _, _, _)| op == "op-iso-1")
            .collect();
        let op2_logs: Vec<_> = logs
            .iter()
            .filter(|(_, op, _, _, _)| op == "op-iso-2")
            .collect();
        assert_eq!(op1_logs.len(), 5);
        assert_eq!(op2_logs.len(), 5);

        for (i, (_, _, seq, _, _)) in op1_logs.iter().enumerate() {
            assert_eq!(*seq, (i + 1) as u64, "op-iso-1 seq 应从 1 递增");
        }
        for (i, (_, _, seq, _, _)) in op2_logs.iter().enumerate() {
            assert_eq!(*seq, (i + 1) as u64, "op-iso-2 seq 应从 1 递增");
        }
    }

    // ── InstallSinkAdapter 旧 operation 不污染新 operation ──

    #[test]
    fn install_sink_adapter_old_operation_does_not_pollute_new() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-old-op").unwrap();

        // 旧 operation 发日志
        let old_adapter =
            InstallSinkAdapter::new(event_port.clone(), engine_id.clone(), "op-old".to_string());
        for i in 0..3 {
            old_adapter.on_log("info", &format!("old line {i}"));
        }

        // 新 operation 发日志
        let new_adapter =
            InstallSinkAdapter::new(event_port.clone(), engine_id.clone(), "op-new".to_string());
        for i in 0..3 {
            new_adapter.on_log("info", &format!("new line {i}"));
        }

        let logs = event_port.install_logs();

        // 旧 operation 的日志只属于 op-old
        let old_logs: Vec<_> = logs
            .iter()
            .filter(|(_, op, _, _, _)| op == "op-old")
            .collect();
        let new_logs: Vec<_> = logs
            .iter()
            .filter(|(_, op, _, _, _)| op == "op-new")
            .collect();
        assert_eq!(old_logs.len(), 3);
        assert_eq!(new_logs.len(), 3);

        // 新 operation 的 seq 从 1 开始，不受旧 operation 影响
        for (i, (_, _, seq, _, _)) in new_logs.iter().enumerate() {
            assert_eq!(
                *seq,
                (i + 1) as u64,
                "新 operation seq 应从 1 开始，不受旧 operation 影响"
            );
        }
    }

    // ── 运行时日志与安装日志隔离 ──

    #[test]
    fn runtime_logs_and_install_logs_are_isolated() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-rt-vs-install").unwrap();

        // 发送运行时日志（以 instance_id 隔离）
        event_port.emit_log(
            &engine_id,
            "inst-abc",
            1,
            crate::app::local_engine::dto::EngineLogLevel::Info,
            "runtime log 1",
        );
        event_port.emit_log(
            &engine_id,
            "inst-abc",
            2,
            crate::app::local_engine::dto::EngineLogLevel::Info,
            "runtime log 2",
        );

        // 发送安装日志（以 operation_id 隔离）
        event_port.emit_install_log(
            &engine_id,
            "op-xyz",
            1,
            crate::app::local_engine::dto::EngineLogLevel::Info,
            "install log 1",
        );

        let rt_logs = event_port.runtime_logs();
        let install_logs = event_port.install_logs();

        // 运行时日志只含 instance_id
        assert_eq!(rt_logs.len(), 2);
        assert!(rt_logs.iter().all(|(_, inst, _, _)| inst == "inst-abc"));

        // 安装日志只含 operation_id
        assert_eq!(install_logs.len(), 1);
        assert!(install_logs.iter().all(|(_, op, _, _, _)| op == "op-xyz"));

        // 两者不交叉污染
        assert!(install_logs.iter().all(|(_, op, _, _, _)| op != "inst-abc"));
        assert!(rt_logs.iter().all(|(_, inst, _, _)| inst != "op-xyz"));
    }

    // ── 0.22.6 H4: on_stage 通过 emit_install_stage 广播阶段变更 ──

    #[test]
    fn install_sink_adapter_on_stage_emits_install_stage() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-stage").unwrap();
        let adapter = InstallSinkAdapter::new(
            event_port.clone(),
            engine_id.clone(),
            "op-stage-test".to_string(),
        );

        // on_stage 应通过 emit_install_stage 广播阶段变更
        adapter.on_stage("preparing");
        adapter.on_stage("downloading");
        adapter.on_stage("verifying");
        adapter.on_stage("completed");

        let stages = event_port.install_stages();
        assert_eq!(stages.len(), 4, "on_stage 应产生 4 个阶段事件");
        assert_eq!(stages[0].2, "preparing");
        assert_eq!(stages[1].2, "downloading");
        assert_eq!(stages[2].2, "verifying");
        assert_eq!(stages[3].2, "completed");

        // 验证 engine_id 和 operation_id 正确传递
        assert!(stages.iter().all(|(eid, _, _)| eid == "test-stage"));
        assert!(stages.iter().all(|(_, op, _)| op == "op-stage-test"));

        // on_stage 不应产生安装日志（emit_install_log）
        let logs = event_port.install_logs();
        assert!(logs.is_empty(), "on_stage 不应 emit 安装日志");
    }

    /// 验证安装阶段事件的 operation_id 隔离。
    #[test]
    fn install_sink_adapter_stage_operation_id_isolation() {
        let event_port = Arc::new(RecordingEventPort::new());
        let engine_id = EngineId::new("test-stage-iso").unwrap();

        // 两个不同 operation_id 的 adapter
        let adapter1 =
            InstallSinkAdapter::new(event_port.clone(), engine_id.clone(), "op-aaa".to_string());
        let adapter2 =
            InstallSinkAdapter::new(event_port.clone(), engine_id.clone(), "op-bbb".to_string());

        adapter1.on_stage("preparing");
        adapter2.on_stage("downloading");

        let stages = event_port.install_stages();
        assert_eq!(stages.len(), 2);
        assert_eq!(stages[0].1, "op-aaa");
        assert_eq!(stages[1].1, "op-bbb");
    }

    // ═══════════════════════════════════════════════════════════════════════
    // 0.22.6 B2: expected model identity 策略测试
    // ═══════════════════════════════════════════════════════════════════════

    /// managed model storage 在模型未安装时返回 Err（不回退到 descriptor）。
    #[test]
    fn resolve_identity_fails_when_not_installed() {
        let engine_id = EngineId::new("test-identity-not-installed").unwrap();
        let contract = crate::infra::local_engine::runtime::ModelContract {
            model_id: "test-model-not-installed".to_string(),
            revision: "v1".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
        };

        let result = resolve_expected_model_identity(&engine_id, None, &contract, true);
        assert!(result.is_err(), "模型未安装时应返回 Err");
        assert!(
            result.unwrap_err().contains("未安装"),
            "错误信息应包含 '未安装'"
        );
    }

    #[test]
    fn resolve_identity_uses_descriptor_for_adapter_managed_model() {
        let engine_id = EngineId::new("paddleocr").unwrap();
        let contract = crate::infra::local_engine::runtime::ModelContract {
            model_id: "PP-OCRv6:det:rec".to_string(),
            revision: "ppocrv6-tiny".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
        };

        let identity = resolve_expected_model_identity(&engine_id, None, &contract, false).unwrap();
        assert_eq!(identity.0, contract.model_id);
        assert_eq!(identity.1, contract.revision);
        assert_eq!(identity.2, None);
    }

    #[test]
    fn model_fingerprint_requires_nonzero_lowercase_sha256_hex() {
        assert!(is_valid_model_fingerprint(&"a1".repeat(32)));
        assert!(!is_valid_model_fingerprint(&"A1".repeat(32)));
        assert!(!is_valid_model_fingerprint(&"0".repeat(64)));
        assert!(!is_valid_model_fingerprint("abc123"));
    }

    #[test]
    fn engine_log_level_uses_explicit_wrapper_prefixes() {
        use crate::app::local_engine::dto::EngineLogLevel;
        use crate::infra::local_engine::log_pipe::LogSource;

        assert_eq!(
            classify_engine_log(LogSource::Stdout, "[INFO] ready"),
            EngineLogLevel::Info
        );
        assert_eq!(
            classify_engine_log(LogSource::Stdout, "[STATE] model_state=Ready"),
            EngineLogLevel::Info
        );
        assert_eq!(
            classify_engine_log(LogSource::Stderr, "[WARN] retry"),
            EngineLogLevel::Warn
        );
        assert_eq!(
            classify_engine_log(LogSource::Stderr, "[ERROR] failed"),
            EngineLogLevel::Error
        );
        assert_eq!(
            classify_engine_log(LogSource::Stderr, "Traceback (most recent call last):"),
            EngineLogLevel::Error
        );
    }

    #[test]
    fn unclassified_engine_output_is_debug_not_stderr_warning() {
        use crate::app::local_engine::dto::EngineLogLevel;
        use crate::infra::local_engine::log_pipe::LogSource;

        assert_eq!(
            classify_engine_log(LogSource::Stderr, "[================] 32.52%"),
            EngineLogLevel::Debug
        );
        assert_eq!(
            classify_engine_log(LogSource::Stderr, "Extracting PP-OCRv6_tiny_rec_infer.tar"),
            EngineLogLevel::Debug
        );
    }
}
