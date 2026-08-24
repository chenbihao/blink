//! Provider-neutral runtime/generation 契约（0.22.2）。
//!
//! 定义所有 provider 共享的闭合类型：runtime kind、artifact identity、
//! generation manifest、compute preference/profile/actual backend、
//! cleanup scope、deferred cleanup 状态等。
//!
//! ## 设计铁则
//!
//! - **闭合枚举**：`RuntimeKind` 是编译期闭合变体，禁止 `String` runtime kind
//!   或任意 JSON map 绕过。
//! - **通用状态机不含引擎字段**：本模块的类型不出现 torch、funasr、paddleocr
//!   等引擎专属字段。引擎专属状态由 adapter 从 manifest/packages 投影。
//! - **provider 专属字段隔离**：Python 扩展和 Binary 扩展各自有独立的 manifest
//!   扩展类型，不泄漏进通用状态转换代码。
//! - **infra 不依赖 app/domain**：本模块只使用标准库、serde、thiserror 和
//!   infra 内部类型。
//!
//! ## 目录拓扑（phase §3.2）
//!
//! ```text
//! %APPDATA%\blink\runtimes\
//! ├─ shared\{provider}\{artifact-id}\        # 只读、内容寻址、引用计数
//! └─ engines\{engine-id}\
//!    ├─ generations\{install-id}\
//!    ├─ staging\{operation-id}\
//!    └─ current.json
//! ```

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

// ── RuntimeKind ───────────────────────────────────────────────────────────

/// 运行时种类（编译期闭合枚举）。
///
/// 首版允许 `PythonVenv`，为受管 `ManagedBinary` 保留闭合变体。
/// **禁止**用 String runtime kind 或前端提交字段绕过此枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimeKind {
    /// Python 虚拟环境（uv 管理 Python distribution + venv + pip packages）。
    PythonVenv,
    /// 受管原生二进制（锁定 archive/可执行文件/DLL + hash + self-test）。
    ManagedBinary,
}

impl RuntimeKind {
    /// 返回 provider 标识字符串（用于路径和日志，不暴露给前端）。
    pub fn provider_id(&self) -> &'static str {
        match self {
            Self::PythonVenv => "python_venv",
            Self::ManagedBinary => "managed_binary",
        }
    }
}

impl std::fmt::Display for RuntimeKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.provider_id())
    }
}

// ── EngineId / ArtifactId ─────────────────────────────────────────────────

/// 引擎稳定标识符。
///
/// 由编译期 descriptor 声明，不接受前端动态传入。
/// 只允许小写字母、数字和连字符，长度 1-64。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct EngineId(String);

impl EngineId {
    /// 创建并校验 EngineId。
    ///
    /// 校验规则：
    /// - 非空，长度 1-64
    /// - 只允许 `[a-z0-9-]`
    /// - 不允许以连字符开头或结尾
    /// - 不允许连续连字符
    pub fn new(id: impl Into<String>) -> Result<Self, RuntimeError> {
        let id = id.into();
        validate_engine_id(&id)?;
        Ok(Self(id))
    }

    /// 获取内部字符串引用。
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// 消耗为 String。
    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for EngineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Artifact 标识符（用于共享 artifact 内容寻址）。
///
/// 只允许小写字母、数字、连字符和点号，长度 1-128。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(String);

impl ArtifactId {
    /// 创建并校验 ArtifactId。
    pub fn new(id: impl Into<String>) -> Result<Self, RuntimeError> {
        let id = id.into();
        validate_artifact_id(&id)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn into_string(self) -> String {
        self.0
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 校验 EngineId 格式。
fn validate_engine_id(id: &str) -> Result<(), RuntimeError> {
    if id.is_empty() || id.len() > 64 {
        return Err(RuntimeError::InvalidEngineId {
            reason: "长度必须在 1-64 之间".to_string(),
            value: id.to_string(),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(RuntimeError::InvalidEngineId {
            reason: "只允许小写字母、数字和连字符".to_string(),
            value: id.to_string(),
        });
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(RuntimeError::InvalidEngineId {
            reason: "不允许以连字符开头或结尾".to_string(),
            value: id.to_string(),
        });
    }
    if id.contains("--") {
        return Err(RuntimeError::InvalidEngineId {
            reason: "不允许连续连字符".to_string(),
            value: id.to_string(),
        });
    }
    Ok(())
}

/// 校验 ArtifactId 格式。
fn validate_artifact_id(id: &str) -> Result<(), RuntimeError> {
    if id.is_empty() || id.len() > 128 {
        return Err(RuntimeError::InvalidArtifactId {
            reason: "长度必须在 1-128 之间".to_string(),
            value: id.to_string(),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.' || c == '_')
    {
        return Err(RuntimeError::InvalidArtifactId {
            reason: "只允许小写字母、数字、连字符、点号和下划线".to_string(),
            value: id.to_string(),
        });
    }
    if id.starts_with('-') || id.starts_with('.') || id.starts_with('_') {
        return Err(RuntimeError::InvalidArtifactId {
            reason: "不允许以连字符、点号或下划线开头".to_string(),
            value: id.to_string(),
        });
    }
    Ok(())
}

// ── ComputePreference / ResolvedProfile / ActualBackend ───────────────────

/// 用户计算偏好（受限枚举，由前端配置提交，不是 runtime kind）。
///
/// - `auto`：按 descriptor 声明的优先级回退，记录每次失败原因。
/// - `gpu_auto`：只在 GPU backend 间选择。
/// - 显式 `cpu/cuda/vulkan/directml`：失败返回可行动错误，不回退。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputePreference {
    Auto,
    Cpu,
    GpuAuto,
    Cuda,
    Vulkan,
    Directml,
}

impl ComputePreference {
    /// 是否为显式后端（失败不回退）。
    pub fn is_explicit(&self) -> bool {
        matches!(self, Self::Cpu | Self::Cuda | Self::Vulkan | Self::Directml)
    }

    /// 是否为 GPU backend（gpu_auto 只在这些之间选择）。
    pub fn is_gpu_backend(&self) -> bool {
        matches!(self, Self::Cuda | Self::Vulkan | Self::Directml)
    }
}

impl std::fmt::Display for ComputePreference {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Auto => f.write_str("auto"),
            Self::Cpu => f.write_str("cpu"),
            Self::GpuAuto => f.write_str("gpu_auto"),
            Self::Cuda => f.write_str("cuda"),
            Self::Vulkan => f.write_str("vulkan"),
            Self::Directml => f.write_str("directml"),
        }
    }
}

/// 计算后端种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ComputeBackend {
    Cpu,
    Cuda,
    Vulkan,
    Directml,
}

impl ComputeBackend {
    /// 是否为 GPU backend。
    pub fn is_gpu(&self) -> bool {
        matches!(self, Self::Cuda | Self::Vulkan | Self::Directml)
    }
}

impl std::fmt::Display for ComputeBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Cpu => f.write_str("cpu"),
            Self::Cuda => f.write_str("cuda"),
            Self::Vulkan => f.write_str("vulkan"),
            Self::Directml => f.write_str("directml"),
        }
    }
}

/// 解析后的运行时 profile（具体 artifact 与兼容合同）。
///
/// 由 descriptor 声明的候选列表 + 本机兼容探测 + artifact 兼容检查 + self-test
/// 全部通过后解析为具体 profile。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedProfile {
    /// profile 标识（如 `cpu-x64`、`cpu-avx2`、`cuda12-sm86`、`vulkan-x64`、`directml-x64`）。
    pub profile_id: String,
    /// 对应的 compute backend 种类。
    pub backend: ComputeBackend,
    /// descriptor 声明的 artifact id（可能多个 profile 共享同一 artifact）。
    pub artifact_id: ArtifactId,
    /// profile 优先级（descriptor 声明的候选顺序，0 = 最高优先级）。
    pub priority: u32,
}

/// 服务启动后由 health 回报的实际后端观测。
///
/// 状态同时保留 requested、resolved、actual 与 fallback reason。
/// 伪造 health 返回不同 backend 时进入 degraded/error，不显示成功。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendObservation {
    /// health 回报的实际 backend。
    pub actual_backend: ComputeBackend,
    /// health 回报的设备名（如 "NVIDIA GeForce RTX 4060" / "CPU"）。
    pub device_name: String,
    /// 观测是否与 resolved profile 一致。
    pub consistent: bool,
}

/// fallback 原因记录（auto 回退时记录每次失败）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FallbackReason {
    /// 被拒绝的 profile id。
    pub rejected_profile: String,
    /// 拒绝原因分类。
    pub reason: FallbackReasonKind,
    /// 人类可读详情。
    pub detail: String,
}

/// fallback 原因分类。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FallbackReasonKind {
    /// descriptor 未声明此 profile。
    NotDeclared,
    /// 本机不兼容（如无 NVIDIA GPU、无 Vulkan 驱动）。
    HostIncompatible,
    /// artifact 不兼容（如 CPU feature 不支持 AVX2）。
    ArtifactIncompatible,
    /// self-test 失败。
    SelfTestFailed,
    /// health 回报的 actual backend 与 resolved 不一致。
    HealthMismatch,
}

// ── ArtifactIdentity ──────────────────────────────────────────────────────

/// Artifact 身份标识（内容寻址）。
///
/// 用于共享 artifact 的引用追踪。一个 artifact 由 provider + id + hash 唯一确定。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactIdentity {
    /// artifact 所属的 provider 种类。
    pub runtime_kind: RuntimeKind,
    /// artifact id（如 `python-3.12.8-x86_64-pc-windows-msvc`）。
    pub artifact_id: ArtifactId,
    /// SHA-256 hash（hex），用于验证完整性。
    pub sha256: String,
}

// ── PackageStatus ─────────────────────────────────────────────────────────

/// 包状态（通用，不含引擎专属字段）。
///
/// Python 的 torch/funasr 等包状态由 adapter 从 `packages` 列表投影，
/// infra 层只理解通用的 PackageStatus。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageStatus {
    /// 包名（如 `torch`、`funasr`、`paddleocr`）。
    pub name: String,
    /// 已安装版本（None 表示未安装）。
    pub installed_version: Option<String>,
    /// descriptor 锁定的版本要求。
    pub locked_version: String,
    /// 是否满足 descriptor 的版本要求。
    pub satisfies_lock: bool,
}

// ── GenerationManifest ────────────────────────────────────────────────────

/// Manifest schema 版本。
pub const MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 不可变 generation 的完整 manifest。
///
/// 通用部分表达所有 provider 共享的元数据；
/// provider 专属字段通过 `extension` 隔离。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GenerationManifest {
    /// Manifest schema 版本。
    pub schema_version: u32,
    /// 引擎 id。
    pub engine_id: EngineId,
    /// 运行时种类。
    pub runtime_kind: RuntimeKind,
    /// 安装 id（generation 目录名）。
    pub install_id: String,
    /// 用户请求的 compute preference。
    pub requested_preference: ComputePreference,
    /// 解析后的 profile。
    pub resolved_profile: ResolvedProfile,
    /// 安装时间（Unix 毫秒）。
    pub installed_at_ms: u64,
    /// artifact 身份标识。
    pub artifact: ArtifactIdentity,
    /// 模型契约（引擎锁定的模型身份）。
    pub model_contract: ModelContract,
    /// fallback 原因（如果 requested != resolved）。
    pub fallback_reasons: Vec<FallbackReason>,
    /// provider 专属扩展。
    pub extension: ManifestExtension,
}

/// 模型契约（锁定模型身份，防止随安装时间漂移）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelContract {
    /// 模型 id（如 `PP-OCRv6/chinese/ch_PP-OCRv6_rec_train`）。
    pub model_id: String,
    /// 模型 revision（如 `v4.0.0`）。
    pub revision: String,
    /// checksum 来源（上游提供稳定 checksum 时为 SHA-256，否则记录下载来源）。
    pub checksum_source: ChecksumSource,
}

/// checksum 来源契约。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChecksumSource {
    /// 上游提供稳定 SHA-256。
    Sha256(String),
    /// 上游不提供稳定 checksum，记录下载来源 URL 和下载时间。
    DownloadSource { url: String, downloaded_at_ms: u64 },
    /// 无 checksum（仅用于 spike/开发，生产不允许）。
    Unverified,
}

/// provider 专属 manifest 扩展（闭合枚举，不允许任意 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManifestExtension {
    /// Python venv 扩展（解释器与锁定包）。
    PythonVenv(PythonManifestExt),
    /// Managed binary 扩展（archive、executable、DLL 及 hash）。
    ManagedBinary(BinaryManifestExt),
}

/// Python venv manifest 扩展。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonManifestExt {
    /// Python 版本（如 `3.12.8`）。
    pub python_version: String,
    /// Python distribution artifact id（引用共享 artifact）。
    pub python_artifact_id: ArtifactId,
    /// venv 内已安装的包列表。
    pub packages: Vec<PackageStatus>,
    /// uv 版本。
    pub uv_version: String,
    /// package index URL（如果使用非默认 index）。
    pub index_url: Option<String>,
    /// self-test 结果。
    pub self_test_passed: bool,
}

/// Managed binary manifest 扩展。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryManifestExt {
    /// archive artifact id。
    pub archive_artifact_id: ArtifactId,
    /// archive SHA-256。
    pub archive_sha256: String,
    /// 可执行文件路径（相对于 generation 根）。
    pub executable: String,
    /// 文件清单与 hash。
    pub files: Vec<FileEntry>,
    /// 引用的共享 stdlib artifact（如 Blink 托管 Python distribution）。
    /// 只读依赖，不创建 venv、不执行 pip。
    pub stdlib_artifact: Option<ArtifactIdentity>,
    /// CPU feature 前置条件（如 `avx2`）。
    pub required_cpu_features: Vec<String>,
    /// driver 前置条件（如 `cuda >= 12.0`）。
    pub required_drivers: Vec<String>,
    /// self-test 结果。
    pub self_test_passed: bool,
}

/// 文件清单条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对于 generation 根的路径。
    pub path: String,
    /// SHA-256 hash（hex）。
    pub sha256: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// 是否为 DLL（用于 DLL 搜索路径设置）。
    pub is_dll: bool,
}

// ── Runtime/Package/Artifact 状态 ──────────────────────────────────────────

/// 运行时状态快照（通用，不含引擎专属字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeStatus {
    /// 运行时种类。
    pub runtime_kind: RuntimeKind,
    /// 环境状态。
    pub environment: EnvironmentState,
    /// 当前 generation 的 install id（None = 未安装）。
    pub current_install_id: Option<String>,
    /// 上一有效 generation 的 install id（用于回滚）。
    pub previous_install_id: Option<String>,
    /// deferred cleanup 状态。
    pub deferred_cleanups: Vec<DeferredCleanup>,
    /// requested/resolved/actual backend 的一致性状态。
    pub backend: BackendVerificationResult,
}

impl RuntimeStatus {
    /// 将 token health 的实际 backend 观测提交到通用状态快照。
    pub fn observe_backend(
        &mut self,
        resolved_backend: ComputeBackend,
        observation: Option<&BackendObservation>,
    ) {
        self.backend = verify_backend_consistency(resolved_backend, observation);
        if matches!(self.backend.state, BackendState::Error) {
            self.environment = EnvironmentState::Broken;
        }
    }
}

/// 环境状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvironmentState {
    /// 未安装。
    Missing,
    /// 已就绪。
    Ready,
    /// 损坏（附带原因由上层记录）。
    Broken,
    /// 需要重建（旧环境不兼容）。
    NeedsRebuild,
}

/// deferred cleanup 状态（被进程占用的旧 generation）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeferredCleanup {
    /// install id。
    pub install_id: String,
    /// 标记时间（Unix 毫秒）。
    pub marked_at_ms: u64,
    /// 原因。
    pub reason: String,
}

// ── CleanupScope ──────────────────────────────────────────────────────────

/// 清理范围（明确区分不同 scope，防止误删）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CleanupScope {
    /// 单引擎的 generation（staging + 旧 generations）。
    EngineGeneration {
        engine_id: EngineId,
        /// 只清理指定的 install_id（None = 清理所有非 current generation）。
        install_ids: Option<Vec<String>>,
    },
    /// 单引擎的模型缓存。
    EngineModelCache { engine_id: EngineId },
    /// provider 共享 artifact（需要引用检查）。
    ProviderSharedArtifact {
        runtime_kind: RuntimeKind,
        artifact_id: ArtifactId,
    },
    /// provider 下载缓存（uv cache 等）。
    ProviderDownloadCache { runtime_kind: RuntimeKind },
}

// ── CurrentPointer ────────────────────────────────────────────────────────

/// `current.json` 指针文件内容。
///
/// 采用同目录临时文件 + replace/rename 原子写入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CurrentPointer {
    /// 当前 generation 的 install id。
    pub install_id: String,
    /// manifest 文件相对路径（相对于 engine 根）。
    pub manifest_path: String,
    /// 更新时间（Unix 毫秒）。
    pub updated_at_ms: u64,
    /// schema 版本。
    pub schema_version: u32,
}

/// CurrentPointer schema 版本。
pub const CURRENT_POINTER_SCHEMA_VERSION: u32 = 1;

// ── RuntimeError ──────────────────────────────────────────────────────────

/// Runtime 层错误类型。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("无效的 EngineId: {reason} (value: {value})")]
    InvalidEngineId { reason: String, value: String },

    #[error("无效的 ArtifactId: {reason} (value: {value})")]
    InvalidArtifactId { reason: String, value: String },

    #[error("路径逃逸: {path}")]
    PathTraversal { path: String },

    #[error("generation 不存在: {install_id}")]
    GenerationNotFound { install_id: String },

    #[error("current.json 不存在")]
    CurrentPointerMissing,

    #[error("current.json 解析失败: {message}")]
    CurrentPointerParseFailed { message: String },

    #[error("manifest 解析失败: {message}")]
    ManifestParseFailed { message: String },

    #[error("manifest 序列化失败: {message}")]
    ManifestSerializeFailed { message: String },

    #[error("manifest schema 版本不兼容: expected={expected}, actual={actual}")]
    ManifestSchemaIncompatible { expected: u32, actual: u32 },

    #[error("staging 目录创建失败: {message}")]
    StagingCreateFailed { message: String },

    #[error("generation 提升失败: {message}")]
    GenerationPromoteFailed { message: String },

    #[error("current.json 原子替换失败: {message}")]
    CurrentPointerSwitchFailed { message: String },

    #[error("安装失败: {message}")]
    InstallFailed { message: String },

    #[error("self-test 失败: {message}")]
    SelfTestFailed { message: String },

    #[error("compute profile 解析失败: {message}")]
    ProfileResolutionFailed { message: String },

    #[error("显式 backend 失败（不回退）: {message}")]
    ExplicitBackendFailed { message: String },

    #[error("health actual backend 不匹配: expected={expected}, actual={actual}")]
    BackendMismatch { expected: String, actual: String },

    #[error("清理失败: {message}")]
    CleanupFailed { message: String },

    #[error("共享 artifact 仍被引用，拒绝删除: {artifact_id}, refs={ref_count}")]
    ArtifactStillReferenced {
        artifact_id: String,
        ref_count: usize,
    },

    #[error("迁移失败: {message}")]
    MigrationFailed { message: String },

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

// ── 路径安全 API ───────────────────────────────────────────────────────────

/// 运行时根目录：`%APPDATA%\blink\runtimes`
pub fn runtimes_root() -> PathBuf {
    #[cfg(test)]
    {
        // 单测不得触碰真实 `%APPDATA%\blink`。进程级唯一根目录同时避免不同
        // cargo test 进程互相清理 refcount/current 测试数据。
        return std::env::temp_dir().join(format!("blink-runtime-tests-{}", std::process::id()));
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::app_data_dir().join("runtimes")
}

/// 共享 artifact 目录：`runtimes/shared/{provider}/{artifact_id}`
pub fn shared_artifact_dir(runtime_kind: RuntimeKind, artifact_id: &ArtifactId) -> PathBuf {
    runtimes_root()
        .join("shared")
        .join(runtime_kind.provider_id())
        .join(artifact_id.as_str())
}

/// 引擎根目录：`runtimes/engines/{engine_id}`
pub fn engine_root(engine_id: &EngineId) -> PathBuf {
    runtimes_root().join("engines").join(engine_id.as_str())
}

/// 引擎 generations 目录：`engines/{engine_id}/generations`
pub fn generations_dir(engine_id: &EngineId) -> PathBuf {
    engine_root(engine_id).join("generations")
}

/// 单个 generation 目录：`engines/{engine_id}/generations/{install_id}`
pub fn generation_dir(engine_id: &EngineId, install_id: &str) -> PathBuf {
    generations_dir(engine_id).join(install_id)
}

/// 引擎 staging 目录：`engines/{engine_id}/staging`
pub fn staging_dir(engine_id: &EngineId) -> PathBuf {
    engine_root(engine_id).join("staging")
}

/// 单个 operation 的 staging 目录：`engines/{engine_id}/staging/{operation_id}`
pub fn operation_staging_dir(engine_id: &EngineId, operation_id: &str) -> PathBuf {
    staging_dir(engine_id).join(operation_id)
}

/// current.json 路径：`engines/{engine_id}/current.json`
pub fn current_pointer_path(engine_id: &EngineId) -> PathBuf {
    engine_root(engine_id).join("current.json")
}

/// manifest 文件路径（generation 目录内）。
pub fn manifest_path(engine_id: &EngineId, install_id: &str) -> PathBuf {
    generation_dir(engine_id, install_id).join("manifest.json")
}

/// 引用计数文件路径（共享 artifact 目录内）。
pub fn refcount_path(runtime_kind: RuntimeKind, artifact_id: &ArtifactId) -> PathBuf {
    shared_artifact_dir(runtime_kind, artifact_id).join("refcount.json")
}

/// 模型缓存根目录：`%APPDATA%\blink\models`
pub fn models_root() -> PathBuf {
    crate::infra::utils::paths::app_data_dir().join("models")
}

/// 引擎模型缓存目录：`models/{engine_id}`
pub fn engine_model_cache_dir(engine_id: &EngineId) -> PathBuf {
    models_root().join(engine_id.as_str())
}

/// 旧 FunASR venv 路径：`%APPDATA%\blink\python\venv`（兼容迁移用）。
pub fn legacy_funasr_venv_dir() -> PathBuf {
    crate::infra::utils::paths::python_dir().join("venv")
}

/// Python 公共资产根目录：`%APPDATA%\blink\python`
pub fn python_shared_root() -> PathBuf {
    crate::infra::utils::paths::python_dir()
}

/// uv 本地安装目录：`python\uv`
pub fn uv_install_dir() -> PathBuf {
    python_shared_root().join("uv")
}

/// uv 本地安装的 `uv.exe` 路径。
pub fn local_uv_exe() -> PathBuf {
    uv_install_dir().join("uv.exe")
}

/// uv cache 目录：`python\cache\uv`
pub fn uv_cache_dir() -> PathBuf {
    python_shared_root().join("cache").join("uv")
}

/// uv 管理的 Python distributions 目录：`python\pythons`
pub fn uv_python_dir() -> PathBuf {
    python_shared_root().join("pythons")
}

// ── 路径安全校验 ───────────────────────────────────────────────────────────

/// 校验 install_id（只允许 `[a-z0-9-]`，防止路径逃逸）。
pub fn validate_install_id(id: &str) -> Result<(), RuntimeError> {
    if id.is_empty() || id.len() > 128 {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    if id.starts_with('-') || id.ends_with('-') || id.contains("--") {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    Ok(())
}

/// 校验 operation_id（只允许 `[a-z0-9-]`，防止路径逃逸）。
pub fn validate_operation_id(id: &str) -> Result<(), RuntimeError> {
    validate_install_id(id)
}

/// 安全校验路径不逃逸出指定根目录。
///
/// 检查规范化后的路径是否以 `root` 为前缀。
/// 拒绝 `..`、绝对路径和符号链接逃逸。
pub fn ensure_path_within(root: &Path, path: &Path) -> Result<PathBuf, RuntimeError> {
    let canonical_root = root.canonicalize().map_err(RuntimeError::Io)?;
    let canonical_path = if path.is_absolute() {
        path.canonicalize().map_err(RuntimeError::Io)?
    } else {
        root.join(path).canonicalize().map_err(RuntimeError::Io)?
    };
    if !canonical_path.starts_with(&canonical_root) {
        return Err(RuntimeError::PathTraversal {
            path: path.display().to_string(),
        });
    }
    Ok(canonical_path)
}

// ── 原子文件操作 ───────────────────────────────────────────────────────────

/// 原子写入小文件（同目录临时文件 + ReplaceFileW/rename）。
///
/// Windows 上使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` 原子替换，
/// 保证目标文件在任何时刻都存在，即使进程崩溃也不会丢失新旧文件。
pub fn atomic_write_file(path: &Path, content: &[u8]) -> Result<(), RuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::CurrentPointerSwitchFailed {
            message: "路径无父目录".to_string(),
        })?;
    std::fs::create_dir_all(parent)?;

    // 创建同目录临时文件
    let tmp_name = format!(
        ".tmp_{}_{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let tmp_path = parent.join(&tmp_name);

    // 写入临时文件
    std::fs::write(&tmp_path, content).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        RuntimeError::Io(e)
    })?;

    // 原子替换
    atomic_replace(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        RuntimeError::CurrentPointerSwitchFailed {
            message: format!("原子替换失败: {e}"),
        }
    })?;

    Ok(())
}

/// 原子替换文件（跨平台）。
///
/// - Windows: 使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`。
/// - Unix: `std::fs::rename`（POSIX rename 是原子的）。
///
fn atomic_replace(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{
            MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;

        let target_wide: Vec<u16> = to
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let source_wide: Vec<u16> = from
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let flags = MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0);
        let result = unsafe {
            MoveFileExW(
                PCWSTR(source_wide.as_ptr()),
                PCWSTR(target_wide.as_ptr()),
                flags,
            )
        };

        result.map_err(std::io::Error::other)
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// 原子写入 JSON 文件。
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    let json = serde_json::to_vec_pretty(value)?;
    atomic_write_file(path, &json)
}

// ── current.json 读写 ──────────────────────────────────────────────────────

/// 读取 current.json。
///
/// 如果文件不存在返回 `Ok(None)`（引擎未安装）。
pub fn read_current_pointer(engine_id: &EngineId) -> Result<Option<CurrentPointer>, RuntimeError> {
    let path = current_pointer_path(engine_id);
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let pointer: CurrentPointer =
        serde_json::from_str(&content).map_err(|e| RuntimeError::CurrentPointerParseFailed {
            message: format!("{e}"),
        })?;
    Ok(Some(pointer))
}

/// 原子写入 current.json。
pub fn write_current_pointer(
    engine_id: &EngineId,
    pointer: &CurrentPointer,
) -> Result<(), RuntimeError> {
    atomic_write_json(&current_pointer_path(engine_id), pointer)
}

// ── manifest 读写 ──────────────────────────────────────────────────────────

/// 读取 generation manifest。
pub fn read_manifest(
    engine_id: &EngineId,
    install_id: &str,
) -> Result<GenerationManifest, RuntimeError> {
    validate_install_id(install_id)?;
    let path = manifest_path(engine_id, install_id);
    if !path.exists() {
        return Err(RuntimeError::GenerationNotFound {
            install_id: install_id.to_string(),
        });
    }
    let content = std::fs::read_to_string(&path)?;
    let manifest: GenerationManifest =
        serde_json::from_str(&content).map_err(|e| RuntimeError::ManifestParseFailed {
            message: format!("{e}"),
        })?;
    if manifest.schema_version != MANIFEST_SCHEMA_VERSION {
        return Err(RuntimeError::ManifestSchemaIncompatible {
            expected: MANIFEST_SCHEMA_VERSION,
            actual: manifest.schema_version,
        });
    }
    Ok(manifest)
}

/// 写入 generation manifest（在 generation 目录内）。
pub fn write_manifest(
    engine_id: &EngineId,
    install_id: &str,
    manifest: &GenerationManifest,
) -> Result<(), RuntimeError> {
    validate_install_id(install_id)?;
    let dir = generation_dir(engine_id, install_id);
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("manifest.json");
    atomic_write_json(&path, manifest)
}

// ── 引用计数 ──────────────────────────────────────────────────────────────

/// 引用计数文件内容。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct RefCount {
    /// 引用此 artifact 的 (engine_id, install_id) 列表。
    pub references: Vec<RefEntry>,
}

/// 单条引用记录。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RefEntry {
    pub engine_id: String,
    pub install_id: String,
}

/// 读取引用计数。
pub fn read_refcount(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
) -> Result<RefCount, RuntimeError> {
    let path = refcount_path(runtime_kind, artifact_id);
    if !path.exists() {
        return Ok(RefCount::default());
    }
    let content = std::fs::read_to_string(&path)?;
    let rc: RefCount = serde_json::from_str(&content)?;
    Ok(rc)
}

/// 写入引用计数。
pub fn write_refcount(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
    rc: &RefCount,
) -> Result<(), RuntimeError> {
    let dir = shared_artifact_dir(runtime_kind, artifact_id);
    std::fs::create_dir_all(&dir)?;
    let path = refcount_path(runtime_kind, artifact_id);
    atomic_write_json(&path, rc)
}

/// 增加引用。
pub fn add_reference(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
    engine_id: &str,
    install_id: &str,
) -> Result<(), RuntimeError> {
    let mut rc = read_refcount(runtime_kind, artifact_id)?;
    let entry = RefEntry {
        engine_id: engine_id.to_string(),
        install_id: install_id.to_string(),
    };
    if !rc.references.contains(&entry) {
        rc.references.push(entry);
        write_refcount(runtime_kind, artifact_id, &rc)?;
    }
    Ok(())
}

/// 移除引用。
pub fn remove_reference(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
    engine_id: &str,
    install_id: &str,
) -> Result<(), RuntimeError> {
    let mut rc = read_refcount(runtime_kind, artifact_id)?;
    let entry = RefEntry {
        engine_id: engine_id.to_string(),
        install_id: install_id.to_string(),
    };
    rc.references.retain(|r| r != &entry);
    write_refcount(runtime_kind, artifact_id, &rc)?;
    Ok(())
}

/// 检查引用计数（用于删除前引用检查）。
pub fn ref_count(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
) -> Result<usize, RuntimeError> {
    let rc = read_refcount(runtime_kind, artifact_id)?;
    Ok(rc.references.len())
}

/// 扫描所有引擎的 manifest，查找引用了指定 artifact 的 generation。
///
/// 用于共享 artifact 删除前的引用检查。
/// 遍历 `runtimes/engines/*/generations/*/manifest.json`，
/// 检查 manifest 中的 artifact identity 是否匹配。
pub fn scan_artifact_references(
    runtime_kind: RuntimeKind,
    artifact_id: &ArtifactId,
) -> Result<Vec<RefEntry>, RuntimeError> {
    let engines_root = runtimes_root().join("engines");
    if !engines_root.exists() {
        return Ok(Vec::new());
    }

    let mut refs = Vec::new();

    for engine_entry in std::fs::read_dir(&engines_root)? {
        let engine_entry = engine_entry?;
        if !engine_entry.file_type()?.is_dir() {
            continue;
        }
        let engine_name = match engine_entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let gens_dir = engines_root.join(&engine_name).join("generations");
        if !gens_dir.exists() {
            continue;
        }

        for gen_entry in std::fs::read_dir(&gens_dir)? {
            let gen_entry = gen_entry?;
            if !gen_entry.file_type()?.is_dir() {
                continue;
            }
            let install_id = match gen_entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            let manifest_file = gens_dir.join(&install_id).join("manifest.json");
            if !manifest_file.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&manifest_file) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let manifest: GenerationManifest = match serde_json::from_str(&content) {
                Ok(m) => m,
                Err(_) => continue,
            };

            // 检查 manifest 中的 artifact 是否匹配
            if manifest.artifact.runtime_kind == runtime_kind
                && manifest.artifact.artifact_id == *artifact_id
            {
                refs.push(RefEntry {
                    engine_id: engine_name.clone(),
                    install_id: install_id.clone(),
                });
            }

            // 检查 Binary manifest 中的 stdlib_artifact 引用
            if let ManifestExtension::ManagedBinary(ref ext) = manifest.extension {
                if let Some(ref stdlib) = ext.stdlib_artifact {
                    if stdlib.runtime_kind == runtime_kind && stdlib.artifact_id == *artifact_id {
                        refs.push(RefEntry {
                            engine_id: engine_name.clone(),
                            install_id: format!("{}#stdlib", install_id),
                        });
                    }
                }
            }
        }
    }

    Ok(refs)
}

// ── 时间戳辅助 ─────────────────────────────────────────────────────────────

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 生成 install_id（时间戳 + 随机后缀）。
pub fn generate_install_id() -> String {
    let now = now_ms();
    let rand = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() as u64;
        ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
    };
    format!("gen-{now:016x}-{rand:04x}")
}

/// 生成 operation_id（时间戳 + 随机后缀）。
pub fn generate_operation_id() -> String {
    let now = now_ms();
    let rand = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() as u64;
        ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
    };
    format!("op-{now:016x}-{rand:04x}")
}

// ── Backend 一致性校验（§3.5/§6.3） ─────────────────────────────────────────

/// Backend 校验结果（状态转换逻辑）。
///
/// §6.3 要求：伪造 health 返回不同 backend 时进入 degraded/error，而非显示成功。
/// 此函数是 actual backend 一致性校验的唯一真源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BackendVerificationResult {
    /// 校验后的运行时 backend 状态。
    pub state: BackendState,
    /// resolved profile 期望的 backend（来自 manifest）。
    pub expected_backend: ComputeBackend,
    /// health 回报的实际 backend（如果已观测到）。
    pub actual_backend: Option<ComputeBackend>,
    /// health 回报的设备名。
    pub device_name: Option<String>,
    /// 不一致原因（如果 state 为 Degraded/Error）。
    pub mismatch_reason: Option<String>,
}

/// 校验后的 backend 运行状态（状态转换逻辑）。
///
/// - `Healthy`：actual == resolved，服务可用。
/// - `Degraded`：actual != resolved，但服务仍可降级运行（如请求 CUDA 但实际跑 CPU）。
/// - `Error`：actual backend 完全不匹配或 health 回报异常，服务不可用。
/// - `Pending`：尚未收到 health 回报，无法判定。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BackendState {
    /// actual == resolved，服务可用。
    Healthy,
    /// actual != resolved，但服务仍可降级运行。
    Degraded,
    /// actual backend 完全不匹配或 health 回报异常。
    Error,
    /// 尚未收到 health 回报。
    Pending,
}

/// 校验 actual backend 与 resolved profile 的一致性。
///
/// §3.5/§6.3 要求：状态同时保留 requested、resolved、actual 与 fallback reason。
/// 伪造 health 返回不同 backend 时进入 degraded/error，不显示成功。
///
/// 校验规则：
/// - `actual == resolved` → `Healthy`
/// - `actual != resolved` 且 actual 仍是有效 backend → `Degraded`（降级运行）
/// - `actual` 为 GPU 但 resolved 为 CPU（或反之）→ `Error`（严重不一致）
/// - health 未回报 → `Pending`
pub fn verify_backend_consistency(
    resolved_backend: ComputeBackend,
    observation: Option<&BackendObservation>,
) -> BackendVerificationResult {
    match observation {
        None => BackendVerificationResult {
            state: BackendState::Pending,
            expected_backend: resolved_backend,
            actual_backend: None,
            device_name: None,
            mismatch_reason: None,
        },
        Some(obs) => {
            let actual = obs.actual_backend;
            if actual == resolved_backend {
                BackendVerificationResult {
                    state: BackendState::Healthy,
                    expected_backend: resolved_backend,
                    actual_backend: Some(actual),
                    device_name: Some(obs.device_name.clone()),
                    mismatch_reason: None,
                }
            } else {
                // actual != resolved
                // GPU ↔ CPU 交叉不一致视为 Error，同侧（GPU→GPU）视为 Degraded
                let cross_class = actual.is_gpu() != resolved_backend.is_gpu();
                let state = if cross_class {
                    BackendState::Error
                } else {
                    BackendState::Degraded
                };
                let reason = format!(
                    "health 回报 backend 不匹配: expected={}, actual={}, device={}",
                    resolved_backend, actual, obs.device_name
                );
                tracing::warn!(%reason, "backend 一致性校验失败");
                BackendVerificationResult {
                    state,
                    expected_backend: resolved_backend,
                    actual_backend: Some(actual),
                    device_name: Some(obs.device_name.clone()),
                    mismatch_reason: Some(reason),
                }
            }
        }
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EngineId 校验 ──────────────────────────────────────────────────────

    #[test]
    fn engine_id_valid() {
        assert!(EngineId::new("funasr").is_ok());
        assert!(EngineId::new("paddleocr").is_ok());
        assert!(EngineId::new("funasr-gguf").is_ok());
        assert!(EngineId::new("engine123").is_ok());
    }

    #[test]
    fn engine_id_rejects_empty() {
        assert!(EngineId::new("").is_err());
    }

    #[test]
    fn engine_id_rejects_uppercase() {
        assert!(EngineId::new("FunASR").is_err());
        assert!(EngineId::new("PaddleOCR").is_err());
    }

    #[test]
    fn engine_id_rejects_leading_trailing_hyphen() {
        assert!(EngineId::new("-funasr").is_err());
        assert!(EngineId::new("funasr-").is_err());
    }

    #[test]
    fn engine_id_rejects_double_hyphen() {
        assert!(EngineId::new("fun--asr").is_err());
    }

    #[test]
    fn engine_id_rejects_special_chars() {
        assert!(EngineId::new("funasr_gpu").is_err());
        assert!(EngineId::new("funasr.ocr").is_err());
        assert!(EngineId::new("funasr/ocr").is_err());
        assert!(EngineId::new("..").is_err());
        assert!(EngineId::new("funasr../../etc/passwd").is_err());
    }

    #[test]
    fn engine_id_rejects_too_long() {
        let long = "a".repeat(65);
        assert!(EngineId::new(&long).is_err());
    }

    // ── ArtifactId 校验 ────────────────────────────────────────────────────

    #[test]
    fn artifact_id_valid() {
        assert!(ArtifactId::new("python-3.12.8-x86_64-pc-windows-msvc").is_ok());
        assert!(ArtifactId::new("onnxruntime-1.20.0").is_ok());
        assert!(ArtifactId::new("llama-funasr-v0.2.0").is_ok());
    }

    #[test]
    fn artifact_id_rejects_uppercase() {
        assert!(ArtifactId::new("Python-3.12").is_err());
    }

    #[test]
    fn artifact_id_rejects_path_traversal() {
        assert!(ArtifactId::new("../../etc/passwd").is_err());
        assert!(ArtifactId::new("a/../b").is_err());
    }

    // ── install_id / operation_id 校验 ─────────────────────────────────────

    #[test]
    fn install_id_valid() {
        assert!(validate_install_id("gen-1234567890abcdef-abcd").is_ok());
        assert!(validate_install_id("install001").is_ok());
    }

    #[test]
    fn install_id_rejects_path_traversal() {
        assert!(validate_install_id("../escape").is_err());
        assert!(validate_install_id("a/../b").is_err());
        assert!(validate_install_id("a/b").is_err());
        assert!(validate_install_id("a\\b").is_err());
    }

    #[test]
    fn install_id_rejects_uppercase() {
        assert!(validate_install_id("Gen-ABC").is_err());
    }

    #[test]
    fn operation_id_valid() {
        assert!(validate_operation_id("op-1234567890abcdef-abcd").is_ok());
    }

    #[test]
    fn operation_id_rejects_path_traversal() {
        assert!(validate_operation_id("../escape").is_err());
    }

    // ── RuntimeKind ─────────────────────────────────────────────────────────

    #[test]
    fn runtime_kind_serde_roundtrip() {
        let kind = RuntimeKind::PythonVenv;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"python_venv\"");
        let back: RuntimeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);

        let kind = RuntimeKind::ManagedBinary;
        let json = serde_json::to_string(&kind).unwrap();
        assert_eq!(json, "\"managed_binary\"");
        let back: RuntimeKind = serde_json::from_str(&json).unwrap();
        assert_eq!(back, kind);
    }

    // ── ComputePreference ───────────────────────────────────────────────────

    #[test]
    fn compute_preference_is_explicit() {
        assert!(!ComputePreference::Auto.is_explicit());
        assert!(!ComputePreference::GpuAuto.is_explicit());
        assert!(ComputePreference::Cpu.is_explicit());
        assert!(ComputePreference::Cuda.is_explicit());
        assert!(ComputePreference::Vulkan.is_explicit());
        assert!(ComputePreference::Directml.is_explicit());
    }

    #[test]
    fn compute_preference_is_gpu_backend() {
        assert!(!ComputePreference::Auto.is_gpu_backend());
        assert!(!ComputePreference::Cpu.is_gpu_backend());
        assert!(ComputePreference::Cuda.is_gpu_backend());
        assert!(ComputePreference::Vulkan.is_gpu_backend());
        assert!(ComputePreference::Directml.is_gpu_backend());
    }

    // ── Manifest 序列化往返 ─────────────────────────────────────────────────

    #[test]
    fn python_manifest_roundtrip() {
        let manifest = GenerationManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            engine_id: EngineId::new("funasr").unwrap(),
            runtime_kind: RuntimeKind::PythonVenv,
            install_id: "gen-test0001".to_string(),
            requested_preference: ComputePreference::Cpu,
            resolved_profile: ResolvedProfile {
                profile_id: "cpu-x64".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                priority: 0,
            },
            installed_at_ms: 1700000000000,
            artifact: ArtifactIdentity {
                runtime_kind: RuntimeKind::PythonVenv,
                artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                sha256: "abc123".to_string(),
            },
            model_contract: ModelContract {
                model_id: "funasr-model".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: ChecksumSource::Unverified,
            },
            fallback_reasons: Vec::new(),
            extension: ManifestExtension::PythonVenv(PythonManifestExt {
                python_version: "3.12.8".to_string(),
                python_artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                packages: vec![PackageStatus {
                    name: "torch".to_string(),
                    installed_version: Some("2.5.0".to_string()),
                    locked_version: "2.5.0".to_string(),
                    satisfies_lock: true,
                }],
                uv_version: "0.6.10".to_string(),
                index_url: None,
                self_test_passed: true,
            }),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let back: GenerationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(back.engine_id, manifest.engine_id);
        assert_eq!(back.runtime_kind, manifest.runtime_kind);
        assert_eq!(back.install_id, manifest.install_id);

        match back.extension {
            ManifestExtension::PythonVenv(ext) => {
                assert_eq!(ext.python_version, "3.12.8");
                assert_eq!(ext.packages.len(), 1);
                assert_eq!(ext.packages[0].name, "torch");
                assert!(ext.self_test_passed);
            }
            ManifestExtension::ManagedBinary(_) => panic!("应为 PythonVenv"),
        }
    }

    #[test]
    fn binary_manifest_roundtrip() {
        let manifest = GenerationManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            engine_id: EngineId::new("funasr-gguf").unwrap(),
            runtime_kind: RuntimeKind::ManagedBinary,
            install_id: "gen-bin0001".to_string(),
            requested_preference: ComputePreference::Cpu,
            resolved_profile: ResolvedProfile {
                profile_id: "cpu-avx2".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                priority: 0,
            },
            installed_at_ms: 1700000000000,
            artifact: ArtifactIdentity {
                runtime_kind: RuntimeKind::ManagedBinary,
                artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                sha256: "def456".to_string(),
            },
            model_contract: ModelContract {
                model_id: "sensevoice-q8".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: ChecksumSource::Sha256("abc789".to_string()),
            },
            fallback_reasons: Vec::new(),
            extension: ManifestExtension::ManagedBinary(BinaryManifestExt {
                archive_artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                archive_sha256: "def456".to_string(),
                executable: "llama-funasr-server.exe".to_string(),
                files: vec![FileEntry {
                    path: "llama-funasr-server.exe".to_string(),
                    sha256: "aaa111".to_string(),
                    size: 50000000,
                    is_dll: false,
                }],
                stdlib_artifact: Some(ArtifactIdentity {
                    runtime_kind: RuntimeKind::PythonVenv,
                    artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                    sha256: "abc123".to_string(),
                }),
                required_cpu_features: vec!["avx2".to_string()],
                required_drivers: Vec::new(),
                self_test_passed: true,
            }),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let back: GenerationManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime_kind, RuntimeKind::ManagedBinary);

        match back.extension {
            ManifestExtension::ManagedBinary(ext) => {
                assert_eq!(ext.executable, "llama-funasr-server.exe");
                assert!(ext.self_test_passed);
                assert!(ext.stdlib_artifact.is_some());
                assert_eq!(ext.required_cpu_features, vec!["avx2"]);
            }
            ManifestExtension::PythonVenv(_) => panic!("应为 ManagedBinary"),
        }
    }

    // ── 原子文件操作 ────────────────────────────────────────────────────────

    #[test]
    fn atomic_write_and_read_current_pointer() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("current.json");

        let pointer = CurrentPointer {
            install_id: "gen-test0001".to_string(),
            manifest_path: "generations/gen-test0001/manifest.json".to_string(),
            updated_at_ms: 1700000000000,
            schema_version: CURRENT_POINTER_SCHEMA_VERSION,
        };

        atomic_write_json(&path, &pointer).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let back: CurrentPointer = serde_json::from_str(&content).unwrap();
        assert_eq!(back.install_id, "gen-test0001");
        assert_eq!(back.schema_version, CURRENT_POINTER_SCHEMA_VERSION);
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("current.json");

        let p1 = CurrentPointer {
            install_id: "gen-v1".to_string(),
            manifest_path: "generations/gen-v1/manifest.json".to_string(),
            updated_at_ms: 1000,
            schema_version: CURRENT_POINTER_SCHEMA_VERSION,
        };
        atomic_write_json(&path, &p1).unwrap();

        let p2 = CurrentPointer {
            install_id: "gen-v2".to_string(),
            manifest_path: "generations/gen-v2/manifest.json".to_string(),
            updated_at_ms: 2000,
            schema_version: CURRENT_POINTER_SCHEMA_VERSION,
        };
        atomic_write_json(&path, &p2).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let back: CurrentPointer = serde_json::from_str(&content).unwrap();
        assert_eq!(back.install_id, "gen-v2");
    }

    #[test]
    fn atomic_replace_failure_preserves_existing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_source = tmp.path().join("missing.tmp");
        let target = tmp.path().join("current.json");
        std::fs::write(&target, b"old-pointer").unwrap();

        assert!(atomic_replace(&missing_source, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old-pointer");
    }

    #[test]
    fn runtime_status_commits_backend_mismatch_as_broken() {
        let pending = verify_backend_consistency(ComputeBackend::Cuda, None);
        let mut status = RuntimeStatus {
            runtime_kind: RuntimeKind::PythonVenv,
            environment: EnvironmentState::Ready,
            current_install_id: Some("gen-backend-test".to_string()),
            previous_install_id: None,
            deferred_cleanups: Vec::new(),
            backend: pending,
        };
        let observation = BackendObservation {
            actual_backend: ComputeBackend::Cpu,
            device_name: "fallback cpu".to_string(),
            consistent: false,
        };
        status.observe_backend(ComputeBackend::Cuda, Some(&observation));
        assert_eq!(status.backend.state, BackendState::Error);
        assert_eq!(status.environment, EnvironmentState::Broken);
    }

    // ── ensure_path_within ──────────────────────────────────────────────────

    #[test]
    fn ensure_path_within_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let valid = root.join("subdir").join("file.txt");
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(&valid, b"test").unwrap();
        assert!(ensure_path_within(&root, &valid).is_ok());

        let escape = root.join("..").join("..").join("etc").join("passwd");
        assert!(ensure_path_within(&root, &escape).is_err());
    }

    // ── generate_install_id / operation_id ──────────────────────────────────

    #[test]
    fn generate_install_id_is_valid() {
        let id = generate_install_id();
        assert!(validate_install_id(&id).is_ok(), "install_id 不合法: {id}");
    }

    #[test]
    fn generate_operation_id_is_valid() {
        let id = generate_operation_id();
        assert!(
            validate_operation_id(&id).is_ok(),
            "operation_id 不合法: {id}"
        );
    }

    #[test]
    fn generate_install_id_unique() {
        let mut ids = Vec::new();
        for _ in 0..10 {
            let id = generate_install_id();
            assert!(!ids.contains(&id), "install_id 重复: {id}");
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    // ── refcount ────────────────────────────────────────────────────────────

    #[test]
    fn refcount_add_remove() {
        let artifact_id = ArtifactId::new("test-refcount-artifact-0001").unwrap();
        let kind = RuntimeKind::PythonVenv;

        // 清理可能存在的测试数据
        let dir = shared_artifact_dir(kind, &artifact_id);
        let _ = std::fs::remove_dir_all(&dir);

        assert_eq!(ref_count(kind, &artifact_id).unwrap(), 0);

        add_reference(kind, &artifact_id, "engine-a", "gen-001").unwrap();
        add_reference(kind, &artifact_id, "engine-b", "gen-002").unwrap();
        assert_eq!(ref_count(kind, &artifact_id).unwrap(), 2);

        // 重复添加不会增加
        add_reference(kind, &artifact_id, "engine-a", "gen-001").unwrap();
        assert_eq!(ref_count(kind, &artifact_id).unwrap(), 2);

        remove_reference(kind, &artifact_id, "engine-a", "gen-001").unwrap();
        assert_eq!(ref_count(kind, &artifact_id).unwrap(), 1);

        // 清理
        let _ = std::fs::remove_dir_all(&dir);
    }
}
