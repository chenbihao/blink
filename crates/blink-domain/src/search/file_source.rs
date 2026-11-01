//! 用户 Everything 的连接状态：按需探测、失败冷却，查询不等待在途探测。
use std::future::Future;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);

pub(super) fn http_client() -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(Duration::from_millis(150))
        .timeout(Duration::from_millis(800))
        .build()
        .expect("Everything HTTP client")
}

struct State {
    port: u16,
    revision: u64,
    available: Option<bool>,
    checked_at: Option<Instant>,
}

#[derive(Clone)]
pub(super) struct UserEverything {
    state: Arc<RwLock<State>>,
    gate: Arc<Mutex<()>>,
    client: reqwest::Client,
}

impl UserEverything {
    pub fn new(port: u16) -> Self {
        Self {
            state: Arc::new(RwLock::new(State {
                port,
                revision: 0,
                available: None,
                checked_at: None,
            })),
            gate: Arc::new(Mutex::new(())),
            client: http_client(),
        }
    }

    pub async fn update_port(&self, port: u16) {
        let mut state = self.state.write().await;
        if state.port != port {
            state.port = port;
            state.revision += 1;
            state.available = None;
            state.checked_at = None;
        }
    }

    pub async fn snapshot(&self) -> (u16, Option<bool>, bool) {
        let state = self.state.read().await;
        // gate 的生命周期同时覆盖探测/恢复与取消，不留下陈旧的 probing 标记。
        (state.port, state.available, self.gate.try_lock().is_err())
    }

    /// 正常查询直接更新连接状态；未知/失败来源只允许一个恢复请求在途。
    /// permit 随 future 丢弃而释放，取消搜索不会阻止后续恢复。
    pub async fn query<T>(&self, port: u16, query: impl Future<Output = Option<T>>) -> Option<T> {
        let (revision, _permit) = {
            let mut state = self.state.write().await;
            if state.port != port
                || (state.available == Some(false)
                    && state.checked_at.is_some_and(|at| at.elapsed() < FAILURE_COOLDOWN))
            {
                return None;
            }
            let permit = if state.available != Some(true) {
                Some(self.gate.try_lock().ok()?)
            } else {
                None
            };
            state.revision += 1;
            (state.revision, permit)
        };
        let result = query.await;
        let mut state = self.state.write().await;
        // 按请求开始顺序提交，旧端口/较早查询的迟到结果不能覆盖新状态。
        if state.revision == revision {
            let available = result.is_some();
            if state.available != Some(available) {
                tracing::debug!(port, available, "用户 Everything 查询连接状态已更新");
            }
            state.available = Some(available);
            state.checked_at = Some(Instant::now());
        }
        result
    }

    pub fn request_probe(&self) {
        let source = self.clone();
        tokio::spawn(async move {
            source.refresh(true).await;
        });
    }

    pub async fn refresh(&self, force: bool) {
        let previous_check = self.state.read().await.checked_at;
        let _permit = if force {
            self.gate.lock().await
        } else {
            match self.gate.try_lock() {
                Ok(permit) => permit,
                Err(_) => return,
            }
        };
        let (port, revision) = {
            let mut state = self.state.write().await;
            // 等待同一在途探测/恢复查询的显式请求，共用它刚提交的结果。
            if state.checked_at.is_some() && state.checked_at != previous_check {
                return;
            }
            if !force
                && state
                    .checked_at
                    .is_some_and(|at| at.elapsed() < FAILURE_COOLDOWN)
            {
                return;
            }
            state.revision += 1;
            (state.port, state.revision)
        };
        let start = Instant::now();
        let url = format!("http://127.0.0.1:{port}/?search=__blink_probe__&json=1&count=0");
        let available = match self.client.get(url).send().await {
            Ok(response) if response.status().is_success() => response
                .json::<serde_json::Value>()
                .await
                .is_ok_and(|value| {
                    value["results"].is_array() && value["totalResults"].is_number()
                }),
            _ => false,
        };
        let mut state = self.state.write().await;
        if state.revision == revision {
            state.available = Some(available);
            state.checked_at = Some(Instant::now());
            tracing::debug!(
                port,
                available,
                elapsed_ms = start.elapsed().as_millis(),
                "用户 Everything 后台探测完成"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn port_change_invalidates_old_availability() {
        let source = UserEverything::new(80);
        source.query(80, async { Some(()) }).await.unwrap();
        let stale = source.query(80, async {
            source.update_port(81).await;
            source.update_port(80).await;
            Some(())
        });
        stale.await.unwrap();
        assert_eq!(source.snapshot().await, (80, None, false));
        source.update_port(81).await;
        assert!(source.query(80, async { Some(()) }).await.is_none());
        assert_eq!(source.snapshot().await, (81, None, false));
    }

    #[tokio::test]
    async fn failed_source_cools_down_then_returns_recovered_query_results() {
        let source = UserEverything::new(80);
        assert!(source.query::<()>(80, async { None }).await.is_none());
        assert_eq!(source.snapshot().await.1, Some(false));
        assert!(source.query::<()>(80, async { panic!("冷却期不得发送请求") }).await.is_none());
        source.state.write().await.checked_at = Some(Instant::now() - FAILURE_COOLDOWN);
        let result = source.query(80, async { Some("recovered.txt") }).await;
        assert_eq!(result, Some("recovered.txt"));
        assert_eq!(source.snapshot().await.1, Some(true));
    }

    #[tokio::test]
    async fn recovery_is_single_flight_and_cancellation_releases_it() {
        let source = UserEverything::new(80);
        let mut recovery = Box::pin(source.query::<()>(80, std::future::pending()));
        assert!(futures::poll!(&mut recovery).is_pending());
        assert!(source.query::<()>(80, async { panic!("恢复请求不可重复发送") }).await.is_none());
        drop(recovery);
        assert_eq!(source.query(80, async { Some("next query") }).await, Some("next query"));
    }

    #[tokio::test]
    async fn late_failure_does_not_override_a_newer_success() {
        let source = UserEverything::new(80);
        source.query(80, async { Some(()) }).await.unwrap();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut older = Box::pin(source.query::<()>(80, async {
            release_rx.await.unwrap();
            None
        }));
        assert!(futures::poll!(&mut older).is_pending());
        source.query(80, async { Some(()) }).await.unwrap();
        release_tx.send(()).unwrap();
        assert!(older.await.is_none());
        assert_eq!(source.snapshot().await.1, Some(true));
    }

    #[tokio::test]
    async fn explicit_refresh_reuses_an_inflight_recovery_result() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let source = UserEverything::new(port);
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let mut recovery = Box::pin(source.query(port, async {
            release_rx.await.unwrap();
            Some(())
        }));
        assert!(futures::poll!(&mut recovery).is_pending());
        assert!(source.snapshot().await.2);
        let mut refresh = Box::pin(source.refresh(true));
        assert!(futures::poll!(&mut refresh).is_pending());
        release_tx.send(()).unwrap();
        recovery.await.unwrap();
        // 若错误地重复探测，无响应的本地 listener 会使此处超时。
        tokio::time::timeout(Duration::from_millis(200), refresh).await.unwrap();
        assert_eq!(source.snapshot().await, (port, Some(true), false));
    }

    #[tokio::test]
    async fn explicit_refresh_bypasses_failure_cooldown() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let source = UserEverything::new(port);
        assert!(source.query::<()>(port, async { None }).await.is_none());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = [0; 2048];
            let len = stream.read(&mut request).await.unwrap();
            assert!(String::from_utf8_lossy(&request[..len]).contains("__blink_probe__"));
            let body = r#"{"totalResults":0,"results":[]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        tokio::time::timeout(Duration::from_secs(2), source.refresh(true)).await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), server).await.unwrap().unwrap();
        assert_eq!(source.snapshot().await, (port, Some(true), false));
    }
}
