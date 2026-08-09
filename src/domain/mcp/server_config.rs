//! MCP server 配置管理（0.13.4）——控制 Blink 作为 MCP server 的行为。
//!
//! 配置存储在配置库 `config` 表的 `mcp:server` key 下，包含：
//! - `enabled`：总开关
//! - `port`：Streamable HTTP 监听端口（0.19.13，默认 32123）
//! - `exposed_capabilities`：允许暴露给外部 client 的 Capability id 列表
//!
//! **暴露策略**（§8.4）：不是所有 Capability 都适合暴露。用户在设置页勾选要暴露的能力。
//! 默认不暴露任何能力（安全优先），用户显式开启后才暴露。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// MCP server 默认端口。
pub const DEFAULT_MCP_SERVER_PORT: u16 = 32123;

/// 合法端口范围下限（IANA 注册端口起始）。
pub const MIN_PORT: u16 = 1024;
/// 合法端口范围上限。
pub const MAX_PORT: u16 = 65535;

fn default_mcp_server_port() -> u16 {
    DEFAULT_MCP_SERVER_PORT
}

/// MCP server 配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerModeConfig {
    /// 总开关——启用后 Blink 作为 MCP server 暴露能力给外部 client。
    #[serde(default)]
    pub enabled: bool,
    /// Streamable HTTP 监听端口（0.19.13）。
    /// 旧 JSON 缺少此字段时自动使用 `DEFAULT_MCP_SERVER_PORT`。
    #[serde(default = "default_mcp_server_port")]
    pub port: u16,
    /// 允许暴露的 Capability id 列表。
    /// 只有在此列表中的能力才会出现在 tool 列表中。
    #[serde(default)]
    pub exposed_capabilities: Vec<String>,
}

impl Default for McpServerModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_MCP_SERVER_PORT,
            exposed_capabilities: Vec::new(),
        }
    }
}

impl McpServerModeConfig {
    /// 校验端口是否在合法范围内。
    pub fn is_port_valid(port: u16) -> bool {
        (MIN_PORT..=MAX_PORT).contains(&port)
    }
}

/// MCP server 配置存储——封装配置库读写。
pub struct McpServerModeConfigStore;

impl McpServerModeConfigStore {
    const KEY: &'static str = "mcp:server";

    /// 加载配置。缺失时返回默认值。
    pub async fn load(pool: &SqlitePool) -> Result<McpServerModeConfig, String> {
        let json = crate::infra::data::config::get_config(pool, Self::KEY).await;
        match json {
            Some(s) if !s.is_empty() => {
                let config: McpServerModeConfig =
                    serde_json::from_str(&s).map_err(|e| e.to_string())?;
                // 校验端口范围——非法时回退到默认值
                if !McpServerModeConfig::is_port_valid(config.port) {
                    tracing::warn!(
                        port = config.port,
                        "MCP server 配置端口非法，回退到默认值 {}",
                        DEFAULT_MCP_SERVER_PORT
                    );
                    Ok(McpServerModeConfig {
                        port: DEFAULT_MCP_SERVER_PORT,
                        ..config
                    })
                } else {
                    Ok(config)
                }
            }
            _ => Ok(McpServerModeConfig::default()),
        }
    }

    /// 保存配置。保存前校验端口范围。
    pub async fn save(pool: &SqlitePool, config: &McpServerModeConfig) -> Result<(), String> {
        if !McpServerModeConfig::is_port_valid(config.port) {
            return Err(format!(
                "端口 {} 不在合法范围 {}..={} 内",
                config.port, MIN_PORT, MAX_PORT
            ));
        }
        let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
        crate::infra::data::config::set_config(pool, Self::KEY, &json)
            .await
            .map_err(|e| e.to_string())
    }
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

    #[test]
    fn default_is_disabled_and_empty() {
        let config = McpServerModeConfig::default();
        assert!(!config.enabled);
        assert!(config.exposed_capabilities.is_empty());
        assert_eq!(config.port, DEFAULT_MCP_SERVER_PORT);
    }

    #[test]
    fn default_port_is_32123() {
        assert_eq!(DEFAULT_MCP_SERVER_PORT, 32123);
        assert_eq!(McpServerModeConfig::default().port, 32123);
    }

    #[test]
    fn port_validation_accepts_valid_range() {
        assert!(!McpServerModeConfig::is_port_valid(0));
        assert!(!McpServerModeConfig::is_port_valid(80));
        assert!(!McpServerModeConfig::is_port_valid(1023));
        assert!(McpServerModeConfig::is_port_valid(1024));
        assert!(McpServerModeConfig::is_port_valid(32123));
        assert!(McpServerModeConfig::is_port_valid(65535));
        // 65535 是 u16 上限，没有更大的值可测
    }

    #[test]
    fn port_validation_rejects_invalid() {
        assert!(!McpServerModeConfig::is_port_valid(0));
        assert!(!McpServerModeConfig::is_port_valid(80));
        assert!(!McpServerModeConfig::is_port_valid(1023));
    }

    #[tokio::test]
    async fn load_returns_default_when_missing() {
        let pool = setup_pool().await;
        let config = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(!config.enabled);
        assert!(config.exposed_capabilities.is_empty());
        assert_eq!(config.port, DEFAULT_MCP_SERVER_PORT);
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let pool = setup_pool().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["screenshot".into(), "search_files".into()],
        };
        McpServerModeConfigStore::save(&pool, &config)
            .await
            .unwrap();

        let loaded = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert_eq!(loaded, config);
    }

    #[tokio::test]
    async fn save_overwrites_previous() {
        let pool = setup_pool().await;
        let config1 = McpServerModeConfig {
            enabled: true,
            port: 32123,
            exposed_capabilities: vec!["cap_a".into()],
        };
        McpServerModeConfigStore::save(&pool, &config1)
            .await
            .unwrap();

        let config2 = McpServerModeConfig {
            enabled: false,
            port: 32124,
            exposed_capabilities: vec!["cap_b".into(), "cap_c".into()],
        };
        McpServerModeConfigStore::save(&pool, &config2)
            .await
            .unwrap();

        let loaded = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.port, 32124);
        assert_eq!(loaded.exposed_capabilities, vec!["cap_b", "cap_c"]);
    }

    #[test]
    fn config_deserializes_with_defaults() {
        // 空 JSON 应该用默认值
        let json = r#"{}"#;
        let config: McpServerModeConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert!(config.exposed_capabilities.is_empty());
        assert_eq!(config.port, DEFAULT_MCP_SERVER_PORT);
    }

    #[test]
    fn old_config_without_port_uses_default() {
        // 旧 JSON（缺少 port 字段）应自动使用默认端口
        let json = r#"{"enabled":true,"exposed_capabilities":["screenshot"]}"#;
        let config: McpServerModeConfig = serde_json::from_str(json).unwrap();
        assert!(config.enabled);
        assert_eq!(config.exposed_capabilities, vec!["screenshot"]);
        assert_eq!(config.port, DEFAULT_MCP_SERVER_PORT);
    }

    #[test]
    fn config_with_custom_port_roundtrip() {
        let config = McpServerModeConfig {
            enabled: true,
            port: 8080,
            exposed_capabilities: vec!["cap".into()],
        };
        let json = serde_json::to_string(&config).unwrap();
        let de: McpServerModeConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(config, de);
        assert_eq!(de.port, 8080);
    }

    #[tokio::test]
    async fn save_rejects_invalid_port() {
        let pool = setup_pool().await;
        let config = McpServerModeConfig {
            enabled: true,
            port: 80, // 非法端口
            exposed_capabilities: vec![],
        };
        let result = McpServerModeConfigStore::save(&pool, &config).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.contains("端口"));
        assert!(err.contains("80"));
    }

    #[tokio::test]
    async fn load_falls_back_to_default_on_invalid_port() {
        let pool = setup_pool().await;
        // 直接写入非法端口的 JSON
        let json = r#"{"enabled":true,"port":80,"exposed_capabilities":[]}"#;
        crate::infra::data::config::set_config(&pool, "mcp:server", json)
            .await
            .unwrap();
        let config = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert_eq!(config.port, DEFAULT_MCP_SERVER_PORT);
        assert!(config.enabled);
    }
}
