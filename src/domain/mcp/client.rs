//! MCP client 编排（0.13.0）——拉起外部 server 子进程 + 握手 + 拉 tool 列表。
//!
//! ## 架构
//!
//! `McpClientManager` 持有所有已连接的 MCP server。每个 server 是一个 stdio 子进程，
//! 通过 `rmcp::ServiceExt::serve()` 建立连接，`peer().list_all_tools()` 拉 tool 列表。
//!
//! ## 生命周期（lazy connect）
//!
//! - **不在 Blink 启动时自动拉起**——避免 npx 下载等慢操作拖慢启动
//! - `ensure_connected()` 在对话窗口首次需要 tool 时 lazy 连接所有 enabled server
//! - 手动「测试连接」/「连接」/「断开」按钮供用户在设置页控制
//!
//! ## 故障降级
//!
//! - server 连接失败 → 重试 1 次（间隔 1s），仍失败则跳过，不阻塞其他 server
//! - 确定性错误（空 command / URL 格式错误）→ 不重试，立即返回
//! - server 崩溃 → 标灰，从 tool 池剔除，UI 提示
//! - 手动重连：`reconnect_server()` 重新拉起子进程
//!
//! ## tool 可见性
//!
//! `collect_tools()` 时过滤 `config.disabled_tools` 中的 tool，只把用户启用的喂给 AI。

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use rig_core::tool::ToolDyn;
use rmcp::ServiceExt;
use rmcp::model::ClientInfo;
use rmcp::service::RunningService;
use tokio::sync::RwLock;

use crate::domain::mcp::config::{McpServerConfig, McpServerConfigStore};

/// 单个 MCP server 的连接状态。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum McpServerStatus {
    /// 已连接，提供 N 个 tool。
    Online { tool_count: usize },
    /// 离线（启动失败 / 崩溃 / 手动停止）。
    Offline { reason: String },
    /// 正在连接中。
    Connecting,
}

/// 单个 MCP server 的 tool 信息（供前端预览）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpToolInfo {
    /// tool 名称（MCP server 提供的）。
    pub name: String,
    /// tool 描述。
    pub description: String,
    /// 是否被用户禁用。
    pub disabled: bool,
}

/// 单个已连接 server 的运行时状态。
struct ConnectedServer {
    /// rmcp 运行时服务（持有子进程 + 连接）。
    /// 类型为 `RunningService<RoleClient, ClientInfo>`——`ClientInfo` 是 rmcp 提供的默认 handler。
    service: RunningService<rmcp::service::RoleClient, ClientInfo>,
    /// 该 server 提供的原始 rmcp tool 列表（用于构造 `McpTool`，避免每次 collect 重新拉取）。
    rmcp_tools: Vec<rmcp::model::Tool>,
    /// 该 server 提供的 tool 信息（含描述 + disabled 标记，供前端预览）。
    tools: Vec<McpToolInfo>,
    /// server 配置（含 disabled_tools 等信息）。
    config: McpServerConfig,
}

/// MCP client 管理器——管理所有外部 MCP server 的连接生命周期。
/// MCP tool 来源信息（0.13.6——供前端工具卡片增强）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct McpToolSource {
    pub tool_name: String,
    pub server_name: String,
    pub transport: String,
}

pub struct McpClientManager {
    /// 已连接的 server（name → ConnectedServer）。
    connected: Arc<RwLock<HashMap<String, ConnectedServer>>>,
    /// 各 server 的状态（供前端查询）。
    statuses: Arc<RwLock<HashMap<String, McpServerStatus>>>,
    /// tool 池版本号（单调递增）。任何改变会喂给 AI 的 tool 池的操作都 bump，
    /// ChatService 的 AgentCacheKey 含此 epoch，拓扑变化自然触发 cache miss。
    epoch: AtomicU64,
}

/// 连接重试次数（确定性错误不重试，只对网络/握手类错误重试）。
const MAX_START_RETRIES: usize = 2;
/// 重试间隔（秒）。
const RETRY_INTERVAL_SECS: u64 = 1;

impl McpClientManager {
    /// 构造空管理器。
    pub fn new() -> Self {
        Self {
            connected: Arc::new(RwLock::new(HashMap::new())),
            statuses: Arc::new(RwLock::new(HashMap::new())),
            epoch: AtomicU64::new(0),
        }
    }

    /// 返回当前 tool 池的版本号（单调递增）。
    ///
    /// 供 ChatService 构造 AgentCacheKey——MCP 拓扑变化时 epoch 不同，触发 cache miss。
    pub fn tool_pool_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    /// bump tool 池版本号，使其与之前所有缓存的 AgentCacheKey 失配。
    fn bump_epoch(&self) {
        let new_epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::debug!(epoch = new_epoch, "MCP: tool 池变化，bump epoch");
    }

    /// 0.13.7: lazy connect——连接所有 enabled 但尚未连接的 server。
    ///
    /// 在对话窗口首次需要 tool 时调用（`ensure_provider` → `ensure_connected` → `collect_tools`）。
    /// 已连接的 server 跳过，不在每次对话都重新连接。
    ///
    /// 与旧的 `start_all` 不同：不在 Blink 启动时调用，避免 npx 下载等慢操作拖慢启动。
    pub async fn ensure_connected(&self, config_pool: &sqlx::SqlitePool) {
        let configs = match McpServerConfigStore::load_all(config_pool).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "MCP: 加载 server 配置失败，跳过 lazy connect");
                return;
            }
        };

        // 过滤出 enabled 且尚未连接的 server
        let connected = self.connected.read().await;
        let to_connect: Vec<_> = configs
            .into_iter()
            .filter(|c| c.enabled && !connected.contains_key(&c.name))
            .collect();
        drop(connected); // 释放读锁

        if to_connect.is_empty() {
            return;
        }

        tracing::info!(
            count = to_connect.len(),
            "MCP: lazy connect——连接尚未连接的 enabled server"
        );

        for config in to_connect {
            // 不阻塞——单个 server 连接失败只记状态
            if let Err(e) = self.start_server(&config).await {
                tracing::warn!(
                    server = %config.name,
                    error = %e,
                    "MCP: server lazy connect 失败"
                );
            }
        }
    }

    /// 0.13.7: 「测试连接」——连接 + 拉 tool 列表 + 立即断开。
    ///
    /// 用于设置页的「测试连接」按钮——验证 server 是否可用，不保持子进程。
    /// 返回 tool 列表供前端预览，断开后状态回到 Offline。
    ///
    /// 0.13.8: 不再经过 `try_connect_once` → `finalize_connection`（会写入 connected 表），
    /// 改用 `try_connect_transient`——连接 + 拉 tool 列表但不存入 connected 表，
    /// 从根源消除测试连接期间临时 entry 被并发的 `collect_tools` 拾取的竞态。
    pub async fn test_connection(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpToolInfo>, String> {
        let tools = self.try_connect_transient(config).await?;
        // service 在 try_connect_transient 中已 drop（子进程被 kill），无需手动 remove

        // 恢复运行时状态：如果 server 已有持久连接（在 connected map 中），
        // 恢复其 Online 状态——test_connection 的 transient 连接不应影响持久连接的状态。
        // 只有原本就没有持久连接的 server 才设为 Offline。
        let connected = self.connected.read().await;
        if let Some(server) = connected.get(&config.name) {
            let tool_count = server.tools.len();
            drop(connected);
            self.set_status(&config.name, McpServerStatus::Online { tool_count })
                .await;
        } else {
            drop(connected);
            self.set_status(
                &config.name,
                McpServerStatus::Offline {
                    reason: "测试连接完成".to_string(),
                },
            )
            .await;
        }
        Ok(tools)
    }

    /// 启动单个 MCP server（含重试）。
    ///
    /// 确定性错误（空 command / URL 格式错误）不重试，立即返回。
    /// 网络类错误（握手超时 / 连接拒绝）重试 MAX_START_RETRIES 次。
    pub async fn start_server(&self, config: &McpServerConfig) -> Result<(), String> {
        self.set_status(&config.name, McpServerStatus::Connecting)
            .await;

        let mut last_err = String::new();
        for attempt in 1..=MAX_START_RETRIES {
            tracing::debug!(
                server = %config.name,
                attempt,
                max = MAX_START_RETRIES,
                "MCP: 尝试连接 server"
            );
            match self.try_connect_once(config).await {
                Ok(tools) => {
                    let tool_count = tools.len();
                    tracing::info!(
                        server = %config.name,
                        tools = tool_count,
                        "MCP: server 已连接"
                    );
                    self.set_status(&config.name, McpServerStatus::Online { tool_count })
                        .await;
                    // server 进入 tool 池，bump epoch 使旧 AgentCacheKey 失配
                    self.bump_epoch();
                    return Ok(());
                }
                Err(e) => {
                    last_err = e.clone();
                    // 确定性错误不重试
                    if Self::is_deterministic_error(&e) {
                        tracing::debug!(
                            server = %config.name,
                            error = %last_err,
                            "MCP: 确定性错误，不重试"
                        );
                        break;
                    }
                    tracing::debug!(
                        server = %config.name,
                        attempt,
                        error = %last_err,
                        "MCP: 连接失败，等待重试"
                    );
                    if attempt < MAX_START_RETRIES {
                        tokio::time::sleep(std::time::Duration::from_secs(RETRY_INTERVAL_SECS))
                            .await;
                    }
                }
            }
        }
        self.set_status(
            &config.name,
            McpServerStatus::Offline {
                reason: last_err.clone(),
            },
        )
        .await;
        Err(last_err)
    }

    /// 判断错误是否为确定性错误（重试也不会成功）。
    ///
    /// 确定性错误包括：
    /// - 空命令 / 空URL
    /// - 程序不存在
    /// - 无效的 HTTP header
    fn is_deterministic_error(err: &str) -> bool {
        err.contains("command 不能为空")
            || err.contains("URL 不能为空")
            || err.contains("program path has no file name")
            || err.contains("系统找不到指定的文件")
            || err.contains("No such file or directory")
    }

    /// 单次连接尝试（不含重试逻辑）。
    ///
    /// 供 `start_server`（含重试）和 `test_connection`（不重试）共用。
    async fn try_connect_once(&self, config: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
        self.try_connect(config).await
    }

    /// 尝试连接单个 server（根据 transport 类型分派 stdio / SSE / HTTP）。
    async fn try_connect(&self, config: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
        use crate::domain::mcp::config::McpTransport;
        match &config.transport {
            McpTransport::Stdio => self.try_connect_stdio(config).await,
            McpTransport::Sse { url, headers } => self.try_connect_sse(config, url, headers).await,
            McpTransport::Http { url, headers } => {
                self.try_connect_http(config, url, headers).await
            }
        }
    }

    /// 0.13.8: 临时连接——连接 + 拉 tool 列表，不写入 connected 表。
    ///
    /// 供 `test_connection` 使用——与 `try_connect_once` 不同，不调用
    /// `finalize_connection`（不存入 connected 表），避免测试连接期间的临时 entry
    /// 被并发的 `collect_tools` 拾取。
    ///
    /// service 在方法返回时 drop → 子进程被 kill，不保持连接。
    async fn try_connect_transient(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpToolInfo>, String> {
        use crate::domain::mcp::config::McpTransport;
        let service = match &config.transport {
            McpTransport::Stdio => self.build_stdio_service(config).await?,
            McpTransport::Sse { url, headers } => {
                self.build_sse_service(config, url, headers).await?
            }
            McpTransport::Http { url, headers } => {
                self.build_http_service(config, url, headers).await?
            }
        };
        // 只拉 tool 列表，不存入 connected 表
        let (_rmcp_tools, tools) = Self::pull_tools_from_service(&service, config).await?;
        // service 在此 drop → 子进程被 kill
        Ok(tools)
    }

    /// stdio 模式连接——拉起子进程 + 握手 + 拉 tool 列表。
    async fn try_connect_stdio(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpToolInfo>, String> {
        let service = self.build_stdio_service(config).await?;
        self.finalize_connection(config, service).await
    }

    /// 构建 stdio 模式的 rmcp service（拉起子进程 + 握手），不含存储逻辑。
    ///
    /// 供 `try_connect_stdio`（持久连接）和 `try_connect_transient`（临时探测）共用。
    async fn build_stdio_service(
        &self,
        config: &McpServerConfig,
    ) -> Result<RunningService<rmcp::service::RoleClient, ClientInfo>, String> {
        // 前置校验——空 command 是确定性错误，不重试
        if config.command.is_empty() {
            return Err(format!(
                "stdio 模式 command 不能为空（server: {}）",
                config.name
            ));
        }

        // 构造子进程命令
        // Windows 上，CreateProcess 只查找 .exe 文件。
        // 命令如 `codegraph` 实际是 `codegraph.cmd`，需要 `cmd /c` 包裹。
        let (program, program_args) = resolve_windows_command(&config.command, &config.args);
        let mut cmd = crate::infra::platform::no_window_tokio(
            tokio::process::Command::new(&program),
        );
        cmd.args(&program_args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        // 创建 stdio transport
        let transport = rmcp::transport::TokioChildProcess::new(cmd)
            .map_err(|e| format!("子进程启动失败: {e}"))?;

        // 用 rmcp 默认 ClientInfo 建立 client 连接
        let client_info = ClientInfo::default();
        client_info
            .serve(transport)
            .await
            .map_err(|e| format!("MCP 握手失败: {e}"))
    }

    /// HTTP 模式连接——Streamable HTTP transport + 握手 + 拉 tool 列表（0.13.6）。
    async fn try_connect_http(
        &self,
        config: &McpServerConfig,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<McpToolInfo>, String> {
        let service = self.build_http_service(config, url, headers).await?;
        self.finalize_connection(config, service).await
    }

    /// 构建 HTTP 模式的 rmcp service（Streamable HTTP transport + 握手），不含存储逻辑。
    ///
    /// 供 `try_connect_http`（持久连接）和 `try_connect_transient`（临时探测）共用。
    async fn build_http_service(
        &self,
        config: &McpServerConfig,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<RunningService<rmcp::service::RoleClient, ClientInfo>, String> {
        // 前置校验——空 URL 是确定性错误
        if url.is_empty() {
            return Err(format!("HTTP 模式 URL 不能为空（server: {}）", config.name));
        }

        tracing::info!(
            server = %config.name,
            url = %url,
            "MCP: 尝试 HTTP 连接"
        );

        // rmcp Streamable HTTP client transport
        // 统一用 from_config：支持 custom_headers（含 Authorization 等任意 header）
        use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
        let mut cfg = StreamableHttpClientTransportConfig::with_uri(url);

        if !headers.is_empty() {
            let mut custom_headers = std::collections::HashMap::new();
            for (k, v) in headers {
                // rmcp 保留 header（accept / mcp-session-id / last-event-id）跳过
                let lower = k.to_lowercase();
                if matches!(
                    lower.as_str(),
                    "accept" | "mcp-session-id" | "last-event-id"
                ) {
                    tracing::warn!(header = %k, "MCP: 保留 header 已跳过");
                    continue;
                }
                if let (Ok(name), Ok(val)) = (
                    http::HeaderName::try_from(k.as_str()),
                    http::HeaderValue::try_from(v.as_str()),
                ) {
                    custom_headers.insert(name, val);
                } else {
                    tracing::warn!(header = %k, "MCP: 无效的 HTTP header，已跳过");
                }
            }
            if !custom_headers.is_empty() {
                cfg = cfg.custom_headers(custom_headers);
            }
        }

        let transport = rmcp::transport::StreamableHttpClientTransport::from_config(cfg);

        let client_info = ClientInfo::default();
        client_info
            .serve(transport)
            .await
            .map_err(|e| format!("HTTP MCP 握手失败: {e}"))
    }

    /// SSE 模式连接——旧版 SSE transport + 握手 + 拉 tool 列表（0.13.8）。
    ///
    /// SSE 协议：GET `/sse` → SSE 长连接 → `endpoint` 事件带 POST URL →
    /// POST JSON-RPC 到 POST URL → 响应通过 SSE 流返回。
    ///
    /// 使用自建 `SseClientTransport`（实现 `rmcp::Transport<RoleClient>`），
    /// 因为 rmcp 的 `StreamableHttpClientTransport` 用 POST 到单一端点，
    /// 对 SSE server 的 `/sse` 端点发 POST 会得到 405 Method Not Allowed。
    async fn try_connect_sse(
        &self,
        config: &McpServerConfig,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<Vec<McpToolInfo>, String> {
        let service = self.build_sse_service(config, url, headers).await?;
        self.finalize_connection(config, service).await
    }

    /// 构建 SSE 模式的 rmcp service（旧版 SSE transport + 握手），不含存储逻辑。
    ///
    /// 供 `try_connect_sse`（持久连接）和 `try_connect_transient`（临时探测）共用。
    async fn build_sse_service(
        &self,
        config: &McpServerConfig,
        url: &str,
        headers: &std::collections::HashMap<String, String>,
    ) -> Result<RunningService<rmcp::service::RoleClient, ClientInfo>, String> {
        if url.is_empty() {
            return Err(format!("SSE 模式 URL 不能为空（server: {}）", config.name));
        }

        tracing::info!(
            server = %config.name,
            url = %url,
            "MCP: 尝试 SSE 连接"
        );

        let transport = crate::domain::mcp::sse_transport::SseClientTransport::new(url, headers)
            .await
            .map_err(|e| format!("SSE 连接失败: {e}"))?;

        let client_info = ClientInfo::default();
        client_info
            .serve(transport)
            .await
            .map_err(|e| format!("SSE MCP 握手失败: {e}"))
    }

    /// 连接后通用逻辑——拉 tool 列表 + 转换 + 存入连接表。
    async fn finalize_connection(
        &self,
        config: &McpServerConfig,
        service: RunningService<rmcp::service::RoleClient, ClientInfo>,
    ) -> Result<Vec<McpToolInfo>, String> {
        let (rmcp_tools, tools) = Self::pull_tools_from_service(&service, config).await?;

        // 存入连接表（同时缓存原始 rmcp tool 列表，collect_tools 时直接用，不重新拉取）
        let mut connected = self.connected.write().await;
        connected.insert(
            config.name.clone(),
            ConnectedServer {
                service,
                rmcp_tools,
                tools: tools.clone(),
                config: config.clone(),
            },
        );

        Ok(tools)
    }

    /// 0.13.8: 从已建立的 service 拉 tool 列表 + 转换为 McpToolInfo（不含存储逻辑）。
    ///
    /// 供 `finalize_connection`（持久连接 → 存入 connected 表）和
    /// `try_connect_transient`（临时探测 → 不存入）共用。
    async fn pull_tools_from_service(
        service: &RunningService<rmcp::service::RoleClient, ClientInfo>,
        config: &McpServerConfig,
    ) -> Result<(Vec<rmcp::model::Tool>, Vec<McpToolInfo>), String> {
        // 拉 tool 列表
        let rmcp_tools = service
            .peer()
            .list_all_tools()
            .await
            .map_err(|e| format!("拉取 tool 列表失败: {e}"))?;

        // 转换为 McpToolInfo（含 disabled 标记）
        let tools: Vec<McpToolInfo> = rmcp_tools
            .iter()
            .map(|t| McpToolInfo {
                name: t.name.to_string(),
                description: t
                    .description
                    .as_ref()
                    .map(|d| d.to_string())
                    .unwrap_or_default(),
                disabled: config.disabled_tools.contains(&t.name.to_string()),
            })
            .collect();

        Ok((rmcp_tools, tools))
    }

    /// 停止单个 server（断开连接 + 杀子进程）。
    pub async fn stop_server(&self, name: &str) {
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.remove(name) {
            // Drop service 会触发子进程清理（TokioChildProcess 的 Drop impl 会 kill）
            drop(server);
            tracing::info!(server = %name, "MCP: server 已停止");
            // 仅当实际 remove 了才 bump——没移除则池子没变，bump 会无谓触发重建
            self.bump_epoch();
        }
        self.set_status(
            name,
            McpServerStatus::Offline {
                reason: "手动停止".to_string(),
            },
        )
        .await;
    }

    /// 重连单个 server（先停再启）。
    pub async fn reconnect_server(
        &self,
        name: &str,
        config_pool: &sqlx::SqlitePool,
    ) -> Result<(), String> {
        self.stop_server(name).await;

        let configs = McpServerConfigStore::load_all(config_pool)
            .await
            .map_err(|e| e.to_string())?;
        let config = configs
            .into_iter()
            .find(|c| c.name == name)
            .ok_or_else(|| format!("未找到 server 配置: {name}"))?;

        self.start_server(&config).await
    }

    /// 收集所有已连接 server 的 tool，包装为 `Vec<Box<dyn ToolDyn>>`。
    ///
    /// 过滤 `disabled_tools` 中的 tool——只把用户启用的喂给 AI。
    /// 返回的每个 tool 是 `rig_core::tool::rmcp::McpTool`（已 impl `ToolDyn`）。
    ///
    /// 使用 `ConnectedServer.rmcp_tools` 缓存（握手时已拉取），不重新调 `list_all_tools`。
    pub async fn collect_tools(&self) -> Vec<Box<dyn ToolDyn>> {
        let connected = self.connected.read().await;
        let mut tools: Vec<Box<dyn ToolDyn>> = Vec::new();

        for (_name, server) in connected.iter() {
            // 获取 peer（ServerSink = Peer<RoleClient>），用于构造 McpTool
            let peer = server.service.peer().clone();
            // 从缓存的 rmcp tool 列表构造 McpTool，过滤 disabled
            for rmcp_tool in &server.rmcp_tools {
                let tool_name = rmcp_tool.name.to_string();
                if server.config.disabled_tools.contains(&tool_name) {
                    continue;
                }
                let mcp_tool =
                    rig_core::tool::rmcp::McpTool::from_mcp_server(rmcp_tool.clone(), peer.clone());
                tools.push(Box::new(mcp_tool));
            }
        }

        tracing::info!(total_tools = tools.len(), "MCP: tool 池收集完成");
        tools
    }

    /// 轻量计数——只统计已连接 server 提供的非 disabled tool 数量。
    ///
    /// 与 `collect_tools()` 不同：不构造 `McpTool`（不做 `rmcp_tool.clone()` +
    /// `peer.clone()` + `Box::new`），只遍历计数，供前端 tool 池规模展示用。
    pub async fn count_tools(&self) -> usize {
        let connected = self.connected.read().await;
        let mut count = 0;
        for server in connected.values() {
            for rmcp_tool in &server.rmcp_tools {
                let tool_name = rmcp_tool.name.to_string();
                if !server.config.disabled_tools.contains(&tool_name) {
                    count += 1;
                }
            }
        }
        count
    }

    /// 获取所有 server 的状态（供前端查询）。
    pub async fn get_statuses(&self) -> HashMap<String, McpServerStatus> {
        self.statuses.read().await.clone()
    }

    /// 获取单个 server 的 tool 列表（供前端预览）。
    pub async fn get_server_tools(&self, name: &str) -> Option<Vec<McpToolInfo>> {
        let connected = self.connected.read().await;
        connected.get(name).map(|s| s.tools.clone())
    }

    /// 获取所有已连接 server 的 tool 名称集合（供前端区分工具来源）。
    ///
    /// 收集所有未被 disabled 的 MCP tool 名称，前端用它判断 tool_call 来源
    /// 是「内置」还是「MCP」，在工具卡片上显示来源标记。
    pub async fn get_all_tool_names(&self) -> Vec<String> {
        let connected = self.connected.read().await;
        let mut names = Vec::new();
        for server in connected.values() {
            for tool in &server.rmcp_tools {
                let name = tool.name.to_string();
                if !server.config.disabled_tools.contains(&name) {
                    names.push(name);
                }
            }
        }
        names
    }

    /// 0.13.6: 获取所有 MCP tool 的来源信息（server 名 + transport 类型）。
    ///
    /// 供前端工具卡片增强——显示 MCP 工具来自哪个 server、用哪种协议。
    pub async fn get_tool_sources(&self) -> Vec<McpToolSource> {
        let connected = self.connected.read().await;
        let mut sources = Vec::new();
        for (server_name, server) in connected.iter() {
            let transport = match &server.config.transport {
                crate::domain::mcp::config::McpTransport::Stdio => "stdio",
                crate::domain::mcp::config::McpTransport::Sse { .. } => "sse",
                crate::domain::mcp::config::McpTransport::Http { .. } => "http",
            };
            for tool in &server.rmcp_tools {
                let tool_name = tool.name.to_string();
                if !server.config.disabled_tools.contains(&tool_name) {
                    sources.push(McpToolSource {
                        tool_name,
                        server_name: server_name.clone(),
                        transport: transport.to_string(),
                    });
                }
            }
        }
        sources
    }

    /// 更新单个 server 的 disabled_tools 并刷新 tool 列表缓存。
    pub async fn update_disabled_tools(&self, name: &str, disabled_tools: Vec<String>) {
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.get_mut(name) {
            server.config.disabled_tools = disabled_tools.clone();
            // 更新 tools 列表中的 disabled 标记
            for tool in &mut server.tools {
                tool.disabled = disabled_tools.contains(&tool.name);
            }
            // disabled_tools 变化改变了喂给 AI 的 tool 池，bump epoch
            self.bump_epoch();
        } else {
            // server 未连接——DB 已更新，运行时缓存无此 server。
            // 下次 ensure_connected 时会从 DB 加载最新配置，最终一致。
            tracing::debug!(
                server = %name,
                "MCP: update_disabled_tools 时 server 未连接，运行时缓存未更新（下次连接时从 DB 加载）"
            );
        }
    }

    /// 停止所有 server（进程退出时调用）。
    pub async fn stop_all(&self) {
        let mut connected = self.connected.write().await;
        let names: Vec<String> = connected.keys().cloned().collect();
        connected.clear();
        drop(connected);
        self.bump_epoch();
        // Drop 所有 service（触发子进程清理）
        tracing::info!(count = names.len(), "MCP: 所有 server 已停止");
    }

    /// 设置单个 server 的状态。
    async fn set_status(&self, name: &str, status: McpServerStatus) {
        self.statuses.write().await.insert(name.to_string(), status);
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

/// Windows 命令解析：处理 `.cmd`/`.bat` 文件。
///
/// `CreateProcess`（Rust `Command::new` 底层）只搜索 `.exe` 文件。
/// 命令如 `codegraph` 实际是 `codegraph.cmd`，直接传给 `Command::new`
/// 会报 "program not found"。
///
/// 策略：
/// 1. 命令已有扩展名 → 原样使用
/// 2. 在 PATH 中找到 `command.exe` → 用完整路径
/// 3. 否则用 `cmd /c` 包裹（cmd.exe 能正确解析 .cmd/.bat）
fn resolve_windows_command(command: &str, args: &[String]) -> (String, Vec<String>) {
    // 命令已有扩展名 → 原样使用
    if std::path::Path::new(command).extension().is_some() {
        return (command.to_string(), args.to_vec());
    }

    #[cfg(target_os = "windows")]
    {
        // 在 PATH 中查找 command.exe
        if let Ok(path) = std::env::var("PATH") {
            for dir in path.split(';') {
                let exe_path = std::path::Path::new(dir).join(format!("{command}.exe"));
                if exe_path.exists() {
                    return (exe_path.to_string_lossy().to_string(), args.to_vec());
                }
            }
        }

        // 未找到 .exe → 用 cmd /c 包裹（处理 .cmd/.bat）
        let mut all_args = vec!["/c".to_string(), command.to_string()];
        all_args.extend_from_slice(args);
        return ("cmd".to_string(), all_args);
    }

    #[cfg(not(target_os = "windows"))]
    {
        (command.to_string(), args.to_vec())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_client_manager_can_construct() {
        let _manager = McpClientManager::new();
    }

    #[tokio::test]
    async fn get_statuses_empty_when_no_servers() {
        let manager = McpClientManager::new();
        let statuses = manager.get_statuses().await;
        assert!(statuses.is_empty());
    }

    #[tokio::test]
    async fn collect_tools_empty_when_no_servers() {
        let manager = McpClientManager::new();
        let tools = manager.collect_tools().await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn get_server_tools_returns_none_for_unknown() {
        let manager = McpClientManager::new();
        assert!(manager.get_server_tools("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn get_all_tool_names_empty_when_no_servers() {
        let manager = McpClientManager::new();
        let names = manager.get_all_tool_names().await;
        assert!(names.is_empty());
    }

    // ── 0.13.7: 空命令校验 + 确定性错误 ──

    #[tokio::test]
    async fn try_connect_stdio_empty_command_returns_error() {
        let manager = McpClientManager::new();
        let config = McpServerConfig {
            name: "empty-test".to_string(),
            transport: crate::domain::mcp::config::McpTransport::Stdio,
            command: String::new(), // 空 command
            args: vec![],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: vec![],
        };
        let result = manager.try_connect_stdio(&config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("command 不能为空"), "error was: {err}");
    }

    #[tokio::test]
    async fn try_connect_http_empty_url_returns_error() {
        let manager = McpClientManager::new();
        let config = McpServerConfig {
            name: "empty-url-test".to_string(),
            transport: crate::domain::mcp::config::McpTransport::Http {
                url: String::new(),
                headers: std::collections::HashMap::new(),
            },
            command: String::new(),
            args: vec![],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: vec![],
        };
        let result = manager
            .try_connect_http(&config, "", &std::collections::HashMap::new())
            .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("URL 不能为空"), "error was: {err}");
    }

    #[test]
    fn is_deterministic_error_detects_empty_command() {
        assert!(McpClientManager::is_deterministic_error(
            "stdio 模式 command 不能为空（server: test）"
        ));
    }

    #[test]
    fn is_deterministic_error_detects_missing_program() {
        assert!(McpClientManager::is_deterministic_error(
            "program path has no file name"
        ));
    }

    #[test]
    fn is_deterministic_error_rejects_network_error() {
        // 网络错误应该重试，不算确定性错误
        assert!(!McpClientManager::is_deterministic_error(
            "MCP 握手失败: connection refused"
        ));
    }

    #[tokio::test]
    async fn start_server_empty_command_does_not_retry() {
        // 空命令是确定性错误，start_server 应立即返回，不重试
        let manager = McpClientManager::new();
        let config = McpServerConfig {
            name: "no-retry-test".to_string(),
            transport: crate::domain::mcp::config::McpTransport::Stdio,
            command: String::new(),
            args: vec![],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: vec![],
        };
        let start = std::time::Instant::now();
        let result = manager.start_server(&config).await;
        let elapsed = start.elapsed();
        assert!(result.is_err());
        // 确定性错误不重试，应该几乎不耗时（< 1s，重试会有 1s sleep）
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "elapsed: {elapsed:?}"
        );
    }

    // ── P3: MCP 双向投影闭环（Capability → MCP Tool → Capability schema 一致性）──

    /// 验证 MCP 双向投影闭环：
    /// CapabilitySchema → 正向投影 → rmcp::Tool → （再验证 name/description/input_schema 一致）
    ///
    /// 这模拟了 Blink 作为 MCP server 暴露能力，Blink 作为 MCP client 消费的全链路。
    /// 实际 stdio 子进程拉起需要编译后的 blink.exe，在单测中用投影一致性验证闭环。
    #[test]
    fn mcp_projection_roundtrip_preserves_schema() {
        use crate::domain::capability::CapabilitySchema;
        use crate::domain::mcp::projection::capability_schema_to_mcp_tool;
        use serde_json::json;

        // 模拟 Blink 暴露的 Capability
        let schema = CapabilitySchema {
            name: "search_files".to_string(),
            description: "搜索文件系统".to_string(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "搜索关键词" }
                },
                "required": ["query"]
            }),
            sensitive: false,
        };

        // 正向投影：Capability → MCP Tool（Blink 作为 server 暴露）
        let mcp_tool = capability_schema_to_mcp_tool(&schema);

        // 验证投影后字段一致（Blink 作为 client 消费时看到的）
        assert_eq!(mcp_tool.name, "search_files");
        assert_eq!(mcp_tool.description.as_deref(), Some("搜索文件系统"));
        assert_eq!(mcp_tool.input_schema["type"], "object");
        assert_eq!(
            mcp_tool.input_schema["properties"]["query"]["type"],
            "string"
        );
        assert_eq!(mcp_tool.input_schema["required"][0], "query");
    }

    /// 验证批量投影 + sensitive 不暴露 annotations 的安全策略。
    #[test]
    fn mcp_batch_projection_and_sensitive_handling() {
        use crate::domain::capability::CapabilitySchema;
        use crate::domain::mcp::projection::capability_schemas_to_mcp_tools;

        let schemas = vec![
            CapabilitySchema::empty("read_clipboard", "读剪贴板"),
            CapabilitySchema {
                name: "search_apps".to_string(),
                description: "搜索应用".to_string(),
                parameters: serde_json::json!({"type": "object", "properties": {}}),
                sensitive: true, // sensitive 不应映射到 MCP annotations
            },
            CapabilitySchema::empty("capture_screen", "截屏"),
        ];

        let tools = capability_schemas_to_mcp_tools(&schemas);
        assert_eq!(tools.len(), 3);
        // sensitive 的 tool 也不应有 annotations（授权由 BlinkMcpServer 在 call_tool 检查）
        for tool in &tools {
            assert!(
                tool.annotations.is_none(),
                "tool {} should not have annotations",
                tool.name
            );
        }
    }

    // ── MCP tool 池 epoch 测试 ──

    #[test]
    fn tool_pool_epoch_starts_at_zero() {
        let manager = McpClientManager::new();
        assert_eq!(manager.tool_pool_epoch(), 0);
    }

    #[tokio::test]
    async fn start_server_failure_does_not_bump_epoch() {
        // 空命令是确定性错误，连接失败不应 bump epoch
        let manager = McpClientManager::new();
        let config = McpServerConfig {
            name: "fail-test".to_string(),
            transport: crate::domain::mcp::config::McpTransport::Stdio,
            command: String::new(),
            args: vec![],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: vec![],
        };
        let epoch_before = manager.tool_pool_epoch();
        let result = manager.start_server(&config).await;
        assert!(result.is_err());
        assert_eq!(
            manager.tool_pool_epoch(),
            epoch_before,
            "连接失败不应 bump epoch"
        );
    }

    #[tokio::test]
    async fn stop_server_on_unknown_name_does_not_bump_epoch() {
        // 对不存在的 server name 调 stop_server，epoch 不应变
        let manager = McpClientManager::new();
        let epoch_before = manager.tool_pool_epoch();
        manager.stop_server("nonexistent").await;
        assert_eq!(
            manager.tool_pool_epoch(),
            epoch_before,
            "stop_server 未实际移除 server 时不应 bump epoch"
        );
    }
}
