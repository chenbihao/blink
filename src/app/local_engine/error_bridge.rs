//! infra 错误 → 领域错误桥接（app 层）。
//!
//! `LocalEngineError::from_runtime/from_process` 原先定义在 domain，
//! 但它们引用 infra 的 `RuntimeError`/`ManagedProcessError`——
//! domain 收敛为不依赖 infra 后，转换器上移到 app 层（app 同时依赖两层）。

use crate::domain::local_engine::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};
use crate::infra::local_engine::process::ManagedProcessError;
use crate::infra::local_engine::runtime::RuntimeError;
use crate::infra::local_engine::runtime::RuntimeError as RE;

/// 从 infra 层 RuntimeError 转换（保留内部上下文）。
pub fn from_runtime(
    phase: ErrorPhase,
    hint: impl Into<String>,
    err: &RuntimeError,
) -> LocalEngineError {
    let code = match err {
        // 配置/路径校验错误
        RE::PathTraversal { .. } => LocalEngineErrorCode::InvalidConfig,

        // 指针/manifest/journal 损坏 → EnvironmentBroken
        RE::CurrentPointerParseFailed { .. }
        | RE::TransactionJournalInvalid { .. }
        | RE::GenerationNotFound { .. }
        | RE::ManifestParseFailed { .. }
        | RE::ManifestSerializeFailed { .. }
        | RE::ManifestSchemaIncompatible { .. } => LocalEngineErrorCode::EnvironmentBroken,

        // staging/install 失败
        RE::StagingCreateFailed { .. }
        | RE::GenerationPromoteFailed { .. }
        | RE::CurrentPointerSwitchFailed { .. }
        | RE::InstallFailed { .. } => LocalEngineErrorCode::InstallFailed,

        // 磁盘不足
        RE::InsufficientDiskSpace { .. } => LocalEngineErrorCode::DiskFull,

        // self-test 失败
        RE::SelfTestFailed { .. } => LocalEngineErrorCode::SelfTestFailed,

        // 操作被取消
        RE::OperationCancelled { .. } => LocalEngineErrorCode::Cancelled,

        // profile 解析失败
        RE::ProfileResolutionFailed { .. } | RE::ExplicitBackendFailed { .. } => {
            LocalEngineErrorCode::ProfileUnresolved
        }

        // 清理失败
        RE::CleanupFailed { .. } => LocalEngineErrorCode::CleanupFailed,

        // 共享 artifact 仍被引用
        RE::ArtifactStillReferenced { .. } => LocalEngineErrorCode::ArtifactReferenced,

        // IO/JSON → Internal
        RE::Io(_) | RE::Json(_) => LocalEngineErrorCode::Internal,
    };

    LocalEngineError {
        code,
        phase,
        action_hint: hint.into(),
        detail: err.to_string(),
    }
}

/// 从 ManagedProcessError 转换。
pub fn from_process(
    phase: ErrorPhase,
    hint: impl Into<String>,
    err: &ManagedProcessError,
) -> LocalEngineError {
    let code = match err {
        ManagedProcessError::AlreadyRunning { .. } => LocalEngineErrorCode::AlreadyRunning,
        ManagedProcessError::NotRunning => LocalEngineErrorCode::NotRunning,
        ManagedProcessError::SpawnFailed { .. } => LocalEngineErrorCode::SpawnFailed,
        ManagedProcessError::AlreadyExited { .. } => LocalEngineErrorCode::AlreadyExited,
        ManagedProcessError::StopFailed { .. } => LocalEngineErrorCode::StopFailed,
        ManagedProcessError::JobObjectFailed { .. } => LocalEngineErrorCode::JobObjectFailed,
        ManagedProcessError::PortConflict { .. } => LocalEngineErrorCode::PortConflict,
        ManagedProcessError::IdentityVerificationFailed { .. } => {
            LocalEngineErrorCode::IdentityVerification
        }
        ManagedProcessError::InternalInconsistency { .. } => LocalEngineErrorCode::Internal,
    };

    LocalEngineError {
        code,
        phase,
        action_hint: hint.into(),
        detail: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_runtime_maps_codes() {
        let runtime_err = RuntimeError::GenerationNotFound {
            install_id: "dep-test".to_string(),
        };
        let err = from_runtime(ErrorPhase::Start, "环境缺失", &runtime_err);
        assert_eq!(err.code, LocalEngineErrorCode::EnvironmentBroken);

        let err = from_runtime(
            ErrorPhase::Install,
            "磁盘不足",
            &RuntimeError::InsufficientDiskSpace {
                message: "test".to_string(),
            },
        );
        assert_eq!(err.code, LocalEngineErrorCode::DiskFull);

        let err = from_runtime(
            ErrorPhase::Install,
            "操作取消",
            &RuntimeError::OperationCancelled {
                message: "test".to_string(),
            },
        );
        assert_eq!(err.code, LocalEngineErrorCode::Cancelled);

        let err = from_runtime(
            ErrorPhase::Rollback,
            "journal 损坏",
            &RuntimeError::TransactionJournalInvalid {
                message: "test".to_string(),
            },
        );
        assert_eq!(err.code, LocalEngineErrorCode::EnvironmentBroken);
    }

    #[test]
    fn from_process_maps_codes() {
        let proc_err = ManagedProcessError::NotRunning;
        let err = from_process(ErrorPhase::Stop, "进程未运行", &proc_err);
        assert_eq!(err.code, LocalEngineErrorCode::NotRunning);
    }
}
