//! 模型安装执行器与目录（从原 ModelService 拆分）。
//!
//! ModelService 已删除——模型资产业务编排（状态、事务、冲突检查、
//! selected/active 投影）统一由 `EngineManager` 承载（单一业务真相）。
//! 本模块只保留：
//! - `ModelRegistry`：编译期模型目录（allowlist）；
//! - `ModelInstallWorker`：下载执行器 trait + FunASR 实现（受管 venv python 驱动）；
//! - 安装 sink：有界缓冲 + 事件广播（operation_id 隔离）；
//! - 模型 DTO 与投影（commands 层使用）。
//!
//! 持久真源是磁盘 manifest（infra model_storage）；本模块无状态。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::local_engine::{
    EngineModelDescriptor, EngineModelStatus, LocalEngineErrorCode, ModelOperationResult,
    SttModelCapabilities,
};
use crate::infra::local_engine::runtime::EngineId;

// ── ModelRegistry ──────────────────────────────────────────────────────────

/// 编译期模型注册表（allowlist）。
///
/// 每个引擎在编译期声明自己支持的模型候选列表。
/// 不暴露动态注册 API——所有注册项在构造时确定。
pub struct ModelRegistry {
    /// engine_id → 模型 descriptor 列表
    models: HashMap<EngineId, Vec<EngineModelDescriptor>>,
}

impl Clone for ModelRegistry {
    fn clone(&self) -> Self {
        Self {
            models: self.models.clone(),
        }
    }
}

impl ModelRegistry {
    /// 创建带指定模型列表的注册表。
    pub fn new_with_models(models: Vec<EngineModelDescriptor>) -> Self {
        let mut map: HashMap<EngineId, Vec<EngineModelDescriptor>> = HashMap::new();
        for m in models {
            map.entry(m.engine_id.clone()).or_default().push(m);
        }
        Self { models: map }
    }

    /// 创建空注册表（测试用）。
    #[cfg(test)]
    pub fn empty() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// 查找引擎的所有模型候选。
    pub fn list(&self, engine_id: &EngineId) -> &[EngineModelDescriptor] {
        self.models
            .get(engine_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 查找特定模型。
    pub fn find(&self, engine_id: &EngineId, model_id: &str) -> Option<&EngineModelDescriptor> {
        self.models
            .get(engine_id)?
            .iter()
            .find(|m| m.model_id == model_id)
    }
}

// ── InstallSink（有界日志/阶段 sink）──────────────────────────────────────

/// 模型安装阶段的日志/进度 sink。
///
/// **铁则**：
/// - 有界：实现必须维护有界缓冲，禁止无限制累积日志。
/// - 阶段性：`emit_stage` 报告安装阶段（如 downloading/verifying），
///   但**不伪造下载百分比**——无法取得字节级进度时只报阶段。
/// - 不接收 URL、executable、argv、环境变量或脚本路径。
pub trait InstallSink: Send + Sync {
    /// 发射一条日志行。
    fn emit_log(&self, line: &str);

    /// 发射阶段变更。
    fn emit_stage(&self, stage: &str);
}

/// 有界内存日志 sink（用于测试和轻量诊断）。
///
/// 缓冲上限为 `max_lines`，超出后丢弃旧行。
pub struct BoundedInstallSink {
    lines: std::sync::Mutex<std::collections::VecDeque<String>>,
    max_lines: usize,
}

impl BoundedInstallSink {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(max_lines)),
            max_lines,
        }
    }
}

impl InstallSink for BoundedInstallSink {
    fn emit_log(&self, line: &str) {
        let mut buf = self.lines.lock().unwrap();
        if buf.len() >= self.max_lines {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
    }

    fn emit_stage(&self, stage: &str) {
        self.emit_log(&format!("[stage] {stage}"));
    }
}

impl BoundedInstallSink {
    /// 取缓冲尾部 n 行（用于失败时把 installer 真实输出附进错误详情）。
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let buf = self.lines.lock().unwrap();
        buf.iter().rev().take(n).rev().cloned().collect()
    }
}

/// 模型安装日志广播 sink——把 installer 输出桥接到 `EventPort`（前端实时事件）
/// 并缓冲到内部 `BoundedInstallSink`（失败诊断用）。
///
/// 与 service.rs 的 `InstallSinkAdapter`（引擎环境安装）语义一致：
/// - `emit_install_log` 以 `operation_id` 隔离，`instance_id` 为空，
///   前端按 `operation_id != null` 识别为操作日志（不做 instance 过滤）；
/// - installer 原始输出默认 debug 级 tracing，`[ERROR]`/`[WARN]` 前缀升级；
/// - 洪泛保护由内部缓冲上限与 installer 侧 `disable_progress_bar` 共同保证。
pub struct BroadcastingInstallSink {
    inner: BoundedInstallSink,
    event_port: Arc<dyn super::EventPort>,
    engine_id: EngineId,
    operation_id: String,
    log_seq: std::sync::atomic::AtomicU64,
}

impl BroadcastingInstallSink {
    pub fn new(
        inner: BoundedInstallSink,
        event_port: Arc<dyn super::EventPort>,
        engine_id: EngineId,
        operation_id: String,
    ) -> Self {
        Self {
            inner,
            event_port,
            engine_id,
            operation_id,
            log_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 取缓冲尾部 n 行（透传内部缓冲）。
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        self.inner.tail_lines(n)
    }
}

/// 从 installer 输出行推断日志级别。
///
/// stdout/stderr 只是传输通道：受信任 installer 的显式前缀优先，
/// 未分类输出归 info（前端展示）+ debug（tracing）。
fn classify_installer_line(line: &str) -> crate::app::local_engine::dto::EngineLogLevel {
    use crate::app::local_engine::dto::EngineLogLevel;
    if line.starts_with("[ERROR]") {
        EngineLogLevel::Error
    } else if line.starts_with("[WARN") || line.starts_with("WARNING") {
        EngineLogLevel::Warn
    } else {
        EngineLogLevel::Info
    }
}

impl InstallSink for BroadcastingInstallSink {
    fn emit_log(&self, line: &str) {
        self.inner.emit_log(line);

        let level = classify_installer_line(line);
        match level {
            crate::app::local_engine::dto::EngineLogLevel::Error => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            crate::app::local_engine::dto::EngineLogLevel::Warn => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            _ => tracing::debug!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
        }

        let seq = self
            .log_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.event_port
            .emit_install_log(&self.engine_id, &self.operation_id, seq, level, line);
    }

    fn emit_stage(&self, stage: &str) {
        self.inner.emit_stage(stage);
        tracing::debug!(
            engine_id = %self.engine_id,
            op = %self.operation_id,
            stage,
            "模型安装阶段变更"
        );
        self.event_port
            .emit_install_stage(&self.engine_id, &self.operation_id, stage);
    }
}

// ── ModelInstallWorker trait ────────────────────────────────────────────────

/// 模型安装 worker trait（installer port）。
///
/// 每个引擎 adapter 提供编译期固定的专用安装 worker。
/// worker 负责实际的模型下载（如 ModelScope/FunASR 官方库），
/// 下载结果写入指定的 staging payload 目录。
///
/// **铁则**：
/// - worker 只负责下载到 staging，不负责校验/提升/manifest
/// - worker 不接收前端提交的 URL、脚本路径、Python 路径
/// - worker 参数必须是 allowlist 中的 model id/revision
/// - worker 设置 MODELSCOPE_CACHE 为本次 staging 目录，禁止回落到用户默认缓存
/// - worker 必须作为受管进程运行，接入 CancellationToken 和超时
/// - worker 通过 `InstallSink` 报告有界日志和阶段，不伪造百分比
#[async_trait::async_trait]
pub trait ModelInstallWorker: Send + Sync {
    /// 下载模型到 staging payload 目录。
    ///
    /// 成功时返回下载来源描述（用于 manifest 的 source/provenance）。
    /// 失败时返回错误（Rust 会清理 staging）。
    ///
    /// `sink` 可选——worker 通过 sink 报告有界日志和安装阶段。
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError>;
}

/// 模型下载结果（worker 返回）。
#[derive(Debug, Clone)]
pub struct ModelDownloadOutcome {
    /// 下载来源描述（如 "modelscope:iic/SenseVoiceSmall"）。
    pub source: String,
    /// 下载来源的 checksum 信息。
    pub checksum_source: ModelDownloadChecksumSource,
}

/// 下载来源 checksum 信息。
#[derive(Debug, Clone)]
pub enum ModelDownloadChecksumSource {
    /// 上游提供稳定 SHA-256（GGUF 模型目录逐文件锁定）。
    Sha256(String),
}

/// 模型下载错误。
#[derive(Debug, thiserror::Error)]
pub enum ModelDownloadError {
    #[error("下载失败: {message}")]
    Failed { message: String },

    #[error("下载被取消")]
    Cancelled,

    /// worker 契约保留：installer 可区分磁盘/网络失败时无需改 trait。
    #[error("磁盘空间不足: {message}")]
    #[allow(dead_code)]
    DiskFull { message: String },

    #[error("网络不可达: {message}")]
    #[allow(dead_code)]
    Network { message: String },

    #[error("worker 内部错误: {message}")]
    Internal { message: String },
}

impl ModelDownloadError {
    /// 映射到 LocalEngineErrorCode。
    pub fn to_code(&self) -> LocalEngineErrorCode {
        match self {
            Self::Cancelled => LocalEngineErrorCode::Cancelled,
            Self::DiskFull { .. } => LocalEngineErrorCode::DiskFull,
            Self::Network { .. } => LocalEngineErrorCode::NetworkError,
            Self::Failed { .. } | Self::Internal { .. } => LocalEngineErrorCode::InstallFailed,
        }
    }
}

/// 空实现（B2 未完成时占位）。
#[cfg(test)]
pub struct NoopModelWorker;

#[cfg(test)]
#[async_trait::async_trait]
impl ModelInstallWorker for NoopModelWorker {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        _model_id: &str,
        _revision: &str,
        _staging_payload_dir: &std::path::Path,
        _cancel_token: CancellationToken,
        _sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        Err(ModelDownloadError::Internal {
            message: "NoopModelWorker: 模型下载未实现（等待 B2 FunASR worker）".to_string(),
        })
    }
}

// ── FunASR 注册（0.22.7.4：GGUF 模型目录为唯一实现）─────────────────────────

/// FunASR 模型目录：SenseVoice Q8 / Paraformer-zh Q8 / Fun-ASR-Nano Q4_K_M
/// （handoff-11：ParaformerOnline ONNX 已退役移除）。
///
/// 旧 Python 时代目录（iic/SenseVoiceSmall / paraformer-zh）已随 0.22.7.4
/// 切换移除；旧配置选择由 `SttConfig::migrate_selection_to_gguf` 确定迁移。
/// 退役 id（如 onnx/paraformer-online）不迁移——保持「未知模型不可用」语义。
pub fn make_funasr_model_registry() -> ModelRegistry {
    let models: Vec<EngineModelDescriptor> =
        crate::app::local_engine::funasr::gguf::gguf_model_specs()
            .iter()
            .map(crate::app::local_engine::funasr::gguf::gguf_model_descriptor)
            .collect();
    ModelRegistry::new_with_models(models)
}
/// - 可生成不同 revision/content（用于 repair 测试）
/// - 通过 sink 报告阶段日志
#[cfg(test)]
pub struct FakeInstaller {
    /// 是否成功下载。
    pub success: bool,
    /// 下载延迟（毫秒），>0 时模拟可取消的下载。
    pub delay_ms: u64,
    /// 写入 staging 的文件内容。
    pub file_content: Vec<u8>,
    /// 写入的文件名（默认 `model.bin`）。
    pub file_name: String,
    /// 下载来源描述。
    pub source: Option<String>,
    /// 下载 checksum 来源。
    pub checksum_source: Option<ModelDownloadChecksumSource>,
}

#[cfg(test)]
impl FakeInstaller {
    /// 创建成功写入固定 payload 的 installer。
    pub fn success() -> Self {
        Self {
            success: true,
            delay_ms: 0,
            file_content: b"fake model data".to_vec(),
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ModelInstallWorker for FakeInstaller {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        model_id: &str,
        _revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloading");
            s.emit_log(&format!("开始下载模型 {model_id}"));
        }

        if self.delay_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)) => {}
                _ = cancel_token.cancelled() => {
                    return Err(ModelDownloadError::Cancelled);
                }
            }
        }

        if cancel_token.is_cancelled() {
            return Err(ModelDownloadError::Cancelled);
        }

        if !self.success {
            return Err(ModelDownloadError::Failed {
                message: "fake download failure".to_string(),
            });
        }

        if let Some(s) = sink.as_deref() {
            s.emit_stage("writing");
            s.emit_log("写入 payload 文件");
        }

        let file_name = if self.file_name.is_empty() {
            "model.bin"
        } else {
            self.file_name.as_str()
        };
        std::fs::write(staging_payload_dir.join(file_name), &self.file_content).map_err(|e| {
            ModelDownloadError::Internal {
                message: e.to_string(),
            }
        })?;

        Ok(ModelDownloadOutcome {
            source: self
                .source
                .clone()
                .unwrap_or_else(|| format!("fake:{model_id}")),
            checksum_source: self
                .checksum_source
                .clone()
                .unwrap_or_else(|| ModelDownloadChecksumSource::Sha256("ab".repeat(32))),
        })
    }
}

// ── DTO ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogItemDto {
    pub engine_id: String,
    pub model_id: String,
    pub display_name: String,
    pub description: String,
    pub revision: String,
    pub estimated_size_mb: Option<u64>,
    pub install_state: String,
    pub verification_state: String,
    pub cache_size_bytes: Option<u64>,
    pub is_selected: bool,
    pub is_active: bool,
    pub compatibility: String,
    /// STT 模型的 per-model 能力声明（Handoff 02：DTO 驱动 UI）。
    ///
    /// 仅 STT 引擎填充；OCR 引擎为 default（全 unknown）。
    /// 前端据此决定流式等高级选项的可见性与可用性，
    /// 不再硬编码模型 id → 能力的映射。
    #[serde(default)]
    pub stt_capabilities: SttModelCapabilities,
    /// 模型业务画像（0.22.9 display-only）：中文质量 / 资源占用定位。
    /// None = 未声明（前端不展示该维度，不猜默认值）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub business: Option<crate::domain::local_engine::ModelBusinessProfile>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationRequestDto {
    pub engine_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationResultDto {
    pub engine_id: String,
    pub model_id: String,
    pub operation_id: String,
    pub operation_kind: String,
    pub final_stage: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

pub fn project_model_status(
    descriptor: &EngineModelDescriptor,
    status: &EngineModelStatus,
) -> ModelCatalogItemDto {
    ModelCatalogItemDto {
        engine_id: descriptor.engine_id.to_string(),
        model_id: descriptor.model_id.clone(),
        display_name: descriptor.display_name.clone(),
        description: descriptor.description.clone(),
        revision: descriptor.revision.clone(),
        estimated_size_mb: descriptor.estimated_size_mb,
        install_state: status.install_state.to_string(),
        verification_state: status.verification_state.to_string(),
        cache_size_bytes: status.cache_size_bytes,
        is_selected: status.is_selected,
        is_active: status.is_active,
        compatibility: status.compatibility.to_string(),
        stt_capabilities: descriptor.stt_capabilities.clone(),
        business: descriptor.business.clone(),
    }
}

pub fn project_model_operation_result(result: &ModelOperationResult) -> ModelOperationResultDto {
    ModelOperationResultDto {
        engine_id: result.engine_id.clone(),
        model_id: result.model_id.clone(),
        operation_id: result.operation_id.clone(),
        operation_kind: result.operation_kind.to_string(),
        final_stage: result.final_stage.to_string(),
        success: result.success,
        error: result
            .error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::local_engine::{
        CapabilityFlag, EngineModelDescriptor, EngineModelStatus, ModelInstallState,
        ModelVerificationState, SttModelCapabilities,
    };
    use crate::infra::local_engine::runtime::{ChecksumSource, EngineId};

    fn test_descriptor_with_caps(
        model_id: &str,
        caps: SttModelCapabilities,
    ) -> EngineModelDescriptor {
        EngineModelDescriptor {
            engine_id: EngineId::new("funasr").expect("funasr is valid"),
            model_id: model_id.to_string(),
            display_name: "Test Model".to_string(),
            description: "test".to_string(),
            revision: "v1".to_string(),
            checksum_source: ChecksumSource::Unverified,
            estimated_size_mb: Some(100),
            compatibility_schema: 1,
            stt_capabilities: caps,
            business: None,
        }
    }

    fn test_status(desc: &EngineModelDescriptor) -> EngineModelStatus {
        let mut st = EngineModelStatus::not_installed(desc);
        st.install_state = ModelInstallState::Installed;
        st.verification_state = ModelVerificationState::Unverified;
        st
    }

    #[test]
    fn dto_includes_stt_capabilities_in_json() {
        let caps = SttModelCapabilities {
            languages: vec!["zh".into(), "en".into()],
            pseudo_streaming: CapabilityFlag::yes(),
            true_streaming: CapabilityFlag::no("test.reason"),
            timestamps: CapabilityFlag::no("test.reason"),
            punctuation: CapabilityFlag::no("test.reason"),
        };
        let desc = test_descriptor_with_caps("test-model", caps);
        let status = test_status(&desc);
        let dto = project_model_status(&desc, &status);
        let json = serde_json::to_value(&dto).unwrap();

        // stt_capabilities 必须存在于 JSON 中
        assert!(json.get("stt_capabilities").is_some());
        let caps_json = &json["stt_capabilities"];

        // languages 数组
        assert_eq!(caps_json["languages"][0], "zh");
        assert_eq!(caps_json["languages"][1], "en");

        // pseudo_streaming: { supported: "yes" }
        assert_eq!(caps_json["pseudo_streaming"]["supported"], "yes");
        assert!(caps_json["pseudo_streaming"].get("reason").is_none());
    }

    #[test]
    fn dto_serializes_default_caps_for_ocr_engine() {
        // OCR 引擎的 descriptor 使用 default caps（全 unknown）
        let desc = EngineModelDescriptor {
            engine_id: EngineId::new("paddleocr").expect("paddleocr is valid"),
            model_id: "ppocrv6".to_string(),
            display_name: "PaddleOCR".to_string(),
            description: "OCR".to_string(),
            revision: "v1".to_string(),
            checksum_source: ChecksumSource::Unverified,
            estimated_size_mb: Some(50),
            compatibility_schema: 1,
            stt_capabilities: SttModelCapabilities::default(),
            business: None,
        };
        let status = test_status(&desc);
        let dto = project_model_status(&desc, &status);
        let json = serde_json::to_value(&dto).unwrap();

        // default caps 仍然序列化（所有能力为 No { reason: "unknown" }）
        assert!(json.get("stt_capabilities").is_some());
        assert_eq!(
            json["stt_capabilities"]["pseudo_streaming"]["supported"],
            "no"
        );
        assert_eq!(
            json["stt_capabilities"]["pseudo_streaming"]["reason"],
            "unknown"
        );
    }

    #[test]
    fn dto_round_trip_preserves_capabilities() {
        let caps = SttModelCapabilities {
            languages: vec!["zh".into()],
            pseudo_streaming: CapabilityFlag::yes(),
            true_streaming: CapabilityFlag::no("round.trip.test"),
            timestamps: CapabilityFlag::yes(),
            punctuation: CapabilityFlag::yes(),
        };
        let desc = test_descriptor_with_caps("round-trip", caps.clone());
        let status = test_status(&desc);
        let dto = project_model_status(&desc, &status);

        let json = serde_json::to_string(&dto).unwrap();
        let back: ModelCatalogItemDto = serde_json::from_str(&json).unwrap();

        assert_eq!(back.stt_capabilities, caps);
    }

    #[test]
    fn dto_backward_compatible_missing_caps_defaults() {
        // 旧前端/旧 JSON 不含 stt_capabilities 时，反序列化使用 default
        let json = serde_json::json!({
            "engine_id": "funasr",
            "model_id": "test",
            "display_name": "Test",
            "description": "test",
            "revision": "v1",
            "estimated_size_mb": 100,
            "install_state": "installed",
            "verification_state": "unverified",
            "cache_size_bytes": null,
            "is_selected": false,
            "is_active": false,
            "compatibility": "unknown",
        });
        let dto: ModelCatalogItemDto = serde_json::from_value(json).unwrap();
        // default caps：所有能力为 No { reason: "unknown" }
        assert!(!dto.stt_capabilities.pseudo_streaming.is_supported());
        assert!(!dto.stt_capabilities.true_streaming.is_supported());
    }

    #[test]
    fn capability_flag_yes_serializes_as_supported_true() {
        let flag = CapabilityFlag::yes();
        let json = serde_json::to_value(&flag).unwrap();
        assert_eq!(json["supported"], "yes");
        // Yes variant 不含 reason
        assert!(json.get("reason").is_none());
    }

    #[test]
    fn capability_flag_no_serializes_with_reason() {
        let flag = CapabilityFlag::no("some.reason");
        let json = serde_json::to_value(&flag).unwrap();
        assert_eq!(json["supported"], "no");
        assert_eq!(json["reason"], "some.reason");
    }

    #[test]
    fn sensevoice_caps_project_correctly() {
        // 验证 SenseVoice 能力矩阵正确投影到 DTO
        let specs = crate::app::local_engine::funasr::gguf::gguf_model_specs();
        let sensevoice = specs
            .iter()
            .find(|s| s.model_id == crate::app::local_engine::funasr::gguf::GGUF_SENSEVOICE_ID)
            .expect("SenseVoice spec must exist");

        let desc = crate::app::local_engine::funasr::gguf::gguf_model_descriptor(sensevoice);
        let status = EngineModelStatus::not_installed(&desc);
        let dto = project_model_status(&desc, &status);

        // SenseVoice 支持五语种
        assert_eq!(dto.stt_capabilities.languages.len(), 5);
        assert!(dto.stt_capabilities.languages.contains(&"zh".to_string()));
        assert!(dto.stt_capabilities.languages.contains(&"en".to_string()));
        assert!(dto.stt_capabilities.languages.contains(&"ja".to_string()));
        assert!(dto.stt_capabilities.languages.contains(&"ko".to_string()));
        assert!(dto.stt_capabilities.languages.contains(&"yue".to_string()));

        // SenseVoice 支持伪流式
        assert!(dto.stt_capabilities.pseudo_streaming.is_supported());

        // SenseVoice 不支持真流式
        assert!(!dto.stt_capabilities.true_streaming.is_supported());
    }

    #[test]
    fn paraformer_caps_project_correctly() {
        let specs = crate::app::local_engine::funasr::gguf::gguf_model_specs();
        let paraformer = specs
            .iter()
            .find(|s| s.model_id == crate::app::local_engine::funasr::gguf::GGUF_PARAFORMER_ID)
            .expect("Paraformer spec must exist");

        let desc = crate::app::local_engine::funasr::gguf::gguf_model_descriptor(paraformer);
        let status = EngineModelStatus::not_installed(&desc);
        let dto = project_model_status(&desc, &status);

        // Paraformer 仅中文
        assert_eq!(dto.stt_capabilities.languages, vec!["zh"]);

        // Paraformer 支持伪流式
        assert!(dto.stt_capabilities.pseudo_streaming.is_supported());
    }

    #[test]
    fn nano_caps_project_correctly() {
        let specs = crate::app::local_engine::funasr::gguf::gguf_model_specs();
        let nano = specs
            .iter()
            .find(|s| s.model_id == crate::app::local_engine::funasr::gguf::GGUF_NANO_ID)
            .expect("Nano spec must exist");

        let desc = crate::app::local_engine::funasr::gguf::gguf_model_descriptor(nano);
        let status = EngineModelStatus::not_installed(&desc);
        let dto = project_model_status(&desc, &status);

        // Nano 仅中文
        assert_eq!(dto.stt_capabilities.languages, vec!["zh"]);

        // Nano 支持伪流式
        assert!(dto.stt_capabilities.pseudo_streaming.is_supported());

        // Nano 不支持真流式（KV 每请求清空）
        assert!(!dto.stt_capabilities.true_streaming.is_supported());
    }

    #[test]
    fn business_profile_projects_and_omits_when_none() {
        // 有 business → DTO 携带；None → 字段整体缺省（旧前端兼容）
        let with_biz = {
            let mut desc = test_descriptor_with_caps("with-biz", SttModelCapabilities::default());
            desc.business = Some(crate::domain::local_engine::ModelBusinessProfile {
                chinese_quality: "corpus_baseline".to_string(),
                resource_footprint: "shared_gguf_worker".to_string(),
                recommended: false,
            });
            desc
        };
        let json =
            serde_json::to_value(project_model_status(&with_biz, &test_status(&with_biz))).unwrap();
        assert_eq!(json["business"]["chinese_quality"], "corpus_baseline");
        assert_eq!(json["business"]["resource_footprint"], "shared_gguf_worker");

        let without_biz = test_descriptor_with_caps("without-biz", SttModelCapabilities::default());
        let json = serde_json::to_value(project_model_status(
            &without_biz,
            &test_status(&without_biz),
        ))
        .unwrap();
        assert!(json.get("business").is_none());

        // 反序列化：缺 business 字段的旧 JSON 仍然可解析（wire 向后兼容）
        let dto: ModelCatalogItemDto = serde_json::from_value(json).unwrap();
        assert!(dto.business.is_none());
    }

    #[test]
    fn funasr_model_registry_declares_business_profiles() {
        // 三个 STT 候选（3 GGUF）都必须声明业务画像——
        // 设置页 FunASR 卡片展示业务差异的数据真源。
        let registry = make_funasr_model_registry();
        let funasr =
            crate::infra::local_engine::runtime::EngineId::new("funasr").expect("funasr is valid");
        let models = registry.list(&funasr);
        assert_eq!(models.len(), 3, "3 GGUF");

        for desc in models {
            let business = desc
                .business
                .as_ref()
                .unwrap_or_else(|| panic!("模型 {} 必须声明 business profile", desc.model_id));
            assert_eq!(business.chinese_quality, "corpus_baseline");
            assert_eq!(business.resource_footprint, "shared_gguf_worker");
        }

        // 退役的 ONNX 模型 id 不在目录中（未知旧 id 保持不可用，不静默换模）
        assert!(registry.find(&funasr, "onnx/paraformer-online").is_none());
    }
}
