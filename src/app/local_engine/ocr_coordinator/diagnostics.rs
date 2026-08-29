//! 诊断与状态投影：service/model/installed 状态探测、WinRT 语言信息、
//! 诊断缓存更新。被 OcrBackendRouter::diagnose 与 singleflight 就绪检查复用。

use crate::domain::capability::builtins::ocr_engine::backend as get_global_backend;
use crate::domain::local_engine::status::{DesiredState, ModelHealth, ServiceHealth};
use crate::domain::ocr::router::OcrRouteDiagnosis;

use super::OcrCoordinator;

impl OcrCoordinator {
    pub(super) async fn is_paddleocr_ready(&self) -> bool {
        let status = match self
            .engine_service
            .get_status(&self.paddleocr_engine_id)
            .await
        {
            Ok(s) => s,
            Err(_) => return false,
        };
        status.status.desired == DesiredState::Running
            && status.status.model == ModelHealth::Ready
            && status.status.service == ServiceHealth::Healthy
    }

    pub(super) async fn is_paddleocr_installed(&self) -> bool {
        let status = match self
            .engine_service
            .get_status(&self.paddleocr_engine_id)
            .await
        {
            Ok(s) => s,
            Err(_) => return false,
        };
        status.status.environment == crate::domain::local_engine::status::EnvironmentHealth::Ready
    }

    pub(super) async fn paddleocr_service_state(&self) -> String {
        let status = match self
            .engine_service
            .get_status(&self.paddleocr_engine_id)
            .await
        {
            Ok(s) => s,
            Err(_) => return "Unknown".to_string(),
        };
        format!("{:?}", status.status.service)
    }

    pub(super) async fn paddleocr_model_state(&self) -> String {
        let status = match self
            .engine_service
            .get_status(&self.paddleocr_engine_id)
            .await
        {
            Ok(s) => s,
            Err(_) => return "Unknown".to_string(),
        };
        format!("{:?}", status.status.model)
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
