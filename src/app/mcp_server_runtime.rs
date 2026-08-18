//! MCP Server Runtime（0.19.13）——主进程 Streamable HTTP MCP Server 生命周期管理。
//!
//! ## 职责
//!
//! - 在 `127.0.0.1:{port}/mcp` 上启动/停止 Streamable HTTP listener
//! - 串行化所有 start/stop/restart/apply 操作，防止竞态产生多个 listener
//! - 热更新暴露清单（不重启 listener，通过 `SharedExposure::rebuild()` + generation 通知）
//! - 提供只读运行时状态快照（供设置页展示）
//! - 应用退出时优雅关闭 listener
//!
//! ## 架构
//!
//! ```text
//! McpServerRuntime
//!   ├── SharedExposure (Arc) ─── 所有 HTTP session 共享
//!   │     ├── ExposureSnapshot (RwLock)
//!   │     └── generation watch::Sender ──→ on_initialized 通知
//!   ├── CapabilityRegistry (Arc) ─── BlinkMcpServer 数据源
//!   ├── DomainEnv (Arc) ─── BlinkMcpServer 桥接
//!   ├── ai_pool (SqlitePool) ─── 审计日志
//!   └── Mutex<RuntimeInner>
//!         ├── status: Disabled / Starting / Listening / Error
//!         ├── port: u16
//!         ├── cancel_token: CancellationToken ──→ listener task
//!         └── listener_handle: JoinHandle
//! ```
//!
//! ## 串行化
//!
//! 所有操作（`apply_config` / `stop` / `shutdown`）都通过 `Mutex<RuntimeInner>` 串行化。
//! 同一时刻只有一个操作在执行，不会出现"旧 stop 覆盖新 start"的竞态。
//!
//! ## 热更新 vs 重启
//!
//! - **暴露清单变更**（`exposed_capabilities` 改了）：调用 `SharedExposure::rebuild()`，
//!   递增 generation，所有活跃 session 的 `on_initialized` 回调收到通知并向 client 发
//!   `notifications/tools/list_changed`。**不重启 listener**。
//! - **端口变更**：先 stop 旧 listener（cancel + wait），再 start 新 listener。
//! - **启停**：start 绑定 `TcpListener` + spawn accept loop；stop cancel token + wait join。

use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tokio_util::sync::CancellationToken;

use crate::domain::capability::CapabilityRegistry;
use crate::domain::event::{CapabilityEnv, EventPort};
use crate::domain::mcp::server::{BlinkMcpServer, SharedExposure};
use crate::domain::mcp::server_config::{DEFAULT_MCP_SERVER_PORT, McpServerModeConfig};

// ── 状态枚举 ─────────────────────────────────────────────────────────────────

/// MCP Server 运行时状态。
///
/// 设置页据此展示状态指示器和错误信息。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    /// 未启用（`enabled = false`）。
    Disabled,
    /// 正在启动（绑定端口中）。
    Starting,
    /// 正在监听，可接受外部 MCP client 连接。
    Listening,
    /// 出错（端口占用等），`error` 字段包含原因。
    Error,
}

/// 运行时只读快照——供设置页展示。
///
/// 状态查询只读，不隐式启动 listener。
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpServerRuntimeSnapshot {
    /// 当前状态。
    pub status: McpServerStatus,
    /// 实际 endpoint（仅 `listening` 时有值）。
    pub endpoint: Option<String>,
    /// 当前配置端口。
    pub port: u16,
    /// 当前暴露的 tool 数量。
    pub tool_count: usize,
    /// 最近错误（仅 `error` 状态有值）。
    pub error: Option<String>,
}

// ── 内部可变状态 ─────────────────────────────────────────────────────────────

/// runtime 内部可变状态——通过 `Mutex` 保护，所有操作串行化。
struct RuntimeInner {
    /// 当前状态。
    status: McpServerStatus,
    /// 当前监听端口。
    port: u16,
    /// 最近错误。
    error: Option<String>,
    /// 当前 listener 的取消令牌（stop 时 cancel）。
    cancel_token: Option<CancellationToken>,
    /// 当前 listener task 的 JoinHandle。
    listener_handle: Option<tokio::task::JoinHandle<()>>,
}

impl Default for RuntimeInner {
    fn default() -> Self {
        Self {
            status: McpServerStatus::Disabled,
            port: DEFAULT_MCP_SERVER_PORT,
            error: None,
            cancel_token: None,
            listener_handle: None,
        }
    }
}

// ── McpServerRuntime ─────────────────────────────────────────────────────────

/// MCP Server Runtime——主进程唯一的 MCP HTTP Server 生命周期管理器。
///
/// 在 `main.rs` setup 中构造，注册为 Tauri managed state。
/// CapabilityRegistry、PluginEngine、SearchService 等 service 注入完成后，
/// 按配置启动 listener（如果 `enabled = true`）。
///
/// **线程安全**：所有操作通过内部 `Mutex` 串行化，可安全跨线程调用。
pub struct McpServerRuntime {
    /// 共享暴露快照——与所有 HTTP session 共享。
    exposure: Arc<SharedExposure>,
    /// CapabilityRegistry——构造 BlinkMcpServer 用。
    cap_registry: Arc<CapabilityRegistry>,
    /// 领域环境——构造 BlinkMcpServer 用。
    cap_env: Arc<dyn CapabilityEnv>,
    /// 事件发射 port——构造 BlinkMcpServer 用。
    event_port: Arc<dyn EventPort>,
    /// AI 库连接池——审计日志写入。
    ai_pool: sqlx::SqlitePool,
    /// 内部可变状态（串行化所有操作）。
    inner: Mutex<RuntimeInner>,
}

impl McpServerRuntime {
    /// 构造 runtime（不启动 listener）。
    ///
    /// 构造后需调用 `apply_config()` 才会按配置启动 listener。
    pub fn new(
        cap_registry: Arc<CapabilityRegistry>,
        cap_env: Arc<dyn CapabilityEnv>,
        event_port: Arc<dyn EventPort>,
        ai_pool: sqlx::SqlitePool,
    ) -> Self {
        Self {
            exposure: Arc::new(SharedExposure::new()),
            cap_registry,
            cap_env,
            event_port,
            ai_pool,
            inner: Mutex::new(RuntimeInner::default()),
        }
    }

    /// 返回共享暴露快照的 Arc 引用。
    ///
    /// 供外部（如 IPC command）调用 `rebuild()` 热更新暴露清单。
    #[allow(dead_code)]
    pub fn exposure(&self) -> Arc<SharedExposure> {
        self.exposure.clone()
    }

    /// 应用配置变更——串行化 start/stop/restart/rebuild。
    ///
    /// 这是设置页保存配置后的统一入口：
    /// - `enabled=false` → stop listener，状态置 `Disabled`
    /// - `enabled=true` + 端口变了 → stop 旧 listener + start 新 listener
    /// - `enabled=true` + 端口没变 → 只 rebuild exposure（热更新，不重启 listener）
    ///
    /// 无论是否需要重启 listener，都会先 rebuild exposure（确保 tool 列表最新）。
    pub async fn apply_config(&self, config: &McpServerModeConfig) {
        let mut inner = self.inner.lock().await;

        // 先重建暴露快照（无论是否需要重启 listener，tool 列表都要更新）
        self.exposure.rebuild(&self.cap_registry, config).await;

        if !config.enabled {
            // 停止 listener（如果正在运行）
            if inner.status == McpServerStatus::Listening
                || inner.status == McpServerStatus::Starting
            {
                tracing::info!("MCP server: 配置已禁用，停止 listener");
                Self::stop_inner(&mut inner).await;
            }
            inner.status = McpServerStatus::Disabled;
            inner.error = None;
            return;
        }

        // enabled = true
        let port_changed = inner.port != config.port;
        let was_listening = inner.status == McpServerStatus::Listening;

        if was_listening && !port_changed {
            // 只更新暴露清单，不重启 listener
            tracing::info!("MCP server: 暴露清单已热更新，listener 不重启");
            return;
        }

        // 需要重启（端口变了或之前未监听）
        if was_listening {
            tracing::info!(
                old_port = inner.port,
                new_port = config.port,
                "MCP server: 端口变更，重启 listener"
            );
            Self::stop_inner(&mut inner).await;
        }

        // 启动新 listener
        inner.port = config.port;
        inner.status = McpServerStatus::Starting;
        inner.error = None;

        match self.start_listener_locked(&mut inner, config.port).await {
            Ok(()) => {
                inner.status = McpServerStatus::Listening;
                tracing::info!(port = config.port, "MCP server: listener 已启动");
            }
            Err(e) => {
                inner.status = McpServerStatus::Error;
                inner.error = Some(e.clone());
                tracing::error!(port = config.port, error = %e, "MCP server: listener 启动失败");
            }
        }
    }

    /// 停止 listener（如果正在运行）。
    ///
    /// 状态置为 `Disabled`。
    #[allow(dead_code)]
    pub async fn stop(&self) {
        let mut inner = self.inner.lock().await;
        Self::stop_inner(&mut inner).await;
        inner.status = McpServerStatus::Disabled;
        inner.error = None;
    }

    /// 关闭 runtime（应用退出时调用）。
    ///
    /// 取消 listener 并等待其结束（带 3 秒超时兜底）。
    pub async fn shutdown(&self) {
        let mut inner = self.inner.lock().await;
        Self::stop_inner(&mut inner).await;
        inner.status = McpServerStatus::Disabled;
        inner.error = None;
        tracing::info!("MCP server: runtime 已关闭");
    }

    /// 获取只读运行时快照。
    ///
    /// 状态查询只读，不隐式启动 listener。
    pub async fn snapshot(&self) -> McpServerRuntimeSnapshot {
        let inner = self.inner.lock().await;
        let tool_count = {
            let snapshot = self.exposure.read().await;
            snapshot.tools.len()
        };
        let endpoint = if inner.status == McpServerStatus::Listening {
            Some(format!("http://127.0.0.1:{}/mcp", inner.port))
        } else {
            None
        };
        McpServerRuntimeSnapshot {
            status: inner.status.clone(),
            endpoint,
            port: inner.port,
            tool_count,
            error: inner.error.clone(),
        }
    }

    // ── 内部方法 ───────────────────────────────────────────────────────────

    /// 内部停止逻辑——取消 cancellation token 并等待 listener task 结束。
    ///
    /// 调用时已持有 `inner` 锁。
    async fn stop_inner(inner: &mut RuntimeInner) {
        if let Some(token) = inner.cancel_token.take() {
            token.cancel();
        }
        if let Some(handle) = inner.listener_handle.take() {
            // 等待 listener task 结束（带超时兜底，防止 hang 住退出）
            match tokio::time::timeout(Duration::from_secs(3), handle).await {
                Ok(Ok(())) => {
                    tracing::debug!("MCP server: listener task 已正常退出");
                }
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "MCP server: listener task 异常退出");
                }
                Err(_) => {
                    tracing::warn!("MCP server: listener task 3 秒内未退出，放弃等待");
                }
            }
        }
    }

    /// 启动 HTTP listener（调用时已持有 `inner` 锁）。
    ///
    /// 流程：
    /// 1. 绑定 `TcpListener` 到 `127.0.0.1:{port}`
    /// 2. 构造 `StreamableHttpService`（rmcp）+ `LocalSessionManager`
    /// 3. 用 `TowerToHyperService` 适配 Tower → hyper
    /// 4. spawn accept loop（带 cancellation）
    async fn start_listener_locked(
        &self,
        inner: &mut RuntimeInner,
        port: u16,
    ) -> Result<(), String> {
        // 1. 绑定 TCP listener（显式 IPv4 loopback）
        let listener = tokio::net::TcpListener::bind(format!("127.0.0.1:{port}"))
            .await
            .map_err(|e| format!("端口 {port} 绑定失败: {e}"))?;

        tracing::info!(port, "MCP server: TCP listener 已绑定");

        // 2. 创建 cancellation token（listener task + rmcp config 共享）
        let cancel_token = CancellationToken::new();
        let server_cancel = cancel_token.clone();

        // 3. 构造 StreamableHttpService
        let cap_registry = self.cap_registry.clone();
        let cap_env = self.cap_env.clone();
        let event_port = self.event_port.clone();
        let ai_pool = self.ai_pool.clone();
        let exposure = self.exposure.clone();

        // service_factory：每个新 session 调用一次，创建独立的 BlinkMcpServer 实例
        let service_factory = move || {
            let server = BlinkMcpServer::new(
                cap_registry.clone(),
                cap_env.clone(),
                event_port.clone(),
                ai_pool.clone(),
                exposure.clone(),
            );
            Ok(server)
        };

        let session_manager = Arc::new(
            rmcp::transport::streamable_http_server::session::local::LocalSessionManager::default(),
        );

        // rmcp Streamable HTTP server 配置
        // StreamableHttpServerConfig 是 #[non_exhaustive]，必须用 Default + builder 方法
        let config = rmcp::transport::streamable_http_server::StreamableHttpServerConfig::default()
            .with_sse_keep_alive(Some(Duration::from_secs(15)))
            .with_sse_retry(Some(Duration::from_secs(3)))
            .with_stateful_mode(true)
            .with_cancellation_token(server_cancel.child_token())
            // 只允许 loopback host，防 DNS rebinding
            .with_allowed_hosts(["127.0.0.1", "localhost"])
            // Origin 校验：只接受 loopback 来源
            // 空 Origin（非浏览器 client）不校验
            .with_allowed_origins(["http://127.0.0.1", "http://localhost"]);

        let service = rmcp::transport::streamable_http_server::StreamableHttpService::new(
            service_factory,
            session_manager,
            config,
        );

        // 4. 适配 Tower Service → hyper Service
        let hyper_service = hyper_util::service::TowerToHyperService::new(service);

        // 5. spawn accept loop
        let handle = tokio::spawn(async move {
            let builder = hyper_util::server::conn::auto::Builder::new(
                hyper_util::rt::TokioExecutor::default(),
            );

            loop {
                // 同时等待新连接和取消信号
                let (conn, _addr) = tokio::select! {
                    result = listener.accept() => {
                        match result {
                            Ok(conn) => conn,
                            Err(e) => {
                                tracing::error!(error = %e, "MCP server: accept 失败，listener 退出");
                                break;
                            }
                        }
                    }
                    _ = server_cancel.cancelled() => {
                        tracing::info!("MCP server: listener 收到取消信号，停止接受新连接");
                        break;
                    }
                };

                // 每个连接 spawn 一个独立 task
                let io = hyper_util::rt::TokioIo::new(conn);
                let svc = hyper_service.clone();
                let cancel = server_cancel.clone();
                let conn_builder = builder.clone();

                tokio::spawn(async move {
                    let serve = conn_builder.serve_connection(io, svc);
                    // 连接级取消：server 关闭时中断长连接 SSE 流
                    tokio::select! {
                        result = serve => {
                            if let Err(e) = result {
                                tracing::debug!(error = %e, "MCP server: 连接结束（含正常关闭）");
                            }
                        }
                        _ = cancel.cancelled() => {
                            // server 关闭中，连接被丢弃
                        }
                    }
                });
            }

            tracing::debug!("MCP server: accept loop 已退出");
        });

        inner.cancel_token = Some(cancel_token);
        inner.listener_handle = Some(handle);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::event::CapabilityEnv;

    /// 验证 runtime 初始状态为 Disabled。
    #[tokio::test]
    async fn runtime_starts_disabled() {
        let runtime = create_test_runtime().await;
        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.status, McpServerStatus::Disabled);
        assert_eq!(snapshot.port, DEFAULT_MCP_SERVER_PORT);
        assert!(snapshot.endpoint.is_none());
        assert_eq!(snapshot.tool_count, 0);
        assert!(snapshot.error.is_none());
    }

    /// 验证 apply_config(disabled) 不启动 listener。
    #[tokio::test]
    async fn apply_disabled_config_stays_disabled() {
        let runtime = create_test_runtime().await;
        let config = McpServerModeConfig {
            enabled: false,
            port: DEFAULT_MCP_SERVER_PORT,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config).await;
        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.status, McpServerStatus::Disabled);
    }

    /// 验证 apply_config(enabled) 尝试绑定端口。
    /// 使用高端口避免冲突。
    #[tokio::test]
    async fn apply_enabled_starts_listening() {
        let runtime = create_test_runtime().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 43210,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config).await;
        let snapshot = runtime.snapshot().await;
        // 可能成功（Listening）或失败（Error，端口被占用）
        assert!(
            snapshot.status == McpServerStatus::Listening
                || snapshot.status == McpServerStatus::Error,
            "预期 Listening 或 Error，实际 {:?}",
            snapshot.status
        );
        if snapshot.status == McpServerStatus::Listening {
            assert_eq!(snapshot.endpoint, Some("http://127.0.0.1:43210/mcp".into()));
        }
        // 清理
        runtime.stop().await;
    }

    /// 验证 stop 后状态变 Disabled。
    #[tokio::test]
    async fn stop_sets_disabled() {
        let runtime = create_test_runtime().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 43211,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config).await;
        runtime.stop().await;
        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.status, McpServerStatus::Disabled);
    }

    /// 验证 shutdown 后状态清理。
    #[tokio::test]
    async fn shutdown_cleans_up() {
        let runtime = create_test_runtime().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 43212,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config).await;
        runtime.shutdown().await;
        // shutdown 后再 snapshot 应该不 panic
        let snapshot = runtime.snapshot().await;
        assert_ne!(snapshot.status, McpServerStatus::Listening);
    }

    /// 验证端口冲突时进入 Error 状态。
    #[tokio::test]
    async fn port_conflict_sets_error() {
        // 先占住端口
        let _guard = tokio::net::TcpListener::bind("127.0.0.1:43213")
            .await
            .expect("bind failed");

        let runtime = create_test_runtime().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 43213,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config).await;
        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.status, McpServerStatus::Error);
        assert!(snapshot.error.is_some());
        assert!(snapshot.error.as_ref().unwrap().contains("43213"));
    }

    /// 验证热更新暴露清单不重启 listener。
    #[tokio::test]
    async fn hot_update_exposure_no_restart() {
        let runtime = create_test_runtime().await;
        let config1 = McpServerModeConfig {
            enabled: true,
            port: 43214,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config1).await;

        // 同端口、改暴露清单——不应重启 listener
        let config2 = McpServerModeConfig {
            enabled: true,
            port: 43214,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config2).await;

        let snapshot = runtime.snapshot().await;
        // 应该还是 Listening（没有重启）
        assert_eq!(snapshot.status, McpServerStatus::Listening);

        runtime.stop().await;
    }

    /// 验证改端口触发重启。
    #[tokio::test]
    async fn port_change_triggers_restart() {
        let runtime = create_test_runtime().await;
        let config1 = McpServerModeConfig {
            enabled: true,
            port: 43215,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config1).await;
        assert_eq!(runtime.snapshot().await.status, McpServerStatus::Listening);

        // 改端口
        let config2 = McpServerModeConfig {
            enabled: true,
            port: 43216,
            exposed_capabilities: vec![],
            exposure_seeded: false,
        };
        runtime.apply_config(&config2).await;
        let snapshot = runtime.snapshot().await;
        assert_eq!(snapshot.status, McpServerStatus::Listening);
        assert_eq!(snapshot.port, 43216);

        runtime.stop().await;
    }

    // ── 端到端热更新（真实 rmcp client 连接）──────────────────────────────

    /// 测试用 mock Capability（默认 policy = Safe + 允许全部来源，含 MCP）。
    struct MockCap(&'static str);

    #[async_trait::async_trait]
    impl crate::domain::capability::Capability for MockCap {
        fn id(&self) -> &str {
            self.0
        }
        fn schema(&self) -> crate::domain::capability::CapabilitySchema {
            crate::domain::capability::CapabilitySchema::empty(self.0, "mock for e2e")
        }
        async fn invoke(
            &self,
            _args: serde_json::Value,
            _ctx: &crate::domain::capability::InvokeContext<'_>,
        ) -> Result<crate::domain::capability::CapabilityResult, crate::domain::capability::CapabilityError>
        {
            Ok(crate::domain::capability::CapabilityResult::Done {
                summary: "mock".into(),
            })
        }
    }

    /// 端到端验证：暴露清单变化后，已连接的 MCP client `tools/list` 返回新工具数。
    ///
    /// 覆盖用户观察到的"切换 MCP 暴露后重新请求工具数量不变"——服务端快照必须实时反映变化。
    #[tokio::test]
    async fn hot_update_reflected_in_live_client() {
        let registry = Arc::new(CapabilityRegistry::new());
        registry
            .register(Arc::new(MockCap("cap_a")) as Arc<dyn crate::domain::capability::Capability>)
            .unwrap();
        registry
            .register(Arc::new(MockCap("cap_b")) as Arc<dyn crate::domain::capability::Capability>)
            .unwrap();

        let runtime = create_test_runtime_with_registry(registry).await;
        let port = 43220;
        runtime
            .apply_config(&McpServerModeConfig {
                enabled: true,
                port,
                exposed_capabilities: vec!["cap_a".into()],
                exposure_seeded: true,
            })
            .await;
        assert_eq!(runtime.snapshot().await.status, McpServerStatus::Listening);

        // 连接真实 rmcp client（Streamable HTTP）
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        let transport = rmcp::transport::StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!(
                "http://127.0.0.1:{port}/mcp"
            )),
        );
        let client_info = rmcp::model::ClientInfo::default();
        use rmcp::ServiceExt;
        let service = client_info
            .serve(transport)
            .await
            .expect("MCP 握手失败");

        let tools_1 = service.peer().list_all_tools().await.expect("拉取工具失败");
        assert_eq!(tools_1.len(), 1, "初始应只有 cap_a");
        assert_eq!(tools_1[0].name, "cap_a");

        // 热更新暴露清单：新增 cap_b（同端口，listener 不重启）
        runtime
            .apply_config(&McpServerModeConfig {
                enabled: true,
                port,
                exposed_capabilities: vec!["cap_a".into(), "cap_b".into()],
                exposure_seeded: true,
            })
            .await;

        let tools_2 = service
            .peer()
            .list_all_tools()
            .await
            .expect("重新拉取工具失败");
        assert_eq!(tools_2.len(), 2, "热更新后应暴露 cap_a + cap_b");
        let names: Vec<String> = tools_2.iter().map(|t| t.name.to_string()).collect();
        assert!(names.iter().any(|n| n == "cap_a"));
        assert!(names.iter().any(|n| n == "cap_b"));

        // 再撤回 cap_b——工具数应回落到 1
        runtime
            .apply_config(&McpServerModeConfig {
                enabled: true,
                port,
                exposed_capabilities: vec!["cap_a".into()],
                exposure_seeded: true,
            })
            .await;
        let tools_3 = service.peer().list_all_tools().await.expect("再次拉取失败");
        assert_eq!(tools_3.len(), 1, "撤回后应只剩 cap_a");

        runtime.stop().await;
    }

    // ── 测试辅助 ───────────────────────────────────────────────────────────

    /// 创建测试用 runtime（最小 fake env）。
    async fn create_test_runtime() -> McpServerRuntime {
        create_test_runtime_with_registry(Arc::new(CapabilityRegistry::new())).await
    }

    /// 创建测试用 runtime，可注入预注册能力的 registry（供端到端热更新测试用）。
    async fn create_test_runtime_with_registry(
        cap_registry: Arc<CapabilityRegistry>,
    ) -> McpServerRuntime {
        // 使用 in-memory SQLite pool
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");


        // 最小 fake DomainEnv——不注入任何 service
        struct FakeEnv;
        #[async_trait::async_trait]
        impl CapabilityEnv for FakeEnv {
            fn db_pools(&self) -> &crate::infra::data::DbPools {
                unimplemented!("not needed for runtime tests")
            }
            fn plugin_engine(&self) -> Option<&Arc<crate::domain::plugin::PluginEngine>> {
                None
            }
            fn search_service(&self) -> Option<&Arc<crate::domain::search::SearchService>> {
                None
            }
            async fn list_managed_settings(
                &self,
            ) -> Result<Vec<crate::domain::config::ManagedSetting>, String> {
                Ok(vec![])
            }
            async fn update_managed_setting(
                &self,
                _setting_id: &str,
                _expected_old_value: serde_json::Value,
                _new_value: serde_json::Value,
            ) -> Result<crate::domain::config::ManagedSettingUpdate, String> {
                Err("not implemented".into())
            }
            fn sticky_service(&self) -> Option<&Arc<crate::domain::sticky::StickyService>> {
                None
            }
            async fn create_sticky_and_notify(
                &self,
                _content: &str,
                _color: crate::domain::sticky::StickyColor,
            ) -> Result<crate::domain::sticky::StickyNote, crate::domain::sticky::StickyWorkflowError>
            {
                unimplemented!("not needed for runtime tests")
            }
            async fn create_sticky_and_show(
                &self,
                _content: &str,
                _x: Option<i32>,
                _y: Option<i32>,
                _w: Option<i32>,
                _h: Option<i32>,
            ) -> Result<String, String> {
                Err("not implemented".into())
            }
            async fn update_sticky_content_and_notify(
                &self,
                _sticky_id: &str,
                _content: &str,
                _expected_updated_at: Option<i64>,
                _source: crate::domain::sticky::StickyChangeSource,
            ) -> Result<i64, crate::domain::sticky::StickyWorkflowError> {
                unimplemented!("not needed for runtime tests")
            }
            async fn set_sticky_visibility_and_notify(
                &self,
                _sticky_id: &str,
                _visible: bool,
            ) -> Result<i64, crate::domain::sticky::StickyWorkflowError> {
                unimplemented!("not needed for runtime tests")
            }
            async fn trash_sticky_and_notify(
                &self,
                _sticky_id: &str,
            ) -> Result<(), crate::domain::sticky::StickyWorkflowError> {
                unimplemented!("not needed for runtime tests")
            }
            async fn close_sticky_and_notify(
                &self,
                _sticky_id: &str,
                _final_content: &str,
                _expected_updated_at: Option<i64>,
            ) -> Result<
                crate::domain::sticky::StickyCloseOutcome,
                crate::domain::sticky::StickyWorkflowError,
            > {
                unimplemented!("not needed for runtime tests")
            }
            fn image_stash(&self) -> Option<&Arc<crate::domain::capability::ImageStash>> {
                None
            }
            fn show_pin_image(
                &self,
                _png_bytes: Vec<u8>,
                _x: Option<i32>,
                _y: Option<i32>,
            ) -> Result<(i32, i32), String> {
                Err("not implemented".into())
            }
        }
        impl EventPort for FakeEnv {
            fn emit(&self, _event: &str, _payload: serde_json::Value) -> Result<(), String> {
                Ok(())
            }
            fn emit_to(
                &self,
                _target: &str,
                _event: &str,
                _payload: serde_json::Value,
            ) -> Result<(), String> {
                Ok(())
            }
        }

        let cap_env: Arc<dyn CapabilityEnv> = Arc::new(FakeEnv);
        let event_port: Arc<dyn EventPort> = Arc::new(FakeEnv);
        McpServerRuntime::new(cap_registry, cap_env, event_port, pool)
    }
}
