//! ParaformerOnline ONNX 模型（`onnx/paraformer-online`）的 app 层装配
//! （0.22.9 Handoff 08 正式注册）。
//!
//! Handoff 07F 矩阵验证通过后，ParaformerOnline 从内部计划项升级为
//! `funasr` 引擎下的第 4 个模型候选：
//!
//! - **稳定 model id**：`onnx/paraformer-online`（真源在 domain 配置层）；
//! - **绑定 `ParaformerOnnxWorker` implementation**（编译期绑定表，见
//!   `implementation_registry`）；
//! - **per-implementation deployment**：ORT DLL + encoder/decoder/CMVN/
//!   tokenizer 全部落在 `impl-paraformer_onnx_worker` 部署空间，与 GGUF
//!   的 engine 级兼容真源互不可见（安装/升级/清理互不影响）；
//! - **安装命令使用 Paraformer provider**（`ParaformerOnnxProvider`，
//!   下载 + SHA-256 校验 + 隔离 self-test）；
//! - **start 时返回真实 StreamingSttPort**（二进制协议 v2 worker），
//!   VoiceService 按 start 冻结的 implementation 选择 port。
//!
//! ## 铁则
//!
//! - 不改变已有用户或 fresh-install 默认模型（默认仍为 SenseVoice GGUF）；
//! - 不迁移/删除既有三条 GGUF 路径；
//! - 前端不能提交 implementation/runtime/path——本模块只从编译期
//!   descriptor、asset lock 与受管部署空间解析。

use std::path::PathBuf;

use crate::domain::local_engine::EngineModelDescriptor;
use crate::infra::local_engine::deployment::{DeploymentSpace, DeploymentStore};
use crate::infra::local_engine::runtime::EngineId;
use crate::infra::local_engine::stt_asset_lock;

use super::FUNASR_ENGINE_ID;

/// ParaformerOnline 稳定模型 id（domain 配置层为真源，此处为 app 层别名）。
pub const PARAFORMER_ONLINE_ID: &str =
    crate::domain::config::stt_config::PARAFORMER_ONLINE_MODEL_ID;

/// ParaformerOnline 的模型 revision（与 ORT 版本绑定的稳定标识）。
///
/// ORT 升级（asset lock 变更）会改变 revision；模型文件变更由
/// `model_generation_id`（全部模型文件 hash 派生）承载。
pub fn paraformer_online_revision() -> String {
    match stt_asset_lock::parse_asset_lock() {
        Ok(lock) => format!("onnx-{}", lock.ort.version),
        Err(_) => "onnx-unknown".to_string(),
    }
}

/// ParaformerOnline implementation 的部署空间（per-implementation 真源）。
pub fn paraformer_online_deployment_space() -> DeploymentSpace {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid");
    DeploymentSpace::resolve(
        &engine_id,
        crate::domain::local_engine::ImplementationId::ParaformerOnnxWorker,
    )
}

/// 从 asset lock 计算期望的 model generation id（与 provider 写入 manifest
/// 的公式同源——`ParaformerOnnxProvider::build_manifest_extension`）。
pub fn expected_model_generation_id() -> Result<String, String> {
    let lock = stt_asset_lock::parse_asset_lock().map_err(|e| e.to_string())?;
    Ok(format!(
        "paraformer-online-{}",
        lock.models
            .iter()
            .map(|m| m.sha256[..12].to_string())
            .collect::<Vec<_>>()
            .join("-")
    ))
}

/// 模型总体积（展示用，字节）。
fn total_model_bytes() -> u64 {
    stt_asset_lock::parse_asset_lock()
        .map(|lock| lock.models.iter().map(|m| m.size_bytes).sum())
        .unwrap_or(0)
}

/// 把 ParaformerOnline 投影为领域模型 descriptor。
pub fn paraformer_online_model_descriptor() -> EngineModelDescriptor {
    EngineModelDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid"),
        model_id: PARAFORMER_ONLINE_ID.to_string(),
        display_name: "Paraformer-Online (ONNX 真流式)".to_string(),
        description: "中文 ASR 真流式（ONNX Runtime worker，边说边出字，native partial）"
            .to_string(),
        revision: paraformer_online_revision(),
        checksum_source: crate::infra::local_engine::runtime::ChecksumSource::Sha256(
            stt_asset_lock::parse_asset_lock()
                .map(|lock| {
                    lock.models
                        .iter()
                        .map(|m| m.sha256.clone())
                        .collect::<Vec<_>>()
                        .join("+")
                })
                .unwrap_or_default(),
        ),
        estimated_size_mb: Some(total_model_bytes() / (1024 * 1024)),
        compatibility_schema: crate::infra::local_engine::stream_worker_proto::PROTOCOL_VERSION
            as u32,
        stt_capabilities: crate::domain::local_engine::SttModelCapabilities {
            languages: vec!["zh".into()],
            // "边说边出字"体验可用——由真流式（native partial）承载
            pseudo_streaming: crate::domain::local_engine::CapabilityFlag::yes(),
            // ParaformerOnline 是真流式模型（CIF 增量出字）
            true_streaming: crate::domain::local_engine::CapabilityFlag::yes(),
            timestamps: crate::domain::local_engine::CapabilityFlag::no(
                "stt.capability.timestamps.not_exposed",
            ),
            punctuation: crate::domain::local_engine::CapabilityFlag::no(
                "stt.capability.punctuation.not_in_model",
            ),
        },
        // 中文质量证据：0.22.9 同语料矩阵（handoff-07F，p50 相对 Nano 基线
        // 恶化 ≤1pp，注册门 PASS）；资源占用：独立 ONNX worker 进程 + ORT。
        business: Some(crate::domain::local_engine::ModelBusinessProfile {
            chinese_quality: "corpus_baseline".to_string(),
            resource_footprint: "dedicated_onnx_worker".to_string(),
            recommended: false,
        }),
    }
}

// ── 部署状态与身份解析 ──────────────────────────────────────────────────────

/// active 部署 slot 目录（None = 未安装）。
pub fn active_deployment_dir() -> Result<Option<PathBuf>, String> {
    match DeploymentStore::active_dir(&paraformer_online_deployment_space()) {
        Ok(Some((_pointer, dir))) => Ok(Some(dir)),
        Ok(None) => Ok(None),
        Err(e) => Err(format!("读取 ParaformerOnline 部署指针失败: {e}")),
    }
}

/// 环境自我检查：active 部署存在且结构完整（worker 端 `validate_deployment`
/// 同源规则——文件存在 + 可选 asset-lock hash 校验）。
pub fn paraformer_online_environment_self_test() -> Result<(), String> {
    let dir = active_deployment_dir()?.ok_or_else(|| {
        "ParaformerOnline ONNX 运行时未安装。请在设置页「引擎」中安装 onnx/paraformer-online 模型。"
            .to_string()
    })?;
    crate::infra::local_engine::paraformer_worker::validate_deployment(&dir).map(|_| ())
}

// ── 启动构造 ────────────────────────────────────────────────────────────────

/// 构建 ParaformerOnline worker 的 `LaunchDescriptor`（FunASR adapter 在
/// `ctx.implementation == ParaformerOnnxWorker` 时分派到此处）。
///
/// worker 是 blink.exe 自身的隐藏子命令（`paraformer-worker --deployment`），
/// 部署目录从 per-implementation 部署空间的 active 指针解析（fail-closed；
/// 操作互斥保证 start 期间指针不漂移）。
///
/// 身份保证：子进程由 `ManagedProcess` spawn 并纳入 Job Object，stdio 管道
/// 与该实例一一绑定——二进制协议 v2 无身份回显字段，进程句柄即身份。
pub fn build_paraformer_online_launch_descriptor() -> Result<
    crate::domain::local_engine::LaunchDescriptor,
    crate::domain::local_engine::LocalEngineError,
> {
    use crate::domain::local_engine::{
        ErrorPhase, LaunchDescriptor, LocalEngineError, LocalEngineErrorCode,
    };

    let deployment_dir = active_deployment_dir()
        .map_err(|reason| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::EnvironmentMissing,
                ErrorPhase::Start,
                "读取 ParaformerOnline 部署失败",
                reason,
            )
        })?
        .ok_or_else(|| {
            LocalEngineError::with_detail(
                LocalEngineErrorCode::EnvironmentMissing,
                ErrorPhase::Start,
                "ParaformerOnline 未安装，请先安装模型",
                "impl-paraformer_onnx_worker 部署空间内无 active deployment（fail-closed）",
            )
        })?;

    let exe = std::env::current_exe().map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "无法定位 blink.exe",
            format!("{e}"),
        )
    })?;

    // 部署完整性由 worker 端 validate_deployment 强校验（缺文件 → 早退），
    // start 的 ready 握手据此失败并附带 worker stderr 日志尾（含缺失文件名）。
    // 此处只记 debug 诊断，不做硬失败——避免与 worker 校验双重判定漂移。
    if let Err(reason) =
        crate::infra::local_engine::paraformer_worker::validate_deployment(&deployment_dir)
    {
        tracing::debug!(
            deployment = %deployment_dir.display(),
            %reason,
            "ParaformerOnline 部署预检未通过（worker 校验将硬失败）"
        );
    }

    tracing::info!(
        deployment = %deployment_dir.display(),
        transport = "stdio(binary-v2)",
        "构建 ParaformerOnline LaunchDescriptor"
    );

    Ok(LaunchDescriptor {
        executable: exe,
        args: vec![
            "paraformer-worker".to_string(),
            "--deployment".to_string(),
            deployment_dir.display().to_string(),
        ],
        current_dir: None,
        env: std::collections::HashMap::new(),
        label: FUNASR_ENGINE_ID.to_string(),
    })
}

// ── ProviderDescriptor（安装事务用）─────────────────────────────────────────

/// 构造 ParaformerOnline 的 `ProviderDescriptor`（OnnxRuntime 安装事务）。
///
/// 安装事务把 ORT DLL + 模型文件写入 `impl-paraformer_onnx_worker`
/// 部署空间；self-test 为隔离验证进程（`blink.exe paraformer-selftest`）。
pub fn make_paraformer_online_provider_descriptor()
-> crate::infra::local_engine::providers::ProviderDescriptor {
    use crate::infra::local_engine::providers::{
        CompatibilityCheck, InstallPlan, OnnxInstallPlan, ProfileCandidate, ProviderDescriptor,
    };
    use crate::infra::local_engine::runtime::{
        ChecksumSource, ComputeBackend, EngineId, ModelContract, RuntimePlan,
    };

    let lock = stt_asset_lock::parse_asset_lock().expect("STT asset-lock.json 必须可解析");
    let dll_artifact = stt_asset_lock::ort_dll_artifact_id().expect("ORT artifact id 合法");
    let dll_sha256 = lock
        .ort
        .files
        .iter()
        .find(|f| f.path.ends_with("onnxruntime.dll"))
        .map(|f| f.sha256.clone())
        .expect("asset lock 必须锁定 onnxruntime.dll");

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).expect("funasr is valid"),
        runtime_kind: RuntimePlan::OnnxRuntime,
        display_name: "Paraformer-Online (ONNX 真流式)".to_string(),
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: dll_artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id: PARAFORMER_ONLINE_ID.to_string(),
            revision: paraformer_online_revision(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::OnnxRuntime(OnnxInstallPlan {
            dll_artifact_id: dll_artifact,
            ort_version: lock.ort.version.clone(),
            dll_url: lock.ort.url.clone(),
            dll_sha256,
            inter_op: 1,
            // 不超过 4 线程，避免推理抢占 Alt+Space 主链路（与 OCR ONNX 同则）
            intra_op: 4,
            execution_provider: "cpu".to_string(),
        }),
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::local_engine::funasr::gguf::GGUF_SENSEVOICE_ID;

    #[test]
    fn model_id_matches_domain_constant() {
        assert_eq!(PARAFORMER_ONLINE_ID, "onnx/paraformer-online");
    }

    #[test]
    fn model_descriptor_declares_true_streaming_zh() {
        let desc = paraformer_online_model_descriptor();
        assert_eq!(desc.model_id, PARAFORMER_ONLINE_ID);
        assert_eq!(desc.engine_id.as_str(), FUNASR_ENGINE_ID);
        assert!(desc.stt_capabilities.true_streaming.is_supported());
        assert!(desc.stt_capabilities.pseudo_streaming.is_supported());
        assert!(!desc.stt_capabilities.timestamps.is_supported());
        assert_eq!(desc.stt_capabilities.languages, vec!["zh"]);
        assert!(desc.estimated_size_mb.unwrap_or(0) > 0);
    }

    #[test]
    fn revision_is_derived_from_asset_lock() {
        let rev = paraformer_online_revision();
        assert!(
            rev.starts_with("onnx-"),
            "revision 应派生自 asset lock ORT 版本: {rev}"
        );
        assert_eq!(rev, "onnx-1.19.2");
    }

    #[test]
    fn expected_generation_id_matches_provider_formula() {
        let generation = expected_model_generation_id().expect("generation id 可计算");
        assert!(generation.starts_with("paraformer-online-"), "{generation}");
        // 与 provider 的公式同源：4 个模型文件 hash 前 12 位
        let parts: Vec<&str> = generation
            .trim_start_matches("paraformer-online-")
            .split('-')
            .collect();
        assert_eq!(parts.len(), 4, "4 个模型文件: {generation}");
        for p in parts {
            assert_eq!(p.len(), 12, "{generation}");
        }
    }

    #[test]
    fn provider_descriptor_declares_onnx_plan() {
        let desc = make_paraformer_online_provider_descriptor();
        assert_eq!(desc.engine_id.as_str(), FUNASR_ENGINE_ID);
        assert_eq!(
            desc.runtime_kind,
            crate::infra::local_engine::runtime::RuntimePlan::OnnxRuntime
        );
        let crate::infra::local_engine::providers::InstallPlan::OnnxRuntime(plan) =
            &desc.install_plan
        else {
            panic!("install_plan 应为 OnnxRuntime");
        };
        assert_eq!(plan.execution_provider, "cpu");
        assert_eq!(plan.intra_op, 4);
        assert_eq!(plan.inter_op, 1);
        assert_eq!(desc.model_contract.model_id, PARAFORMER_ONLINE_ID);
        assert_eq!(desc.model_contract.revision, paraformer_online_revision());
    }

    #[test]
    fn default_selection_remains_gguf_sensevoice() {
        // 铁则：注册新模型不改变 fresh-install 默认模型（default 配置真源不变）
        assert_eq!(
            crate::app::stt_config::SttConfig::default()
                .local_engine
                .funasr_model,
            GGUF_SENSEVOICE_ID
        );
        assert_ne!(GGUF_SENSEVOICE_ID, PARAFORMER_ONLINE_ID);
    }
}
