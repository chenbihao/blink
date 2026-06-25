//! PluginEngine(见 §3.5):聚合所有 builtin 插件的查询执行器。
//!
//! 0.4 退化为纯执行器:不再自匹配(`matching_plugins`/`match_keyword` 上移至 RuleRouter),
//! 改为接收「要查哪些插件 + 各自 arg」的指令列表,直接查询对应插件进程。
//! SearchService 在 `route()` 后按 `Route::Takeover`/`Route::Mixed` 调用 `query_subset`。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sqlx::SqlitePool;
use tokio::task::JoinSet;

use crate::search::engine::{SearchAction, SearchItem};

use super::process::PluginHandle;
use super::protocol::{PluginAction, PluginItem, PluginQueryContext};

/// 聚合所有 builtin 插件的查询执行器(见 §3.5)。
///
/// 0.5.1 起:持有每个插件的 `PluginConfig`(enabled + settings)内存快照 + DB pool。
/// - `enabled=false`:`query_subset` 跳过该插件。
/// - `settings`:每次 query 注入 `PluginRequest`(0.5 §2.4 透传协议),天然热更新。
pub struct PluginEngine {
    plugins: Vec<Arc<PluginHandle>>,
    /// plugin_id → 配置。启动时 `init_configs` 从 DB 加载;`update_config` 时更新。
    configs: Arc<RwLock<HashMap<String, crate::config::PluginConfig>>>,
    pool: SqlitePool,
}

impl PluginEngine {
    pub fn new(plugins: Vec<Arc<PluginHandle>>, pool: SqlitePool) -> Self {
        PluginEngine {
            plugins,
            configs: Arc::new(RwLock::new(HashMap::new())),
            pool,
        }
    }

    /// 获取所有插件的 manifest 信息（设置页用）。
    pub fn list_plugins(&self) -> Vec<serde_json::Value> {
        self.plugins
            .iter()
            .map(|p| {
                let manifest = p.manifest();
                let triggers: Vec<String> = manifest
                    .triggers
                    .iter()
                    .map(|t| match t {
                        super::PluginTrigger::Keyword { keyword, .. } => keyword.clone(),
                        super::PluginTrigger::Regex { pattern, .. } => format!("regex: {pattern}"),
                    })
                    .collect();

                // settings_schema → resolve 后的 JSON(title/description/label 已转字符串)
                let schema: Vec<serde_json::Value> = manifest
                    .settings_schema
                    .iter()
                    .map(|f| {
                        serde_json::json!({
                            "key": f.key,
                            "type": match f.kind {
                                super::manifest::SettingType::Boolean => "boolean",
                                super::manifest::SettingType::String => "string",
                                super::manifest::SettingType::Number => "number",
                                super::manifest::SettingType::Enum => "enum",
                            },
                            "title": f.title.resolve(),
                            "description": f.description.as_ref().map(|d| d.resolve()),
                            "default": f.default.clone(),
                            "min": f.min,
                            "max": f.max,
                            "options": f.options.iter().map(|o| serde_json::json!({
                                "value": o.value.clone(),
                                "label": o.label.resolve(),
                            })).collect::<Vec<_>>(),
                        })
                    })
                    .collect();

                serde_json::json!({
                    "id": manifest.id,
                    "name": manifest.name,
                    "version": manifest.version,
                    "description": manifest.description,
                    "triggers": triggers,
                    "enabled": self.is_enabled(&manifest.id),
                    "settings": self.get_settings(&manifest.id),
                    "settings_schema": schema,
                })
            })
            .collect()
    }

    /// 按给定候选列表查询插件。每个候选 = (plugin_id, arg)。
    /// 多插件并发,各自内部有 timeout 兜底。结果顺序无关——融合层按 score 重排。
    ///
    /// 0.5.1:`enabled=false` 跳过;`settings` 注入到每个 query 请求(§2.4 透传协议)。
    pub async fn query_subset(
        &self,
        candidates: &[(String, String)],
        context: &PluginQueryContext,
    ) -> Vec<SearchItem> {
        if candidates.is_empty() {
            return Vec::new();
        }
        let mut set: JoinSet<Vec<SearchItem>> = JoinSet::new();
        for (id, arg) in candidates {
            if !self.is_enabled(id) {
                tracing::debug!(plugin_id = %id, "query_subset: 插件已禁用,跳过");
                continue;
            }
            let Some(plugin) = self.find_plugin(id) else {
                tracing::debug!(plugin_id = %id, "query_subset: 插件未找到");
                continue;
            };
            let plugin_id = id.clone();
            let arg = arg.clone();
            let context = context.clone();
            let settings = self.get_settings(id);
            set.spawn(async move {
                match plugin.query(&arg, &context, settings.as_ref()).await {
                    Ok(items) => items
                        .into_iter()
                        .map(|it| to_search_item(&plugin_id, it))
                        .collect(),
                    Err(e) => {
                        tracing::warn!(plugin = %plugin_id, error = %e, "插件查询失败");
                        Vec::new()
                    }
                }
            });
        }
        let mut items = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(part) = res {
                items.extend(part);
            }
        }
        items
    }

    fn find_plugin(&self, id: &str) -> Option<Arc<PluginHandle>> {
        self.plugins.iter().find(|p| p.id() == id).cloned()
    }

    // ── 配置管理(0.5.1)──────────────────────────────────────────────────────

    /// 启动时从 DB 加载所有插件配置;不存在则写默认 {enabled:true, settings:null}。
    /// main.rs 在构造后、注入 SearchService 前 `block_on` 调用。
    pub async fn init_configs(&self) {
        let mut configs = self.configs.write().unwrap();
        for plugin in &self.plugins {
            let id = plugin.id();
            match crate::config::get_plugin_config(&self.pool, id).await {
                Some(cfg) => {
                    configs.insert(id.to_string(), cfg);
                }
                None => {
                    // 默认 settings 从 manifest.settings_schema 生成(无 schema 则 null)
                    let default = crate::config::PluginConfig {
                        enabled: true,
                        settings: plugin.manifest().default_settings(),
                    };
                    match crate::config::set_plugin_config(&self.pool, id, &default).await {
                        Ok(()) => tracing::info!(plugin = %id, "初始化插件配置(默认)"),
                        Err(e) => tracing::warn!(plugin = %id, error = %e, "写默认插件配置失败"),
                    }
                    configs.insert(id.to_string(), default);
                }
            }
        }
    }

    /// 更新插件配置:写 DB + 更新内存 map(command 层 `update_plugin_config` 调)。
    pub async fn update_config(
        &self,
        plugin_id: &str,
        config: crate::config::PluginConfig,
    ) -> Result<(), String> {
        crate::config::set_plugin_config(&self.pool, plugin_id, &config).await?;
        self.configs
            .write()
            .unwrap()
            .insert(plugin_id.to_string(), config);
        Ok(())
    }

    /// 插件是否启用(无配置记录视为启用)。
    pub fn is_enabled(&self, id: &str) -> bool {
        self.configs
            .read()
            .unwrap()
            .get(id)
            .map(|c| c.enabled)
            .unwrap_or(true)
    }

    /// 取插件 settings(None = 无配置或 settings 为 null)。
    fn get_settings(&self, id: &str) -> Option<serde_json::Value> {
        self.configs
            .read()
            .unwrap()
            .get(id)
            .map(|c| c.settings.clone())
            .filter(|s| !s.is_null())
    }
}

/// 插件结果项 → 内部 SearchItem。
fn to_search_item(plugin_id: &str, item: PluginItem) -> SearchItem {
    let action = match item.action {
        PluginAction::Copy { text } => SearchAction::Copy { text },
        PluginAction::Open { path } => SearchAction::Open { path },
    };
    SearchItem {
        id: format!("plugin:{plugin_id}:{}", item.title),
        title: item.title,
        subtitle: item.subtitle,
        score: item.score.clamp(0.0, 1.0),
        action,
        source: plugin_id.to_string(),
    }
}
