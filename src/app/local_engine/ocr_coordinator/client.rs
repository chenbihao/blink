//! PaddleOCR HTTP client：/recognize 识别请求、/health 健康探测、
//! endpoint/token 身份获取。请求级取消覆盖（select! + ctx.cancellation）在此层。

use std::time::Duration;

use bytes::Bytes;

use crate::domain::capability::builtins::ocr_engine::OcrResult;
use crate::domain::ocr::context::OcrRequestContext;
use crate::domain::ocr::error::StructuredOcrError;

use super::OcrCoordinator;
use super::mapping::map_paddleocr_response;
use super::singleflight::Lease;

impl OcrCoordinator {
    /// HTTP 调用 PaddleOCR /recognize。接收 Bytes，reqwest 直接消费，零拷贝。
    ///
    /// **取消覆盖**：HTTP 请求通过 select! 同时监听 ctx.cancellation.cancelled()。
    pub(super) async fn paddleocr_recognize(
        &self,
        png_data: Bytes,
        ctx: &OcrRequestContext,
        endpoint_url: &str,
        token: &str,
        lease: &Lease,
        request_png_size: (u32, u32),
    ) -> Result<OcrResult, StructuredOcrError> {
        if ctx.should_stop() {
            return Err(StructuredOcrError::cancelled());
        }

        let timeout = ctx.remaining_timeout().unwrap_or(Duration::from_secs(30));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            .build()
            .map_err(|e| {
                StructuredOcrError::protocol_error(format!("HTTP client 构建失败: {e}"))
            })?;

        let url = format!(
            "{endpoint_url}/recognize?request_id={}&timeout_ms={}",
            ctx.request_id,
            timeout.as_millis() as u32
        );

        // 尺寸一致性校验基准由 recognize 入口的预算检查提供（0.22.6.1），
        // 此处不再重复解析。png_data 在 move 到 HTTP body 之前不得被消费。

        let send_future = client
            .post(&url)
            .header("X-Engine-Token", token)
            .header("Content-Type", "image/png")
            .body(png_data)
            .send();

        let resp = tokio::select! {
            r = send_future => r.map_err(|e| {
                if ctx.is_cancelled() {
                    StructuredOcrError::cancelled()
                } else {
                    StructuredOcrError::protocol_error(format!("HTTP 请求失败: {e}"))
                }
            })?,
            _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
        };

        let status_code = resp.status();
        let resp_json: serde_json::Value = tokio::select! {
            r = resp.json() => r.map_err(|e| StructuredOcrError::protocol_error(format!("响应解析失败: {e}")))?,
            _ = ctx.cancellation.cancelled() => return Err(StructuredOcrError::cancelled()),
        };

        if !status_code.is_success() {
            let detail = resp_json
                .get("detail")
                .and_then(|v| v.as_str())
                .unwrap_or("unknown");
            return Err(match status_code.as_u16() {
                401 => StructuredOcrError::protocol_error(format!("token 不匹配: {detail}")),
                400 => StructuredOcrError::decode_error(detail),
                // 413 = Python 侧输入预算拒绝（compressed bytes/尺寸/decoded 像素）——
                // 投影为结构化 input_too_large（带实际值与上限），不是 ProtocolError/Internal
                413 => StructuredOcrError::input_too_large(
                    format!("OCR 输入超出资源预算: {detail}"),
                    serde_json::json!({ "http_status": 413, "reason": detail }),
                ),
                408 => StructuredOcrError::timeout(),
                503 => {
                    let model_state = resp_json
                        .get("detail")
                        .and_then(|v| v.as_str())
                        .unwrap_or("unknown");
                    if model_state.contains("model_failed") {
                        StructuredOcrError::model_not_ready("Failed")
                    } else {
                        StructuredOcrError::model_not_ready(model_state)
                    }
                }
                _ => StructuredOcrError::protocol_error(format!("HTTP {status_code}: {detail}")),
            });
        }

        // 从 lease 传入模型契约，不再硬编码
        let (expected_model_id, expected_model_revision) = lease.model_contract();
        map_paddleocr_response(
            &resp_json,
            &ctx.request_id,
            &expected_model_id,
            expected_model_revision,
            request_png_size,
        )
    }

    /// 获取 PaddleOCR endpoint 和 auth token。
    ///
    /// 接受 ctx 以在 endpoint 获取过程中覆盖取消/deadline（Handoff B.III）。
    pub(super) async fn get_paddleocr_endpoint(
        &self,
        ctx: &OcrRequestContext,
    ) -> Option<(String, String)> {
        // 取消覆盖：endpoint 获取前检查
        if ctx.should_stop() {
            return None;
        }
        // Task 5: 使用 select! 覆盖取消和 deadline
        let identity = tokio::select! {
            r = self.engine_service.get_current_identity(&self.paddleocr_engine_id) => r.ok()??,
            _ = ctx.cancellation.cancelled() => return None,
            _ = self.sleep_until_deadline(ctx) => return None,
        };
        Some((identity.endpoint.base_url(), identity.token))
    }

    /// 诊断路径：无取消覆盖的 endpoint 获取（paddleocr_health_info 专用）。
    async fn get_paddleocr_endpoint_raw(&self) -> Option<(String, String)> {
        let identity = self
            .engine_service
            .get_current_identity(&self.paddleocr_engine_id)
            .await
            .ok()??;
        Some((identity.endpoint.base_url(), identity.token))
    }

    pub(super) async fn paddleocr_health_info(
        &self,
    ) -> (
        Option<String>,
        Option<String>,
        Option<String>,
        Option<String>,
    ) {
        // 诊断路径：使用无 ctx 的简化获取
        let (endpoint_url, token) = match self.get_paddleocr_endpoint_raw().await {
            Some(et) => et,
            None => return (None, None, None, None),
        };
        let client = match reqwest::Client::builder()
            .timeout(Duration::from_secs(3))
            .build()
        {
            Ok(c) => c,
            Err(_) => return (None, None, None, None),
        };
        let resp = client
            .get(format!("{endpoint_url}/health"))
            .header("X-Engine-Token", token)
            .send()
            .await;
        let resp = match resp {
            Ok(r) if r.status().is_success() => r,
            _ => return (None, None, None, None),
        };
        let json: serde_json::Value = match resp.json().await {
            Ok(j) => j,
            Err(_) => return (None, None, None, None),
        };
        let get_str = |key: &str| {
            json.get(key)
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
        };
        (
            get_str("model_id"),
            get_str("model_revision"),
            get_str("instance_id"),
            get_str("actual_backend"),
        )
    }
}
