//! MCP 旧版 SSE transport 实现（0.13.8）。
//!
//! ## 背景
//!
//! rmcp 1.8 仅提供 `StreamableHttpClientTransport`（MCP 2025-03-26 规范），
//! 它用 POST 发消息到单个端点。但很多 MCP server（如 JetBrains IDE）使用旧版
//! SSE transport（MCP 2024-11-05 规范），协议流程不同：
//!
//! 1. GET `/sse` → 建立 SSE 长连接
//! 2. server 推送 `endpoint` 事件 → client 获取 POST URL
//! 3. client POST JSON-RPC 消息到 POST URL
//! 4. server 通过 SSE 流推送响应
//!
//! Streamable HTTP 对 `/sse` 发 POST → 405 Method Not Allowed。
//!
//! ## 实现
//!
//! `SseClientTransport` 实现 `rmcp::Transport<RoleClient>`：
//! - 构造时 GET SSE URL + 等待 `endpoint` 事件 → 获取 POST URL
//! - 后台 task 持续解析 SSE 事件 → JSON-RPC 消息 → channel
//! - `send`：POST JSON-RPC 到 POST URL
//! - `receive`：从 channel 读取下一条 server 消息
//! - `close`：设置 shutdown flag，后台 task 退出

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use futures::StreamExt;
use rmcp::model::ServerJsonRpcMessage;
use rmcp::transport::Transport;
use rmcp::{RoleClient, service::TxJsonRpcMessage};
use sse_stream::SseStream;
use thiserror::Error;
use tokio::sync::mpsc::Receiver;

/// SSE transport 错误类型。
#[derive(Debug, Error)]
pub enum SseTransportError {
    #[error("SSE 解析错误: {0}")]
    Sse(String),
    #[error("HTTP 请求错误: {0}")]
    Http(String),
    #[allow(dead_code)] // 预留变体：receive 关闭时目前返回 None 而非构造此错误
    #[error("Transport channel closed")]
    TransportChannelClosed,
    #[error("反序列化错误: {0}")]
    Deserialize(String),
    #[error("POST 请求失败: {0}")]
    PostFailed(String),
}

impl From<sse_stream::Error> for SseTransportError {
    fn from(e: sse_stream::Error) -> Self {
        Self::Sse(e.to_string())
    }
}

impl From<reqwest::Error> for SseTransportError {
    fn from(e: reqwest::Error) -> Self {
        Self::Http(e.to_string())
    }
}

impl From<serde_json::Error> for SseTransportError {
    fn from(e: serde_json::Error) -> Self {
        Self::Deserialize(e.to_string())
    }
}

/// 旧版 SSE transport——实现 rmcp `Transport<RoleClient>`。
///
/// 协议流程：GET SSE URL → 等待 endpoint 事件 → POST 消息到 endpoint URL →
/// 通过 SSE 流接收响应。
pub struct SseClientTransport {
    /// 从后台 SSE 解析 task 接收消息的 channel。
    rx: Receiver<Result<ServerJsonRpcMessage, SseTransportError>>,
    /// POST 消息的目标 URL（从 SSE `endpoint` 事件获取）。
    post_url: String,
    /// reqwest HTTP client（clone 廉价——内部 Arc）。
    client: reqwest::Client,
    /// 自定义请求头（随 POST 请求一起发送）。
    headers: HashMap<String, String>,
    /// 后台 task 的 shutdown flag。
    shutdown: Arc<AtomicBool>,
}

impl SseClientTransport {
    /// 创建 SSE client transport。
    ///
    /// 流程：
    /// 1. GET SSE URL → 建立 SSE 长连接
    /// 2. 等待 `endpoint` 事件 → 获取 POST URL
    /// 3. 后台 task 持续解析 SSE 事件 → JSON-RPC 消息 → channel
    ///
    /// 返回 `Err` 表示 SSE 连接/握手失败。
    pub async fn new(sse_url: &str, headers: &HashMap<String, String>) -> Result<Self, String> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

        // 构建 GET 请求
        let mut req = client.get(sse_url).header("Accept", "text/event-stream");
        for (k, v) in headers {
            req = req.header(k, v);
        }

        // 发送 GET 请求
        let response = req.send().await.map_err(|e| format!("SSE 连接失败: {e}"))?;

        if !response.status().is_success() {
            return Err(format!(
                "SSE 连接失败：HTTP {} {}",
                response.status().as_u16(),
                response.status().canonical_reason().unwrap_or("")
            ));
        }

        // 检查 content-type
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|v| v.to_str().ok())
            .unwrap_or("");
        if !content_type.contains("text/event-stream") {
            return Err(format!(
                "SSE 连接失败：期望 text/event-stream，得到 {content_type}"
            ));
        }

        tracing::info!(sse_url, "SSE: 连接已建立，等待 endpoint 事件");

        // HTTP response body → SSE 事件流
        let byte_stream = response.bytes_stream();
        let sse_stream = SseStream::from_bytes_stream(byte_stream);

        // 创建 channel + shutdown flag
        let (tx, rx) =
            tokio::sync::mpsc::channel::<Result<ServerJsonRpcMessage, SseTransportError>>(32);
        let shutdown = Arc::new(AtomicBool::new(false));

        // 用于等待 POST URL 的共享状态
        let post_url_holder = Arc::new(tokio::sync::Mutex::new(None::<String>));
        let post_url_for_task = post_url_holder.clone();
        let shutdown_for_task = shutdown.clone();

        // 后台 task：持续解析 SSE 事件 → JSON-RPC 消息 → channel
        tokio::spawn(async move {
            let mut sse_stream = std::pin::pin!(sse_stream);
            loop {
                if shutdown_for_task.load(Ordering::Relaxed) {
                    break;
                }
                match sse_stream.next().await {
                    Some(Ok(sse)) => {
                        // 检查 endpoint 事件（包含 POST URL）
                        if sse.event.as_deref() == Some("endpoint") {
                            if let Some(data) = &sse.data {
                                let url = data.trim();
                                tracing::info!(post_url = url, "SSE: 收到 endpoint 事件");
                                let mut lock = post_url_for_task.lock().await;
                                *lock = Some(url.to_string());
                            }
                            continue;
                        }

                        // 解析为 JSON-RPC 消息
                        if let Some(data) = &sse.data {
                            if data.trim().is_empty() {
                                continue;
                            }
                            match serde_json::from_str::<ServerJsonRpcMessage>(data) {
                                Ok(msg) => {
                                    if tx.send(Ok(msg)).await.is_err() {
                                        break;
                                    }
                                }
                                Err(e) => {
                                    tracing::debug!(error = %e, "SSE: 消息解析失败，跳过");
                                }
                            }
                        }
                    }
                    Some(Err(e)) => {
                        let _ = tx.send(Err(SseTransportError::from(e))).await;
                        break;
                    }
                    None => break,
                }
            }
            tracing::debug!("SSE: 后台解析 task 已退出");
        });

        // 等待 POST URL（带超时）
        let post_url = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let lock = post_url_holder.lock().await;
                if let Some(url) = lock.as_ref() {
                    return url.clone();
                }
                drop(lock);
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .map_err(|_| "SSE 连接超时：未收到 endpoint 事件".to_string())?;

        // 如果 POST URL 是相对路径，解析为绝对 URL
        let post_url = resolve_url(sse_url, &post_url);

        tracing::info!(post_url = %post_url, "SSE: transport 已就绪");

        Ok(Self {
            rx,
            post_url,
            client,
            headers: headers.clone(),
            shutdown,
        })
    }
}

impl Transport<RoleClient> for SseClientTransport {
    type Error = SseTransportError;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleClient>,
    ) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send + 'static {
        let post_url = self.post_url.clone();
        let client = self.client.clone();
        let headers = self.headers.clone();

        async move {
            let json = serde_json::to_string(&item)?;
            let mut req = client
                .post(&post_url)
                .header("Content-Type", "application/json")
                .body(json);
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            let response = req.send().await?;
            if !response.status().is_success() {
                return Err(SseTransportError::PostFailed(format!(
                    "HTTP {} {}",
                    response.status().as_u16(),
                    response.status().canonical_reason().unwrap_or("")
                )));
            }
            Ok(())
        }
    }

    fn receive(
        &mut self,
    ) -> impl std::future::Future<Output = Option<rmcp::service::RxJsonRpcMessage<RoleClient>>> + Send
    {
        // 从 channel 读取下一条 server 消息
        async move {
            match self.rx.recv().await {
                Some(Ok(msg)) => Some(msg),
                Some(Err(_)) | None => None,
            }
        }
    }

    fn close(&mut self) -> impl std::future::Future<Output = Result<(), Self::Error>> + Send {
        self.shutdown.store(true, Ordering::Relaxed);
        async { Ok(()) }
    }
}

/// 将相对 POST URL 解析为绝对 URL（基于 SSE URL 的 scheme://host:port）。
///
/// 例：SSE URL = `http://127.0.0.1:64342/sse`，POST URL = `/message`
/// → `http://127.0.0.1:64342/message`
fn resolve_url(sse_url: &str, post_url: &str) -> String {
    if post_url.starts_with("http://") || post_url.starts_with("https://") {
        return post_url.to_string();
    }
    // 从 SSE URL 提取 scheme://host:port
    if let Some(idx) = sse_url.find("://") {
        let after_scheme = &sse_url[idx + 3..];
        if let Some(slash_idx) = after_scheme.find('/') {
            let base = &sse_url[..idx + 3 + slash_idx];
            return format!("{base}{post_url}");
        }
        return format!("{sse_url}{post_url}");
    }
    post_url.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_url_absolute() {
        assert_eq!(
            resolve_url(
                "http://127.0.0.1:64342/sse",
                "http://127.0.0.1:64342/message"
            ),
            "http://127.0.0.1:64342/message"
        );
    }

    #[test]
    fn resolve_url_relative() {
        assert_eq!(
            resolve_url("http://127.0.0.1:64342/sse", "/message"),
            "http://127.0.0.1:64342/message"
        );
    }

    #[test]
    fn resolve_url_relative_no_path() {
        assert_eq!(
            resolve_url("http://localhost:8080", "/message"),
            "http://localhost:8080/message"
        );
    }
}
