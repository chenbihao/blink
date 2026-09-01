//! 面向 UI 的本地引擎 DTO（0.22.5 H1）。
//!
//! 本模块定义设置页与前端之间的稳定 wire contract。
//! 设计原则：
//!
//! - **不暴露内部实现细节**：descriptor 的 artifact URL、executable、argv、
//!   environment variables、token、endpoint 身份信息、内部文件路径一律不投影。
//! - **service_epoch 字符串化**：JS 对 u64 精度不安全，epoch/revision 在
//!   通用 IPC/event 顶层投影为字符串。
//! - **status query 与 event 使用同一 DTO shape**：前端只维护一套解析器。
//! - **日志历史与实时事件使用同一 shape**：结构化 DTO 包含 instance_id + seq。
//!
//! ## 分层归属
//!
//! - 本模块在 `app` 层，从 domain `EngineDefinition` / `EngineStatus` 投影为 UI 契约。
//! - `domain`/`infra` 层不依赖本模块。

use serde::{Deserialize, Serialize};

use crate::domain::local_engine::{
    CapabilityKind, EngineDefinition, EngineStatus, EngineStatusSnapshot, LifecyclePolicy,
    ProcessState, ResourceBudget,
};
use crate::infra::local_engine::runtime::{ComputeBackend, ComputePreference, RuntimePlan};

// ── Catalog DTO ──────────────────────────────────────────────────────────────

/// 引擎目录项——前端 catalog 列表展示用。
///
/// 从 `EngineDefinition` 投影，只包含 UI 可见字段。
/// **不暴露** artifact URL、executable、argv、env、内部文件路径。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineCatalogItem {
    /// 稳定 engine id。
    pub engine_id: String,
    /// 显示名称。
    pub display_name: String,
    /// 显示描述。
    pub description: String,
    /// 显示图标标识。
    pub icon: String,
    /// 引擎版本。
    pub version: String,
    /// 能力种类（"stt" / "ocr"）。
    pub capability_kind: String,
    /// 运行时种类（"python_venv" / "managed_binary"）。
    pub runtime_kind: String,
    /// 生命周期策略（"manual" / "on_demand" / "auto"）。
    pub lifecycle: String,
    /// 模型 id。
    pub model_id: String,
    /// 模型 revision。
    pub model_revision: String,
    /// 资源预算摘要。
    pub resource_budget: ResourceBudgetDto,
    /// 允许展示的 compute options 列表（含兼容性判定）。
    pub compute_options: Vec<ComputeOptionDto>,
    /// 当前保存的 compute preference（字符串）。
    pub current_compute_preference: String,
}

/// 资源预算 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResourceBudgetDto {
    pub estimated_env_disk_mb: Option<u64>,
    pub estimated_model_disk_mb: Option<u64>,
    pub estimated_stable_ram_mb: Option<u64>,
    pub estimated_peak_ram_mb: Option<u64>,
}

/// compute option DTO——每个 descriptor 声明的候选 profile 投影。
///
/// `compatible` / `disabled_reason` 由 `ProviderDescriptor` 的兼容性检查
/// 真源决定，不由前端自行猜测。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ComputeOptionDto {
    /// 用户偏好（"auto" / "cpu" / "gpu_auto" / "cuda" / "vulkan" / "directml"）。
    pub preference: String,
    /// 对应的 profile 标识。
    pub profile_id: String,
    /// 对应的 compute backend。
    pub backend: String,
    /// 本机是否兼容。
    pub compatible: bool,
    /// 不兼容时的稳定原因（i18n key 或人类可读文案）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disabled_reason: Option<String>,
}

// ── Status DTO ───────────────────────────────────────────────────────────────

/// 引擎状态快照 DTO——status query 与 `LOCAL_ENGINE_STATUS` event 共用。
///
/// `service_epoch` 和 `revision` 均为字符串，避免 JS u64 精度问题。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatusDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 服务 epoch（字符串化，如 "epoch-0016a3f4..."）。
    pub service_epoch: String,
    /// revision（字符串化，单 epoch 内单调递增）。
    pub revision: String,
    /// 完整状态快照。
    pub status: EngineStatusWire,
}

/// `EngineStatus` 的 wire 序列化形式。
///
/// 直接复用 `EngineStatus` 的 serde derive——它已经实现了
/// `Serialize` / `Deserialize`，且 `service_epoch` 是 `ServiceEpoch`（Display 为字符串）。
/// 这里用包装结构确保 service_epoch + revision 在 DTO 顶层以字符串形式出现。
///
/// **`process` 使用显式 `ProcessStateDto`**，不再暴露 `serde_json::Value`，
/// 消除 unit variant（stopped/starting/stopping）序列化为字符串 vs data variant
///（running/exited）序列化为对象的前端歧义。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStatusWire {
    /// 用户期望状态。
    pub desired: String,
    /// 当前长操作。
    pub operation: serde_json::Value,
    /// 环境观测状态（部署状态：missing / ready / broken / needs_rebuild）。
    pub environment: String,
    /// 进程观测状态（显式 DTO，前端不猜测 serde enum shape）。
    pub process: ProcessStateDto,
    /// 服务健康观测。
    pub service: String,
    /// 模型健康观测。
    pub model: String,
    /// **可用性（0.22.6 phase B）**：desired=Running && service 可用 && model Ready。
    ///
    /// 由后端按三维正交状态推导（`EngineStatus::is_available_for_requests`），
    /// 前端不再自行从 process/service/model 猜测"能不能用"。
    pub available: bool,
    /// 计算设备三层信息（含 resolved profile / backend 校验 / fallback 记录）。
    pub backend: serde_json::Value,
    /// 最近一次错误（如果有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<serde_json::Value>,
}

/// 进程观测状态的显式 wire DTO。
///
/// 取代直接序列化 `ProcessState` enum——后者使用 `#[serde(rename_all = "snake_case")]`，
/// unit variants（Stopped/Starting/Stopping）序列化为裸字符串 `"stopped"`，
/// data variants（Running/Exited）序列化为对象 `{"running":{"pid":1234}}`，
/// 前端无法用统一逻辑消费，且 `"stopped" in process` 会抛 TypeError（字符串不是对象）。
///
/// 显式 DTO 统一为 `{ state, pid?, reason? }`，前端按 `state` 字段分支即可。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProcessStateDto {
    /// 进程状态：`stopped` / `starting` / `running` / `stopping` / `exited`。
    pub state: String,
    /// 进程 PID（仅 `running` 时存在）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 退出原因（仅 `exited` 时存在）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

// ── Log DTO ──────────────────────────────────────────────────────────────────

/// 闭合的日志级别枚举——app 协议层使用，serde lower-case wire 值。
///
/// 前端接收稳定的 `info` | `warn` | `error` | `debug` | `trace`。
/// 对未知外部输入（如第三方库的日志前缀）有明确 fallback 到 `debug`。
///
/// 不扩大到 domain 层——只在 app 协议层闭合。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EngineLogLevel {
    Info,
    Warn,
    Error,
    Debug,
    Trace,
}

impl EngineLogLevel {
    /// 从任意字符串解析为闭合枚举，未知值 fallback 到 `Debug`。
    pub fn from_str_lossy(s: &str) -> Self {
        match s {
            "info" => Self::Info,
            "warn" => Self::Warn,
            "error" => Self::Error,
            "trace" => Self::Trace,
            _ => Self::Debug,
        }
    }
}

impl std::fmt::Display for EngineLogLevel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Info => f.write_str("info"),
            Self::Warn => f.write_str("warn"),
            Self::Error => f.write_str("error"),
            Self::Debug => f.write_str("debug"),
            Self::Trace => f.write_str("trace"),
        }
    }
}

/// 结构化日志 DTO——历史查询与 `LOCAL_ENGINE_LOG` 实时事件共用。
///
/// 运行时日志使用 `instance_id` 隔离；安装时日志使用 `operation_id` 隔离。
/// `operation_id` 仅在安装/修复日志中存在，运行时日志为 `None`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineLogDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 实例 id（用于按 instance 隔离运行时日志）。
    ///
    /// 安装日志时此字段为空字符串。
    pub instance_id: String,
    /// 安装操作 id（用于按 operation 隔离安装日志）。
    ///
    /// 运行时日志时此字段为 `None`。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    /// 序号（单调递增，消费者用于去重/检测 gap）。
    pub seq: String,
    /// 时间戳（RFC 3339）。
    pub timestamp: String,
    /// 日志级别（闭合枚举，serde lower-case wire 值）。
    pub level: EngineLogLevel,
    /// 文本内容。
    pub text: String,
}

// ── 投影函数 ──────────────────────────────────────────────────────────────────

/// 从 `EngineDefinition` + 兼容性检查结果投影 catalog item。
///
/// `compute_options` 的 `compatible` / `disabled_reason` 由传入的
/// `compatibility_results` 决定——调用方须从 `ProviderDescriptor` 真源
/// 执行 `check_compatibility`，不由前端猜测。
pub fn project_catalog_item(
    descriptor: &EngineDefinition,
    compatibility_results: &[(ComputePreference, bool, Option<String>)],
    current_preference: ComputePreference,
) -> EngineCatalogItem {
    // 构建 compute options
    let compute_options: Vec<ComputeOptionDto> = descriptor
        .install_plan
        .compute_candidates
        .iter()
        .map(|c| {
            // 查找兼容性结果
            let (compatible, disabled_reason) = compatibility_results
                .iter()
                .find(|(pref, _, _)| *pref == c.preference)
                .map(|(_, compat, reason)| (*compat, reason.clone()))
                .unwrap_or((false, Some("未找到兼容性检查结果".to_string())));

            ComputeOptionDto {
                preference: preference_to_string(c.preference),
                profile_id: c.profile_id.clone(),
                backend: backend_to_string(map_preference_to_backend(c.preference)),
                compatible,
                disabled_reason,
            }
        })
        .collect();

    EngineCatalogItem {
        engine_id: descriptor.engine_id.to_string(),
        display_name: descriptor.display.name.clone(),
        description: descriptor.display.description.clone(),
        icon: descriptor.display.icon.clone(),
        version: descriptor.display.version.clone(),
        capability_kind: capability_kind_to_string(descriptor.capability_kind),
        runtime_kind: runtime_kind_to_string(descriptor.runtime_kind),
        lifecycle: lifecycle_to_string(descriptor.lifecycle),
        model_id: descriptor.model_contract.model_id.clone(),
        model_revision: descriptor.model_contract.revision.clone(),
        resource_budget: project_resource_budget(&descriptor.resource_budget),
        compute_options,
        current_compute_preference: preference_to_string(current_preference),
    }
}

/// 从 `EngineStatusSnapshot` 投影 status DTO。
///
/// service_epoch 和 revision 均字符串化。
pub fn project_status(snapshot: &EngineStatusSnapshot) -> EngineStatusDto {
    EngineStatusDto {
        engine_id: snapshot.engine_id.to_string(),
        service_epoch: snapshot.service_epoch.to_string(),
        revision: snapshot.revision.to_string(),
        status: project_status_wire(&snapshot.status),
    }
}

/// 从 `EngineStatus` 投影 wire 形式。
fn project_status_wire(status: &EngineStatus) -> EngineStatusWire {
    EngineStatusWire {
        desired: desired_to_string(status.desired),
        operation: serde_json::to_value(&status.operation).unwrap_or(serde_json::Value::Null),
        environment: environment_health_to_string(status.environment),
        process: project_process_state(&status.process),
        service: service_health_to_string(status.service),
        model: model_health_to_string(status.model.clone()),
        available: status.is_available_for_requests(),
        backend: serde_json::to_value(&status.backend).unwrap_or(serde_json::Value::Null),
        last_error: status
            .last_error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

// ── 枚举→字符串投影 ──────────────────────────────────────────────────────────

fn capability_kind_to_string(k: CapabilityKind) -> String {
    match k {
        CapabilityKind::Stt => "stt".to_string(),
        CapabilityKind::Ocr => "ocr".to_string(),
    }
}

fn runtime_kind_to_string(k: RuntimePlan) -> String {
    match k {
        RuntimePlan::PythonVenv => "python_venv".to_string(),
        RuntimePlan::ManagedBinary => "managed_binary".to_string(),
        RuntimePlan::OnnxRuntime => "onnx_runtime".to_string(),
    }
}

fn lifecycle_to_string(l: LifecyclePolicy) -> String {
    match l {
        LifecyclePolicy::Manual => "manual".to_string(),
        LifecyclePolicy::OnDemand => "on_demand".to_string(),
        LifecyclePolicy::KeepRunning => "keep_running".to_string(),
        LifecyclePolicy::StopAfterUse => "stop_after_use".to_string(),
    }
}

fn preference_to_string(p: ComputePreference) -> String {
    match p {
        ComputePreference::Auto => "auto".to_string(),
        ComputePreference::Cpu => "cpu".to_string(),
        ComputePreference::GpuAuto => "gpu_auto".to_string(),
        ComputePreference::Cuda => "cuda".to_string(),
        ComputePreference::Vulkan => "vulkan".to_string(),
        ComputePreference::Directml => "directml".to_string(),
    }
}

fn backend_to_string(b: ComputeBackend) -> String {
    match b {
        ComputeBackend::Cpu => "cpu".to_string(),
        ComputeBackend::Cuda => "cuda".to_string(),
        ComputeBackend::Vulkan => "vulkan".to_string(),
        ComputeBackend::Directml => "directml".to_string(),
    }
}

fn map_preference_to_backend(p: ComputePreference) -> ComputeBackend {
    match p {
        ComputePreference::Cpu => ComputeBackend::Cpu,
        ComputePreference::Cuda => ComputeBackend::Cuda,
        ComputePreference::Vulkan => ComputeBackend::Vulkan,
        ComputePreference::Directml => ComputeBackend::Directml,
        _ => ComputeBackend::Cpu,
    }
}

/// 从 domain `ProcessState` 投影为显式 wire DTO。
///
/// 消除 serde enum `snake_case` 序列化导致的 unit/data variant shape 不一致问题。
/// 前端只需按 `state` 字段分支，`pid` / `reason` 是可选字段。
pub fn project_process_state(process: &ProcessState) -> ProcessStateDto {
    match process {
        ProcessState::Stopped => ProcessStateDto {
            state: "stopped".to_string(),
            pid: None,
            reason: None,
        },
        ProcessState::Starting => ProcessStateDto {
            state: "starting".to_string(),
            pid: None,
            reason: None,
        },
        ProcessState::Running { pid } => ProcessStateDto {
            state: "running".to_string(),
            pid: Some(*pid),
            reason: None,
        },
        ProcessState::Stopping => ProcessStateDto {
            state: "stopping".to_string(),
            pid: None,
            reason: None,
        },
        ProcessState::Exited { reason } => ProcessStateDto {
            state: "exited".to_string(),
            pid: None,
            reason: Some(reason.clone()),
        },
    }
}

fn desired_to_string(d: crate::domain::local_engine::DesiredState) -> String {
    match d {
        crate::domain::local_engine::DesiredState::Stopped => "stopped".to_string(),
        crate::domain::local_engine::DesiredState::Running => "running".to_string(),
    }
}

pub fn environment_health_to_string(h: crate::domain::local_engine::EnvironmentHealth) -> String {
    match h {
        crate::domain::local_engine::EnvironmentHealth::Missing => "missing".to_string(),
        crate::domain::local_engine::EnvironmentHealth::Ready => "ready".to_string(),
        crate::domain::local_engine::EnvironmentHealth::Broken => "broken".to_string(),
        crate::domain::local_engine::EnvironmentHealth::NeedsRebuild => "needs_rebuild".to_string(),
    }
}

pub fn service_health_to_string(s: crate::domain::local_engine::ServiceHealth) -> String {
    match s {
        crate::domain::local_engine::ServiceHealth::Unknown => "unknown".to_string(),
        crate::domain::local_engine::ServiceHealth::Unreachable => "unreachable".to_string(),
        crate::domain::local_engine::ServiceHealth::Healthy => "healthy".to_string(),
        crate::domain::local_engine::ServiceHealth::Degraded => "degraded".to_string(),
    }
}

pub fn model_health_to_string(m: crate::domain::local_engine::ModelHealth) -> String {
    match m {
        crate::domain::local_engine::ModelHealth::Unknown => "unknown".to_string(),
        crate::domain::local_engine::ModelHealth::NotLoaded => "not_loaded".to_string(),
        crate::domain::local_engine::ModelHealth::Downloading => "downloading".to_string(),
        crate::domain::local_engine::ModelHealth::Loading => "loading".to_string(),
        crate::domain::local_engine::ModelHealth::Ready => "ready".to_string(),
        crate::domain::local_engine::ModelHealth::Failed => "failed".to_string(),
    }
}

fn project_resource_budget(b: &ResourceBudget) -> ResourceBudgetDto {
    ResourceBudgetDto {
        estimated_env_disk_mb: b.estimated_env_disk_mb,
        estimated_model_disk_mb: b.estimated_model_disk_mb,
        estimated_stable_ram_mb: b.estimated_stable_ram_mb,
        estimated_peak_ram_mb: b.estimated_peak_ram_mb,
    }
}

// ── Storage DTO（0.22.5 H2）──────────────────────────────────────────────────

/// 引擎存储概览——`get_local_engine_storage` 返回。
///
/// 列出所有可诊断/可清理的存储目标，前端据此展示预览和确认弹窗。
/// **不暴露**用户目录的完整路径——`path_display` 只在确有诊断价值时提供。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineStorageDto {
    /// 引擎 id（公共资产项可为空）。
    pub engine_id: Option<String>,
    /// 存储目标列表。
    pub targets: Vec<StorageTargetDto>,
    /// 总占用字节数。
    pub total_size_bytes: u64,
    /// 可释放总字节数（仅 removable 项之和）。
    pub releasable_size_bytes: u64,
}

/// 单个存储目标——前端展示为一行/一项。
///
/// `target_id` 是前端提交清理时的唯一标识。
/// 后端在执行清理时**重新解析** target_id，不信任前端提交的 path/size/shared/current。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageTargetDto {
    /// 稳定目标 id——前端用此 id 提交清理。
    ///
    /// 格式由后端定义，编码 scope + engine_id + 附加键（如 slot 或 artifact id）。
    /// 前端不解析此 id，只在 cleanup 请求中原样提交。
    pub target_id: String,
    /// 目标种类。
    pub kind: StorageTargetKindDto,
    /// 归属引擎 id（公共项可为空）。
    pub engine_id: Option<String>,
    /// 语义标签 key（前端 i18n 查找文案）。
    pub label_key: String,
    /// 人类可读的 fallback 标签（i18n 未命中时展示）。
    pub label_fallback: String,
    /// 占用字节数。
    pub size_bytes: u64,
    /// 是否为当前使用中的对象（当前环境，不可删除）。
    pub current: bool,
    /// 是否可清理。
    pub removable: bool,
    /// 是否为共享资产。
    pub shared: bool,
    /// 共享资产是否需要单独确认。
    pub requires_separate_confirmation: bool,
    /// 不可清理原因（blocked 时填）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocked_reason: Option<String>,
    /// 受影响的引擎 id 列表（公共资产用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub affected_engine_ids: Option<Vec<String>>,
    /// 引用计数（公共资产用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference_count: Option<u32>,
    /// 诊断用路径显示（仅确有诊断价值时提供，避免无条件暴露用户目录）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub path_display: Option<String>,
}

/// 存储目标种类（wire 字符串）。
///
/// 只表达用户可理解的对象类别，不暴露内部 slot/journal/residue/generation
/// 或 provider 类型名。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageTargetKindDto {
    /// 引擎环境（当前部署或其清理残留）。
    EngineEnvironment,
    /// 已安装模型资产（删除走模型管理，带引用检查）。
    InstalledModel,
    /// 引擎私有缓存（staging 残留、引擎自有缓存目录）。
    EngineCache,
    /// 共享托管运行时（跨引擎只读运行时，如 Blink 托管 Python）。
    SharedRuntime,
    /// 共享下载缓存（如 uv 下载缓存）。
    SharedDownloadCache,
    /// 旧版遗留资产。
    LegacyAsset,
}

// ── Cleanup 请求/结果 DTO ────────────────────────────────────────────────────

/// 清理请求 DTO——前端提交。
///
/// 前端提交 `engine_id` + `target_ids` + `operation_id`（可选）。
/// 后端重新解析 target_id，**不信任前端提交的路径、size、shared、current 标志**。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupRequestDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 要清理的目标 id 列表。
    pub target_ids: Vec<String>,
    /// 操作 id（用于取消关联）。
    pub operation_id: Option<String>,
}

/// 清理结果 DTO——后端返回。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupResultDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 操作 id。
    pub operation_id: String,
    /// 已清理的目标 id 列表。
    pub cleaned_target_ids: Vec<String>,
    /// 被跳过的目标 id 列表（如 active deployment、被引用的共享资产）。
    pub skipped_target_ids: Vec<String>,
    /// 已释放字节数。
    pub released_bytes: u64,
    /// deferred cleanup 目标 id 列表（被进程占用，已登记延迟清理）。
    pub deferred_target_ids: Vec<String>,
    /// 错误信息（如果有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

// ── Cancel 请求/结果 DTO ────────────────────────────────────────────────────

/// 取消操作结果 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelResultDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 被取消的操作 id。
    pub operation_id: String,
    /// 是否成功取消。
    pub cancelled: bool,
    /// 未取消原因（如操作已结束或不匹配）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// 环境变更操作（install/repair）结果 DTO——0.22.6 phase B。
///
/// **取消是正常终态**：`end_state="cancelled"` 不是错误——前端不应把
/// 该响应当失败处理。失败走 CommandError（保留结构化 code/phase/detail）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineOperationFinishedDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 本次操作的 id（与 status 事件中的 operation_id 对应）。
    pub operation_id: String,
    /// 操作终态：`completed` | `cancelled`。
    pub end_state: String,
}

// ── Orphan Stop 结果 DTO（0.22.6.6）──────────────────────────────────────────

/// 孤儿引擎停止结果 DTO——`stop_orphan_engine` command 返回。
///
/// 包含终止状态和诊断信息，不暴露 PID/路径等敏感字段。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanStopResultDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 是否成功终止孤儿进程。
    pub stopped: bool,
    /// 诊断原因（如 "lease_not_found"、"pid_not_exist"、"adoptable_killed"、
    /// "verification_failed" 等）。
    pub reason: String,
    /// 可读详情（不含敏感信息）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

// ── Engine Preferences DTO（0.22.5 H2）────────────────────────────────────────

/// 引擎受限偏好 DTO——`get_local_engine_preferences` 返回。
///
/// 只包含闭合字段，不暴露 executable/argv/env/path/url/runtime kind。
/// 字段按引擎支持情况填充：funasr 有 compute_preference + auto_start，
/// paddleocr 有 compute_preference + ocr_backend + lifecycle。
///
///
/// 0.22.6 H4：对应 Tauri command `get_local_engine_preferences` 已注册到 invoke_handler。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EnginePreferencesDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 计算设备偏好（"auto" / "cpu" / "cuda" 等，取决于引擎 descriptor 声明）。
    pub compute_preference: Option<String>,
    /// 自动启动（仅 FunASR 支持）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    /// OCR 路由后端（仅 PaddleOCR 支持）：windows / paddleocr / auto。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_backend: Option<String>,
    /// 生命周期策略（仅 PaddleOCR / descriptor 允许的引擎支持）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// 保存偏好后是否需要重建环境（profile 变化时为 true）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_rebuild: Option<bool>,
}

/// 引擎偏好 patch DTO——`set_local_engine_preferences` 接收。
///
/// **闭合字段**：只接受 `compute_preference`、`auto_start`、`ocr_backend`、`lifecycle`。
/// 禁止包含 executable/argv/env/path/url/runtime kind 或任意 engine_config。
/// 未知字段在反序列化时被拒绝（`#[serde(deny_unknown_fields)]`）。
///
///
/// 0.22.6 H4：对应 Tauri command `set_local_engine_preferences` 已注册到 invoke_handler。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EnginePreferencesPatchDto {
    /// 计算设备偏好（可选；不提供则不修改）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compute_preference: Option<String>,
    /// 自动启动（可选；仅 FunASR 支持；不提供则不修改）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub auto_start: Option<bool>,
    /// OCR 路由后端（可选；仅 PaddleOCR 支持；不提供则不修改）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ocr_backend: Option<String>,
    /// 生命周期策略（可选；仅 PaddleOCR / descriptor 允许的引擎支持；不提供则不修改）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
}

// ── 诊断 DTO ─────────────────────────────────────────────────────────────────

/// 引擎详细诊断 DTO——`get_engine_diagnostics` command 返回。
///
/// 闭合 DTO，替代旧 `serde_json::json!` 手拼响应。
/// environment/process/service 使用稳定 wire 值，禁止 `format!("{:?}", ...)`。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineDiagnosticsDto {
    /// 引擎 id。
    pub engine_id: String,
    /// 环境观测状态（稳定 wire 值：`missing` / `ready` / `broken` / `needs_rebuild`）。
    pub environment: String,
    /// 进程观测状态投影。
    pub process: ProcessStateDto,
    /// 服务健康观测（稳定 wire 值：`unknown` / `unreachable` / `healthy` / `degraded`）。
    pub service: String,
    /// 当前运行时模型健康观测（`not_loaded` / `loading` / `ready` / `failed`）。
    pub model: String,
    /// 引擎专属诊断条目列表（各 adapter 自行定义）。
    pub adapter_diagnostics: Vec<DiagnosticEntryDto>,
    /// 最近日志条目（双源合并后的截断列表）。
    pub recent_logs: Vec<EngineLogDto>,
    /// 孤儿进程恢复状态。
    pub orphan_recovery: OrphanRecoveryDto,
}

/// 单条诊断条目 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiagnosticEntryDto {
    /// 条目 key（稳定标识，前端 i18n 引用）。
    pub key: String,
    /// 显示值（不含敏感信息）。
    pub value: String,
    /// 条目标签（`info` / `warning` / `error`）。
    pub label: String,
}

/// 孤儿进程恢复状态 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OrphanRecoveryDto {
    /// 是否存在孤儿 lease。
    pub present: bool,
    /// 是否可执行恢复操作。
    pub actionable: bool,
    /// 原因说明（稳定 wire 值）。
    pub reason: String,
}

/// 从 domain `EngineDiagnostic` 投影为 DTO。
pub fn project_diagnostics(
    diag: &crate::domain::local_engine::EngineDiagnostic,
) -> Vec<DiagnosticEntryDto> {
    diag.entries
        .iter()
        .map(|e| DiagnosticEntryDto {
            key: e.key.clone(),
            value: e.value.clone(),
            label: e.label.clone(),
        })
        .collect()
}

#[cfg(test)]
mod tests;
