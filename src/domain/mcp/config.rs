//! MCP server 配置管理（0.13.0）——存储在配置库 `config` 表。
//!
//! 每个 MCP server 配置包含：name / command / args / env / enabled / disabled_tools。
//! 配置以 JSON 序列化存储在 `config` 表的 `mcp:servers` key 下。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// MCP 传输协议类型（0.13.6 → 0.13.8）。
///
/// 决定 MCP client 如何连接到外部 server——
/// - `Stdio`：拉起子进程，通过 stdin/stdout 通信
/// - `Sse`：旧版 SSE transport（MCP 2024-11-05 规范）
///   GET `/sse` 建立 SSE 长连接 + POST 到 endpoint URL 发消息
/// - `Http`：Streamable HTTP（MCP 2025-03-26 规范）
///   POST 到单个端点，响应可为 JSON 或 SSE
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum McpTransport {
    /// stdio 子进程。
    Stdio,
    /// 旧版 SSE transport。
    /// `url` 字段必填（SSE 端点），`headers` 可选。
    Sse {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
    },
    /// Streamable HTTP（远程 server）。
    /// `url` 字段必填，`headers` 可选（如 Authorization）。
    Http {
        url: String,
        #[serde(default)]
        headers: std::collections::HashMap<String, String>,
    },
}

impl Default for McpTransport {
    fn default() -> Self {
        Self::Stdio
    }
}

/// 单个 MCP server 的配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerConfig {
    /// 唯一标识（用户自定义名称，如 `"filesystem"`）。
    pub name: String,
    /// 传输协议类型（0.13.6，默认 stdio）。
    #[serde(default)]
    pub transport: McpTransport,
    /// 可执行文件路径或命令（如 `"npx"` / `"node"` / `"C:\\path\\to\\server.exe"`）。
    /// HTTP 模式下可为空字符串。
    #[serde(default)]
    pub command: String,
    /// 命令行参数（如 `["-y", "@modelcontextprotocol/server-filesystem", "C:\\Users"]`）。
    #[serde(default)]
    pub args: Vec<String>,
    /// 环境变量（如 `{"API_KEY": "xxx"}`）。
    #[serde(default)]
    pub env: std::collections::HashMap<String, String>,
    /// 是否启用（启动时是否自动拉起）。
    #[serde(default = "default_enabled")]
    pub enabled: bool,
    /// 被用户取消的 tool 名称列表（tool 粒度开关，控制喂给 AI 的 tool 子集）。
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

fn default_enabled() -> bool {
    true
}

/// MCP server 配置存储——封装配置库 CRUD。
pub struct McpServerConfigStore;

impl McpServerConfigStore {
    /// 配置库 key。
    const KEY: &'static str = "mcp:servers";

    /// 加载所有 MCP server 配置。
    pub async fn load_all(pool: &SqlitePool) -> Result<Vec<McpServerConfig>, McpConfigError> {
        let json = crate::infra::data::config::get_config(pool, Self::KEY).await;
        match json {
            Some(s) if !s.is_empty() => {
                let servers: Vec<McpServerConfig> = serde_json::from_str(&s)
                    .map_err(|e| McpConfigError::Deserialize(e.to_string()))?;
                Ok(servers)
            }
            _ => Ok(Vec::new()),
        }
    }

    /// 保存所有 MCP server 配置（全量覆盖）。
    pub async fn save_all(
        pool: &SqlitePool,
        servers: &[McpServerConfig],
    ) -> Result<(), McpConfigError> {
        let json =
            serde_json::to_string(servers).map_err(|e| McpConfigError::Serialize(e.to_string()))?;
        crate::infra::data::config::set_config(pool, Self::KEY, &json)
            .await
            .map_err(|e| McpConfigError::Db(e.to_string()))?;
        Ok(())
    }

    /// 添加或更新单个 server（按 name 去重）。
    pub async fn upsert(pool: &SqlitePool, config: McpServerConfig) -> Result<(), McpConfigError> {
        let mut servers = Self::load_all(pool).await?;
        if let Some(existing) = servers.iter_mut().find(|s| s.name == config.name) {
            *existing = config;
        } else {
            servers.push(config);
        }
        Self::save_all(pool, &servers).await
    }

    /// 删除单个 server（按 name）。
    pub async fn delete(pool: &SqlitePool, name: &str) -> Result<(), McpConfigError> {
        let mut servers = Self::load_all(pool).await?;
        servers.retain(|s| s.name != name);
        Self::save_all(pool, &servers).await
    }

    /// 更新单个 server 的 enabled 状态。
    pub async fn set_enabled(
        pool: &SqlitePool,
        name: &str,
        enabled: bool,
    ) -> Result<(), McpConfigError> {
        let mut servers = Self::load_all(pool).await?;
        if let Some(s) = servers.iter_mut().find(|s| s.name == name) {
            s.enabled = enabled;
            Self::save_all(pool, &servers).await
        } else {
            Err(McpConfigError::NotFound(name.to_string()))
        }
    }

    /// 更新单个 server 的 disabled_tools 列表（tool 粒度开关）。
    pub async fn set_disabled_tools(
        pool: &SqlitePool,
        name: &str,
        disabled_tools: Vec<String>,
    ) -> Result<(), McpConfigError> {
        let mut servers = Self::load_all(pool).await?;
        if let Some(s) = servers.iter_mut().find(|s| s.name == name) {
            s.disabled_tools = disabled_tools;
            Self::save_all(pool, &servers).await
        } else {
            Err(McpConfigError::NotFound(name.to_string()))
        }
    }
}

/// 配置操作错误。
#[derive(Debug, thiserror::Error)]
pub enum McpConfigError {
    #[error("MCP 配置反序列化失败: {0}")]
    Deserialize(String),
    #[error("MCP 配置序列化失败: {0}")]
    Serialize(String),
    #[error("MCP 配置数据库错误: {0}")]
    Db(String),
    #[error("未找到 MCP server: {0}")]
    NotFound(String),
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    async fn setup_pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:")
            .await
            .expect("failed to create in-memory db");
        sqlx::query("CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)")
            .execute(&pool)
            .await
            .expect("failed to create config table");
        pool
    }

    fn make_config(name: &str) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            transport: McpTransport::Stdio,
            command: "npx".to_string(),
            args: vec![
                "-y".to_string(),
                "@modelcontextprotocol/server-filesystem".to_string(),
            ],
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: Vec::new(),
        }
    }

    #[tokio::test]
    async fn load_all_empty_returns_empty_vec() {
        let pool = setup_pool().await;
        let servers = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert!(servers.is_empty());
    }

    #[tokio::test]
    async fn upsert_and_load_roundtrip() {
        let pool = setup_pool().await;
        let config = make_config("filesystem");
        McpServerConfigStore::upsert(&pool, config.clone())
            .await
            .unwrap();

        let loaded = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "filesystem");
        assert_eq!(loaded[0].command, "npx");
    }

    #[tokio::test]
    async fn upsert_existing_updates_in_place() {
        let pool = setup_pool().await;
        McpServerConfigStore::upsert(&pool, make_config("fs"))
            .await
            .unwrap();

        let mut updated = make_config("fs");
        updated.command = "node".to_string();
        McpServerConfigStore::upsert(&pool, updated).await.unwrap();

        let loaded = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].command, "node");
    }

    #[tokio::test]
    async fn delete_removes_server() {
        let pool = setup_pool().await;
        McpServerConfigStore::upsert(&pool, make_config("a"))
            .await
            .unwrap();
        McpServerConfigStore::upsert(&pool, make_config("b"))
            .await
            .unwrap();

        McpServerConfigStore::delete(&pool, "a").await.unwrap();

        let loaded = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert_eq!(loaded.len(), 1);
        assert_eq!(loaded[0].name, "b");
    }

    #[tokio::test]
    async fn set_enabled_updates_flag() {
        let pool = setup_pool().await;
        McpServerConfigStore::upsert(&pool, make_config("fs"))
            .await
            .unwrap();

        McpServerConfigStore::set_enabled(&pool, "fs", false)
            .await
            .unwrap();

        let loaded = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert_eq!(loaded[0].enabled, false);
    }

    #[tokio::test]
    async fn set_disabled_tools_updates_list() {
        let pool = setup_pool().await;
        McpServerConfigStore::upsert(&pool, make_config("fs"))
            .await
            .unwrap();

        McpServerConfigStore::set_disabled_tools(&pool, "fs", vec!["dangerous_tool".to_string()])
            .await
            .unwrap();

        let loaded = McpServerConfigStore::load_all(&pool).await.unwrap();
        assert_eq!(loaded[0].disabled_tools, vec!["dangerous_tool".to_string()]);
    }

    #[tokio::test]
    async fn set_enabled_not_found_returns_error() {
        let pool = setup_pool().await;
        let result = McpServerConfigStore::set_enabled(&pool, "nonexistent", true).await;
        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), McpConfigError::NotFound(_)));
    }

    #[test]
    fn mcp_server_config_serializes_with_defaults() {
        let json = r#"{"name":"test","command":"echo"}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.name, "test");
        assert_eq!(config.command, "echo");
        assert!(config.args.is_empty());
        assert!(config.env.is_empty());
        assert!(config.enabled); // default true
        assert!(config.disabled_tools.is_empty());
    }

    #[test]
    fn mcp_transport_stdio_roundtrip() {
        let transport = McpTransport::Stdio;
        let json = serde_json::to_string(&transport).unwrap();
        assert_eq!(json, r#"{"type":"stdio"}"#);
        let de: McpTransport = serde_json::from_str(&json).unwrap();
        assert_eq!(transport, de);
    }

    #[test]
    fn mcp_transport_http_roundtrip() {
        let mut headers = std::collections::HashMap::new();
        headers.insert("Authorization".to_string(), "Bearer token123".to_string());
        let transport = McpTransport::Http {
            url: "http://localhost:8000/mcp".to_string(),
            headers,
        };
        let json = serde_json::to_string(&transport).unwrap();
        let de: McpTransport = serde_json::from_str(&json).unwrap();
        assert_eq!(transport, de);
    }

    #[test]
    fn mcp_transport_default_is_stdio() {
        assert_eq!(McpTransport::default(), McpTransport::Stdio);
    }

    #[test]
    fn mcp_server_config_with_http_transport_roundtrip() {
        let config = McpServerConfig {
            name: "remote-api".to_string(),
            transport: McpTransport::Http {
                url: "http://example.com/mcp".to_string(),
                headers: std::collections::HashMap::new(),
            },
            command: String::new(),
            args: Vec::new(),
            env: std::collections::HashMap::new(),
            enabled: true,
            disabled_tools: Vec::new(),
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: McpServerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, de);
        assert!(de.command.is_empty()); // HTTP 模式 command 可为空
    }

    #[test]
    fn mcp_server_config_without_transport_defaults_to_stdio() {
        // 旧配置（无 transport 字段）应默认为 Stdio
        let json = r#"{"name":"legacy","command":"npx"}"#;
        let config: McpServerConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.transport, McpTransport::Stdio);
    }
}
