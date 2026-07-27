//! MCP client 编排（0.13.0）——拉起外部 server 子进程 + 握手 + 拉 tool 列表。
//!
//! ## 架构
//!
//! `McpClientManager` 持有所有已连接的 MCP server。每个 server 是一个 stdio 子进程，
//! 通过 `rmcp::ServiceExt::serve()` 建立连接，`peer().list_all_tools()` 拉 tool 列表。
//!
//! ## 故障降级
//!
//! - server 启动失败 → 重试 3 次（间隔 2s），仍失败则跳过，不阻塞其他 server
//! - server 崩溃 → 标灰，从 tool 池剔除，UI 提示
//! - 手动重连：`reconnect_server()` 重新拉起子进程
//!
//! ## tool 可见性
//!
//! `collect_tools()` 时过滤 `config.disabled_tools` 中的 tool，只把用户启用的喂给 AI。

use std::collections::HashMap;
use std::sync::Arc;

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
pub struct McpClientManager {
    /// 已连接的 server（name → ConnectedServer）。
    connected: Arc<RwLock<HashMap<String, ConnectedServer>>>,
    /// 各 server 的状态（供前端查询）。
    statuses: Arc<RwLock<HashMap<String, McpServerStatus>>>,
}

/// 启动重试次数。
const MAX_START_RETRIES: usize = 3;
/// 重试间隔（秒）。
const RETRY_INTERVAL_SECS: u64 = 2;

impl McpClientManager {
    /// 构造空管理器。
    pub fn new() -> Self {
        Self {
            connected: Arc::new(RwLock::new(HashMap::new())),
            statuses: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// 启动所有已配置且 enabled 的 MCP server。
    ///
    /// 从配置库加载 server 列表，逐个拉起子进程并握手。
    /// 单个 server 失败不阻塞其他——记 Offline 状态，继续下一个。
    pub async fn start_all(&self, config_pool: &sqlx::SqlitePool) {
        let configs = match McpServerConfigStore::load_all(config_pool).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "MCP: 加载 server 配置失败，跳过全部 MCP server");
                return;
            }
        };

        let enabled: Vec<_> = configs.into_iter().filter(|c| c.enabled).collect();
        tracing::info!(
            total = enabled.len(),
            "MCP: 开始启动已配置的外部 server"
        );

        for config in enabled {
            // 不阻塞——单个 server 启动失败只记状态
            if let Err(e) = self.start_server(&config).await {
                tracing::warn!(
                    server = %config.name,
                    error = %e,
                    "MCP: server 启动失败（已重试 {MAX_START_RETRIES} 次）"
                );
            }
        }
    }

    /// 启动单个 MCP server（含重试）。
    pub async fn start_server(&self, config: &McpServerConfig) -> Result<(), String> {
        self.set_status(&config.name, McpServerStatus::Connecting)
            .await;

        let mut last_err = String::new();
        for attempt in 1..=MAX_START_RETRIES {
            tracing::debug!(
                server = %config.name,
                attempt,
                max = MAX_START_RETRIES,
                "MCP: 尝试启动 server"
            );
            match self.try_connect(config).await {
                Ok(tools) => {
                    let tool_count = tools.len();
                    tracing::info!(
                        server = %config.name,
                        tools = tool_count,
                        "MCP: server 已连接"
                    );
                    self.set_status(
                        &config.name,
                        McpServerStatus::Online { tool_count },
                    )
                    .await;
                    return Ok(());
                }
                Err(e) => {
                    last_err = e;
                    tracing::debug!(
                        server = %config.name,
                        attempt,
                        error = %last_err,
                        "MCP: 启动失败，等待重试"
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

    /// 尝试连接单个 server（拉起子进程 + 握手 + 拉 tool 列表）。
    async fn try_connect(&self, config: &McpServerConfig) -> Result<Vec<McpToolInfo>, String> {
        // 构造子进程命令
        let mut cmd = tokio::process::Command::new(&config.command);
        cmd.args(&config.args);
        for (k, v) in &config.env {
            cmd.env(k, v);
        }

        // 创建 stdio transport
        let transport = rmcp::transport::TokioChildProcess::new(cmd)
            .map_err(|e| format!("子进程启动失败: {e}"))?;

        // 用 rmcp 默认 ClientInfo 建立 client 连接
        let client_info = ClientInfo::default();
        let service = client_info
            .serve(transport)
            .await
            .map_err(|e| format!("MCP 握手失败: {e}"))?;

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

    /// 停止单个 server（断开连接 + 杀子进程）。
    pub async fn stop_server(&self, name: &str) {
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.remove(name) {
            // Drop service 会触发子进程清理（TokioChildProcess 的 Drop impl 会 kill）
            drop(server);
            tracing::info!(server = %name, "MCP: server 已停止");
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

        tracing::info!(
            total_tools = tools.len(),
            "MCP: tool 池收集完成"
        );
        tools
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

    /// 更新单个 server 的 disabled_tools 并刷新 tool 列表缓存。
    pub async fn update_disabled_tools(
        &self,
        name: &str,
        disabled_tools: Vec<String>,
    ) {
        let mut connected = self.connected.write().await;
        if let Some(server) = connected.get_mut(name) {
            server.config.disabled_tools = disabled_tools.clone();
            // 更新 tools 列表中的 disabled 标记
            for tool in &mut server.tools {
                tool.disabled = disabled_tools.contains(&tool.name);
            }
        }
    }

    /// 停止所有 server（进程退出时调用）。
    pub async fn stop_all(&self) {
        let mut connected = self.connected.write().await;
        let names: Vec<String> = connected.keys().cloned().collect();
        connected.clear();
        drop(connected);
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
}
