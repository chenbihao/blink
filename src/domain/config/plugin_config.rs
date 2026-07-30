//! 插件配置（0.14.6 §2.1 从 `app/config.rs` 迁入）。
//!
//! PluginConfig 只管 enabled + settings。trigger/surface 不在此——继续由 manifest
//! 单一来源驱动 RuleRouter。

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;

fn default_null() -> serde_json::Value {
    serde_json::Value::Null
}

fn default_true() -> bool {
    true
}

/// 用户自定义触发关键字。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CustomTrigger {
    pub keyword: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub surface: Option<String>,
}

/// 插件独立配置。settings 是 free-form JSON（manifest 声明 schema,core 只存不解释）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(from = "CompatPluginConfig")]
pub struct PluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    #[serde(default)]
    #[allow(dead_code)]
    pub disabled_default_triggers: Vec<String>,
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

/// 兼容旧配置格式（用于数据迁移）。
#[derive(Debug, Deserialize)]
struct CompatPluginConfig {
    pub enabled: bool,
    #[serde(default = "default_null")]
    pub settings: serde_json::Value,
    #[serde(default)]
    #[allow(dead_code)]
    pub disable_default_triggers: Option<bool>,
    #[serde(default)]
    pub disabled_default_triggers: Option<Vec<String>>,
    #[serde(default)]
    pub custom_triggers: Vec<CustomTrigger>,
}

impl From<CompatPluginConfig> for PluginConfig {
    fn from(compat: CompatPluginConfig) -> Self {
        let disabled_default_triggers = compat.disabled_default_triggers.unwrap_or_default();

        Self {
            enabled: compat.enabled,
            settings: compat.settings,
            disabled_default_triggers,
            custom_triggers: compat.custom_triggers,
        }
    }
}

impl Default for PluginConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            settings: serde_json::Value::Null,
            disabled_default_triggers: Vec::new(),
            custom_triggers: Vec::new(),
        }
    }
}

impl PluginConfig {
    /// 合并 manifest triggers 和自定义 triggers，返回最终生效列表。
    pub fn effective_triggers(
        &self,
        manifest_triggers: &[crate::domain::plugin::PluginTrigger],
    ) -> Vec<crate::domain::plugin::PluginTrigger> {
        let mut result = Vec::new();

        for trigger in manifest_triggers {
            match trigger {
                crate::domain::plugin::PluginTrigger::Keyword { keyword, .. } => {
                    if self.disabled_default_triggers.contains(keyword) {
                        continue;
                    }
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Regex { .. } => {
                    result.push(trigger.clone());
                }
                crate::domain::plugin::PluginTrigger::Context { .. } => {
                    result.push(trigger.clone());
                }
            }
        }

        for ct in &self.custom_triggers {
            if ct.enabled {
                result.push(crate::domain::plugin::PluginTrigger::Keyword {
                    keyword: ct.keyword.clone(),
                    exclusive: true,
                });
            }
        }

        result
    }
}

/// 获取单个插件配置（key=`plugin:{id}`）。不存在返回 None。
pub async fn get_plugin_config(pool: &SqlitePool, plugin_id: &str) -> Option<PluginConfig> {
    let key = format!("plugin:{plugin_id}");
    crate::infra::data::history::get_config(pool, &key)
        .await
        .and_then(|json| serde_json::from_str(&json).ok())
}

/// 设置插件配置（upsert,写 `plugin:{id}`）。
pub async fn set_plugin_config(
    pool: &SqlitePool,
    plugin_id: &str,
    config: &PluginConfig,
) -> Result<(), String> {
    let key = format!("plugin:{plugin_id}");
    let json = serde_json::to_string(config).map_err(|e| e.to_string())?;
    crate::infra::data::history::set_config(pool, &key, &json)
        .await
        .map_err(|e| e.to_string())?;
    tracing::debug!(plugin_id, enabled = config.enabled, "插件配置已更新");
    Ok(())
}

/// 获取所有插件配置（key 前缀 `plugin:`）。
#[allow(dead_code)]
pub async fn get_all_plugin_config(pool: &SqlitePool) -> Vec<(String, PluginConfig)> {
    crate::infra::data::history::get_all_config(pool)
        .await
        .into_iter()
        .filter_map(|(k, v)| {
            let id = k.strip_prefix("plugin:")?;
            let cfg = serde_json::from_str(&v).ok()?;
            Some((id.to_string(), cfg))
        })
        .collect()
}
