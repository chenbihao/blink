//! 用户 Everything 的连接状态：探测单飞、后台刷新，查询不等待探测。
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

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
    probing: bool,
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
                probing: false,
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
            state.probing = false;
        }
    }

    pub async fn snapshot(&self) -> (u16, Option<bool>, bool) {
        let state = self.state.read().await;
        (state.port, state.available, state.probing)
    }

    pub async fn record_query(&self, port: u16, available: bool) {
        let mut state = self.state.write().await;
        if state.port == port {
            state.revision += 1;
            state.available = Some(available);
            state.checked_at = Some(Instant::now());
            state.probing = false;
        }
    }

    pub fn request_probe(&self) {
        let source = self.clone();
        tokio::spawn(async move {
            source.refresh(false).await;
        });
    }

    pub async fn refresh(&self, force: bool) {
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
            if !force
                && state
                    .checked_at
                    .is_some_and(|at| at.elapsed() < Duration::from_secs(30))
            {
                return;
            }
            state.probing = true;
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
            state.probing = false;
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
        source.record_query(80, true).await;
        source.update_port(81).await;
        source.record_query(80, true).await;
        assert_eq!(source.snapshot().await, (81, None, false));
    }
}
