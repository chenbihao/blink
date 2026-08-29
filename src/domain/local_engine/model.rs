//! 本地引擎模型资产生命命周期协议（0.22.6 H3）。
//!
//! 定义通用、闭合的 `EngineModelDescriptor` / `EngineModelStatus` 协议，
//! 让 Blink 统一管理同一引擎下多个受限模型候选（如 FunASR 的
//! SenseVoice Small 和 Paraformer-zh）。
//!
//! ## 设计铁则
//!
//! - **模型身份为 `engine_id + model_id`**：不再用单一 `local_model_id`
//!   假设。所有模型管理操作都以联合身份寻址。
//! - **descriptor 不得静态写死单一模型契约**：`EngineDefinition` 的
//!   `model_contract` 只作为默认/回退契约；实际期望模型来自本次
//!   受限启动配置或模型 descriptor。
//! - **前端不提交 URL、任意路径、脚本或外部命令**：模型安装是
//!   真实事务（staging/下载/校验/提升），不接受前端注入的下载源。
//! - **三类状态分离**：`installed`（已下载校验）、`selected`（用户
//!   配置选择）、`active`（进程 health 实际回报）必须独立表达。
//! - **删除引用保护**：删除正在使用或被配置引用的模型必须返回
//!   结构化冲突，不能静默切换。
//!
//! ## 分层归属
//!
//! - `domain/local_engine/model.rs`：纯数据协议 + 状态机逻辑，
//!   不发送 Tauri 事件，不接触 infra。
//! - `app/local_engine/model_service.rs`：编排模型安装/修复/删除事务，
//!   调用 adapter + infra 执行实际操作。

use serde::{Deserialize, Serialize};

use super::identity::{ChecksumSource, EngineId};

use super::error::{ErrorPhase, LocalEngineError, LocalEngineErrorCode};

// ── EngineModelDescriptor ──────────────────────────────────────────────────

/// 通用模型描述符（编译期内置 allowlist，不接受前端动态传入）。
///
/// 每个引擎在编译期声明自己支持的受限模型候选列表。
/// 例如 FunASR 声明 SenseVoice Small 和 Paraformer-zh 两个候选。
///
/// **不接受前端提交的 URL、路径或外部命令**：descriptor 只声明
/// 模型身份和校验契约，实际下载由 adapter/引擎层按自身机制完成。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineModelDescriptor {
    /// 所属引擎 id。
    pub engine_id: EngineId,
    /// 稳定模型 id（如 "iic/SenseVoiceSmall" / "paraformer-zh"）。
    pub model_id: String,
    /// 显示名称。
    pub display_name: String,
    /// 简短描述。
    pub description: String,
    /// 模型 revision（逻辑版本标识）。
    pub revision: String,
    /// checksum 来源（上游提供 SHA-256 时为校验值，否则记录来源）。
    pub checksum_source: ChecksumSource,
    /// 预计模型体积（MB），None 表示未知。
    pub estimated_size_mb: Option<u64>,
    /// 模型兼容性 schema 版本（用于校验服务 health 回报的模型身份）。
    pub compatibility_schema: u32,
}

impl EngineModelDescriptor {
    /// 构造 SenseVoice Small 模型 descriptor（FunASR 专用）。
    ///
    /// 此函数仅供 `funasr.rs` adapter 内部调用，不暴露给前端。
    pub fn sensevoice_small() -> Self {
        Self {
            engine_id: EngineId::new("funasr").expect("funasr is valid"),
            model_id: "iic/SenseVoiceSmall".to_string(),
            display_name: "SenseVoice Small".to_string(),
            description: "五语种 ASR（中/英/日/韩/粤），CPU 首选".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
            estimated_size_mb: Some(234),
            compatibility_schema: 1,
        }
    }

    /// 构造 Paraformer-zh 模型 descriptor（FunASR 专用）。
    ///
    /// 此函数仅供 `funasr.rs` adapter 内部调用，不暴露给前端。
    pub fn paraformer_zh() -> Self {
        Self {
            engine_id: EngineId::new("funasr").expect("funasr is valid"),
            model_id: "paraformer-zh".to_string(),
            display_name: "Paraformer-zh".to_string(),
            description: "SeacoParaformer 中文 ASR，原生支持热词".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
            estimated_size_mb: Some(234),
            compatibility_schema: 1,
        }
    }

    /// 校验 health 回报的模型身份是否与此 descriptor 匹配。
    ///
    /// 检查 `model_id`、`revision` 和（如果有）`content_fingerprint`。
    ///
    /// 返回 `Ok(())` 表示匹配，`Err` 表示不匹配且携带原因。
    pub fn verify_health_identity(
        &self,
        health_model_id: Option<&str>,
        health_revision: Option<&str>,
        health_fingerprint: Option<&str>,
    ) -> Result<ModelIdentityVerification, LocalEngineError> {
        let model_id_match = health_model_id == Some(self.model_id.as_str());

        let revision_match = health_revision == Some(self.revision.as_str());

        // fingerprint 是可选的：如果 health 回报了，则检查非空；
        // 如果 health 没回报（非 Ready 状态），则视为 None（不阻塞）。
        let fingerprint_ok = health_fingerprint.map(|fp| !fp.is_empty()).unwrap_or(true);

        if model_id_match && revision_match && fingerprint_ok {
            Ok(ModelIdentityVerification::Matched {
                model_id: self.model_id.clone(),
                revision: self.revision.clone(),
                fingerprint: health_fingerprint.map(|s| s.to_string()),
            })
        } else {
            Ok(ModelIdentityVerification::Mismatched {
                expected_model_id: self.model_id.clone(),
                expected_revision: self.revision.clone(),
                actual_model_id: health_model_id.map(|s| s.to_string()),
                actual_revision: health_revision.map(|s| s.to_string()),
                actual_fingerprint: health_fingerprint.map(|s| s.to_string()),
            })
        }
    }
}

/// 模型身份校验结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ModelIdentityVerification {
    /// 身份匹配。
    Matched {
        model_id: String,
        revision: String,
        fingerprint: Option<String>,
    },
    /// 身份不匹配。
    Mismatched {
        expected_model_id: String,
        expected_revision: String,
        actual_model_id: Option<String>,
        actual_revision: Option<String>,
        actual_fingerprint: Option<String>,
    },
}

impl ModelIdentityVerification {
    /// 是否匹配。
    pub fn is_matched(&self) -> bool {
        matches!(self, Self::Matched { .. })
    }
}

// ── EngineModelStatus ──────────────────────────────────────────────────────

/// 模型安装状态（独立于引擎进程/服务状态）。
///
/// 三类状态分离铁则：
/// - `installed`：模型资产已下载并校验，可被功能选择。
/// - `selected`：用户配置希望使用的 `engine_id + model_id`。
/// - `active`：当前运行实例 health 实际回报的模型身份。
///
/// 这三类状态必须独立表达，不得压缩成一个含混状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EngineModelStatus {
    /// 所属引擎 id。
    pub engine_id: EngineId,
    /// 模型 id。
    pub model_id: String,
    /// 安装状态。
    pub install_state: ModelInstallState,
    /// 校验状态。
    pub verification_state: ModelVerificationState,
    /// 缓存占用（bytes），None 表示未扫描。
    pub cache_size_bytes: Option<u64>,
    /// 是否被用户配置选择（selected）。
    pub is_selected: bool,
    /// 是否为当前进程实际模型（active，来自 health 回报）。
    pub is_active: bool,
    /// 兼容性状态。
    pub compatibility: ModelCompatibility,
}

impl EngineModelStatus {
    /// 创建未安装的初始状态。
    pub fn not_installed(descriptor: &EngineModelDescriptor) -> Self {
        Self {
            engine_id: descriptor.engine_id.clone(),
            model_id: descriptor.model_id.clone(),
            install_state: ModelInstallState::NotInstalled,
            verification_state: ModelVerificationState::Unknown,
            cache_size_bytes: None,
            is_selected: false,
            is_active: false,
            compatibility: ModelCompatibility::Unknown,
        }
    }

    /// 判断模型是否已安装且校验通过。
    pub fn is_usable(&self) -> bool {
        matches!(self.install_state, ModelInstallState::Installed)
            && matches!(
                self.verification_state,
                ModelVerificationState::Verified | ModelVerificationState::Unverified
            )
    }
}

// ── ModelInstallState ──────────────────────────────────────────────────────

/// 模型安装状态机。
///
/// 状态转移：
/// ```text
/// NotInstalled → Downloading → Staging → Verifying → Installed
///                    ↓             ↓          ↓
///               DownloadFailed  StagingFailed  VerificationFailed
///                    ↓             ↓          ↓
///                    └─────── NotInstalled ────┘
///
/// Installed → Repairing → Installed (or RepairFailed → Installed)
/// Installed → Deleting → NotInstalled (or DeleteBlocked if referenced)
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelInstallState {
    /// 未安装。
    NotInstalled,
    /// 下载中。
    Downloading,
    /// 已下载，暂存 staging（待校验/提升）。
    Staging,
    /// 校验中。
    Verifying,
    /// 已安装（已下载并校验通过）。
    Installed,
    /// 修复中。
    Repairing,
    /// 下载失败。
    DownloadFailed,
    /// 暂存失败。
    StagingFailed,
    /// 校验失败。
    VerificationFailed,
    /// 修复失败。
    RepairFailed,
    /// 删除中。
    Deleting,
    /// 删除被阻止（被引用）。
    DeleteBlocked,
}

impl Default for ModelInstallState {
    fn default() -> Self {
        Self::NotInstalled
    }
}

impl std::fmt::Display for ModelInstallState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotInstalled => f.write_str("not_installed"),
            Self::Downloading => f.write_str("downloading"),
            Self::Staging => f.write_str("staging"),
            Self::Verifying => f.write_str("verifying"),
            Self::Installed => f.write_str("installed"),
            Self::Repairing => f.write_str("repairing"),
            Self::DownloadFailed => f.write_str("download_failed"),
            Self::StagingFailed => f.write_str("staging_failed"),
            Self::VerificationFailed => f.write_str("verification_failed"),
            Self::RepairFailed => f.write_str("repair_failed"),
            Self::Deleting => f.write_str("deleting"),
            Self::DeleteBlocked => f.write_str("delete_blocked"),
        }
    }
}

impl ModelInstallState {
    /// 是否处于活跃操作中（不可并发启动新操作）。
    pub fn is_busy(&self) -> bool {
        matches!(
            self,
            Self::Downloading | Self::Staging | Self::Verifying | Self::Repairing | Self::Deleting
        )
    }

    /// 是否已安装。
    pub fn is_installed(&self) -> bool {
        matches!(self, Self::Installed)
    }

    /// 是否处于失败状态。
    pub fn is_failed(&self) -> bool {
        matches!(
            self,
            Self::DownloadFailed
                | Self::StagingFailed
                | Self::VerificationFailed
                | Self::RepairFailed
        )
    }
}

// ── ModelVerificationState ─────────────────────────────────────────────────

/// 模型校验状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVerificationState {
    /// 未知（尚未校验）。
    Unknown,
    /// 已校验通过（model_id + revision + fingerprint 匹配）。
    Verified,
    /// 校验失败（身份不匹配）。
    Mismatched,
    /// 上游不提供稳定 checksum，已记录来源但无法字节级校验。
    Unverified,
    /// 模型文件损坏。
    Corrupted,
}

impl Default for ModelVerificationState {
    fn default() -> Self {
        Self::Unknown
    }
}

impl std::fmt::Display for ModelVerificationState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::Verified => f.write_str("verified"),
            Self::Mismatched => f.write_str("mismatched"),
            Self::Unverified => f.write_str("unverified"),
            Self::Corrupted => f.write_str("corrupted"),
        }
    }
}

// ── ModelCompatibility ────────────────────────────────────────────────────

/// 模型与当前引擎环境的兼容性。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelCompatibility {
    /// 未知（尚未检查）。
    Unknown,
    /// 兼容。
    Compatible,
    /// 不兼容（如模型需要 GPU 但环境只有 CPU）。
    Incompatible { reason: String },
}

impl Default for ModelCompatibility {
    fn default() -> Self {
        Self::Unknown
    }
}

impl std::fmt::Display for ModelCompatibility {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unknown => f.write_str("unknown"),
            Self::Compatible => f.write_str("compatible"),
            Self::Incompatible { reason } => {
                write!(f, "incompatible:{reason}")
            }
        }
    }
}

// ── ModelOperation ────────────────────────────────────────────────────────

/// 模型长操作种类（闭合枚举）。
///
/// **Wire 格式（serde snake_case）**：`install`、`repair`、`delete`。
///
/// **i18n key 约定**：复用 `local_engine.operation.{wire_value}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOperationKind {
    /// 安装（下载 + 校验 + 提升）。
    Install,
    /// 修复（重新下载/校验损坏模型）。
    Repair,
    /// 删除。
    Delete,
}

impl std::fmt::Display for ModelOperationKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Install => f.write_str("install"),
            Self::Repair => f.write_str("repair"),
            Self::Delete => f.write_str("delete"),
        }
    }
}

/// 模型操作阶段（与引擎操作阶段正交）。
///
/// **Wire 格式（serde snake_case）**：`preparing`、`downloading`、`verifying`、
/// `promoting`、`cleaning`、`done`、`cancelled`、`failed`。
///
/// **i18n key 约定**：复用 `local_engine.operation.stage.{wire_value}`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelOperationStage {
    /// 准备中（创建 staging、解析下载源）。
    Preparing,
    /// 下载中。
    Downloading,
    /// 校验中。
    Verifying,
    /// 提升中（staging → 最终位置原子切换）。
    Promoting,
    /// 清理中（删除场景）。
    Cleaning,
    /// 已完成。
    Done,
    /// 已取消。
    Cancelled,
    /// 已失败。
    Failed,
}

impl std::fmt::Display for ModelOperationStage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Preparing => f.write_str("preparing"),
            Self::Downloading => f.write_str("downloading"),
            Self::Verifying => f.write_str("verifying"),
            Self::Promoting => f.write_str("promoting"),
            Self::Cleaning => f.write_str("cleaning"),
            Self::Done => f.write_str("done"),
            Self::Cancelled => f.write_str("cancelled"),
            Self::Failed => f.write_str("failed"),
        }
    }
}

// ── DeleteConflict ────────────────────────────────────────────────────────

/// 删除模型时的结构化冲突。
///
/// 删除正在使用或被配置引用的模型必须返回此结构，
/// 不能静默切换到其他模型。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelDeleteConflict {
    /// 被阻止删除的模型 engine_id。
    pub engine_id: EngineId,
    /// 被阻止删除的模型 model_id。
    pub model_id: String,
    /// 冲突原因列表。
    pub reasons: Vec<DeleteConflictReason>,
}

/// 单条删除冲突原因。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeleteConflictReason {
    /// 模型被当前 SttConfig/语音配置引用（selected）。
    ReferencedByConfig {
        /// 配置字段名（如 "funasr_model"）。
        config_field: String,
        /// 配置值。
        config_value: String,
    },
    /// 模型为当前运行实例的 active 模型（health 回报）。
    ActiveInRunningInstance {
        /// 实例 id。
        instance_id: String,
    },
    /// 模型被引擎 descriptor 作为默认契约引用。
    ReferencedByDescriptor {
        /// descriptor model_id。
        descriptor_model_id: String,
    },
}

impl ModelDeleteConflict {
    /// 从冲突构造 LocalEngineError。
    pub fn to_error(&self) -> LocalEngineError {
        let reasons_str = self
            .reasons
            .iter()
            .map(|r| match r {
                DeleteConflictReason::ReferencedByConfig {
                    config_field,
                    config_value,
                } => {
                    format!("配置 {config_field}={config_value} 引用了此模型")
                }
                DeleteConflictReason::ActiveInRunningInstance { instance_id } => {
                    format!("运行实例 {instance_id} 正在使用此模型")
                }
                DeleteConflictReason::ReferencedByDescriptor {
                    descriptor_model_id,
                } => {
                    format!("引擎 descriptor 默认模型 {descriptor_model_id} 引用了此模型")
                }
            })
            .collect::<Vec<_>>()
            .join("; ");

        LocalEngineError::with_detail(
            LocalEngineErrorCode::ArtifactReferenced,
            ErrorPhase::Cleanup,
            "模型被引用，无法删除",
            format!(
                "engine_id={}, model_id={}, 原因: {}",
                self.engine_id, self.model_id, reasons_str
            ),
        )
    }
}

// ── 模型状态机转移逻辑 ────────────────────────────────────────────────────

/// 模型状态机转移结果。
///
/// `Ok(new_state)` 表示转移成功；
/// `Err` 表示非法转移（如从 NotInstalled 直接到 Installed）。
pub fn transition_install_state(
    current: &ModelInstallState,
    target: ModelInstallState,
) -> Result<ModelInstallState, LocalEngineError> {
    use ModelInstallState::*;

    let allowed = match (current, &target) {
        // NotInstalled → Downloading / Deleting(无操作)
        (NotInstalled, Downloading) => true,
        (NotInstalled, NotInstalled) => true,

        // Downloading → Staging / DownloadFailed / NotInstalled(取消)
        (Downloading, Staging) => true,
        (Downloading, DownloadFailed) => true,
        (Downloading, NotInstalled) => true,

        // Staging → Verifying / StagingFailed / NotInstalled(取消)
        (Staging, Verifying) => true,
        (Staging, StagingFailed) => true,
        (Staging, NotInstalled) => true,

        // Verifying → Installed / VerificationFailed
        (Verifying, Installed) => true,
        (Verifying, VerificationFailed) => true,

        // DownloadFailed → Downloading(重试) / NotInstalled
        (DownloadFailed, Downloading) => true,
        (DownloadFailed, NotInstalled) => true,

        // StagingFailed → Downloading(重试) / NotInstalled
        (StagingFailed, Downloading) => true,
        (StagingFailed, NotInstalled) => true,

        // VerificationFailed → Downloading(重新下载) / NotInstalled
        (VerificationFailed, Downloading) => true,
        (VerificationFailed, NotInstalled) => true,

        // Installed → Repairing / Deleting / Installed
        (Installed, Repairing) => true,
        (Installed, Deleting) => true,
        (Installed, DeleteBlocked) => true,
        (Installed, Installed) => true,

        // Repairing → Installed / RepairFailed
        (Repairing, Installed) => true,
        (Repairing, RepairFailed) => true,

        // RepairFailed → Repairing(重试) / Installed
        (RepairFailed, Repairing) => true,
        (RepairFailed, Installed) => true,

        // Deleting → NotInstalled / DeleteBlocked
        (Deleting, NotInstalled) => true,
        (Deleting, DeleteBlocked) => true,

        // DeleteBlocked → Deleting(强制删除? 不允许) / Installed
        (DeleteBlocked, Installed) => true,

        // 同状态自环
        _ => current == &target,
    };

    if allowed {
        Ok(target)
    } else {
        Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::InvalidConfig,
            ErrorPhase::Config,
            "非法状态转移",
            format!("模型状态机不允许从 {current} 转移到 {target}"),
        ))
    }
}

// ── 模型操作请求/结果 ────────────────────────────────────────────────────

/// 模型操作请求（前端提交，闭合字段）。
///
/// **前端不提交 URL、任意路径、脚本或外部命令**。
/// 前端只提供 `engine_id`、`model_id` 和 `operation_id`（可选）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationRequest {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 操作 id（可选，用于取消关联）。
    pub operation_id: Option<String>,
}

/// 模型操作结果。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationResult {
    /// 引擎 id。
    pub engine_id: String,
    /// 模型 id。
    pub model_id: String,
    /// 操作 id。
    pub operation_id: String,
    /// 操作种类。
    pub operation_kind: ModelOperationKind,
    /// 最终阶段。
    pub final_stage: ModelOperationStage,
    /// 是否成功。
    pub success: bool,
    /// 错误信息（如果有）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<LocalEngineError>,
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── EngineModelDescriptor ─────────────────────────────────────────────

    #[test]
    fn sensevoice_small_descriptor_has_correct_identity() {
        let desc = EngineModelDescriptor::sensevoice_small();
        assert_eq!(desc.engine_id.as_str(), "funasr");
        assert_eq!(desc.model_id, "iic/SenseVoiceSmall");
        assert_eq!(desc.revision, "funasr-1.x");
        assert!(desc.estimated_size_mb.is_some());
    }

    #[test]
    fn paraformer_zh_descriptor_has_correct_identity() {
        let desc = EngineModelDescriptor::paraformer_zh();
        assert_eq!(desc.engine_id.as_str(), "funasr");
        assert_eq!(desc.model_id, "paraformer-zh");
        assert_eq!(desc.revision, "funasr-1.x");
    }

    #[test]
    fn descriptor_serialization_roundtrip() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let json = serde_json::to_string(&desc).unwrap();
        let back: EngineModelDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back.model_id, desc.model_id);
        assert_eq!(back.revision, desc.revision);
    }

    // ── health 身份校验 ──────────────────────────────────────────────────

    #[test]
    fn verify_health_identity_matches_when_all_fields_present() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let result = desc.verify_health_identity(
            Some("iic/SenseVoiceSmall"),
            Some("funasr-1.x"),
            Some("abc123fingerprint"),
        );
        assert!(result.unwrap().is_matched());
    }

    #[test]
    fn verify_health_identity_mismatches_on_wrong_model_id() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let result = desc.verify_health_identity(Some("paraformer-zh"), Some("funasr-1.x"), None);
        let v = result.unwrap();
        assert!(!v.is_matched());
    }

    #[test]
    fn verify_health_identity_mismatches_on_wrong_revision() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let result = desc.verify_health_identity(Some("iic/SenseVoiceSmall"), Some("v2.0"), None);
        assert!(!result.unwrap().is_matched());
    }

    #[test]
    fn verify_health_identity_rejects_empty_fingerprint() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let result = desc.verify_health_identity(
            Some("iic/SenseVoiceSmall"),
            Some("funasr-1.x"),
            Some(""), // 空 fingerprint
        );
        assert!(!result.unwrap().is_matched());
    }

    #[test]
    fn verify_health_identity_accepts_missing_fingerprint() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let result = desc.verify_health_identity(
            Some("iic/SenseVoiceSmall"),
            Some("funasr-1.x"),
            None, // fingerprint 缺失（非 Ready 状态）
        );
        assert!(result.unwrap().is_matched());
    }

    #[test]
    fn paraformer_health_identity_verification() {
        let desc = EngineModelDescriptor::paraformer_zh();
        let result = desc.verify_health_identity(
            Some("paraformer-zh"),
            Some("funasr-1.x"),
            Some("para123fingerprint"),
        );
        assert!(result.unwrap().is_matched());
    }

    // ── ModelInstallState 状态机 ──────────────────────────────────────────

    #[test]
    fn state_machine_not_installed_to_downloading() {
        let result = transition_install_state(
            &ModelInstallState::NotInstalled,
            ModelInstallState::Downloading,
        );
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), ModelInstallState::Downloading);
    }

    #[test]
    fn state_machine_downloading_to_staging() {
        let result =
            transition_install_state(&ModelInstallState::Downloading, ModelInstallState::Staging);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_staging_to_verifying() {
        let result =
            transition_install_state(&ModelInstallState::Staging, ModelInstallState::Verifying);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_verifying_to_installed() {
        let result =
            transition_install_state(&ModelInstallState::Verifying, ModelInstallState::Installed);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_not_installed_to_installed_rejected() {
        // 不能跳过中间步骤
        let result = transition_install_state(
            &ModelInstallState::NotInstalled,
            ModelInstallState::Installed,
        );
        assert!(result.is_err());
    }

    #[test]
    fn state_machine_installed_to_deleting() {
        let result =
            transition_install_state(&ModelInstallState::Installed, ModelInstallState::Deleting);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_deleting_to_not_installed() {
        let result = transition_install_state(
            &ModelInstallState::Deleting,
            ModelInstallState::NotInstalled,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_installed_to_repairing() {
        let result =
            transition_install_state(&ModelInstallState::Installed, ModelInstallState::Repairing);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_downloading_to_download_failed() {
        let result = transition_install_state(
            &ModelInstallState::Downloading,
            ModelInstallState::DownloadFailed,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_download_failed_to_downloading_retry() {
        let result = transition_install_state(
            &ModelInstallState::DownloadFailed,
            ModelInstallState::Downloading,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_downloading_cancel_to_not_installed() {
        let result = transition_install_state(
            &ModelInstallState::Downloading,
            ModelInstallState::NotInstalled,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_staging_cancel_to_not_installed() {
        let result =
            transition_install_state(&ModelInstallState::Staging, ModelInstallState::NotInstalled);
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_deleting_to_delete_blocked() {
        let result = transition_install_state(
            &ModelInstallState::Installed,
            ModelInstallState::DeleteBlocked,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_delete_blocked_to_installed() {
        let result = transition_install_state(
            &ModelInstallState::DeleteBlocked,
            ModelInstallState::Installed,
        );
        assert!(result.is_ok());
    }

    #[test]
    fn state_machine_busy_states() {
        assert!(ModelInstallState::Downloading.is_busy());
        assert!(ModelInstallState::Staging.is_busy());
        assert!(ModelInstallState::Verifying.is_busy());
        assert!(ModelInstallState::Repairing.is_busy());
        assert!(ModelInstallState::Deleting.is_busy());
        assert!(!ModelInstallState::NotInstalled.is_busy());
        assert!(!ModelInstallState::Installed.is_busy());
        assert!(!ModelInstallState::DownloadFailed.is_busy());
    }

    #[test]
    fn state_machine_failed_states() {
        assert!(ModelInstallState::DownloadFailed.is_failed());
        assert!(ModelInstallState::StagingFailed.is_failed());
        assert!(ModelInstallState::VerificationFailed.is_failed());
        assert!(ModelInstallState::RepairFailed.is_failed());
        assert!(!ModelInstallState::Installed.is_failed());
        assert!(!ModelInstallState::NotInstalled.is_failed());
    }

    #[test]
    fn state_machine_installed_state() {
        assert!(ModelInstallState::Installed.is_installed());
        assert!(!ModelInstallState::NotInstalled.is_installed());
        assert!(!ModelInstallState::Downloading.is_installed());
    }

    // ── DeleteConflict ──────────────────────────────────────────────────

    #[test]
    fn delete_conflict_to_error_has_correct_code() {
        let conflict = ModelDeleteConflict {
            engine_id: EngineId::new("funasr").unwrap(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            reasons: vec![
                DeleteConflictReason::ReferencedByConfig {
                    config_field: "funasr_model".to_string(),
                    config_value: "iic/SenseVoiceSmall".to_string(),
                },
                DeleteConflictReason::ActiveInRunningInstance {
                    instance_id: "inst-abc".to_string(),
                },
            ],
        };
        let err = conflict.to_error();
        assert_eq!(err.code, LocalEngineErrorCode::ArtifactReferenced);
        assert_eq!(err.phase, ErrorPhase::Cleanup);
        assert!(err.detail.contains("funasr_model"));
        assert!(err.detail.contains("inst-abc"));
    }

    #[test]
    fn delete_conflict_with_descriptor_reference() {
        let conflict = ModelDeleteConflict {
            engine_id: EngineId::new("funasr").unwrap(),
            model_id: "iic/SenseVoiceSmall".to_string(),
            reasons: vec![DeleteConflictReason::ReferencedByDescriptor {
                descriptor_model_id: "iic/SenseVoiceSmall".to_string(),
            }],
        };
        let err = conflict.to_error();
        assert_eq!(err.code, LocalEngineErrorCode::ArtifactReferenced);
    }

    // ── EngineModelStatus ───────────────────────────────────────────────

    #[test]
    fn model_status_not_installed_initial() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let status = EngineModelStatus::not_installed(&desc);
        assert_eq!(status.install_state, ModelInstallState::NotInstalled);
        assert!(!status.is_selected);
        assert!(!status.is_active);
        assert!(!status.is_usable());
    }

    #[test]
    fn model_status_is_usable_when_installed_and_verified() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Verified;
        assert!(status.is_usable());
    }

    #[test]
    fn model_status_is_usable_when_installed_and_unverified() {
        // 上游不提供 checksum 的模型（如 FunASR），Unverified 也视为可用
        let desc = EngineModelDescriptor::sensevoice_small();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Unverified;
        assert!(status.is_usable());
    }

    #[test]
    fn model_status_not_usable_when_mismatched() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Mismatched;
        assert!(!status.is_usable());
    }

    #[test]
    fn model_status_not_usable_when_corrupted() {
        let desc = EngineModelDescriptor::sensevoice_small();
        let mut status = EngineModelStatus::not_installed(&desc);
        status.install_state = ModelInstallState::Installed;
        status.verification_state = ModelVerificationState::Corrupted;
        assert!(!status.is_usable());
    }

    // ── 序列化 ──────────────────────────────────────────────────────────

    #[test]
    fn install_state_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ModelInstallState::NotInstalled).unwrap(),
            "\"not_installed\""
        );
        assert_eq!(
            serde_json::to_string(&ModelInstallState::DownloadFailed).unwrap(),
            "\"download_failed\""
        );
        assert_eq!(
            serde_json::to_string(&ModelInstallState::DeleteBlocked).unwrap(),
            "\"delete_blocked\""
        );
    }

    #[test]
    fn verification_state_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ModelVerificationState::Unknown).unwrap(),
            "\"unknown\""
        );
        assert_eq!(
            serde_json::to_string(&ModelVerificationState::Mismatched).unwrap(),
            "\"mismatched\""
        );
    }

    #[test]
    fn operation_kind_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ModelOperationKind::Install).unwrap(),
            "\"install\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationKind::Delete).unwrap(),
            "\"delete\""
        );
    }

    #[test]
    fn operation_stage_serde_snake_case() {
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Preparing).unwrap(),
            "\"preparing\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Cancelled).unwrap(),
            "\"cancelled\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Failed).unwrap(),
            "\"failed\""
        );
    }

    // ── Wire 格式稳定性测试（0.22.6 H5）──────────────────────────────────

    #[test]
    fn model_operation_stage_all_wire_values_stable() {
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Preparing).unwrap(),
            "\"preparing\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Downloading).unwrap(),
            "\"downloading\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Verifying).unwrap(),
            "\"verifying\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Promoting).unwrap(),
            "\"promoting\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Cleaning).unwrap(),
            "\"cleaning\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Done).unwrap(),
            "\"done\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Cancelled).unwrap(),
            "\"cancelled\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationStage::Failed).unwrap(),
            "\"failed\""
        );
    }

    #[test]
    fn model_operation_kind_all_wire_values_stable() {
        assert_eq!(
            serde_json::to_string(&ModelOperationKind::Install).unwrap(),
            "\"install\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationKind::Repair).unwrap(),
            "\"repair\""
        );
        assert_eq!(
            serde_json::to_string(&ModelOperationKind::Delete).unwrap(),
            "\"delete\""
        );
    }

    #[test]
    fn model_operation_stage_display_matches_wire() {
        assert_eq!(ModelOperationStage::Preparing.to_string(), "preparing");
        assert_eq!(ModelOperationStage::Downloading.to_string(), "downloading");
        assert_eq!(ModelOperationStage::Verifying.to_string(), "verifying");
        assert_eq!(ModelOperationStage::Promoting.to_string(), "promoting");
        assert_eq!(ModelOperationStage::Cleaning.to_string(), "cleaning");
        assert_eq!(ModelOperationStage::Done.to_string(), "done");
        assert_eq!(ModelOperationStage::Cancelled.to_string(), "cancelled");
        assert_eq!(ModelOperationStage::Failed.to_string(), "failed");
    }

    #[test]
    fn model_operation_kind_display_matches_wire() {
        assert_eq!(ModelOperationKind::Install.to_string(), "install");
        assert_eq!(ModelOperationKind::Repair.to_string(), "repair");
        assert_eq!(ModelOperationKind::Delete.to_string(), "delete");
    }
}
