//! 本地引擎状态快照（0.22.3）。
//!
//! 三维观测：environment → service → model，不做线性"三级状态机"。
//! desired 与 observed state 正交，不能从进程存活推导模型 Ready。
//!
//! ## 状态版本语义
//!
//! - `service_epoch`：每次 `LocalEngineService` 实例随机生成。
//!   前端遇到新 epoch 必须重置旧快照，避免 Blink 重启后旧 revision 压住新状态。
//! - `revision`：仅在同一 `service_epoch` 内严格单调递增。
//!   新 epoch 与旧 epoch 不可直接按 revision 覆盖。
//!
//! ## operation_id 门控
//!
//! - 每个 engine id 同时只允许一个变更操作。
//! - 长操作携带 `operation_id`、阶段与 `cancellable`。
//! - 迟到操作（operation_id 不匹配当前）不能提交状态。

#[cfg(test)]
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::infra::local_engine::runtime::{
    BackendVerificationResult, ComputePreference, ResolvedProfile,
};

use super::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

// ── ServiceEpoch ──────────────────────────────────────────────────────────

/// 服务 epoch——每次 `LocalEngineService` 实例随机生成。
///
/// 前端遇到新 epoch 必须重置旧快照。
/// 新 epoch 与旧 epoch 不可直接按 revision 比较。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ServiceEpoch(pub u64);

impl ServiceEpoch {
    /// 随机生成新 epoch。
    pub fn new() -> Self {
        Self(generate_epoch_value())
    }
}

impl Default for ServiceEpoch {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for ServiceEpoch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "epoch-{:016x}", self.0)
    }
}

/// 生成随机 epoch 值（不引入 rand crate）。
fn generate_epoch_value() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let c = COUNTER.fetch_add(1, Ordering::Relaxed);
    let pid = std::process::id() as u64;
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64;
    // 混合 pid + counter + 时间戳，保证不同实例不同
    pid.rotate_left(8) ^ c.rotate_left(16) ^ now.rotate_left(24)
}

// ── DesiredState ──────────────────────────────────────────────────────────

/// 用户期望的引擎状态（与 observed state 正交）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DesiredState {
    /// 用户要求停止。
    Stopped,
    /// 用户要求运行。
    Running,
}

impl std::fmt::Display for DesiredState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("stopped"),
            Self::Running => f.write_str("running"),
        }
    }
}

// ── EngineOperation ─────────────────────────────────────────────────────────

/// 当前进行中的长操作（Idle 表示无操作）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineOperation {
    /// 操作种类。
    pub kind: OperationKind,
    /// 操作实例 id（Idle 时为空字符串）。
    pub operation_id: String,
    /// 操作当前阶段。
    pub stage: OperationStage,
    /// 是否可取消。
    pub cancellable: bool,
}

/// 操作种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationKind {
    /// 无操作。
    Idle,
    /// 安装。
    Installing,
    /// 更新。
    Updating,
    /// 修复。
    Repairing,
    /// 迁移。
    Migrating,
    /// 回滚。
    RollingBack,
    /// 清理。
    Cleaning,
}

impl std::fmt::Display for OperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Idle => f.write_str("idle"),
            Self::Installing => f.write_str("installing"),
            Self::Updating => f.write_str("updating"),
            Self::Repairing => f.write_str("repairing"),
            Self::Migrating => f.write_str("migrating"),
            Self::RollingBack => f.write_str("rolling_back"),
            Self::Cleaning => f.write_str("cleaning"),
        }
    }
}

/// 操作阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OperationStage {
    /// 等待开始。
    Pending,
    /// 准备环境。
    Preparing,
    /// 下载/安装中。
    Downloading,
    /// 验证中。
    Verifying,
    /// 提升中（staging → generation）。
    Promoting,
    /// 切换 current 指针。
    Switching,
    /// 首次启动验证。
    Validating,
    /// 已完成。
    Completed,
    /// 已取消。
    Cancelled,
    /// 已失败。
    Failed,
}

impl std::fmt::Display for OperationStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Pending => f.write_str("pending"),
            Self::Preparing => f.write_str("preparing"),
            Self::Downloading => f.write_str("downloading"),
            Self::Verifying => f.write_str("verifying"),
            Self::Promoting => f.write_str("promoting"),
            Self::Switching => f.write_str("switching"),
            Self::Validating => f.write_str("validating"),
            Self::Completed => f.write_str("completed"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

impl Default for EngineOperation {
    fn default() -> Self {
        Self {
            kind: OperationKind::Idle,
            operation_id: String::new(),
            stage: OperationStage::Pending,
            cancellable: false,
        }
    }
}

impl EngineOperation {
    /// 是否为活跃操作（非 Idle）。
    pub fn is_active(&self) -> bool {
        self.kind != OperationKind::Idle
    }

    /// 是否已结束（Completed / Cancelled / Failed）。
    pub fn is_finished(&self) -> bool {
        matches!(
            self.stage,
            OperationStage::Completed | OperationStage::Cancelled | OperationStage::Failed
        )
    }
}

// ── ProcessState ────────────────────────────────────────────────────────────

/// 进程观测状态（domain 层投影，复用 infra ProcessStatus 语义）。
///
/// **正交铁则**：process Running 不自动推出 service Healthy 或 model Ready。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProcessState {
    /// 进程未启动或已完全退出。
    Stopped,
    /// 正在启动。
    Starting,
    /// 进程运行中，附带 PID。
    Running { pid: u32 },
    /// 正在停止。
    Stopping,
    /// 进程已退出，附带退出原因描述。
    Exited { reason: String },
}

impl Default for ProcessState {
    fn default() -> Self {
        Self::Stopped
    }
}

impl std::fmt::Display for ProcessState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stopped => f.write_str("stopped"),
            Self::Starting => f.write_str("starting"),
            Self::Running { pid } => write!(f, "running(pid={pid})"),
            Self::Stopping => f.write_str("stopping"),
            Self::Exited { reason } => write!(f, "exited({reason})"),
        }
    }
}

// ── ServiceHealth / ModelHealth ─────────────────────────────────────────────

/// 服务健康观测（独立于进程状态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceHealth {
    /// 未知（尚未探测）。
    Unknown,
    /// 服务不可达。
    Unreachable,
    /// 服务健康。
    Healthy,
    /// 服务降级（部分功能受限）。
    Degraded,
}

impl Default for ServiceHealth {
    fn default() -> Self {
        Self::Unknown
    }
}

impl std::fmt::Display for ServiceHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::Unreachable => f.write_str("unreachable"),
            Self::Healthy => f.write_str("healthy"),
            Self::Degraded => f.write_str("degraded"),
        }
    }
}

/// 模型健康观测（独立于进程和服务状态）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelHealth {
    /// 未知（尚未探测）。
    Unknown,
    /// 模型未加载。
    NotLoaded,
    /// 模型下载中。
    Downloading,
    /// 模型加载中。
    Loading,
    /// 模型已就绪。
    Ready,
    /// 模型加载/运行失败。
    Failed,
}

impl Default for ModelHealth {
    fn default() -> Self {
        Self::Unknown
    }
}

impl std::fmt::Display for ModelHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::NotLoaded => f.write_str("not_loaded"),
            Self::Downloading => f.write_str("downloading"),
            Self::Loading => f.write_str("loading"),
            Self::Ready => f.write_str("ready"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

// ── EnvironmentHealth ──────────────────────────────────────────────────────

/// 环境观测状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentHealth {
    /// 环境未安装。
    Missing,
    /// 环境已就绪。
    Ready,
    /// 环境损坏。
    Broken,
    /// 需要重建。
    NeedsRebuild,
}

impl Default for EnvironmentHealth {
    fn default() -> Self {
        Self::Missing
    }
}

impl std::fmt::Display for EnvironmentHealth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Missing => f.write_str("missing"),
            Self::Ready => f.write_str("ready"),
            Self::Broken => f.write_str("broken"),
            Self::NeedsRebuild => f.write_str("needs_rebuild"),
        }
    }
}

// ── BackendInfo ────────────────────────────────────────────────────────────

/// 计算设备三层信息（§3.5）。
///
/// - requested preference：用户意图
/// - resolved profile：具体 artifact 与兼容合同
/// - actual backend：health 回报的实际后端
/// - fallback reason：auto 回退时记录每次失败原因
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendInfo {
    /// 用户请求的 compute preference。
    pub requested_preference: ComputePreference,
    /// 解析后的 profile（如果已解析）。
    pub resolved_profile: Option<ResolvedProfile>,
    /// backend 一致性校验结果（来自 infra runtime）。
    pub backend_verification: BackendVerificationResult,
    /// fallback 原因列表（auto 回退时记录每次失败）。
    pub fallback_reasons: Vec<FallbackEntry>,
}

impl Default for BackendInfo {
    fn default() -> Self {
        Self {
            requested_preference: ComputePreference::Auto,
            resolved_profile: None,
            backend_verification: BackendVerificationResult {
                state: crate::infra::local_engine::runtime::BackendState::Pending,
                expected_backend: crate::infra::local_engine::runtime::ComputeBackend::Cpu,
                actual_backend: None,
                device_name: None,
                mismatch_reason: None,
            },
            fallback_reasons: Vec::new(),
        }
    }
}

/// fallback 记录条目（domain 层投影，复用 infra FallbackReason 语义）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackEntry {
    /// 被拒绝的 profile id。
    pub rejected_profile: String,
    /// 拒绝原因分类。
    pub reason: String,
    /// 人类可读详情。
    pub detail: String,
}

// ── EngineStatus ───────────────────────────────────────────────────────────

/// 引擎状态快照（完整三维观测）。
///
/// desired 与 observed state 正交：
/// - `desired` 表达用户意图，不自动从进程/服务/模型状态推导。
/// - `process`、`service`、`model` 各自独立观测。
/// - env ready 不推出 server/model ready；child 存活也不推出端口上的服务属于该 child。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatus {
    /// 本次 `LocalEngineService` 实例的随机 epoch。
    pub service_epoch: ServiceEpoch,
    /// 用户期望状态（正交于 observed state）。
    pub desired: DesiredState,
    /// 当前长操作（Idle = 无操作）。
    pub operation: EngineOperation,
    /// 环境观测状态。
    pub environment: EnvironmentHealth,
    /// 进程观测状态。
    pub process: ProcessState,
    /// 服务健康观测。
    pub service: ServiceHealth,
    /// 模型健康观测。
    pub model: ModelHealth,
    /// revision（仅在同一 service_epoch 内严格单调递增）。
    pub revision: u64,
    /// 计算设备三层信息。
    pub backend: BackendInfo,
    /// 最近一次错误（如果有）。
    pub last_error: Option<LocalEngineError>,
}

impl Default for EngineStatus {
    fn default() -> Self {
        Self {
            service_epoch: ServiceEpoch::new(),
            desired: DesiredState::Stopped,
            operation: EngineOperation::default(),
            environment: EnvironmentHealth::Missing,
            process: ProcessState::Stopped,
            service: ServiceHealth::Unknown,
            model: ModelHealth::Unknown,
            revision: 0,
            backend: BackendInfo::default(),
            last_error: None,
        }
    }
}

impl EngineStatus {
    /// 创建新 epoch 的初始状态。
    /// revision 从 0 开始；旧 epoch 的 revision 不能压住新 epoch。
    pub fn new_epoch() -> Self {
        Self {
            service_epoch: ServiceEpoch::new(),
            ..Default::default()
        }
    }

    /// 判断是否可以从 desired Stopped 安全推出"用户不需要引擎运行"。
    ///
    /// 注意：这不是 observed state，只是用户意图。
    pub fn is_desired_stopped(&self) -> bool {
        self.desired == DesiredState::Stopped
    }

    /// 判断进程是否活跃（Starting/Running/Stopping）。
    pub fn is_process_active(&self) -> bool {
        matches!(
            self.process,
            ProcessState::Starting | ProcessState::Running { .. } | ProcessState::Stopping
        )
    }

    /// 判断模型是否就绪。
    ///
    /// **正交铁则**：此方法不检查 process 或 service 状态，
    /// 只看 model 维度本身。
    pub fn is_model_ready(&self) -> bool {
        self.model == ModelHealth::Ready
    }

    /// 判断服务是否可用（Healthy 或 Degraded）。
    pub fn is_service_available(&self) -> bool {
        matches!(
            self.service,
            ServiceHealth::Healthy | ServiceHealth::Degraded
        )
    }

    /// 判断引擎是否可用于业务请求。
    ///
    /// 需要 desired=Running 且 service 可用且 model 就绪。
    /// process Running 单独不足以推出可用——必须 service 和 model 都确认。
    pub fn is_available_for_requests(&self) -> bool {
        self.desired == DesiredState::Running
            && self.is_service_available()
            && self.is_model_ready()
    }
}

// ── StatusCommitGuard ──────────────────────────────────────────────────────

/// 状态提交守卫——验证 operation_id 和 epoch 匹配后才允许提交。
///
/// 迟到操作（operation_id 不匹配或 epoch 不匹配）不能提交状态。
#[derive(Debug, Clone)]
pub struct StatusCommitGuard {
    epoch: ServiceEpoch,
    current_operation_id: Option<String>,
    revision: u64,
}

impl StatusCommitGuard {
    /// 为当前状态创建提交守卫。
    pub fn for_status(status: &EngineStatus) -> Self {
        Self {
            epoch: status.service_epoch.clone(),
            current_operation_id: if status.operation.is_active() {
                Some(status.operation.operation_id.clone())
            } else {
                None
            },
            revision: status.revision,
        }
    }

    /// 检查提交是否被允许。
    ///
    /// 条件：
    /// 1. epoch 必须匹配（防跨 epoch 覆盖）
    /// 2. 如果有活跃操作，operation_id 必须匹配（防迟到操作覆盖）
    /// 3. 新 revision 必须严格大于当前 revision
    pub fn can_commit(
        &self,
        epoch: &ServiceEpoch,
        operation_id: Option<&str>,
        new_revision: u64,
    ) -> Result<(), LocalEngineError> {
        // 1. epoch 必须匹配
        if epoch != &self.epoch {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Rejected,
                ErrorPhase::Request,
                "状态已过期，请刷新",
                format!("epoch 不匹配: expected={}, got={}", self.epoch, epoch),
            ));
        }

        // 2. operation_id 门控
        if let Some(ref current_op) = self.current_operation_id {
            if let Some(submitted_op) = operation_id {
                if submitted_op != current_op.as_str() {
                    return Err(LocalEngineError::with_detail(
                        LocalEngineErrorCode::Rejected,
                        ErrorPhase::Request,
                        "操作已过期",
                        format!(
                            "operation_id 不匹配: expected={}, got={}",
                            current_op, submitted_op
                        ),
                    ));
                }
            } else {
                // 有活跃操作但提交未带 operation_id
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::Rejected,
                    ErrorPhase::Request,
                    "操作进行中，请等待",
                    "有活跃操作但提交未携带 operation_id".to_string(),
                ));
            }
        }

        // 3. revision 必须严格递增
        if new_revision <= self.revision {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Rejected,
                ErrorPhase::Request,
                "状态已过期",
                format!(
                    "revision 非递增: current={}, submitted={}",
                    self.revision, new_revision
                ),
            ));
        }

        Ok(())
    }
}

// ── FallbackTracker ─────────────────────────────────────────────────────────

/// fallback 语义追踪器。
///
/// 区分"显式 backend 失败"和"auto fallback"：
/// - 显式 backend（cpu/cuda/vulkan/directml）失败：返回可行动错误，不回退。
/// - auto fallback：按候选顺序回退，记录每次失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FallbackOutcome {
    /// 显式 backend 失败（不回退，返回错误）。
    ExplicitBackendFailed {
        preference: ComputePreference,
        reason: String,
    },
    /// auto fallback 成功解析到一个 profile。
    AutoFallbackResolved {
        rejected: Vec<FallbackEntry>,
        resolved: ResolvedProfile,
    },
    /// auto fallback 全部候选失败。
    AutoFallbackExhausted { rejected: Vec<FallbackEntry> },
}

impl std::fmt::Display for FallbackOutcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ExplicitBackendFailed { preference, reason } => {
                write!(f, "explicit {} failed: {}", preference, reason)
            }
            Self::AutoFallbackResolved { rejected, resolved } => {
                write!(
                    f,
                    "auto fallback resolved to {} ({} rejected)",
                    resolved.profile_id,
                    rejected.len()
                )
            }
            Self::AutoFallbackExhausted { rejected } => {
                write!(f, "auto fallback exhausted ({} rejected)", rejected.len())
            }
        }
    }
}

// ── EngineStatusSnapshot ───────────────────────────────────────────────────

/// 引擎状态快照（用于事件发布 / IPC 传输）。
///
/// 包含 engine_id、service_epoch、revision 和完整快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatusSnapshot {
    /// 引擎 id。
    pub engine_id: crate::infra::local_engine::runtime::EngineId,
    /// 服务 epoch。
    pub service_epoch: ServiceEpoch,
    /// revision。
    pub revision: u64,
    /// 完整状态快照。
    pub status: EngineStatus,
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── revision 严格递增 ──────────────────────────────────────────────────

    #[test]
    fn revision_strictly_increases() {
        let guard = StatusCommitGuard {
            epoch: ServiceEpoch(42),
            current_operation_id: None,
            revision: 5,
        };

        // revision 6 > 5，允许
        assert!(guard.can_commit(&ServiceEpoch(42), None, 6).is_ok());

        // revision 5 == 5，拒绝
        let err = guard.can_commit(&ServiceEpoch(42), None, 5).unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);

        // revision 4 < 5，拒绝
        let err = guard.can_commit(&ServiceEpoch(42), None, 4).unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
    }

    // ── 新 epoch 与旧 epoch 不可直接按 revision 覆盖 ─────────────────────

    #[test]
    fn new_epoch_rejects_old_epoch_commits() {
        let old_epoch = ServiceEpoch(42);
        let new_epoch = ServiceEpoch(99);
        let guard = StatusCommitGuard {
            epoch: old_epoch,
            current_operation_id: None,
            revision: 1000,
        };

        // 旧 epoch 的 revision 1001 > 1000，但 epoch 不匹配，拒绝
        let err = guard.can_commit(&new_epoch, None, 1001).unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
        assert!(err.detail.contains("epoch"));
    }

    // ── desired Running + process Starting 等组合可表达 ───────────────────

    #[test]
    fn desired_running_with_process_starting_is_expressible() {
        let status = EngineStatus {
            desired: DesiredState::Running,
            process: ProcessState::Starting,
            ..Default::default()
        };

        assert_eq!(status.desired, DesiredState::Running);
        assert_eq!(status.process, ProcessState::Starting);
        assert!(!status.is_available_for_requests()); // 尚未 ready
    }

    #[test]
    fn desired_stopped_with_process_running_is_expressible() {
        // 用户要求停止但进程尚在退出
        let status = EngineStatus {
            desired: DesiredState::Stopped,
            process: ProcessState::Stopping,
            ..Default::default()
        };

        assert_eq!(status.desired, DesiredState::Stopped);
        assert_eq!(status.process, ProcessState::Stopping);
        assert!(status.is_desired_stopped());
        assert!(!status.is_available_for_requests());
    }

    #[test]
    fn desired_running_with_process_stopped_is_expressible() {
        // 用户要求运行但进程尚未启动（例如安装完成后等待启动）
        let status = EngineStatus {
            desired: DesiredState::Running,
            process: ProcessState::Stopped,
            ..Default::default()
        };

        assert_eq!(status.desired, DesiredState::Running);
        assert_eq!(status.process, ProcessState::Stopped);
        assert!(!status.is_available_for_requests());
    }

    // ── process Running 不自动推出 service Healthy / model Ready ──────────

    #[test]
    fn process_running_does_not_imply_service_healthy() {
        let status = EngineStatus {
            process: ProcessState::Running { pid: 12345 },
            service: ServiceHealth::Unknown,
            model: ModelHealth::Unknown,
            ..Default::default()
        };

        // 进程在运行，但 service 和 model 都未知
        assert!(status.is_process_active());
        assert!(!status.is_service_available());
        assert!(!status.is_model_ready());
        assert!(!status.is_available_for_requests());
    }

    #[test]
    fn process_running_with_service_unreachable_does_not_imply_ready() {
        let status = EngineStatus {
            process: ProcessState::Running { pid: 42 },
            service: ServiceHealth::Unreachable,
            model: ModelHealth::NotLoaded,
            ..Default::default()
        };

        assert!(!status.is_available_for_requests());
    }

    #[test]
    fn env_ready_does_not_imply_service_or_model_ready() {
        let status = EngineStatus {
            environment: EnvironmentHealth::Ready,
            process: ProcessState::Stopped,
            service: ServiceHealth::Unknown,
            model: ModelHealth::Unknown,
            ..Default::default()
        };

        assert_eq!(status.environment, EnvironmentHealth::Ready);
        assert!(!status.is_service_available());
        assert!(!status.is_model_ready());
        assert!(!status.is_available_for_requests());
    }

    // ── operation_id 匹配门，迟到操作不能提交 ──────────────────────────────

    #[test]
    fn operation_id_gate_rejects_mismatched() {
        let guard = StatusCommitGuard {
            epoch: ServiceEpoch(1),
            current_operation_id: Some("op-current-001".to_string()),
            revision: 10,
        };

        // 匹配的 operation_id，允许
        assert!(
            guard
                .can_commit(&ServiceEpoch(1), Some("op-current-001"), 11)
                .is_ok()
        );

        // 不匹配的 operation_id，拒绝
        let err = guard
            .can_commit(&ServiceEpoch(1), Some("op-late-002"), 11)
            .unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
        assert!(err.detail.contains("operation_id"));
    }

    #[test]
    fn operation_id_gate_rejects_missing_id_when_active() {
        let guard = StatusCommitGuard {
            epoch: ServiceEpoch(1),
            current_operation_id: Some("op-active".to_string()),
            revision: 10,
        };

        // 有活跃操作但提交未带 operation_id
        let err = guard.can_commit(&ServiceEpoch(1), None, 11).unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Rejected);
    }

    #[test]
    fn operation_id_gate_allows_missing_id_when_idle() {
        let guard = StatusCommitGuard {
            epoch: ServiceEpoch(1),
            current_operation_id: None,
            revision: 10,
        };

        // Idle 时允许不带 operation_id
        assert!(guard.can_commit(&ServiceEpoch(1), None, 11).is_ok());
    }

    // ── 错误序列化字段稳定 ─────────────────────────────────────────────────

    #[test]
    fn error_serialization_roundtrip_preserves_all_fields() {
        let err = LocalEngineError::with_detail(
            LocalEngineErrorCode::PortConflict,
            ErrorPhase::Start,
            "端口 8000 被占用",
            "bind 127.0.0.1:8000 failed: Address already in use",
        );

        let json = serde_json::to_string(&err).unwrap();
        let back: LocalEngineError = serde_json::from_str(&json).unwrap();

        assert_eq!(back.code, LocalEngineErrorCode::PortConflict);
        assert_eq!(back.phase, ErrorPhase::Start);
        assert_eq!(back.action_hint, "端口 8000 被占用");
        assert_eq!(
            back.detail,
            "bind 127.0.0.1:8000 failed: Address already in use"
        );
    }

    // ── epoch 唯一性 ────────────────────────────────────────────────────────

    #[test]
    fn new_epochs_are_different() {
        let e1 = ServiceEpoch::new();
        std::thread::sleep(Duration::from_millis(2));
        let e2 = ServiceEpoch::new();
        // 高概率不同（混合了 pid + counter + 时间戳）
        // 注意：理论上可能碰撞，但实际不会
        assert_ne!(e1, e2);
    }

    // ── 显式 backend 失败与 auto fallback 语义不混淆 ───────────────────────

    #[test]
    fn explicit_backend_failure_vs_auto_fallback_semantics() {
        // 显式 backend 失败——不回退
        let explicit = FallbackOutcome::ExplicitBackendFailed {
            preference: ComputePreference::Cuda,
            reason: "no CUDA device found".to_string(),
        };
        assert!(matches!(
            explicit,
            FallbackOutcome::ExplicitBackendFailed { .. }
        ));

        // auto fallback 成功——记录拒绝项
        let auto = FallbackOutcome::AutoFallbackResolved {
            rejected: vec![FallbackEntry {
                rejected_profile: "cuda-sm86".to_string(),
                reason: "no_cuda_device".to_string(),
                detail: "NVIDIA driver not found".to_string(),
            }],
            resolved: ResolvedProfile {
                profile_id: "cpu-x64".to_string(),
                backend: crate::infra::local_engine::runtime::ComputeBackend::Cpu,
                artifact_id: crate::infra::local_engine::runtime::ArtifactId::new("python-3.12.8")
                    .unwrap(),
                priority: 1,
            },
        };
        assert!(matches!(auto, FallbackOutcome::AutoFallbackResolved { .. }));

        // 两者不能互相混淆
        assert!(!matches!(
            explicit,
            FallbackOutcome::AutoFallbackResolved { .. }
        ));
        assert!(!matches!(
            auto,
            FallbackOutcome::ExplicitBackendFailed { .. }
        ));
    }

    // ── 状态快照序列化 ─────────────────────────────────────────────────────

    #[test]
    fn status_snapshot_serialization_roundtrip() {
        let status = EngineStatus {
            service_epoch: ServiceEpoch(42),
            desired: DesiredState::Running,
            operation: EngineOperation {
                kind: OperationKind::Installing,
                operation_id: "op-test-001".to_string(),
                stage: OperationStage::Downloading,
                cancellable: true,
            },
            environment: EnvironmentHealth::Ready,
            process: ProcessState::Running { pid: 1234 },
            service: ServiceHealth::Healthy,
            model: ModelHealth::Ready,
            revision: 42,
            backend: BackendInfo::default(),
            last_error: None,
        };

        let json = serde_json::to_string(&status).unwrap();
        let back: EngineStatus = serde_json::from_str(&json).unwrap();

        assert_eq!(back.service_epoch, ServiceEpoch(42));
        assert_eq!(back.desired, DesiredState::Running);
        assert_eq!(back.operation.kind, OperationKind::Installing);
        assert_eq!(back.operation.operation_id, "op-test-001");
        assert_eq!(back.process, ProcessState::Running { pid: 1234 });
        assert_eq!(back.service, ServiceHealth::Healthy);
        assert_eq!(back.model, ModelHealth::Ready);
        assert_eq!(back.revision, 42);
    }

    // ── is_available_for_requests 需要三维都就绪 ───────────────────────────

    #[test]
    fn available_for_requests_requires_all_three_dimensions() {
        // 缺 service
        let s1 = EngineStatus {
            desired: DesiredState::Running,
            service: ServiceHealth::Unreachable,
            model: ModelHealth::Ready,
            process: ProcessState::Running { pid: 1 },
            ..Default::default()
        };
        assert!(!s1.is_available_for_requests());

        // 缺 model
        let s2 = EngineStatus {
            desired: DesiredState::Running,
            service: ServiceHealth::Healthy,
            model: ModelHealth::Loading,
            process: ProcessState::Running { pid: 2 },
            ..Default::default()
        };
        assert!(!s2.is_available_for_requests());

        // 缺 desired Running
        let s3 = EngineStatus {
            desired: DesiredState::Stopped,
            service: ServiceHealth::Healthy,
            model: ModelHealth::Ready,
            process: ProcessState::Running { pid: 3 },
            ..Default::default()
        };
        assert!(!s3.is_available_for_requests());

        // 三维都就绪
        let s4 = EngineStatus {
            desired: DesiredState::Running,
            service: ServiceHealth::Healthy,
            model: ModelHealth::Ready,
            process: ProcessState::Running { pid: 4 },
            ..Default::default()
        };
        assert!(s4.is_available_for_requests());
    }
}
