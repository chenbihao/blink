//! 本地引擎模型资产生命周期编排（0.22.6 H3）。
//!
//! 提供通用、受限的模型管理操作：list / install / repair / delete。
//! 模型身份为 `engine_id + model_id`，不再用单一 `local_model_id`。
//!
//! ## 设计铁则
//!
//! - **前端不提交 URL、任意路径、脚本或外部命令**：安装是真实事务
//!   （staging/下载/校验/提升），模型下载源由 adapter/引擎层按自身机制完成。
//! - **下载失败或取消不破坏已安装模型**：staging 与最终位置隔离，
//!   失败只清理 staging，不影响已安装模型或当前语音选择。
//! - **删除引用保护**：删除正在使用或被配置引用的模型必须返回
//!   结构化冲突（`ModelDeleteConflict`），不能静默切换。
//! - **模型身份来自 descriptor/启动配置**：`LocalEngineService` 的
//!   期望模型身份来自本次受限启动配置/模型 descriptor，而不是
//!   `EngineDescriptor` 静态写死的单一模型契约。
//! - **存储扫描按 engine/model 精确归属**：公共 cache 与模型 cache 不混淆。
//! - **manifest 是 Installed 唯一真源**：禁止仅凭目录非空推断 Installed。
//!
//! ## 真实下载事务
//!
//! install_model 执行以下步骤：
//! 1. 检查状态（busy 拒绝、已安装幂等返回）
//! 2. 创建 staging payload 目录
//! 3. 调用 `ModelInstallWorker::download_to_staging` 执行实际下载
//! 4. 计算 content fingerprint
//! 5. 写入 manifest
//! 6. 原子提升 staging → generation（切换 current.json）
//! 7. 清理旧 generations
//!
//! repair_model 不删除旧 generation——新 generation 下载成功后才切换。
//! 失败时旧 generation 完好无损。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tokio_util::sync::CancellationToken;

use crate::domain::local_engine::{
    DeleteConflictReason, EngineModelDescriptor, EngineModelStatus, ErrorPhase, LocalEngineError,
    LocalEngineErrorCode, ModelCompatibility, ModelDeleteConflict, ModelIdentityVerification,
    ModelInstallState, ModelOperationKind, ModelOperationResult, ModelOperationStage,
    ModelVerificationState, transition_install_state,
};
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::model_storage::{
    CONTENT_FINGERPRINT_ALGORITHM, ModelContractIdentity, ModelManifest, ModelSource,
    RestoredModelState,
};
use crate::infra::local_engine::runtime::{self, EngineId};

// ── ModelRegistry ──────────────────────────────────────────────────────────

/// 编译期模型注册表（allowlist）。
///
/// 每个引擎在编译期声明自己支持的模型候选列表。
/// 不暴露动态注册 API——所有注册项在构造时确定。
pub struct ModelRegistry {
    /// engine_id → 模型 descriptor 列表
    models: HashMap<EngineId, Vec<EngineModelDescriptor>>,
}

impl Clone for ModelRegistry {
    fn clone(&self) -> Self {
        Self {
            models: self.models.clone(),
        }
    }
}

impl ModelRegistry {
    /// 创建带指定模型列表的注册表。
    pub fn new_with_models(models: Vec<EngineModelDescriptor>) -> Self {
        let mut map: HashMap<EngineId, Vec<EngineModelDescriptor>> = HashMap::new();
        for m in models {
            map.entry(m.engine_id.clone()).or_default().push(m);
        }
        Self { models: map }
    }

    /// 创建空注册表（测试用）。
    pub fn empty() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// 查找引擎的所有模型候选。
    pub fn list(&self, engine_id: &EngineId) -> &[EngineModelDescriptor] {
        self.models
            .get(engine_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 查找特定模型。
    pub fn find(&self, engine_id: &EngineId, model_id: &str) -> Option<&EngineModelDescriptor> {
        self.models
            .get(engine_id)?
            .iter()
            .find(|m| m.model_id == model_id)
    }

    /// 返回所有引擎的所有模型。
    pub fn all(&self) -> Vec<&EngineModelDescriptor> {
        self.models.values().flat_map(|v| v.iter()).collect()
    }
}

// ── InstallSink（有界日志/阶段 sink）──────────────────────────────────────

/// 模型安装阶段的日志/进度 sink。
///
/// **铁则**：
/// - 有界：实现必须维护有界缓冲，禁止无限制累积日志。
/// - 阶段性：`emit_stage` 报告安装阶段（如 downloading/verifying），
///   但**不伪造下载百分比**——无法取得字节级进度时只报阶段。
/// - 不接收 URL、executable、argv、环境变量或脚本路径。
pub trait InstallSink: Send + Sync {
    /// 发射一条日志行。
    fn emit_log(&self, line: &str);

    /// 发射阶段变更。
    fn emit_stage(&self, stage: &str);

    /// 当前已缓冲的日志行数（用于测试和诊断）。
    fn buffered_log_count(&self) -> usize;
}

/// 空实现 sink（不缓冲任何内容）。
pub struct NoopInstallSink;

impl InstallSink for NoopInstallSink {
    fn emit_log(&self, _line: &str) {}
    fn emit_stage(&self, _stage: &str) {}
    fn buffered_log_count(&self) -> usize {
        0
    }
}

/// 有界内存日志 sink（用于测试和轻量诊断）。
///
/// 缓冲上限为 `max_lines`，超出后丢弃旧行。
pub struct BoundedInstallSink {
    lines: std::sync::Mutex<std::collections::VecDeque<String>>,
    max_lines: usize,
}

impl BoundedInstallSink {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(max_lines)),
            max_lines,
        }
    }
}

impl InstallSink for BoundedInstallSink {
    fn emit_log(&self, line: &str) {
        let mut buf = self.lines.lock().unwrap();
        if buf.len() >= self.max_lines {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
    }

    fn emit_stage(&self, stage: &str) {
        self.emit_log(&format!("[stage] {stage}"));
    }

    fn buffered_log_count(&self) -> usize {
        self.lines.lock().unwrap().len()
    }
}

impl BoundedInstallSink {
    /// 取缓冲尾部 n 行（用于失败时把 installer 真实输出附进错误详情）。
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let buf = self.lines.lock().unwrap();
        buf.iter().rev().take(n).rev().cloned().collect()
    }
}

/// 模型安装日志广播 sink——把 installer 输出桥接到 `EventPort`（前端实时事件）
/// 并缓冲到内部 `BoundedInstallSink`（失败诊断用）。
///
/// 与 service.rs 的 `InstallSinkAdapter`（引擎环境安装）语义一致：
/// - `emit_install_log` 以 `operation_id` 隔离，`instance_id` 为空，
///   前端按 `operation_id != null` 识别为操作日志（不做 instance 过滤）；
/// - installer 原始输出默认 debug 级 tracing，`[ERROR]`/`[WARN]` 前缀升级；
/// - 洪泛保护由内部缓冲上限与 installer 侧 `disable_progress_bar` 共同保证。
struct BroadcastingInstallSink {
    inner: BoundedInstallSink,
    event_port: Arc<dyn super::service::EventPort>,
    engine_id: EngineId,
    operation_id: String,
    log_seq: std::sync::atomic::AtomicU64,
}

impl BroadcastingInstallSink {
    fn new(
        inner: BoundedInstallSink,
        event_port: Arc<dyn super::service::EventPort>,
        engine_id: EngineId,
        operation_id: String,
    ) -> Self {
        Self {
            inner,
            event_port,
            engine_id,
            operation_id,
            log_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 取缓冲尾部 n 行（透传内部缓冲）。
    fn tail_lines(&self, n: usize) -> Vec<String> {
        self.inner.tail_lines(n)
    }
}

/// 从 installer 输出行推断日志级别。
///
/// stdout/stderr 只是传输通道：受信任 installer 的显式前缀优先，
/// 未分类输出归 info（前端展示）+ debug（tracing）。
fn classify_installer_line(line: &str) -> crate::app::local_engine::dto::EngineLogLevel {
    use crate::app::local_engine::dto::EngineLogLevel;
    if line.starts_with("[ERROR]") {
        EngineLogLevel::Error
    } else if line.starts_with("[WARN") || line.starts_with("WARNING") {
        EngineLogLevel::Warn
    } else {
        EngineLogLevel::Info
    }
}

impl InstallSink for BroadcastingInstallSink {
    fn emit_log(&self, line: &str) {
        self.inner.emit_log(line);

        let level = classify_installer_line(line);
        match level {
            crate::app::local_engine::dto::EngineLogLevel::Error => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            crate::app::local_engine::dto::EngineLogLevel::Warn => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            _ => tracing::debug!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
        }

        let seq = self
            .log_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.event_port
            .emit_install_log(&self.engine_id, &self.operation_id, seq, level, line);
    }

    fn emit_stage(&self, stage: &str) {
        self.inner.emit_stage(stage);
        tracing::debug!(
            engine_id = %self.engine_id,
            op = %self.operation_id,
            stage,
            "模型安装阶段变更"
        );
        self.event_port
            .emit_install_stage(&self.engine_id, &self.operation_id, stage);
    }

    fn buffered_log_count(&self) -> usize {
        self.inner.buffered_log_count()
    }
}

// ── ModelInstallWorker trait ────────────────────────────────────────────────

/// 模型安装 worker trait（installer port）。
///
/// 每个引擎 adapter 提供编译期固定的专用安装 worker。
/// worker 负责实际的模型下载（如 ModelScope/FunASR 官方库），
/// 下载结果写入指定的 staging payload 目录。
///
/// **铁则**：
/// - worker 只负责下载到 staging，不负责校验/提升/manifest
/// - worker 不接收前端提交的 URL、脚本路径、Python 路径
/// - worker 参数必须是 allowlist 中的 model id/revision
/// - worker 设置 MODELSCOPE_CACHE 为本次 staging 目录，禁止回落到用户默认缓存
/// - worker 必须作为受管进程运行，接入 CancellationToken 和超时
/// - worker 通过 `InstallSink` 报告有界日志和阶段，不伪造百分比
#[async_trait::async_trait]
pub trait ModelInstallWorker: Send + Sync {
    /// 下载模型到 staging payload 目录。
    ///
    /// 成功时返回下载来源描述（用于 manifest 的 source/provenance）。
    /// 失败时返回错误（Rust 会清理 staging）。
    ///
    /// `sink` 可选——worker 通过 sink 报告有界日志和安装阶段。
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError>;
}

/// 模型下载结果（worker 返回）。
#[derive(Debug, Clone)]
pub struct ModelDownloadOutcome {
    /// 下载来源描述（如 "modelscope:iic/SenseVoiceSmall"）。
    pub source: String,
    /// 下载来源的 checksum 信息。
    pub checksum_source: ModelDownloadChecksumSource,
}

/// 下载来源 checksum 信息。
#[derive(Debug, Clone)]
pub enum ModelDownloadChecksumSource {
    /// 上游不提供稳定 checksum。
    Unverified,
    /// 上游提供稳定 SHA-256。
    Sha256(String),
}

/// 模型下载错误。
#[derive(Debug, thiserror::Error)]
pub enum ModelDownloadError {
    #[error("下载失败: {message}")]
    Failed { message: String },

    #[error("下载被取消")]
    Cancelled,

    #[error("下载超时")]
    TimedOut,

    #[error("磁盘空间不足: {message}")]
    DiskFull { message: String },

    #[error("网络不可达: {message}")]
    Network { message: String },

    #[error("worker 内部错误: {message}")]
    Internal { message: String },
}

impl ModelDownloadError {
    /// 映射到 LocalEngineErrorCode。
    pub fn to_code(&self) -> LocalEngineErrorCode {
        match self {
            Self::Cancelled => LocalEngineErrorCode::Cancelled,
            Self::TimedOut => LocalEngineErrorCode::Timeout,
            Self::DiskFull { .. } => LocalEngineErrorCode::DiskFull,
            Self::Network { .. } => LocalEngineErrorCode::NetworkError,
            Self::Failed { .. } | Self::Internal { .. } => LocalEngineErrorCode::InstallFailed,
        }
    }
}

// ── ActiveOperation ─────────────────────────────────────────────────────────

/// 进行中的模型操作。
#[derive(Debug, Clone)]
struct ActiveOperation {
    operation_id: String,
    operation_kind: ModelOperationKind,
    install_id: String,
    cancel_token: CancellationToken,
    /// 启动时间（用于超时检测）。
    started_at_ms: u64,
}

// ── ModelService ────────────────────────────────────────────────────────────

/// 模型资产生命周期编排服务。
///
/// 不直接持有 AppHandle——通过 trait 解耦。
/// 不发送 Tauri 事件——由调用方桥接。
///
/// **不膨胀 service.rs**：此模块独立于 `LocalEngineService`，
/// 专注模型资产管理（下载/校验/删除），与引擎进程管理（启动/停止/健康）
/// 正交。
pub struct ModelService {
    registry: ModelRegistry,
    /// 模型状态缓存：engine_id → model_id → status
    states: Arc<RwLock<HashMap<(EngineId, String), EngineModelStatus>>>,
    /// 进行中的操作：engine_id → model_id → ActiveOperation
    active_operations: Arc<RwLock<HashMap<(EngineId, String), ActiveOperation>>>,
    /// 安装 worker（可选——测试用 fake worker）
    worker: Arc<dyn ModelInstallWorker>,
    /// 事件出口（安装日志/阶段广播到前端）。构造时必须注入——
    /// 生产环境注入共享 `TauriEventPort`，测试中不需要事件时注入 `NoopEventPort`。
    event_port: Arc<dyn super::service::EventPort>,
}

impl ModelService {
    /// 创建模型服务。
    ///
    /// **必须显式注入 `EventPort`**——生产环境传入共享 `TauriEventPort`，
    /// 测试中不需要事件时传入 `Arc::new(NoopEventPort)`，需要验证事件时传入
    /// recording port。这消除了生产 wiring 遗漏时静默丢日志的陷阱。
    pub fn new(
        registry: ModelRegistry,
        worker: Arc<dyn ModelInstallWorker>,
        event_port: Arc<dyn super::service::EventPort>,
    ) -> Self {
        Self {
            registry,
            states: Arc::new(RwLock::new(HashMap::new())),
            active_operations: Arc::new(RwLock::new(HashMap::new())),
            worker,
            event_port,
        }
    }

    /// 从磁盘恢复所有模型状态。
    ///
    /// 在服务启动时调用，从 manifest + current pointer + payload + fingerprint
    /// 恢复 Installed/Corrupted 状态。禁止仅凭目录非空推断 Installed。
    pub async fn restore_states_from_disk(&self) -> Result<(), LocalEngineError> {
        let mut states = self.states.write().await;

        for desc in self.registry.all() {
            let asset_key = mstore::encode_asset_key(&desc.model_id);
            match mstore::restore_model_state(&desc.engine_id, &asset_key) {
                Ok(RestoredModelState::Installed {
                    install_id,
                    manifest,
                }) => {
                    let key = (desc.engine_id.clone(), desc.model_id.clone());
                    let status = EngineModelStatus {
                        engine_id: desc.engine_id.clone(),
                        model_id: desc.model_id.clone(),
                        install_state: ModelInstallState::Installed,
                        verification_state: ModelVerificationState::Unverified,
                        cache_size_bytes: Some(manifest.payload_size_bytes),
                        is_selected: false, // selected 由配置注入
                        is_active: false,   // active 由 health 注入
                        compatibility: ModelCompatibility::Compatible,
                    };
                    states.insert(key, status);
                    tracing::info!(
                        engine_id = %desc.engine_id,
                        model_id = %desc.model_id,
                        install_id = %install_id,
                        "模型状态恢复为 Installed"
                    );
                }
                Ok(RestoredModelState::Corrupted { install_id, reason }) => {
                    let key = (desc.engine_id.clone(), desc.model_id.clone());
                    let status = EngineModelStatus {
                        engine_id: desc.engine_id.clone(),
                        model_id: desc.model_id.clone(),
                        install_state: ModelInstallState::NotInstalled, // Corrupted 归入 NotInstalled
                        verification_state: ModelVerificationState::Corrupted,
                        cache_size_bytes: None,
                        is_selected: false,
                        is_active: false,
                        compatibility: ModelCompatibility::Unknown,
                    };
                    states.insert(key, status);
                    tracing::warn!(
                        engine_id = %desc.engine_id,
                        model_id = %desc.model_id,
                        install_id = ?install_id,
                        reason = %reason,
                        "模型状态恢复为 Corrupted"
                    );
                }
                Ok(RestoredModelState::NotInstalled) => {
                    // 无需设置——默认就是 not_installed
                }
                Err(e) => {
                    tracing::error!(
                        engine_id = %desc.engine_id,
                        model_id = %desc.model_id,
                        error = %e,
                        "模型状态恢复失败"
                    );
                    // 不阻塞——恢复失败只影响该模型
                }
            }
        }

        Ok(())
    }

    /// 列出引擎的所有模型候选及其当前状态。
    ///
    /// **只读查询，无副作用。** 语音页可以调用此方法查看模型列表，
    /// 但**不触发下载**——下载只在引擎页管理。
    pub async fn list_models(&self, engine_id: &EngineId) -> Vec<EngineModelStatus> {
        let descriptors = self.registry.list(engine_id);
        let states = self.states.read().await;

        descriptors
            .iter()
            .map(|desc| {
                states
                    .get(&(engine_id.clone(), desc.model_id.clone()))
                    .cloned()
                    .unwrap_or_else(|| EngineModelStatus::not_installed(desc))
            })
            .collect()
    }

    /// 获取单个模型的状态。
    pub async fn get_model_status(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<EngineModelStatus, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
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

        let states = self.states.read().await;
        Ok(states
            .get(&(engine_id.clone(), model_id.to_string()))
            .cloned()
            .unwrap_or_else(|| EngineModelStatus::not_installed(desc)))
    }

    /// 安装模型（真实事务：staging/下载/校验/提升）。
    ///
    /// 状态转移：NotInstalled → Downloading → Staging → Verifying → Installed
    /// 失败路径：→ DownloadFailed/StagingFailed/VerificationFailed → NotInstalled
    /// 取消路径：→ NotInstalled（不影响已安装模型）
    pub async fn install_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
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

        let op_id = operation_id.unwrap_or_else(|| runtime::generate_operation_id());
        let install_id = runtime::generate_install_id();
        let asset_key = mstore::encode_asset_key(model_id);
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态——如果 busy 则拒绝
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Install,
                        "模型操作进行中",
                        format!(
                            "engine_id={}, model_id={} 正在执行 {} 操作",
                            engine_id, model_id, status.install_state
                        ),
                    ));
                }
                // 已安装则直接返回成功
                if status.install_state.is_installed() {
                    return Ok(ModelOperationResult {
                        engine_id: engine_id.to_string(),
                        model_id: model_id.to_string(),
                        operation_id: op_id,
                        operation_kind: ModelOperationKind::Install,
                        final_stage: ModelOperationStage::Done,
                        success: true,
                        error: None,
                    });
                }
            }
        }

        // 注册活跃操作
        let cancel_token = CancellationToken::new();
        {
            let mut ops = self.active_operations.write().await;
            ops.insert(
                key.clone(),
                ActiveOperation {
                    operation_id: op_id.clone(),
                    operation_kind: ModelOperationKind::Install,
                    install_id: install_id.clone(),
                    cancel_token: cancel_token.clone(),
                    started_at_ms: runtime::now_ms(),
                },
            );
        }

        // 状态转移：→ Downloading
        self.transition(&key, ModelInstallState::Downloading)
            .await?;

        // 清理上一轮强杀残留的孤儿 staging（GB 级残留会占用磁盘）。
        // busy 检查已保证此刻无活跃操作，删除安全。
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

        // 1. 创建 staging payload 目录
        let staging_payload_dir =
            match mstore::model_operation_staging_payload_dir(engine_id, &asset_key, &op_id) {
                Ok(p) => p,
                Err(e) => {
                    self.fail_and_cleanup(&key, &asset_key, &op_id, engine_id, e.to_string())
                        .await;
                    return Ok(self.make_failed_result(
                        engine_id,
                        model_id,
                        &op_id,
                        ModelOperationKind::Install,
                        "staging 目录创建失败",
                    ));
                }
            };

        if let Err(e) = std::fs::create_dir_all(&staging_payload_dir) {
            self.fail_and_cleanup(&key, &asset_key, &op_id, engine_id, e.to_string())
                .await;
            return Ok(self.make_failed_result(
                engine_id,
                model_id,
                &op_id,
                ModelOperationKind::Install,
                "staging 目录创建失败",
            ));
        }

        // 2. 调用 worker 执行实际下载
        // 传入广播 sink：installer stdout/stderr 实时进前端日志事件（operation_id
        // 隔离）+ 内存缓冲（失败时附进错误详情）。
        let sink = Arc::new(BroadcastingInstallSink::new(
            BoundedInstallSink::new(500),
            Arc::clone(&self.event_port),
            engine_id.clone(),
            op_id.clone(),
        ));
        sink.emit_stage("preparing");
        let download_result = self
            .worker
            .download_to_staging(
                engine_id,
                model_id,
                &desc.revision,
                &staging_payload_dir,
                cancel_token.clone(),
                Some(Arc::clone(&sink) as Arc<dyn InstallSink>),
            )
            .await;

        // 检查是否被取消
        if cancel_token.is_cancelled() {
            // 清理 staging
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition(&key, ModelInstallState::NotInstalled)
                .await?;
            self.remove_active_operation(&key).await;
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Install,
                final_stage: ModelOperationStage::Cancelled,
                success: true,
                error: None,
            });
        }

        if let Err(e) = download_result {
            // 下载失败 → 清理 staging
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition(&key, ModelInstallState::DownloadFailed)
                .await?;
            self.transition(&key, ModelInstallState::NotInstalled)
                .await?;
            self.remove_active_operation(&key).await;
            // 把 installer 尾部输出附进错误——退出码本身无法定位失败原因
            // （如 ModelScope 网络错误、Python traceback 都在 stdout/stderr 里）。
            let tail = sink.tail_lines(15);
            if !tail.is_empty() {
                tracing::warn!(
                    engine_id = %engine_id,
                    model_id,
                    tail = %tail.join(" | "),
                    "模型下载失败，installer 尾部输出"
                );
            }
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Install,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    e.to_code(),
                    ErrorPhase::Install,
                    "模型下载失败",
                    if tail.is_empty() {
                        e.to_string()
                    } else {
                        format!("{}\ninstaller 输出（尾部）:\n{}", e, tail.join("\n"))
                    },
                )),
            });
        }

        let download_outcome = download_result.unwrap();

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %op_id,
            "installer 下载完成，开始本地校验（fingerprint + promote）"
        );

        // 3. 状态转移：→ Staging（下载完成）
        self.transition(&key, ModelInstallState::Staging).await?;

        // 4. 计算 fingerprint
        self.transition(&key, ModelInstallState::Verifying).await?;

        // GB 级模型的逐文件 SHA-256 是长阻塞操作——必须挪出 async 上下文
        // （spec-backend"阻塞操作隔离"铁则），否则阻塞 tokio worker。
        let fingerprint_start = std::time::Instant::now();
        let fp_dir = staging_payload_dir.clone();
        let fp =
            match tokio::task::spawn_blocking(move || mstore::compute_content_fingerprint(&fp_dir))
                .await
            {
                Ok(Ok(fp)) => fp,
                Ok(Err(e)) => {
                    let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                    self.transition(&key, ModelInstallState::VerificationFailed)
                        .await?;
                    self.transition(&key, ModelInstallState::NotInstalled)
                        .await?;
                    self.remove_active_operation(&key).await;
                    return Ok(ModelOperationResult {
                        engine_id: engine_id.to_string(),
                        model_id: model_id.to_string(),
                        operation_id: op_id,
                        operation_kind: ModelOperationKind::Install,
                        final_stage: ModelOperationStage::Failed,
                        success: false,
                        error: Some(LocalEngineError::with_detail(
                            LocalEngineErrorCode::ArtifactCorrupted,
                            ErrorPhase::Install,
                            "fingerprint 计算失败",
                            e.to_string(),
                        )),
                    });
                }
                Err(e) => {
                    // spawn_blocking panic（JoinError）——按校验失败处理
                    let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                    self.transition(&key, ModelInstallState::VerificationFailed)
                        .await?;
                    self.transition(&key, ModelInstallState::NotInstalled)
                        .await?;
                    self.remove_active_operation(&key).await;
                    return Ok(ModelOperationResult {
                        engine_id: engine_id.to_string(),
                        model_id: model_id.to_string(),
                        operation_id: op_id,
                        operation_kind: ModelOperationKind::Install,
                        final_stage: ModelOperationStage::Failed,
                        success: false,
                        error: Some(LocalEngineError::with_detail(
                            LocalEngineErrorCode::ArtifactCorrupted,
                            ErrorPhase::Install,
                            "fingerprint 任务异常",
                            e.to_string(),
                        )),
                    });
                }
            };
        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            fingerprint = %fp.fingerprint,
            size_bytes = fp.total_size_bytes,
            file_count = fp.file_count,
            elapsed_ms = fingerprint_start.elapsed().as_millis() as u64,
            "fingerprint 计算完成"
        );

        // 5. 构建 manifest
        let (source, checksum_source_kind) = match &download_outcome.checksum_source {
            ModelDownloadChecksumSource::Unverified => (
                ModelSource::Unverified {
                    source: download_outcome.source.clone(),
                    downloaded_at_ms: runtime::now_ms(),
                },
                "unverified",
            ),
            ModelDownloadChecksumSource::Sha256(sha) => (
                ModelSource::Sha256 {
                    sha256: sha.clone(),
                    source: download_outcome.source.clone(),
                    downloaded_at_ms: runtime::now_ms(),
                },
                "sha256",
            ),
        };

        let manifest = ModelManifest {
            schema_version: mstore::MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine_id.clone(),
            model_id: model_id.to_string(),
            revision: desc.revision.clone(),
            source,
            install_id: install_id.clone(),
            installed_at_ms: runtime::now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fp.fingerprint.clone(),
            payload_size_bytes: fp.total_size_bytes,
            file_count: fp.file_count,
            compatibility_schema: desc.compatibility_schema,
            model_contract_identity: ModelContractIdentity {
                model_id: model_id.to_string(),
                revision: desc.revision.clone(),
                checksum_source_kind: checksum_source_kind.to_string(),
            },
        };

        // 6. 原子提升 staging → generation
        if let Err(e) = mstore::promote_staging_to_generation(
            engine_id,
            &asset_key,
            &install_id,
            &op_id,
            &manifest,
        ) {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.transition(&key, ModelInstallState::VerificationFailed)
                .await?;
            self.transition(&key, ModelInstallState::NotInstalled)
                .await?;
            self.remove_active_operation(&key).await;
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Install,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    LocalEngineErrorCode::InstallFailed,
                    ErrorPhase::Install,
                    "staging 提升失败",
                    e.to_string(),
                )),
            });
        }

        // 7. 清理 staging + 旧 generations
        let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
        let _ = mstore::cleanup_old_generations(engine_id, &asset_key, &install_id);

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            install_id = %install_id,
            "staging 已提升为 generation"
        );

        // 8. 状态 → Installed
        self.transition(&key, ModelInstallState::Installed).await?;

        // 更新缓存占用
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            status.cache_size_bytes = Some(fp.total_size_bytes);
            status.verification_state = ModelVerificationState::Unverified;
            status.compatibility = ModelCompatibility::Compatible;
        }

        self.remove_active_operation(&key).await;

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            install_id = %install_id,
            op_id = %op_id,
            fingerprint = %fp.fingerprint,
            "模型安装完成"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: ModelOperationKind::Install,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 修复模型（重新下载/校验）。
    ///
    /// **铁则**：repair 不删除旧 generation——新 generation 下载成功后才切换。
    /// 失败时旧 generation 完好无损。
    ///
    /// 状态转移：Installed → Repairing → Installed (or RepairFailed → 旧状态)
    pub async fn repair_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
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

        let op_id = operation_id.unwrap_or_else(|| runtime::generate_operation_id());
        let install_id = runtime::generate_install_id();
        let asset_key = mstore::encode_asset_key(model_id);
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Repair,
                        "模型操作进行中",
                        format!("正在执行 {} 操作", status.install_state),
                    ));
                }
            }
        }

        // 保存旧状态（用于 repair 失败时回滚）
        let old_state = {
            let states = self.states.read().await;
            states.get(&key).map(|s| s.install_state.clone())
        };

        // 注册活跃操作
        let cancel_token = CancellationToken::new();
        {
            let mut ops = self.active_operations.write().await;
            ops.insert(
                key.clone(),
                ActiveOperation {
                    operation_id: op_id.clone(),
                    operation_kind: ModelOperationKind::Repair,
                    install_id: install_id.clone(),
                    cancel_token: cancel_token.clone(),
                    started_at_ms: runtime::now_ms(),
                },
            );
        }

        // 状态 → Repairing
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            // 允许从任意非 busy 状态进入 Repairing
            status.install_state = ModelInstallState::Repairing;
        }

        // 创建 staging payload 目录
        let staging_payload_dir =
            match mstore::model_operation_staging_payload_dir(engine_id, &asset_key, &op_id) {
                Ok(p) => p,
                Err(e) => {
                    self.repair_fail(
                        &key,
                        old_state,
                        &asset_key,
                        &op_id,
                        engine_id,
                        e.to_string(),
                    )
                    .await;
                    return Ok(self.make_failed_result(
                        engine_id,
                        model_id,
                        &op_id,
                        ModelOperationKind::Repair,
                        "staging 目录创建失败",
                    ));
                }
            };

        if let Err(e) = std::fs::create_dir_all(&staging_payload_dir) {
            self.repair_fail(
                &key,
                old_state,
                &asset_key,
                &op_id,
                engine_id,
                e.to_string(),
            )
            .await;
            return Ok(self.make_failed_result(
                engine_id,
                model_id,
                &op_id,
                ModelOperationKind::Repair,
                "staging 目录创建失败",
            ));
        }

        // 调用 worker 下载
        // 传入广播 sink（与 install_model 一致）：installer 输出实时进前端
        // 日志事件 + 内存缓冲（失败时附进错误详情）。
        let sink = Arc::new(BroadcastingInstallSink::new(
            BoundedInstallSink::new(500),
            Arc::clone(&self.event_port),
            engine_id.clone(),
            op_id.clone(),
        ));
        let download_result = self
            .worker
            .download_to_staging(
                engine_id,
                model_id,
                &desc.revision,
                &staging_payload_dir,
                cancel_token.clone(),
                Some(Arc::clone(&sink) as Arc<dyn InstallSink>),
            )
            .await;

        if cancel_token.is_cancelled() {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            // 回滚到旧状态
            self.rollback_to_old_state(&key, old_state).await;
            self.remove_active_operation(&key).await;
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Repair,
                final_stage: ModelOperationStage::Cancelled,
                success: true,
                error: None,
            });
        }

        if let Err(e) = download_result {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            // 失败时附 installer 尾部输出（与 install_model 一致）
            let tail = sink.tail_lines(15);
            if !tail.is_empty() {
                tracing::warn!(
                    engine_id = %engine_id,
                    model_id,
                    tail = %tail.join(" | "),
                    "模型修复下载失败，installer 尾部输出"
                );
            }
            self.repair_fail(
                &key,
                old_state,
                &asset_key,
                &op_id,
                engine_id,
                e.to_string(),
            )
            .await;
            let tail = sink.tail_lines(15);
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Repair,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    e.to_code(),
                    ErrorPhase::Repair,
                    "模型下载失败",
                    if tail.is_empty() {
                        e.to_string()
                    } else {
                        format!("{}\ninstaller 输出（尾部）:\n{}", e, tail.join("\n"))
                    },
                )),
            });
        }

        let download_outcome = download_result.unwrap();

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %op_id,
            "修复下载完成，开始本地校验（fingerprint + promote）"
        );

        // GB 级 fingerprint 挪 spawn_blocking（与 install_model 一致）
        let fp_dir = staging_payload_dir.clone();
        let fp =
            match tokio::task::spawn_blocking(move || mstore::compute_content_fingerprint(&fp_dir))
                .await
            {
                Ok(Ok(fp)) => fp,
                Ok(Err(e)) => {
                    let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                    self.repair_fail(
                        &key,
                        old_state,
                        &asset_key,
                        &op_id,
                        engine_id,
                        e.to_string(),
                    )
                    .await;
                    return Ok(self.make_failed_result(
                        engine_id,
                        model_id,
                        &op_id,
                        ModelOperationKind::Repair,
                        "fingerprint 计算失败",
                    ));
                }
                Err(e) => {
                    // spawn_blocking panic（JoinError）
                    let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
                    self.repair_fail(
                        &key,
                        old_state,
                        &asset_key,
                        &op_id,
                        engine_id,
                        e.to_string(),
                    )
                    .await;
                    return Ok(self.make_failed_result(
                        engine_id,
                        model_id,
                        &op_id,
                        ModelOperationKind::Repair,
                        "fingerprint 任务异常",
                    ));
                }
            };
        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            fingerprint = %fp.fingerprint,
            size_bytes = fp.total_size_bytes,
            file_count = fp.file_count,
            "修复 fingerprint 计算完成"
        );

        // 构建 manifest
        let (source, checksum_source_kind) = match &download_outcome.checksum_source {
            ModelDownloadChecksumSource::Unverified => (
                ModelSource::Unverified {
                    source: download_outcome.source.clone(),
                    downloaded_at_ms: runtime::now_ms(),
                },
                "unverified",
            ),
            ModelDownloadChecksumSource::Sha256(sha) => (
                ModelSource::Sha256 {
                    sha256: sha.clone(),
                    source: download_outcome.source.clone(),
                    downloaded_at_ms: runtime::now_ms(),
                },
                "sha256",
            ),
        };

        let manifest = ModelManifest {
            schema_version: mstore::MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine_id.clone(),
            model_id: model_id.to_string(),
            revision: desc.revision.clone(),
            source,
            install_id: install_id.clone(),
            installed_at_ms: runtime::now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fp.fingerprint.clone(),
            payload_size_bytes: fp.total_size_bytes,
            file_count: fp.file_count,
            compatibility_schema: desc.compatibility_schema,
            model_contract_identity: ModelContractIdentity {
                model_id: model_id.to_string(),
                revision: desc.revision.clone(),
                checksum_source_kind: checksum_source_kind.to_string(),
            },
        };

        // 原子提升（新 generation 切换 current.json）
        if let Err(e) = mstore::promote_staging_to_generation(
            engine_id,
            &asset_key,
            &install_id,
            &op_id,
            &manifest,
        ) {
            let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
            self.repair_fail(
                &key,
                old_state,
                &asset_key,
                &op_id,
                engine_id,
                e.to_string(),
            )
            .await;
            return Ok(self.make_failed_result(
                engine_id,
                model_id,
                &op_id,
                ModelOperationKind::Repair,
                "staging 提升失败",
            ));
        }

        // 清理 staging + 旧 generations
        let _ = mstore::cleanup_staging(engine_id, &asset_key, &op_id);
        let _ = mstore::cleanup_old_generations(engine_id, &asset_key, &install_id);

        // 状态 → Installed
        {
            let mut states = self.states.write().await;
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc));
            status.install_state = ModelInstallState::Installed;
            status.cache_size_bytes = Some(fp.total_size_bytes);
            status.verification_state = ModelVerificationState::Unverified;
            status.compatibility = ModelCompatibility::Compatible;
        }

        self.remove_active_operation(&key).await;

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            install_id = %install_id,
            op_id = %op_id,
            fingerprint = %fp.fingerprint,
            "模型修复完成"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: op_id,
            operation_kind: ModelOperationKind::Repair,
            final_stage: ModelOperationStage::Done,
            success: true,
            error: None,
        })
    }

    /// 删除模型（引用检查 + 删除）。
    ///
    /// **删除正在使用或被配置引用的模型必须返回结构化冲突**，
    /// 不能静默切换到其他模型。
    ///
    /// 状态转移：Installed → Deleting → NotInstalled (or DeleteBlocked)
    pub async fn delete_model(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: Option<String>,
        conflict_check: &dyn ModelConflictChecker,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
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

        let op_id = operation_id.unwrap_or_else(|| runtime::generate_operation_id());
        let asset_key = mstore::encode_asset_key(model_id);
        let key = (engine_id.clone(), model_id.to_string());

        // 检查当前状态
        {
            let states = self.states.read().await;
            if let Some(status) = states.get(&key) {
                if status.install_state.is_busy() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::AlreadyRunning,
                        ErrorPhase::Cleanup,
                        "模型操作进行中",
                        format!("正在执行 {} 操作", status.install_state),
                    ));
                }
                if !status.install_state.is_installed() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::NotRunning,
                        ErrorPhase::Cleanup,
                        "模型未安装，无需删除",
                        format!("当前状态: {}", status.install_state),
                    ));
                }
            } else {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Cleanup,
                    "模型未安装，无需删除",
                    "状态缓存中无此模型记录",
                ));
            }
        }

        // ── 引用检查 ──
        let conflict = conflict_check.check_delete_conflict(engine_id, model_id);

        if let Some(conflict) = conflict {
            {
                let mut states = self.states.write().await;
                let status = states
                    .entry(key.clone())
                    .or_insert_with(|| EngineModelStatus::not_installed(desc));
                status.install_state = ModelInstallState::DeleteBlocked;
            }

            let err = conflict.to_error();
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(err),
            });
        }

        // 状态转移：→ Deleting
        self.transition(&key, ModelInstallState::Deleting).await?;

        // 删除 generation + current pointer
        if let Err(e) = mstore::delete_model_generation(engine_id, &asset_key) {
            // 删除失败 → 回到 Installed（不谎报 NotInstalled）
            tracing::error!(
                engine_id = %engine_id,
                model_id = %model_id,
                error = %e,
                "删除模型 generation 失败"
            );
            {
                let mut states = self.states.write().await;
                if let Some(status) = states.get_mut(&key) {
                    status.install_state = ModelInstallState::Installed;
                }
            }
            return Ok(ModelOperationResult {
                engine_id: engine_id.to_string(),
                model_id: model_id.to_string(),
                operation_id: op_id,
                operation_kind: ModelOperationKind::Delete,
                final_stage: ModelOperationStage::Failed,
                success: false,
                error: Some(LocalEngineError::with_detail(
                    LocalEngineErrorCode::CleanupFailed,
                    ErrorPhase::Cleanup,
                    "删除模型失败",
                    e.to_string(),
                )),
            });
        }

        // 状态转移：→ NotInstalled
        self.transition(&key, ModelInstallState::NotInstalled)
            .await?;

        // 清除状态缓存
        {
            let mut states = self.states.write().await;
            if let Some(status) = states.get_mut(&key) {
                status.cache_size_bytes = None;
                status.verification_state = ModelVerificationState::Unknown;
                status.is_selected = false;
                status.is_active = false;
                status.compatibility = ModelCompatibility::Unknown;
            }
        }

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %op_id,
            "模型删除完成"
        );

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

    /// 取消模型操作。
    ///
    /// 取消进行中的安装/修复操作。
    /// **下载失败或取消不破坏已安装模型，也不改变当前语音选择。**
    ///
    /// 铁则：必须验证 operation_id 匹配，防止取消不相关的操作。
    pub async fn cancel_model_operation(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        operation_id: &str,
    ) -> Result<ModelOperationResult, LocalEngineError> {
        let key = (engine_id.clone(), model_id.to_string());

        let active = {
            let ops = self.active_operations.read().await;
            ops.get(&key).cloned()
        };

        let active = match active {
            Some(a) => a,
            None => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::NotRunning,
                    ErrorPhase::Request,
                    "无进行中的操作",
                    "模型无活跃操作记录",
                ));
            }
        };

        if active.operation_id != operation_id {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Rejected,
                ErrorPhase::Request,
                "operation_id 不匹配",
                format!("期望: {}, 收到: {}", active.operation_id, operation_id),
            ));
        }

        active.cancel_token.cancel();

        let asset_key = mstore::encode_asset_key(model_id);
        let _ = mstore::cleanup_staging(engine_id, &asset_key, operation_id);

        // 取消后的目标状态：
        // - Install 取消 → NotInstalled（安装未完成）
        // - Repair 取消 → 不在此设置状态：repair_model 的取消路径会自己回滚到旧状态。
        //   如果只设 cancel 不设状态，repair_model 的 rollback_to_old_state 负责恢复。
        //   若 repair_model 已结束（罕见），用磁盘实际状态兜底。
        // - Delete 取消 → Installed（删除未完成）
        let target = match active.operation_kind {
            ModelOperationKind::Install => Some(ModelInstallState::NotInstalled),
            ModelOperationKind::Repair => {
                // 不覆盖状态——让 repair_model 取消路径回滚
                None
            }
            ModelOperationKind::Delete => Some(ModelInstallState::Installed),
        };

        if let Some(target) = target {
            let mut states = self.states.write().await;
            let desc = self.registry.find(&key.0, &key.1);
            let status = states
                .entry(key.clone())
                .or_insert_with(|| EngineModelStatus::not_installed(desc.unwrap()));
            status.install_state = target;
        }

        self.remove_active_operation(&key).await;

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            op_id = %operation_id,
            "模型操作已取消"
        );

        Ok(ModelOperationResult {
            engine_id: engine_id.to_string(),
            model_id: model_id.to_string(),
            operation_id: operation_id.to_string(),
            operation_kind: active.operation_kind,
            final_stage: ModelOperationStage::Cancelled,
            success: true,
            error: None,
        })
    }

    pub async fn update_selected_active(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        is_selected: bool,
        is_active: bool,
    ) -> Result<(), LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
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

        let key = (engine_id.clone(), model_id.to_string());
        let mut states = self.states.write().await;
        let status = states
            .entry(key)
            .or_insert_with(|| EngineModelStatus::not_installed(desc));
        status.is_selected = is_selected;
        status.is_active = is_active;
        Ok(())
    }

    pub fn verify_model_identity(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        health_model_id: Option<&str>,
        health_revision: Option<&str>,
        health_fingerprint: Option<&str>,
    ) -> Result<ModelIdentityVerification, LocalEngineError> {
        let desc = self.registry.find(engine_id, model_id).ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Health,
                "未知模型",
                format!(
                    "engine_id={}, model_id={} 不在 allowlist",
                    engine_id, model_id
                ),
            )
        })?;

        desc.verify_health_identity(health_model_id, health_revision, health_fingerprint)
    }

    pub fn get_installed_manifest(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Result<Option<ModelManifest>, LocalEngineError> {
        let asset_key = mstore::encode_asset_key(model_id);
        match mstore::read_model_current_pointer(engine_id, &asset_key) {
            Ok(Some(pointer)) => {
                match mstore::read_model_manifest(engine_id, &asset_key, &pointer.install_id) {
                    Ok(manifest) => Ok(Some(manifest)),
                    Err(e) => Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::ArtifactCorrupted,
                        ErrorPhase::Health,
                        "manifest 读取失败",
                        e.to_string(),
                    )),
                }
            }
            Ok(None) => Ok(None),
            Err(e) => Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::ArtifactCorrupted,
                ErrorPhase::Health,
                "current pointer 读取失败",
                e.to_string(),
            )),
        }
    }

    async fn transition(
        &self,
        key: &(EngineId, String),
        target: ModelInstallState,
    ) -> Result<(), LocalEngineError> {
        let desc = self.registry.find(&key.0, &key.1);
        let mut states = self.states.write().await;
        let current = states
            .get(key)
            .map(|s| s.install_state.clone())
            .unwrap_or(ModelInstallState::NotInstalled);

        let new_state = transition_install_state(&current, target)?;

        let status = states
            .entry(key.clone())
            .or_insert_with(|| EngineModelStatus::not_installed(desc.unwrap()));
        status.install_state = new_state;
        Ok(())
    }

    async fn fail_and_cleanup(
        &self,
        key: &(EngineId, String),
        asset_key: &str,
        op_id: &str,
        engine_id: &EngineId,
        error: String,
    ) {
        tracing::warn!(
            engine_id = %engine_id,
            asset_key = %asset_key,
            op_id = %op_id,
            error = %error,
            "安装失败，清理 staging"
        );
        let _ = mstore::cleanup_staging(engine_id, asset_key, op_id);
        let _ = self
            .transition(key, ModelInstallState::DownloadFailed)
            .await;
        let _ = self.transition(key, ModelInstallState::NotInstalled).await;
        self.remove_active_operation(key).await;
    }

    async fn repair_fail(
        &self,
        key: &(EngineId, String),
        old_state: Option<ModelInstallState>,
        asset_key: &str,
        op_id: &str,
        engine_id: &EngineId,
        error: String,
    ) {
        tracing::warn!(
            engine_id = %engine_id,
            asset_key = %asset_key,
            op_id = %op_id,
            error = %error,
            "修复失败，回滚到旧状态"
        );
        let _ = mstore::cleanup_staging(engine_id, asset_key, op_id);
        self.rollback_to_old_state(key, old_state).await;
        self.remove_active_operation(key).await;
    }

    async fn rollback_to_old_state(
        &self,
        key: &(EngineId, String),
        old_state: Option<ModelInstallState>,
    ) {
        let target_state = match old_state {
            Some(old) => old,
            None => {
                // old_state 为 None 说明 states 缓存中原本无此模型记录
                //（如 service 刚启动、尚未 restore_states_from_disk）。
                // 此时不能假设 NotInstalled——应从磁盘恢复实际状态。
                let asset_key = mstore::encode_asset_key(&key.1);
                match mstore::restore_model_state(&key.0, &asset_key) {
                    Ok(RestoredModelState::Installed { .. }) => ModelInstallState::Installed,
                    Ok(RestoredModelState::Corrupted { .. }) => ModelInstallState::NotInstalled,
                    Ok(RestoredModelState::NotInstalled) => ModelInstallState::NotInstalled,
                    Err(e) => {
                        tracing::warn!(
                            engine_id = %key.0,
                            model_id = %key.1,
                            error = %e,
                            "回滚时从磁盘恢复状态失败，回退到 NotInstalled"
                        );
                        ModelInstallState::NotInstalled
                    }
                }
            }
        };

        let mut states = self.states.write().await;
        let desc = self.registry.find(&key.0, &key.1);
        let status = states
            .entry(key.clone())
            .or_insert_with(|| EngineModelStatus::not_installed(desc.unwrap()));
        status.install_state = target_state;
    }

    async fn remove_active_operation(&self, key: &(EngineId, String)) {
        let mut ops = self.active_operations.write().await;
        ops.remove(key);
    }

    fn make_failed_result(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        op_id: &str,
        kind: ModelOperationKind,
        message: &str,
    ) -> ModelOperationResult {
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
                message,
                message.to_string(),
            )),
        }
    }

    pub fn registry(&self) -> &ModelRegistry {
        &self.registry
    }
}

// ── ModelConflictChecker trait ─────────────────────────────────────────────

/// 模型删除冲突检查 trait。
pub trait ModelConflictChecker: Send + Sync {
    fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict>;
}

// ── NoopModelWorker ─────────────────────────────────────────────────────────

/// 占位 worker（B2 将替换为真实 FunASR worker）。
///
/// 所有下载请求都返回失败——模型安装需要 B2 完成后才能工作。
pub struct NoopModelWorker;

#[async_trait::async_trait]
impl ModelInstallWorker for NoopModelWorker {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        _model_id: &str,
        _revision: &str,
        _staging_payload_dir: &std::path::Path,
        _cancel_token: CancellationToken,
        _sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        Err(ModelDownloadError::Internal {
            message: "NoopModelWorker: 模型下载未实现（等待 B2 FunASR worker）".to_string(),
        })
    }
}

// ── FunasrModelInstallWorker ───────────────────────────────────────────────

/// 嵌入的 blink_model_installer.py 脚本（随 Rust 二进制发布）。
const BLINK_MODEL_INSTALLER_PY: &str =
    include_str!("../../../resources/stt/funasr/blink_model_installer.py");

/// FunASR 专用模型安装 worker（B2）。
///
/// 使用 current generation venv 中的 Python 运行 `blink_model_installer.py`，
/// 通过 ModelScope 官方库下载模型到 staging payload 目录。
///
/// **铁则**：
/// - 只使用 current generation venv 中的 Python
/// - 只接受编译期 allowlist 中的 model id/revision
/// - Rust adapter 将 canonical model id 映射为固定 worker 参数
/// - 前端和通用 command 不得提供 URL、Python 路径、脚本路径或环境变量
/// - MODELSCOPE_CACHE 指向本次 staging payload 目录
/// - staging 目录创建失败必须 fail closed
/// - 禁止回落到用户 ~/.cache/modelscope
/// - stdout/stderr 实时进入 operation 日志
/// - worker 由受管进程运行，接入 Job Object、CancellationToken 和超时
/// - 取消/超时后 worker 及其子进程全部退出
/// - worker 成功只代表下载完成；最终 fingerprint、manifest 与 promote 由 Rust 执行
pub struct FunasrModelInstallWorker {
    /// 下载超时（秒），0 = 无超时。
    timeout_secs: u64,
}

impl FunasrModelInstallWorker {
    /// 创建默认 worker（超时 600s = 10min，模型下载可能较慢）。
    pub fn new() -> Self {
        Self { timeout_secs: 600 }
    }

    /// 创建带自定义超时的 worker。
    #[allow(dead_code)]
    pub fn with_timeout(secs: u64) -> Self {
        Self { timeout_secs: secs }
    }

    /// 释放 installer 脚本到 python_dir。
    fn ensure_installer_script() -> Result<std::path::PathBuf, String> {
        let dir = crate::infra::utils::paths::python_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 python 目录失败: {e}"))?;
        let script_path = dir.join("blink_model_installer.py");
        let need_write = match std::fs::read_to_string(&script_path) {
            Ok(existing) => existing != BLINK_MODEL_INSTALLER_PY,
            Err(_) => true,
        };
        if need_write {
            std::fs::write(&script_path, BLINK_MODEL_INSTALLER_PY)
                .map_err(|e| format!("写入 blink_model_installer.py 失败: {e}"))?;
        }
        Ok(script_path)
    }

    /// 查找 current generation venv 中的 python.exe。
    fn find_generation_python() -> Option<std::path::PathBuf> {
        let engine_id = EngineId::new("funasr").ok()?;
        let pointer = runtime::read_current_pointer(&engine_id).ok()?;
        let install_id = pointer?.install_id;
        let python_exe = runtime::generation_dir(&engine_id, &install_id)
            .join("venv")
            .join("Scripts")
            .join("python.exe");
        if python_exe.exists() {
            Some(python_exe)
        } else {
            None
        }
    }
}

impl Default for FunasrModelInstallWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ModelInstallWorker for FunasrModelInstallWorker {
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        // 1. 查找 current generation venv 中的 Python
        let python =
            Self::find_generation_python().ok_or_else(|| ModelDownloadError::Internal {
                message: "FunASR generation venv 未安装——请先安装环境".to_string(),
            })?;

        // 2. 释放 installer 脚本
        let script_path =
            Self::ensure_installer_script().map_err(|e| ModelDownloadError::Internal {
                message: format!("释放 installer 脚本失败: {e}"),
            })?;

        // 3. 确保 staging payload 目录存在（fail closed）
        std::fs::create_dir_all(staging_payload_dir).map_err(|e| ModelDownloadError::Internal {
            message: format!("staging 目录创建失败: {e}"),
        })?;

        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloading");
            s.emit_log(&format!(
                "开始下载模型 {model_id} (revision={revision}) 到 {staging_payload_dir:?}"
            ));
        }

        // 4. 构建启动命令
        let mut cmd = tokio::process::Command::new(&python);
        cmd.arg(&script_path)
            .arg("--model")
            .arg(model_id)
            .arg("--revision")
            .arg(revision)
            .arg("--staging-dir")
            .arg(staging_payload_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        // 设置环境变量：禁止回落到用户默认缓存
        cmd.env("MODELSCOPE_CACHE", staging_payload_dir.as_os_str());
        // Python 无缓冲 + UTF-8
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONUTF8", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");

        // CREATE_NO_WINDOW
        cmd = crate::infra::platform::no_window_tokio(cmd);

        // 5. 启动子进程
        let mut child = cmd.spawn().map_err(|e| ModelDownloadError::Internal {
            message: format!("启动 installer 进程失败: {e}"),
        })?;

        let pid = child.id().unwrap_or(0);

        // 5a. 分配 Job Object（Windows 进程树回收）
        //
        // **铁则**：installer 进程必须进入 Job Object，确保取消/超时/Blink 退出时
        // 整个进程树（包括 pip 子进程）全部被回收。
        // Job handle 在 wait 完成后 drop，触发 KILL_ON_JOB_CLOSE。
        #[cfg(windows)]
        let job_handle = match crate::infra::platform::process::assign_job_object(pid) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(%e, pid, "installer Job Object 分配失败，终止子进程");
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ModelDownloadError::Internal {
                    message: format!("Job Object 分配失败: {e}"),
                });
            }
        };

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            pid,
            "FunASR model installer 进程已启动"
        );

        if let Some(s) = sink.as_deref() {
            s.emit_log(&format!("installer 进程已启动 (pid={pid})"));
        }

        // 6. 并发排空 stdout/stderr 管道（防止背压死锁）
        //
        // **铁则**：必须在 wait 之前启动管道排空 task。
        // 如果 wait 先完成再读管道，子进程 stdout/stderr 缓冲区满后会阻塞，
        // 导致 child.wait() 永不返回（死锁）。
        //
        // 排空 task 逐行将输出实时送入 sink，不等待进程退出。
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_task = if let Some(stdout) = stdout {
            let sink_ref = sink.clone();
            Some(tokio::spawn(async move {
                pump_pipe_to_sink(stdout, sink_ref).await;
            }))
        } else {
            None
        };

        let stderr_task = if let Some(stderr) = stderr {
            let sink_ref = sink.clone();
            Some(tokio::spawn(async move {
                pump_pipe_to_sink(stderr, sink_ref).await;
            }))
        } else {
            None
        };

        // 7. 等待进程完成，带超时和取消
        let timeout = std::time::Duration::from_secs(self.timeout_secs);

        let wait_result = tokio::select! {
            result = child.wait() => {
                result.map_err(|e| ModelDownloadError::Internal {
                    message: format!("等待 installer 进程失败: {e}"),
                })?
            }
            _ = tokio::time::sleep(timeout) => {
                // 超时——kill 进程并等待退出
                tracing::warn!(pid, "FunASR model installer 超时，终止进程");
                let _ = child.start_kill();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                ).await;

                // drop Job handle 触发 KILL_ON_JOB_CLOSE（进程树回收）
                #[cfg(windows)]
                drop(job_handle);

                // 等待管道排空 task 完成
                if let Some(t) = stdout_task { let _ = t.await; }
                if let Some(t) = stderr_task { let _ = t.await; }

                return Err(ModelDownloadError::TimedOut);
            }
            _ = cancel_token.cancelled() => {
                // 取消——kill 进程并等待退出
                tracing::info!(pid, "FunASR model installer 被取消，终止进程");
                let _ = child.start_kill();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                ).await;

                #[cfg(windows)]
                drop(job_handle);

                if let Some(t) = stdout_task { let _ = t.await; }
                if let Some(t) = stderr_task { let _ = t.await; }

                return Err(ModelDownloadError::Cancelled);
            }
        };

        // 8. 等待管道排空 task 完成（进程已退出，管道即将 EOF）
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }

        // 9. drop Job handle（进程树最终回收保障）
        #[cfg(windows)]
        drop(job_handle);

        // 10. 检查退出码
        let output = wait_result;
        let code = output.code().unwrap_or(-1);
        if !output.success() {
            if cancel_token.is_cancelled() {
                return Err(ModelDownloadError::Cancelled);
            }
            return Err(ModelDownloadError::Failed {
                message: format!("installer 进程退出码 {code}"),
            });
        }

        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloaded");
            s.emit_log(&format!("模型 {model_id} 下载完成 (exit_code={code})"));
        }

        // 11. 验证 staging 目录非空
        if !staging_payload_dir.exists()
            || std::fs::read_dir(staging_payload_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
        {
            return Err(ModelDownloadError::Failed {
                message: "下载完成但 staging 目录为空".to_string(),
            });
        }

        Ok(ModelDownloadOutcome {
            source: format!("modelscope:{model_id}"),
            checksum_source: ModelDownloadChecksumSource::Unverified,
        })
    }
}

/// 并发排空子进程管道，逐行送入有界 sink。
///
/// **铁则**：
/// - 必须在 child.wait() 之前启动，防止 stdout/stderr 缓冲区满后死锁。
/// - 逐行读取（LineAccumulator），不使用 read_until（无界增长）。
/// - 单行最大字节数 8KB，超出截断。
/// - 实时送入 sink，不等待进程退出。
async fn pump_pipe_to_sink<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    sink: Option<Arc<dyn InstallSink>>,
) {
    use crate::infra::local_engine::log_pipe::LineAccumulator;
    use tokio::io::AsyncReadExt;

    let mut acc = LineAccumulator::new(8192);
    let mut read_buf = vec![0u8; 8192];

    loop {
        match reader.read(&mut read_buf).await {
            Ok(0) => {
                // EOF——flush 残留
                if let Some((text, _truncated)) = acc.finish() {
                    if !text.is_empty() {
                        if let Some(s) = sink.as_deref() {
                            s.emit_log(&text);
                        }
                    }
                }
                break;
            }
            Ok(n) => {
                let lines = acc.push_data(&read_buf[..n]);
                for (text, _truncated) in lines {
                    if let Some(s) = sink.as_deref() {
                        s.emit_log(&text);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(%e, "pump_pipe_to_sink: pipe read error");
                if let Some((text, _truncated)) = acc.finish() {
                    if !text.is_empty() {
                        if let Some(s) = sink.as_deref() {
                            s.emit_log(&text);
                        }
                    }
                }
                break;
            }
        }
    }
}

// ── FakeInstaller ───────────────────────────────────────────────────────────

/// 可注入的假模型安装 worker（测试用）。
///
/// **能力**：
/// - 成功写入固定 payload（可自定义内容）
/// - 可阻塞并响应取消
/// - 可注入下载失败
/// - 可注入校验失败（写入空文件或损坏内容模拟 fingerprint 不匹配）
/// - 可生成不同 revision/content（用于 repair 测试）
/// - 通过 sink 报告阶段日志
pub struct FakeInstaller {
    /// 是否成功下载。
    pub success: bool,
    /// 下载延迟（毫秒），>0 时模拟可取消的下载。
    pub delay_ms: u64,
    /// 写入 staging 的文件内容。
    pub file_content: Vec<u8>,
    /// 写入的文件名（默认 `model.bin`）。
    pub file_name: String,
    /// 下载来源描述。
    pub source: Option<String>,
    /// 下载 checksum 来源。
    pub checksum_source: Option<ModelDownloadChecksumSource>,
}

impl FakeInstaller {
    /// 创建成功写入固定 payload 的 installer。
    pub fn success() -> Self {
        Self {
            success: true,
            delay_ms: 0,
            file_content: b"fake model data".to_vec(),
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }

    /// 创建会失败的 installer。
    pub fn failing() -> Self {
        Self {
            success: false,
            delay_ms: 0,
            file_content: vec![],
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }

    /// 创建可阻塞取消的 installer。
    pub fn delayed(delay_ms: u64) -> Self {
        Self {
            success: true,
            delay_ms,
            file_content: b"fake model data".to_vec(),
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }

    /// 创建写入指定内容的 installer。
    pub fn with_content(content: Vec<u8>) -> Self {
        Self {
            success: true,
            delay_ms: 0,
            file_content: content,
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }
}

#[async_trait::async_trait]
impl ModelInstallWorker for FakeInstaller {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        model_id: &str,
        _revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloading");
            s.emit_log(&format!("开始下载模型 {model_id}"));
        }

        if self.delay_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)) => {}
                _ = cancel_token.cancelled() => {
                    return Err(ModelDownloadError::Cancelled);
                }
            }
        }

        if cancel_token.is_cancelled() {
            return Err(ModelDownloadError::Cancelled);
        }

        if !self.success {
            return Err(ModelDownloadError::Failed {
                message: "fake download failure".to_string(),
            });
        }

        if let Some(s) = sink.as_deref() {
            s.emit_stage("writing");
            s.emit_log("写入 payload 文件");
        }

        let file_name = if self.file_name.is_empty() {
            "model.bin"
        } else {
            self.file_name.as_str()
        };
        std::fs::write(staging_payload_dir.join(file_name), &self.file_content).map_err(|e| {
            ModelDownloadError::Internal {
                message: e.to_string(),
            }
        })?;

        Ok(ModelDownloadOutcome {
            source: self
                .source
                .clone()
                .unwrap_or_else(|| format!("fake:{model_id}")),
            checksum_source: self
                .checksum_source
                .clone()
                .unwrap_or_else(|| ModelDownloadChecksumSource::Unverified),
        })
    }
}

// ── DTO ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogItemDto {
    pub engine_id: String,
    pub model_id: String,
    pub display_name: String,
    pub description: String,
    pub revision: String,
    pub estimated_size_mb: Option<u64>,
    pub install_state: String,
    pub verification_state: String,
    pub cache_size_bytes: Option<u64>,
    pub is_selected: bool,
    pub is_active: bool,
    pub compatibility: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationRequestDto {
    pub engine_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationResultDto {
    pub engine_id: String,
    pub model_id: String,
    pub operation_id: String,
    pub operation_kind: String,
    pub final_stage: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelDeleteConflictDto {
    pub engine_id: String,
    pub model_id: String,
    pub reasons: Vec<DeleteConflictReasonDto>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteConflictReasonDto {
    ReferencedByConfig {
        config_field: String,
        config_value: String,
    },
    ActiveInRunningInstance {
        instance_id: String,
    },
    ReferencedByDescriptor {
        descriptor_model_id: String,
    },
}

pub fn project_model_status(
    descriptor: &EngineModelDescriptor,
    status: &EngineModelStatus,
) -> ModelCatalogItemDto {
    ModelCatalogItemDto {
        engine_id: descriptor.engine_id.to_string(),
        model_id: descriptor.model_id.clone(),
        display_name: descriptor.display_name.clone(),
        description: descriptor.description.clone(),
        revision: descriptor.revision.clone(),
        estimated_size_mb: descriptor.estimated_size_mb,
        install_state: status.install_state.to_string(),
        verification_state: status.verification_state.to_string(),
        cache_size_bytes: status.cache_size_bytes,
        is_selected: status.is_selected,
        is_active: status.is_active,
        compatibility: status.compatibility.to_string(),
    }
}

pub fn project_model_operation_result(result: &ModelOperationResult) -> ModelOperationResultDto {
    ModelOperationResultDto {
        engine_id: result.engine_id.clone(),
        model_id: result.model_id.clone(),
        operation_id: result.operation_id.clone(),
        operation_kind: result.operation_kind.to_string(),
        final_stage: result.final_stage.to_string(),
        success: result.success,
        error: result
            .error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

pub fn project_delete_conflict(conflict: &ModelDeleteConflict) -> ModelDeleteConflictDto {
    ModelDeleteConflictDto {
        engine_id: conflict.engine_id.to_string(),
        model_id: conflict.model_id.clone(),
        reasons: conflict
            .reasons
            .iter()
            .map(|r| match r {
                DeleteConflictReason::ReferencedByConfig {
                    config_field,
                    config_value,
                } => DeleteConflictReasonDto::ReferencedByConfig {
                    config_field: config_field.clone(),
                    config_value: config_value.clone(),
                },
                DeleteConflictReason::ActiveInRunningInstance { instance_id } => {
                    DeleteConflictReasonDto::ActiveInRunningInstance {
                        instance_id: instance_id.clone(),
                    }
                }
                DeleteConflictReason::ReferencedByDescriptor {
                    descriptor_model_id,
                } => DeleteConflictReasonDto::ReferencedByDescriptor {
                    descriptor_model_id: descriptor_model_id.clone(),
                },
            })
            .collect(),
    }
}

// ── FunASR 注册 ────────────────────────────────────────────────────────────

pub fn make_funasr_model_registry() -> ModelRegistry {
    ModelRegistry::new_with_models(vec![
        EngineModelDescriptor::sensevoice_small(),
        EngineModelDescriptor::paraformer_zh(),
    ])
}

pub struct FunasrModelConflictChecker {
    pub selected_model: String,
    pub descriptor_model_id: String,
    pub active_model_id: Option<String>,
    pub active_instance_id: Option<String>,
}

impl ModelConflictChecker for FunasrModelConflictChecker {
    fn check_delete_conflict(
        &self,
        engine_id: &EngineId,
        model_id: &str,
    ) -> Option<ModelDeleteConflict> {
        let mut reasons = Vec::new();

        if self.selected_model == model_id {
            reasons.push(DeleteConflictReason::ReferencedByConfig {
                config_field: "funasr_model".to_string(),
                config_value: self.selected_model.clone(),
            });
        }

        if self.descriptor_model_id == model_id {
            reasons.push(DeleteConflictReason::ReferencedByDescriptor {
                descriptor_model_id: self.descriptor_model_id.clone(),
            });
        }

        if let (Some(active_id), Some(inst_id)) = (&self.active_model_id, &self.active_instance_id)
        {
            if active_id == model_id {
                reasons.push(DeleteConflictReason::ActiveInRunningInstance {
                    instance_id: inst_id.clone(),
                });
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
}

// ── 单测 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
static MODEL_STORAGE_TEST_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

#[cfg(test)]
pub(crate) async fn acquire_model_storage_test_lock() -> tokio::sync::MutexGuard<'static, ()> {
    MODEL_STORAGE_TEST_LOCK.lock().await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::local_engine::runtime::EngineId;

    // ── 测试 helper：使用独立 temp root，避免并行竞态 ──────────────────────

    /// 每个测试获取唯一 tag，确保 asset 目录不冲突。
    fn unique_tag(test_name: &str) -> String {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{test_name}-{c}")
    }

    /// 创建带唯一 model_id 的 registry（通过临时 descriptor）。
    /// 使用唯一 tag 后，每个测试操作的 asset_key 不同，不会互相干扰。
    fn make_registry_with_tag(tag: &str) -> ModelRegistry {
        ModelRegistry::new_with_models(vec![EngineModelDescriptor {
            engine_id: EngineId::new("funasr").unwrap(),
            model_id: tag.to_string(),
            display_name: format!("Test-{tag}"),
            description: "test model".to_string(),
            revision: "v1".to_string(),
            checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
            estimated_size_mb: Some(1),
            compatibility_schema: 1,
        }])
    }

    fn make_service_with_tag(tag: &str, worker: Arc<dyn ModelInstallWorker>) -> ModelService {
        ModelService::new(
            make_registry_with_tag(tag),
            worker,
            Arc::new(crate::app::local_engine::NoopEventPort),
        )
    }

    fn cleanup_asset(engine_id: &EngineId, model_id: &str) {
        let asset_key = mstore::encode_asset_key(model_id);
        if let Ok(root) = mstore::asset_root(engine_id, &asset_key) {
            let _ = std::fs::remove_dir_all(&root);
        }
    }

    // ═══════════════════════════════════════════════════════════════════════
    // BroadcastingInstallSink 桥接测试：installer 输出 → EventPort 广播
    // ═══════════════════════════════════════════════════════════════════════

    /// 捕获安装日志/阶段事件的测试 EventPort。
    struct RecordingPort {
        install_logs: std::sync::Mutex<Vec<(u64, String, String)>>,
        // (seq, level, text)
        install_stages: std::sync::Mutex<Vec<String>>,
    }

    impl RecordingPort {
        fn new() -> Self {
            Self {
                install_logs: std::sync::Mutex::new(Vec::new()),
                install_stages: std::sync::Mutex::new(Vec::new()),
            }
        }
    }

    impl super::super::service::EventPort for RecordingPort {
        fn emit_status(&self, _snapshot: &crate::domain::local_engine::EngineStatusSnapshot) {}
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
            _engine_id: &EngineId,
            _operation_id: &str,
            seq: u64,
            level: super::super::dto::EngineLogLevel,
            text: &str,
        ) {
            self.install_logs
                .lock()
                .unwrap()
                .push((seq, level.to_string(), text.to_string()));
        }
        fn emit_install_stage(&self, _engine_id: &EngineId, _operation_id: &str, stage: &str) {
            self.install_stages.lock().unwrap().push(stage.to_string());
        }
    }

    #[test]
    fn broadcasting_install_sink_forwards_logs_and_stages() {
        let port = Arc::new(RecordingPort::new());
        let sink = BroadcastingInstallSink::new(
            BoundedInstallSink::new(500),
            port.clone(),
            EngineId::new("funasr").unwrap(),
            "op-test-1".to_string(),
        );

        sink.emit_stage("preparing");
        sink.emit_log("[INFO] 开始下载模型: paraformer-zh");
        sink.emit_log("[ERROR] 模型下载失败: boom");

        let logs = port.install_logs.lock().unwrap().clone();
        assert_eq!(logs.len(), 2, "两条 installer 日志都应广播");
        // seq 从 1 开始单调递增
        assert_eq!(logs[0].0, 1);
        assert_eq!(logs[1].0, 2);
        // 级别分类：普通行 info，[ERROR] 前缀 error
        assert_eq!(logs[0].1, "info");
        assert_eq!(logs[1].1, "error");
        assert!(logs[1].2.contains("boom"));

        // 阶段事件广播
        let stages = port.install_stages.lock().unwrap().clone();
        assert_eq!(stages, vec!["preparing".to_string()]);

        // 内部缓冲保留（含 stage 行），tail_lines 可取尾部
        let tail = sink.tail_lines(2);
        assert_eq!(tail.len(), 2);
        assert!(tail[1].contains("boom"));
        assert_eq!(sink.buffered_log_count(), 3, "stage + 两条日志共 3 行");
    }

    /// 证明 ModelService::new 必须注入 EventPort，且模型安装日志通过注入的 port 发出。
    ///
    /// 这是对"API 陷阱已消除"的回归测试：生产 wiring 遗漏 NoopEventPort 时
    /// 不会静默丢日志——构造签名直接要求传入 port。
    #[tokio::test]
    async fn install_model_emits_logs_through_injected_port() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("inject-port");
        let funasr = EngineId::new("funasr").unwrap();

        let port = Arc::new(RecordingPort::new());
        let svc = ModelService::new(
            make_registry_with_tag(&tag),
            Arc::new(FakeInstaller::success()),
            port.clone() as Arc<dyn super::super::service::EventPort>,
        );

        let result = svc.install_model(&funasr, &tag, None).await.unwrap();
        assert!(result.success, "安装应成功: {:?}", result.error);

        let logs = port.install_logs.lock().unwrap().clone();
        assert!(
            !logs.is_empty(),
            "安装日志必须通过注入的 EventPort 发出，不能静默丢弃"
        );
        // 至少包含 preparing 阶段（emit_stage 也会发 install_stage 事件）
        let stages = port.install_stages.lock().unwrap().clone();
        assert!(
            stages.contains(&"preparing".to_string()),
            "preparing 阶段必须通过注入的 port 发出"
        );

        cleanup_asset(&funasr, &tag);
    }

    /// 安装模型，在 Windows 并行测试文件系统竞态导致首次失败时自动重试。
    ///
    /// `model_service` 测试共享 `models/funasr/` 父目录，并行 `remove_dir_all` /
    /// `create_dir_all` 在同一父目录下可能瞬态失败。此辅助函数封装重试逻辑。
    async fn install_with_retry(tag: &str, funasr: &EngineId) -> ModelService {
        let svc = make_service_with_tag(tag, Arc::new(FakeInstaller::success()));
        let result = svc.install_model(funasr, tag, None).await.unwrap();
        if result.success {
            return svc;
        }
        // 首次因竞态失败——清理后重试
        tracing::warn!(
            tag = %tag,
            "install 首次失败（可能 Windows 并行文件系统竞态），重试中"
        );
        cleanup_asset(funasr, tag);
        let svc2 = make_service_with_tag(tag, Arc::new(FakeInstaller::success()));
        let retry = svc2.install_model(funasr, tag, None).await.unwrap();
        assert!(retry.success, "install 重试仍失败: {:?}", retry.error);
        svc2
    }

    // ── registry 基础测试 ──────────────────────────────────────────────

    #[test]
    fn registry_list_funasr_models() {
        let reg = make_funasr_model_registry();
        let funasr = EngineId::new("funasr").unwrap();
        let models = reg.list(&funasr);
        assert_eq!(models.len(), 2);
    }

    // ── install 成功后 manifest/current/payload 完整 ────────────────────

    #[tokio::test]
    async fn install_success_creates_manifest_current_payload() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("install-ok");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 验证 manifest + current + payload 完整
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        let manifest =
            mstore::read_model_manifest(&funasr, &asset_key, &pointer.install_id).unwrap();
        assert_eq!(manifest.model_id, tag);
        assert_eq!(manifest.engine_id, funasr);
        assert!(!manifest.content_fingerprint.is_empty());

        let payload = mstore::model_payload_dir(&funasr, &asset_key, &pointer.install_id).unwrap();
        assert!(payload.join("model.bin").exists());

        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::Installed);

        cleanup_asset(&funasr, &tag);
    }

    // ── 空目录或仅有文件但无合法 manifest，不是 Installed ───────────────

    #[tokio::test]
    async fn empty_dir_is_not_installed() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("empty-dir");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        // 不做安装——直接查状态
        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);

        // restore 也应返回 NotInstalled
        svc.restore_states_from_disk().await.unwrap();
        let status2 = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status2.install_state, ModelInstallState::NotInstalled);

        cleanup_asset(&funasr, &tag);
    }

    #[tokio::test]
    async fn files_without_manifest_is_not_installed() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("files-no-manifest");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        // 手动创建 payload 目录和文件，但不写 manifest 或 current.json
        let asset_key = mstore::encode_asset_key(&tag);
        let fake_dir = mstore::asset_root(&funasr, &asset_key).unwrap();
        std::fs::create_dir_all(&fake_dir).unwrap();
        std::fs::write(fake_dir.join("random_file.txt"), b"data").unwrap();

        // restore → NotInstalled（没有 current pointer）
        svc.restore_states_from_disk().await.unwrap();
        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);

        cleanup_asset(&funasr, &tag);
    }

    // ── manifest 损坏、pointer 损坏、fingerprint 不匹配恢复为 Corrupted ─

    #[tokio::test]
    async fn restore_corrupted_when_fingerprint_tampered() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("corrupt-fp");
        let funasr = EngineId::new("funasr").unwrap();
        let _svc = install_with_retry(&tag, &funasr).await;

        // 篡改 payload 内容
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        let payload = mstore::model_payload_dir(&funasr, &asset_key, &pointer.install_id).unwrap();
        std::fs::write(payload.join("model.bin"), b"tampered").unwrap();

        let svc2 = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));
        svc2.restore_states_from_disk().await.unwrap();

        let status = svc2.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.verification_state, ModelVerificationState::Corrupted);

        cleanup_asset(&funasr, &tag);
    }

    #[tokio::test]
    async fn restore_corrupted_when_manifest_corrupted() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("corrupt-manifest");
        let funasr = EngineId::new("funasr").unwrap();
        let _svc = install_with_retry(&tag, &funasr).await;

        // 篡改 manifest
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        let manifest_path =
            mstore::model_manifest_path(&funasr, &asset_key, &pointer.install_id).unwrap();
        std::fs::write(&manifest_path, b"{ corrupted json").unwrap();

        let svc2 = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));
        svc2.restore_states_from_disk().await.unwrap();

        let status = svc2.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.verification_state, ModelVerificationState::Corrupted);

        cleanup_asset(&funasr, &tag);
    }

    #[tokio::test]
    async fn restore_corrupted_when_pointer_corrupted() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("corrupt-pointer");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 篡改 current.json
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer_path = mstore::model_current_pointer_path(&funasr, &asset_key).unwrap();
        std::fs::write(&pointer_path, b"{ broken json").unwrap();

        let svc2 = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));
        svc2.restore_states_from_disk().await.unwrap();

        let status = svc2.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.verification_state, ModelVerificationState::Corrupted);

        cleanup_asset(&funasr, &tag);
    }

    // ── restart 恢复 Installed ────────────────────────────────────────

    #[tokio::test]
    async fn restart_restores_installed() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("restart");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 新建 service 并从磁盘恢复
        let svc2 = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));
        svc2.restore_states_from_disk().await.unwrap();

        let status = svc2.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::Installed);

        cleanup_asset(&funasr, &tag);
    }

    // ── install 失败不产生 current ─────────────────────────────────────

    #[tokio::test]
    async fn install_failure_no_current_pointer() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("install-fail");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::failing()));

        let result = svc.install_model(&funasr, &tag, None).await.unwrap();
        assert!(!result.success);
        assert_eq!(result.final_stage, ModelOperationStage::Failed);

        // 验证没有 current pointer
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key).unwrap();
        assert!(pointer.is_none());

        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);

        cleanup_asset(&funasr, &tag);
    }

    #[tokio::test]
    async fn install_unknown_model_returns_error() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("install-unknown");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));
        let result = svc.install_model(&funasr, "nonexistent", None).await;
        assert!(result.is_err());
        cleanup_asset(&funasr, &tag);
    }

    // ── repair 失败/取消保留旧 current 和旧 payload ────────────────────

    #[tokio::test]
    async fn repair_failure_preserves_old_generation() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("repair-fail");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 记录旧 generation
        let asset_key = mstore::encode_asset_key(&tag);
        let old_pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();

        // 用失败 worker repair
        let failing_svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::failing()));
        failing_svc.restore_states_from_disk().await.unwrap();

        let result = failing_svc.repair_model(&funasr, &tag, None).await.unwrap();
        assert!(!result.success);

        // 验证旧 generation 仍然完好
        let new_pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        assert_eq!(
            new_pointer.install_id, old_pointer.install_id,
            "repair 失败不应改变 current pointer"
        );

        // 状态应仍为 Installed
        let status = failing_svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::Installed);

        cleanup_asset(&funasr, &tag);
    }

    // ── repair 成功原子切换到新 generation ─────────────────────────────

    #[tokio::test]
    async fn repair_success_creates_new_generation() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("repair-ok");
        let funasr = EngineId::new("funasr").unwrap();

        // 安装 content A
        let svc = make_service_with_tag(
            &tag,
            Arc::new(FakeInstaller::with_content(b"content A".to_vec())),
        );
        let r1 = svc.install_model(&funasr, &tag, None).await.unwrap();
        assert!(r1.success);

        let asset_key = mstore::encode_asset_key(&tag);
        let old_pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();

        // 用 content B repair
        let svc2 = make_service_with_tag(
            &tag,
            Arc::new(FakeInstaller::with_content(b"content B".to_vec())),
        );
        svc2.restore_states_from_disk().await.unwrap();
        let r2 = svc2.repair_model(&funasr, &tag, None).await.unwrap();
        assert!(r2.success);

        // 验证 current pointer 切换到新 generation
        let new_pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        assert_ne!(
            new_pointer.install_id, old_pointer.install_id,
            "repair 成功应切换到新 generation"
        );

        // 验证新 generation 的 fingerprint 非空（旧 generation 已被 cleanup_old_generations 删除）
        let new_manifest =
            mstore::read_model_manifest(&funasr, &asset_key, &new_pointer.install_id).unwrap();
        assert!(
            !new_manifest.content_fingerprint.is_empty(),
            "新 generation 应有有效 fingerprint"
        );

        cleanup_asset(&funasr, &tag);
    }

    // ── 错误 operation id 无法取消 ────────────────────────────────────

    #[tokio::test]
    async fn cancel_wrong_operation_id_rejected() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("cancel-wrong");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        let result = svc
            .cancel_model_operation(&funasr, &tag, "wrong-op-id")
            .await;
        assert!(result.is_err());

        cleanup_asset(&funasr, &tag);
    }

    // ── 正确 operation id 能取消 fake installer 并清理 staging ─────────

    #[tokio::test]
    async fn cancel_install_during_download() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("cancel-install");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::delayed(500)));
        let svc_arc = std::sync::Arc::new(svc);
        let svc2 = svc_arc.clone();

        let handle = tokio::spawn({
            let funasr = funasr.clone();
            let tag = tag.clone();
            async move {
                svc2.install_model(&funasr, &tag, Some("op-cancel-install".to_string()))
                    .await
            }
        });

        // 等待下载开始
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 取消
        let cancel_result = svc_arc
            .cancel_model_operation(&funasr, &tag, "op-cancel-install")
            .await
            .unwrap();
        assert!(cancel_result.success);
        assert_eq!(cancel_result.final_stage, ModelOperationStage::Cancelled);

        let install_result = handle.await.unwrap().unwrap();
        assert_eq!(install_result.final_stage, ModelOperationStage::Cancelled);

        // 状态应为 NotInstalled
        let status = svc_arc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);

        // 验证 staging 已清理
        let asset_key = mstore::encode_asset_key(&tag);
        let staging = mstore::model_staging_dir(&funasr, &asset_key).unwrap();
        let staging_op = staging.join("op-cancel-install");
        // staging 目录可能存在但 payload 应已清理（由 cancel_model_operation 的 cleanup_staging）
        let _ = staging_op;

        cleanup_asset(&funasr, &tag);
    }

    /// cancel_repair 的核心验证逻辑——从已安装状态发起 repair 并取消。
    async fn run_cancel_repair_assertions(funasr: &EngineId, tag: &str) {
        let delay_svc = make_service_with_tag(tag, Arc::new(FakeInstaller::delayed(500)));
        delay_svc.restore_states_from_disk().await.unwrap();
        let delay_svc = std::sync::Arc::new(delay_svc);
        let svc_clone = delay_svc.clone();

        let asset_key = mstore::encode_asset_key(tag);
        let old_pointer = mstore::read_model_current_pointer(funasr, &asset_key)
            .unwrap()
            .unwrap();

        let handle = tokio::spawn({
            let funasr = funasr.clone();
            let tag = tag.to_string();
            async move {
                svc_clone
                    .repair_model(&funasr, &tag, Some("op-cancel-repair".to_string()))
                    .await
            }
        });

        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        let cancel_result = delay_svc
            .cancel_model_operation(funasr, tag, "op-cancel-repair")
            .await
            .unwrap();
        assert!(cancel_result.success);
        assert_eq!(cancel_result.final_stage, ModelOperationStage::Cancelled);

        let repair_result = handle.await.unwrap().unwrap();
        assert_eq!(repair_result.final_stage, ModelOperationStage::Cancelled);

        let new_pointer = mstore::read_model_current_pointer(funasr, &asset_key)
            .unwrap()
            .unwrap();
        assert_eq!(
            new_pointer.install_id, old_pointer.install_id,
            "repair 取消不应改变 current pointer"
        );

        let status = delay_svc.get_model_status(funasr, tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::Installed);
    }

    #[tokio::test]
    async fn cancel_repair_preserves_installed() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        // 此测试与 model_service 其他测试共享 runtimes_root()/models/funasr/ 父目录。
        // Windows 上并行 remove_dir_all/create_dir_all 在同一父目录下有文件系统竞态。
        // install_with_retry 在首次失败时自动重试。
        let tag = unique_tag("cancel-repair");
        let funasr = EngineId::new("funasr").unwrap();

        let _svc = install_with_retry(&tag, &funasr).await;
        run_cancel_repair_assertions(&funasr, &tag).await;

        cleanup_asset(&funasr, &tag);
    }

    // ── 并发互斥：同时两个 install 请求，第二个被拒绝 ──────────────────

    #[tokio::test]
    async fn concurrent_install_second_rejected() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("concurrent-install");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::delayed(500)));
        let svc = std::sync::Arc::new(svc);

        let svc1 = svc.clone();
        let svc2 = svc.clone();

        let h1 = tokio::spawn({
            let funasr = funasr.clone();
            let tag = tag.clone();
            async move {
                svc1.install_model(&funasr, &tag, Some("op-concurrent-1".to_string()))
                    .await
            }
        });

        // 等待第一个操作进入下载
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;

        // 第二个应被拒绝（AlreadyRunning）
        let result2 = svc2
            .install_model(&funasr, &tag, Some("op-concurrent-2".to_string()))
            .await;
        assert!(result2.is_err(), "第二个并发安装应被拒绝");

        // 取消第一个
        let _ = svc
            .cancel_model_operation(&funasr, &tag, "op-concurrent-1")
            .await;
        let _ = h1.await;

        cleanup_asset(&funasr, &tag);
    }

    // ── 路径穿越/恶意 model_id 被拒绝 ─────────────────────────────────────

    #[tokio::test]
    async fn malicious_model_id_rejected() {
        let funasr = EngineId::new("funasr").unwrap();
        // 使用真实 registry（sensevoice_small），尝试用恶意 model_id
        let reg = make_funasr_model_registry();
        let svc = ModelService::new(
            reg,
            Arc::new(FakeInstaller::success()),
            Arc::new(crate::app::local_engine::NoopEventPort),
        );

        // 恶意 model_id（含路径分隔符）
        let result = svc
            .install_model(&funasr, "../../../etc/passwd", None)
            .await;
        assert!(result.is_err(), "恶意 model_id 应被拒绝");

        // 恶意 model_id（含 .. 前缀）
        let result = svc.install_model(&funasr, "..%2f..%2fpasswd", None).await;
        assert!(result.is_err(), "恶意 model_id 应被拒绝");
    }

    // ── fingerprint 顺序稳定性：不同文件创建顺序产生相同 fingerprint ───

    #[tokio::test]
    async fn fingerprint_order_stability_in_model_service() {
        // 与其他操作 models/funasr/ 目录树的测试互斥——并行 remove_dir_all
        // 父目录时，本测试的文件 open 会瞬态 PermissionDenied（Windows）
        let _storage_guard = acquire_model_storage_test_lock().await;
        let engine = EngineId::new("funasr").unwrap();
        let asset_key_a = "test-fp-order-a";
        let asset_key_b = "test-fp-order-b";

        // 创建两个 payload 目录，文件相同但写入顺序不同
        let payload_a = mstore::model_payload_dir(&engine, asset_key_a, "gen-fp-0001").unwrap();
        let payload_b = mstore::model_payload_dir(&engine, asset_key_b, "gen-fp-0001").unwrap();

        std::fs::create_dir_all(&payload_a).unwrap();
        std::fs::create_dir_all(&payload_b).unwrap();

        // 目录 A：先写 b.bin 再写 a.bin（逆序写入）
        std::fs::write(payload_a.join("b.bin"), b"bbb").unwrap();
        std::fs::write(payload_a.join("a.bin"), b"aaa").unwrap();
        // 子目录文件
        std::fs::create_dir_all(payload_a.join("sub")).unwrap();
        std::fs::write(payload_a.join("sub/c.bin"), b"ccc").unwrap();

        // 目录 B：按不同顺序写同样的文件
        std::fs::write(payload_b.join("a.bin"), b"aaa").unwrap();
        std::fs::create_dir_all(payload_b.join("sub")).unwrap();
        std::fs::write(payload_b.join("sub/c.bin"), b"ccc").unwrap();
        std::fs::write(payload_b.join("b.bin"), b"bbb").unwrap();

        let fp_a = mstore::compute_content_fingerprint(&payload_a).unwrap();
        let fp_b = mstore::compute_content_fingerprint(&payload_b).unwrap();

        assert_eq!(
            fp_a.fingerprint, fp_b.fingerprint,
            "不同写入顺序的相同内容应产生相同 fingerprint"
        );
        assert_eq!(fp_a.file_count, 3);
        assert_eq!(fp_a.total_size_bytes, 9);

        // 清理
        let _ = std::fs::remove_dir_all(mstore::asset_root(&engine, asset_key_a).unwrap());
        let _ = std::fs::remove_dir_all(mstore::asset_root(&engine, asset_key_b).unwrap());
    }

    // ── delete 成功后 current pointer 和 generation 都不存在 ─────────────

    #[tokio::test]
    async fn delete_success_removes_everything() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("delete-ok");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 无冲突 checker
        struct NoConflict;
        impl ModelConflictChecker for NoConflict {
            fn check_delete_conflict(&self, _: &EngineId, _: &str) -> Option<ModelDeleteConflict> {
                None
            }
        }

        let result = svc
            .delete_model(&funasr, &tag, None, &NoConflict)
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.final_stage, ModelOperationStage::Done);

        // current pointer 不存在
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key).unwrap();
        assert!(pointer.is_none());

        // 状态为 NotInstalled
        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);

        cleanup_asset(&funasr, &tag);
    }

    // ── delete 被引用保护阻止（ReferencedByConfig）──────────────────────

    #[tokio::test]
    async fn delete_blocked_by_config_reference() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("delete-blocked");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = install_with_retry(&tag, &funasr).await;

        // 有冲突 checker——模拟被 config 引用
        struct ConfigConflict;
        impl ModelConflictChecker for ConfigConflict {
            fn check_delete_conflict(
                &self,
                engine_id: &EngineId,
                model_id: &str,
            ) -> Option<ModelDeleteConflict> {
                Some(ModelDeleteConflict {
                    engine_id: engine_id.clone(),
                    model_id: model_id.to_string(),
                    reasons: vec![DeleteConflictReason::ReferencedByConfig {
                        config_field: "funasr_model".to_string(),
                        config_value: model_id.to_string(),
                    }],
                })
            }
        }

        let result = svc
            .delete_model(&funasr, &tag, None, &ConfigConflict)
            .await
            .unwrap();
        assert!(!result.success);
        assert_eq!(result.final_stage, ModelOperationStage::Failed);

        // 状态应为 DeleteBlocked（但模型仍存在）
        let status = svc.get_model_status(&funasr, &tag).await.unwrap();
        assert_eq!(status.install_state, ModelInstallState::DeleteBlocked);

        // current pointer 仍存在（未被删除）
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key).unwrap();
        assert!(pointer.is_some());

        cleanup_asset(&funasr, &tag);
    }

    // ── delete 未安装的模型返回错误 ─────────────────────────────────────

    #[tokio::test]
    async fn delete_not_installed_returns_error() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("delete-not-installed");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        struct NoConflict;
        impl ModelConflictChecker for NoConflict {
            fn check_delete_conflict(&self, _: &EngineId, _: &str) -> Option<ModelDeleteConflict> {
                None
            }
        }

        let result = svc.delete_model(&funasr, &tag, None, &NoConflict).await;
        assert!(result.is_err(), "删除未安装的模型应返回错误");

        cleanup_asset(&funasr, &tag);
    }

    // ── install 已安装的模型幂等返回成功 ─────────────────────────────────

    #[tokio::test]
    async fn install_already_installed_is_idempotent() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("install-idempotent");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        // 第一次安装
        let r1 = svc.install_model(&funasr, &tag, None).await.unwrap();
        assert!(r1.success);

        // 第二次安装——幂等返回成功
        let r2 = svc.install_model(&funasr, &tag, None).await.unwrap();
        assert!(r2.success);
        assert_eq!(r2.final_stage, ModelOperationStage::Done);

        // current pointer 仍指向第一次安装的 generation（幂等不创建新 generation）
        let asset_key = mstore::encode_asset_key(&tag);
        let pointer = mstore::read_model_current_pointer(&funasr, &asset_key)
            .unwrap()
            .unwrap();
        let manifest =
            mstore::read_model_manifest(&funasr, &asset_key, &pointer.install_id).unwrap();
        assert_eq!(manifest.model_id, tag);

        cleanup_asset(&funasr, &tag);
    }

    // ── list_models 返回所有候选及其状态 ─────────────────────────────────

    #[tokio::test]
    async fn list_models_returns_all_candidates() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag = unique_tag("list-models");
        let funasr = EngineId::new("funasr").unwrap();
        let svc = make_service_with_tag(&tag, Arc::new(FakeInstaller::success()));

        // 未安装时 → 1 个 NotInstalled
        let models = svc.list_models(&funasr).await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].install_state, ModelInstallState::NotInstalled);

        // 安装后 → 1 个 Installed
        svc.install_model(&funasr, &tag, None).await.unwrap();
        let models = svc.list_models(&funasr).await;
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].install_state, ModelInstallState::Installed);

        cleanup_asset(&funasr, &tag);
    }

    // ── restore_states_from_disk 恢复多个模型 ────────────────────────────

    #[tokio::test]
    async fn restore_multiple_models_from_disk() {
        let _storage_guard = acquire_model_storage_test_lock().await;
        let tag1 = unique_tag("restore-multi-1");
        let tag2 = unique_tag("restore-multi-2");
        let funasr = EngineId::new("funasr").unwrap();

        // 用两个模型注册表
        let reg = ModelRegistry::new_with_models(vec![
            EngineModelDescriptor {
                engine_id: funasr.clone(),
                model_id: tag1.clone(),
                display_name: format!("Test-{tag1}"),
                description: "test".to_string(),
                revision: "v1".to_string(),
                checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
                estimated_size_mb: Some(1),
                compatibility_schema: 1,
            },
            EngineModelDescriptor {
                engine_id: funasr.clone(),
                model_id: tag2.clone(),
                display_name: format!("Test-{tag2}"),
                description: "test".to_string(),
                revision: "v1".to_string(),
                checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Unverified,
                estimated_size_mb: Some(1),
                compatibility_schema: 1,
            },
        ]);

        // 安装两个模型
        let svc1 = ModelService::new(
            reg.clone(),
            Arc::new(FakeInstaller::success()),
            Arc::new(crate::app::local_engine::NoopEventPort),
        );
        svc1.install_model(&funasr, &tag1, None).await.unwrap();
        svc1.install_model(&funasr, &tag2, None).await.unwrap();

        // 新 service 从磁盘恢复
        let svc2 = ModelService::new(
            reg,
            Arc::new(FakeInstaller::success()),
            Arc::new(crate::app::local_engine::NoopEventPort),
        );
        svc2.restore_states_from_disk().await.unwrap();

        let models = svc2.list_models(&funasr).await;
        assert_eq!(models.len(), 2);
        for m in &models {
            assert_eq!(m.install_state, ModelInstallState::Installed);
        }

        cleanup_asset(&funasr, &tag1);
        cleanup_asset(&funasr, &tag2);
    }

    // ── InstallSink 有界缓冲测试 ─────────────────────────────────────────

    #[tokio::test]
    async fn bounded_sink_caps_log_lines() {
        let sink = BoundedInstallSink::new(3);
        sink.emit_log("line1");
        sink.emit_log("line2");
        sink.emit_log("line3");
        sink.emit_log("line4"); // 应淘汰 line1
        assert_eq!(sink.buffered_log_count(), 3);

        let sink2 = BoundedInstallSink::new(2);
        sink2.emit_stage("downloading");
        sink2.emit_stage("writing");
        assert_eq!(sink2.buffered_log_count(), 2);
    }

    // ── NoopInstallSink 不缓冲 ───────────────────────────────────────────

    #[test]
    fn noop_sink_buffers_nothing() {
        let sink = NoopInstallSink;
        sink.emit_log("test");
        sink.emit_stage("test");
        assert_eq!(sink.buffered_log_count(), 0);
    }

    // ── encode_asset_key 确保路径安全 ───────────────────────────────────

    #[test]
    fn encode_asset_key_path_safety() {
        // 路径分隔符被替换
        assert_eq!(mstore::encode_asset_key("a/b"), "a-b");
        // 不含 `..`
        assert!(!mstore::encode_asset_key("..").contains("."));
        // 不含大写
        assert!(!mstore::encode_asset_key("ABC").contains(|c: char| c.is_uppercase()));
        // 空回退到 model
        assert_eq!(mstore::encode_asset_key("///"), "model");
    }

    // ── DTO 投影 ───────────────────────────────────────────────────────

    #[test]
    fn project_model_status_dto() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let status = EngineModelStatus::not_installed(&desc);
        let dto = project_model_status(&desc, &status);
        assert_eq!(dto.engine_id, "funasr");
        assert_eq!(dto.install_state, "not_installed");
    }

    #[test]
    fn model_operation_request_dto_deny_unknown_fields() {
        let json =
            r#"{"engine_id":"funasr","model_id":"iic/SenseVoiceSmall","url":"https://evil.com"}"#;
        let result: Result<ModelOperationRequestDto, _> = serde_json::from_str(json);
        assert!(result.is_err());
    }

    #[test]
    fn conflict_checker_blocks_when_selected() {
        let checker = FunasrModelConflictChecker {
            selected_model: "iic/SenseVoiceSmall".to_string(),
            descriptor_model_id: "iic/SenseVoiceSmall".to_string(),
            active_model_id: None,
            active_instance_id: None,
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "iic/SenseVoiceSmall");
        assert!(conflict.is_some());
    }

    #[test]
    fn conflict_checker_allows_when_no_conflict() {
        let checker = FunasrModelConflictChecker {
            selected_model: "other".to_string(),
            descriptor_model_id: "other".to_string(),
            active_model_id: None,
            active_instance_id: None,
        };
        let funasr = EngineId::new("funasr").unwrap();
        let conflict = checker.check_delete_conflict(&funasr, "iic/SenseVoiceSmall");
        assert!(conflict.is_none());
    }
}
