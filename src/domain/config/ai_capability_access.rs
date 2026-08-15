//! AI Capability 出口授权存储（0.21.5）。
//!
//! 负责 `AiCapabilityAccessConfig` 的加载、保存和推荐集合生成。
//!
//! ## 推荐集合生成规则（§3.4）
//!
//! 首次升级且不存在 AI 单项配置时，生成并持久化"推荐" allowlist：
//! - 代码允许 `LocalAi` 且 `DangerClass::Safe` 的普通生产能力默认开启。
//! - Safe + sensitive 的普通读取能力也默认开启，但调用时仍走 sensitive 确认。
//! - Dangerous、仅本地、诊断信息采集和诊断恢复类默认关闭。
//! - 用户修改后以持久化的 capability id 集合为真源。
//!
//! ## 存储落点
//!
//! `blink_config.db` KV 分片，key `ai.capability_access`，
//! 走 `ConfigStore<T>` + `blink://config-changed` 广播。

use sqlx::SqlitePool;

use super::shards::AiCapabilityAccessConfig;
use super::store::ConfigStore;
use crate::domain::capability::{
    AiDefault, CapabilityPolicy, CapabilityRegistry, DangerClass, InvocationOrigin,
};

/// AI Capability 出口授权存储——封装配置库读写 + 推荐集合生成。
pub struct AiCapabilityAccessStore;

impl AiCapabilityAccessStore {
    const KEY: &'static str = "ai.capability_access";

    /// 加载配置。缺失时返回默认值（空 enabled_capabilities）。
    ///
    /// 调用方应在加载后检查 `profile == "recommended"` 且 `enabled_capabilities` 为空，
    /// 以判断是否需要首次生成推荐集合（`generate_recommended`）。
    pub async fn load(pool: &SqlitePool) -> AiCapabilityAccessConfig {
        ConfigStore::get::<AiCapabilityAccessConfig>(pool).await
    }

    /// 保存配置。
    pub async fn save(
        pool: &SqlitePool,
        config: &AiCapabilityAccessConfig,
    ) -> Result<(), String> {
        ConfigStore::set::<AiCapabilityAccessConfig>(pool, config)
            .await
            .map_err(|e| e.to_string())
    }

    /// 判断配置是否需要首次生成推荐集合。
    ///
    /// 条件：`enabled_capabilities` 为空且 `profile == "recommended"`。
    /// 一旦生成并持久化，后续即使清空也不自动重新生成（用户可手动重置）。
    pub fn needs_initial_generation(config: &AiCapabilityAccessConfig) -> bool {
        config.enabled_capabilities.is_empty() && config.profile == "recommended"
    }

    /// 生成推荐 allowlist——按 §3.4 规则从 CapabilityRegistry 筛选。
    ///
    /// **规则**：
    /// - `policy.allowed_origins` 包含 `LocalAi`（代码允许 AI 调用）
    /// - `policy.danger == Safe`（非 Dangerous）
    /// - `policy.ai_default == On`（代码级推荐开启）
    ///
    /// 排除项（即使满足上述条件也不自动进入）：
    /// - `DangerClass::Dangerous`（默认关闭）
    /// - `ai_default == Off`（诊断类、local-only 等默认关闭）
    /// - 诊断信息采集和诊断恢复类（`ai_default == Off` 已覆盖）
    pub fn generate_recommended(registry: &CapabilityRegistry) -> Vec<String> {
        let mut ids: Vec<String> = registry
            .entries()
            .into_iter()
            .filter(|(_, cap)| {
                let policy = cap.policy();
                Self::is_recommended(&policy)
            })
            .map(|(id, _)| id)
            .collect();
        ids.sort();
        ids
    }

    /// 判断单个 Capability 的 policy 是否符合推荐集合条件。
    ///
    /// 提取为独立方法供单测使用。
    pub fn is_recommended(policy: &CapabilityPolicy) -> bool {
        // 必须允许 LocalAi 来源
        if !policy.allows_origin(InvocationOrigin::LocalAi) {
            return false;
        }
        // Dangerous 默认关闭
        if policy.danger == DangerClass::Dangerous {
            return false;
        }
        // ai_default == Off 的默认关闭（诊断/local-only 等）
        if policy.ai_default == AiDefault::Off {
            return false;
        }
        true
    }

    /// 首次生成推荐集合并持久化（若需要）。
    ///
    /// 如果配置已存在（`needs_initial_generation` 返回 false），不做任何操作。
    /// 返回最终的 `AiCapabilityAccessConfig`（可能是已有的或新生成的）。
    ///
    /// **静默生成**：不做用户提示（当前用户量少，静默生成）。
    pub async fn ensure_recommended(
        pool: &SqlitePool,
        registry: &CapabilityRegistry,
    ) -> AiCapabilityAccessConfig {
        let config = Self::load(pool).await;
        if !Self::needs_initial_generation(&config) {
            return config;
        }

        // 首次生成推荐集合
        let recommended = Self::generate_recommended(registry);
        tracing::info!(
            count = recommended.len(),
            "AiCapabilityAccess: 首次生成推荐 allowlist（静默）"
        );

        let new_config = AiCapabilityAccessConfig {
            schema_version: 1,
            profile: "recommended".to_string(),
            enabled_capabilities: recommended,
        };

        if let Err(e) = Self::save(pool, &new_config).await {
            tracing::warn!(error = %e, "AiCapabilityAccess: 推荐集合持久化失败，使用内存配置");
        }

        new_config
    }

    /// 获取当前允许的 Capability id 集合（HashSet）。
    ///
    /// 供 `build_agent_tools` 过滤 tool 池使用。
    pub async fn enabled_set(pool: &SqlitePool) -> std::collections::HashSet<String> {
        let config = Self::load(pool).await;
        config.enabled_capabilities.into_iter().collect()
    }

    /// 更新单个 Capability 的启用状态。
    ///
    /// 返回更新后的配置。调用方负责广播 `blink://config-changed`。
    pub async fn toggle_capability(
        pool: &SqlitePool,
        capability_id: &str,
        enabled: bool,
    ) -> Result<AiCapabilityAccessConfig, String> {
        let mut config = Self::load(pool).await;
        if enabled {
            if !config.enabled_capabilities.contains(&capability_id.to_string()) {
                config.enabled_capabilities.push(capability_id.to_string());
            }
        } else {
            config.enabled_capabilities.retain(|id| id != capability_id);
        }
        Self::save(pool, &config).await?;
        Ok(config)
    }

    /// 批量更新 Capability 启用状态。
    ///
    /// 返回更新后的配置。调用方负责广播 `blink://config-changed`。
    pub async fn toggle_capabilities(
        pool: &SqlitePool,
        ids_with_enabled: &[(String, bool)],
    ) -> Result<AiCapabilityAccessConfig, String> {
        let mut config = Self::load(pool).await;
        for (capability_id, enabled) in ids_with_enabled {
            if *enabled {
                if !config.enabled_capabilities.contains(capability_id) {
                    config.enabled_capabilities.push(capability_id.clone());
                }
            } else {
                config.enabled_capabilities.retain(|id| id != capability_id);
            }
        }
        Self::save(pool, &config).await?;
        Ok(config)
    }

    /// 重置为推荐集合（用户在设置页点"恢复默认"时调用）。
    ///
    /// 重新生成推荐集合并覆盖当前配置。`profile` 保持为 `"recommended"`。
    pub async fn reset_to_recommended(
        pool: &SqlitePool,
        registry: &CapabilityRegistry,
    ) -> Result<AiCapabilityAccessConfig, String> {
        let recommended = Self::generate_recommended(registry);
        let config = AiCapabilityAccessConfig {
            schema_version: 1,
            profile: "recommended".to_string(),
            enabled_capabilities: recommended,
        };
        Self::save(pool, &config).await?;
        Ok(config)
    }
}

// ── 单测 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::{
        CapabilityPolicy, CapabilityRegistry, ConfirmationPolicy, DangerClass, InvocationOrigin,
        OriginSet, RuntimeRequirement, AiDefault, McpDefault,
    };
    use crate::domain::config::shards::AiCapabilityAccessConfig;
    use crate::domain::config::store::ConfigKey;
    use sqlx::sqlite::SqlitePoolOptions;

    /// 创建内存 SQLite 池 + config 表。
    async fn test_pool() -> SqlitePool {
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .expect("in-memory pool");
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at INTEGER NOT NULL)",
        )
        .execute(&pool)
        .await
        .expect("create config table");
        pool
    }

    // ── is_recommended 单测 ──────────────────────────────────────────────

    #[test]
    fn safe_ai_default_on_allows_ai_is_recommended() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            danger: DangerClass::Safe,
            ai_default: AiDefault::On,
            ..Default::default()
        };
        assert!(AiCapabilityAccessStore::is_recommended(&policy));
    }

    #[test]
    fn dangerous_is_not_recommended() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            danger: DangerClass::Dangerous,
            ai_default: AiDefault::On, // 即使 On 也不推荐 Dangerous
            confirmation: ConfirmationPolicy::dangerous(true),
            ..Default::default()
        };
        assert!(!AiCapabilityAccessStore::is_recommended(&policy));
    }

    #[test]
    fn ai_default_off_is_not_recommended() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            danger: DangerClass::Safe,
            ai_default: AiDefault::Off, // 诊断/local-only
            ..Default::default()
        };
        assert!(!AiCapabilityAccessStore::is_recommended(&policy));
    }

    #[test]
    fn no_ai_origin_is_not_recommended() {
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::CLI, // 不含 LocalAi
            danger: DangerClass::Safe,
            ai_default: AiDefault::On,
            ..Default::default()
        };
        assert!(!AiCapabilityAccessStore::is_recommended(&policy));
    }

    #[test]
    fn safe_sensitive_ai_on_is_recommended() {
        // Safe + sensitive + ai_default On 仍推荐（调用时走 sensitive 确认）
        let policy = CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            danger: DangerClass::Safe,
            sensitive: true,
            ai_default: AiDefault::On,
            confirmation: ConfirmationPolicy::sensitive(),
            ..Default::default()
        };
        assert!(AiCapabilityAccessStore::is_recommended(&policy));
    }

    // ── needs_initial_generation 单测 ────────────────────────────────────

    #[test]
    fn needs_generation_when_empty_and_recommended() {
        let config = AiCapabilityAccessConfig::default();
        assert!(AiCapabilityAccessStore::needs_initial_generation(&config));
    }

    #[test]
    fn no_generation_when_has_enabled() {
        let config = AiCapabilityAccessConfig {
            enabled_capabilities: vec!["open_url".into()],
            ..Default::default()
        };
        assert!(!AiCapabilityAccessStore::needs_initial_generation(&config));
    }

    #[test]
    fn no_generation_when_profile_not_recommended() {
        let config = AiCapabilityAccessConfig {
            profile: "custom".into(),
            ..Default::default()
        };
        assert!(!AiCapabilityAccessStore::needs_initial_generation(&config));
    }

    // ── ConfigKey 单测 ───────────────────────────────────────────────────

    #[test]
    fn config_key_is_ai_capability_access() {
        assert_eq!(AiCapabilityAccessConfig::KEY, "ai.capability_access");
    }

    // ── Store CRUD 单测 ──────────────────────────────────────────────────

    #[tokio::test]
    async fn load_returns_default_when_empty() {
        let pool = test_pool().await;
        let config = AiCapabilityAccessStore::load(&pool).await;
        assert!(config.enabled_capabilities.is_empty());
        assert_eq!(config.profile, "recommended");
        assert_eq!(config.schema_version, 1);
    }

    #[tokio::test]
    async fn save_and_load_roundtrip() {
        let pool = test_pool().await;
        let config = AiCapabilityAccessConfig {
            schema_version: 1,
            profile: "custom".into(),
            enabled_capabilities: vec!["open_url".into(), "search_apps".into()],
        };
        AiCapabilityAccessStore::save(&pool, &config)
            .await
            .expect("save");

        let loaded = AiCapabilityAccessStore::load(&pool).await;
        assert_eq!(loaded.profile, "custom");
        assert_eq!(loaded.enabled_capabilities, vec!["open_url", "search_apps"]);
    }

    #[tokio::test]
    async fn toggle_capability_adds_and_removes() {
        let pool = test_pool().await;

        // 添加
        let config = AiCapabilityAccessStore::toggle_capability(&pool, "open_url", true)
            .await
            .expect("toggle on");
        assert!(config.enabled_capabilities.contains(&"open_url".to_string()));

        // 移除
        let config = AiCapabilityAccessStore::toggle_capability(&pool, "open_url", false)
            .await
            .expect("toggle off");
        assert!(!config.enabled_capabilities.contains(&"open_url".to_string()));
    }

    #[tokio::test]
    async fn toggle_capability_idempotent_when_already_enabled() {
        let pool = test_pool().await;
        AiCapabilityAccessStore::toggle_capability(&pool, "open_url", true)
            .await
            .expect("toggle on 1");
        let config = AiCapabilityAccessStore::toggle_capability(&pool, "open_url", true)
            .await
            .expect("toggle on 2");
        // 不应重复添加
        let count = config
            .enabled_capabilities
            .iter()
            .filter(|id| *id == "open_url")
            .count();
        assert_eq!(count, 1);
    }

    #[tokio::test]
    async fn toggle_capabilities_batch() {
        let pool = test_pool().await;
        let ops = vec![
            ("open_url".into(), true),
            ("search_apps".into(), true),
            ("open_path".into(), true),
        ];
        let config = AiCapabilityAccessStore::toggle_capabilities(&pool, &ops)
            .await
            .expect("batch toggle");
        assert_eq!(config.enabled_capabilities.len(), 3);

        // 批量移除一个
        let ops = vec![("open_url".into(), false)];
        let config = AiCapabilityAccessStore::toggle_capabilities(&pool, &ops)
            .await
            .expect("batch remove");
        assert_eq!(config.enabled_capabilities.len(), 2);
        assert!(!config.enabled_capabilities.contains(&"open_url".to_string()));
    }

    #[tokio::test]
    async fn ensure_recommended_generates_on_first_call() {
        let pool = test_pool().await;
        let registry = CapabilityRegistry::default();

        let config = AiCapabilityAccessStore::ensure_recommended(&pool, &registry).await;
        // 推荐集合应包含 ai_default=On 的 Capability（如 open_url/open_path 等）
        // 具体数量取决于 inventory 注册的能力，这里只验证非空（生产环境有 20+ 能力）
        assert!(!config.enabled_capabilities.is_empty(), "推荐集合不应为空");
        assert_eq!(config.profile, "recommended");
    }

    #[tokio::test]
    async fn ensure_recommended_skips_when_already_exists() {
        let pool = test_pool().await;
        // 先保存一个非空配置
        let initial = AiCapabilityAccessConfig {
            schema_version: 1,
            profile: "custom".into(),
            enabled_capabilities: vec!["my_custom_cap".into()],
        };
        AiCapabilityAccessStore::save(&pool, &initial)
            .await
            .expect("save initial");

        let registry = CapabilityRegistry::default();
        let config = AiCapabilityAccessStore::ensure_recommended(&pool, &registry).await;
        // 不应覆盖已有配置
        assert_eq!(config.profile, "custom");
        assert_eq!(config.enabled_capabilities, vec!["my_custom_cap"]);
    }

    #[tokio::test]
    async fn enabled_set_returns_hashset() {
        let pool = test_pool().await;
        AiCapabilityAccessStore::toggle_capability(&pool, "cap_a", true)
            .await
            .expect("toggle");
        AiCapabilityAccessStore::toggle_capability(&pool, "cap_b", true)
            .await
            .expect("toggle");

        let set = AiCapabilityAccessStore::enabled_set(&pool).await;
        assert!(set.contains("cap_a"));
        assert!(set.contains("cap_b"));
        assert!(!set.contains("cap_c"));
    }

    #[tokio::test]
    async fn reset_to_recommended_overrides_custom() {
        let pool = test_pool().await;
        // 先保存自定义配置
        let custom = AiCapabilityAccessConfig {
            schema_version: 1,
            profile: "custom".into(),
            enabled_capabilities: vec!["my_custom_cap".into()],
        };
        AiCapabilityAccessStore::save(&pool, &custom)
            .await
            .expect("save custom");

        let registry = CapabilityRegistry::default();
        let reset = AiCapabilityAccessStore::reset_to_recommended(&pool, &registry)
            .await
            .expect("reset");
        // 重置后 profile 回到 recommended
        assert_eq!(reset.profile, "recommended");
        // 推荐集合不含自定义 cap
        assert!(!reset.enabled_capabilities.contains(&"my_custom_cap".to_string()));
    }

    // ── generate_recommended 与真实 registry 集成测试 ─────────────────────

    #[test]
    fn generate_recommended_excludes_dangerous_caps() {
        let registry = CapabilityRegistry::default();
        let recommended = AiCapabilityAccessStore::generate_recommended(&registry);
        // Dangerous 类不应出现在推荐集合中
        for dangerous_id in &["lock", "shutdown", "restart", "sleep", "clear_history", "exit_blink"] {
            assert!(
                !recommended.contains(&dangerous_id.to_string()),
                "Dangerous capability {dangerous_id} 不应在推荐集合中"
            );
        }
    }

    #[test]
    fn generate_recommended_includes_safe_ai_on_caps() {
        let registry = CapabilityRegistry::default();
        let recommended = AiCapabilityAccessStore::generate_recommended(&registry);
        // Safe + ai_default On 的能力应在推荐集合中
        for safe_id in &["open_url", "open_path", "reveal_in_explorer", "search_apps"] {
            assert!(
                recommended.contains(&safe_id.to_string()),
                "Safe AI-on capability {safe_id} 应在推荐集合中"
            );
        }
    }

    #[test]
    fn generate_recommended_excludes_diagnostic_caps() {
        let registry = CapabilityRegistry::default();
        let recommended = AiCapabilityAccessStore::generate_recommended(&registry);
        // 诊断类（ai_default Off）不应出现
        for diag_id in &[
            "blink_print_debug_info",
            "blink_debug_inithook",
            "update_setting",
        ] {
            assert!(
                !recommended.contains(&diag_id.to_string()),
                "诊断/local-only capability {diag_id} 不应在推荐集合中"
            );
        }
    }

    #[test]
    fn generate_recommended_is_sorted() {
        let registry = CapabilityRegistry::default();
        let recommended = AiCapabilityAccessStore::generate_recommended(&registry);
        let mut sorted = recommended.clone();
        sorted.sort();
        assert_eq!(recommended, sorted, "推荐集合应按字母序排序");
    }
}
