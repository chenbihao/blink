//! FunASR 启动构造：`FunasrEngineConfig` 配置投影 + `LaunchDescriptor`
//! （active deployment venv 解析、启动参数、受限环境变量、model_storage manifest 注入）。

use std::collections::HashMap;

use crate::domain::local_engine::{
    AdapterConfig, ErrorPhase, LaunchContext, LaunchDescriptor, LocalEngineError,
    LocalEngineErrorCode,
};
use crate::domain::stt::funasr;
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::runtime as engine_runtime;
use crate::infra::local_engine::runtime::{ComputeBackend, EngineId};

use super::{FUNASR_ENGINE_ID, PackageChecker, active_deployment_venv_python};

// ── FunasrEngineConfig（从 SttConfig 投影） ────────────────────────────────

/// FunASR 引擎配置（从 `SttConfig.local_engine` 投影）。
///
/// 保持已有配置 key 和 serde 形状，不做配置迁移，不改默认值。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct FunasrEngineConfig {
    /// 模型标识（如 "iic/SenseVoiceSmall" / "paraformer-zh"）
    pub funasr_model: String,
    /// 推理设备: "cpu" 或 "cuda"
    pub device: String,
    /// CPU 推理线程数（None = 自动）
    #[serde(default)]
    pub num_threads: Option<u32>,
    /// 热词列表（英文逗号分隔，每项格式「词 权重」）
    #[serde(default)]
    pub hotwords: Option<String>,
    /// ITN 逆文本归一化
    pub use_itn: bool,
    /// VAD 切句参数（伪流式模式生效）
    #[serde(default)]
    pub vad: VadConfigProjection,
    /// Blink 启动后自动启动服务
    #[serde(default)]
    pub auto_start_server: bool,
}

/// VAD 配置投影（保持与 SttConfig 相同的 serde 形状）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct VadConfigProjection {
    /// RMS 低于此值视为静默。
    #[serde(default)]
    pub silence_threshold: f64,
    /// 静默持续多久判定句尾。
    #[serde(default)]
    pub min_silence_ms: u32,
    /// 最小句子长度。
    #[serde(default)]
    pub min_sentence_ms: u32,
}

impl FunasrEngineConfig {
    /// 从 `SttConfig` 的 `local_engine` 配置投影。
    ///
    /// 保持已有配置 key 和 serde 形状。
    pub fn from_stt_config(local: &crate::domain::config::stt_config::LocalEngineConfig) -> Self {
        Self {
            funasr_model: local.funasr_model.clone(),
            device: local.device.clone(),
            num_threads: local.num_threads,
            hotwords: local.hotwords.clone(),
            use_itn: local.use_itn,
            vad: VadConfigProjection {
                silence_threshold: local.vad.silence_threshold,
                min_silence_ms: local.vad.min_silence_ms,
                min_sentence_ms: local.vad.min_sentence_ms,
            },
            auto_start_server: local.auto_start_server,
        }
    }

    /// 转为 `serde_json::Value` 以注入 `AdapterConfig::engine_config`。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

// ── LaunchDescriptor 构造 ───────────────────────────────────────────────────

/// 从 resolved profile 推导 FunASR 启动执行设备（唯一真相）。
///
/// 0.22.6 产品约束：FunASR 只支持 CPU profile（descriptor 只声明 `cpu-x64`，
/// 安装的是 torch/torchaudio CPU wheel）。resolved profile 是当前不支持的
/// backend 时返回结构化 `Unsupported`——**不得默认回落或猜测**。
pub(super) fn funasr_device_for_backend(
    backend: ComputeBackend,
) -> Result<String, LocalEngineError> {
    match backend {
        ComputeBackend::Cpu => Ok("cpu".to_string()),
        other => Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::Unsupported,
            ErrorPhase::Start,
            "不支持的执行后端",
            format!(
                "FunASR 0.22.6 只支持 CPU profile，resolved profile backend={other:?}。\
                 不默认回落到 CPU——请修正 compute preference 或安装对应 profile。"
            ),
        )),
    }
}

/// 构建 FunASR 命令行参数（纯函数，便于回归测试）。
///
/// `--device` 必须来自 resolved profile 推导结果（见 `funasr_device_for_backend`），
/// 历史 STT config 的 device 字段不参与。
/// 身份参数不出现在命令行（BLINK_ENGINE_TOKEN 等由 service 层注入环境变量）。
pub(super) fn build_funasr_args(
    model: &str,
    device: &str,
    port: u16,
    hotwords_path: Option<&std::path::Path>,
    use_itn: bool,
    script_path: &std::path::Path,
) -> Vec<String> {
    let mut args: Vec<String> = Vec::new();
    args.push(script_path.to_string_lossy().to_string());
    args.push("--model".to_string());
    args.push(model.to_string());
    args.push("--port".to_string());
    args.push(port.to_string());
    args.push("--device".to_string());
    args.push(device.to_string());
    if let Some(hw_path) = hotwords_path {
        args.push("--hotwords".to_string());
        args.push(hw_path.to_string_lossy().to_string());
    }
    if use_itn {
        args.push("--use-itn".to_string());
    }
    args
}

/// 构建 FunASR 的 `LaunchDescriptor`。
///
/// 返回模型 id 对应的子模型列表。
///
/// 与 Python installer 的 `ALLOWED_MODELS` submodels 字段保持一致。
/// - SenseVoice 系列：内置 VAD/标点/ITN，无需子模型
/// - paraformer-zh：需要 fsmn-vad + ct-punc
///
/// 返回空 Vec 表示无需子模型。
pub(super) fn funasr_submodels_for(model_id: &str) -> Vec<&'static str> {
    let name_lower = model_id.to_lowercase();
    if name_lower.contains("sensevoice") {
        Vec::new()
    } else if name_lower.contains("paraformer") {
        vec!["fsmn-vad", "ct-punc"]
    } else {
        Vec::new()
    }
}

/// 从 `FunasrEngineConfig` 产生启动请求，保留：
/// - `funasr_model`
/// - device/计算偏好现有语义
/// - port/preferred port
/// - hotwords
/// - ITN
/// - VAD
/// - auto_start_server
///
/// 热词文件生成、ITN、VAD 和 HTTP transcription 业务语义不变。
///
/// **设备唯一真相（0.22.6.1）**：启动执行设备只从 `ctx.resolved_profile.backend`
/// 推导——CPU profile 必须生成 `--device cpu`；`FunasrEngineConfig.device` 是
/// 历史 wire/config 兼容字段，**不得**再覆盖 resolved profile（防止归一化为
/// CPU 的 compute_preference 与残留 `device=cuda` 构成双真相）。
pub(super) fn build_funasr_launch_descriptor(
    funasr_config: &FunasrEngineConfig,
    _adapter_config: &AdapterConfig,
    ctx: &LaunchContext,
    package_checker: PackageChecker,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let model = &funasr_config.funasr_model;
    // 设备只从 resolved profile 推导；历史 config device 不参与启动执行
    let device = funasr_device_for_backend(ctx.resolved_profile.backend)?;
    // 使用 service 分配的 endpoint 端口，不用 adapter_config.preferred_port
    let port = ctx.endpoint.port();

    // 只使用 deployment-managed venv，不 fallback 到旧全局 venv
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "engine_id 无效",
            format!("解析 engine_id 失败: {e}"),
        )
    })?;
    let python_path = active_deployment_venv_python(&engine_id);
    let python = python_path.ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "Python 环境未就绪",
            "FunASR 环境未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」按钮。\
             （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
        )
    })?;

    // 检查 funasr 是否已安装（使用 active deployment venv 中的 python）
    let (funasr_ok, _) = package_checker(&python, "funasr");
    if !funasr_ok {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "funasr 包未安装",
            "funasr 包未安装。请在设置页「引擎」→「本地模型运行时」中点击「修复」或「安装环境」按钮。",
        ));
    }

    // 确保 blink_stt_server.py 已释放
    let script_path = funasr::ensure_server_script().map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "释放 blink_stt_server.py 失败",
            e,
        )
    })?;

    tracing::info!(
        script = %script_path.display(),
        model,
        port,
        device = %device,
        resolved_profile = %ctx.resolved_profile.profile_id,
        "构建 FunASR LaunchDescriptor"
    );

    // 构建参数列表（--device 来自 resolved profile，不来自历史 config device）
    let hotwords_path = funasr::write_hotwords_file(&funasr_config.hotwords);
    let args = build_funasr_args(
        model,
        &device,
        port,
        hotwords_path.as_deref(),
        funasr_config.use_itn,
        script_path.as_path(),
    );

    // 受限环境变量
    let mut env = HashMap::new();
    // Python 输出无缓冲 + UTF-8 模式（修复 Windows 控制台中文乱码）
    env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());

    // 0.22.6 B2: MODELSCOPE_CACHE fail-closed——创建失败直接返回错误，不 fallback
    // 到用户默认缓存（~/.cache/modelscope），避免模型文件散落到不可控位置。
    let models_dir =
        engine_runtime::engine_model_cache_dir(&EngineId::new(FUNASR_ENGINE_ID).unwrap());
    if let Err(e) = std::fs::create_dir_all(&models_dir) {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "MODELSCOPE_CACHE 目录创建失败",
            format!(
                "创建 ModelScope 缓存目录失败: {e}。Blink 不 fallback 到用户默认缓存——请检查磁盘空间和权限。"
            ),
        ));
    }
    let models_path = models_dir.display().to_string();
    tracing::info!(path = %models_path, "ModelScope 缓存目录");
    env.insert("MODELSCOPE_CACHE".to_string(), models_path);

    // 0.22.6 B2: 从 model_storage manifest 动态获取模型身份
    // 不使用 descriptor 中静态硬编码的 model_contract——而是从当前安装的
    // active model manifest 中读取 model_id/revision/payload_dir/fingerprint。
    // 这样 health Ready 校验可以核对实际安装的模型身份，而非 descriptor 静态值。
    let canonical_model_id = &funasr_config.funasr_model;
    let asset_key = mstore::encode_asset_key(canonical_model_id);
    let funasr_engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    match mstore::restore_model_state(&funasr_engine_id, &asset_key) {
        Ok(mstore::RestoredModelState::Installed { manifest, .. }) => {
            // 从 manifest 注入动态模型身份环境变量
            env.insert("BLINK_MODEL_ID".to_string(), manifest.model_id.clone());
            env.insert(
                "BLINK_MODEL_REVISION".to_string(),
                manifest.revision.clone(),
            );
            // payload 目录绝对路径
            let payload_dir =
                mstore::model_payload_dir(&funasr_engine_id, &asset_key, &manifest.slot_id)
                    .map_err(|e| {
                        LocalEngineError::with_detail(
                            LocalEngineErrorCode::Internal,
                            ErrorPhase::Start,
                            "payload 目录路径计算失败",
                            e.to_string(),
                        )
                    })?;
            env.insert(
                "BLINK_MODEL_PAYLOAD_DIR".to_string(),
                payload_dir.display().to_string(),
            );
            env.insert(
                "BLINK_MODEL_FINGERPRINT".to_string(),
                manifest.content_fingerprint.clone(),
            );
            // 0.22.6 B2: 注入子模型列表（VAD/punc 等）
            // 从静态映射获取子模型列表——与 Python installer 的 ALLOWED_MODELS 一致。
            // SenseVoice 内置 VAD/标点/ITN，无需子模型；
            // Paraformer 需要 fsmn-vad + ct-punc。
            let submodels = funasr_submodels_for(&manifest.model_id);
            if !submodels.is_empty() {
                env.insert("BLINK_MODEL_SUBMODELS".to_string(), submodels.join(","));
            }
            tracing::info!(
                model_id = %manifest.model_id,
                revision = %manifest.revision,
                slot_id = %manifest.slot_id,
                fingerprint = %manifest.content_fingerprint,
                submodels = ?submodels,
                "从 manifest 注入动态模型身份"
            );
        }
        Ok(mstore::RestoredModelState::Corrupted { reason, .. }) => {
            tracing::warn!(
                model_id = %canonical_model_id,
                reason = %reason,
                "模型状态 Corrupted——不注入 payload_dir，Python 将报错"
            );
            // 不注入 BLINK_MODEL_*——Python server 会因 payload_dir 缺失而报错
        }
        Ok(mstore::RestoredModelState::NotInstalled) => {
            tracing::warn!(
                model_id = %canonical_model_id,
                "模型未安装——不注入 payload_dir"
            );
        }
        Err(e) => {
            tracing::warn!(
                model_id = %canonical_model_id,
                error = %e,
                "模型状态恢复失败——不注入 payload_dir"
            );
        }
    }

    Ok(LaunchDescriptor {
        executable: python,
        args,
        current_dir: None,
        env,
        label: FUNASR_ENGINE_ID.to_string(),
    })
}
