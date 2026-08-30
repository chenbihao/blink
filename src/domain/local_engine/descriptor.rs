//! 引擎描述符（0.22.3）。
//!
//! 描述符声明引擎的静态事实：稳定 id、显示元数据、能力种类、运行时种类/profile、
//! 安装计划或其受限引用、schema/model contract、生命周期策略、超时/空闲 TTL、
//! 资源和 cleanup 声明。
//!
//! ## 设计铁则
//!
//! - **编译期内置 allowlist**：descriptor 由 Rust 编译期声明，不接受前端动态传入。
//! - **闭合枚举**：`CapabilityKind`、`RuntimePlan` 均为闭合枚举，
//!   前端无法提交 runtime kind、URL、executable、argv 或环境变量。
//! - **复用 infra 类型**：`RuntimePlan`、`EngineId`、`ArtifactId`、
//!   `ResolvedProfile`、`ComputePreference`、`ModelContract` 均复用
//!   `infra/local_engine/runtime` 中的已有类型，不复制第二套同义类型。
//! - **安装计划受限引用**：descriptor 不直接持有可执行路径或 argv，
//!   只引用由 provider 管理的 artifact/lock 等标识。
//! - **不是任意进程托管器**：descriptor 不会暴露成接收外部字符串的通用入口。

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::identity::{ArtifactId, ComputePreference, ModelContract, ResolvedProfile, RuntimePlan};

use super::error::LocalEngineError;

// ── CapabilityKind ─────────────────────────────────────────────────────────

/// 引擎所属能力种类（闭合枚举，不接受前端自定义）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CapabilityKind {
    /// 语音转文字。
    Stt,
    /// 光学字符识别。
    Ocr,
}

impl std::fmt::Display for CapabilityKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Stt => f.write_str("stt"),
            Self::Ocr => f.write_str("ocr"),
        }
    }
}

// ── ServiceTransport ───────────────────────────────────────────────────────

/// 引擎服务业务面传输方式（0.22.7）。
///
/// 决定 `EngineManager` 如何做 health 验证、STT 请求走哪条通道：
/// - `Http`：本地 HTTP endpoint + token（现有 Python server 路径）。
/// - `StdioWorker`：常驻子进程 stdin/stdout NDJSON 协议（GGUF worker 路径）。
///
/// 闭合枚举——不提供前端自定义通道的能力。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTransport {
    #[default]
    Http,
    StdioWorker,
}

impl std::fmt::Display for ServiceTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Http => f.write_str("http"),
            Self::StdioWorker => f.write_str("stdio_worker"),
        }
    }
}

// ── LifecyclePolicy ────────────────────────────────────────────────────────

/// 引擎默认生命周期策略。
///
/// - `Manual`：用户手动启停；FunASR 沿用用户现有 auto_start_server。
/// - `OnDemand`：按需启动，空闲后自动停止；PP-OCRv6 默认策略。
/// - `KeepRunning`：保持运行，不自动停止。
/// - `StopAfterUse`：使用后立即停止。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecyclePolicy {
    Manual,
    OnDemand,
    KeepRunning,
    StopAfterUse,
}

impl std::fmt::Display for LifecyclePolicy {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Manual => f.write_str("manual"),
            Self::OnDemand => f.write_str("on_demand"),
            Self::KeepRunning => f.write_str("keep_running"),
            Self::StopAfterUse => f.write_str("stop_after_use"),
        }
    }
}

// ── InstallPlanRef ────────────────────────────────────────────────────────

/// 安装计划的受限引用。
///
/// descriptor 不直接持有可执行路径或 argv，只引用由 provider 管理的 artifact/lock 标识。
/// 这是"受限引用"——它引用 provider 已锁定的 artifact，不接收外部字符串注入。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstallPlanRef {
    /// 运行时种类（闭合枚举，决定使用哪个 provider）。
    pub runtime_kind: RuntimePlan,
    /// 此引擎使用的 artifact id 列表（provider 管理的锁定标识）。
    /// 例如 Python 引擎引用 python distribution artifact id。
    pub artifact_ids: Vec<ArtifactId>,
    /// descriptor 声明的候选 compute profile 列表（按优先级排序）。
    /// adapter + provider 从中解析出本机兼容的 profile。
    pub compute_candidates: Vec<ComputeCandidate>,
    /// 环境 schema 版本。
    pub schema_version: u32,
}

/// descriptor 声明的候选 compute profile。
///
/// 每个 candidate 声明一个 preference → profile 映射。
/// adapter 从中选出本机兼容且 self-test 通过的 profile。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ComputeCandidate {
    /// 用户偏好（auto/cpu/gpu_auto/cuda/vulkan/directml）。
    pub preference: ComputePreference,
    /// 对应的 profile 标识。
    pub profile_id: String,
    /// 对应的 artifact id（provider 管理的锁定标识）。
    pub artifact_id: ArtifactId,
}

// ── ResourceBudget ────────────────────────────────────────────────────────

/// 引擎资源预算提示。
///
/// 用于 UI 展示预计资源占用和清理影响，不做硬性限制。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBudget {
    /// 预计环境磁盘占用（MB），None 表示未知。
    pub estimated_env_disk_mb: Option<u64>,
    /// 预计模型磁盘占用（MB），None 表示未知。
    pub estimated_model_disk_mb: Option<u64>,
    /// 预计稳定工作集内存（MB），None 表示未知。
    pub estimated_stable_ram_mb: Option<u64>,
    /// 预计峰值内存（MB），None 表示未知。
    pub estimated_peak_ram_mb: Option<u64>,
}

impl Default for ResourceBudget {
    fn default() -> Self {
        Self {
            estimated_env_disk_mb: None,
            estimated_model_disk_mb: None,
            estimated_stable_ram_mb: None,
            estimated_peak_ram_mb: None,
        }
    }
}

// ── EngineTimeouts ─────────────────────────────────────────────────────────

/// 引擎超时与空闲 TTL 配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineTimeouts {
    /// 启动超时（等待 service ready）。
    pub start_timeout: Duration,
    /// 模型加载超时。
    pub model_load_timeout: Duration,
    /// 空闲 TTL（OnDemand 策略下，无请求后多久停止）。
    pub idle_ttl: Duration,
}

impl Default for EngineTimeouts {
    fn default() -> Self {
        Self {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(60),
            idle_ttl: Duration::from_secs(300),
        }
    }
}

// ── EngineDefinition ──────────────────────────────────────────────────────

/// 引擎描述符（静态事实声明，编译期内置）。
///
/// descriptor 声明引擎的稳定身份、能力种类、运行时种类、安装计划引用、
/// 模型契约、生命周期策略、超时/空闲 TTL 和资源预算。
///
/// **不接受前端动态传入**：descriptor 由 Rust 编译期声明，
/// 前端只能传 `engine_id` 与有限动作。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EngineDefinition {
    /// 稳定 engine id（编译期声明）。
    pub engine_id: super::identity::EngineId,
    /// 显示元数据。
    pub display: EngineDisplay,
    /// 能力种类（STT/OCR，闭合枚举）。
    pub capability_kind: CapabilityKind,
    /// 运行时种类（PythonVenv/ManagedBinary，闭合枚举）。
    pub runtime_kind: RuntimePlan,
    /// 服务业务面传输方式（0.22.7：HTTP 或 stdio worker）。
    #[serde(default)]
    pub service_transport: ServiceTransport,
    /// 安装计划受限引用。
    pub install_plan: InstallPlanRef,
    /// 模型契约（锁定模型身份，防止随安装时间漂移）。
    pub model_contract: ModelContract,
    /// 默认生命周期策略。
    pub lifecycle: LifecyclePolicy,
    /// 超时与空闲 TTL。
    pub timeouts: EngineTimeouts,
    /// 资源预算提示。
    pub resource_budget: ResourceBudget,
}

// ── EngineDisplay ──────────────────────────────────────────────────────────

/// 引擎显示元数据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineDisplay {
    /// 显示名称（如 "FunASR 语音识别"）。
    pub name: String,
    /// 简短描述。
    pub description: String,
    /// 图标标识（Lucide icon name，不使用 emoji）。
    pub icon: String,
    /// 版本号（引擎实现版本，非模型版本）。
    pub version: String,
}

// ── Descriptor validation ──────────────────────────────────────────────────

impl EngineDefinition {
    /// 校验 descriptor 内部一致性。
    ///
    /// 确保安装计划引用的 runtime_kind 与 descriptor 的 runtime_kind 一致，
    /// 且候选 profile 中不出现 descriptor 未声明的 artifact。
    pub fn validate(&self) -> Result<(), LocalEngineError> {
        // runtime_kind 一致性
        if self.install_plan.runtime_kind != self.runtime_kind {
            return Err(LocalEngineError::with_detail(
                super::error::LocalEngineErrorCode::InvalidConfig,
                super::error::ErrorPhase::Config,
                "引擎配置不一致",
                format!(
                    "install_plan runtime_kind ({}) != descriptor runtime_kind ({})",
                    self.install_plan.runtime_kind, self.runtime_kind
                ),
            ));
        }

        // 候选 profile 的 artifact 必须在 install_plan.artifact_ids 中
        for candidate in &self.install_plan.compute_candidates {
            if !self
                .install_plan
                .artifact_ids
                .contains(&candidate.artifact_id)
            {
                return Err(LocalEngineError::with_detail(
                    super::error::LocalEngineErrorCode::InvalidConfig,
                    super::error::ErrorPhase::Config,
                    "引擎配置不一致",
                    format!(
                        "compute candidate '{}' 引用了未声明的 artifact_id '{}'",
                        candidate.profile_id, candidate.artifact_id
                    ),
                ));
            }
        }

        Ok(())
    }

    /// 检查此 descriptor 是否声明了给定的 compute preference。
    pub fn has_preference(&self, pref: ComputePreference) -> bool {
        self.install_plan
            .compute_candidates
            .iter()
            .any(|c| c.preference == pref)
    }

    /// 检查 resolved profile 是否在此 descriptor 声明的候选范围内。
    pub fn is_profile_allowed(&self, resolved: &ResolvedProfile) -> bool {
        self.install_plan
            .compute_candidates
            .iter()
            .any(|c| c.profile_id == resolved.profile_id)
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::identity::{
        ArtifactId, ComputeBackend, ComputePreference, EngineId, ModelContract, ResolvedProfile,
        RuntimePlan,
    };

    fn make_test_descriptor() -> EngineDefinition {
        let artifact_id = ArtifactId::new("python-3.12.8").unwrap();

        EngineDefinition {
            engine_id: EngineId::new("funasr").unwrap(),
            display: EngineDisplay {
                name: "FunASR 语音识别".to_string(),
                description: "本地 FunASR 语音转文字".to_string(),
                icon: "mic".to_string(),
                version: "0.1.0".to_string(),
            },
            capability_kind: CapabilityKind::Stt,
            runtime_kind: RuntimePlan::PythonVenv,
            service_transport: ServiceTransport::Http,
            install_plan: InstallPlanRef {
                runtime_kind: RuntimePlan::PythonVenv,
                artifact_ids: vec![artifact_id.clone()],
                compute_candidates: vec![ComputeCandidate {
                    preference: ComputePreference::Cpu,
                    profile_id: "cpu-x64".to_string(),
                    artifact_id: artifact_id.clone(),
                }],
                schema_version: 1,
            },
            model_contract: ModelContract {
                model_id: "funasr-model".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: crate::domain::local_engine::identity::ChecksumSource::Unverified,
            },
            lifecycle: LifecyclePolicy::Manual,
            timeouts: EngineTimeouts::default(),
            resource_budget: ResourceBudget::default(),
        }
    }

    #[test]
    fn descriptor_validates_ok() {
        let desc = make_test_descriptor();
        assert!(desc.validate().is_ok());
    }

    #[test]
    fn descriptor_rejects_mismatched_runtime_kind() {
        let mut desc = make_test_descriptor();
        desc.install_plan.runtime_kind = RuntimePlan::ManagedBinary;
        let err = desc.validate().unwrap_err();
        assert_eq!(
            err.code,
            super::super::error::LocalEngineErrorCode::InvalidConfig
        );
    }

    #[test]
    fn descriptor_rejects_undeclared_artifact_in_candidate() {
        let mut desc = make_test_descriptor();
        let undeclared = ArtifactId::new("undeclared-artifact").unwrap();
        desc.install_plan.compute_candidates.push(ComputeCandidate {
            preference: ComputePreference::Cuda,
            profile_id: "cuda-sm86".to_string(),
            artifact_id: undeclared,
        });
        assert!(desc.validate().is_err());
    }

    #[test]
    fn descriptor_only_allows_closed_capability_kind() {
        // CapabilityKind 只能是 Stt 或 Ocr，serde 反序列化验证
        assert_eq!(
            serde_json::to_string(&CapabilityKind::Stt).unwrap(),
            "\"stt\""
        );
        assert_eq!(
            serde_json::to_string(&CapabilityKind::Ocr).unwrap(),
            "\"ocr\""
        );

        // 反序列化不接受未知值
        let result: Result<CapabilityKind, _> = serde_json::from_str("\"tts\"");
        assert!(result.is_err());
    }

    #[test]
    fn descriptor_only_allows_closed_runtime_kind() {
        let json = serde_json::to_string(&RuntimePlan::PythonVenv).unwrap();
        assert_eq!(json, "\"python_venv\"");

        let result: Result<RuntimePlan, _> = serde_json::from_str("\"custom_runtime\"");
        assert!(result.is_err());
    }

    #[test]
    fn descriptor_has_preference_checks() {
        let desc = make_test_descriptor();
        assert!(desc.has_preference(ComputePreference::Cpu));
        assert!(!desc.has_preference(ComputePreference::Cuda));
    }

    #[test]
    fn descriptor_is_profile_allowed() {
        let desc = make_test_descriptor();

        let allowed = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(desc.is_profile_allowed(&allowed));

        let not_allowed = ResolvedProfile {
            profile_id: "cuda-sm86".to_string(),
            backend: ComputeBackend::Cuda,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(!desc.is_profile_allowed(&not_allowed));
    }

    #[test]
    fn descriptor_serialization_roundtrip() {
        let desc = make_test_descriptor();
        let json = serde_json::to_string(&desc).unwrap();
        let back: EngineDefinition = serde_json::from_str(&json).unwrap();
        assert_eq!(back.engine_id, desc.engine_id);
        assert_eq!(back.capability_kind, CapabilityKind::Stt);
        assert_eq!(back.runtime_kind, RuntimePlan::PythonVenv);
        assert_eq!(back.lifecycle, LifecyclePolicy::Manual);
    }
}
