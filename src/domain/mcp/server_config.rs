//! MCP server 配置管理（0.13.4）——控制 Blink 作为 MCP server 的行为。
//!
//! 配置存储在配置库 `config` 表的 `mcp:server` key 下，包含：
//! - `enabled`：总开关
//! - `exposed_capabilities`：允许暴露给外部 client 的 Capability id 列表
//!
//! **暴露策略**（§8.4）：不是所有 Capability 都适合暴露。用户在设置页勾选要暴露的能力。
//! 默认不暴露任何能力（安全优先），用户显式开启后才暴露。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

/// MCP server 配置。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct McpServerModeConfig {
    /// 总开关——启用后 Blink 作为 MCP server 暴露能力给外部 client。
    #[serde(default)]
    pub enabled: bool,
    /// 允许暴露的 Capability id 列表。
    /// 只有在此列表中的能力才会出现在 tool 列表中。
    #[serde(default)]
    pub exposed_capabilities: Vec<String>,
}

impl Default for McpServerModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            exposed_capabilities: Vec::new(),
        }
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
            Some(s) if !s.is_empty() => serde_json::from_str(&s).map_err(|e| e.to_string()),
            _ => Ok(McpServerModeConfig::default()),
        }
    }

    /// 保存配置。
    pub async fn save(pool: &SqlitePool, config: &McpServerModeConfig) -> Result<(), String> {
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
    }

    #[tokio::test]
    async fn load_returns_default_when_missing() {
        let pool = setup_pool().await;
        let config = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(!config.enabled);
        assert!(config.exposed_capabilities.is_empty());
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let pool = setup_pool().await;
        let config = McpServerModeConfig {
            enabled: true,
            exposed_capabilities: vec!["capture_screen".into(), "search_files".into()],
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
            exposed_capabilities: vec!["cap_a".into()],
        };
        McpServerModeConfigStore::save(&pool, &config1)
            .await
            .unwrap();

        let config2 = McpServerModeConfig {
            enabled: false,
            exposed_capabilities: vec!["cap_b".into(), "cap_c".into()],
        };
        McpServerModeConfigStore::save(&pool, &config2)
            .await
            .unwrap();

        let loaded = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(!loaded.enabled);
        assert_eq!(loaded.exposed_capabilities, vec!["cap_b", "cap_c"]);
    }

    #[test]
    fn config_deserializes_with_defaults() {
        // 空 JSON 应该用默认值
        let json = r#"{}"#;
        let config: McpServerModeConfig = serde_json::from_str(json).unwrap();
        assert!(!config.enabled);
        assert!(config.exposed_capabilities.is_empty());
    }
}
