//! MCP server 配置管理（0.13.4）——控制 Blink 作为 MCP server 的行为。
//!
//! 配置存储在配置库 `config` 表的 `mcp:server` key 下，包含：
//! - `enabled`：总开关
//! - `port`：Streamable HTTP 监听端口（0.19.13，默认 32123）
//! - `exposed_capabilities`：允许暴露给外部 client 的 Capability id 列表
//! - `exposure_seeded`：默认暴露集合是否已生成（0.21.10）
//!
//! **暴露策略**：0.21.10 起"无风险"能力（非 Dangerous、非 sensitive、代码允许
//! Mcp 来源且非 Forbidden）在首次启动时静默进入默认暴露集合（与 AI 推荐
//! allowlist 的首次生成同构）；用户此后可自由增删，清空也不再自动重新生成。
//! Dangerous / Forbidden 项仍永远被 `ExposureSnapshot` 代码级过滤。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

use crate::domain::capability::{CapabilityPolicy, CapabilityRegistry};

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
    /// 默认暴露集合是否已生成过（0.21.10）。
    /// 区分"从未配置"与"用户清空"——后者不再自动重新生成默认集合。
    #[serde(default)]
    pub exposure_seeded: bool,
}

impl Default for McpServerModeConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            port: DEFAULT_MCP_SERVER_PORT,
            exposed_capabilities: Vec::new(),
            exposure_seeded: false,
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

    /// 0.21.10：是否需要首次生成默认暴露集合——从未生成且列表为空。
    pub fn needs_initial_generation(config: &McpServerModeConfig) -> bool {
        !config.exposure_seeded && config.exposed_capabilities.is_empty()
    }

    /// 0.21.10：生成默认暴露集合——"无风险"能力（对齐能力目录风险列的空态）。
    ///
    /// **规则**：允许 Mcp 来源 + 非 Dangerous + 非 sensitive + 非 Forbidden。
    /// Dangerous / Forbidden 即使被用户手动加入也会被 ExposureSnapshot 过滤，
    /// 默认集合自然也不包含它们。与 AI 推荐 allowlist（ai_capability_access）
    /// 的生成规则同构。
    pub fn generate_default_exposure(registry: &CapabilityRegistry) -> Vec<String> {
        let mut ids: Vec<String> = registry
            .entries()
            .into_iter()
            .filter(|(_, cap)| Self::is_safe_default(&cap.policy()))
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        ids
    }

    /// 判断单个 Capability 的 policy 是否进入默认暴露集合（独立方法供单测）。
    pub fn is_safe_default(policy: &CapabilityPolicy) -> bool {
        use crate::domain::capability::{DangerClass, InvocationOrigin, McpDefault};

        if !policy.allows_origin(InvocationOrigin::Mcp) {
            return false;
        }
        if policy.danger == DangerClass::Dangerous {
            return false;
        }
        if policy.sensitive {
            return false;
        }
        if policy.mcp_default == McpDefault::Forbidden {
            return false;
        }
        true
    }

    /// 0.21.10：首次启动静默生成并持久化默认暴露集合（若需要）。
    ///
    /// 与 AI allowlist 的 `ensure_recommended` 同构：生成后置 `exposure_seeded`，
    /// 用户此后清空列表也不会自动重新生成。
    pub async fn ensure_default_exposure(
        pool: &SqlitePool,
        registry: &CapabilityRegistry,
    ) -> McpServerModeConfig {
        let mut config = match Self::load(pool).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "MCP server 配置加载失败，跳过默认暴露生成");
                return McpServerModeConfig::default();
            }
        };
        // 旧版本已有显式暴露列表但没有 seeded 字段：保留原列表，只补迁移标记。
        // 否则用户之后清空列表，下一次启动会被误判为“从未生成”而重新填充。
        if !config.exposure_seeded && !config.exposed_capabilities.is_empty() {
            config.exposure_seeded = true;
            if let Err(e) = Self::save(pool, &config).await {
                tracing::warn!(error = %e, "McpServerModeConfig: 已有暴露列表的种子标记持久化失败");
            }
            return config;
        }
        if !Self::needs_initial_generation(&config) {
            return config;
        }

        let ids = Self::generate_default_exposure(registry);
        tracing::info!(
            count = ids.len(),
            "McpServerModeConfig: 首次生成默认暴露集合（无风险能力，静默）"
        );
        config.exposed_capabilities = ids;
        config.exposure_seeded = true;
        if let Err(e) = Self::save(pool, &config).await {
            tracing::warn!(error = %e, "McpServerModeConfig: 默认暴露集合持久化失败，使用内存配置");
        }
        config
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
            exposure_seeded: false,
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
            exposure_seeded: false,
        };
        McpServerModeConfigStore::save(&pool, &config1)
            .await
            .unwrap();

        let config2 = McpServerModeConfig {
            enabled: false,
            port: 32124,
            exposed_capabilities: vec!["cap_b".into(), "cap_c".into()],
            exposure_seeded: false,
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
            exposure_seeded: false,
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
            exposure_seeded: false,
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

    // ── 0.21.10 默认暴露集合 ─────────────────────────────────────────────────

    #[test]
    fn safe_default_exposure_policy_rules() {
        use crate::domain::capability::{DangerClass, McpDefault, OriginSet};

        // 默认 policy：ALL 来源 + Safe + 非 sensitive + DefaultOff → 进默认集合
        let base = CapabilityPolicy::default();
        assert!(McpServerModeConfigStore::is_safe_default(&base));

        // 不允许 Mcp 来源（仅本地）→ 不进
        let mut p = base.clone();
        p.allowed_origins = OriginSet::ALL_LOCAL;
        assert!(!McpServerModeConfigStore::is_safe_default(&p));

        // Dangerous → 不进（代码级还会被 ExposureSnapshot 二次过滤）
        let mut p = base.clone();
        p.danger = DangerClass::Dangerous;
        assert!(!McpServerModeConfigStore::is_safe_default(&p));

        // sensitive（敏感读取）→ 不进
        let mut p = base.clone();
        p.sensitive = true;
        assert!(!McpServerModeConfigStore::is_safe_default(&p));

        // MCP 代码级禁止 → 不进
        let mut p = base.clone();
        p.mcp_default = McpDefault::Forbidden;
        assert!(!McpServerModeConfigStore::is_safe_default(&p));
    }

    #[test]
    fn needs_initial_generation_rules() {
        // 从未配置（默认值）→ 需要
        assert!(McpServerModeConfigStore::needs_initial_generation(
            &McpServerModeConfig::default()
        ));

        // 已生成后用户清空 → 不再自动重新生成
        let cleared = McpServerModeConfig {
            exposure_seeded: true,
            ..Default::default()
        };
        assert!(!McpServerModeConfigStore::needs_initial_generation(
            &cleared
        ));

        // 已有显式列表 → 不需要
        let mut customized = McpServerModeConfig::default();
        customized.exposed_capabilities = vec!["cap_a".to_string()];
        assert!(!McpServerModeConfigStore::needs_initial_generation(
            &customized
        ));
    }

    #[tokio::test]
    async fn ensure_default_exposure_seeds_once_and_persists() {
        let pool = setup_pool().await;
        // default registry 经 inventory 收集真实内置能力，安全集非空
        let registry = CapabilityRegistry::default();

        let config = McpServerModeConfigStore::ensure_default_exposure(&pool, &registry).await;
        assert!(config.exposure_seeded);
        assert!(!config.exposed_capabilities.is_empty());

        // 持久化后重读仍带 seeded 标记
        let reloaded = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(reloaded.exposure_seeded);

        // 用户清空后再次 ensure：不重新生成（seeded 已置位）
        let mut cleared = reloaded;
        cleared.exposed_capabilities = vec![];
        McpServerModeConfigStore::save(&pool, &cleared)
            .await
            .unwrap();
        let again = McpServerModeConfigStore::ensure_default_exposure(&pool, &registry).await;
        assert!(again.exposed_capabilities.is_empty());
    }

    #[tokio::test]
    async fn ensure_default_exposure_marks_existing_list_as_seeded_without_replacing_it() {
        let pool = setup_pool().await;
        let registry = CapabilityRegistry::default();
        let existing = McpServerModeConfig {
            exposed_capabilities: vec!["open_url".to_string()],
            exposure_seeded: false,
            ..Default::default()
        };
        McpServerModeConfigStore::save(&pool, &existing)
            .await
            .unwrap();

        let migrated = McpServerModeConfigStore::ensure_default_exposure(&pool, &registry).await;
        assert!(migrated.exposure_seeded);
        assert_eq!(migrated.exposed_capabilities, vec!["open_url"]);

        let persisted = McpServerModeConfigStore::load(&pool).await.unwrap();
        assert!(persisted.exposure_seeded);
        assert_eq!(persisted.exposed_capabilities, vec!["open_url"]);
    }
}
