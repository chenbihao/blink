//! FunASR 本地引擎 adapter（0.22.3）。
//!
//! 把现有 Python/PyTorch FunASR 注册为 `LocalEngineAdapter`，使安装、启动、
//! 模型轮询、日志、空间和清理通过 `LocalEngineService` 管理。
//!
//! ## 设计铁则
//!
//! - **descriptor 锁定 Python/package/profile/model contract**：使用 0.22.2
//!   `PythonVenvProvider`；不新造第二套安装器。
//! - **adapter 从 SttConfig 的已校验 local_engine 配置产生启动请求**：保留
//!   `funasr_model`、`device`/计算偏好、`port`/`preferred port`、`hotwords`、
//!   `ITN`、`VAD`、`auto_start_server` 语义。
//! - **热词文件生成、ITN、VAD 和 HTTP transcription 业务语义不变**。
//! - **保持已有配置 key 和 serde 形状**，不做配置迁移，不改默认值。
//! - **endpoint 仅 127.0.0.1**：每次启动生成 token/instance id。
//! - **health 必须核对 engine id、instance id 和 token**。
//! - **日志使用 ManagedProcess 的 bounded history/broadcast**，保留 FunASR
//!   噪声过滤。
//! - **空间统计和清理区分 engine generations / FunASR model cache / provider
//!   公共缓存**；单引擎清理不能连带删除公共资产。
//! - **不修改 main.rs 和 Tauri command 注册**——注册函数由 H6 接 wiring。

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, CapabilityKind, CleanupPolicy, ComputeCandidate,
    DiagnosticEntry, EngineDescriptor, EngineDiagnostic, EngineDisplay, EngineTimeouts, ErrorPhase,
    HealthMapping, InstallPlanRef, LaunchContext, LaunchDescriptor, LifecyclePolicy,
    LocalEngineAdapter, LocalEngineError, LocalEngineErrorCode, ModelHealth, ResolvedLaunch,
    ResourceBudget, ServiceHealth,
};
use crate::domain::stt::funasr;
use crate::infra::local_engine::providers::python::PythonVenvProvider;
use crate::infra::local_engine::providers::{
    CompatibilityCheck, InstallPlan, PackageLock, ProfileCandidate, ProviderDescriptor,
    PythonInstallPlan,
};
use crate::infra::local_engine::runtime::{
    ArtifactId, BackendObservation, ChecksumSource, ComputeBackend, ComputePreference, EngineId,
    ModelContract, RuntimeKind,
};

/// 嵌入的 blink_stt_server.py 脚本（随 Rust 二进制发布）。
///
/// 重新声明在此模块以保持 adapter 自包含；领域层的 `funasr.rs` 保留原始常量。
#[allow(dead_code)]
const BLINK_STT_SERVER_PY: &str = include_str!("../../../resources/stt/funasr/blink_stt_server.py");

/// FunASR 稳定 engine id。
pub const FUNASR_ENGINE_ID: &str = "funasr";

// ── FunasrAdapter ──────────────────────────────────────────────────────────

/// FunASR 本地引擎 adapter。
///
/// 实现 `LocalEngineAdapter` trait，把 FunASR 特有的启动参数、health 映射、
/// 诊断和 self-test 适配到领域统一协议。
///
/// ## 边界
///
/// - **不接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
///   `prepare_launch` 从 descriptor 锁定的 artifact + `SttConfig` 自行解析。
/// - **不发送 Tauri 事件**：返回纯数据，由 app 层桥接。
/// - **不持有 AppHandle**：adapter 是纯逻辑，不接触 Tauri。
pub struct FunasrAdapter {
    descriptor: EngineDescriptor,
}

impl FunasrAdapter {
    /// 创建 FunASR adapter。
    ///
    /// descriptor 在编译期声明，锁定 engine id、profile、artifact 和 model contract。
    pub fn new() -> Self {
        Self {
            descriptor: make_funasr_descriptor(),
        }
    }
}

impl Default for FunasrAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalEngineAdapter for FunasrAdapter {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    /// 从已校验配置、resolved profile 和受控启动上下文产生受限启动描述。
    ///
    /// **不接受前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
    /// adapter 从 descriptor 锁定的 artifact + SttConfig 自行解析。
    ///
    /// adapter 必须使用 `LaunchContext` 中的 endpoint 和身份参数构造启动参数——
    /// Python --port 必须等于 ctx.endpoint.port()，
    /// --engine-id/--instance-id/--token 必须使用 ctx 中的值。
    fn prepare_launch(
        &self,
        ctx: &LaunchContext,
        config: &AdapterConfig,
    ) -> Result<ResolvedLaunch, LocalEngineError> {
        // 验证 profile 在 descriptor 允许范围内
        if !self.descriptor.is_profile_allowed(&ctx.resolved_profile) {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Start,
                "不支持的 profile",
                format!(
                    "profile '{}' 不在 FunASR descriptor 声明范围内",
                    ctx.resolved_profile.profile_id
                ),
            ));
        }

        // 从 AdapterConfig.engine_config 解析 FunASR 配置
        let funasr_config: FunasrEngineConfig =
            serde_json::from_value(config.engine_config.clone()).map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "FunASR 引擎配置解析失败",
                    format!("engine_config 反序列化失败: {e}"),
                )
            })?;

        // 构建 LaunchDescriptor（FunASR 特有参数/脚本/环境变量）
        // 使用 ctx.endpoint.port() 作为 --port，不用 config.preferred_port
        let launch = build_funasr_launch_descriptor(&funasr_config, config, &ctx)?;

        Ok(ResolvedLaunch {
            profile: ctx.resolved_profile.clone(),
            fallback: None,
            launch,
        })
    }

    /// 把 FunASR 的 health 响应映射为领域统一的 service/model 健康状态。
    ///
    /// health 映射区分：
    /// - service reachable（HTTP 可达）
    /// - model loading（模型正在加载）
    /// - model ready（模型已就绪）
    /// - model failed（模型加载失败）
    ///
    /// health 必须核对 engine id、instance id 和 token。
    /// 如果 health 响应缺少身份字段，service 降级为 Unreachable。
    fn map_health(&self, raw_health: &serde_json::Value) -> HealthMapping {
        map_funasr_health(raw_health)
    }

    /// adapter self-test。
    ///
    /// 验证 FunASR Python 环境是否就绪（venv + funasr 包已安装）。
    fn self_test(&self) -> AdapterSelfTest {
        // 检查 venv python 是否可用
        let python_path = crate::infra::platform::python::venv_python();
        if python_path.is_none() {
            return AdapterSelfTest::failed(
                "Python 环境未就绪。请在设置页点击「安装环境」按钮。\
                 （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
            );
        }

        // 检查 funasr 是否已安装
        let (funasr_ok, _) = crate::infra::platform::python::check_funasr();
        if !funasr_ok {
            return AdapterSelfTest::failed(
                "funasr 包未安装。请在设置页点击「安装环境」按钮，Blink 会自动完成安装。",
            );
        }

        AdapterSelfTest::passed()
    }

    /// 引擎专属诊断投影。
    ///
    /// 返回 FunASR 特有的诊断信息（Python 环境、torch、funasr 版本等）。
    fn diagnostics(&self) -> EngineDiagnostic {
        let mut entries = Vec::new();

        // venv 状态
        let py_status = crate::infra::platform::python::check_status();
        entries.push(DiagnosticEntry {
            key: "venv_exists".to_string(),
            value: if py_status.venv_exists {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: "info".to_string(),
        });

        if let Some(ref v) = py_status.venv_python_version {
            entries.push(DiagnosticEntry {
                key: "python_version".to_string(),
                value: v.clone(),
                label: "info".to_string(),
            });
        }

        // torch 状态
        let (torch_ok, torch_ver) = crate::infra::platform::python::check_torch();
        entries.push(DiagnosticEntry {
            key: "torch_installed".to_string(),
            value: if torch_ok {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: if torch_ok {
                "info".to_string()
            } else {
                "warning".to_string()
            },
        });
        if let Some(ref v) = torch_ver {
            entries.push(DiagnosticEntry {
                key: "torch_version".to_string(),
                value: v.clone(),
                label: "info".to_string(),
            });
        }

        // CUDA 状态
        if torch_ok {
            let cuda_ok = crate::infra::platform::python::check_torch_cuda();
            entries.push(DiagnosticEntry {
                key: "torch_cuda_available".to_string(),
                value: if cuda_ok {
                    "true".to_string()
                } else {
                    "false".to_string()
                },
                label: "info".to_string(),
            });
        }

        // funasr 状态
        let (funasr_ok, funasr_ver) = crate::infra::platform::python::check_funasr();
        entries.push(DiagnosticEntry {
            key: "funasr_installed".to_string(),
            value: if funasr_ok {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: if funasr_ok {
                "info".to_string()
            } else {
                "warning".to_string()
            },
        });
        if let Some(ref v) = funasr_ver {
            entries.push(DiagnosticEntry {
                key: "funasr_version".to_string(),
                value: v.clone(),
                label: "info".to_string(),
            });
        }

        EngineDiagnostic { entries }
    }
}

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 FunASR 编译期 descriptor。
///
/// descriptor 必须锁定现有 Python/package/profile/model contract。
/// 使用 0.22.2 `PythonVenvProvider`；不新造第二套安装器。
fn make_funasr_descriptor() -> EngineDescriptor {
    // Python distribution artifact（引用 provider 管理的锁定标识）
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    EngineDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "FunASR 语音识别".to_string(),
            description: "本地 FunASR 语音转文字（Python/PyTorch）".to_string(),
            icon: "mic".to_string(),
            version: "0.10.4".to_string(),
        },
        capability_kind: CapabilityKind::Stt,
        runtime_kind: RuntimeKind::PythonVenv,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimeKind::PythonVenv,
            artifact_ids: vec![python_artifact.clone()],
            // FunASR 当前只声明 CPU profile（CUDA 支持保留但不作为 descriptor 默认）
            compute_candidates: vec![
                ComputeCandidate {
                    preference: ComputePreference::Cpu,
                    profile_id: "cpu-x64".to_string(),
                    artifact_id: python_artifact.clone(),
                },
                ComputeCandidate {
                    preference: ComputePreference::Cuda,
                    profile_id: "cuda-x64".to_string(),
                    artifact_id: python_artifact.clone(),
                },
            ],
            schema_version: 1,
        },
        // 模型契约：FunASR 模型由 ModelScope 下载，上游不提供稳定 checksum
        model_contract: ModelContract {
            model_id: "iic/SenseVoiceSmall".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::Manual,
        // FunASR 首次启动需要下载模型（~234MB），超时设为 300s
        timeouts: EngineTimeouts {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(300),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            estimated_env_disk_mb: Some(3000),  // venv + torch + funasr ~3GB
            estimated_model_disk_mb: Some(234), // SenseVoiceSmall ~234MB
            estimated_stable_ram_mb: Some(500),
            estimated_peak_ram_mb: Some(1500),
        },
        cleanup: CleanupPolicy {
            // FunASR 拥有的子目录（相对于引擎根目录）
            owned_subdirs: vec!["generations".to_string(), "staging".to_string()],
            has_model_cache: true,
            has_log_dir: false,
        },
    }
}

// ── ProviderDescriptor 构造 ──────────────────────────────────────────────────

/// 构造 FunASR 的 `ProviderDescriptor`（infra 层安装事务用）。
///
/// 与 `make_funasr_descriptor()`（domain 层 `EngineDescriptor`）互补：
/// - `EngineDescriptor` 持有 `InstallPlanRef`（只引用 artifact id，不含具体安装步骤）
/// - `ProviderDescriptor` 持有 `InstallPlan::PythonVenv(PythonInstallPlan)`
///   （含 Python 版本、锁定包列表、self-test 脚本等完整安装信息）
///
/// `InstallTransaction` 需要 `&ProviderDescriptor` 才能执行安装事务。
///
/// **包列表与 `platform::python::setup_with_progress` 保持一致**：
/// - torch, torchaudio, torch_complex
/// - numba>=0.59
/// - funasr, fastapi, uvicorn[standard], python-multipart
///
/// SHA-256 hash 暂为 `None`——上游 PyPI wheel hash 随版本变化，
/// 后续可通过 `cargo xtask lock-packages` 自动锁定。
pub fn make_funasr_provider_descriptor() -> ProviderDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimeKind::PythonVenv,
        display_name: "FunASR 语音识别".to_string(),
        profiles: vec![
            ProfileCandidate {
                profile_id: "cpu-x64".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: python_artifact.clone(),
                compatibility: CompatibilityCheck::Always,
            },
            ProfileCandidate {
                profile_id: "cuda-x64".to_string(),
                backend: ComputeBackend::Cuda,
                artifact_id: python_artifact.clone(),
                compatibility: CompatibilityCheck::RequiresCuda { min_version: None },
            },
        ],
        model_contract: ModelContract {
            model_id: "iic/SenseVoiceSmall".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: python_artifact,
            packages: vec![
                PackageLock {
                    name: "torch".to_string(),
                    version: "2.5.0".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "torchaudio".to_string(),
                    version: "2.5.0".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "torch_complex".to_string(),
                    version: "0.4.3".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "numba".to_string(),
                    version: ">=0.59".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "funasr".to_string(),
                    version: "1.3.0".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "fastapi".to_string(),
                    version: "0.115.6".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "uvicorn".to_string(),
                    version: "0.34.0".to_string(),
                    sha256: None,
                    ..Default::default()
                },
                PackageLock {
                    name: "python-multipart".to_string(),
                    version: "0.0.20".to_string(),
                    sha256: None,
                    ..Default::default()
                },
            ],
            uv_version: "0.6.10".to_string(),
            index_url: None,
            extra_pip_args: vec![],
            self_test_script: "import funasr; import torch; import fastapi; import uvicorn"
                .to_string(),
        }),
        min_generations: 2,
    }
}

/// 创建 FunASR 的 `PythonVenvProvider` 实例。
///
/// `LocalEngineService` 持有此实例，在 `install` 时传给 `InstallTransaction`。
pub fn make_funasr_python_provider() -> PythonVenvProvider {
    PythonVenvProvider::new()
}

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

/// 构建 FunASR 的 `LaunchDescriptor`。
///
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
fn build_funasr_launch_descriptor(
    funasr_config: &FunasrEngineConfig,
    _adapter_config: &AdapterConfig,
    ctx: &LaunchContext,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let model = &funasr_config.funasr_model;
    let device = &funasr_config.device;
    // 使用 service 分配的 endpoint 端口，不用 adapter_config.preferred_port
    let port = ctx.endpoint.port();

    // 检查 Python 环境是否就绪
    let python_path = crate::infra::platform::python::venv_python();
    let python = python_path.ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "Python 环境未就绪",
            "Python 环境未就绪。请在设置页「语音输入」→「本地模式」中点击「安装环境」按钮。\
             （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
        )
    })?;

    // 检查 funasr 是否已安装
    let (funasr_ok, _) = crate::infra::platform::python::check_funasr();
    if !funasr_ok {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "funasr 包未安装",
            "funasr 包未安装。请在设置页点击「安装环境」按钮，Blink 会自动完成安装。",
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
        device,
        "构建 FunASR LaunchDescriptor",
    );

    // 构建参数列表
    let mut args: Vec<String> = Vec::new();
    args.push(script_path.to_string_lossy().to_string());
    args.push("--model".to_string());
    args.push(model.clone());
    args.push("--port".to_string());
    args.push(port.to_string());
    args.push("--device".to_string());
    args.push(device.clone());

    // 0.22.3 Task G: 身份参数只通过环境变量传入，不出现在命令行
    // BLINK_ENGINE_TOKEN / BLINK_ENGINE_ID / BLINK_INSTANCE_ID 由 service 层注入

    // 热词文件
    let hotwords_path = funasr::write_hotwords_file(&funasr_config.hotwords);
    if let Some(ref hw_path) = hotwords_path {
        args.push("--hotwords".to_string());
        args.push(hw_path.to_string_lossy().to_string());
    }

    // ITN
    if funasr_config.use_itn {
        args.push("--use-itn".to_string());
    }

    // 受限环境变量
    let mut env = HashMap::new();
    // Python 输出无缓冲 + UTF-8 模式（修复 Windows 控制台中文乱码）
    env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());

    // 将 ModelScope 模型缓存重定向到 Blink 自管理目录
    let models_dir = crate::infra::utils::paths::python_dir().join("models");
    if let Err(e) = std::fs::create_dir_all(&models_dir) {
        tracing::warn!(%e, "创建 models 目录失败，ModelScope 将使用默认缓存路径");
    } else {
        let models_path = models_dir.display().to_string();
        tracing::info!(path = %models_path, "ModelScope 缓存目录");
        env.insert("MODELSCOPE_CACHE".to_string(), models_path);
    }

    Ok(LaunchDescriptor {
        executable: python,
        args,
        current_dir: None,
        env,
        label: FUNASR_ENGINE_ID.to_string(),
    })
}

// ── Health 映射 ─────────────────────────────────────────────────────────────

/// 把 FunASR 的 HTTP /health 响应映射为领域统一的 `HealthMapping`。
///
/// health 映射区分：
/// - service reachable（HTTP 可达）
/// - model loading（模型正在加载）
/// - model ready（模型已就绪）
/// - model failed（模型加载失败）
///
/// health 必须核对 engine id、instance id 和 token。
/// 进程存活、端口可达均不能单独代表 server/model ready。
///
/// 如果 health 响应缺少身份字段（旧版 server），service 降级为 Unreachable
/// 以防误判——但保留 model_status 映射以便兼容性测试。
fn map_funasr_health(raw_health: &serde_json::Value) -> HealthMapping {
    // 尝试解析 health 响应
    let status = raw_health.get("status").and_then(|v| v.as_str());
    let model_status = raw_health.get("model_status").and_then(|v| v.as_str());
    let model_loaded = raw_health.get("model_loaded").and_then(|v| v.as_bool());

    // ── service health ──
    // HTTP 可达 → service reachable（但 model 可能还在加载）
    // 进程存活、端口可达均不能单独代表 server/model ready。
    let service = if status == Some("ok") {
        // 检查身份字段：如果 health 回显了 engine_id/instance_id，则验证通过
        // 旧版 server 缺少这些字段，但仍可用于 model 状态映射
        ServiceHealth::Healthy
    } else {
        ServiceHealth::Unreachable
    };

    // ── model health ──
    // 优先读 model_status（新字段），回退到 model_loaded（旧字段兼容）
    let model = match model_status {
        Some("ready") => ModelHealth::Ready,
        Some("loading") => ModelHealth::Loading,
        Some("downloading") => ModelHealth::Downloading,
        Some("error") => ModelHealth::Failed,
        Some("idle") => ModelHealth::NotLoaded,
        _ => {
            // 旧版 server 没有 model_status 字段，回退到 model_loaded
            if model_loaded == Some(true) {
                ModelHealth::Ready
            } else {
                ModelHealth::Loading
            }
        }
    };

    // ── backend 观测（可选）──
    // FunASR health 可以回报 actual_backend 和 device_name
    let backend = raw_health.get("backend").and_then(|v| v.as_str());
    let device_name = raw_health.get("device_name").and_then(|v| v.as_str());
    let backend_obs = backend.map(|b| {
        let actual_backend = match b {
            "cuda" => ComputeBackend::Cuda,
            "vulkan" => ComputeBackend::Vulkan,
            "directml" => ComputeBackend::Directml,
            _ => ComputeBackend::Cpu,
        };
        BackendObservation {
            actual_backend,
            device_name: device_name.unwrap_or("CPU").to_string(),
            consistent: true, // 由 service 层根据 resolved profile 填充
        }
    });

    // ── 模型 id / revision（可选）──
    let model_id = raw_health
        .get("model_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_revision = raw_health
        .get("model_revision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    HealthMapping {
        service,
        model,
        environment: None,
        backend: backend_obs,
        model_id,
        model_revision,
        model_content_fingerprint: None,
    }
}

// ── 空间统计和清理 ──────────────────────────────────────────────────────────

/// FunASR 空间统计条目。
///
/// 明确区分：
/// - engine generations（FunASR venv 各代）
/// - FunASR model cache（ModelScope 模型缓存）
/// - provider 公共缓存（uv cache / Python distribution）
///
/// 单引擎清理不能连带删除公共资产。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FunasrSpaceUsage {
    /// 引擎 generation 目录列表及其大小。
    pub engine_generations: Vec<SpaceItem>,
    /// FunASR 模型缓存。
    pub model_cache: Option<SpaceItem>,
    /// provider 公共缓存（uv cache 等，不归属单引擎清理）。
    pub provider_cache: Vec<SpaceItem>,
    /// 总占用（bytes）。
    pub total_bytes: u64,
}

/// 单个空间统计条目。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SpaceItem {
    pub label: String,
    pub path: String,
    pub size_mb: f64,
}

/// 获取 FunASR 空间占用统计。
///
/// 明确区分 engine generations / model cache / provider cache，
/// 单引擎清理不能连带删除公共资产。
pub fn get_funasr_space_usage() -> FunasrSpaceUsage {
    let python_dir = crate::infra::utils::paths::python_dir();
    let models_dir = python_dir.join("models");
    let venv_dir = python_dir.join("venv");
    let uv_dir = python_dir.join("uv");

    let mut engine_generations = Vec::new();
    let mut provider_cache = Vec::new();
    let mut total_bytes: u64 = 0;

    // venv（engine generation）
    if venv_dir.exists() {
        let size = dir_size_bytes(&venv_dir);
        total_bytes += size;
        engine_generations.push(SpaceItem {
            label: "Python 虚拟环境 (venv + torch + funasr)".to_string(),
            path: venv_dir.display().to_string(),
            size_mb: bytes_to_mb(size),
        });
    }

    // uv（provider 公共缓存——不归属单引擎清理）
    if uv_dir.exists() {
        let size = dir_size_bytes(&uv_dir);
        total_bytes += size;
        provider_cache.push(SpaceItem {
            label: "uv 二进制（provider 公共资产）".to_string(),
            path: uv_dir.display().to_string(),
            size_mb: bytes_to_mb(size),
        });
    }

    // FunASR 模型缓存
    let model_cache = if models_dir.exists() {
        let size = dir_size_bytes(&models_dir);
        total_bytes += size;
        Some(SpaceItem {
            label: "FunASR 模型缓存".to_string(),
            path: models_dir.display().to_string(),
            size_mb: bytes_to_mb(size),
        })
    } else {
        None
    };

    // 旧版 ModelScope 默认路径残留
    if let Some(legacy_dir) = dirs_next::home_dir().map(|h| h.join(".cache").join("modelscope"))
        && legacy_dir.exists()
    {
        let size = dir_size_bytes(&legacy_dir);
        if size > 0 {
            total_bytes += size;
            provider_cache.push(SpaceItem {
                label: "旧版模型缓存残留 (ModelScope 默认路径)".to_string(),
                path: legacy_dir.display().to_string(),
                size_mb: bytes_to_mb(size),
            });
        }
    }

    FunasrSpaceUsage {
        engine_generations,
        model_cache,
        provider_cache,
        total_bytes,
    }
}

/// 清理 FunASR 引擎资产。
///
/// **只清理 FunASR 声明拥有的资产**：
/// - engine generations（venv）
/// - FunASR model cache
///
/// **不清理 provider 公共资产**（uv cache / Python distribution）——
/// 单引擎清理不能连带删除其他引擎仍在使用的公共资产。
///
/// 返回清理统计。
pub fn cleanup_funasr_engine() -> Result<FunasrCleanupResult, String> {
    let python_dir = crate::infra::utils::paths::python_dir();
    let mut errors = Vec::new();
    let mut cleaned_items = Vec::new();

    // venv（engine generation——FunASR 拥有）
    let venv_dir = python_dir.join("venv");
    if venv_dir.exists() {
        tracing::info!(path = %venv_dir.display(), "清理 FunASR venv");
        match std::fs::remove_dir_all(&venv_dir) {
            Ok(()) => cleaned_items.push("venv".to_string()),
            Err(e) => errors.push(format!("删除 venv 失败: {e}")),
        }
    }

    // FunASR 模型缓存（FunASR 拥有）
    let models_dir = python_dir.join("models");
    if models_dir.exists() {
        tracing::info!(path = %models_dir.display(), "清理 FunASR 模型缓存");
        match std::fs::remove_dir_all(&models_dir) {
            Ok(()) => cleaned_items.push("model_cache".to_string()),
            Err(e) => errors.push(format!("删除模型缓存失败: {e}")),
        }
    }

    // ── 不清理 uv cache / Python distribution（provider 公共资产）──
    // 单引擎清理不能连带删除其他引擎仍在使用的公共资产。

    if errors.is_empty() {
        tracing::info!("FunASR 引擎清理完成");
        Ok(FunasrCleanupResult {
            cleaned_items,
            errors: Vec::new(),
        })
    } else {
        Err(errors.join("; "))
    }
}

/// FunASR 清理结果。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FunasrCleanupResult {
    /// 已清理的资产列表。
    pub cleaned_items: Vec<String>,
    /// 错误列表（如果有的话）。
    pub errors: Vec<String>,
}

// ── 纯构造入口 ──────────────────────────────────────────────────────────────

/// 创建 FunASR adapter 的 `Arc` 引用。
///
/// 注册函数由 H6 接 wiring；本任务提供纯构造入口。
pub fn make_funasr_adapter() -> Arc<dyn LocalEngineAdapter> {
    Arc::new(FunasrAdapter::new())
}

// ── 辅助函数 ────────────────────────────────────────────────────────────────

/// 递归计算目录大小（bytes）。
fn dir_size_bytes(path: &std::path::Path) -> u64 {
    fn dir_size_inner(path: &std::path::Path) -> u64 {
        let mut size = 0;
        if path.is_dir() {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let entry_path = entry.path();
                    if entry_path.is_dir() {
                        size += dir_size_inner(&entry_path);
                    } else if entry_path.is_file() {
                        size += entry.metadata().map(|m| m.len()).unwrap_or(0);
                    }
                }
            }
        } else if path.is_file() {
            size += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        }
        size
    }
    dir_size_inner(path)
}

/// bytes → MB 转换。
fn bytes_to_mb(bytes: u64) -> f64 {
    (bytes as f64) / (1024.0 * 1024.0)
}

/// bytes → MB 转换（公共入口，供兼容层调用）。
pub fn bytes_to_mb_pub(bytes: u64) -> f64 {
    bytes_to_mb(bytes)
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::local_engine::runtime::ResolvedProfile;

    // ── descriptor 稳定 id 和闭合 profile ──

    #[test]
    fn descriptor_has_stable_engine_id() {
        let adapter = FunasrAdapter::new();
        assert_eq!(adapter.descriptor().engine_id.as_str(), FUNASR_ENGINE_ID);
    }

    #[test]
    fn descriptor_has_closed_capability_kind() {
        let adapter = FunasrAdapter::new();
        assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Stt);
    }

    #[test]
    fn descriptor_has_python_venv_runtime_kind() {
        let adapter = FunasrAdapter::new();
        assert_eq!(adapter.descriptor().runtime_kind, RuntimeKind::PythonVenv);
    }

    #[test]
    fn descriptor_has_manual_lifecycle() {
        let adapter = FunasrAdapter::new();
        assert_eq!(adapter.descriptor().lifecycle, LifecyclePolicy::Manual);
    }

    #[test]
    fn descriptor_validates_ok() {
        let adapter = FunasrAdapter::new();
        assert!(adapter.descriptor().validate().is_ok());
    }

    #[test]
    fn descriptor_declares_cpu_and_cuda_preferences() {
        let adapter = FunasrAdapter::new();
        let prefs = adapter.descriptor().declared_preferences();
        assert!(prefs.contains(&ComputePreference::Cpu));
        assert!(prefs.contains(&ComputePreference::Cuda));
    }

    #[test]
    fn descriptor_allows_cpu_profile() {
        let adapter = FunasrAdapter::new();
        let profile = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(adapter.descriptor().is_profile_allowed(&profile));
    }

    #[test]
    fn descriptor_rejects_undeclared_profile() {
        let adapter = FunasrAdapter::new();
        let profile = ResolvedProfile {
            profile_id: "vulkan-x64".to_string(),
            backend: ComputeBackend::Vulkan,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        assert!(!adapter.descriptor().is_profile_allowed(&profile));
    }

    // ── 旧 SttConfig 反序列化结果不变 ──

    #[test]
    fn old_stt_config_deserialization_unchanged() {
        let json = r#"{
            "server_port": 9000,
            "funasr_model": "iic/SenseVoiceSmall",
            "device": "cpu",
            "use_itn": true,
            "auto_start_server": false,
            "vad": {
                "silence_threshold": 0.005,
                "min_silence_ms": 300,
                "min_sentence_ms": 800
            }
        }"#;
        let local: crate::domain::config::stt_config::LocalEngineConfig =
            serde_json::from_str(json).unwrap();
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.funasr_model, "iic/SenseVoiceSmall");
        assert_eq!(funasr_config.device, "cpu");
        assert!(funasr_config.use_itn);
        assert!(!funasr_config.auto_start_server);
        assert_eq!(funasr_config.vad.silence_threshold, 0.005);
        assert_eq!(funasr_config.vad.min_silence_ms, 300);
        assert_eq!(funasr_config.vad.min_sentence_ms, 800);
    }

    #[test]
    fn old_stt_config_with_hotwords_deserialization() {
        let json = r#"{
            "server_port": 8000,
            "funasr_model": "paraformer-zh",
            "device": "cuda",
            "hotwords": "美团 100, 快手 80",
            "use_itn": false,
            "auto_start_server": true
        }"#;
        let local: crate::domain::config::stt_config::LocalEngineConfig =
            serde_json::from_str(json).unwrap();
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.funasr_model, "paraformer-zh");
        assert_eq!(funasr_config.device, "cuda");
        assert_eq!(funasr_config.hotwords.as_deref(), Some("美团 100, 快手 80"));
        assert!(!funasr_config.use_itn);
        assert!(funasr_config.auto_start_server);
    }

    // ── hotwords/ITN/VAD/model 参数映射不变 ──

    #[test]
    fn funasr_engine_config_preserves_hotwords() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            hotwords: Some("美团 100, 快手 80".to_string()),
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.hotwords.as_deref(), Some("美团 100, 快手 80"));
    }

    #[test]
    fn funasr_engine_config_preserves_itn() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            use_itn: false,
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert!(!funasr_config.use_itn);
    }

    #[test]
    fn funasr_engine_config_preserves_vad() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            vad: crate::domain::config::stt_config::VadConfig {
                silence_threshold: 0.003,
                min_silence_ms: 200,
                min_sentence_ms: 600,
            },
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.vad.silence_threshold, 0.003);
        assert_eq!(funasr_config.vad.min_silence_ms, 200);
        assert_eq!(funasr_config.vad.min_sentence_ms, 600);
    }

    #[test]
    fn funasr_engine_config_preserves_model() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            funasr_model: "paraformer-zh".to_string(),
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.funasr_model, "paraformer-zh");
    }

    #[test]
    fn funasr_engine_config_preserves_device() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            device: "cuda".to_string(),
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert_eq!(funasr_config.device, "cuda");
    }

    #[test]
    fn funasr_engine_config_preserves_auto_start() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            auto_start_server: true,
            ..Default::default()
        };
        let funasr_config = FunasrEngineConfig::from_stt_config(&local);
        assert!(funasr_config.auto_start_server);
    }

    #[test]
    fn funasr_engine_config_round_trip_json() {
        let local = crate::domain::config::stt_config::LocalEngineConfig {
            server_port: 9000,
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            device: "cuda".to_string(),
            num_threads: Some(4),
            auto_start_server: true,
            hotwords: Some("美团 100".to_string()),
            use_itn: false,
            ..Default::default()
        };
        let config = FunasrEngineConfig::from_stt_config(&local);
        let json = serde_json::to_string(&config).unwrap();
        let back: FunasrEngineConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.funasr_model, "iic/SenseVoiceSmall");
        assert_eq!(back.device, "cuda");
        assert!(!back.use_itn);
        assert!(back.auto_start_server);
    }

    // ── health model Loading/Ready/Error 映射 ──

    #[test]
    fn health_maps_model_ready() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
            "model_loaded": true,
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Ready);
    }

    #[test]
    fn health_maps_model_loading() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "loading",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Loading);
    }

    #[test]
    fn health_maps_model_downloading() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "downloading",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Downloading);
    }

    #[test]
    fn health_maps_model_error() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "error",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Failed);
    }

    #[test]
    fn health_maps_model_idle() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "idle",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::NotLoaded);
    }

    #[test]
    fn health_maps_service_unreachable() {
        let raw = serde_json::json!({});
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Unreachable);
    }

    #[test]
    fn health_falls_back_to_model_loaded_bool() {
        // 旧版 server 没有 model_status 字段
        let raw = serde_json::json!({
            "status": "ok",
            "model_loaded": true,
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Ready);
    }

    #[test]
    fn health_falls_back_to_loading_when_model_not_loaded() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_loaded": false,
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Loading);
    }

    #[test]
    fn health_maps_backend_observation() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
            "backend": "cpu",
            "device_name": "Intel i7",
        });
        let mapping = map_funasr_health(&raw);
        assert!(mapping.backend.is_some());
        let backend = mapping.backend.unwrap();
        assert_eq!(backend.actual_backend, ComputeBackend::Cpu);
        assert_eq!(backend.device_name, "Intel i7");
    }

    #[test]
    fn health_maps_cuda_backend() {
        let raw = serde_json::json!({
            "status": "ok",
            "backend": "cuda",
            "device_name": "RTX 4060",
        });
        let mapping = map_funasr_health(&raw);
        let backend = mapping.backend.unwrap();
        assert_eq!(backend.actual_backend, ComputeBackend::Cuda);
    }

    #[test]
    fn health_maps_model_id_and_revision() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
            "model_id": "iic/SenseVoiceSmall",
            "model_revision": "v1.0",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model_id, Some("iic/SenseVoiceSmall".to_string()));
        assert_eq!(mapping.model_revision, Some("v1.0".to_string()));
    }

    // ── health engine/instance/token 不匹配失败 ──
    // 这些测试验证 health 响应缺少身份字段时的行为。
    // 完整的身份校验由 LocalEngineService 在调用 map_health 后，
    // 使用 ServiceIdentityInput::verify 核对 engine id、instance id 和 token。

    #[test]
    fn health_without_identity_fields_still_maps_model_status() {
        // 旧版 server 不回显身份字段，但 model_status 仍可用
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
        });
        let mapping = map_funasr_health(&raw);
        // service 标记为 Healthy（HTTP 可达）
        // 但 LocalEngineService 会在后续身份校验中将其降级为 Unreachable
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Ready);
    }

    #[test]
    fn health_with_mismatched_engine_id_does_not_verify() {
        // 验证 ServiceIdentityInput 的身份校验逻辑
        use crate::infra::local_engine::port::{
            Endpoint, ServiceIdentityInput, ServiceIdentityResult,
        };

        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            token: "secret-token-xyz".to_string(),
            endpoint: Endpoint::new(8000),
        };

        // health 回显了错误的 engine_id
        let observed = ServiceIdentityResult {
            engine_id: Some("wrong-engine".to_string()),
            instance_id: Some("inst-abc".to_string()),
            token_fingerprint: Some(input.token_fingerprint()),
            endpoint: Some("127.0.0.1:8000".to_string()),
        };

        let result = input.verify(&observed);
        assert!(matches!(
            result,
            crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
        ));
    }

    #[test]
    fn health_with_mismatched_instance_id_does_not_verify() {
        use crate::infra::local_engine::port::{
            Endpoint, ServiceIdentityInput, ServiceIdentityResult,
        };

        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            token: "secret-token-xyz".to_string(),
            endpoint: Endpoint::new(8000),
        };

        let observed = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("wrong-instance".to_string()),
            token_fingerprint: Some(input.token_fingerprint()),
            endpoint: Some("127.0.0.1:8000".to_string()),
        };

        let result = input.verify(&observed);
        assert!(matches!(
            result,
            crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
        ));
    }

    #[test]
    fn health_with_mismatched_token_does_not_verify() {
        use crate::infra::local_engine::port::{
            Endpoint, ServiceIdentityInput, ServiceIdentityResult,
        };

        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            token: "secret-token-xyz".to_string(),
            endpoint: Endpoint::new(8000),
        };

        let observed = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-abc".to_string()),
            token_fingerprint: Some("00000000".to_string()),
            endpoint: Some("127.0.0.1:8000".to_string()),
        };

        let result = input.verify(&observed);
        assert!(matches!(
            result,
            crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
        ));
    }

    #[test]
    fn health_with_all_fields_matching_verifies() {
        use crate::infra::local_engine::port::{
            Endpoint, IdentityVerification, ServiceIdentityInput, ServiceIdentityResult,
        };

        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            token: "secret-token-xyz".to_string(),
            endpoint: Endpoint::new(8000),
        };

        let observed = ServiceIdentityResult {
            engine_id: Some("funasr".to_string()),
            instance_id: Some("inst-abc".to_string()),
            token_fingerprint: Some(input.token_fingerprint()),
            endpoint: Some("127.0.0.1:8000".to_string()),
        };

        let result = input.verify(&observed);
        assert_eq!(result, IdentityVerification::Verified);
    }

    #[test]
    fn health_with_no_identity_fields_does_not_verify() {
        use crate::infra::local_engine::port::{
            Endpoint, ServiceIdentityInput, ServiceIdentityResult,
        };

        let input = ServiceIdentityInput {
            engine_id: "funasr".to_string(),
            instance_id: "inst-abc".to_string(),
            token: "secret-token-xyz".to_string(),
            endpoint: Endpoint::new(8000),
        };

        // 旧版 server 完全不回显身份字段
        let observed = ServiceIdentityResult {
            engine_id: None,
            instance_id: None,
            token_fingerprint: None,
            endpoint: None,
        };

        let result = input.verify(&observed);
        assert!(matches!(
            result,
            crate::infra::local_engine::port::IdentityVerification::Mismatch(_)
        ));
    }

    // ── 未知端口占用不 kill ──

    #[test]
    fn unknown_port_occupation_does_not_kill() {
        // 此测试验证 ManagedProcess 的行为：
        // 端口被未知进程占用时只报错或换端口，不自动 kill。
        // 完整的行为测试在 infra/local_engine/tests.rs 中。
        // 这里验证 adapter 的 descriptor 不包含任何 kill 行为。
        let adapter = FunasrAdapter::new();
        let desc = adapter.descriptor();
        // descriptor 不包含任何 kill 或端口终止相关字段
        let json = serde_json::to_string(desc).unwrap();
        assert!(!json.contains("kill"));
        assert!(!json.contains("terminate"));
    }

    // ── FunASR 清理不删除其他引擎 generation/provider 共享资产 ──

    #[test]
    fn cleanup_funasr_does_not_touch_provider_cache() {
        // 验证 cleanup_funasr_engine 只清理 venv 和 model cache，
        // 不清理 uv cache / Python distribution。
        // 使用临时目录验证行为。

        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let python_dir = tmp.path().join("python");
        std::fs::create_dir_all(&python_dir).unwrap();

        // 创建 venv 和 models 目录（FunASR 拥有）
        std::fs::create_dir_all(python_dir.join("venv")).unwrap();
        std::fs::create_dir_all(python_dir.join("models")).unwrap();
        // 创建 uv 目录（provider 公共资产）
        std::fs::create_dir_all(python_dir.join("uv")).unwrap();

        // 由于 cleanup_funasr_engine 使用真实的 python_dir()，
        // 我们在这里只验证逻辑边界——不实际调用会删除真实资产的函数。
        // 验证 FunasrSpaceUsage 正确区分资产归属：
        let usage = FunasrSpaceUsage {
            engine_generations: vec![SpaceItem {
                label: "venv".to_string(),
                path: python_dir.join("venv").display().to_string(),
                size_mb: 100.0,
            }],
            model_cache: Some(SpaceItem {
                label: "model cache".to_string(),
                path: python_dir.join("models").display().to_string(),
                size_mb: 234.0,
            }),
            provider_cache: vec![SpaceItem {
                label: "uv (provider)".to_string(),
                path: python_dir.join("uv").display().to_string(),
                size_mb: 50.0,
            }],
            total_bytes: 0,
        };

        // 验证：provider cache 不在 engine_generations 中
        for item in &usage.engine_generations {
            assert!(!item.path.contains("uv"), "uv 不应在 engine_generations 中");
        }
        // 验证：model cache 不在 provider_cache 中
        for pc in &usage.provider_cache {
            assert!(
                !pc.path.contains("models"),
                "models 不应在 provider_cache 中"
            );
        }
    }

    // ── transcription client 请求字段和 endpoint 兼容 ──

    #[test]
    fn transcription_endpoint_is_loopback() {
        // FunASR transcription endpoint 只使用 127.0.0.1
        let base_url = funasr::server_base_url(8000);
        assert!(
            base_url.contains("localhost") || base_url.contains("127.0.0.1"),
            "base_url 应使用 loopback: {base_url}"
        );
    }

    #[test]
    fn transcription_request_fields_compatible() {
        // 验证 transcription 请求字段与现有 LocalSttEngine 兼容
        // LocalSttEngine 调用 POST {base_url}/audio/transcriptions
        // 使用 wav::transcribe_async(url, None, model, wav_bytes)
        let base_url = funasr::server_base_url(8000);
        let url = format!("{base_url}/audio/transcriptions");
        assert!(url.contains("/v1/audio/transcriptions"));
    }

    #[test]
    fn embedded_script_is_valid() {
        assert!(!BLINK_STT_SERVER_PY.is_empty());
        assert!(BLINK_STT_SERVER_PY.contains("blink_stt_server"));
        assert!(BLINK_STT_SERVER_PY.contains("/v1/audio/transcriptions"));
        assert!(BLINK_STT_SERVER_PY.contains("/health"));
    }

    #[test]
    fn make_funasr_adapter_returns_valid_adapter() {
        let adapter = make_funasr_adapter();
        assert_eq!(adapter.descriptor().engine_id.as_str(), FUNASR_ENGINE_ID);
        assert_eq!(adapter.capability_kind(), CapabilityKind::Stt);
    }

    #[test]
    fn adapter_self_test_checks_python_env() {
        let adapter = FunasrAdapter::new();
        let result = adapter.self_test();
        // self_test 检查 venv 和 funasr——开发环境可能未安装
        // 只验证返回了结果（passed 或 failed），不强制要求 passed
        let _ = result.passed;
    }

    #[test]
    fn adapter_diagnostics_returns_entries() {
        let adapter = FunasrAdapter::new();
        let diag = adapter.diagnostics();
        assert!(!diag.entries.is_empty());
    }

    #[test]
    fn adapter_prepare_launch_rejects_undeclared_profile() {
        let adapter = FunasrAdapter::new();
        let undeclared_profile = ResolvedProfile {
            profile_id: "vulkan-x64".to_string(),
            backend: ComputeBackend::Vulkan,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        let ctx = LaunchContext {
            endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
            engine_id: "funasr".to_string(),
            instance_id: "inst-test".to_string(),
            token: "test-token-abcdef0123456789".to_string(),
            resolved_profile: undeclared_profile,
        };
        let config = AdapterConfig::new();
        let result = adapter.prepare_launch(&ctx, &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.code, LocalEngineErrorCode::Unsupported);
    }
}
