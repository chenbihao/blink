//! 本地引擎领域错误（0.22.3）。
//!
//! 结构化错误模型：稳定 code + 发生阶段 + 可行动信息 + 内部上下文。
//! 不用 String 作为领域错误总协议，前端可按 code 分类展示。
//!
//! ## 设计铁则
//!
//! - **稳定 code**：`LocalEngineErrorCode` 是闭合枚举，前端据此分类展示
//!   （重试 / 配置缺失 / 权限 / 不支持 / 未知）。
//! - **发生阶段**：`phase` 记录错误发生在哪个生命周期阶段
//!   （install / start / health / stop / cleanup / config / self_test）。
//! - **可行动信息**：`action_hint` 给用户可操作的提示文案。
//! - **内部上下文**：`detail` 保留给开发者的诊断上下文，不暴露给前端。

use serde::{Deserialize, Serialize};

// ── LocalEngineError ──────────────────────────────────────────────────────

/// 本地引擎领域错误。
///
/// 可序列化，IPC 边界保留 `code` 字段供前端分类展示。
/// 不用 String 作为领域错误总协议。
#[derive(Debug, Clone, thiserror::Error, Serialize, Deserialize)]
pub struct LocalEngineError {
    /// 稳定错误码（前端分类展示的唯一依据）。
    pub code: LocalEngineErrorCode,
    /// 错误发生的生命周期阶段。
    pub phase: ErrorPhase,
    /// 用户可操作的行动提示。
    pub action_hint: String,
    /// 内部诊断上下文（开发者用，不展示给用户）。
    pub detail: String,
}

impl std::fmt::Display for LocalEngineError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "[{:?} / {:?}] {} ({})",
            self.code, self.phase, self.action_hint, self.detail
        )
    }
}

impl LocalEngineError {
    /// 带内部诊断上下文构造。
    pub fn with_detail(
        code: LocalEngineErrorCode,
        phase: ErrorPhase,
        action_hint: impl Into<String>,
        detail: impl Into<String>,
    ) -> Self {
        Self {
            code,
            phase,
            action_hint: action_hint.into(),
            detail: detail.into(),
        }
    }
}

// ── LocalEngineErrorCode ───────────────────────────────────────────────────

/// 稳定错误码（闭合枚举，前端分类展示的唯一依据）。
///
/// 不用 String，前端可据此选择重试 / 配置缺失 / 权限 / 不支持 / 未知。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LocalEngineErrorCode {
    /// 环境未安装。
    EnvironmentMissing,
    /// 环境损坏（manifest 不一致、文件缺失等）。
    EnvironmentBroken,
    /// 安装失败。
    InstallFailed,
    /// self-test 失败。
    SelfTestFailed,
    /// compute profile 无法解析。
    ProfileUnresolved,
    /// health 回报的 actual backend 与 resolved 不匹配。
    BackendMismatch,
    /// 清理失败。
    CleanupFailed,
    /// 共享 artifact 仍被引用，拒绝删除。
    ArtifactReferenced,
    /// 迁移失败。
    MigrationFailed,
    /// 进程已在运行。
    AlreadyRunning,
    /// 进程未运行。
    NotRunning,
    /// 进程 spawn 失败。
    SpawnFailed,
    /// 进程已退出。
    AlreadyExited,
    /// 停止失败。
    StopFailed,
    /// Job Object 分配失败。
    JobObjectFailed,
    /// 端口冲突（未知进程占用）。
    PortConflict,
    /// 身份验证失败，拒绝终止。
    IdentityVerification,
    /// 配置无效。
    InvalidConfig,
    /// 操作不支持（如 engine_id 不在 allowlist）。
    Unsupported,
    /// 磁盘空间不足。
    DiskFull,
    /// 网络不可达或下载失败。
    NetworkError,
    /// artifact/hash 损坏。
    ArtifactCorrupted,
    /// 操作被取消。
    Cancelled,
    /// 操作被拒绝（如迟到 operation_id）。
    Rejected,
    /// 服务不可达。
    ServiceUnreachable,
    /// 模型未就绪。
    ModelNotReady,
    /// 超时。
    Timeout,
    /// 内部错误（未分类）。
    Internal,
}

// ── ErrorPhase ────────────────────────────────────────────────────────────

/// 错误发生的生命周期阶段。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorPhase {
    /// 配置读取与校验。
    Config,
    /// 安装阶段。
    Install,
    /// 更新阶段。
    Update,
    /// 修复阶段。
    Repair,
    /// 迁移阶段。
    Migrate,
    /// 回滚阶段。
    Rollback,
    /// 启动阶段。
    Start,
    /// 健康检查阶段。
    Health,
    /// 停止阶段。
    Stop,
    /// 清理阶段。
    Cleanup,
    /// self-test 阶段。
    SelfTest,
    /// 请求/业务调用阶段。
    Request,
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_serialization_has_stable_fields() {
        let err = LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "请先安装 FunASR 环境",
            "current.json not found",
        );

        let json = serde_json::to_string(&err).unwrap();
        let back: LocalEngineError = serde_json::from_str(&json).unwrap();

        assert_eq!(back.code, LocalEngineErrorCode::EnvironmentMissing);
        assert_eq!(back.phase, ErrorPhase::Start);
        assert_eq!(back.action_hint, "请先安装 FunASR 环境");
        assert_eq!(back.detail, "current.json not found");
    }

    #[test]
    fn error_code_serde_snake_case() {
        let json = serde_json::to_string(&LocalEngineErrorCode::EnvironmentMissing).unwrap();
        assert_eq!(json, "\"environment_missing\"");

        let json = serde_json::to_string(&LocalEngineErrorCode::SelfTestFailed).unwrap();
        assert_eq!(json, "\"self_test_failed\"");

        let json = serde_json::to_string(&LocalEngineErrorCode::PortConflict).unwrap();
        assert_eq!(json, "\"port_conflict\"");
    }

    #[test]
    fn error_phase_serde_snake_case() {
        let json = serde_json::to_string(&ErrorPhase::SelfTest).unwrap();
        assert_eq!(json, "\"self_test\"");
    }

    #[test]
    fn error_display_contains_code_and_phase() {
        let err = LocalEngineError::with_detail(
            LocalEngineErrorCode::Timeout,
            ErrorPhase::Health,
            "健康检查超时",
            "",
        );
        let display = format!("{err}");
        assert!(display.contains("Timeout"));
        assert!(display.contains("Health"));
    }
}
