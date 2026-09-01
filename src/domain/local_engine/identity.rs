//! 本地引擎身份与计算计划类型（domain 唯一定义）。
//!
//! 本模块是 `EngineId`、`ModelId`、`ArtifactId`、`RuntimePlan` 与 compute
//! preference/profile/observation 系列的**唯一定义处**。infra/app 层一律
//! 从这里引用（infra 通过 re-export 保持旧 import 路径兼容），不复制第二套
//! 同义类型。
//!
//! ## 设计铁则
//!
//! - **闭合枚举**：`RuntimePlan` 是编译期闭合变体（`PythonVenv` /
//!   `ManagedBinary`），禁止 String runtime plan 或任意 JSON map 绕过。
//! - **domain 不依赖 infra/Tauri/Windows**：本模块只使用标准库、serde 与
//!   领域错误类型。
//! - **校验失败返回领域错误**：身份构造失败返回 `LocalEngineError`
//!   （`InvalidConfig`），不引用 infra 的 `RuntimeError`。

use serde::{Deserialize, Serialize};

use super::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

// ── RuntimePlan ────────────────────────────────────────────────────────────

/// 运行时计划（编译期闭合枚举）。
///
/// 描述引擎的可执行环境形态：
/// - `PythonVenv`：uv 管理 Python distribution + venv + pip packages；
/// - `ManagedBinary`：锁定 archive/可执行文件/DLL + hash + self-test；
/// - `OnnxRuntime`：ONNX Runtime DLL + 版本化 artifact（0.22.8 新增）。
///
/// **禁止**用 String runtime plan 或前端提交字段绕过此枚举。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RuntimePlan {
    /// Python 虚拟环境（uv 管理 Python distribution + venv + pip packages）。
    PythonVenv,
    /// 受管原生二进制（锁定 archive/可执行文件/DLL + hash + self-test）。
    ManagedBinary,
    /// ONNX Runtime（版本化 DLL + 模型 generation，0.22.8）。
    ///
    /// 与 `ManagedBinary` 的区别：OnnxRuntime 是**共享动态运行时**，
    /// 不伪装成 `ManagedBinary`（不启动子进程），DLL 由 in-process
    /// lazy Session 持有；Provider 负责版本化下载、hash 校验和 promote。
    OnnxRuntime,
}

impl RuntimePlan {
    /// 返回 provider 标识字符串（用于路径和日志，不暴露给前端）。
    pub fn provider_id(&self) -> &'static str {
        match self {
            Self::PythonVenv => "python_venv",
            Self::ManagedBinary => "managed_binary",
            Self::OnnxRuntime => "onnx_runtime",
        }
    }
}

impl std::fmt::Display for RuntimePlan {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.provider_id())
    }
}

// ── EngineId / ModelId / ArtifactId ────────────────────────────────────────

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
    pub fn new(id: impl Into<String>) -> Result<Self, LocalEngineError> {
        let id = id.into();
        validate_engine_id(&id)?;
        Ok(Self(id))
    }

    /// 获取内部字符串引用。
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EngineId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 模型稳定标识符（引擎 allowlist 内的受限模型身份）。
///
/// 模型 id 允许包含 `/`、大写与点号（如 `iic/SenseVoiceSmall`）——
/// 文件系统安全由存储层的 asset key 编码保证，此处只做身份合法性校验：
/// 非空、长度 1-128、不含控制字符与首尾空白。
/// 协议面预留：模型目录/操作当前仍以 `String model_id` 过渡，
/// 新类型待模型 API 全面收敛后接入。
#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ModelId(String);

#[allow(dead_code)]
impl ModelId {
    /// 创建并校验 ModelId。
    pub fn new(id: impl Into<String>) -> Result<Self, LocalEngineError> {
        let id = id.into();
        validate_model_id(&id)?;
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

impl std::fmt::Display for ModelId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Artifact 标识符（用于共享 artifact 内容寻址）。
///
/// 只允许小写字母、数字、连字符、点号和下划线，长度 1-128。
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ArtifactId(String);

impl ArtifactId {
    /// 创建并校验 ArtifactId。
    pub fn new(id: impl Into<String>) -> Result<Self, LocalEngineError> {
        let id = id.into();
        validate_artifact_id(&id)?;
        Ok(Self(id))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ArtifactId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

fn invalid_id(hint: &str, reason: &str, value: &str) -> LocalEngineError {
    LocalEngineError::with_detail(
        LocalEngineErrorCode::InvalidConfig,
        ErrorPhase::Config,
        hint,
        format!("{reason} (value: {value})"),
    )
}

/// 校验 EngineId 格式。
pub fn validate_engine_id(id: &str) -> Result<(), LocalEngineError> {
    if id.is_empty() || id.len() > 64 {
        return Err(invalid_id("引擎 id 非法", "长度必须在 1-64 之间", id));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(invalid_id(
            "引擎 id 非法",
            "只允许小写字母、数字和连字符",
            id,
        ));
    }
    if id.starts_with('-') || id.ends_with('-') {
        return Err(invalid_id("引擎 id 非法", "不允许以连字符开头或结尾", id));
    }
    if id.contains("--") {
        return Err(invalid_id("引擎 id 非法", "不允许连续连字符", id));
    }
    Ok(())
}

/// 校验 ModelId 格式。
#[allow(dead_code)]
pub fn validate_model_id(id: &str) -> Result<(), LocalEngineError> {
    if id.is_empty() || id.len() > 128 {
        return Err(invalid_id("模型 id 非法", "长度必须在 1-128 之间", id));
    }
    if id.chars().any(|c| c.is_control()) {
        return Err(invalid_id("模型 id 非法", "不允许包含控制字符", id));
    }
    if id.trim() != id {
        return Err(invalid_id("模型 id 非法", "不允许首尾空白", id));
    }
    Ok(())
}

/// 校验 ArtifactId 格式。
pub fn validate_artifact_id(id: &str) -> Result<(), LocalEngineError> {
    if id.is_empty() || id.len() > 128 {
        return Err(invalid_id("artifact id 非法", "长度必须在 1-128 之间", id));
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '.' || c == '_')
    {
        return Err(invalid_id(
            "artifact id 非法",
            "只允许小写字母、数字、连字符、点号和下划线",
            id,
        ));
    }
    if id.starts_with('-') || id.starts_with('.') || id.starts_with('_') {
        return Err(invalid_id(
            "artifact id 非法",
            "不允许以连字符、点号或下划线开头",
            id,
        ));
    }
    Ok(())
}

// ── ComputePreference / ResolvedProfile / ActualBackend ───────────────────

/// 用户计算偏好（受限枚举，由前端配置提交，不是 runtime plan）。
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
    /// 字段名保持 `runtime_kind`（manifest JSON 兼容），类型为 `RuntimePlan`。
    pub runtime_kind: RuntimePlan,
    /// artifact id（如 `python-3.12.8-x86_64-pc-windows-msvc`）。
    pub artifact_id: ArtifactId,
    /// SHA-256 hash（hex），用于验证完整性。
    pub sha256: String,
}

// ── ModelContract / ChecksumSource ────────────────────────────────────────

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

// ── Backend 一致性校验 ─────────────────────────────────────────────────────

/// Backend 校验结果（状态转换逻辑）。
///
/// 伪造 health 返回不同 backend 时进入 degraded/error，而非显示成功。
/// 此类型与其校验函数是 actual backend 一致性校验的唯一真源。
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

// ── Endpoint ───────────────────────────────────────────────────────────────

/// 受管引擎服务的 loopback endpoint。
///
/// 端口号由 infra 的 `EndpointAllocator` 分配；本类型只承载值，
/// 供 domain 的 `LaunchContext` 与 infra 共享同一 endpoint 定义。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Endpoint {
    port: u16,
}

impl Endpoint {
    /// 创建 loopback endpoint。
    /// 端口号由 `EndpointAllocator` 分配，外部不应直接构造。
    #[cfg(test)]
    pub fn new(port: u16) -> Self {
        Self { port }
    }

    /// stdio worker 占位 endpoint（0.22.7）。
    ///
    /// NDJSON worker 不监听端口；此值仅填充 `LaunchContext`/lease 的
    /// endpoint 语义位，health 校验不核对（worker ready 不回显 endpoint）。
    pub fn stdio_placeholder() -> Self {
        Self { port: 0 }
    }

    /// 创建 loopback endpoint（非测试环境仅 infra allocator 可达）。
    #[cfg(not(test))]
    pub(crate) fn new(port: u16) -> Self {
        Self { port }
    }

    /// 返回端口号。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 返回 `127.0.0.1:port` 的 SocketAddr。
    /// 当前生产路径只用 `base_url`/`port`；由 infra port 模块测试行使。
    #[allow(dead_code)]
    pub fn socket_addr(&self) -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], self.port))
    }

    /// 返回 `http://127.0.0.1:port` base URL 字符串。
    pub fn base_url(&self) -> String {
        format!("http://127.0.0.1:{}", self.port)
    }
}

impl std::fmt::Display for Endpoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "127.0.0.1:{}", self.port)
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_id_valid() {
        assert!(EngineId::new("funasr").is_ok());
        assert!(EngineId::new("paddleocr").is_ok());
        assert!(EngineId::new("funasr-gguf").is_ok());
        assert!(EngineId::new("engine123").is_ok());
    }

    #[test]
    fn engine_id_rejects_invalid() {
        assert!(EngineId::new("").is_err());
        assert!(EngineId::new("FunASR").is_err());
        assert!(EngineId::new("-funasr").is_err());
        assert!(EngineId::new("funasr-").is_err());
        assert!(EngineId::new("fun--asr").is_err());
        assert!(EngineId::new("funasr_gpu").is_err());
        assert!(EngineId::new("funasr/ocr").is_err());
        assert!(EngineId::new("..").is_err());
        assert!(EngineId::new("a".repeat(65)).is_err());
    }

    #[test]
    fn model_id_valid_and_invalid() {
        assert!(ModelId::new("iic/SenseVoiceSmall").is_ok());
        assert!(ModelId::new("paraformer-zh").is_ok());
        assert!(ModelId::new("").is_err());
        assert!(ModelId::new(" leading").is_err());
        assert!(ModelId::new("trailing ").is_err());
        assert!(ModelId::new("control\u{0}char").is_err());
        assert!(ModelId::new("a".repeat(129)).is_err());
    }

    #[test]
    fn artifact_id_valid_and_invalid() {
        assert!(ArtifactId::new("python-3.12.8-x86_64-pc-windows-msvc").is_ok());
        assert!(ArtifactId::new("onnxruntime-1.20.0").is_ok());
        assert!(ArtifactId::new("Python-3.12").is_err());
        assert!(ArtifactId::new("../../etc/passwd").is_err());
        assert!(ArtifactId::new(".hidden").is_err());
    }

    #[test]
    fn runtime_plan_serde_roundtrip() {
        let plan = RuntimePlan::PythonVenv;
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(json, "\"python_venv\"");
        let back: RuntimePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);

        let plan = RuntimePlan::ManagedBinary;
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(json, "\"managed_binary\"");
        let back: RuntimePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);

        let plan = RuntimePlan::OnnxRuntime;
        let json = serde_json::to_string(&plan).unwrap();
        assert_eq!(json, "\"onnx_runtime\"");
        let back: RuntimePlan = serde_json::from_str(&json).unwrap();
        assert_eq!(back, plan);

        // 闭合枚举：未知值拒绝
        assert!(serde_json::from_str::<RuntimePlan>("\"custom\"").is_err());
    }

    #[test]
    fn endpoint_roundtrip() {
        let ep = Endpoint::new(8100);
        assert_eq!(ep.port(), 8100);
        assert_eq!(ep.base_url(), "http://127.0.0.1:8100");
        assert_eq!(ep.to_string(), "127.0.0.1:8100");
    }
}
