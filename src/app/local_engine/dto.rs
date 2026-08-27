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
//! - 本模块在 `app` 层，从 domain `EngineDescriptor` / `EngineStatus` 投影为 UI 契约。
//! - `domain`/`infra` 层不依赖本模块。

use serde::{Deserialize, Serialize};

use crate::domain::local_engine::{
    CapabilityKind, CleanupPolicy, EngineDescriptor, EngineStatus, EngineStatusSnapshot,
    LifecyclePolicy, ProcessState, ResourceBudget,
};
use crate::infra::local_engine::runtime::{ComputeBackend, ComputePreference, RuntimeKind};

// ── Catalog DTO ──────────────────────────────────────────────────────────────

/// 引擎目录项——前端 catalog 列表展示用。
///
/// 从 `EngineDescriptor` 投影，只包含 UI 可见字段。
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
    /// cleanup 能力摘要。
    pub cleanup_summary: CleanupSummaryDto,
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

/// cleanup 能力摘要 DTO。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CleanupSummaryDto {
    /// 引擎拥有的子目录列表。
    pub owned_subdirs: Vec<String>,
    /// 是否有模型缓存。
    pub has_model_cache: bool,
    /// 是否有日志目录。
    pub has_log_dir: bool,
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
    /// 环境观测状态。
    pub environment: String,
    /// 进程观测状态（显式 DTO，前端不猜测 serde enum shape）。
    pub process: ProcessStateDto,
    /// 服务健康观测。
    pub service: String,
    /// 模型健康观测。
    pub model: String,
    /// 计算设备三层信息。
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
    /// 日志级别（"info" / "warn" / "error" / "debug" / "trace"）。
    pub level: String,
    /// 文本内容。
    pub text: String,
}

// ── 投影函数 ──────────────────────────────────────────────────────────────────

/// 从 `EngineDescriptor` + 兼容性检查结果投影 catalog item。
///
/// `compute_options` 的 `compatible` / `disabled_reason` 由传入的
/// `compatibility_results` 决定——调用方须从 `ProviderDescriptor` 真源
/// 执行 `check_compatibility`，不由前端猜测。
pub fn project_catalog_item(
    descriptor: &EngineDescriptor,
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
        cleanup_summary: project_cleanup_summary(&descriptor.cleanup),
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
        environment: environment_health_to_string(status.environment.clone()),
        process: project_process_state(&status.process),
        service: service_health_to_string(status.service),
        model: model_health_to_string(status.model.clone()),
        backend: serde_json::to_value(&status.backend).unwrap_or(serde_json::Value::Null),
        last_error: status
            .last_error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

/// 从 `LogEntry` 投影日志 DTO。
///
/// `instance_id` 从外部传入——因为 `LogEntry` 本身不含 instance_id
/// （它属于 ManagedProcess 实例，service 在查询时知道是哪个实例）。
///
/// 预留：当前 LOCAL_ENGINE_LOG 事件由 service 内联投影，此函数供未来
/// 统一日志投影入口使用。
#[allow(dead_code)]
pub fn project_log(
    engine_id: &str,
    instance_id: &str,
    entry: &crate::infra::local_engine::log_pipe::LogEntry,
) -> EngineLogDto {
    let level = match entry.source {
        crate::infra::local_engine::log_pipe::LogSource::Stdout => "info",
        crate::infra::local_engine::log_pipe::LogSource::Stderr => "warn",
    };
    let timestamp = chrono::DateTime::from_timestamp_millis(entry.timestamp_ms as i64)
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());

    EngineLogDto {
        engine_id: engine_id.to_string(),
        instance_id: instance_id.to_string(),
        operation_id: None,
        seq: entry.seq.to_string(),
        timestamp,
        level: level.to_string(),
        text: entry.text.clone(),
    }
}

// ── 枚举→字符串投影 ──────────────────────────────────────────────────────────

fn capability_kind_to_string(k: CapabilityKind) -> String {
    match k {
        CapabilityKind::Stt => "stt".to_string(),
        CapabilityKind::Ocr => "ocr".to_string(),
    }
}

fn runtime_kind_to_string(k: RuntimeKind) -> String {
    match k {
        RuntimeKind::PythonVenv => "python_venv".to_string(),
        RuntimeKind::ManagedBinary => "managed_binary".to_string(),
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
fn project_process_state(process: &ProcessState) -> ProcessStateDto {
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

fn environment_health_to_string(h: crate::domain::local_engine::EnvironmentHealth) -> String {
    match h {
        crate::domain::local_engine::EnvironmentHealth::Missing => "missing".to_string(),
        crate::domain::local_engine::EnvironmentHealth::Ready => "ready".to_string(),
        crate::domain::local_engine::EnvironmentHealth::Broken { .. } => "broken".to_string(),
        crate::domain::local_engine::EnvironmentHealth::NeedsRebuild => "needs_rebuild".to_string(),
    }
}

fn service_health_to_string(s: crate::domain::local_engine::ServiceHealth) -> String {
    match s {
        crate::domain::local_engine::ServiceHealth::Unknown => "unknown".to_string(),
        crate::domain::local_engine::ServiceHealth::Unreachable => "unreachable".to_string(),
        crate::domain::local_engine::ServiceHealth::Healthy => "healthy".to_string(),
        crate::domain::local_engine::ServiceHealth::Degraded => "degraded".to_string(),
    }
}

fn model_health_to_string(m: crate::domain::local_engine::ModelHealth) -> String {
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

fn project_cleanup_summary(c: &CleanupPolicy) -> CleanupSummaryDto {
    CleanupSummaryDto {
        owned_subdirs: c.owned_subdirs.clone(),
        has_model_cache: c.has_model_cache,
        has_log_dir: c.has_log_dir,
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
    /// 格式由后端定义，编码了 scope + engine_id + 附加键（如 install_id 或 artifact_id）。
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
    /// 是否为当前 generation（不可删除）。
    pub current: bool,
    /// 是否为上一 generation（可删除）。
    pub previous: bool,
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
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StorageTargetKindDto {
    /// 引擎 generation（venv 或 managed binary 环境）。
    EngineGeneration,
    /// 引擎模型缓存。
    EngineModelCache,
    /// Provider 共享 artifact（如 Python distribution）。
    ProviderSharedArtifact,
    /// Provider 下载缓存（如 uv cache）。
    ProviderDownloadCache,
    /// 旧版遗留资产（仅确实可证明归属该引擎时）。
    LegacyOwnedAsset,
}

impl StorageTargetKindDto {
    /// 从字符串解析（command 层用）。
    ///
    /// 预留：当前 command 层直接使用 enum 变体构造，此方法供未来
    /// 从前端字符串参数构造时使用。
    #[allow(dead_code)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "engine_generation" => Some(Self::EngineGeneration),
            "engine_model_cache" => Some(Self::EngineModelCache),
            "provider_shared_artifact" => Some(Self::ProviderSharedArtifact),
            "provider_download_cache" => Some(Self::ProviderDownloadCache),
            "legacy_owned_asset" => Some(Self::LegacyOwnedAsset),
            _ => None,
        }
    }

    /// 转字符串。
    ///
    /// 预留：当前 DTO 序列化由 serde derive 处理，此方法供未来
    /// 手动序列化或日志输出时使用。
    #[allow(dead_code)]
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::EngineGeneration => "engine_generation",
            Self::EngineModelCache => "engine_model_cache",
            Self::ProviderSharedArtifact => "provider_shared_artifact",
            Self::ProviderDownloadCache => "provider_download_cache",
            Self::LegacyOwnedAsset => "legacy_owned_asset",
        }
    }
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
    /// 被跳过的目标 id 列表（如 current generation、被引用的共享资产）。
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
/// paddleocr 有 compute_preference + lifecycle。
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
    /// 生命周期策略（仅 PaddleOCR / descriptor 允许的引擎支持）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
    /// 保存偏好后是否需要重建环境（profile 变化时为 true）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub requires_rebuild: Option<bool>,
}

/// 引擎偏好 patch DTO——`set_local_engine_preferences` 接收。
///
/// **闭合字段**：只接受 `compute_preference`、`auto_start`、`lifecycle`。
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
    /// 生命周期策略（可选；仅 PaddleOCR / descriptor 允许的引擎支持；不提供则不修改）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lifecycle: Option<String>,
}

// ── 测试 ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::{
        DesiredState, EngineStatus, EnvironmentHealth, ModelHealth, ProcessState, ServiceHealth,
    };
    use crate::infra::local_engine::runtime::EngineId;
    use serde_json::json;

    // ── ProcessStateDto 各状态可序列化 ──────────────────────────────────────────

    #[test]
    fn process_state_dto_stopped_serializes() {
        let dto = ProcessStateDto {
            state: "stopped".to_string(),
            pid: None,
            reason: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["state"], "stopped");
        assert!(json.get("pid").is_none() || json["pid"].is_null());
        assert!(json.get("reason").is_none() || json["reason"].is_null());
    }

    #[test]
    fn process_state_dto_starting_serializes() {
        let dto = ProcessStateDto {
            state: "starting".to_string(),
            pid: None,
            reason: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["state"], "starting");
    }

    #[test]
    fn process_state_dto_running_serializes_with_pid() {
        let dto = ProcessStateDto {
            state: "running".to_string(),
            pid: Some(1234),
            reason: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["state"], "running");
        assert_eq!(json["pid"], 1234);
    }

    #[test]
    fn process_state_dto_stopping_serializes() {
        let dto = ProcessStateDto {
            state: "stopping".to_string(),
            pid: None,
            reason: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["state"], "stopping");
    }

    #[test]
    fn process_state_dto_exited_serializes_with_reason() {
        let dto = ProcessStateDto {
            state: "exited".to_string(),
            pid: None,
            reason: Some("exit code 1".to_string()),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["state"], "exited");
        assert_eq!(json["reason"], "exit code 1");
    }

    // ── project_process_state 投影各变体 ─────────────────────────────────────────

    #[test]
    fn project_process_state_stopped() {
        let dto = project_process_state(&ProcessState::Stopped);
        assert_eq!(dto.state, "stopped");
        assert!(dto.pid.is_none());
        assert!(dto.reason.is_none());
    }

    #[test]
    fn project_process_state_starting() {
        let dto = project_process_state(&ProcessState::Starting);
        assert_eq!(dto.state, "starting");
    }

    #[test]
    fn project_process_state_running() {
        let dto = project_process_state(&ProcessState::Running { pid: 5678 });
        assert_eq!(dto.state, "running");
        assert_eq!(dto.pid, Some(5678));
    }

    #[test]
    fn project_process_state_stopping() {
        let dto = project_process_state(&ProcessState::Stopping);
        assert_eq!(dto.state, "stopping");
    }

    #[test]
    fn project_process_state_exited() {
        let dto = project_process_state(&ProcessState::Exited {
            reason: "crashed".to_string(),
        });
        assert_eq!(dto.state, "exited");
        assert_eq!(dto.reason, Some("crashed".to_string()));
    }

    // ── query 与 status event 使用同一 DTO shape ──────────────────────────────────

    #[test]
    fn project_status_produces_consistent_shape() {
        // 构造一个 domain EngineStatusSnapshot
        let engine_id = EngineId::new("funasr").unwrap();
        let mut status = EngineStatus::default();
        status.desired = DesiredState::Running;
        status.process = ProcessState::Running { pid: 4242 };
        status.service = ServiceHealth::Healthy;
        status.model = ModelHealth::Ready;
        status.environment = EnvironmentHealth::Ready;

        let snapshot = crate::domain::local_engine::EngineStatusSnapshot {
            engine_id,
            service_epoch: crate::domain::local_engine::ServiceEpoch::new(),
            revision: 1u64,
            status,
        };

        // query 路径：project_status
        let query_dto = project_status(&snapshot);
        let query_json = serde_json::to_value(&query_dto).unwrap();

        // event 路径：也调用 project_status（emit_status 内部调用同一函数）
        // 由于两者调用同一个函数，这里验证序列化 shape 一致
        let event_dto = project_status(&snapshot);
        let event_json = serde_json::to_value(&event_dto).unwrap();

        // 两者完全相同
        assert_eq!(query_json, event_json);

        // service_epoch 是字符串
        assert!(query_json["service_epoch"].is_string());
        assert!(
            query_json["service_epoch"]
                .as_str()
                .unwrap()
                .starts_with("epoch-")
        );

        // revision 是字符串
        assert!(query_json["revision"].is_string());
        assert_eq!(query_json["revision"], "1");

        // process 是显式 DTO 对象
        assert!(query_json["status"]["process"].is_object());
        assert_eq!(query_json["status"]["process"]["state"], "running");
        assert_eq!(query_json["status"]["process"]["pid"], 4242);
    }

    // ── service_epoch/revision 是字符串（不是数字）────────────────────────────

    #[test]
    fn engine_status_dto_service_epoch_revision_are_strings() {
        let dto = EngineStatusDto {
            engine_id: "funasr".to_string(),
            service_epoch: "epoch-abc123".to_string(),
            revision: "42".to_string(),
            status: EngineStatusWire {
                desired: "stopped".to_string(),
                operation: json!(null),
                environment: "missing".to_string(),
                process: ProcessStateDto {
                    state: "stopped".to_string(),
                    pid: None,
                    reason: None,
                },
                service: "unknown".to_string(),
                model: "unknown".to_string(),
                backend: json!(null),
                last_error: None,
            },
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json["service_epoch"].is_string());
        assert!(json["revision"].is_string());
        assert_eq!(json["service_epoch"], "epoch-abc123");
        assert_eq!(json["revision"], "42");
    }

    // ── EngineLogDto seq 是字符串 ──────────────────────────────────────────────

    #[test]
    fn engine_log_dto_seq_is_string() {
        let dto = EngineLogDto {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            operation_id: None,
            seq: "12345".to_string(),
            timestamp: "2026-08-26T00:00:00Z".to_string(),
            level: "info".to_string(),
            text: "test log".to_string(),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json["seq"].is_string());
        assert_eq!(json["seq"], "12345");
    }

    // ── ProcessStateDto 前端不会遇到字符串裸值 ──────────────────────────────────

    #[test]
    fn process_state_dto_never_serializes_as_bare_string() {
        // 旧 ProcessState enum 用 #[serde(rename_all = "snake_case")]，
        // Stopped/Starting/Stopping 序列化为裸字符串 "stopped"。
        // ProcessStateDto 必须序列化为对象 { "state": "stopped" }。
        let dto = ProcessStateDto {
            state: "stopped".to_string(),
            pid: None,
            reason: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        // 必须是对象，不是字符串
        assert!(json.is_object());
        // 前端可以安全执行 process.state，不会抛 TypeError
        assert_eq!(json["state"], "stopped");
    }

    // ── ProcessStateDto 可反序列化（round-trip）────────────────────────────────

    #[test]
    fn process_state_dto_round_trip() {
        let original = ProcessStateDto {
            state: "running".to_string(),
            pid: Some(9999),
            reason: None,
        };
        let json = serde_json::to_string(&original).unwrap();
        let deserialized: ProcessStateDto = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.state, "running");
        assert_eq!(deserialized.pid, Some(9999));
    }

    // ── project_log 投影 seq 为字符串 ──────────────────────────────────────────

    #[test]
    fn project_log_seq_is_string() {
        let entry = crate::infra::local_engine::log_pipe::LogEntry {
            seq: 42,
            timestamp_ms: 1724630400000,
            source: crate::infra::local_engine::log_pipe::LogSource::Stdout,
            text: "test line".to_string(),
        };
        let dto = project_log("funasr", "inst-abc", &entry);
        let json = serde_json::to_value(&dto).unwrap();
        assert!(json["seq"].is_string());
        assert_eq!(json["seq"], "42");
    }

    // ── EnginePreferencesPatchDto deny_unknown_fields ──────────────────────────

    #[test]
    fn patch_dto_accepts_known_fields() {
        let json = json!({
            "compute_preference": "cpu",
            "auto_start": true,
            "lifecycle": "on_demand"
        });
        let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto.compute_preference, Some("cpu".to_string()));
        assert_eq!(dto.auto_start, Some(true));
        assert_eq!(dto.lifecycle, Some("on_demand".to_string()));
    }

    #[test]
    fn patch_dto_accepts_partial_fields() {
        let json = json!({"compute_preference": "cuda"});
        let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
        assert_eq!(dto.compute_preference, Some("cuda".to_string()));
        assert!(dto.auto_start.is_none());
        assert!(dto.lifecycle.is_none());
    }

    #[test]
    fn patch_dto_accepts_empty_object() {
        let json = json!({});
        let dto: EnginePreferencesPatchDto = serde_json::from_value(json).unwrap();
        assert!(dto.compute_preference.is_none());
        assert!(dto.auto_start.is_none());
        assert!(dto.lifecycle.is_none());
    }

    #[test]
    fn patch_dto_rejects_unknown_fields() {
        let json = json!({
            "compute_preference": "cpu",
            "executable": "/bin/evil",
            "argv": ["--malicious"],
            "env": {"SECRET": "leaked"}
        });
        let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
        assert!(
            result.is_err(),
            "deny_unknown_fields should reject unknown fields"
        );
    }

    #[test]
    fn patch_dto_rejects_engine_config_injection() {
        // 前端不应能注入 engine_config
        let json = json!({
            "compute_preference": "cpu",
            "engine_config": {"port": 9999, "token": "evil"}
        });
        let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    #[test]
    fn patch_dto_rejects_script_path_injection() {
        let json = json!({
            "compute_preference": "cpu",
            "script_path": "/etc/passwd"
        });
        let result: Result<EnginePreferencesPatchDto, _> = serde_json::from_value(json);
        assert!(result.is_err());
    }

    // ── EnginePreferencesDto serialization ───────────────────────────────────

    #[test]
    fn preferences_dto_funasr_shape() {
        let dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(true),
            lifecycle: None,
            requires_rebuild: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "funasr");
        assert_eq!(json["compute_preference"], "cpu");
        assert_eq!(json["auto_start"], true);
        // lifecycle 和 requires_rebuild 被 skip_serializing_if 跳过
        assert!(json.get("lifecycle").is_none() || json["lifecycle"].is_null());
        assert!(json.get("requires_rebuild").is_none() || json["requires_rebuild"].is_null());
    }

    #[test]
    fn preferences_dto_paddleocr_shape() {
        let dto = EnginePreferencesDto {
            engine_id: "paddleocr".to_string(),
            compute_preference: Some("auto".to_string()),
            auto_start: None,
            lifecycle: Some("on_demand".to_string()),
            requires_rebuild: Some(true),
        };
        let json = serde_json::to_value(&dto).unwrap();
        assert_eq!(json["engine_id"], "paddleocr");
        assert_eq!(json["compute_preference"], "auto");
        // auto_start 为 None 被 skip
        assert!(json.get("auto_start").is_none() || json["auto_start"].is_null());
        assert_eq!(json["lifecycle"], "on_demand");
        assert_eq!(json["requires_rebuild"], true);
    }

    #[test]
    fn preferences_dto_does_not_expose_internals() {
        let dto = EnginePreferencesDto {
            engine_id: "funasr".to_string(),
            compute_preference: Some("cpu".to_string()),
            auto_start: Some(true),
            lifecycle: None,
            requires_rebuild: None,
        };
        let json = serde_json::to_value(&dto).unwrap();
        // 不包含 executable / argv / env / path / url / token
        assert!(json.get("executable").is_none());
        assert!(json.get("argv").is_none());
        assert!(json.get("env").is_none());
        assert!(json.get("engine_config").is_none());
        assert!(json.get("file_path").is_none());
        assert!(json.get("script_path").is_none());
        assert!(json.get("artifact_url").is_none());
        assert!(json.get("token").is_none());
        assert!(json.get("endpoint").is_none());
    }
}
