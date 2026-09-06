//! PaddleOCR 本地引擎 adapter（0.22.5 引入，0.22.8 起 ONNX in-process）。
//!
//! 把 PP-OCRv6 本地引擎注册为 `LocalEngineAdapter`，使安装、状态、
//! 诊断和清理通过 `EngineManager` 管理。
//!
//! ## 当前实现（0.22.8+）
//!
//! - **ONNX in-process**：识别由 `OcrCoordinator` 持有的 `OnnxOcrExecutor`
//!   承载（`oar-ocr` + ORT lazy Session），adapter 不再 spawn 子进程。
//! - **descriptor 锁定 runtime/profile/model contract**：`OnnxRuntime` +
//!   asset-lock 编译期 artifact id；不新造第二套安装器。
//! - **稳定 engine id**：`paddleocr` 不变；旧 Python manifest 可安全读取
//!   并投影为 legacy（见 `ManifestExtension::PythonVenv`）。
//! - **adapter 从 OcrConfig 的已校验配置产生 `AdapterConfig`**：保留
//!   `paddle_model`、`cpu_threads` 语义（`PaddleOcrEngineConfig`）。
//! - **空间统计和清理区分 engine deployment / 模型缓存 / 公共资产**；
//!   单引擎清理不能连带删除公共资产。
//!
//! ## 与 FunASR adapter 的区别
//!
//! - **CapabilityKind::Ocr**（非 Stt）
//! - **LifecyclePolicy::OnDemand**（非 Manual）
//! - **in-process executor**：无子进程、无 NDJSON transport、无 HTTP health
//!
//! ## 子模块（0.22 结构拆分）
//!
//! - [`descriptor`]：`EngineDefinition` / `ProviderDescriptor` 编译期装配
//! - [`tests`]：adapter 回归测试

mod descriptor;
#[cfg(test)]
mod tests;

pub use self::descriptor::make_paddleocr_onnx_provider_descriptor;

use self::descriptor::make_paddleocr_descriptor;

use std::sync::Arc;

use crate::domain::local_engine::{
    AdapterConfig, AdapterSelfTest, DiagnosticEntry, EngineDefinition, EngineDiagnostic,
    ErrorPhase, HealthMapping, LaunchContext, LocalEngineAdapter, LocalEngineError,
    LocalEngineErrorCode, ResolvedLaunch,
};
use crate::infra::local_engine::runtime::EngineId;

/// PaddleOCR 稳定 engine id。
pub const PADDLEOCR_ENGINE_ID: &str = "paddleocr";

// ── PaddleOcrEngineConfig（从 OcrConfig 投影） ────────────────────────────

/// PaddleOCR 引擎配置（从 `OcrConfig` 投影）。
///
/// 0.22.10：原 `launch.rs` 随 Python HTTP 启动链退役删除；本配置结构由
/// `config_source.rs` / `media.rs` 消费（`AdapterConfig.engine_config` 投影），
/// 予以保留。
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
    /// production lock——如果 paddle_model 未通过生产资格门，
    /// 强制降级为 Tiny（唯一通过候选）。这是 defense-in-depth：
    /// `OcrConfig::validate()` 已在配置写入时拦截，这里在投影时再检查一次。
    pub fn from_ocr_config() -> Self {
        let cfg = crate::domain::config::ocr_config::get_ocr_config();
        let model = if cfg.paddle_model.is_production_ready() {
            cfg.paddle_model.to_string()
        } else {
            tracing::warn!(
                model = %cfg.paddle_model,
                "paddle_model 未通过生产资格门，强制降级为 tiny"
            );
            crate::domain::ocr::config::PaddleModel::Tiny.to_string()
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

/// PaddleOCR ONNX in-process implementation 的部署空间（0.22.9 兼容真源映射）。
///
/// 0.22.8 ONNX deployment 位于 engine 级空间；读取层经此入口把旧 pointer
/// 明确映射到 in-process implementation——不复制、不改写、不搬迁用户资产。
pub(crate) fn onnx_inprocess_deployment_space()
-> crate::infra::local_engine::deployment::DeploymentSpace {
    let engine_id = EngineId::new(PADDLEOCR_ENGINE_ID).expect("paddleocr is valid");
    crate::infra::local_engine::deployment::DeploymentSpace::resolve(
        &engine_id,
        crate::domain::local_engine::ImplementationId::PaddleOcrOnnxInProcess,
    )
}

// ── PaddleocrAdapter ───────────────────────────────────────────────────────

/// PaddleOCR 本地引擎 adapter。
///
/// 实现 `LocalEngineAdapter` trait，把 PaddleOCR 特有的配置投影、
/// 诊断和 self-test 适配到领域统一协议。
///
/// ## 边界
///
/// - **不接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
/// - **不发送 Tauri 事件**：返回纯数据，由 app 层桥接。
/// - **不持有 AppHandle**：adapter 是纯逻辑，不接触 Tauri。
/// - **无子进程**：0.22.8 起识别在 `OcrCoordinator` 的 in-process executor
///   中执行，`prepare_launch` 不再是有效入口（fail-closed）。
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

    /// PaddleOCR 为 ONNX in-process 引擎，无子进程启动路径。
    ///
    /// 生产启动入口是 commands 层的 `start_inprocess` + `inject_executor`
    /// （`OcrCoordinator`），不经 `EngineManager::start` → `prepare_launch`。
    /// 此方法保留 trait 兼容并 fail-closed：任何到达这里的调用都是
    /// 误用，不得静默假装可以启动。
    fn prepare_launch(
        &self,
        ctx: &LaunchContext,
        _config: &AdapterConfig,
    ) -> Result<ResolvedLaunch, LocalEngineError> {
        Err(LocalEngineError::with_detail(
            LocalEngineErrorCode::Unsupported,
            ErrorPhase::Start,
            "PaddleOCR 无子进程启动路径",
            format!(
                "paddleocr 自 0.22.8 起为 ONNX in-process 引擎（ctx engine '{}')，\
                 识别由 OcrCoordinator 承载，不支持 prepare_launch",
                ctx.engine_id
            ),
        ))
    }

    /// PaddleOCR 无子进程 health 协议（0.22.8 起 in-process）。
    ///
    /// 引擎真实状态由 `OcrCoordinator` 的 executor 状态机投影，
    /// 不经 EngineManager 的 health 轮询。返回 Unknown 表示
    /// "本 adapter 不承载 health 语义"，不伪造健康状态。
    fn map_health(&self, _raw_health: &serde_json::Value) -> HealthMapping {
        HealthMapping {
            service: crate::domain::local_engine::ServiceHealth::Unknown,
            model: crate::domain::local_engine::ModelHealth::Unknown,
            environment: None,
            backend: None,
            model_id: None,
            model_revision: None,
            model_content_fingerprint: None,
        }
    }

    /// ONNX 模型资产由 deployment 事务（asset-lock + model generation）管理，
    /// 不经 FunASR 式的 `model_storage` active pointer，因此不能要求它。
    fn uses_managed_model_storage(&self) -> bool {
        false
    }

    /// adapter self-test。
    ///
    /// 0.22.8: ONNX in-process——检查 active deployment 中的 ORT DLL 和模型文件。
    fn self_test(&self) -> AdapterSelfTest {
        use crate::infra::local_engine::deployment::DeploymentStore;

        let (_pointer, dir) = match DeploymentStore::active_dir(&onnx_inprocess_deployment_space())
        {
            Ok(Some(p)) => p,
            _ => {
                return AdapterSelfTest::failed(
                    "ONNX OCR 环境未就绪。请在设置页点击「安装环境」按钮。\
                     （Blink 会自动下载 ONNX Runtime + PP-OCRv6 模型）",
                );
            }
        };

        let dll = dir.join("onnxruntime.dll");
        let det = dir.join("pp-ocrv6_tiny_det.onnx");
        let rec = dir.join("pp-ocrv6_tiny_rec.onnx");
        let dict = dir.join("ppocrv6_tiny_dict.txt");

        let mut missing = Vec::new();
        if !dll.exists() {
            missing.push("onnxruntime.dll");
        }
        if !det.exists() {
            missing.push("pp-ocrv6_tiny_det.onnx");
        }
        if !rec.exists() {
            missing.push("pp-ocrv6_tiny_rec.onnx");
        }
        if !dict.exists() {
            missing.push("ppocrv6_tiny_dict.txt");
        }

        if missing.is_empty() {
            AdapterSelfTest::passed()
        } else {
            AdapterSelfTest::failed(format!(
                "ONNX OCR 文件缺失: {}。请重新安装环境。",
                missing.join(", ")
            ))
        }
    }

    /// 0.22.8: ONNX 诊断——检查 deployment 中的 ORT DLL 和模型文件。
    fn diagnostics(&self) -> EngineDiagnostic {
        use crate::infra::local_engine::deployment::DeploymentStore;

        let mut entries = Vec::new();

        let (_pointer, dir) = match DeploymentStore::active_dir(&onnx_inprocess_deployment_space())
        {
            Ok(Some(p)) => p,
            _ => {
                entries.push(DiagnosticEntry {
                    key: "onnx_deployment".to_string(),
                    value: "false".to_string(),
                    label: "warning".to_string(),
                });
                return EngineDiagnostic { entries };
            }
        };

        entries.push(DiagnosticEntry {
            key: "onnx_deployment".to_string(),
            value: "true".to_string(),
            label: "info".to_string(),
        });

        let dll = dir.join("onnxruntime.dll");
        let det = dir.join("pp-ocrv6_tiny_det.onnx");
        let rec = dir.join("pp-ocrv6_tiny_rec.onnx");
        let dict = dir.join("ppocrv6_tiny_dict.txt");

        entries.push(DiagnosticEntry {
            key: "ort_dll".to_string(),
            value: if dll.exists() { "true" } else { "false" }.to_string(),
            label: if dll.exists() { "info" } else { "warning" }.to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "det_model".to_string(),
            value: if det.exists() { "true" } else { "false" }.to_string(),
            label: if det.exists() { "info" } else { "warning" }.to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "rec_model".to_string(),
            value: if rec.exists() { "true" } else { "false" }.to_string(),
            label: if rec.exists() { "info" } else { "warning" }.to_string(),
        });
        entries.push(DiagnosticEntry {
            key: "dict_file".to_string(),
            value: if dict.exists() { "true" } else { "false" }.to_string(),
            label: if dict.exists() { "info" } else { "warning" }.to_string(),
        });

        EngineDiagnostic { entries }
    }
}

// ── adapter 工厂函数 ───────────────────────────────────────────────────────

/// 创建 PaddleOCR adapter（`Arc<dyn LocalEngineAdapter>`）。
///
/// 对齐 `funasr::make_funasr_adapter()` 模式，供 main.rs wiring 使用。
pub fn make_paddleocr_adapter() -> Arc<dyn LocalEngineAdapter> {
    Arc::new(PaddleocrAdapter::new())
}
