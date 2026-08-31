//! PaddleOCR 启动构造：`PaddleOcrEngineConfig` 配置投影 + `LaunchDescriptor`
//! （active deployment venv 解析、server 脚本释放、启动参数、受限环境变量）。

use std::collections::HashMap;

use crate::domain::local_engine::{
    ErrorPhase, LaunchContext, LaunchDescriptor, LocalEngineError, LocalEngineErrorCode,
};
use crate::domain::ocr::config::PaddleModel;
use crate::infra::local_engine::runtime as engine_runtime;
use crate::infra::local_engine::runtime::EngineId;

use super::{
    BLINK_OCR_SERVER_PY, OCR_RECT_PY, PADDLEOCR_ENGINE_ID, active_deployment_venv_python,
    check_paddleocr_with,
};

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
pub(super) fn build_paddleocr_launch_descriptor(
    ocr_config: &PaddleOcrEngineConfig,
    ctx: &LaunchContext,
) -> Result<LaunchDescriptor, LocalEngineError> {
    let port = ctx.endpoint.port();

    // 使用 deployment-managed venv（由 PythonVenvProvider 创建的隔离 venv）
    let engine_id = EngineId::new(&ctx.engine_id).map_err(|e| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::Internal,
            ErrorPhase::Start,
            "engine_id 无效",
            format!("解析 engine_id 失败: {e}"),
        )
    })?;
    // 只使用 deployment-managed venv，不 fallback 到 legacy 全局 venv
    let python_path = active_deployment_venv_python(&engine_id);
    let python = python_path.ok_or_else(|| {
        LocalEngineError::with_detail(
            LocalEngineErrorCode::EnvironmentMissing,
            ErrorPhase::Start,
            "Python 环境未就绪",
            "PaddleOCR 环境未安装。请调用 install_paddleocr 或在设置页点击「安装环境」。\
             （Blink 会自动下载 uv + Python 3.12 + paddlepaddle + paddleocr）",
        )
    })?;

    // 检查 paddleocr 是否已安装（使用 active deployment venv 中的 python）
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
    let mut args: Vec<String> = vec![
        script_path.to_string_lossy().to_string(),
        "--port".to_string(),
        port.to_string(),
        "--model".to_string(),
        ocr_config.model.clone(),
        "--token".to_string(),
        ctx.token.clone(),
        "--engine-id".to_string(),
        ctx.engine_id.clone(),
        "--instance-id".to_string(),
        ctx.instance_id.clone(),
        "--model-cache".to_string(),
        model_cache_dir.to_string_lossy().to_string(),
        "--cpu-threads".to_string(),
        ocr_config.cpu_threads.to_string(),
    ];

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

    // rect 归一化 seam（server 运行时 import 的生产模块，与脚本同目录释放）
    let rect_module_path = dir.join("ocr_rect.py");
    let need_write_rect = match std::fs::read_to_string(&rect_module_path) {
        Ok(existing) => existing != OCR_RECT_PY,
        Err(_) => true,
    };
    if need_write_rect {
        std::fs::write(&rect_module_path, OCR_RECT_PY)
            .map_err(|e| format!("写入 ocr_rect.py 失败: {e}"))?;
        tracing::info!(path = %rect_module_path.display(), "已释放 ocr_rect.py");
    }

    Ok(script_path)
}
