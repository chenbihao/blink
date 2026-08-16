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
//! - 首次可见用 `prewarm()` 后台连接；prompt 用 `prepare_enabled()` 最多等待 5 秒
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
use std::time::{Duration, Instant};

use rig_core::tool::ToolDyn;
use rmcp::ServiceExt;
use rmcp::model::ClientInfo;
use rmcp::service::RunningService;
use tokio::sync::{Mutex, Notify, RwLock, Semaphore};

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

/// 已完成握手和 tool 拉取、但尚未通过 generation 校验的候选连接。
///
/// transport 只有在 `commit_connection` 通过代际校验后才进入 `connected`；
/// 旧任务的候选值会直接 drop，从而关闭旧 stdio/HTTP/SSE transport。
struct PendingConnection {
    service: RunningService<rmcp::service::RoleClient, ClientInfo>,
    rmcp_tools: Vec<rmcp::model::Tool>,
    tools: Vec<McpToolInfo>,
    config: McpServerConfig,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ConnectIntent {
    Automatic,
    Manual,
}

struct ServerLifecycle {
    generation: u64,
    /// 只比较会影响 transport 的字段；enabled/disabled_tools 不应导致重连。
    connection_config: Option<McpServerConfig>,
    connecting_generation: Option<u64>,
    cooldown_until: Option<Instant>,
    last_result: Option<(u64, Result<(), String>)>,
    notify: Arc<Notify>,
}

impl ServerLifecycle {
    fn new(config: &McpServerConfig) -> Self {
        Self {
            generation: 1,
            connection_config: Some(config.clone()),
            connecting_generation: None,
            cooldown_until: None,
            last_result: None,
            notify: Arc::new(Notify::new()),
        }
    }
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
    /// 每个 server 的 generation / single-flight / cooldown 状态。
    lifecycles: Mutex<HashMap<String, ServerLifecycle>>,
    /// 不同 server 的 transport 建连并发上限。
    connect_slots: Arc<Semaphore>,
    #[cfg(test)]
    connect_attempts: AtomicU64,
    /// tool 池版本号（单调递增）。任何改变会喂给 AI 的 tool 池的操作都 bump，
    /// ChatService 的 AgentCacheKey 含此 epoch，拓扑变化自然触发 cache miss。
    epoch: AtomicU64,
}

/// 连接重试次数（确定性错误不重试，只对网络/握手类错误重试）。
const MAX_START_RETRIES: usize = 2;
/// 重试间隔（秒）。
const RETRY_INTERVAL_SECS: u64 = 1;
/// 自动连接失败后的冷却期；配置变化和手动操作可绕过。
const FAILURE_COOLDOWN: Duration = Duration::from_secs(30);
/// 跨 server 同时建立 transport 的默认上限。
const MAX_CONCURRENT_CONNECTS: usize = 3;
/// 单次 transport 握手 + tool 拉取的上限，防止旧 generation 永久占住 single-flight。
const CONNECT_ATTEMPT_TIMEOUT: Duration = Duration::from_secs(15);
/// prompt 等待 MCP 工具准备的独立预算。
pub const PROMPT_PREPARE_BUDGET: Duration = Duration::from_secs(5);

impl McpClientManager {
    /// 构造空管理器。
    pub fn new() -> Self {
        Self {
            connected: Arc::new(RwLock::new(HashMap::new())),
            statuses: Arc::new(RwLock::new(HashMap::new())),
            lifecycles: Mutex::new(HashMap::new()),
            connect_slots: Arc::new(Semaphore::new(MAX_CONCURRENT_CONNECTS)),
            #[cfg(test)]
            connect_attempts: AtomicU64::new(0),
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

    /// 首次可见等非阻塞触发点使用：任务立即进入后台，不阻塞窗口显示。
    pub fn prewarm(self: &Arc<Self>, config_pool: sqlx::SqlitePool) {
        let manager = self.clone();
        tokio::spawn(async move {
            manager.connect_enabled(&config_pool, None).await;
        });
    }

    /// prompt 使用：最多等待 `budget`，未完成的连接任务保持在后台继续运行。
    ///
    /// 返回 true 表示预算内全部 enabled server 的当前连接任务已结束；false 表示预算到期。
    pub async fn prepare_enabled(
        self: &Arc<Self>,
        config_pool: &sqlx::SqlitePool,
        budget: Duration,
    ) -> bool {
        self.connect_enabled(config_pool, Some(budget)).await
    }

    async fn connect_enabled(
        self: &Arc<Self>,
        config_pool: &sqlx::SqlitePool,
        budget: Option<Duration>,
    ) -> bool {
        let configs = match McpServerConfigStore::load_all(config_pool).await {
            Ok(configs) => configs,
            Err(e) => {
                tracing::warn!(error = %e, "MCP: 加载 server 配置失败，跳过连接准备");
                return true;
            }
        };
        let enabled: Vec<_> = configs
            .into_iter()
            .filter(|config| config.enabled)
            .collect();
        if enabled.is_empty() {
            return true;
        }

        let mut handles = Vec::with_capacity(enabled.len());
        for config in enabled {
            let manager = self.clone();
            handles.push(tokio::spawn(async move {
                if let Err(error) = manager
                    .connect_server(&config, ConnectIntent::Automatic)
                    .await
                {
                    tracing::debug!(server = %config.name, %error, "MCP: 自动连接未就绪");
                }
            }));
        }

        let wait_all = futures::future::join_all(handles);
        if let Some(budget) = budget {
            match tokio::time::timeout(budget, wait_all).await {
                Ok(_) => true,
                Err(_) => {
                    tracing::debug!(
                        budget_ms = budget.as_millis(),
                        "MCP: 工具准备预算到期，使用当前已就绪快照"
                    );
                    false
                }
            }
        } else {
            wait_all.await;
            true
        }
    }

    /// 设置页「测试连接」：加入持久连接 single-flight，成功后直接复用为在线连接。
    pub async fn test_connection(
        &self,
        config: &McpServerConfig,
    ) -> Result<Vec<McpToolInfo>, String> {
        // 0.19.11: 设置页测试也加入同名 server 的 single-flight，避免与预热/prompt
        // 同时各建一条 transport。测试成功后保留为正常在线连接。
        self.connect_server(config, ConnectIntent::Manual).await?;
        self.get_server_tools(&config.name)
            .await
            .ok_or_else(|| format!("server {} 连接完成但 tool 快照缺失", config.name))
    }

    /// 启动单个 MCP server（含重试）。
    ///
    /// 确定性错误（空 command / URL 格式错误）不重试，立即返回。
    /// 网络类错误（握手超时 / 连接拒绝）重试 MAX_START_RETRIES 次。
    pub async fn start_server(&self, config: &McpServerConfig) -> Result<(), String> {
        self.connect_server(config, ConnectIntent::Manual).await
    }

    async fn connect_server(
        &self,
        config: &McpServerConfig,
        intent: ConnectIntent,
    ) -> Result<(), String> {
        let generation = self.register_config(config).await;

        loop {
            let notified = {
                let mut lifecycles = self.lifecycles.lock().await;
                let lifecycle = lifecycles
                    .get_mut(&config.name)
                    .expect("register_config must create lifecycle");
                if lifecycle.generation != generation
                    || !lifecycle
                        .connection_config
                        .as_ref()
                        .is_some_and(|current| same_connection_config(current, config))
                {
                    return Err(format!("server {} 配置已变化，取消旧连接", config.name));
                }

                if self.connected.read().await.contains_key(&config.name) {
                    return Ok(());
                }

                if lifecycle.connecting_generation.is_some() {
                    Some(lifecycle.notify.clone().notified_owned())
                } else {
                    if intent == ConnectIntent::Automatic
                        && lifecycle
                            .cooldown_until
                            .is_some_and(|until| until > Instant::now())
                    {
                        return Err(format!("server {} 处于连接冷却期", config.name));
                    }
                    lifecycle.connecting_generation = Some(generation);
                    lifecycle.last_result = None;
                    None
                }
            };

            if let Some(notified) = notified {
                notified.await;
                let lifecycles = self.lifecycles.lock().await;
                let Some(lifecycle) = lifecycles.get(&config.name) else {
                    return Err(format!("server {} 已删除", config.name));
                };
                if let Some((result_generation, result)) = &lifecycle.last_result
                    && *result_generation == generation
                {
                    return result.clone();
                }
                continue;
            }
            break;
        }

        self.set_status_if_generation(&config.name, generation, McpServerStatus::Connecting)
            .await;

        let _permit = self
            .connect_slots
            .acquire()
            .await
            .map_err(|_| "MCP 连接并发控制器已关闭".to_string())?;
        if !self
            .is_current_generation(&config.name, generation, config)
            .await
        {
            self.finish_stale_attempt(&config.name, generation).await;
            return Err(format!("server {} 配置已变化，取消旧连接", config.name));
        }
        let mut last_err = String::new();
        let mut candidate = None;
        for attempt in 1..=MAX_START_RETRIES {
            if !self
                .is_current_generation(&config.name, generation, config)
                .await
            {
                self.finish_stale_attempt(&config.name, generation).await;
                return Err(format!("server {} 配置已变化，取消旧连接", config.name));
            }
            #[cfg(test)]
            self.connect_attempts.fetch_add(1, Ordering::SeqCst);
            tracing::debug!(
                server = %config.name,
                attempt,
                max = MAX_START_RETRIES,
                "MCP: 尝试连接 server"
            );
            let attempt_result =
                tokio::time::timeout(CONNECT_ATTEMPT_TIMEOUT, self.build_connection(config))
                    .await
                    .map_err(|_| {
                        format!("MCP 连接超时（{} 秒）", CONNECT_ATTEMPT_TIMEOUT.as_secs())
                    })
                    .and_then(|result| result);
            match attempt_result {
                Ok(connection) => {
                    candidate = Some(connection);
                    break;
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

        self.finish_connection(config, generation, candidate, last_err)
            .await
    }

    /// 注册/刷新配置并返回当前 generation。
    ///
    /// transport 字段变化会立即让旧任务失效并关闭已连接 transport；仅 enabled 或
    /// disabled_tools 变化不重连，后者只更新稳定 tool snapshot。
    async fn register_config(&self, config: &McpServerConfig) -> u64 {
        let mut lifecycles = self.lifecycles.lock().await;
        let lifecycle = lifecycles
            .entry(config.name.clone())
            .or_insert_with(|| ServerLifecycle::new(config));
        let connection_changed = lifecycle
            .connection_config
            .as_ref()
            .is_some_and(|current| !same_connection_config(current, config));

        if connection_changed {
            lifecycle.generation = lifecycle.generation.saturating_add(1);
            lifecycle.cooldown_until = None;
            lifecycle.last_result = None;
            lifecycle.notify.notify_waiters();
        }
        lifecycle.connection_config = Some(config.clone());
        let generation = lifecycle.generation;

        let mut connected = self.connected.write().await;
        if connection_changed {
            if connected.remove(&config.name).is_some() {
                self.bump_epoch();
            }
            self.statuses.write().await.insert(
                config.name.clone(),
                McpServerStatus::Offline {
                    reason: "配置已更新，等待重新连接".to_string(),
                },
            );
        } else if let Some(server) = connected.get_mut(&config.name) {
            let visibility_changed = server.config.disabled_tools != config.disabled_tools;
            server.config.enabled = config.enabled;
            server.config.disabled_tools = config.disabled_tools.clone();
            for tool in &mut server.tools {
                tool.disabled = config.disabled_tools.contains(&tool.name);
            }
            if visibility_changed {
                self.bump_epoch();
            }
        }
        generation
    }

    async fn build_connection(
        &self,
        config: &McpServerConfig,
    ) -> Result<PendingConnection, String> {
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
        let (rmcp_tools, tools) = Self::pull_tools_from_service(&service, config).await?;
        Ok(PendingConnection {
            service,
            rmcp_tools,
            tools,
            config: config.clone(),
        })
    }

    /// 唯一连接任务的原子提交点。generation 不匹配时 candidate 在返回时 drop。
    async fn finish_connection(
        &self,
        config: &McpServerConfig,
        generation: u64,
        candidate: Option<PendingConnection>,
        error: String,
    ) -> Result<(), String> {
        let mut lifecycles = self.lifecycles.lock().await;
        let Some(lifecycle) = lifecycles.get_mut(&config.name) else {
            return Err(format!("server {} 已删除，丢弃旧连接", config.name));
        };
        if lifecycle.generation != generation
            || !lifecycle
                .connection_config
                .as_ref()
                .is_some_and(|current| same_connection_config(current, config))
        {
            drop(candidate);
            if lifecycle.connecting_generation == Some(generation) {
                lifecycle.connecting_generation = None;
                lifecycle.notify.notify_waiters();
            }
            return Err(format!("server {} 配置已变化，丢弃旧连接", config.name));
        }

        let result = if let Some(mut candidate) = candidate {
            if let Some(latest_config) = lifecycle.connection_config.as_ref() {
                candidate.config.enabled = latest_config.enabled;
                candidate.config.disabled_tools = latest_config.disabled_tools.clone();
                for tool in &mut candidate.tools {
                    tool.disabled = candidate.config.disabled_tools.contains(&tool.name);
                }
            }
            let tool_count = candidate.tools.len();
            self.connected.write().await.insert(
                config.name.clone(),
                ConnectedServer {
                    service: candidate.service,
                    rmcp_tools: candidate.rmcp_tools,
                    tools: candidate.tools,
                    config: candidate.config,
                },
            );
            self.statuses
                .write()
                .await
                .insert(config.name.clone(), McpServerStatus::Online { tool_count });
            lifecycle.cooldown_until = None;
            self.bump_epoch();
            tracing::info!(server = %config.name, tools = tool_count, "MCP: server 已连接");
            Ok(())
        } else {
            let error = if error.is_empty() {
                "MCP 连接失败".to_string()
            } else {
                error
            };
            lifecycle.cooldown_until = Some(Instant::now() + FAILURE_COOLDOWN);
            self.statuses.write().await.insert(
                config.name.clone(),
                McpServerStatus::Offline {
                    reason: error.clone(),
                },
            );
            Err(error)
        };
        lifecycle.connecting_generation = None;
        lifecycle.last_result = Some((generation, result.clone()));
        lifecycle.notify.notify_waiters();
        result
    }

    async fn set_status_if_generation(&self, name: &str, generation: u64, status: McpServerStatus) {
        let lifecycles = self.lifecycles.lock().await;
        if lifecycles
            .get(name)
            .is_some_and(|lifecycle| lifecycle.generation == generation)
        {
            self.statuses.write().await.insert(name.to_string(), status);
        }
    }

    async fn is_current_generation(
        &self,
        name: &str,
        generation: u64,
        config: &McpServerConfig,
    ) -> bool {
        self.lifecycles
            .lock()
            .await
            .get(name)
            .is_some_and(|lifecycle| {
                lifecycle.generation == generation
                    && lifecycle
                        .connection_config
                        .as_ref()
                        .is_some_and(|current| same_connection_config(current, config))
            })
    }

    async fn finish_stale_attempt(&self, name: &str, generation: u64) {
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(lifecycle) = lifecycles.get_mut(name)
            && lifecycle.connecting_generation == Some(generation)
        {
            lifecycle.connecting_generation = None;
            lifecycle.notify.notify_waiters();
        }
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

    /// 构建 stdio 模式的 rmcp service（拉起子进程 + 握手），不含存储逻辑。
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
        let mut cmd =
            crate::infra::platform::no_window_tokio(tokio::process::Command::new(&program));
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

    /// 构建 HTTP 模式的 rmcp service（Streamable HTTP transport + 握手），不含存储逻辑。
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
            url = %redact_url_secrets(url),
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

    /// 构建 SSE 模式的 rmcp service（旧版 SSE transport + 握手），不含存储逻辑。
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
            url = %redact_url_secrets(url),
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

    /// 0.13.8: 从已建立的 service 拉 tool 列表 + 转换为 McpToolInfo（不含存储逻辑）。
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
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(lifecycle) = lifecycles.get_mut(name) {
            lifecycle.generation = lifecycle.generation.saturating_add(1);
            lifecycle.connection_config = None;
            lifecycle.cooldown_until = None;
            lifecycle.last_result = None;
            lifecycle.notify.notify_waiters();
        }
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.remove(name) {
            // Drop service 会触发子进程清理（TokioChildProcess 的 Drop impl 会 kill）
            drop(server);
            tracing::info!(server = %name, "MCP: server 已停止");
            // 仅当实际 remove 了才 bump——没移除则池子没变，bump 会无谓触发重建
            self.bump_epoch();
        }
        self.statuses.write().await.insert(
            name.to_string(),
            McpServerStatus::Offline {
                reason: "手动停止".to_string(),
            },
        );
    }

    /// 配置写库后立即同步运行时代际，保证旧任务在 command 返回前已失效。
    pub async fn apply_config(&self, config: &McpServerConfig) {
        self.register_config(config).await;
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
        let mut lifecycles = self.lifecycles.lock().await;
        if let Some(config) = lifecycles
            .get_mut(name)
            .and_then(|lifecycle| lifecycle.connection_config.as_mut())
        {
            config.disabled_tools = disabled_tools.clone();
        }
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.get_mut(name) {
            let changed = server.config.disabled_tools != disabled_tools;
            server.config.disabled_tools = disabled_tools.clone();
            // 更新 tools 列表中的 disabled 标记
            for tool in &mut server.tools {
                tool.disabled = disabled_tools.contains(&tool.name);
            }
            // disabled_tools 变化改变了喂给 AI 的 tool 池，bump epoch
            if changed {
                self.bump_epoch();
            }
        } else {
            // server 未连接——DB 已更新，运行时缓存无此 server。
            // 下次 prewarm / prompt prepare 会从 DB 加载最新配置，最终一致。
            tracing::debug!(
                server = %name,
                "MCP: update_disabled_tools 时 server 未连接，运行时缓存未更新（下次连接时从 DB 加载）"
            );
        }
    }

    /// 停止所有 server（进程退出时调用）。
    pub async fn stop_all(&self) {
        let mut lifecycles = self.lifecycles.lock().await;
        for lifecycle in lifecycles.values_mut() {
            lifecycle.generation = lifecycle.generation.saturating_add(1);
            lifecycle.connection_config = None;
            lifecycle.cooldown_until = None;
            lifecycle.last_result = None;
            lifecycle.notify.notify_waiters();
        }
        let mut connected = self.connected.write().await;
        let names: Vec<String> = connected.keys().cloned().collect();
        connected.clear();
        if !names.is_empty() {
            self.bump_epoch();
        }
        // Drop 所有 service（触发子进程清理）
        tracing::info!(count = names.len(), "MCP: 所有 server 已停止");
    }
}

impl Default for McpClientManager {
    fn default() -> Self {
        Self::new()
    }
}

fn same_connection_config(left: &McpServerConfig, right: &McpServerConfig) -> bool {
    left.name == right.name
        && left.transport == right.transport
        && left.command == right.command
        && left.args == right.args
        && left.env == right.env
}

/// 脱敏 URL query 中的敏感参数——防止 API key/token 明文进入日志。
///
/// MCP server 配置常把凭证塞在 URL query string 里（如 Tavily 的
/// `?tavilyApiKey=tvly-dev-...`），直接 `%url` 打印会违反「敏感信息永不记日志」
/// 铁则（spec-backend.md §3.2）。这里对参数名命中 key/token/secret/password/auth
/// 的值统一替换为 `***REDACTED***`，保留 scheme/host/path/参数名便于诊断。
///
/// 取舍：关键词匹配宁严勿松——宁可误打码非密钥参数（如 `oauth_scope`，其值是
/// scope 字符串），也不漏放真实凭证。诊断时从参数名仍能看出语义。
fn redact_url_secrets(url: &str) -> String {
    // 无 ? 的 URL（纯 endpoint）无 query，原样返回
    let Some((prefix, query)) = url.split_once('?') else {
        return url.to_string();
    };
    let redacted = query
        .split('&')
        .map(|pair| {
            if let Some((k, v)) = pair.split_once('=') {
                if is_sensitive_param(k) {
                    format!("{k}=***REDACTED***")
                } else {
                    format!("{k}={v}")
                }
            } else {
                // 无 = 的裸参数（如 ?flag）原样保留
                pair.to_string()
            }
        })
        .collect::<Vec<_>>()
        .join("&");
    format!("{prefix}?{redacted}")
}

/// 判断 query 参数名是否疑似敏感凭证（不区分大小写）。
fn is_sensitive_param(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.contains("key")
        || lower.contains("token")
        || lower.contains("secret")
        || lower.contains("password")
        || lower.contains("passwd")
        || lower.contains("auth")
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
        ("cmd".to_string(), all_args)
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
        let manager = McpClientManager::new();
        assert_eq!(
            manager.connect_slots.available_permits(),
            MAX_CONCURRENT_CONNECTS
        );
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
        let result = manager.build_stdio_service(&config).await;
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
            .build_http_service(&config, "", &std::collections::HashMap::new())
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

    fn empty_stdio_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: crate::domain::mcp::config::McpTransport::Stdio,
            command: String::new(),
            args: vec![],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: vec![],
        }
    }

    #[tokio::test]
    async fn concurrent_manual_requests_share_one_connection_attempt() {
        let manager = Arc::new(McpClientManager::new());
        let permits = manager
            .connect_slots
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_CONNECTS as u32)
            .await
            .unwrap();
        let config = empty_stdio_config("single-flight");

        let first_manager = manager.clone();
        let first_config = config.clone();
        let first = tokio::spawn(async move { first_manager.start_server(&first_config).await });
        loop {
            if manager
                .lifecycles
                .lock()
                .await
                .get(&config.name)
                .is_some_and(|lifecycle| lifecycle.connecting_generation.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        let second_manager = manager.clone();
        let second_config = config.clone();
        let second = tokio::spawn(async move { second_manager.start_server(&second_config).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        drop(permits);

        assert!(first.await.unwrap().is_err());
        assert!(second.await.unwrap().is_err());
        assert_eq!(manager.connect_attempts.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn automatic_failure_respects_cooldown_but_manual_retry_bypasses_it() {
        let manager = McpClientManager::new();
        let config = empty_stdio_config("cooldown");

        assert!(
            manager
                .connect_server(&config, ConnectIntent::Automatic)
                .await
                .is_err()
        );
        assert!(
            manager
                .connect_server(&config, ConnectIntent::Automatic)
                .await
                .is_err()
        );
        assert_eq!(manager.connect_attempts.load(Ordering::SeqCst), 1);

        assert!(manager.start_server(&config).await.is_err());
        assert_eq!(manager.connect_attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn configuration_change_invalidates_queued_old_generation() {
        let manager = Arc::new(McpClientManager::new());
        let permits = manager
            .connect_slots
            .clone()
            .acquire_many_owned(MAX_CONCURRENT_CONNECTS as u32)
            .await
            .unwrap();
        let old_config = empty_stdio_config("generation");
        let task_manager = manager.clone();
        let task_config = old_config.clone();
        let old_task = tokio::spawn(async move { task_manager.start_server(&task_config).await });

        loop {
            if manager
                .lifecycles
                .lock()
                .await
                .get(&old_config.name)
                .is_some_and(|lifecycle| lifecycle.connecting_generation.is_some())
            {
                break;
            }
            tokio::task::yield_now().await;
        }

        let mut new_config = old_config.clone();
        new_config.args = vec!["changed".to_string()];
        manager.apply_config(&new_config).await;

        let new_manager = manager.clone();
        let next_config = new_config.clone();
        let new_task = tokio::spawn(async move { new_manager.start_server(&next_config).await });
        tokio::time::sleep(Duration::from_millis(20)).await;
        assert!(
            !new_task.is_finished(),
            "新 generation 必须等待旧 single-flight 释放"
        );
        drop(permits);

        let error = old_task.await.unwrap().unwrap_err();
        assert!(error.contains("配置已变化"), "error was: {error}");
        assert!(new_task.await.unwrap().is_err());
        assert_eq!(manager.connect_attempts.load(Ordering::SeqCst), 1);
        assert!(matches!(
            manager.get_statuses().await.get(&old_config.name),
            Some(McpServerStatus::Offline { .. })
        ));
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
            CapabilitySchema::empty("screenshot", "截屏"),
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

    // ── 0.18.7: URL query 敏感参数脱敏（防 API key 明文进日志）──

    #[test]
    fn redact_url_secrets_masks_api_key() {
        // Tavily 风格：key 在 query
        let url = "https://mcp.tavily.com/mcp/?tavilyApiKey=tvly-dev-SECRET123";
        let redacted = redact_url_secrets(url);
        assert!(
            redacted.contains("tavilyApiKey=***REDACTED***"),
            "key 值应被打码: {redacted}"
        );
        assert!(!redacted.contains("SECRET123"), "明文不得残留: {redacted}");
        // scheme/host/path 保留用于诊断
        assert!(
            redacted.starts_with("https://mcp.tavily.com/mcp/?"),
            "endpoint 应保留: {redacted}"
        );
    }

    #[test]
    fn redact_url_secrets_masks_multiple_sensitive_params() {
        let url =
            "https://api.example.com/sse?token=abc123&refresh=yes&secret=topsecret&X-API-Key=k1";
        let redacted = redact_url_secrets(url);
        assert!(redacted.contains("token=***REDACTED***"));
        assert!(redacted.contains("secret=***REDACTED***"));
        assert!(redacted.contains("X-API-Key=***REDACTED***"));
        // 非敏感参数原样保留
        assert!(redacted.contains("refresh=yes"));
    }

    #[test]
    fn redact_url_secrets_case_insensitive() {
        // 大小写不敏感：KEY / Token / SECRET 都该被打码
        let url = "https://x.com/?KEY=k1&Token=t1&SECRET=s1";
        let redacted = redact_url_secrets(url);
        assert!(redacted.contains("KEY=***REDACTED***"));
        assert!(redacted.contains("Token=***REDACTED***"));
        assert!(redacted.contains("SECRET=***REDACTED***"));
    }

    #[test]
    fn redact_url_secrets_preserves_url_without_query() {
        // 纯 endpoint（无 ?）原样返回
        let url = "https://mcp.context7.com/mcp";
        assert_eq!(redact_url_secrets(url), url);
    }

    #[test]
    fn redact_url_secrets_preserves_bare_flag_params() {
        // 无 = 的裸参数（如 ?flag）原样保留，不 panic
        let url = "https://x.com/api?flag&key=secret";
        let redacted = redact_url_secrets(url);
        assert!(redacted.contains("flag&"));
        assert!(redacted.contains("key=***REDACTED***"));
    }

    #[test]
    fn is_sensitive_param_detects_common_names() {
        for name in &[
            "key",
            "apiKey",
            "api_key",
            "X-API-Key",
            "token",
            "access_token",
            "accessToken",
            "refreshToken",
            "secret",
            "clientSecret",
            "password",
            "passwd",
            "Authorization",
            "auth",
        ] {
            assert!(is_sensitive_param(name), "{name} 应被判定为敏感参数");
        }
    }

    #[test]
    fn is_sensitive_param_rejects_non_sensitive() {
        for name in &["model", "stream", "version", "limit", "url", "page"] {
            assert!(!is_sensitive_param(name), "{name} 不应被判定为敏感参数");
        }
    }
}
