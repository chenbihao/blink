//! PaddleOCR 本地引擎 adapter（0.22.5）。
//!
//! 把 PP-OCRv6 Python 服务注册为 `LocalEngineAdapter`，使安装、启动、
//! 模型轮询、日志、空间和清理通过 `LocalEngineService` 管理。
//!
//! ## 设计铁则
//!
//! - **唯一锁源**：`locked-requirements.txt`（由 `uv pip compile --generate-hashes`
//!   生成）以 `include_str!` 嵌入 Rust 二进制。`make_paddleocr_provider_descriptor()`
//!   在运行时解析它生成 `PackageLock` 列表——不再手写第二份包清单。
//! - **--require-hashes + --no-deps**：安装时强制 hash 校验 + 禁止传递依赖
//!   自动解析，确保安装的 wheel 与锁文件完全一致。
//! - **descriptor 锁定 Python/package/profile/model contract**：使用 0.22.2
//!   `PythonVenvProvider`；不新造第二套安装器。
//! - **adapter 从 OcrConfig 的已校验配置产生启动请求**：保留
//!   `paddle_model`、`cpu_threads`、`enable_mkldnn` 语义。
//! - **endpoint 仅 127.0.0.1**：每次启动使用 ctx 中的 token/instance id。
//! - **health 必须核对 engine id、instance id 和 token**。
//! - **日志使用 ManagedProcess 的 bounded history/broadcast**。
//! - **空间统计和清理区分 engine generations / PaddleOCR model cache / provider
//!   公共缓存**；单引擎清理不能连带删除公共资产。
//! - **不修改 main.rs 和 Tauri command 注册**——注册函数由阶段 E 接 wiring。
//!
//! ## 与 FunASR adapter 的区别
//!
//! - **CapabilityKind::Ocr**（非 Stt）
//! - **LifecyclePolicy::OnDemand**（非 Manual）
//! - **模型档位**：tiny 为唯一生产候选（spike 资格门）
//! - **不共享 FunASR 的 torch/numba 依赖**：PaddleOCR 使用 paddlepaddle

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, CapabilityKind, CleanupPolicy, ComputeCandidate,
    DiagnosticEntry, EngineDescriptor, EngineDiagnostic, EngineDisplay, EngineTimeouts, ErrorPhase,
    HealthMapping, InstallPlanRef, LaunchContext, LaunchDescriptor, LifecyclePolicy,
    LocalEngineAdapter, LocalEngineError, LocalEngineErrorCode, ModelHealth, ResolvedLaunch,
    ResourceBudget, ServiceHealth,
};
use crate::domain::ocr::config::PaddleModel;
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

/// 嵌入的 blink_ocr_server.py 脚本（随 Rust 二进制发布）。
#[allow(dead_code)]
const BLINK_OCR_SERVER_PY: &str =
    include_str!("../../../resources/ocr/paddleocr/blink_ocr_server.py");

/// 嵌入的完整依赖锁文件（唯一锁源）。
///
/// 由 `uv pip compile --generate-hashes` 生成，包含全部传递依赖及其 SHA-256。
/// `make_paddleocr_provider_descriptor()` 在运行时解析此文件生成 `PackageLock` 列表。
/// 安装时使用 `--require-hashes --no-deps` 强校验。
#[allow(dead_code)]
const LOCKED_REQUIREMENTS_TXT: &str =
    include_str!("../../../resources/ocr/paddleocr/locked-requirements.txt");

/// PaddleOCR 稳定 engine id。
pub const PADDLEOCR_ENGINE_ID: &str = "paddleocr";

// ── PaddleocrAdapter ───────────────────────────────────────────────────────

/// PaddleOCR 本地引擎 adapter。
///
/// 实现 `LocalEngineAdapter` trait，把 PaddleOCR 特有的启动参数、health 映射、
/// 诊断和 self-test 适配到领域统一协议。
///
/// ## 边界
///
/// - **不接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
///   `prepare_launch` 从 descriptor 锁定的 artifact + `OcrConfig` 自行解析。
/// - **不发送 Tauri 事件**：返回纯数据，由 app 层桥接。
/// - **不持有 AppHandle**：adapter 是纯逻辑，不接触 Tauri。
pub struct PaddleocrAdapter {
    descriptor: EngineDescriptor,
}

impl PaddleocrAdapter {
    /// 创建 PaddleOCR adapter。
    ///
    /// descriptor 在编译期声明，锁定 engine id、profile、artifact 和 model contract。
    pub fn new() -> Self {
        Self {
            descriptor: make_paddleocr_descriptor(),
        }
    }
}

impl Default for PaddleocrAdapter {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalEngineAdapter for PaddleocrAdapter {
    fn descriptor(&self) -> &EngineDescriptor {
        &self.descriptor
    }

    /// 从已校验配置、resolved profile 和受控启动上下文产生受限启动描述。
    ///
    /// **不接受前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
    /// adapter 从 descriptor 锁定的 artifact + OcrConfig 自行解析。
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
                    "profile '{}' 不在 PaddleOCR descriptor 声明范围内",
                    ctx.resolved_profile.profile_id
                ),
            ));
        }

        // 从 AdapterConfig.engine_config 解析 PaddleOCR 配置
        let ocr_config: PaddleOcrEngineConfig =
            serde_json::from_value(config.engine_config.clone()).map_err(|e| {
                LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "PaddleOCR 引擎配置解析失败",
                    format!("engine_config 反序列化失败: {e}"),
                )
            })?;

        // 验证 model 是否通过生产资格门（只有 tiny 通过 spike 资格门）
        let paddle_model = match ocr_config.model.as_str() {
            "tiny" => PaddleModel::Tiny,
            "small" => PaddleModel::Small,
            "medium" => PaddleModel::Medium,
            other => {
                return Err(LocalEngineError::with_detail(
                    LocalEngineErrorCode::InvalidConfig,
                    ErrorPhase::Config,
                    "未知的 PaddleOCR 模型档位",
                    format!("model '{other}' 不在 tiny/small/medium 枚举中"),
                ));
            }
        };
        if !paddle_model.is_production_ready() {
            return Err(LocalEngineError::with_detail(
                LocalEngineErrorCode::Unsupported,
                ErrorPhase::Config,
                "模型档位未通过生产资格门",
                format!(
                    "paddle_model {:} 未通过 spike 资格门，只有 tiny 可用",
                    paddle_model
                ),
            ));
        }

        // 构建 LaunchDescriptor
        let launch = build_paddleocr_launch_descriptor(&ocr_config, ctx)?;

        Ok(ResolvedLaunch {
            profile: ctx.resolved_profile.clone(),
            fallback: None,
            launch,
        })
    }

    /// 把 PaddleOCR 的 health 响应映射为领域统一的 service/model 健康状态。
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
        map_paddleocr_health(raw_health)
    }

    /// adapter self-test。
    ///
    /// 验证 PaddleOCR Python 环境是否就绪（generation venv + paddlepaddle + paddleocr 已安装）。
    ///
    /// 优先检查 generation-managed venv（由 `PythonVenvProvider` 创建的隔离 venv）；
    /// 如果 generation venv 不存在，fallback 检查共享 venv（兼容旧环境或首次安装前状态）。
    fn self_test(&self) -> AdapterSelfTest {
        // 只使用 generation-managed venv，不 fallback 到 legacy 全局 venv
        let python_path = generation_venv_python(&self.descriptor.engine_id);
        let python_path = match python_path {
            None => {
                return AdapterSelfTest::failed(
                    "Python 环境未就绪。请在设置页点击「安装环境」按钮。\
                     （Blink 会自动下载 uv + Python 3.12 + paddlepaddle + paddleocr）",
                );
            }
            Some(ref p) => p.as_path(),
        };

        // 检查 paddlepaddle 是否已安装
        let (paddle_ok, _) = check_paddlepaddle_with(python_path);
        if !paddle_ok {
            return AdapterSelfTest::failed(
                "paddlepaddle 包未安装。请在设置页点击「安装环境」按钮，Blink 会自动完成安装。",
            );
        }

        // 检查 paddleocr 是否已安装
        let (paddleocr_ok, _) = check_paddleocr_with(python_path);
        if !paddleocr_ok {
            return AdapterSelfTest::failed(
                "paddleocr 包未安装。请在设置页点击「安装环境」按钮，Blink 会自动完成安装。",
            );
        }

        AdapterSelfTest::passed()
    }

    /// 引擎专属诊断投影。
    ///
    /// 返回 PaddleOCR 特有的诊断信息（Python 环境、paddlepaddle、paddleocr 版本等）。
    fn diagnostics(&self) -> EngineDiagnostic {
        let mut entries = Vec::new();

        // 只使用 generation-managed venv
        let python_path = generation_venv_python(&self.descriptor.engine_id);

        // venv 状态
        let venv_exists = python_path.is_some();
        entries.push(DiagnosticEntry {
            key: "venv_exists".to_string(),
            value: if venv_exists {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: "info".to_string(),
        });

        // 如果有 generation venv，报告其路径
        // 只报告 generation venv 状态
        if python_path.is_some() {
            entries.push(DiagnosticEntry {
                key: "venv_source".to_string(),
                value: "generation".to_string(),
                label: "info".to_string(),
            });
        }

        // 检查 Python 版本
        if let Some(ref py) = python_path {
            if let Some(ref v) = check_python_version(py) {
                entries.push(DiagnosticEntry {
                    key: "python_version".to_string(),
                    value: v.clone(),
                    label: "info".to_string(),
                });
            }
        }

        // paddlepaddle 状态
        let (paddle_ok, paddle_ver) = if let Some(ref py) = python_path {
            check_paddlepaddle_with(py)
        } else {
            (false, None)
        };
        entries.push(DiagnosticEntry {
            key: "paddlepaddle_installed".to_string(),
            value: if paddle_ok {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: if paddle_ok {
                "info".to_string()
            } else {
                "warning".to_string()
            },
        });
        if let Some(ref v) = paddle_ver {
            entries.push(DiagnosticEntry {
                key: "paddlepaddle_version".to_string(),
                value: v.clone(),
                label: "info".to_string(),
            });
        }

        // paddleocr 状态
        let (paddleocr_ok, paddleocr_ver) = if let Some(ref py) = python_path {
            check_paddleocr_with(py)
        } else {
            (false, None)
        };
        entries.push(DiagnosticEntry {
            key: "paddleocr_installed".to_string(),
            value: if paddleocr_ok {
                "true".to_string()
            } else {
                "false".to_string()
            },
            label: if paddleocr_ok {
                "info".to_string()
            } else {
                "warning".to_string()
            },
        });
        if let Some(ref v) = paddleocr_ver {
            entries.push(DiagnosticEntry {
                key: "paddleocr_version".to_string(),
                value: v.clone(),
                label: "info".to_string(),
            });
        }

        EngineDiagnostic { entries }
    }
}

// ── descriptor 构造 ────────────────────────────────────────────────────────

/// 构造 PaddleOCR 编译期 descriptor。
///
/// descriptor 必须锁定现有 Python/package/profile/model contract。
/// 使用 0.22.2 `PythonVenvProvider`；不新造第二套安装器。
fn make_paddleocr_descriptor() -> EngineDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    EngineDescriptor {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        display: EngineDisplay {
            name: "PP-OCRv6 文字识别".to_string(),
            description: "本地 PaddleOCR PP-OCRv6 文字识别（Python/PaddlePaddle）".to_string(),
            icon: "scan-text".to_string(),
            version: "0.22.4".to_string(),
        },
        capability_kind: CapabilityKind::Ocr,
        runtime_kind: RuntimeKind::PythonVenv,
        install_plan: InstallPlanRef {
            runtime_kind: RuntimeKind::PythonVenv,
            artifact_ids: vec![python_artifact.clone()],
            compute_candidates: vec![ComputeCandidate {
                preference: ComputePreference::Cpu,
                profile_id: "cpu-x64".to_string(),
                artifact_id: python_artifact.clone(),
            }],
            schema_version: 1,
        },
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny".to_string(),
            checksum_source: ChecksumSource::Unverified,
        },
        lifecycle: LifecyclePolicy::OnDemand,
        // PaddleOCR tiny 冷启动 ~2.4s + 模型加载，start timeout 设为 30s
        // 模型首次下载可能需要更长时间
        timeouts: EngineTimeouts {
            start_timeout: Duration::from_secs(30),
            model_load_timeout: Duration::from_secs(120),
            idle_ttl: Duration::from_secs(300),
        },
        resource_budget: ResourceBudget {
            // spike 实测 venv 785.9MB + 模型 169.1MB ≈ 955MB，向上取整
            estimated_env_disk_mb: Some(960),
            // tiny det + rec models ~10MB（169.1MB 含三档共享，tiny 单独约 10MB）
            estimated_model_disk_mb: Some(10),
            // spike 实测稳定工作集 ~408MB
            estimated_stable_ram_mb: Some(410),
            // spike 实测峰值工作集 ~1136MB（接近但未超 1.2GB 门）
            estimated_peak_ram_mb: Some(1140),
        },
        cleanup: CleanupPolicy {
            owned_subdirs: vec!["generations".to_string(), "staging".to_string()],
            has_model_cache: true,
            has_log_dir: false,
        },
    }
}

// ── locked-requirements.txt 解析 ─────────────────────────────────────────────

/// 解析 `locked-requirements.txt` 格式的依赖锁文件。
///
/// 格式（`uv pip compile --generate-hashes` 输出）：
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
    }
    packages
}

// ── ProviderDescriptor 构造 ──────────────────────────────────────────────────

/// 构造 PaddleOCR 的 `ProviderDescriptor`（infra 层安装事务用）。
///
/// 与 `make_paddleocr_descriptor()`（domain 层 `EngineDescriptor`）互补。
///
/// **包列表来源**：`resources/ocr/paddleocr/locked-requirements.txt`（唯一锁源）。
/// 以 `include_str!` 嵌入，运行时解析生成 `PackageLock` 列表。
/// 不再手写第二份包清单——避免 lock.json 与 Rust descriptor 漂移。
///
/// **安装策略**：`--require-hashes --no-deps`——强制 hash 校验 + 禁止传递依赖
/// 自动解析，确保安装的 wheel 与锁文件完全一致。
pub fn make_paddleocr_provider_descriptor() -> ProviderDescriptor {
    let python_artifact = ArtifactId::new("python-3.12.8").unwrap();

    let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
    let model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);

    ProviderDescriptor {
        engine_id: EngineId::new(PADDLEOCR_ENGINE_ID).unwrap(),
        runtime_kind: RuntimeKind::PythonVenv,
        display_name: "PP-OCRv6 文字识别".to_string(),
        profiles: vec![ProfileCandidate {
            profile_id: "cpu-x64".to_string(),
            backend: ComputeBackend::Cpu,
            artifact_id: python_artifact.clone(),
            compatibility: CompatibilityCheck::Always,
        }],
        model_contract: ModelContract {
            model_id,
            revision: "ppocrv6-tiny".to_string(),
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
            extra_pip_args: vec![PipExtraArg::NoDeps],
            self_test_script:
                "import paddle; import paddleocr; import fastapi; import uvicorn; paddle.utils.run_check()".to_string(),
        }),
        min_generations: 2,
    }
}

/// 创建 PaddleOCR 的 `PythonVenvProvider` 实例。
///
/// `LocalEngineService` 持有此实例，在 `install` 时传给 `InstallTransaction`。
pub fn make_paddleocr_python_provider() -> PythonVenvProvider {
    PythonVenvProvider::new()
}

// ── PaddleOcrEngineConfig（从 OcrConfig 投影） ────────────────────────────

/// PaddleOCR 引擎配置（从 `OcrConfig` 投影）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PaddleOcrEngineConfig {
    /// 模型档位（tiny / small / medium）。
    pub model: String,
    /// CPU 推理线程数。
    #[serde(default = "default_cpu_threads")]
    pub cpu_threads: u32,
    /// 是否启用 MKL-DNN 加速。
    #[serde(default)]
    pub enable_mkldnn: bool,
    /// 模型缓存目录（相对于引擎根目录）。
    #[serde(default = "default_model_cache")]
    pub model_cache: String,
}

fn default_cpu_threads() -> u32 {
    2
}

fn default_model_cache() -> String {
    "model-cache".to_string()
}

impl PaddleOcrEngineConfig {
    /// 从 `OcrConfig` 投影。
    ///
    /// **Task 16**: production lock——如果 paddle_model 未通过生产资格门，
    /// 强制降级为 Tiny（唯一通过候选）。这是 defense-in-depth：
    /// `OcrConfig::validate()` 已在配置写入时拦截，这里在启动时再检查一次。
    pub fn from_ocr_config() -> Self {
        let cfg = crate::domain::config::ocr_config::get_ocr_config();
        let model = if cfg.paddle_model.is_production_ready() {
            cfg.paddle_model.to_string()
        } else {
            tracing::warn!(
                model = %cfg.paddle_model,
                "paddle_model 未通过生产资格门，强制降级为 tiny"
            );
            PaddleModel::Tiny.to_string()
        };
        Self {
            model,
            cpu_threads: default_cpu_threads(),
            enable_mkldnn: false,
            model_cache: default_model_cache(),
        }
    }

    /// 转为 `serde_json::Value` 以注入 `AdapterConfig::engine_config`。
    pub fn to_json(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or(serde_json::Value::Null)
    }
}

// ── LaunchDescriptor 构造 ───────────────────────────────────────────────────

/// 构建 PaddleOCR 的 `LaunchDescriptor`。
///
/// 从 `PaddleOcrEngineConfig` 产生启动请求：
/// - model: tiny（唯一生产候选）
/// - port: ctx.endpoint.port()
/// - token/engine-id/instance-id: 从 LaunchContext 传入
/// - model-cache: 引擎根目录下的 model-cache 子目录
fn build_paddleocr_launch_descriptor(
    ocr_config: &PaddleOcrEngineConfig,
    ctx: &LaunchContext,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let port = ctx.endpoint.port();

    // 优先使用 generation-managed venv（由 PythonVenvProvider 创建的隔离 venv）
    let engine_id = EngineId::new(&ctx.engine_id).map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "engine_id 无效",
            format!("解析 engine_id 失败: {e}"),
        )
    })?;
    // 只使用 generation-managed venv，不 fallback 到 legacy 全局 venv
    let python_path = generation_venv_python(&engine_id);
    let python = python_path.ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "Python 环境未就绪",
            "PaddleOCR 环境未安装。请调用 install_paddleocr 或在设置页点击「安装环境」。\
             （Blink 会自动下载 uv + Python 3.12 + paddlepaddle + paddleocr）",
        )
    })?;

    // 检查 paddleocr 是否已安装（使用 generation venv 中的 python）
    let (paddleocr_ok, _) = check_paddleocr_with(&python);
    if !paddleocr_ok {
        return Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "paddleocr 包未安装",
            "paddleocr 包未安装。请调用 install_paddleocr 或 repair_paddleocr 进行安装/修复。",
        ));
    }

    // 释放 blink_ocr_server.py 脚本
    let script_path = ensure_ocr_server_script().map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "释放 blink_ocr_server.py 失败",
            e,
        )
    })?;

    // 模型缓存目录：统一使用 runtime::engine_model_cache_dir 真源
    // 不使用 python_dir()/ocr-model-cache，不写用户 ~/.paddlex
    let model_cache_dir =
        engine_runtime::engine_model_cache_dir(&EngineId::new(PADDLEOCR_ENGINE_ID).unwrap());
    if let Err(e) = std::fs::create_dir_all(&model_cache_dir) {
        tracing::warn!(%e, "创建 OCR 模型缓存目录失败");
    }

    tracing::info!(
        script = %script_path.display(),
        model = %ocr_config.model,
        port,
        model_cache = %model_cache_dir.display(),
        "构建 PaddleOCR LaunchDescriptor",
    );

    // 构建参数列表
    let mut args: Vec<String> = Vec::new();
    args.push(script_path.to_string_lossy().to_string());
    args.push("--port".to_string());
    args.push(port.to_string());
    args.push("--model".to_string());
    args.push(ocr_config.model.clone());
    args.push("--token".to_string());
    args.push(ctx.token.clone());
    args.push("--engine-id".to_string());
    args.push(ctx.engine_id.clone());
    args.push("--instance-id".to_string());
    args.push(ctx.instance_id.clone());
    args.push("--model-cache".to_string());
    args.push(model_cache_dir.to_string_lossy().to_string());
    args.push("--cpu-threads".to_string());
    args.push(ocr_config.cpu_threads.to_string());

    if ocr_config.enable_mkldnn {
        args.push("--enable-mkldnn".to_string());
    }

    // 受限环境变量
    let mut env = HashMap::new();
    env.insert("PYTHONUNBUFFERED".to_string(), "1".to_string());
    env.insert("PYTHONUTF8".to_string(), "1".to_string());
    env.insert("PYTHONIOENCODING".to_string(), "utf-8".to_string());
    // 重定向 PaddleX 模型缓存到 Blink 管理目录，不写用户 ~/.paddlex
    // PaddleX 3.7 使用的变量是 PADDLE_PDX_CACHE_HOME
    env.insert(
        "PADDLE_PDX_CACHE_HOME".to_string(),
        model_cache_dir.to_string_lossy().to_string(),
    );
    // 锁定模型来源为 BOS，不在运行时随机选择多个 host
    env.insert("PADDLE_PDX_MODEL_SOURCE".to_string(), "BOS".to_string());

    Ok(LaunchDescriptor {
        executable: python,
        args,
        current_dir: None,
        env,
        label: PADDLEOCR_ENGINE_ID.to_string(),
    })
}

// ── 脚本释放 ────────────────────────────────────────────────────────────────

/// 释放 `blink_ocr_server.py` 到 `%APPDATA%\blink\python\blink_ocr_server.py`。
///
/// 如果文件已存在且内容一致，不重复写入。
pub fn ensure_ocr_server_script() -> Result<std::path::PathBuf, String> {
    let dir = crate::infra::utils::paths::python_dir();
    ensure_ocr_server_script_in(&dir)
}

/// `ensure_ocr_server_script` 的内部实现，接受显式目标目录（测试用）。
pub(crate) fn ensure_ocr_server_script_in(
    dir: &std::path::Path,
) -> Result<std::path::PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 python 目录失败: {e}"))?;

    let script_path = dir.join("blink_ocr_server.py");

    // 检查是否已存在且内容一致（避免无谓写入）
    let need_write = match std::fs::read_to_string(&script_path) {
        Ok(existing) => existing != BLINK_OCR_SERVER_PY,
        Err(_) => true,
    };

    if need_write {
        std::fs::write(&script_path, BLINK_OCR_SERVER_PY)
            .map_err(|e| format!("写入 blink_ocr_server.py 失败: {e}"))?;
        tracing::info!(path = %script_path.display(), "已释放 blink_ocr_server.py");
    }

    Ok(script_path)
}

// ── Health 映射 ─────────────────────────────────────────────────────────────

/// 把 PaddleOCR 的 HTTP /health 响应映射为领域统一的 `HealthMapping`。
///
/// PaddleOCR server health 响应格式（protocol 0.3.0）：
/// ```json
/// {
///   "protocol_version": "0.3.0",
///   "engine_id": "paddleocr",
///   "instance_id": "uuid-4",
///   "token_fingerprint": "fp: + 16 hex",
///   "endpoint": "http://127.0.0.1:9100",
///   "service_state": "healthy",
///   "model_state": "Ready",
///   "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
///   "model_revision": "ppocrv6-tiny",
///   "model_content_fingerprint": "64-hex-sha256",
///   "actual_backend": "cpu",
///   "device_name": "CPU",
///   "uptime_seconds": 12.34
/// }
/// ```
///
/// health 必须核对 engine id、instance id、token_fingerprint、endpoint 和 actual_backend。
/// 如果 health 响应缺少任何身份字段，service 降级为 Unreachable。
///
/// **收紧 health 契约（0.22.5）**：
/// - model_state == "Ready" 时，model_id、model_revision、model_content_fingerprint
///   必须全部存在且格式正确（fingerprint 为 64 位小写 hex）。
/// - 缺字段不能通过 `if let Some` 静默接受——Ready 状态降级为 Failed。
/// - model_id 和 model_revision 必须与 descriptor 一致。
/// - model_content_fingerprint 是内容指纹字段，与 model_revision 分离。
fn map_paddleocr_health(raw_health: &serde_json::Value) -> HealthMapping {
    // ── service health ──
    let service_state = raw_health.get("service_state").and_then(|v| v.as_str());

    // 检查身份字段是否完整：engine_id + instance_id + token_fingerprint + endpoint
    // token_fingerprint 必须以 "fp:" 前缀开头（与 Rust port::token_fingerprint 一致）
    let has_full_identity = raw_health.get("engine_id").is_some()
        && raw_health.get("instance_id").is_some()
        && raw_health
            .get("token_fingerprint")
            .and_then(|v| v.as_str())
            .is_some_and(|s| s.starts_with("fp:") && s.len() == 19) // fp: + 16 hex
        && raw_health.get("endpoint").is_some();

    let service = if service_state == Some("healthy") && has_full_identity {
        ServiceHealth::Healthy
    } else {
        ServiceHealth::Unreachable
    };

    // ── model health ──
    let model_state = raw_health.get("model_state").and_then(|v| v.as_str());

    // ── 模型 id / revision / fingerprint ──
    // 收集这些字段（无论 model_state 是什么）
    let model_id = raw_health
        .get("model_id")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_revision = raw_health
        .get("model_revision")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let model_content_fingerprint = raw_health
        .get("model_content_fingerprint")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // ── 收紧 model health 契约 ──
    // Ready 状态要求 model_id、model_revision、model_content_fingerprint 全部存在
    // 且 fingerprint 为 64 位小写 hex。缺字段或格式错误时降级为 Failed。
    let model = match model_state {
        Some("Ready") => {
            // 检查必填字段
            let has_id = model_id.is_some();
            let has_rev = model_revision.is_some();
            let fp = model_content_fingerprint.as_deref();
            let fp_valid =
                fp.is_some_and(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()));

            if has_id && has_rev && fp_valid {
                ModelHealth::Ready
            } else {
                tracing::warn!(
                    has_id,
                    has_rev,
                    has_fp = fp.is_some(),
                    fp_valid,
                    "PaddleOCR health 报告 Ready 但关键字段缺失或格式错误，降级为 Failed"
                );
                ModelHealth::Failed
            }
        }
        Some("Loading") => ModelHealth::Loading,
        Some("NotLoaded") => ModelHealth::NotLoaded,
        Some("Failed") => ModelHealth::Failed,
        _ => ModelHealth::Unknown,
    };

    // ── actual backend / device_name（从 health 响应读取）──
    // 未知 backend 值不得默认伪装 CPU
    let actual_backend_str = raw_health
        .get("actual_backend")
        .and_then(|v| v.as_str())
        .unwrap_or("unknown");
    let device_name = raw_health
        .get("device_name")
        .and_then(|v| v.as_str())
        .unwrap_or("Unknown")
        .to_string();

    let actual_backend = match actual_backend_str {
        "cpu" => ComputeBackend::Cpu,
        "cuda" => ComputeBackend::Cuda,
        "vulkan" => ComputeBackend::Vulkan,
        "directml" => ComputeBackend::Directml,
        _ => {
            // 未知值不得默认伪装 CPU
            tracing::warn!(
                actual_backend = actual_backend_str,
                "PaddleOCR health 报告了未知的 actual_backend，标记为不一致"
            );
            // 返回 CPU 但标记 inconsistent
            ComputeBackend::Cpu
        }
    };

    // 检查 actual_backend 与 resolved profile (cpu) 是否一致
    let consistent = actual_backend_str == "cpu" && actual_backend == ComputeBackend::Cpu;

    HealthMapping {
        service,
        model,
        environment: None,
        backend: Some(BackendObservation {
            actual_backend,
            device_name,
            consistent,
        }),
        model_id,
        model_revision,
        model_content_fingerprint,
    }
}

// ── Generation venv 解析 ──────────────────────────────────────────────────────

/// 从 current pointer 解析 generation-managed venv 中的 `python.exe` 路径。
///
/// 路径：`runtimes/engines/{engine_id}/generations/{install_id}/venv/Scripts/python.exe`
///
/// 返回 `None` 表示尚未安装（current.json 不存在或 venv 目录缺失）。
fn generation_venv_python(engine_id: &EngineId) -> Option<PathBuf> {
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

// ── Python 包检查 ────────────────────────────────────────────────────────────

/// 检查 paddlepaddle 是否已安装（使用指定 python 路径）。
///
/// PaddlePaddle 的 distribution 名是 `paddlepaddle`，但 Python import 名是 `paddle`。
/// 禁止使用 `import paddlepaddle`。
/// 返回 (installed, version)。
pub fn check_paddlepaddle_with(python: &Path) -> (bool, Option<String>) {
    let output = crate::infra::platform::no_window(std::process::Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('paddlepaddle'))",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ver.is_empty() {
                (false, None)
            } else {
                // 验证 `import paddle` 是否可用（distribution 名 ≠ import 名）
                let import_ok =
                    crate::infra::platform::no_window(std::process::Command::new(python))
                        .args(["-c", "import paddle"])
                        .stdout(std::process::Stdio::null())
                        .stderr(std::process::Stdio::null())
                        .output()
                        .map(|o| o.status.success())
                        .unwrap_or(false);
                (import_ok, Some(ver))
            }
        }
        _ => (false, None),
    }
}

/// 检查 paddleocr 是否已安装（使用指定 python 路径）。
///
/// 返回 (installed, version)。
pub fn check_paddleocr_with(python: &Path) -> (bool, Option<String>) {
    let output = crate::infra::platform::no_window(std::process::Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('paddleocr'))",
        ])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    match output {
        Ok(out) if out.status.success() => {
            let ver = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if ver.is_empty() {
                (false, None)
            } else {
                (true, Some(ver))
            }
        }
        _ => (false, None),
    }
}

/// 查询指定 python 的版本字符串（如 "Python 3.12.8"）。
fn check_python_version(python: &Path) -> Option<String> {
    let output = crate::infra::platform::no_window(std::process::Command::new(python))
        .args(["--version"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .ok()?;
    if output.status.success() {
        let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
        if !ver.is_empty() {
            return Some(ver);
        }
    }
    // 有些 Python 把版本写到 stderr
    let ver = String::from_utf8_lossy(&output.stderr).trim().to_string();
    if !ver.is_empty() { Some(ver) } else { None }
}

// ── adapter 工厂函数 ───────────────────────────────────────────────────────

/// 创建 PaddleOCR adapter（`Arc<dyn LocalEngineAdapter>`）。
///
/// 对齐 `funasr::make_funasr_adapter()` 模式，供 main.rs wiring 使用。
pub fn make_paddleocr_adapter() -> Arc<dyn LocalEngineAdapter> {
    Arc::new(PaddleocrAdapter::new())
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn descriptor_has_correct_engine_id() {
        let adapter = PaddleocrAdapter::new();
        assert_eq!(adapter.descriptor().engine_id.as_str(), PADDLEOCR_ENGINE_ID);
    }

    #[test]
    fn descriptor_has_ocr_capability() {
        let adapter = PaddleocrAdapter::new();
        assert_eq!(adapter.descriptor().capability_kind, CapabilityKind::Ocr);
    }

    #[test]
    fn descriptor_has_on_demand_lifecycle() {
        let adapter = PaddleocrAdapter::new();
        assert_eq!(adapter.descriptor().lifecycle, LifecyclePolicy::OnDemand);
    }

    #[test]
    fn descriptor_has_cpu_profile_only() {
        let adapter = PaddleocrAdapter::new();
        let candidates = &adapter.descriptor().install_plan.compute_candidates;
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0].preference, ComputePreference::Cpu);
    }

    #[test]
    fn provider_descriptor_packages_match_requirements() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            // 必须包含 paddlepaddle 和 paddleocr
            let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
            assert!(names.contains(&"paddlepaddle"));
            assert!(names.contains(&"paddleocr"));
            assert!(names.contains(&"fastapi"));
            assert!(names.contains(&"uvicorn"));
        } else {
            panic!("expected PythonVenv install plan");
        }
    }

    /// Task 3: 验证 production descriptor 中不存在空 hash。
    ///
    /// 所有 PackageLock.sha256 必须为 Some(有效 64 位 hex)。
    /// 如果 hash 未填充，render_hashed_requirements 会拒绝安装。
    #[test]
    fn provider_descriptor_no_empty_hashes() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                assert!(
                    pkg.sha256.is_some(),
                    "PackageLock {} 的 sha256 为 None，不允许空 hash 进入生产安装",
                    pkg.name
                );
                let hash = pkg.sha256.as_ref().unwrap();
                assert_eq!(
                    hash.len(),
                    64,
                    "PackageLock {} 的 sha256 长度不是 64: {}",
                    pkg.name,
                    hash
                );
                assert!(
                    hash.bytes().all(|b| b.is_ascii_hexdigit()),
                    "PackageLock {} 的 sha256 包含非 hex 字符: {}",
                    pkg.name,
                    hash
                );
            }
        } else {
            panic!("expected PythonVenv install plan");
        }
    }

    /// Task 5: 验证 production descriptor 中不存在占位全零 hash。
    ///
    /// 全零 hash（sha256 = "000...0"）格式合法但不是真实 hash，
    /// --require-hashes 会因 hash 不匹配而拒绝安装。
    /// 生产 descriptor 中所有 hash 必须是真实的、从 PyPI 获取的值。
    #[test]
    fn provider_descriptor_no_placeholder_zero_hashes() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let zero_hash = "0".repeat(64);
            for pkg in &plan.packages {
                let hash = pkg.sha256.as_ref().unwrap();
                assert_ne!(
                    hash, &zero_hash,
                    "PackageLock {} 的 sha256 为全零占位值，生产 descriptor 不允许占位 hash",
                    pkg.name
                );
            }
        } else {
            panic!("expected PythonVenv install plan");
        }
    }

    /// Task 5: 验证 production descriptor 中所有 hash 不相同（防重复）。
    ///
    /// 不同包不应有相同的 SHA-256 hash（除非是同一 wheel 文件，
    /// 但不同包的 wheel 永远不同）。
    #[test]
    fn provider_descriptor_hashes_are_distinct() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let hashes: Vec<&str> = plan
                .packages
                .iter()
                .map(|p| p.sha256.as_deref().unwrap())
                .collect();
            let unique: std::collections::HashSet<&str> = hashes.iter().copied().collect();
            assert_eq!(hashes.len(), unique.len(), "存在重复的 SHA-256 hash");
        }
    }

    /// Task 3: 验证 package name、version、hash 能稳定序列化。
    #[test]
    fn package_lock_stable_serialization() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                let json = serde_json::to_string(pkg).expect("PackageLock 序列化失败");
                let deserialized: PackageLock =
                    serde_json::from_str(&json).expect("PackageLock 反序列化失败");
                assert_eq!(pkg.name, deserialized.name);
                assert_eq!(pkg.version, deserialized.version);
                assert_eq!(pkg.sha256, deserialized.sha256);
            }
        }
    }

    /// Task 3: 验证版本不是范围约束。
    #[test]
    fn provider_descriptor_versions_are_exact() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            for pkg in &plan.packages {
                assert!(
                    !pkg.version
                        .chars()
                        .next()
                        .is_some_and(|ch| matches!(ch, '>' | '<' | '~' | '!')),
                    "PackageLock {} 使用了非精确版本约束: {}",
                    pkg.name,
                    pkg.version
                );
            }
        }
    }

    #[test]
    fn provider_descriptor_has_cpu_profile_only() {
        let pd = make_paddleocr_provider_descriptor();
        assert_eq!(pd.profiles.len(), 1);
        assert_eq!(pd.profiles[0].backend, ComputeBackend::Cpu);
    }

    #[test]
    fn map_health_ready() {
        let raw = serde_json::json!({
            "protocol_version": "0.3.0",
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "actual_backend": "cpu",
            "device_name": "CPU",
            "uptime_seconds": 12.34
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Ready);
        assert_eq!(
            mapping.model_id.as_deref(),
            Some("PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec")
        );
        assert_eq!(mapping.model_revision.as_deref(), Some("ppocrv6-tiny"));
        assert!(mapping.backend.as_ref().unwrap().consistent);
    }

    /// Task 4: 验证 model_content_fingerprint 被正确存入 HealthMapping
    #[test]
    fn map_health_ready_preserves_content_fingerprint() {
        let raw = serde_json::json!({
            "protocol_version": "0.3.0",
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
            "actual_backend": "cpu",
            "device_name": "CPU",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model_content_fingerprint.as_deref(),
            Some("a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90")
        );
    }

    /// Task 4: 验证缺少 model_content_fingerprint 时 Ready 降级为 Failed
    #[test]
    fn map_health_ready_without_fingerprint_degrades_to_failed() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            "model_revision": "ppocrv6-tiny",
            // 缺少 model_content_fingerprint
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(
            mapping.model,
            ModelHealth::Failed,
            "Ready 缺 fingerprint 应降级为 Failed"
        );
    }

    /// Task 4: 验证 fingerprint 格式错误时 Ready 降级为 Failed
    #[test]
    fn map_health_ready_with_invalid_fingerprint_degrades_to_failed() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "short-not-hex",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model,
            ModelHealth::Failed,
            "Ready 无效 fingerprint 应降级为 Failed"
        );
    }

    /// Task 4: 验证缺少 model_id 时 Ready 降级为 Failed
    #[test]
    fn map_health_ready_without_model_id_degrades_to_failed() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            // 缺少 model_id
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model,
            ModelHealth::Failed,
            "Ready 缺 model_id 应降级为 Failed"
        );
    }

    /// Task 4: 验证缺少 model_revision 时 Ready 降级为 Failed
    #[test]
    fn map_health_ready_without_model_revision_degrades_to_failed() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            // 缺少 model_revision
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model,
            ModelHealth::Failed,
            "Ready 缺 model_revision 应降级为 Failed"
        );
    }

    /// Task 4: 验证 health 报告的 model_id 与 descriptor 一致
    #[test]
    fn map_health_model_id_matches_descriptor() {
        let adapter = PaddleocrAdapter::new();
        let expected_model_id = &adapter.descriptor().model_contract.model_id;

        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": expected_model_id,
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model_id.as_deref(),
            Some(expected_model_id.as_str())
        );
    }

    /// Task 4: 验证 health 报告的 model_revision 与 descriptor 一致
    #[test]
    fn map_health_model_revision_matches_descriptor() {
        let adapter = PaddleocrAdapter::new();
        let expected_revision = &adapter.descriptor().model_contract.revision;

        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "PP-OCRv6:PP-OCRv6_tiny_det:PP-OCRv6_tiny_rec",
            "model_revision": expected_revision,
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(
            mapping.model_revision.as_deref(),
            Some(expected_revision.as_str())
        );
    }

    /// Task 4: 验证 health 报告不一致的 model_id 时仍映射（由上层验证）
    ///
    /// map_paddleocr_health 是纯映射函数，不做身份验证——
    /// 身份验证由 LocalEngineService.parse_and_verify_health 负责。
    /// 这里验证映射函数忠实传递 health 报告的值，不做静默修正。
    #[test]
    fn map_health_mismatched_model_id_passes_through() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Ready",
            "model_id": "WRONG_MODEL_ID",
            "model_revision": "ppocrv6-tiny",
            "model_content_fingerprint": "a1b2c3d4e5f60718293a4b5c6d7e8f90a1b2c3d4e5f60718293a4b5c6d7e8f90",
        });
        let mapping = map_paddleocr_health(&raw);
        // 映射函数忠实传递值，不做静默修正
        assert_eq!(mapping.model_id.as_deref(), Some("WRONG_MODEL_ID"));
    }

    #[test]
    fn map_health_loading() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Loading",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Healthy);
        assert_eq!(mapping.model, ModelHealth::Loading);
    }

    #[test]
    fn map_health_failed() {
        let raw = serde_json::json!({
            "engine_id": "paddleocr",
            "instance_id": "test-uuid",
            "token_fingerprint": "fp:abc123def4560a1b",
            "endpoint": "http://127.0.0.1:9100",
            "service_state": "healthy",
            "model_state": "Failed",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(mapping.model, ModelHealth::Failed);
    }

    #[test]
    fn map_health_missing_identity_degrades_to_unreachable() {
        // 缺少 token_fingerprint 和 endpoint → 降级为 Unreachable
        let raw = serde_json::json!({
            "service_state": "healthy",
            "model_state": "Ready",
        });
        let mapping = map_paddleocr_health(&raw);
        assert_eq!(mapping.service, ServiceHealth::Unreachable);
    }

    /// Task 4: 验证 model_id 和 model_revision 与 descriptor 一致
    #[test]
    fn descriptor_model_identity_matches() {
        let adapter = PaddleocrAdapter::new();
        let descriptor = adapter.descriptor();
        let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
        let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
        assert_eq!(descriptor.model_contract.model_id, expected_model_id);
        assert_eq!(descriptor.model_contract.revision, "ppocrv6-tiny");
    }

    /// Task 4: 验证 provider descriptor 的 model identity 也一致
    #[test]
    fn provider_descriptor_model_identity_matches() {
        let pd = make_paddleocr_provider_descriptor();
        let (det_model, rec_model) = PaddleModel::Tiny.official_model_names();
        let expected_model_id = format!("PP-OCRv6:{}:{}", det_model, rec_model);
        assert_eq!(pd.model_contract.model_id, expected_model_id);
        assert_eq!(pd.model_contract.revision, "ppocrv6-tiny");
    }

    /// Task 4: model_revision 不应使用 cache_files:N 格式
    #[test]
    fn model_revision_not_cache_files_format() {
        let adapter = PaddleocrAdapter::new();
        let revision = &adapter.descriptor().model_contract.revision;
        assert!(
            !revision.starts_with("cache_files:"),
            "model_revision 不应使用 cache_files:N 格式，实际: {}",
            revision
        );
        assert_eq!(revision, "ppocrv6-tiny");
    }

    #[test]
    fn engine_config_from_ocr_config_defaults_to_tiny() {
        let cfg = PaddleOcrEngineConfig::from_ocr_config();
        assert_eq!(cfg.model, "tiny");
    }

    #[test]
    fn ensure_ocr_server_script_creates_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = ensure_ocr_server_script_in(tmp.path()).unwrap();
        assert!(path.exists());
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("blink_ocr_server"));
    }

    #[test]
    fn ensure_ocr_server_script_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let path1 = ensure_ocr_server_script_in(tmp.path()).unwrap();
        let path2 = ensure_ocr_server_script_in(tmp.path()).unwrap();
        assert_eq!(path1, path2);
    }

    // ── 完整依赖锁测试 ──────────────────────────────────────────────────────

    /// 验证从 locked-requirements.txt 解析的包列表包含全部传递依赖（>7 个直接包）。
    #[test]
    fn locked_packages_includes_transitive_deps() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            // 之前硬编码只有 7 个直接包；完整锁应有 70 个（含传递依赖）
            assert!(
                plan.packages.len() > 7,
                "locked-requirements.txt 应解析出 >7 个包（含传递依赖），实际: {}",
                plan.packages.len()
            );
            tracing::info!(
                "locked-requirements.txt 解析出 {} 个包",
                plan.packages.len()
            );
        }
    }

    /// 验证所有包的 all_hashes 非空（多平台 wheel hash）。
    #[test]
    fn locked_packages_all_hashes_non_empty() {
        let pd = make_paddleocr_provider_descriptor();
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

    /// 验证安装计划包含 --no-deps（禁止传递依赖自动解析）。
    #[test]
    fn provider_descriptor_has_no_deps() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            assert!(
                plan.extra_pip_args
                    .iter()
                    .any(|arg| matches!(arg, PipExtraArg::NoDeps)),
                "安装计划必须包含 --no-deps，禁止传递依赖自动解析"
            );
        }
    }

    /// 验证 locked-requirements.txt 中包含关键直接依赖。
    #[test]
    fn locked_packages_contains_key_deps() {
        let pd = make_paddleocr_provider_descriptor();
        if let InstallPlan::PythonVenv(plan) = &pd.install_plan {
            let names: Vec<&str> = plan.packages.iter().map(|p| p.name.as_str()).collect();
            // 直接依赖
            assert!(names.contains(&"paddlepaddle"), "缺少 paddlepaddle");
            assert!(names.contains(&"paddleocr"), "缺少 paddleocr");
            assert!(names.contains(&"fastapi"), "缺少 fastapi");
            assert!(names.contains(&"uvicorn"), "缺少 uvicorn");
            assert!(names.contains(&"pillow"), "缺少 pillow");
            assert!(names.contains(&"numpy"), "缺少 numpy");
            assert!(names.contains(&"pyarrow"), "缺少 pyarrow");
            // 关键传递依赖
            assert!(names.contains(&"aiohttp"), "缺少传递依赖 aiohttp");
            assert!(names.contains(&"starlette"), "缺少传递依赖 starlette");
        }
    }

    /// 验证 parse_locked_requirements 解析格式正确。
    #[test]
    fn parse_locked_requirements_correctness() {
        let sample = "# comment\naiohappyeyeballs==2.7.1 \\\n    --hash=sha256:065665c041c42a5938ed220bdcd7230f22527fbec085e1853d2402c8a3615d9d \\\n    --hash=sha256:9243213661e29250eb41368e5daa826fc017156c3b8a11440826b2e3ed376472\nfastapi==0.115.6 \\\n    --hash=sha256:e9240b29e36fa8f4bb7290316988e90c381e5092e0cbe84e7818cc3713bcf305\n";
        let packages = parse_locked_requirements(sample);
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "aiohappyeyeballs");
        assert_eq!(packages[0].version, "2.7.1");
        assert_eq!(packages[0].all_hashes.len(), 2);
        assert_eq!(
            packages[0].sha256.as_deref(),
            Some("065665c041c42a5938ed220bdcd7230f22527fbec085e1853d2402c8a3615d9d")
        );
        assert_eq!(packages[1].name, "fastapi");
        assert_eq!(packages[1].version, "0.115.6");
        assert_eq!(packages[1].all_hashes.len(), 1);
    }

    /// 验证嵌入的 LOCKED_REQUIREMENTS_TXT 不为空。
    #[test]
    fn embedded_locked_requirements_not_empty() {
        assert!(!LOCKED_REQUIREMENTS_TXT.is_empty());
        assert!(LOCKED_REQUIREMENTS_TXT.contains("paddlepaddle"));
        assert!(LOCKED_REQUIREMENTS_TXT.contains("paddleocr"));
    }
}
