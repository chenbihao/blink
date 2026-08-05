//! Mock HTTP server：起在环回 127.0.0.1:0 上，可配置响应延迟。
//!
//! **为什么手写而不依赖 wiremock**：spike 只需要"接收连接、按配置延迟后回一个最小 HTTP 200"，
//! 引入 wiremock 会拉进十几个依赖。手写 60 行 tokio TCP 更快、更可控。

use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::Notify;

/// Mock server 句柄。`base_url` 供客户端连接；`shutdown()` 优雅停止。
pub struct MockServer {
    pub base_url: String,
    shutdown: Arc<Notify>,
}

impl MockServer {
    /// 起一个 mock server，收到任何 HTTP 请求后延迟 `response_delay` 再回 200。
    /// 用于验证"客户端硬超时"——如果 `response_delay` > 客户端超时，客户端必须能主动 abort。
    pub async fn start(response_delay: Duration) -> std::io::Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let addr = listener.local_addr()?;
        let shutdown = Arc::new(Notify::new());
        let shutdown_signal = shutdown.clone();

        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = shutdown_signal.notified() => break,
                    accept = listener.accept() => {
                        match accept {
                            Ok((mut stream, _)) => {
                                tokio::spawn(async move {
                                    // 读点 HTTP request bytes（不解析，只是让客户端认为连接已建立）
                                    let mut buf = [0u8; 512];
                                    let _ = tokio::time::timeout(
                                        Duration::from_millis(100),
                                        stream.read(&mut buf),
                                    ).await;

                                    // 按配置延迟——模拟慢供应商
                                    tokio::time::sleep(response_delay).await;

                                    let body = br#"{"ok":true}"#;
                                    let response = format!(
                                        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n",
                                        body.len(),
                                    );
                                    let _ = stream.write_all(response.as_bytes()).await;
                                    let _ = stream.write_all(body).await;
                                    let _ = stream.shutdown().await;
                                });
                            }
                            Err(_) => break,
                        }
                    }
                }
            }
        });

        Ok(MockServer {
            base_url: format!("http://{addr}"),
            shutdown,
        })
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.shutdown.notify_waiters();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_server_boots_and_serves_after_delay() {
        let server = MockServer::start(Duration::from_millis(50)).await.unwrap();
        // 简单验证：给足够时间等响应，reqwest 拿到 200
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let resp = client.get(&server.base_url).send().await.unwrap();
        assert_eq!(resp.status(), 200);
    }
}
