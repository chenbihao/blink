//! PluginEngine(见 §3.5):聚合所有 builtin 插件的查询执行器。
//!
//! 0.4 退化为纯执行器:不再自匹配(`matching_plugins`/`match_keyword` 上移至 RuleRouter),
//! 改为接收「要查哪些插件 + 各自 arg」的指令列表,直接查询对应插件进程。
//! SearchService 在 `route()` 后按 `Route::Takeover`/`Route::Mixed` 调用 `query_subset`。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use sqlx::SqlitePool;
use tokio::task::JoinSet;

use crate::domain::search::engine::{SearchAction, SearchItem};
use crate::domain::search::scorer::clamp_plugin_score;

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
    configs: Arc<RwLock<HashMap<String, crate::app::config::PluginConfig>>>,
    pool: SqlitePool,
    /// 全局代理(HTTP,HTTPS),进程启动时 env 注入;插件 ure/reqwest 原生读取。
    #[allow(dead_code)] // 保留用于未来插件代理配置
    global_proxy: Option<(String, String)>,
}

impl PluginEngine {
    pub fn new(plugins: Vec<Arc<PluginHandle>>, pool: SqlitePool, global_proxy: Option<(String, String)>) -> Self {
        PluginEngine {
            plugins,
            configs: Arc::new(RwLock::new(HashMap::new())),
            pool,
            global_proxy,
        }
    }

    /// 获取所有插件的 manifest 信息（设置页用）。
    pub fn list_plugins(&self, lang: &str) -> Vec<serde_json::Value> {
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
                        super::PluginTrigger::Context { when, .. } => format!("context: {when:?}"),
                    })
                    .collect();

                // settings_schema → resolve 后的 JSON(title/description/label 已转字符串)
                let schema: Vec<serde_json::Value> = manifest
                    .settings_schema
                    .iter()
                    .map(|f| {
                        let mut field = serde_json::json!({
                            "key": f.key,
                            "type": match f.kind {
                                super::manifest::SettingType::Boolean => "boolean",
                                super::manifest::SettingType::String => "string",
                                super::manifest::SettingType::Number => "number",
                                super::manifest::SettingType::Enum
                                | super::manifest::SettingType::Select => "enum",
                                super::manifest::SettingType::SortableList => "sortable_list",
                            },
                            "title": f.title.resolve(lang),
                            "description": f.description.as_ref().map(|d| d.resolve(lang)),
                            "default": f.default.clone(),
                            "min": f.min,
                            "max": f.max,
                            "options": f.options.iter().map(|o| serde_json::json!({
                                "value": o.value.clone(),
                                "label": o.label.resolve(lang),
                            })).collect::<Vec<_>>(),
                        });
                        // 添加 group 信息（支持对象格式，包含 title 和可选 description）
                        if let Some(ref group) = f.group {
                            field["group"] = if let Some(ref desc) = group.description {
                                serde_json::json!({ "title": group.title, "description": desc })
                            } else {
                                serde_json::json!(group.title)
                            };
                        }
                        field
                    })
                    .collect();

                let config = self.get_config(&manifest.id).unwrap_or_default();
                let custom_triggers: Vec<serde_json::Value> = config
                    .custom_triggers
                    .iter()
                    .map(|t| {
                        serde_json::json!({
                            "keyword": t.keyword,
                            "enabled": t.enabled,
                        })
                    })
                    .collect();

                serde_json::json!({
                    "id": manifest.id,
                    "name": manifest.name.resolve(lang),
                    "version": manifest.version,
                    "description": manifest.description.resolve(lang),
                    "triggers": triggers,
                    "custom_triggers": custom_triggers,
                    "disabled_default_triggers": config.disabled_default_triggers,
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
            // min_arg_length 快速失败:仅对非空参数生效(空参数=无参触发,用插件默认配置)。
            // 用 chars().count() 而非 len()——中文等宽字符每个算 1 而非 3 字节
            let min_len = plugin.manifest().runtime.min_arg_length.unwrap_or(0);
            let arg_len = arg.chars().count();
            if min_len > 0 && arg_len > 0 && arg_len < min_len {
                tracing::debug!(plugin_id = %id, arg_len, min_len, "query_subset: 参数过短,快速失败");
                continue;
            }
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

    /// 启动时从 DB 加载所有插件配置;不存在则写默认。
    /// 默认 `enabled` 走 `manifest.default_enabled`（缺省 true，翻译等"需配置才能用"的
    /// 插件声明为 false）；默认 `settings` 走 `manifest.default_settings()`（无 schema 则 null）。
    /// main.rs 在构造后、注入 SearchService 前 `block_on` 调用。
    pub async fn init_configs(&self) {
        let mut configs = self.configs.write().unwrap();
        for plugin in &self.plugins {
            let id = plugin.id();
            match crate::app::config::get_plugin_config(&self.pool, id).await {
                Some(cfg) => {
                    configs.insert(id.to_string(), cfg);
                }
                None => {
                    // 首装：从 manifest 生成默认配置。
                    // - settings 走 settings_schema
                    // - enabled 走 manifest.default_enabled（缺省 true；需配置密钥
                    //   才能用的插件如翻译声明为 false，避免装完就撞到无法工作的入口）
                    let manifest = plugin.manifest();
                    let mut default = crate::app::config::PluginConfig::default();
                    default.settings = manifest.default_settings();
                    default.enabled = manifest.default_enabled;
                    match crate::app::config::set_plugin_config(&self.pool, id, &default).await {
                        Ok(()) => tracing::info!(
                            plugin = %id,
                            enabled = default.enabled,
                            "初始化插件配置(默认)",
                        ),
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
        config: crate::app::config::PluginConfig,
        router: Option<&crate::domain::intent::RuleRouter>,
    ) -> Result<(), String> {
        crate::app::config::set_plugin_config(&self.pool, plugin_id, &config).await?;
        self.configs
            .write()
            .unwrap()
            .insert(plugin_id.to_string(), config.clone());

        // 如果传入了 router，热更新触发规则
        if let Some(r) = router {
            if let Some(manifest) = self.get_manifest(plugin_id) {
                let effective_triggers = config.effective_triggers(&manifest.triggers);
                r.reload_plugin_triggers(plugin_id, &effective_triggers);
                tracing::info!(plugin = %plugin_id, "插件配置更新,触发规则已热重载");
            }
        }

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

    /// 获取插件完整配置（含自定义 triggers）。
    pub fn get_config(&self, id: &str) -> Option<crate::app::config::PluginConfig> {
        self.configs.read().unwrap().get(id).cloned()
    }

    /// 获取插件的 manifest。
    pub fn get_manifest(&self, id: &str) -> Option<super::PluginManifest> {
        self.plugins
            .iter()
            .find(|p| p.id() == id)
            .map(|p| p.manifest().clone())
    }

    /// 列出所有已加载插件的 manifest（0.8.3 §4.6 设置页 context binding 面板用）。
    ///
    /// 用于设置页遍历 `manifest.triggers` 收集所有 `PluginTrigger::Context`。
    pub fn list_manifests(&self) -> Vec<super::PluginManifest> {
        self.plugins
            .iter()
            .map(|p| p.manifest().clone())
            .collect()
    }

    /// 获取插件的显示名称,回退为 plugin_id。
    /// 用于**结果占位项**拼接(service.rs placeholder),属结果文案层——
    /// 该层尚未接 locale(context.lang 待阶段二),故固定取 name 的 zh 回退值,不随语言切换。
    /// 设置页展示走 list_plugins → resolve(lang) 本地化,与此独立。
    pub fn get_display_name(&self, id: &str) -> String {
        self.find_plugin(id)
            .map(|p| p.manifest().name.resolve("zh"))
            .unwrap_or_else(|| id.to_string())
    }

    /// 获取插件的最短参数长度(字符数)，用于快速失败过滤。
    /// 无配置则返回 0 (不限长度)。
    pub fn get_min_arg_length(&self, id: &str) -> usize {
        self.find_plugin(id)
            .and_then(|p| p.manifest().runtime.min_arg_length)
            .unwrap_or(0)
    }

    /// 获取插件的防抖间隔(毫秒)。0 或未配置 = 不防抖(每键触发)。
    pub fn get_debounce_ms(&self, id: &str) -> u64 {
        self.find_plugin(id)
            .and_then(|p| p.manifest().runtime.debounce_ms)
            .unwrap_or(0)
    }

    /// 空参数引导文案（0.8.1）。manifest 未配置则返回 None。
    /// `lang` 由 SearchService 传入（AppConfig.language 快照）；`resolve` 内部按
    /// lang → zh → 首个 的顺序回退，永不 panic。
    pub fn get_empty_arg_hint(&self, id: &str, lang: &str) -> Option<String> {
        self.find_plugin(id)
            .and_then(|p| p.manifest().runtime.empty_arg_hint.as_ref().map(|h| h.resolve(lang)))
    }

    /// 更新全局代理配置 + 重置所有插件进程(保存后调用)。
    /// 下次 query 自动用新 env 重启，用户零感知热更新。
    pub async fn update_global_proxy(&self, proxy: Option<(String, String)>) {
        // 先更新每个 PluginHandle 的 proxy 字段(新进程 spawn 时会用它)
        for plugin in &self.plugins {
            plugin.update_proxy(proxy.clone());
        }
        // 杀掉已启动的旧进程(下次 query 自动用新 proxy 重启)
        for plugin in &self.plugins {
            plugin.reset_process().await;
        }
        tracing::info!("全局代理配置已更新，所有插件进程已重置");
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

/// `PluginSettingResolver` 实现（0.8.2 §3.4.1 + 0.8.3 §4.13 扩展）——把 settings 字段
/// 字符串读出来给 `RuleRouter` 用；0.8.3 起同时暴露 `is_enabled` 供 Context Suggestion
/// 产出前查启用态（禁用联动决策）。
///
/// 读取路径：`configs[id].settings[key]`，只接受 `Value::String`。禁用插件（`enabled=false`）
/// 仍返回 setting——`RuleRouter` 到时按需自行过滤；此层职责仅"读值"。
impl super::PluginSettingResolver for PluginEngine {
    fn get_string(&self, plugin_id: &str, key: &str) -> Option<String> {
        self.get_settings(plugin_id)?
            .get(key)?
            .as_str()
            .map(|s| s.to_string())
    }

    /// 委托到自身 `is_enabled`（0.8.3 §4.13 P1「运行时查启用态」）。
    fn is_enabled(&self, plugin_id: &str) -> bool {
        PluginEngine::is_enabled(self, plugin_id)
    }

    /// 读 manifest.name.resolve(lang) 作 Ghost display（0.8.3 §4.13 P0 修订）。
    /// 未加载的插件返回 None,调用方 fallback 到 id 末段。
    fn get_display_name(&self, plugin_id: &str, lang: &str) -> Option<String> {
        self.plugins
            .iter()
            .find(|p| p.manifest().id == plugin_id)
            .map(|p| p.manifest().name.resolve(lang))
    }
}

/// 插件结果项 → 内部 SearchItem。
/// 特殊处理：score < 0 表示插件返回的错误信息，保留原 score 让排序到最后。
fn to_search_item(plugin_id: &str, item: PluginItem) -> SearchItem {
    let action = match item.action {
        PluginAction::None => SearchAction::None,
        PluginAction::Copy { text } => SearchAction::Copy { text, hit_id: None },
        PluginAction::Open { path } => SearchAction::Open { path },
    };
    // 负分 = 插件错误信息，不 clamp（保留负分排到最后）
    let score = if item.score < 0.0 {
        item.score
    } else {
        clamp_plugin_score(item.score)
    };
    SearchItem {
        id: format!("plugin:{plugin_id}:{}", item.title),
        title: item.title,
        subtitle: item.subtitle,
        score,
        action,
        source: plugin_id.to_string(),
        score_detail: Some(format!("plugin={:.2}", score)),
    }
}
