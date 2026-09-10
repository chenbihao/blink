//! 诊断与状态投影：executor 状态探测、WinRT 语言信息、诊断缓存更新。
//!
//! 0.22.8-D: 从 engine_service.get_status() 改为 executor.state() 投影。
//! 被 OcrBackendRouter::diagnose 与 singleflight 就绪检查复用。
//!
//! 0.22.11: 「已安装」与「已加载/Ready」分开表达。
//! - `is_paddleocr_installed()` 从 deployment 文件真源判定，不依赖内存 executor。
//! - `is_paddleocr_ready()` 仍检查 executor 的 Ready 状态（内存态）。
//! - `paddleocr_service_state()` / `paddleocr_model_state()` 区分
//!   NotInstalled（deployment 不存在）和 NotLoaded（deployment 存在但 executor 未注入）。

use crate::domain::capability::builtins::ocr_engine::backend as get_global_backend;
use crate::domain::ocr::router::OcrRouteDiagnosis;
use crate::infra::local_engine::onnx_ocr::ExecutorState;

use super::OcrCoordinator;

impl OcrCoordinator {
    /// 0.22.8-D: 检查 executor 是否 Ready。
    /// 0.22.11: Ready 只表示内存中 executor 已构建且 Session 就绪。
    pub(super) async fn is_paddleocr_ready(&self) -> bool {
        match self
            .executor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(e) => e.state().is_ready(),
            None => false,
        }
    }

    /// 0.22.11: 检查 PaddleOCR 环境是否已安装。
    ///
    /// 「已安装」从 deployment 文件真源判定（与 `build_onnx_executor_from_deployment`
    /// 和 `PaddleocrAdapter::self_test` 同源），不依赖内存中是否已有 executor。
    ///
    /// 判定标准：active deployment 目录存在且四个关键文件齐全
    /// (det_model, rec_model, dict_path, dll_path)。
    pub(super) async fn is_paddleocr_installed(&self) -> bool {
        Self::check_deployment_installed()
    }

    /// 检查 deployment 文件是否齐全（纯函数，可被 self_test 复用）。
    fn check_deployment_installed() -> bool {
        use crate::app::local_engine::paddleocr::onnx_inprocess_deployment_space;
        use crate::infra::local_engine::deployment::DeploymentStore;

        let (_pointer, dir) = match DeploymentStore::active_dir(&onnx_inprocess_deployment_space())
        {
            Ok(Some(p)) => p,
            _ => return false,
        };

        let det = dir.join("pp-ocrv6_tiny_det.onnx");
        let rec = dir.join("pp-ocrv6_tiny_rec.onnx");
        let dict = dir.join("ppocrv6_tiny_dict.txt");
        let dll = dir.join("onnxruntime.dll");

        det.exists() && rec.exists() && dict.exists() && dll.exists()
    }

    /// 0.22.11: executor 状态投影为 service_state 字符串。
    ///
    /// 区分三种状态：
    /// - NotInstalled: deployment 文件不存在
    /// - NotLoaded: deployment 存在但 executor 未注入（已安装未加载）
    /// - 其他: executor 内部状态投影
    pub(super) async fn paddleocr_service_state(&self) -> String {
        if !Self::check_deployment_installed() {
            return "NotInstalled".to_string();
        }
        match self
            .executor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(e) => e.state().to_string(),
            None => "NotLoaded".to_string(),
        }
    }

    /// 0.22.11: executor 状态投影为 model_state 字符串。
    ///
    /// 与 `paddleocr_service_state` 同样区分 NotInstalled / NotLoaded。
    pub(super) async fn paddleocr_model_state(&self) -> String {
        if !Self::check_deployment_installed() {
            return "NotInstalled".to_string();
        }
        match self
            .executor
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
        {
            Some(e) => match e.state() {
                ExecutorState::Ready { .. } => "Ready".to_string(),
                ExecutorState::Starting { .. } => "Loading".to_string(),
                ExecutorState::Failed { .. } => "Failed".to_string(),
                ExecutorState::Idle => "Idle".to_string(),
                ExecutorState::Stopping { .. } => "Stopping".to_string(),
            },
            None => "NotLoaded".to_string(),
        }
    }

    pub(super) fn update_diagnosis(&self, diagnosis: OcrRouteDiagnosis) {
        if let Ok(mut w) = self.last_diagnosis.write() {
            *w = Some(diagnosis);
        }
    }

    pub(super) async fn winrt_diagnostics(&self) -> (Vec<String>, Option<String>) {
        let backend = get_global_backend();
        let langs = backend.available_languages().await;
        let lang = backend.engine_language().await;
        (langs, lang)
    }
}
