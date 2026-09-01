//! 诊断与状态投影：executor 状态探测、WinRT 语言信息、诊断缓存更新。
//!
//! 0.22.8-D: 从 engine_service.get_status() 改为 executor.state() 投影。
//! 被 OcrBackendRouter::diagnose 与 singleflight 就绪检查复用。

use crate::domain::capability::builtins::ocr_engine::backend as get_global_backend;
use crate::domain::ocr::router::OcrRouteDiagnosis;
use crate::infra::local_engine::onnx_ocr::ExecutorState;

use super::OcrCoordinator;

impl OcrCoordinator {
    /// 0.22.8-D: 检查 executor 是否 Ready。
    pub(super) async fn is_paddleocr_ready(&self) -> bool {
        match self.executor.read().unwrap().as_ref() {
            Some(e) => e.state().is_ready(),
            None => false,
        }
    }

    /// 0.22.8-D: 检查 executor 是否已安装（executor 存在即视为已安装）。
    pub(super) async fn is_paddleocr_installed(&self) -> bool {
        self.executor.read().unwrap().is_some()
    }

    /// 0.22.8-D: executor 状态投影为 service_state 字符串。
    pub(super) async fn paddleocr_service_state(&self) -> String {
        match self.executor.read().unwrap().as_ref() {
            Some(e) => e.state().to_string(),
            None => "NotInstalled".to_string(),
        }
    }

    /// 0.22.8-D: executor 状态投影为 model_state 字符串。
    pub(super) async fn paddleocr_model_state(&self) -> String {
        match self.executor.read().unwrap().as_ref() {
            Some(e) => match e.state() {
                ExecutorState::Ready { .. } => "Ready".to_string(),
                ExecutorState::Starting { .. } => "Loading".to_string(),
                ExecutorState::Failed { .. } => "Failed".to_string(),
                ExecutorState::Idle => "Idle".to_string(),
                ExecutorState::Stopping { .. } => "Stopping".to_string(),
            },
            None => "NotInstalled".to_string(),
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
