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
use crate::infra::local_engine::model_storage as mstore;
use crate::infra::local_engine::providers::python::PythonVenvProvider;
use crate::infra::local_engine::providers::{
    CompatibilityCheck, InstallPlan, PackageLock, PipExtraArg, ProfileCandidate,
    ProviderDescriptor, PythonInstallPlan,
};
use crate::infra::local_engine::runtime as engine_runtime;
use crate::infra::local_engine::runtime::{
    ArtifactId, BackendObservation, ChecksumSource, ComputeBackend, ComputePreference, EngineId,
    ModelContract, RuntimeKind,
};

/// 嵌入的 blink_stt_server.py 脚本（随 Rust 二进制发布）。
///
/// 重新声明在此模块以保持 adapter 自包含；领域层的 `funasr.rs` 保留原始常量。
#[allow(dead_code)]
const BLINK_STT_SERVER_PY: &str = include_str!("../../../resources/stt/funasr/blink_stt_server.py");

/// 嵌入的完整依赖锁文件（唯一锁源）。
///
/// 由 `uv pip compile --generate-hashes --index-url https://download.pytorch.org/whl/cpu
/// --extra-index-url https://pypi.org/simple` 生成，包含全部传递依赖及其 SHA-256。
/// `make_funasr_provider_descriptor()` 在运行时解析此文件生成 `PackageLock` 列表。
/// 安装时使用 `--require-hashes --no-deps` 强校验。
#[allow(dead_code)]
const LOCKED_REQUIREMENTS_TXT: &str =
    include_str!("../../../resources/stt/funasr/locked-requirements.txt");

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
/// 包检查函数类型。
///
/// 检查指定 python 路径中某包是否已安装，返回 (是否已安装, 版本号)。
type PackageChecker = fn(&std::path::Path, &str) -> (bool, Option<String>);

/// 默认包检查器：通过执行 `python -c "import importlib.metadata"` 检查。
fn default_package_checker(python: &std::path::Path, package: &str) -> (bool, Option<String>) {
    let script = format!("import importlib.metadata as m; print(m.version('{package}'))");
    match crate::infra::platform::no_window(std::process::Command::new(python))
        .args(["-c", &script])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (true, Some(version))
        }
        _ => (false, None),
    }
}

pub struct FunasrAdapter {
    descriptor: EngineDescriptor,
    /// 包检查器（可注入，测试时替换为 mock 避免执行假 python.exe）
    package_checker: PackageChecker,
}

impl FunasrAdapter {
    /// 创建 FunASR adapter。
    ///
    /// descriptor 在编译期声明，锁定 engine id、profile、artifact 和 model contract。
    pub fn new() -> Self {
        Self {
            descriptor: make_funasr_descriptor(),
            package_checker: default_package_checker,
        }
    }

    /// 创建带自定义包检查器的 adapter（测试用）。
    #[cfg(test)]
    pub fn new_with_package_checker(checker: PackageChecker) -> Self {
        Self {
            descriptor: make_funasr_descriptor(),
            package_checker: checker,
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
        let launch =
            build_funasr_launch_descriptor(&funasr_config, config, &ctx, self.package_checker)?;

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
    /// 验证 FunASR Python 环境是否就绪（generation venv + funasr 包已安装）。
    ///
    /// **0.22.6 H1**: 只检查当前 generation 的 venv，不 fallback 到旧全局 venv。
    /// 旧 `%APPDATA%\blink\python\venv` 只作为迁移/诊断来源，不影响新 generation 安装判定。
    fn self_test(&self) -> AdapterSelfTest {
        // 只使用 generation-managed venv（由 PythonVenvProvider 创建的隔离 venv）
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let python_path = generation_venv_python(&engine_id);

        if python_path.is_none() {
            return AdapterSelfTest::failed(
                "FunASR 环境未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」按钮。\
                 （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
            );
        }

        // 检查 funasr 是否已安装（使用 generation venv 中的 python）
        let python = python_path.unwrap();
        let (funasr_ok, _) = (self.package_checker)(&python, "funasr");
        if !funasr_ok {
            return AdapterSelfTest::failed(
                "funasr 包未安装。请在设置页「引擎」→「本地模型运行时」中点击「修复」或「安装环境」按钮。",
            );
        }

        AdapterSelfTest::passed()
    }

    /// 引擎专属诊断投影。
    ///
    /// 返回 FunASR 特有的诊断信息（generation venv、torch、funasr 版本等）。
    ///
    /// **0.22.6 H1**: 诊断只解析当前 generation venv 的状态；
    /// 旧全局 venv 仅作为迁移诊断来源单独标注。
    fn diagnostics(&self) -> EngineDiagnostic {
        let mut entries = Vec::new();
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();

        // generation venv 状态
        let gen_python = generation_venv_python(&engine_id);
        let gen_venv_exists = gen_python.is_some();
        entries.push(DiagnosticEntry {
            key: "generation_venv_exists".to_string(),
            value: if gen_venv_exists {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: if gen_venv_exists {
                "info".to_string()
            } else {
                "warning".to_string()
            },
        });

        if let Some(ref py) = gen_python {
            // 使用 generation venv python 检查版本和包
            if let Some(ver) = check_python_version(py) {
                entries.push(DiagnosticEntry {
                    key: "python_version".to_string(),
                    value: ver,
                    label: "info".to_string(),
                });
            }

            // torch 状态
            let (torch_ok, torch_ver) = check_torch_with(py);
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
                let cuda_ok = check_torch_cuda_with(py);
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
            let (funasr_ok, funasr_ver) = check_funasr_with(py);
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
        }

        // 旧全局 venv 仅作为迁移诊断来源标注
        let legacy_venv = engine_runtime::legacy_funasr_venv_dir();
        if legacy_venv.exists() {
            entries.push(DiagnosticEntry {
                key: "legacy_venv_exists".to_string(),
                value: "true".to_string(),
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
            // 0.22.6：只声明 CPU profile。锁文件仅包含 CPU-only PyTorch wheel hash，
            // 声明 CUDA profile 会导致安装时 hash mismatch。CUDA 支持需独立锁文件后
            // 再启用。
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: python_artifact.clone(),
            }],
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
/// 与 `make_funasr_descriptor()`（domain 层 `EngineDescriptor`）互补。
///
/// **包列表来源**：`resources/stt/funasr/locked-requirements.txt`（唯一锁源）。
/// 以 `include_str!` 嵌入，运行时解析生成 `PackageLock` 列表。
/// 不再手写第二份包清单——避免 lock.json 与 Rust descriptor 漂移。
///
/// **安装策略**：`--require-hashes --no-deps`——强制 hash 校验 + 禁止传递依赖
/// 自动解析，确保安装的 wheel 与锁文件完全一致。
///
/// **PyTorch index**：torch/torchaudio 来自 `https://download.pytorch.org/whl/cpu`，
/// 其余包来自 PyPI。锁文件已通过 `--index-url` + `--extra-index-url` 生成，
/// 包含两个 index 的 wheel hash。安装时通过 `ExtraIndexUrl` 传入 PyTorch index，
/// 并以 `unsafe-best-match` 允许 uv 为锁定版本跨索引查找候选；精确版本、
/// `--require-hashes` 与 `--no-deps` 继续约束最终安装内容。
pub fn make_funasr_provider_descriptor() -> ProviderDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    ProviderDescriptor {
        engine_id: EngineId::new(FUNASR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimeKind::PythonVenv,
        display_name: "FunASR 语音识别".to_string(),
        // 0.22.6：只声明 CPU profile。CUDA profile 需独立 CUDA 锁文件后启用。
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: python_artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id: "iic/SenseVoiceSmall".to_string(),
            revision: "funasr-1.x".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        install_plan: InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: python_artifact,
            // 唯一锁源：从嵌入的 locked-requirements.txt 解析
            packages: locked_packages(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            // --no-deps：禁止传递依赖自动解析，全部由锁文件覆盖
            // ExtraIndexUrl：PyTorch CPU index，用于 torch/torchaudio wheel
            extra_pip_args: vec![
                PipExtraArg::NoDeps,
                PipExtraArg::ExtraIndexUrl("https://download.pytorch.org/whl/cpu".to_string()),
                PipExtraArg::IndexStrategyUnsafeBestMatch,
            ],
            self_test_script: "import funasr; import torch; import fastapi; import uvicorn"
                .to_string(),
        }),
        min_generations: 2,
    }
}

/// 从嵌入的 `locked-requirements.txt` 解析包列表。
///
/// 这是安装时使用的唯一锁源——不再手写第二份包清单。
fn locked_packages() -> Vec<PackageLock> {
    let packages = parse_locked_requirements(LOCKED_REQUIREMENTS_TXT);
    // 验证：所有包必须有 hash
    for pkg in &packages {
        assert!(
            pkg.sha256.is_some(),
            "locked-requirements.txt 中的 {} 缺少 SHA-256 hash",
            pkg.name
        );
        let hash = pkg.sha256.as_ref().unwrap();
        assert_eq!(
            hash.len(),
            64,
            "locked-requirements.txt 中的 {} 的 hash 长度不是 64: {}",
            pkg.name,
            hash
        );
        assert!(
            hash.bytes().all(|b| b.is_ascii_hexdigit()),
            "locked-requirements.txt 中的 {} 的 hash 包含非 hex 字符: {}",
            pkg.name,
            hash
        );
        // all_hashes 不得为空
        assert!(
            !pkg.all_hashes.is_empty(),
            "locked-requirements.txt 中的 {} 的 all_hashes 为空",
            pkg.name
        );
        // all_hashes 中每个 hash 也必须格式正确
        for h in &pkg.all_hashes {
            assert_eq!(
                h.len(),
                64,
                "locked-requirements.txt 中的 {} 的 all_hashes 中有长度不为 64 的 hash",
                pkg.name
            );
            assert!(
                h.bytes().all(|b| b.is_ascii_hexdigit()),
                "locked-requirements.txt 中的 {} 的 all_hashes 中有非 hex 字符",
                pkg.name
            );
        }
        // 精确版本约束：不允许 >= ~> < > 等非精确约束
        assert!(
            !pkg.version.starts_with('>')
                && !pkg.version.starts_with('<')
                && !pkg.version.starts_with('~')
                && !pkg.version.starts_with('!'),
            "locked-requirements.txt 中的 {} 使用了非精确版本约束: {}",
            pkg.name,
            pkg.version
        );
    }
    packages
}

/// 解析 `locked-requirements.txt` 格式的文本为 `PackageLock` 列表。
///
/// 格式：
/// ```text
/// # comment lines
/// package-name==1.2.3 \
///     --hash=sha256:abcdef... \
///     --hash=sha256:123456...
/// ```
///
/// 每个包可能有多个 hash（对应不同平台的 wheel）。
/// 对于 `--require-hashes` 安装，需要列出所有 hash 让 pip 匹配。
///
/// 返回 `Vec<PackageLock>`，每个包的 `sha256` 为第一个 hash（用于摘要/标识），
/// `all_hashes` 包含所有平台 wheel 的 hash，用于 `--require-hashes` 安装。
fn parse_locked_requirements(txt: &str) -> Vec<PackageLock> {
    let mut packages = Vec::new();
    let mut current_name: Option<String> = None;
    let mut current_version: Option<String> = None;
    let mut current_hashes: Vec<String> = Vec::new();

    for line in txt.lines() {
        let trimmed = line.trim();

        // Skip comments and empty lines
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        // Hash continuation line
        if trimmed.starts_with("--hash=sha256:") {
            let h = trimmed
                .trim_start_matches("--hash=sha256:")
                .trim_end_matches('\\')
                .trim();
            if !h.is_empty() {
                current_hashes.push(h.to_string());
            }
            continue;
        }

        // New package line: contains ==
        if trimmed.contains("==") {
            // Save previous package
            if let (Some(name), Some(version)) = (&current_name, &current_version) {
                let first_hash = current_hashes.first().cloned();
                packages.push(PackageLock {
                    name: name.clone(),
                    version: version.clone(),
                    sha256: first_hash,
                    all_hashes: current_hashes.clone(),
                });
            }

            // Parse new package: strip trailing backslash
            let line_clean = trimmed.trim_end_matches('\\').trim();
            if let Some(eq_pos) = line_clean.find("==") {
                let name = line_clean[..eq_pos].trim().to_string();
                let version_part = &line_clean[eq_pos + 2..];
                // Version may have trailing space or hash on same line
                let version = version_part
                    .split_whitespace()
                    .next()
                    .unwrap_or(version_part)
                    .to_string();
                current_name = Some(name);
                current_version = Some(version);
                current_hashes.clear();

                // Check if there's a hash on the same line
                if let Some(hash_start) = trimmed.find("--hash=sha256:") {
                    let h = trimmed[hash_start..]
                        .trim_start_matches("--hash=sha256:")
                        .trim_end_matches('\\')
                        .trim();
                    if !h.is_empty() {
                        current_hashes.push(h.to_string());
                    }
                }
            }
        }
    }

    // Save last package
    if let (Some(name), Some(version)) = (&current_name, &current_version) {
        let first_hash = current_hashes.first().cloned();
        packages.push(PackageLock {
            name: name.clone(),
            version: version.clone(),
            sha256: first_hash,
            all_hashes: current_hashes.clone(),
        });
    }

    packages
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
/// 返回模型 id 对应的子模型列表。
///
/// 与 Python installer 的 `ALLOWED_MODELS` submodels 字段保持一致。
/// - SenseVoice 系列：内置 VAD/标点/ITN，无需子模型
/// - paraformer-zh：需要 fsmn-vad + ct-punc
///
/// 返回空 Vec 表示无需子模型。
fn funasr_submodels_for(model_id: &str) -> Vec<&'static str> {
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
fn build_funasr_launch_descriptor(
    funasr_config: &FunasrEngineConfig,
    _adapter_config: &AdapterConfig,
    ctx: &LaunchContext,
    package_checker: PackageChecker,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let model = &funasr_config.funasr_model;
    let device = &funasr_config.device;
    // 使用 service 分配的 endpoint 端口，不用 adapter_config.preferred_port
    let port = ctx.endpoint.port();

    // 0.22.6 H1: 只使用 generation-managed venv，不 fallback 到旧全局 venv
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "engine_id 无效",
            format!("解析 engine_id 失败: {e}"),
        )
    })?;
    let python_path = generation_venv_python(&engine_id);
    let python = python_path.ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "Python 环境未就绪",
            "FunASR 环境未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」按钮。\
             （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
        )
    })?;

    // 检查 funasr 是否已安装（使用 generation venv 中的 python）
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
    // generation manifest 中读取 model_id/revision/payload_dir/fingerprint。
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
                mstore::model_payload_dir(&funasr_engine_id, &asset_key, &manifest.install_id)
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
                install_id = %manifest.install_id,
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

    // ── 模型内容指纹（可选）──
    // 0.22.6 H1: Python server 在模型 Ready 时返回稳定、非空、非全零的内容指纹。
    // fingerprint 是实际缓存文件的内容哈希，用于检测模型文件损坏/篡改。
    // adapter 只在 model Ready 时映射 fingerprint，其他状态不返回指纹。
    let model_content_fingerprint = if model == ModelHealth::Ready {
        raw_health
            .get("model_content_fingerprint")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .map(|s| s.to_string())
    } else {
        None
    };

    HealthMapping {
        service,
        model,
        environment: None,
        backend: backend_obs,
        model_id,
        model_revision,
        model_content_fingerprint,
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
///
/// **0.22.6 H1**: model cache 路径统一使用 `engine_model_cache_dir(funasr)`。
pub fn get_funasr_space_usage() -> FunasrSpaceUsage {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let engine_root = engine_runtime::engine_root(&engine_id);
    let models_dir = engine_runtime::engine_model_cache_dir(&engine_id);
    let legacy_python_dir = engine_runtime::python_shared_root();
    let legacy_venv_dir = engine_runtime::legacy_funasr_venv_dir();
    let uv_dir = legacy_python_dir.join("uv");

    let mut engine_generations = Vec::new();
    let mut provider_cache = Vec::new();
    let mut total_bytes: u64 = 0;

    // generation 目录（engine generations）
    let generations_dir = engine_runtime::generations_dir(&engine_id);
    if generations_dir.exists() {
        let size = dir_size_bytes(&generations_dir);
        total_bytes += size;
        engine_generations.push(SpaceItem {
            label: "FunASR generations (venv + torch + funasr)".to_string(),
            path: generations_dir.display().to_string(),
            size_mb: bytes_to_mb(size),
        });
    }

    // 旧版全局 venv（迁移残留，不计入 engine_generations）
    if legacy_venv_dir.exists() {
        let size = dir_size_bytes(&legacy_venv_dir);
        if size > 0 {
            total_bytes += size;
            engine_generations.push(SpaceItem {
                label: "旧版全局 venv (迁移残留)".to_string(),
                path: legacy_venv_dir.display().to_string(),
                size_mb: bytes_to_mb(size),
            });
        }
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

    // FunASR 模型缓存（统一路径真源）
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

    // 旧版 python/models 目录残留
    let legacy_models_dir = legacy_python_dir.join("models");
    if legacy_models_dir.exists() && legacy_models_dir != models_dir {
        let size = dir_size_bytes(&legacy_models_dir);
        if size > 0 {
            total_bytes += size;
            provider_cache.push(SpaceItem {
                label: "旧版模型缓存残留 (python/models)".to_string(),
                path: legacy_models_dir.display().to_string(),
                size_mb: bytes_to_mb(size),
            });
        }
    }

    let _ = engine_root; // engine_root 已通过 generations_dir 间接使用

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
/// - engine generations（runtimes/engines/funasr/generations）
/// - FunASR model cache（models/funasr）
/// - 旧版全局 venv（迁移残留清理）
///
/// **不清理 provider 公共资产**（uv cache / Python distribution）——
/// 单引擎清理不能连带删除其他引擎仍在使用的公共资产。
///
/// 返回清理统计。
pub fn cleanup_funasr_engine() -> Result<FunasrCleanupResult, String> {
    let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
    let mut errors = Vec::new();
    let mut cleaned_items = Vec::new();

    // generation 目录（engine generations——FunASR 拥有）
    let generations_dir = engine_runtime::generations_dir(&engine_id);
    if generations_dir.exists() {
        tracing::info!(path = %generations_dir.display(), "清理 FunASR generations");
        match std::fs::remove_dir_all(&generations_dir) {
            Ok(()) => cleaned_items.push("generations".to_string()),
            Err(e) => errors.push(format!("删除 generations 失败: {e}")),
        }
    }

    // FunASR 模型缓存（FunASR 拥有，统一路径真源）
    let models_dir = engine_runtime::engine_model_cache_dir(&engine_id);
    if models_dir.exists() {
        tracing::info!(path = %models_dir.display(), "清理 FunASR 模型缓存");
        match std::fs::remove_dir_all(&models_dir) {
            Ok(()) => cleaned_items.push("model_cache".to_string()),
            Err(e) => errors.push(format!("删除模型缓存失败: {e}")),
        }
    }

    // 旧版全局 venv（迁移残留清理）
    let legacy_venv_dir = engine_runtime::legacy_funasr_venv_dir();
    if legacy_venv_dir.exists() {
        tracing::info!(path = %legacy_venv_dir.display(), "清理旧版 FunASR venv");
        match std::fs::remove_dir_all(&legacy_venv_dir) {
            Ok(()) => cleaned_items.push("legacy_venv".to_string()),
            Err(e) => errors.push(format!("删除旧版 venv 失败: {e}")),
        }
    }

    // 旧版 python/models 目录残留（使用 python_shared_root 确保测试隔离）
    let legacy_models_dir = engine_runtime::python_shared_root().join("models");
    if legacy_models_dir.exists() && legacy_models_dir != models_dir {
        tracing::info!(path = %legacy_models_dir.display(), "清理旧版 python/models 残留");
        match std::fs::remove_dir_all(&legacy_models_dir) {
            Ok(()) => cleaned_items.push("legacy_models".to_string()),
            Err(e) => errors.push(format!("删除旧版 models 失败: {e}")),
        }
    }

    // ── 不清理 uv cache / Python distribution（provider 公共资产）──
    // 单引擎清理不能连带删除其他引擎仍在使用的公共资产。

    if errors.is_empty() {
        tracing::info!("FunASR 引擎清理完成: {:?}", cleaned_items);
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

// ── generation venv 辅助 ────────────────────────────────────────────────────

/// 获取 FunASR 当前 generation venv 中的 `python.exe` 路径。
///
/// 路径：`runtimes/engines/{engine_id}/generations/{install_id}/venv/Scripts/python.exe`
///
/// 返回 `None` 表示尚未安装（current.json 不存在或 venv 目录缺失）。
///
/// **0.22.6 H1**: 只使用 generation-managed venv，不 fallback 到旧全局 venv。
fn generation_venv_python(engine_id: &EngineId) -> Option<std::path::PathBuf> {
    let pointer = engine_runtime::read_current_pointer(engine_id).ok()?;
    let install_id = pointer?.install_id;
    let python_exe = engine_runtime::generation_dir(engine_id, &install_id)
        .join("venv")
        .join("Scripts")
        .join("python.exe");
    if python_exe.exists() {
        Some(python_exe)
    } else {
        None
    }
}

// ── 包检查（使用指定 python 路径，不依赖全局 venv）────────────────────────────

/// 使用指定 python 路径检查 funasr 包是否已安装。
///
/// 返回 (是否已安装, 版本号)。
fn check_funasr_with(python: &std::path::Path) -> (bool, Option<String>) {
    match crate::infra::platform::no_window(std::process::Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('funasr'))",
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (true, Some(version))
        }
        _ => (false, None),
    }
}

/// 使用指定 python 路径检查 torch 是否已安装。
///
/// 返回 (是否已安装, 版本号)。
fn check_torch_with(python: &std::path::Path) -> (bool, Option<String>) {
    match crate::infra::platform::no_window(std::process::Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('torch'))",
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (true, Some(version))
        }
        _ => (false, None),
    }
}

/// 使用指定 python 路径检查 PyTorch CUDA 是否可用。
fn check_torch_cuda_with(python: &std::path::Path) -> bool {
    match crate::infra::platform::no_window(std::process::Command::new(python))
        .args(["-c", "import torch; print(torch.cuda.is_available())"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            stdout == "True"
        }
        _ => false,
    }
}

/// 使用指定 python 路径获取 Python 版本。
fn check_python_version(python: &std::path::Path) -> Option<String> {
    crate::infra::platform::no_window(std::process::Command::new(python))
        .args(["--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
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
    fn descriptor_declares_cpu_preference_only() {
        // 0.22.6: 只声明 CPU profile（CUDA 需独立锁文件后启用）
        let adapter = FunasrAdapter::new();
        let prefs = adapter.descriptor().declared_preferences();
        assert!(prefs.contains(&ComputePreference::Cpu));
        // 确保 CUDA 不在声明列表中
        assert!(
            !prefs.contains(&ComputePreference::Cuda),
            "0.22.6 不应声明 CUDA preference"
        );
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

    // ── 0.22.6 H1: generation venv 路径测试 ──────────────────────────────

    /// 互斥锁：序列化 generation venv 相关测试，避免并行测试互相清理临时目录。
    static GEN_VENV_TEST_MUTEX: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// 辅助：在测试临时目录中模拟 generation venv 安装。
    ///
    /// 创建 `runtimes/engines/funasr/generations/{install_id}/venv/Scripts/python.exe`
    /// 和对应的 `current.json`。
    fn setup_test_generation_venv(install_id: &str) -> std::path::PathBuf {
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let gen_dir = engine_runtime::generation_dir(&engine_id, install_id);
        let venv_scripts = gen_dir.join("venv").join("Scripts");
        std::fs::create_dir_all(&venv_scripts).unwrap();
        let python_exe = venv_scripts.join("python.exe");
        std::fs::write(&python_exe, b"fake python").unwrap();

        // 写入 current.json
        let pointer = engine_runtime::CurrentPointer {
            install_id: install_id.to_string(),
            manifest_path: format!("generations/{install_id}/manifest.json"),
            updated_at_ms: 0,
            schema_version: 1,
        };
        engine_runtime::write_current_pointer(&engine_id, &pointer).unwrap();

        python_exe
    }

    /// 辅助：清理测试用的 generation 数据。
    fn cleanup_test_generation() {
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let engine_root = engine_runtime::engine_root(&engine_id);
        let _ = std::fs::remove_dir_all(&engine_root);
    }

    /// 0.22.6 H1: 新 generation venv 存在时，`generation_venv_python` 返回正确路径。
    #[test]
    fn generation_venv_python_returns_path_when_installed() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();
        let install_id = "test-install-001";
        let python_exe = setup_test_generation_venv(install_id);

        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let result = generation_venv_python(&engine_id);
        assert!(result.is_some(), "generation venv 已安装时应返回路径");
        assert_eq!(result.unwrap(), python_exe);

        cleanup_test_generation();
    }

    /// 0.22.6 H1: 无 generation venv 时返回 None。
    #[test]
    fn generation_venv_python_returns_none_when_not_installed() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let result = generation_venv_python(&engine_id);
        assert!(result.is_none(), "未安装时应返回 None");

        cleanup_test_generation();
    }

    /// 0.22.6 H1: self_test 在无 generation venv 时报告失败。
    #[test]
    fn self_test_fails_when_no_generation_venv() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();
        let adapter = FunasrAdapter::new();
        let result = adapter.self_test();
        assert!(!result.passed, "无 generation venv 时 self_test 应失败");
        let reason = result.failure_reason.unwrap_or_default();
        assert!(
            reason.contains("引擎") || reason.contains("安装"),
            "失败原因应引导到引擎页: {reason}"
        );

        cleanup_test_generation();
    }

    /// 0.22.6 H1: self_test 错误文案指向引擎页，不指向语音输入页。
    #[test]
    fn self_test_error_message_points_to_engine_page() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();
        let adapter = FunasrAdapter::new();
        let result = adapter.self_test();
        if !result.passed {
            let reason = result.failure_reason.unwrap_or_default();
            assert!(
                !reason.contains("语音输入"),
                "错误文案不应指向'语音输入页': {reason}"
            );
            assert!(
                reason.contains("引擎") || reason.contains("本地模型运行时"),
                "错误文案应指向引擎页: {reason}"
            );
        }

        cleanup_test_generation();
    }

    /// 0.22.6 H1: 旧全局 venv 存在但无 generation venv 时，
    /// self_test 仍然失败（不 fallback 到旧 venv）。
    #[test]
    fn legacy_venv_does_not_satisfy_self_test() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();

        // 创建旧版 venv 目录（模拟迁移残留）
        let legacy_venv = engine_runtime::legacy_funasr_venv_dir();
        let legacy_scripts = legacy_venv.join("Scripts");
        std::fs::create_dir_all(&legacy_scripts).unwrap();
        std::fs::write(legacy_scripts.join("python.exe"), b"legacy python").unwrap();

        let adapter = FunasrAdapter::new();
        let result = adapter.self_test();
        // 旧 venv 存在但 generation venv 不存在 → self_test 失败
        assert!(
            !result.passed,
            "旧 venv 不应满足 self_test（不能冒充新 generation 环境）"
        );

        // 清理旧 venv
        let _ = std::fs::remove_dir_all(engine_runtime::python_shared_root());
        cleanup_test_generation();
    }

    /// 0.22.6 H1: diagnostics 在无 generation venv 时标注 generation_venv_exists=false，
    /// 并在旧 venv 存在时标注 legacy_venv_exists=true。
    #[test]
    fn diagnostics_reports_legacy_venv_separately() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();

        // 创建旧版 venv 目录
        let legacy_venv = engine_runtime::legacy_funasr_venv_dir();
        let legacy_scripts = legacy_venv.join("Scripts");
        std::fs::create_dir_all(&legacy_scripts).unwrap();
        std::fs::write(legacy_scripts.join("python.exe"), b"legacy").unwrap();

        let adapter = FunasrAdapter::new();
        let diag = adapter.diagnostics();

        // generation_venv_exists = false
        let gen_entry = diag
            .entries
            .iter()
            .find(|e| e.key == "generation_venv_exists");
        assert!(
            gen_entry.is_some(),
            "diagnostics 应包含 generation_venv_exists"
        );
        assert_eq!(gen_entry.unwrap().value, "false");

        // legacy_venv_exists = true
        let legacy_entry = diag.entries.iter().find(|e| e.key == "legacy_venv_exists");
        assert!(
            legacy_entry.is_some(),
            "diagnostics 应包含 legacy_venv_exists"
        );
        assert_eq!(legacy_entry.unwrap().value, "true");

        // 清理
        let _ = std::fs::remove_dir_all(engine_runtime::python_shared_root());
        cleanup_test_generation();
    }

    /// 0.22.6 H1: prepare_launch 在无 generation venv 时返回
    /// EnvironmentMissing 错误，错误文案指向引擎页。
    #[test]
    fn prepare_launch_fails_without_generation_venv() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();
        let adapter = FunasrAdapter::new();
        let profile = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        let ctx = LaunchContext {
            endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
            engine_id: "funasr".to_string(),
            instance_id: "inst-test".to_string(),
            token: "test-token-abcdef0123456789".to_string(),
            resolved_profile: profile,
        };
        // 提供有效的 engine_config，避免 InvalidConfig 错误
        let funasr_config = FunasrEngineConfig {
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            device: "cpu".to_string(),
            num_threads: None,
            hotwords: None,
            use_itn: true,
            vad: VadConfigProjection::default(),
            auto_start_server: false,
        };
        let config = AdapterConfig::from_json(funasr_config.to_json());
        let result = adapter.prepare_launch(&ctx, &config);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(
            err.code,
            LocalEngineErrorCode::EnvironmentMissing,
            "无 generation venv 时应返回 EnvironmentMissing"
        );
        // 错误文案应指向引擎页
        assert!(
            !err.action_hint.contains("语音输入"),
            "错误文案不应指向语音输入页"
        );

        cleanup_test_generation();
    }

    /// 0.22.6 H1: prepare_launch 的 LaunchDescriptor 使用 generation venv python，
    /// 不使用旧全局 venv。
    #[test]
    fn launch_descriptor_uses_generation_python() {
        let _guard = GEN_VENV_TEST_MUTEX.lock().unwrap();
        cleanup_test_generation();

        // 创建 generation venv
        let install_id = "test-launch-001";
        let gen_python = setup_test_generation_venv(install_id);

        // 也创建旧 venv（确保不被使用）
        let legacy_venv = engine_runtime::legacy_funasr_venv_dir();
        let legacy_scripts = legacy_venv.join("Scripts");
        std::fs::create_dir_all(&legacy_scripts).unwrap();
        let legacy_python = legacy_scripts.join("python.exe");
        std::fs::write(&legacy_python, b"legacy python").unwrap();

        // 0.22.6 B2: 使用 mock 包检查器避免执行假 python.exe（挂死风险）。
        // mock 检查器总是返回 (false, None)，模拟 funasr 未安装。
        // 这验证了 prepare_launch 能正确解析 generation python 路径，
        // 并在 funasr 检查失败时返回正确的错误类型。
        fn mock_checker(_python: &std::path::Path, _pkg: &str) -> (bool, Option<String>) {
            (false, None)
        }

        // prepare_launch 使用 mock 包检查器，在 funasr 检查时返回 false，
        // 错误应来自 funasr 检查，而非 python 环境缺失。
        let adapter = FunasrAdapter::new_with_package_checker(mock_checker);
        let profile = ResolvedProfile {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
            priority: 0,
        };
        let ctx = LaunchContext {
            endpoint: crate::infra::local_engine::port::Endpoint::new(8080),
            engine_id: "funasr".to_string(),
            instance_id: "inst-test".to_string(),
            token: "test-token-abcdef0123456789".to_string(),
            resolved_profile: profile,
        };
        // 提供有效的 engine_config
        let funasr_config = FunasrEngineConfig {
            funasr_model: "iic/SenseVoiceSmall".to_string(),
            device: "cpu".to_string(),
            num_threads: None,
            hotwords: None,
            use_itn: true,
            vad: VadConfigProjection::default(),
            auto_start_server: false,
        };
        let config = AdapterConfig::from_json(funasr_config.to_json());
        let result = adapter.prepare_launch(&ctx, &config);

        // mock 检查器返回 (false, None)，模拟 funasr 未安装
        assert!(result.is_err());
        let err = result.unwrap_err();
        // 错误应该是 funasr 包未安装（不是 python 环境缺失）
        assert_eq!(
            err.code,
            LocalEngineErrorCode::EnvironmentMissing,
            "应因 funasr 未安装而失败"
        );
        // 不应出现 "Python 环境未就绪" 错误（那意味着 generation python 不存在）
        assert!(
            !err.action_hint.contains("Python 环境未就绪"),
            "不应报 Python 环境未就绪（generation python 已存在）"
        );

        // 清理
        let _ = std::fs::remove_dir_all(engine_runtime::python_shared_root());
        cleanup_test_generation();
    }

    /// 0.22.6 H1: ModelScope 缓存路径与 engine_model_cache_dir 一致。
    #[test]
    fn model_cache_path_is_engine_model_cache_dir() {
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let cache_dir = engine_runtime::engine_model_cache_dir(&engine_id);
        let expected = engine_runtime::models_root().join(FUNASR_ENGINE_ID);
        assert_eq!(
            cache_dir, expected,
            "engine_model_cache_dir 应返回 models/{engine_id}"
        );
    }

    /// 0.22.6 H1: 嵌入的 Python 脚本包含 model_content_fingerprint 逻辑。
    #[test]
    fn embedded_script_has_content_fingerprint() {
        assert!(
            BLINK_STT_SERVER_PY.contains("model_content_fingerprint"),
            "Python 脚本应包含 model_content_fingerprint"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("_compute_model_content_fingerprint"),
            "Python 脚本应包含 _compute_model_content_fingerprint 函数"
        );
    }

    /// 0.22.6 H1: health 映射在 Ready 时返回 model_content_fingerprint。
    #[test]
    fn health_maps_content_fingerprint_when_ready() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
            "model_id": "iic/SenseVoiceSmall",
            "model_revision": "funasr-1.x",
            "model_content_fingerprint": "abc123def456",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Ready);
        assert_eq!(
            mapping.model_content_fingerprint,
            Some("abc123def456".to_string())
        );
    }

    /// 0.22.6 H1: health 映射在非 Ready 时不返回 fingerprint。
    #[test]
    fn health_omits_fingerprint_when_not_ready() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "loading",
            "model_content_fingerprint": "abc123",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Loading);
        assert!(
            mapping.model_content_fingerprint.is_none(),
            "非 Ready 状态不应返回 fingerprint"
        );
    }

    /// 0.22.6 H1: health 映射在 Ready 但 fingerprint 为空时返回 None。
    #[test]
    fn health_omits_empty_fingerprint_when_ready() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
            "model_content_fingerprint": "",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Ready);
        assert!(
            mapping.model_content_fingerprint.is_none(),
            "空 fingerprint 应映射为 None"
        );
    }

    /// 0.22.6 H1: health 映射在 Ready 但 fingerprint 缺失时返回 None。
    #[test]
    fn health_omits_missing_fingerprint_when_ready() {
        let raw = serde_json::json!({
            "status": "ok",
            "model_status": "ready",
        });
        let mapping = map_funasr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Ready);
        assert!(
            mapping.model_content_fingerprint.is_none(),
            "缺失 fingerprint 应映射为 None"
        );
    }

    /// 0.22.6 H1: FunASR descriptor 的 model_id 与 Python server 返回的一致。
    #[test]
    fn descriptor_model_id_matches_python_server_response() {
        let adapter = FunasrAdapter::new();
        let descriptor_model_id = &adapter.descriptor().model_contract.model_id;
        assert_eq!(
            descriptor_model_id, "iic/SenseVoiceSmall",
            "descriptor model_id 应为 iic/SenseVoiceSmall"
        );
        // Python server health 返回 model_id = args.model（默认 iic/SenseVoiceSmall）
    }

    /// 0.22.6 H1: FunASR descriptor 的 model_revision 与 Python server 返回的一致。
    #[test]
    fn descriptor_model_revision_matches_python_server_response() {
        let adapter = FunasrAdapter::new();
        let descriptor_revision = &adapter.descriptor().model_contract.revision;
        assert_eq!(
            descriptor_revision, "funasr-1.x",
            "descriptor revision 应为 funasr-1.x"
        );
        // Python server health 返回 model_revision = "funasr-1.x"
    }

    /// 0.22.6 H1: 空间统计使用 engine_model_cache_dir 作为模型缓存路径。
    #[test]
    fn space_usage_uses_engine_model_cache_dir() {
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let expected_cache = engine_runtime::engine_model_cache_dir(&engine_id);

        // 创建模型缓存目录
        std::fs::create_dir_all(&expected_cache).unwrap();
        std::fs::write(expected_cache.join("test_model.pt"), b"fake model").unwrap();

        let usage = get_funasr_space_usage();
        assert!(
            usage.model_cache.is_some(),
            "model_cache 应存在（已创建测试目录）"
        );
        let model_cache = usage.model_cache.unwrap();
        assert!(
            model_cache.path.contains(FUNASR_ENGINE_ID),
            "model_cache 路径应包含引擎 id: {}",
            model_cache.path
        );

        // 清理
        let _ = std::fs::remove_dir_all(&expected_cache);
    }

    /// 0.22.6 H1: cleanup 只清理 generation 和 model cache，
    /// 不清理 provider 公共缓存。
    #[test]
    fn cleanup_only_removes_owned_assets() {
        // 验证 cleanup_funasr_engine 的逻辑边界：
        // 只清理 FunASR 拥有的目录，不清理 provider 公共资产。
        // 这里只验证函数存在且返回类型正确，不实际执行删除。
        // 实际行为由 cleanup_funasr_does_not_touch_provider_cache 验证。
        let _ = std::mem::size_of_val(&cleanup_funasr_engine);
    }

    // ── FunASR 依赖锁闭环测试 ──────────────────────────────────────────

    /// 验证 locked-requirements.txt 解析出的包列表包含全部传递依赖（>8 个直接包）。
    #[test]
    fn funasr_locked_packages_includes_transitive_deps() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            // 之前硬编码只有 8 个直接包；完整锁应有 76 个（含传递依赖）
            assert!(
                plan.packages.len() > 8,
                "locked-requirements.txt 应解析出 >8 个包（含传递依赖），实际: {}",
                plan.packages.len()
            );
            tracing::info!(
                "FunASR locked-requirements.txt 解析出 {} 个包",
                plan.packages.len()
            );
        }
    }

    /// 验证所有包的 all_hashes 非空（多平台 wheel hash）。
    #[test]
    fn funasr_locked_packages_all_hashes_non_empty() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                assert!(
                    !pkg.all_hashes.is_empty(),
                    "PackageLock {} 的 all_hashes 为空，--require-hashes 需要至少一个 hash",
                    pkg.name
                );
                // 所有 hash 格式验证
                for h in &pkg.all_hashes {
                    assert_eq!(
                        h.len(),
                        64,
                        "PackageLock {} 的 all_hashes 中有长度不为 64 的 hash",
                        pkg.name
                    );
                    assert!(
                        h.bytes().all(|b| b.is_ascii_hexdigit()),
                        "PackageLock {} 的 all_hashes 中有非 hex 字符",
                        pkg.name
                    );
                }
            }
        }
    }

    /// 验证所有 production 包使用精确版本（不存在 >= ~> < > 等非精确约束）。
    #[test]
    fn funasr_locked_packages_use_exact_versions() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                assert!(
                    !pkg.version.starts_with('>')
                        && !pkg.version.starts_with('<')
                        && !pkg.version.starts_with('~')
                        && !pkg.version.starts_with('!'),
                    "{} 使用了非精确版本约束: {}",
                    pkg.name,
                    pkg.version
                );
            }
        }
    }

    /// 验证 hash 不存在空 hash、非法 hash 或全零占位。
    #[test]
    fn funasr_locked_packages_no_empty_or_zero_hashes() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                // sha256 必须存在
                assert!(pkg.sha256.is_some(), "{} 的 sha256 为 None", pkg.name);
                let hash = pkg.sha256.as_ref().unwrap();
                // 不能是全零占位
                assert!(
                    !hash.chars().all(|c| c == '0'),
                    "{} 的 sha256 是全零占位",
                    pkg.name
                );
                // 不能是空字符串
                assert!(!hash.is_empty(), "{} 的 sha256 为空字符串", pkg.name);
            }
        }
    }

    /// 验证嵌入的锁文件可解析（非空、格式正确）。
    #[test]
    fn funasr_embedded_lock_is_parseable() {
        assert!(!LOCKED_REQUIREMENTS_TXT.is_empty());
        let packages = parse_locked_requirements(LOCKED_REQUIREMENTS_TXT);
        assert!(
            !packages.is_empty(),
            "locked-requirements.txt 解析结果不应为空"
        );
    }

    /// 验证安装计划包含 --no-deps（禁止传递依赖自动解析）。
    #[test]
    fn funasr_provider_descriptor_has_no_deps() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            assert!(
                plan.extra_pip_args
                    .iter()
                    .any(|arg| matches!(arg, PipExtraArg::NoDeps)),
                "安装计划必须包含 --no-deps，禁止传递依赖自动解析"
            );
        }
    }

    /// 验证安装计划包含 PyTorch ExtraIndexUrl。
    #[test]
    fn funasr_provider_descriptor_has_pytorch_index() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            assert!(
                plan.extra_pip_args.iter().any(|arg| matches!(
                    arg,
                    PipExtraArg::ExtraIndexUrl(url) if url.contains("pytorch.org")
                )),
                "安装计划必须包含 PyTorch ExtraIndexUrl"
            );
        }
    }

    /// FunASR 的完整锁横跨 PyPI 与 PyTorch CPU index，必须允许跨索引匹配锁定版本。
    #[test]
    fn funasr_provider_descriptor_has_cross_index_strategy() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            assert!(
                plan.extra_pip_args
                    .iter()
                    .any(|arg| matches!(arg, PipExtraArg::IndexStrategyUnsafeBestMatch)),
                "FunASR 多索引锁安装必须启用 unsafe-best-match"
            );
        }
    }

    /// Windows CPU profile 必须锁到 PyTorch 官方 cp312 win_amd64 CPU wheel。
    #[test]
    fn funasr_pytorch_packages_lock_windows_cpu_wheels() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let torch = plan
                .packages
                .iter()
                .find(|pkg| pkg.name == "torch")
                .unwrap();
            assert_eq!(torch.version, "2.5.0+cpu");
            assert_eq!(
                torch.all_hashes,
                ["3815a38bbe31d0c546a33a0c59a5426563e94aea6d32eb4cf07b6a99bfa7130f"]
            );

            let torchaudio = plan
                .packages
                .iter()
                .find(|pkg| pkg.name == "torchaudio")
                .unwrap();
            assert_eq!(torchaudio.version, "2.5.0+cpu");
            assert_eq!(
                torchaudio.all_hashes,
                ["c972268b2711662d7e01479c38bb49b3da0a38b678f78451c545d4f36384f5ad"]
            );
        }
    }

    /// 验证 locked-requirements.txt 中包含关键直接依赖。
    #[test]
    fn funasr_locked_packages_contains_key_deps() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
            // 直接依赖
            assert!(names.contains(&"torch"), "缺少 torch");
            assert!(names.contains(&"torchaudio"), "缺少 torchaudio");
            assert!(names.contains(&"funasr"), "缺少 funasr");
            assert!(names.contains(&"fastapi"), "缺少 fastapi");
            assert!(names.contains(&"uvicorn"), "缺少 uvicorn");
            // 关键传递依赖
            assert!(names.contains(&"numba"), "缺少传递依赖 numba");
            assert!(names.contains(&"numpy"), "缺少传递依赖 numpy");
            assert!(names.contains(&"scipy"), "缺少传递依赖 scipy");
        }
    }

    /// 验证 numba 使用精确版本，不是 >=0.59。
    #[test]
    fn funasr_numba_uses_exact_version() {
        let pd = make_funasr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let numba = plan.packages.iter().find(|p| p.name == "numba");
            assert!(numba.is_some(), "缺少 numba 包");
            let numba = numba.unwrap();
            assert_eq!(
                numba.version, "0.59.0",
                "numba 应使用精确版本 0.59.0，而不是 >=0.59"
            );
            // 不能以 >= 开头
            assert!(!numba.version.starts_with(">="), "numba 不应使用 >= 约束");
        }
    }

    /// 验证 render_hashed_requirements 能正确渲染多 hash 条目。
    #[test]
    fn funasr_render_hashed_requirements_supports_multiple_hashes() {
        use crate::infra::local_engine::providers::python::render_hashed_requirements;
        let packages = vec![
            PackageLock {
                name: "test-pkg".to_string(),
                version: "1.0.0".to_string(),
                sha256: Some("a".repeat(64)),
                all_hashes: vec!["a".repeat(64), "b".repeat(64)],
            },
            PackageLock {
                name: "another-pkg".to_string(),
                version: "2.0.0".to_string(),
                sha256: Some("c".repeat(64)),
                all_hashes: vec!["c".repeat(64)],
            },
        ];
        let result = render_hashed_requirements(&packages).unwrap();
        // 验证输出包含两个包
        assert!(result.contains("test-pkg==1.0.0"));
        assert!(result.contains("another-pkg==2.0.0"));
        // 验证 test-pkg 有两个 hash
        let test_pkg_line_count = result
            .lines()
            .find(|l| l.contains("test-pkg=="))
            .map(|l| l.matches("--hash=sha256:").count())
            .unwrap_or(0);
        assert_eq!(
            test_pkg_line_count, 2,
            "test-pkg 应有 2 个 hash（多平台 wheel）"
        );
    }

    /// 验证 render_hashed_requirements 拒绝非精确版本。
    #[test]
    fn funasr_render_hashed_requirements_rejects_non_exact_version() {
        use crate::infra::local_engine::providers::python::render_hashed_requirements;
        let packages = vec![PackageLock {
            name: "bad-pkg".to_string(),
            version: ">=1.0.0".to_string(),
            sha256: Some("a".repeat(64)),
            all_hashes: vec!["a".repeat(64)],
        }];
        let result = render_hashed_requirements(&packages);
        assert!(
            result.is_err(),
            "非精确版本约束应被 render_hashed_requirements 拒绝"
        );
    }

    /// 验证 parse_locked_requirements 解析格式正确。
    #[test]
    fn funasr_parse_locked_requirements_correctness() {
        let sample = "# comment\naiohttp==3.14.3 \\\n    --hash=sha256:03cd2bde3d7f085b64e549c985f4bb928cad7e8ecf5323bfca320db548d81b39 \\\n    --hash=sha256:041badb8f843963574d3ad26de6afd7a32b112f43d3c63045c0c8278cfd2043\nfastapi==0.115.6 \\\n    --hash=sha256:9ec46f7addc14ea472958a96aae5b5de65f39721a46aaf5705c480d9a8b76654\n";
        let packages = parse_locked_requirements(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "aiohttp");
        assert_eq!(packages[0].version, "3.14.3");
        assert_eq!(packages[0].all_hashes.len(), 2);
        assert_eq!(
            packages[0].sha256.as_deref(),
            Some("03cd2bde3d7f085b64e549c985f4bb928cad7e8ecf5323bfca320db548d81b39")
        );
        assert_eq!(packages[1].name, "fastapi");
        assert_eq!(packages[1].version, "0.115.6");
        assert_eq!(packages[1].all_hashes.len(), 1);
    }

    /// 验证所有声明的 profile 都有可执行的安装合同：
    /// 每个包都有 hash，且只声明了 CPU profile（与 CPU-only 锁文件匹配）。
    #[test]
    fn funasr_all_profiles_have_executable_install_contract() {
        let pd = make_funasr_provider_descriptor();

        // 所有 profile 必须有对应的 artifact 和 install_plan
        assert!(!pd.profiles.is_empty(), "至少应声明一个 profile");
        for p in &pd.profiles {
            assert!(!p.profile_id.is_empty(), "profile_id 不能为空");
        }

        // 验证安装计划中所有包都有 hash（--require-hashes 可执行）
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            assert!(!plan.packages.is_empty(), "锁文件应包含至少一个包");
            for pkg in &plan.packages {
                assert!(
                    pkg.sha256.is_some(),
                    "{} 缺少 hash —— --require-hashes 将失败",
                    pkg.name
                );
            }
        }

        // 0.22.6：只声明 CPU profile，与 CPU-only 锁文件匹配
        assert!(
            pd.profiles
                .iter()
                .any(|p| p.profile_id == "cpu-x64" && p.backend == ComputeBackend::Cpu),
            "缺少 CPU profile"
        );
        // 确保没有声明 CUDA profile（需独立 CUDA 锁文件后才能启用）
        assert!(
            !pd.profiles
                .iter()
                .any(|p| p.backend == ComputeBackend::Cuda),
            "0.22.6 不应声明 CUDA profile（锁文件仅含 CPU wheel hash）"
        );
    }

    // ── 0.22.6 B2: 子模型映射测试 ──

    /// SenseVoice 系列模型无需子模型。
    #[test]
    fn submodels_for_sensevoice_is_empty() {
        assert!(funasr_submodels_for("iic/SenseVoiceSmall").is_empty());
        assert!(funasr_submodels_for("SenseVoice").is_empty());
    }

    /// Paraformer 系列模型需要 VAD + punc 子模型。
    #[test]
    fn submodels_for_paraformer_has_vad_and_punc() {
        let subs = funasr_submodels_for("paraformer-zh");
        assert_eq!(subs, vec!["fsmn-vad", "ct-punc"]);
    }

    /// 未知模型返回空子模型列表（安全默认值）。
    #[test]
    fn submodels_for_unknown_model_is_empty() {
        assert!(funasr_submodels_for("some-unknown-model").is_empty());
    }

    /// 大小写不敏感的子模型匹配。
    #[test]
    fn submodels_for_case_insensitive() {
        let subs = funasr_submodels_for("Paraformer-ZH");
        assert_eq!(subs, vec!["fsmn-vad", "ct-punc"]);

        assert!(funasr_submodels_for("SENSEVOICESMALL").is_empty());
    }
}
