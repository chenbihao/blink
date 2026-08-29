//! PaddleOCR 本地引擎 adapter（0.22.5）。
//!
//! 把 PP-OCRv6 Python 服务注册为 `LocalEngineAdapter`，使安装、启动、
//! 模型轮询、日志、空间和清理通过 `EngineManager` 管理。
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
//!
//! ## 子模块（0.22 结构拆分）
//!
//! - [`descriptor`]：`EngineDefinition` / `ProviderDescriptor` 编译期装配
//! - [`launch`]：`PaddleOcrEngineConfig` 投影 + `LaunchDescriptor` 构造 + server 脚本释放
//! - [`health`]：health / model identity 纯函数映射
//! - [`locks`]：`locked-requirements.txt` 解析（唯一锁源）
//! - [`tests`]：adapter 回归测试

mod descriptor;
mod health;
mod launch;
mod locks;
#[cfg(test)]
mod tests;

pub use self::descriptor::{make_paddleocr_provider_descriptor, make_paddleocr_python_provider};
// ensure_ocr_server_script* 保持原单文件的模块路径（launch 内部与测试消费），
// bin crate 下需 allow。
#[allow(unused_imports)]
pub(crate) use self::launch::ensure_ocr_server_script_in;
#[allow(unused_imports)]
pub use self::launch::{PaddleOcrEngineConfig, ensure_ocr_server_script};

use self::descriptor::make_paddleocr_descriptor;
use self::health::map_paddleocr_health;
use self::launch::build_paddleocr_launch_descriptor;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, DiagnosticEntry, EngineDefinition, EngineDiagnostic,
    ErrorPhase, HealthMapping, LaunchContext, LocalEngineAdapter, LocalEngineError,
    LocalEngineErrorCode, ResolvedLaunch,
};
use crate::domain::ocr::config::PaddleModel;
use crate::infra::local_engine::runtime::EngineId;

/// 嵌入的 blink_ocr_server.py 脚本（随 Rust 二进制发布）。
#[allow(dead_code)]
const BLINK_OCR_SERVER_PY: &str =
    include_str!("../../../../resources/ocr/paddleocr/blink_ocr_server.py");

/// 嵌入的 rect 归一化 seam 模块（随 Rust 二进制发布，与 server 脚本同目录）。
///
/// 纯 stdlib 生产实现——blink_ocr_server.py import 它做 rect 归一化，
/// `test_ocr_rect.py` 直接测试同一实现，不存在两套映射。
#[allow(dead_code)]
const OCR_RECT_PY: &str = include_str!("../../../../resources/ocr/paddleocr/ocr_rect.py");

/// 嵌入的完整依赖锁文件（唯一锁源）。
///
/// 由 `uv pip compile --generate-hashes` 生成，包含全部传递依赖及其 SHA-256。
/// `make_paddleocr_provider_descriptor()` 在运行时解析此文件生成 `PackageLock` 列表。
/// 安装时使用 `--require-hashes --no-deps` 强校验。
#[allow(dead_code)]
const LOCKED_REQUIREMENTS_TXT: &str =
    include_str!("../../../../resources/ocr/paddleocr/locked-requirements.txt");

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
    descriptor: EngineDefinition,
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
    fn descriptor(&self) -> &EngineDefinition {
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

    /// PaddleOCR 目前由受信任的 Python wrapper 在专属缓存中下载模型，
    /// 并由 wrapper manifest 校验 det/rec 文件与内容指纹；尚未迁入统一
    /// 模型资产管理（active slot pointer），因此不能要求 model_storage
    /// active pointer。
    fn uses_managed_model_storage(&self) -> bool {
        false
    }

    /// adapter self-test。
    ///
    /// 验证 PaddleOCR Python 环境是否就绪（active deployment venv + paddlepaddle + paddleocr 已安装）。
    ///
    /// 只检查 deployment-managed venv（由 `PythonVenvProvider` 创建的隔离 venv）。
    fn self_test(&self) -> AdapterSelfTest {
        // 只使用 deployment-managed venv，不 fallback 到 legacy 全局 venv
        let python_path = active_deployment_venv_python(&self.descriptor.engine_id);
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

        // 只使用 deployment-managed venv
        let python_path = active_deployment_venv_python(&self.descriptor.engine_id);

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

        // 如果有 active deployment venv，报告其路径
        // 只报告 active deployment venv 状态
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

// ── Generation venv 解析 ──────────────────────────────────────────────────────

/// 从 active deployment pointer 解析受管 venv 中的 `python.exe` 路径。
///
/// 路径：`runtimes/engines/{engine_id}/slots/{slot}/venv/Scripts/python.exe`
///
/// 返回 `None` 表示尚未安装（deployment.json 不存在或 venv 目录缺失）。
fn active_deployment_venv_python(engine_id: &EngineId) -> Option<PathBuf> {
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
