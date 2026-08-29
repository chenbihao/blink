//! FunASR 本地引擎 adapter（0.22.3）。
//!
//! 把现有 Python/PyTorch FunASR 注册为 `LocalEngineAdapter`，使安装、启动、
//! 模型轮询、日志、空间和清理通过 `EngineManager` 管理。
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
//!
//! ## 子模块（0.22 结构拆分）
//!
//! - [`descriptor`]：`EngineDefinition` / `ProviderDescriptor` 编译期装配
//! - [`launch`]：`FunasrEngineConfig` 投影 + `LaunchDescriptor` 启动构造
//! - [`health`]：health / model identity 纯函数映射
//! - [`locks`]：`locked-requirements.txt` 解析（唯一锁源）
//! - [`tests`]：adapter 回归测试

mod descriptor;
mod health;
mod launch;
mod locks;
#[cfg(test)]
mod tests;

pub use self::descriptor::make_funasr_provider_descriptor;
// VadConfigProjection 保持原单文件的模块路径（配置 serde 形状的一部分）；
// 仅测试与非本模块路径消费，bin crate 下需 allow。
#[allow(unused_imports)]
pub use self::launch::{FunasrEngineConfig, VadConfigProjection};

use self::descriptor::make_funasr_descriptor;
use self::health::map_funasr_health;
use self::launch::build_funasr_launch_descriptor;

use std::sync::Arc;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, DiagnosticEntry, EngineDefinition, EngineDiagnostic,
    ErrorPhase, HealthMapping, LaunchContext, LocalEngineAdapter, LocalEngineError,
    LocalEngineErrorCode, ResolvedLaunch,
};
use crate::infra::local_engine::runtime::EngineId;

/// 嵌入的 blink_stt_server.py 脚本（随 Rust 二进制发布）。
///
/// 重新声明在此模块以保持 adapter 自包含；领域层的 `funasr.rs` 保留原始常量。
#[allow(dead_code)]
const BLINK_STT_SERVER_PY: &str =
    include_str!("../../../../resources/stt/funasr/blink_stt_server.py");

/// 嵌入的完整依赖锁文件（唯一锁源）。
///
/// 由 `uv pip compile --generate-hashes --index-url https://download.pytorch.org/whl/cpu
/// --extra-index-url https://pypi.org/simple` 生成，包含全部传递依赖及其 SHA-256。
/// `make_funasr_provider_descriptor()` 在运行时解析此文件生成 `PackageLock` 列表。
/// 安装时使用 `--require-hashes --no-deps` 强校验。
#[allow(dead_code)]
const LOCKED_REQUIREMENTS_TXT: &str =
    include_str!("../../../../resources/stt/funasr/locked-requirements.txt");

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
    descriptor: EngineDefinition,
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
    fn descriptor(&self) -> &EngineDefinition {
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
    /// 验证 FunASR Python 环境是否就绪（active deployment venv + funasr 包已安装）。
    ///
    /// 只检查 active deployment 的 venv，不 fallback 到旧全局 venv。
    /// 旧 `%APPDATA%\blink\python\venv` 只作为迁移/诊断来源，不影响新 generation 安装判定。
    fn self_test(&self) -> AdapterSelfTest {
        // 只使用 deployment-managed venv（由 PythonVenvProvider 创建的隔离 venv）
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();
        let python_path = active_deployment_venv_python(&engine_id);

        if python_path.is_none() {
            return AdapterSelfTest::failed(
                "FunASR 环境未安装。请在设置页「引擎」→「本地模型运行时」中点击「安装环境」按钮。\
                 （Blink 会自动下载 uv + Python 3.12 + torch + funasr）",
            );
        }

        // 检查 funasr 是否已安装（使用 active deployment venv 中的 python）
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
    /// 返回 FunASR 特有的诊断信息（active deployment venv、torch、funasr 版本等）。
    ///
    /// 诊断只解析 active deployment venv 的状态；
    /// 旧全局 venv 仅作为迁移诊断来源单独标注。
    fn diagnostics(&self) -> EngineDiagnostic {
        let mut entries = Vec::new();
        let engine_id = EngineId::new(FUNASR_ENGINE_ID).unwrap();

        // active deployment venv 状态
        let gen_python = active_deployment_venv_python(&engine_id);
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
            // 使用 active deployment venv python 检查版本和包
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

        // （旧全局 venv 迁移诊断已随世代目录迁移删除）

        EngineDiagnostic { entries }
    }
}

// ── 纯构造入口 ──────────────────────────────────────────────────────────────

/// 创建 FunASR adapter 的 `Arc` 引用。
///
/// 注册函数由 H6 接 wiring；本任务提供纯构造入口。
pub fn make_funasr_adapter() -> Arc<dyn LocalEngineAdapter> {
    Arc::new(FunasrAdapter::new())
}

// ── generation venv 辅助 ────────────────────────────────────────────────────

/// 获取 FunASR active deployment venv 中的 `python.exe` 路径。
///
/// 路径：`runtimes/engines/{engine_id}/slots/{slot}/venv/Scripts/python.exe`
///
/// 返回 `None` 表示尚未安装（deployment.json 不存在或 venv 目录缺失）。
///
/// 只使用 deployment-managed venv，不 fallback 到旧全局 venv。
fn active_deployment_venv_python(engine_id: &EngineId) -> Option<std::path::PathBuf> {
    let (_pointer, dir) =
        crate::infra::local_engine::deployment::DeploymentStore::active_dir(engine_id)
            .ok()
            .flatten()?;
    let python_exe = dir.join("venv").join("Scripts").join("python.exe");
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
