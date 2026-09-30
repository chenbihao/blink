//! HTTP 请求/响应体日志包装（0.21.16）。
//!
//! 实现 rig-core `HttpClientExt`，包一层 `reqwest::Client`，打印发给 AI provider 的
//! **原始 wire JSON**（请求体 + SSE 响应帧）。体量很大（尤其带 MCP 工具池的请求体一次
//! 可达几十 KB），因此由设置页「AI HTTP 请求/响应体日志」开关控制，**默认关闭**：
//!
//! - 关闭（默认）：纯透传，零日志开销
//! - 开启：以 **debug** 级打印（`blink::ai::http` target）——请求体 + 响应状态码
//!   （`send`，非流式）+ 逐 chunk 原始 SSE 帧（`send_streaming`，对话）
//!
//! 该层拿到的是真实发出的请求 JSON 与真实收到的 SSE 帧，用于排查 provider 兼容问题
//! （如本地 qwen 思考块为何不显示——直接看响应里有没有 `reasoning_content` 字段）。
//!
//! **接入**：`domain::ai::factory` 的 `build_openai_client` / `build_anthropic_client` /
//! `build_gemini_client` / `build_ollama_client` 统一用
//! `*::Client::builder().http_client(LoggingHttpClient::default())` 注入，四种协议全覆盖
//! （OpenAI 兼容 / Anthropic / Gemini / Ollama）。

use std::sync::atomic::{AtomicBool, Ordering};

use bytes::Bytes;
use futures::StreamExt;
use rig_core::http_client::{
    Error as HttpClientError, HttpClientExt, LazyBody, Method, MultipartForm, Request, Response,
    StreamingResponse, Uri, sse::BoxedStream,
};
use rig_core::wasm_compat::WasmCompatSend;

/// 请求/响应体日志 target（`blink` 子级，自动继承全局级别过滤）。
const TARGET: &str = "blink::ai::http";
/// 单条请求/响应体日志最大字符数（防超长工具 schema / 回复刷爆单行）。
const LOG_BODY_MAX: usize = 200_000;

/// 请求/响应体日志开关（设置页「AI HTTP 请求/响应体日志」触发，默认关）。
/// 关闭时纯透传零开销；开启后以 debug 级打印请求体与 SSE 响应帧。
static HTTP_BODY_LOG: AtomicBool = AtomicBool::new(false);

/// 运行时开关（设置页 `set_config` 触发，立即生效）。
pub fn set_body_log_enabled(enabled: bool) {
    HTTP_BODY_LOG.store(enabled, Ordering::Relaxed);
}

/// 是否打印请求/响应体。
fn body_log_enabled() -> bool {
    HTTP_BODY_LOG.load(Ordering::Relaxed)
}

/// 带请求/响应体日志的 HTTP client 包装。
///
/// 内部持有 `reqwest::Client`（Arc 后端，clone 廉价），`Default` = `reqwest::Client::new()`，
/// 满足 rig `Client<H>` 对 `H: Clone + Debug + Default` 的约束。
#[derive(Debug, Clone, Default)]
pub struct LoggingHttpClient(pub reqwest::Client);

impl HttpClientExt for LoggingHttpClient {
    fn send<T, U>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, HttpClientError>> + WasmCompatSend + 'static
    where
        T: Into<Bytes>,
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let do_log = body_log_enabled();
        let (parts, body) = req.into_parts();
        let bytes: Bytes = body.into();
        if do_log {
            log_request(&parts.method, &parts.uri, &bytes);
        }
        let client = self.0.clone();
        async move {
            let response = client
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(bytes)
                .send()
                .await
                .map_err(|e| HttpClientError::Instance(Box::new(e)))?;
            if !response.status().is_success() {
                return Err(non_success_error(response).await);
            }
            if do_log {
                tracing::debug!(
                    target: TARGET,
                    status = %response.status(),
                    "HTTP 响应（非流式，体不打印）"
                );
            }
            let mut res = Response::builder().status(response.status());
            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }
            let body: LazyBody<U> = Box::pin(async move {
                let bytes = response
                    .bytes()
                    .await
                    .map_err(|e| HttpClientError::Instance(Box::new(e)))?;
                Ok(U::from(bytes))
            });
            res.body(body).map_err(HttpClientError::Protocol)
        }
    }

    fn send_multipart<U>(
        &self,
        req: Request<MultipartForm>,
    ) -> impl Future<Output = Result<Response<LazyBody<U>>, HttpClientError>> + WasmCompatSend + 'static
    where
        U: From<Bytes> + WasmCompatSend + 'static,
    {
        let client = self.0.clone();
        async move { client.send_multipart(req).await }
    }

    fn send_streaming<T>(
        &self,
        req: Request<T>,
    ) -> impl Future<Output = Result<StreamingResponse, HttpClientError>> + WasmCompatSend
    where
        T: Into<Bytes> + WasmCompatSend,
    {
        let do_log = body_log_enabled();
        let (parts, body) = req.into_parts();
        let bytes: Bytes = body.into();
        if do_log {
            log_request(&parts.method, &parts.uri, &bytes);
        }
        let client = self.0.clone();
        async move {
            let request = client
                .request(parts.method, parts.uri.to_string())
                .headers(parts.headers)
                .body(bytes)
                .build()
                .map_err(|e| HttpClientError::Instance(Box::new(e)))?;
            let response = client
                .execute(request)
                .await
                .map_err(|e| HttpClientError::Instance(Box::new(e)))?;
            if !response.status().is_success() {
                return Err(non_success_error(response).await);
            }
            let status = response.status();
            let mut res = Response::builder()
                .status(status)
                .version(response.version());
            if let Some(hs) = res.headers_mut() {
                *hs = response.headers().clone();
            }
            let body: BoxedStream = Box::pin(response.bytes_stream().map(move |chunk| {
                let chunk = chunk.map_err(|e| HttpClientError::Instance(Box::new(e)));
                if do_log && let Ok(bytes) = &chunk {
                    tracing::debug!(
                        target: TARGET,
                        status = %status,
                        body = %truncate_lossy(bytes),
                        "HTTP 流式响应体"
                    );
                }
                chunk
            }));
            res.body(body).map_err(HttpClientError::Protocol)
        }
    }
}

/// 打印请求体（trace）。
fn log_request(method: &Method, uri: &Uri, bytes: &Bytes) {
    tracing::trace!(
        target: TARGET,
        method = %method,
        uri = %uri,
        body = %truncate_lossy(bytes),
        "HTTP 请求体"
    );
}

/// 非 2xx 错误：读响应体文本拼进错误（与 rig `non_success_status_error` 一致，保证 blink
/// 现有 `map_rig_error` 的 4xx/5xx 诊断路径不受影响）。
async fn non_success_error(response: reqwest::Response) -> HttpClientError {
    let status = response.status();
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
    HttpClientError::InvalidStatusCodeWithMessage(status, message)
}

/// UTF-8 lossy + 截断，供日志单行展示。
fn truncate_lossy(bytes: &[u8]) -> String {
    let s = String::from_utf8_lossy(bytes);
    let count = s.chars().count();
    if count <= LOG_BODY_MAX {
        return s.into_owned();
    }
    let mut out: String = s.chars().take(LOG_BODY_MAX).collect();
    out.push('…');
    out
}
