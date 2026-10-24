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
    BoxedStream, Error as HttpClientError, HeaderMap, HttpClientExt, LazyBody, Method,
    MultipartForm, Request, Response, StreamingResponse, Uri,
};
use rig_core::wasm_compat::WasmCompatSend;
use rig_reqwest::ReqwestClient;

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
/// 内部持有 `rig_reqwest::ReqwestClient`（rig 0.43 起默认 transport 拆到 rig-reqwest；
/// `Default` 即 rig 的进程共享 client，包装共享 reqwest 连接池，clone 廉价），
/// 满足 rig `Client<H>` 对 `H: Clone + Debug + Default` 的约束。
///
/// 0.27.4：AI factory 改用 [`LoggingHttpClient::from_net`]——client 来自 net 工厂
/// 的 AI 类别（代理/直连策略统一接线）；`Default`（rig 共享 client）保留给测试。
#[derive(Debug, Clone, Default)]
pub struct LoggingHttpClient(pub ReqwestClient);

impl LoggingHttpClient {
    /// 从 net 工厂构造（生产路径）：代理策略与 network generation 生效，
    /// provider 重建（fingerprint 变化）后自动换新 client。
    pub fn from_net() -> Self {
        Self(crate::net::rig_http_client())
    }
}

/// 共享 reqwest client 未构建成功（reqwest builder 失败，极罕见）时 send 报的占位错误。
#[derive(Debug)]
struct SharedClientUnavailable;

impl std::fmt::Display for SharedClientUnavailable {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "进程共享 reqwest client 不可用")
    }
}

impl std::error::Error for SharedClientUnavailable {}

impl LoggingHttpClient {
    /// 取内部 reqwest client（共享 client 构建失败时返回错误，与 rig-reqwest 语义一致）。
    fn inner_client(&self) -> Result<reqwest::Client, HttpClientError> {
        self.0
            .inner()
            .cloned()
            .ok_or_else(|| HttpClientError::instance(SharedClientUnavailable))
    }
}

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
        let client = self.inner_client();
        async move {
            let client = client?;
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
        let client = self.inner_client();
        async move {
            let client = client?;
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

/// 打印请求体（trace；0.27.10 起图片/base64 脱敏——只留长度元数据）。
fn log_request(method: &Method, uri: &Uri, bytes: &Bytes) {
    tracing::trace!(
        target: TARGET,
        method = %method,
        uri = %uri,
        body = redact_image_payload(&truncate_lossy(bytes)),
        "HTTP 请求体"
    );
}

/// 0.27.10：图片 payload 脱敏（phase §3.4——HTTP 正文日志对图片/base64
/// 脱敏，只留必要诊断元数据）。
///
/// 策略（递归形状无关）：任何 ≥1024 字符的 base64 字符块替换为
/// `<redacted-image:{len}ch>`。该阈值覆盖 OpenAI `image_url` data URL、
/// Anthropic `source.base64`、Gemini `inline_data.data` 与裸 base64 正文；
/// 常规文本/短字段不受影响。data URL 前缀保留以便识别格式。
pub(crate) fn redact_image_payload(body: &str) -> String {
    const MIN_REDACT_LEN: usize = 1024;
    // 预扫描快速路径：无长 base64 块时零拷贝返回原串
    let longest = longest_base64_run(body);
    if longest < MIN_REDACT_LEN {
        return body.to_string();
    }

    // base64 字符均为 ASCII——按切片复制非 run 段，多字节 UTF-8 不经过逐字节路径
    let mut out = String::with_capacity(body.len() / 8);
    let bytes = body.as_bytes();
    let mut i = 0;
    let mut last_copy = 0;
    while i < bytes.len() {
        if is_base64_char(bytes[i]) {
            let start = i;
            while i < bytes.len() && is_base64_char(bytes[i]) {
                i += 1;
            }
            if i - start >= MIN_REDACT_LEN {
                out.push_str(&body[last_copy..start]);
                out.push_str(&format!("<redacted-image:{}ch>", i - start));
                last_copy = i;
            }
        } else {
            i += 1;
        }
    }
    out.push_str(&body[last_copy..]);
    out
}

fn is_base64_char(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'+' || b == b'/' || b == b'='
}

fn longest_base64_run(body: &str) -> usize {
    let mut longest = 0;
    let mut current = 0;
    for &b in body.as_bytes() {
        if is_base64_char(b) {
            current += 1;
            longest = longest.max(current);
        } else {
            current = 0;
        }
    }
    longest
}

/// 非 2xx 错误：读响应体文本拼进错误（0.43 起保留 status/headers/body 三元组，
/// blink 现有 `map_rig_error` 的 4xx/5xx 诊断路径不受影响）。
async fn non_success_error(response: reqwest::Response) -> HttpClientError {
    let status = response.status();
    let headers: HeaderMap = response.headers().clone();
    let message = response
        .text()
        .await
        .unwrap_or_else(|error| format!("failed to read error response body: {error}"));
    HttpClientError::non_success_with_details(status, headers, message)
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

#[cfg(test)]
mod redact_tests {
    use super::redact_image_payload;

    #[test]
    fn redact_leaves_normal_bodies_untouched() {
        let body = r#"{"model":"gpt-4o","messages":[{"role":"user","content":"你好世界"}]}"#;
        assert_eq!(redact_image_payload(body), body);
        // 短 base64 字段（如哈希）不触发
        let with_short = r#"{"id":"QUJDREVG"}"#;
        assert_eq!(redact_image_payload(with_short), with_short);
    }

    #[test]
    fn redact_replaces_long_base64_runs() {
        let b64 = "A".repeat(2048);
        let body = format!(r#"{{"image_url":{{"url":"data:image/png;base64,{b64}"}}}}"#);
        let out = redact_image_payload(&body);
        assert!(!out.contains(&b64), "长 base64 被替换");
        assert!(out.contains("<redacted-image:2048ch>"), "保留长度元数据");
        assert!(out.contains("data:image/png;base64,"), "data URL 前缀保留");
    }

    #[test]
    fn redact_handles_multiple_and_multibyte() {
        let b64a = "B".repeat(1500);
        let b64b = "C".repeat(1200);
        let body = format!("中文前缀 {b64a} 中间文本 {b64b} 尾部");
        let out = redact_image_payload(&body);
        assert!(out.contains("中文前缀") && out.contains("中间文本") && out.contains("尾部"));
        assert!(out.contains("<redacted-image:1500ch>"));
        assert!(out.contains("<redacted-image:1200ch>"));
        assert!(!out.contains(&b64a) && !out.contains(&b64b));
    }
}
