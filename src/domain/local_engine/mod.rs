//! 本地引擎领域协议。
//!
//! 定义 provider-neutral、框架无关的本地引擎身份、状态、描述符、操作协调、
//! 错误分类和纯状态提交逻辑。
//!
//! ## 分层归属
//!
//! - `domain/local_engine`：稳定 id、声明、状态类型、操作协调器、错误分类、
//!   生命周期策略和引擎特有的启动/健康适配接口；**不依赖 infra、不发送
//!   Tauri 事件、不持有 AppHandle、不直接使用 windows crate**。
//! - `infra/local_engine`：启动/停止子进程、排空管道、PID 身份验证、端口探测、
//!   部署事务与模型资产事务；身份类型从本模块引用（re-export 保持兼容）。
//! - `app/local_engine`：`EngineManager` 读取配置、串行化状态、调用
//!   adapter + infra、持有运行实例、广播事件。
//!
//! ## 身份类型唯一定义
//!
//! `EngineId`、`ModelId`、`ArtifactId`、`RuntimePlan`、`ComputePreference`
//! 系列、`ModelContract`、`Endpoint` 等定义在 `identity`——infra 通过
//! re-export 引用，不复制第二套同义类型。
//!
//! ## 接口约定
//!
//! - **app/local_engine**：`EngineManager` 负责调用 adapter 的
//!   `prepare_launch` 产生 `LaunchDescriptor`，转换为 infra 的 `LaunchRequest`
//!   后交给 `ManagedProcess` 执行。`EngineManager` 持有 `EngineStatus`
//!   并通过 `StatusCommitGuard` 验证提交。事件发布由 app 层桥接。
//! - **业务接入**：各引擎 adapter 实现（如 `FunasrAdapter`、
//!   `PaddleOcrAdapter`）在 app 层编译期注册，前端只传 `engine_id` 和
//!   有限动作（install/start/stop/repair/cleanup）。

pub mod adapter;
pub mod descriptor;
pub mod error;
pub mod identity;
pub mod model;
pub mod operation;
pub mod status;

// 领域层公共类型重导出
#[allow(unused_imports)]
pub use adapter::{
    AdapterConfig, AdapterSelfTest, DiagnosticEntry, EngineDiagnostic, HealthMapping,
    LaunchContext, LaunchDescriptor, LocalEngineAdapter, ResolvedLaunch,
};
#[allow(unused_imports)]
pub use descriptor::{
    CapabilityKind, CleanupPolicy, ComputeCandidate, EngineDefinition, EngineDisplay,
    EngineTimeouts, InstallPlanRef, LifecyclePolicy, ResourceBudget,
};
#[allow(unused_imports)]
pub use error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};
#[allow(unused_imports)]
pub use identity::{
    ArtifactId, ArtifactIdentity, BackendObservation, BackendState, BackendVerificationResult,
    ChecksumSource, ComputeBackend, ComputePreference, Endpoint, EngineId, FallbackReason,
    FallbackReasonKind, ModelContract, ModelId, ResolvedProfile, RuntimePlan,
    verify_backend_consistency,
};
#[allow(unused_imports)]
pub use model::{
    DeleteConflictReason, EngineModelDescriptor, EngineModelStatus, ModelCompatibility,
    ModelDeleteConflict, ModelIdentityVerification, ModelInstallState, ModelOperationKind,
    ModelOperationRequest, ModelOperationResult, ModelOperationStage, ModelVerificationState,
    transition_install_state,
};
#[allow(unused_imports)]
pub use operation::{CancelOutcome, EngineOperationCoordinator, OperationGuard};
#[allow(unused_imports)]
pub use status::{
    BackendInfo, DesiredState, EngineOperation, EngineStatus, EngineStatusSnapshot,
    EnvOperationEndState, EnvironmentHealth, FallbackEntry, FallbackOutcome, ModelHealth,
    OperationKind, OperationStage, ProcessState, ServiceEpoch, ServiceHealth, StatusCommitGuard,
};

// ── 领域层纯逻辑测试 ──────────────────────────────────────────────────────

#[cfg(test)]
mod domain_tests {
    use super::*;

    /// 测试：domain/local_engine 不 use infra/tauri/windows crate。
    /// 此测试是编译期保证——如果本模块引用了 infra::/tauri::/windows::，
    /// 不会在此测试失败，而是会在验收阶段被 rg 发现。
    /// 这里保留一个逻辑测试验证状态正交性。
    #[test]
    fn desired_and_observed_are_orthogonal() {
        // 所有 desired × process 组合都可表达
        for desired in [DesiredState::Stopped, DesiredState::Running] {
            for process in [
                ProcessState::Stopped,
                ProcessState::Starting,
                ProcessState::Running { pid: 1 },
                ProcessState::Stopping,
                ProcessState::Exited {
                    reason: "test".to_string(),
                },
            ] {
                let status = EngineStatus {
                    desired,
                    process: process.clone(),
                    ..Default::default()
                };
                // desired 和 process 可以独立表达
                assert_eq!(status.desired, desired);
                assert_eq!(status.process, process);
            }
        }
    }

    /// 测试：descriptor 只允许闭合 engine/profile。
    #[test]
    fn descriptor_only_allows_closed_engine_profile() {
        use descriptor::*;

        let artifact = ArtifactId::new("python-3.12.8").unwrap();
        let desc = EngineDefinition {
            engine_id: EngineId::new("funasr").unwrap(),
            display: EngineDisplay {
                name: "FunASR".to_string(),
                description: "STT".to_string(),
                icon: "mic".to_string(),
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
            model_contract: ModelContract {
                model_id: "test".to_string(),
                revision: "v1".to_string(),
                checksum_source: ChecksumSource::Unverified,
            },
            lifecycle: LifecyclePolicy::Manual,
            timeouts: EngineTimeouts::default(),
            resource_budget: ResourceBudget::default(),
            cleanup: CleanupPolicy::default(),
        };

        // 声明的 profile 允许
        let allowed = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(desc.is_profile_allowed(&allowed));

        // 未声明的 profile 拒绝
        let disallowed = ResolvedProfile {
            profile_id: "cuda-sm99".to_string(),
            backend: ComputeBackend::Cuda,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(!desc.is_profile_allowed(&disallowed));
    }
}
