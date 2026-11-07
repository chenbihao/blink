//! 0.28.27 共享下载内核行为测试：受控 TCP 服务器模拟慢流/切源/校验，
//! 覆盖「响应体停滞换源、字节上限、取消、成功路径与暂存清理」。

use std::time::Duration;

use tokio_util::sync::CancellationToken;

use crate::utils::download::{stream_to_file, AttemptError, StreamLimits};

/// 单响应受控 TCP 服务器：返回预置 HTTP/1.1 响应字节，支持写入后挂起（模拟停滞）。
struct TestServer {
    addr: std::net::SocketAddr,
    handle: tokio::task::JoinHandle<()>,
}

impl TestServer {
    /// `body_then_hang=true`：完整响应后保持连接（客户端停滞判定应触发）。
    /// `body_bytes`：响应体内容。
    fn spawn(body_bytes: Vec<u8>, body_then_hang: bool) -> Self {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        listener.set_nonblocking(true).unwrap();
        let listener = tokio::net::TcpListener::from_std(listener).unwrap();
        let handle = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            use tokio::io::{AsyncReadExt, AsyncWriteExt};
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await;
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body_bytes.len()
            );
            let _ = sock.write_all(header.as_bytes()).await;
            if body_then_hang {
                // 只发前半段后挂起：客户端应在响应体中途触发停滞/取消判定
                let half = body_bytes.len() / 2;
                let _ = sock.write_all(&body_bytes[..half]).await;
            } else {
                let _ = sock.write_all(&body_bytes).await;
            }
            if body_then_hang {
                // 挂住连接直到客户端断开
                let mut drain = [0u8; 64];
                loop {
                    match sock.read(&mut drain).await {
                        Ok(0) | Err(_) => break,
                        Ok(_) => {}
                    }
                }
            }
        });
        Self { addr, handle }
    }

    fn url(&self) -> String {
        format!("http://{}/file.bin", self.addr)
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

fn limits(stall_secs: u64, max_bytes: Option<u64>) -> StreamLimits {
    StreamLimits {
        request_timeout: Duration::from_secs(10),
        stall_timeout: Duration::from_secs(stall_secs),
        max_bytes,
    }
}

#[tokio::test]
async fn full_body_downloads_and_hashes() {
    let body = vec![7u8; 4096];
    let server = TestServer::spawn(body.clone(), false);
    let client = crate::net::client(crate::net::RequestCategory::Download);
    let dest = std::env::temp_dir().join(format!("blink-dl-test-{}.bin", std::process::id()));
    let mut hashed = 0usize;
    let outcome = stream_to_file(
        &client,
        &server.url(),
        &dest,
        &limits(5, None),
        None,
        |_, _| {},
        |chunk| hashed += chunk.len(),
    )
    .await
    .expect("download should succeed");
    assert_eq!(outcome.bytes as usize, body.len());
    assert_eq!(hashed, body.len());
    let on_disk = std::fs::read(&dest).unwrap();
    assert_eq!(on_disk, body, "产物正确性：落盘字节与响应体一致");
    let _ = std::fs::remove_file(&dest);
}

#[tokio::test]
async fn stalled_body_is_retryable_and_cleans_temp_file() {
    let body = vec![1u8; 1024];
    let server = TestServer::spawn(body, true); // 响应体后挂起
    let client = crate::net::client(crate::net::RequestCategory::Download);
    let dest = std::env::temp_dir().join(format!("blink-dl-test-stall-{}.bin", std::process::id()));
    let err = stream_to_file(
        &client,
        &server.url(),
        &dest,
        &limits(1, None), // 1s 无进度即停滞
        None,
        |_, _| {},
        |_: &[u8]| {},
    )
    .await
    .expect_err("stalled body should fail");
    assert!(
        matches!(err, AttemptError::Retryable { ref reason } if reason.contains("停滞")),
        "停滞应分类为可换源错误，实际 {err:?}"
    );
    assert!(!dest.exists(), "失败路径清理暂存文件");
}

#[tokio::test]
async fn byte_limit_is_retryable_and_cleans_temp_file() {
    let body = vec![2u8; 8192];
    let server = TestServer::spawn(body, false);
    let client = crate::net::client(crate::net::RequestCategory::Download);
    let dest = std::env::temp_dir().join(format!("blink-dl-test-limit-{}.bin", std::process::id()));
    let err = stream_to_file(
        &client,
        &server.url(),
        &dest,
        &limits(5, Some(1024)),
        None,
        |_, _| {},
        |_: &[u8]| {},
    )
    .await
    .expect_err("over-limit should fail");
    assert!(matches!(err, AttemptError::Retryable { .. }), "超限可换源，实际 {err:?}");
    assert!(!dest.exists(), "超限清理暂存文件");
}

#[tokio::test]
async fn cancellation_wins_and_cleans_temp_file() {
    let body = vec![3u8; 4096];
    let server = TestServer::spawn(body, true); // 挂住连接等取消
    let client = crate::net::client(crate::net::RequestCategory::Download);
    let dest = std::env::temp_dir().join(format!("blink-dl-test-cancel-{}.bin", std::process::id()));
    let token = CancellationToken::new();
    let t = token.clone();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(150)).await;
        t.cancel();
    });
    let err = stream_to_file(
        &client,
        &server.url(),
        &dest,
        &limits(30, None),
        Some(&token),
        |_, _| {},
        |_: &[u8]| {},
    )
    .await
    .expect_err("cancelled download should fail");
    assert!(matches!(err, AttemptError::Cancelled), "取消立即终止，实际 {err:?}");
    assert!(!dest.exists(), "取消清理暂存文件");
}

#[tokio::test]
async fn http_error_status_is_retryable() {
    // 直接用不可达端口模拟连接失败 → Retryable（换源语义）
    let client = crate::net::client(crate::net::RequestCategory::Download);
    let dest = std::env::temp_dir().join(format!("blink-dl-test-conn-{}.bin", std::process::id()));
    let err = stream_to_file(
        &client,
        "http://127.0.0.1:1/file.bin",
        &dest,
        &limits(2, None),
        None,
        |_, _| {},
        |_: &[u8]| {},
    )
    .await
    .expect_err("unreachable host should fail");
    assert!(matches!(err, AttemptError::Retryable { .. }), "连接失败可换源，实际 {err:?}");
}
