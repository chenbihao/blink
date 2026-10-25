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
use crate::search::scorer::clamp_plugin_score;

use super::process::PluginHandle;
use super::protocol::{PluginAction, PluginItem, PluginQueryContext};

#[cfg(test)]
#[path = "config_tests.rs"]
mod config_tests;

/// 插件集合快照（0.28.1 §五.4）。
///
/// 集合整体替换、`generation` 单调递增：运行期安装/更新/卸载不再固定在
/// 构造时的 Vec，而是发布新快照。持有旧快照句柄（如已注册的
/// `PluginCapabilityAdapter`）通过 `PluginEngine::is_active_handle` 的
/// Arc 指针比对发现代次过期——旧 Arc 不继承新实现的授权。
pub struct PluginSet {
    pub generation: u64,
    pub plugins: Vec<Arc<PluginHandle>>,
}

/// 聚合所有 builtin 插件的查询执行器(见 §3.5)。
///
/// 0.5.1 起:持有每个插件的 `PluginConfig`(enabled + settings)内存快照 + DB pool。
/// - `enabled=false`:`query_subset` 跳过该插件。
/// - `settings`:每次 query 注入 `PluginRequest`(0.5 §2.4 透传协议),天然热更新。
/// 0.28.1 起:插件集合运行期可变（`PluginSet` 快照，见上）。
pub struct PluginEngine {
    plugins: RwLock<Arc<PluginSet>>,
    /// plugin_id → 配置。启动时 `init_configs` 从 DB 加载;`update_config` 时更新。
    configs: Arc<RwLock<HashMap<String, crate::config::PluginConfig>>>,
    pool: SqlitePool,
    update_lock: tokio::sync::Mutex<()>,
    /// 全局代理(HTTP,HTTPS),进程启动时 env 注入;插件 ure/reqwest 原生读取。
    #[allow(dead_code)] // 保留用于未来插件代理配置
    global_proxy: Option<(String, String)>,
}

impl PluginEngine {
    pub fn new(
        plugins: Vec<Arc<PluginHandle>>,
        pool: SqlitePool,
        global_proxy: Option<(String, String)>,
    ) -> Self {
        PluginEngine {
            plugins: RwLock::new(Arc::new(PluginSet {
                generation: 1,
                plugins,
            })),
            configs: Arc::new(RwLock::new(HashMap::new())),
            pool,
            update_lock: tokio::sync::Mutex::new(()),
            global_proxy,
        }
    }

    /// 当前集合快照（Arc 克隆，读方无锁遍历）。
    pub fn plugin_set(&self) -> Arc<PluginSet> {
        self.plugins.read().unwrap().clone()
    }

    /// 运行期发布新集合（0.28.1）：整体替换 + generation +1。
    ///
    /// 返回 (新 generation, 被替换的旧句柄列表)——调用方（runtime.rs 发布器）
    /// 负责 bounded drain 旧进程、撤销路由与能力出口；本方法只换快照。
    pub fn replace_plugins(&self, new_plugins: Vec<Arc<PluginHandle>>) -> (u64, Vec<Arc<PluginHandle>>) {
        let mut guard = self.plugins.write().unwrap();
        let retired = guard.plugins.clone();
        let generation = guard.generation + 1;
        *guard = Arc::new(PluginSet {
            generation,
            plugins: new_plugins,
        });
        (generation, retired)
    }

    /// 指定 handle 是否仍是当前代次的活跃成员（Arc 指针比对）。
    ///
    /// 旧 adapter 在 invoke 前复核：插件被更新/卸载后，绑定旧 handle 的
    /// adapter 必须拒绝执行，不能借旧 Arc 继续调用新实现或复活的旧实现。
    pub fn is_active_handle(&self, plugin_id: &str, handle: &Arc<PluginHandle>) -> bool {
        let set = self.plugin_set();
        set.plugins
            .iter()
            .any(|p| p.id() == plugin_id && Arc::ptr_eq(p, handle))
    }

    /// 为单个插件初始化配置（发布新插件后调用；init_configs 的单插件版）。
    pub async fn ensure_config_for(&self, plugin: &Arc<PluginHandle>) {
        let id = plugin.id();
        if self.configs.read().unwrap().contains_key(id) {
            return;
        }
        let loaded = match crate::config::get_plugin_config(&self.pool, id).await {
            Some(cfg) => cfg,
            None => {
                let manifest = plugin.manifest();
                let default = crate::config::PluginConfig {
                    settings: manifest.default_settings(),
                    enabled: manifest.default_enabled,
                    ..Default::default()
                };
                if let Err(e) = crate::config::set_plugin_config(&self.pool, id, &default).await {
                    tracing::warn!(plugin = %id, error = %e, "写默认插件配置失败");
                }
                default
            }
        };
        self.configs.write().unwrap().insert(id.to_string(), loaded);
    }

    /// 获取所有插件的 manifest 信息（设置页用）。
    pub fn list_plugins(&self, lang: &str) -> Vec<serde_json::Value> {
        self.plugin_set()
            .plugins
            .iter()
            .map(|p| {
                let manifest = p.manifest();
                // 插件卡片"关键词："行只展示 Keyword/Regex 触发（用户可编辑的入口）。
                // Context 触发是"能力声明",归属"智能感知"面板（list_context_bindings），
                // 不与 keyword tag 混排 —— 不然会渲染出 `context: TextIsNonTargetLang`
                // 这类 Debug 字面量，且用户在此处也无法操作它。
                let triggers: Vec<String> = manifest
                    .triggers
                    .iter()
                    .filter_map(|t| match t {
                        super::PluginTrigger::Keyword { keyword, .. } => Some(keyword.clone()),
                        super::PluginTrigger::Regex { pattern, .. } => {
                            Some(format!("regex: {pattern}"))
                        }
                        super::PluginTrigger::Context { .. } => None,
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
                                serde_json::json!({ "title": group.title.resolve(lang), "description": desc.resolve(lang) })
                            } else {
                                serde_json::json!(group.title.resolve(lang))
                            };
                        }
                        // no_space 透传给设置页做空格检测（密钥类字段），默认 false 不输出
                        if f.no_space {
                            field["no_space"] = serde_json::json!(true);
                        }
                        // secret 透传给设置页做 password 渲染 + 掩码（0.25.16），默认 false 不输出
                        if f.secret {
                            field["secret"] = serde_json::json!(true);
                        }
                        // 条件显隐透传（设置页联动显隐，如降级顺序仅在允许降级时显示）
                        if let Some(ref vw) = f.visible_when {
                            field["visible_when"] =
                                serde_json::json!({ "key": vw.key, "equals": vw.equals });
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
                    "enabled": config.enabled,
                    "revision": config.revision,
                    // 0.25.20：runtime 类型透传——python/node 脚本插件启用时
                    // 前端引导安装托管解释器（powershell 系统自带、process 无需）。
                    "runtime_type": manifest.runtime.r#type.wire_name(),
                    "settings": config.settings,
                    "settings_schema": schema,
                })
            })
            .collect()
    }

    /// 插件就绪状态（0.28.2 管理页）：能否直接使用，缺什么给修复入口线索。
    ///
    /// 判定项（有意保守，宁可漏报不误报）：
    /// - `disabled`：`plugin:{id}` 配置关闭；
    /// - `missing_secret`：manifest 声明了 `secret:true` 字段且**一个都没配置**
    ///   （settings 占位空串 + CM 无值）。部分配置即视为就绪——选错引擎等
    ///   运行期错误由查询链路的错误项反馈，不在此预判；
    /// - `missing_dataset`：manifest 声明数据包且宿主已注入 datasets 目录，
    ///   但对应数据包目录不存在（词库实际挂载在 0.28.4；builtin v1 句柄无
    ///   datasets 目录时跳过本项，不误报）。
    ///
    /// 托管解释器（python/node）可用性不在此判定——设置页已有运行时引导
    /// （runtime_type + script_interpreters_status），保持单一真源。
    pub fn readiness(&self, id: &str, lang: &str) -> Option<serde_json::Value> {
        let handle = self
            .plugin_set()
            .plugins
            .iter()
            .find(|p| p.id() == id)?
            .clone();
        let manifest = handle.manifest();
        let mut blockers: Vec<serde_json::Value> = Vec::new();

        if !self.is_enabled(id) {
            blockers.push(blocker("disabled", None, lang));
        }

        let secret_fields: Vec<&super::manifest::SettingField> = manifest
            .settings_schema
            .iter()
            .filter(|f| f.secret)
            .collect();
        if !secret_fields.is_empty() {
            let settings = self.get_settings(id);
            let any_configured = secret_fields.iter().any(|f| {
                // 旧明文（迁移前）或 CM 值（secret_hint 只对声明字段放行）
                settings
                    .as_ref()
                    .and_then(|s| s.get(&f.key))
                    .and_then(|v| v.as_str())
                    .is_some_and(|v| !v.trim().is_empty())
                    || self.secret_hint(id, &f.key).is_some()
            });
            if !any_configured {
                blockers.push(blocker("missing_secret", Some(&secret_fields[0].key), lang));
            }
        }

        if !manifest.datasets.is_empty()
            && let Some(base) = handle.dirs().map(|d| std::path::PathBuf::from(d.datasets))
        {
            let missing: Vec<&super::manifest::DatasetDef> = manifest
                .datasets
                .iter()
                .filter(|d| !base.join(&d.id).is_dir())
                .collect();
            if !missing.is_empty() {
                blockers.push(blocker("missing_dataset", Some(&missing[0].id), lang));
            }
        }

        Some(serde_json::json!({
            "plugin_id": id,
            "enabled": self.is_enabled(id),
            "ready": blockers.is_empty(),
            "blockers": blockers,
        }))
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
        self.query_subset_with(
            candidates,
            context,
            |plugin, arg, context, settings| async move {
                let plugin_id = plugin.id().to_owned();
                match plugin.query(&arg, &context, settings.as_ref()).await {
                    Ok(items) => items,
                    Err(e) => {
                        tracing::warn!(plugin = %plugin_id, error = %e, "插件查询失败");
                        Vec::new()
                    }
                }
            },
        )
        .await
    }

    async fn query_subset_with<F, Fut>(
        &self,
        candidates: &[(String, String)],
        context: &PluginQueryContext,
        query: F,
    ) -> Vec<SearchItem>
    where
        F: Fn(Arc<PluginHandle>, String, PluginQueryContext, Option<serde_json::Value>) -> Fut
            + Clone
            + Send
            + Sync
            + 'static,
        Fut: std::future::Future<Output = Vec<PluginItem>> + Send + 'static,
    {
        if candidates.is_empty() {
            return Vec::new();
        }
        let mut set: JoinSet<(String, Option<u64>, Vec<SearchItem>)> = JoinSet::new();
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
            // 0.25.16:喂插件进程的 settings 用 resolved（CM 密钥已还原明文），
            // 插件二进制与协议零改动——SQLite 侧只有空串占位。
            let (settings, settings_revision) = self.resolved_settings_snapshot(id).await;
            if !self.is_enabled(id) {
                continue;
            }
            let query = query.clone();
            set.spawn(async move {
                let items = query(plugin, arg, context, settings)
                    .await
                    .into_iter()
                    .map(|it| to_search_item(&plugin_id, it))
                    .collect();
                (plugin_id, settings_revision, items)
            });
        }
        let mut completed = Vec::new();
        while let Some(res) = set.join_next().await {
            if let Ok(result) = res {
                completed.push(result);
            }
        }

        // 等全部并发查询完成后，在同一配置快照下校验，避免先完成的旧结果在
        // 等待其他插件期间通过校验、随后又和更新后的配置结果一起返回。
        let configs = self.configs.read().unwrap();
        let mut items = Vec::new();
        for (plugin_id, settings_revision, part) in completed {
            let current = match configs.get(&plugin_id) {
                Some(config) => config.enabled && Some(config.revision) == settings_revision,
                // 无配置记录按 is_enabled 的约定视为启用；但如果查询曾带有 revision，
                // 此后记录消失也代表快照不再有效。
                None => settings_revision.is_none(),
            };
            if !current {
                tracing::debug!(
                    plugin_id = %plugin_id,
                    settings_revision = ?settings_revision,
                    "query_subset: 插件配置已变化或已禁用,丢弃迟到结果"
                );
                continue;
            }
            items.extend(part);
        }
        items
    }

    fn find_plugin(&self, id: &str) -> Option<Arc<PluginHandle>> {
        self.plugin_set()
            .plugins
            .iter()
            .find(|p| p.id() == id)
            .cloned()
    }

    /// 获取所有已加载插件的句柄（0.9.3 插件 tool 注册用）。
    /// 0.28.1 起集合运行期可变——返回快照克隆（Arc 列表），调用方迭代期间
    /// 的发布不影响本次迭代，但下次调用可见新集合。
    pub fn all_plugins(&self) -> Vec<Arc<PluginHandle>> {
        self.plugin_set().plugins.clone()
    }

    // ── 配置管理(0.5.1)──────────────────────────────────────────────────────

    /// 启动时从 DB 加载所有插件配置;不存在则写默认。
    /// 默认 `enabled` 走 `manifest.default_enabled`（缺省 true，翻译等"需配置才能用"的
    /// 插件声明为 false）；默认 `settings` 走 `manifest.default_settings()`（无 schema 则 null）。
    /// main.rs 在构造后、注入 SearchService 前 `block_on` 调用。
    pub async fn init_configs(&self) {
        // 先完成全部 DB 读写，再短暂持锁合并——避免 RwLock guard 跨 await
        let mut loaded = HashMap::new();
        for plugin in &self.plugin_set().plugins {
            let id = plugin.id();
            match crate::config::get_plugin_config(&self.pool, id).await {
                Some(cfg) => {
                    loaded.insert(id.to_string(), cfg);
                }
                None => {
                    // 首装：从 manifest 生成默认配置。
                    // - settings 走 settings_schema
                    // - enabled 走 manifest.default_enabled（缺省 true；需配置密钥
                    //   才能用的插件如翻译声明为 false，避免装完就撞到无法工作的入口）
                    let manifest = plugin.manifest();
                    let default = crate::config::PluginConfig {
                        settings: manifest.default_settings(),
                        enabled: manifest.default_enabled,
                        ..Default::default()
                    };
                    match crate::config::set_plugin_config(&self.pool, id, &default).await {
                        Ok(()) => tracing::info!(
                            plugin = %id,
                            enabled = default.enabled,
                            "初始化插件配置(默认)",
                        ),
                        Err(e) => tracing::warn!(plugin = %id, error = %e, "写默认插件配置失败"),
                    }
                    loaded.insert(id.to_string(), default);
                }
            }
        }
        self.configs.write().unwrap().extend(loaded);
    }

    /// 更新插件配置:写 DB + 更新内存 map，返回本次提交的 revision。
    ///
    /// 0.25.16:持久化前先过 `sanitize_config_secrets`——manifest `secret:true` 字段的
    /// 非空值转存 Credential Manager，SQLite settings 只留空串占位。
    pub async fn update_config(
        &self,
        plugin_id: &str,
        mut config: crate::config::PluginConfig,
        router: Option<&crate::intent::RuleRouter>,
    ) -> Result<u64, String> {
        self.update_config_with_store(
            plugin_id,
            &mut config,
            &super::secret_update::PlatformSecretStore,
            vec![],
        )
        .await?;
        // 如果传入了 router，热更新触发规则
        if let Some(r) = router
            && let Some(manifest) = self.get_manifest(plugin_id)
        {
            let effective_triggers = config.effective_triggers(&manifest.triggers);
            r.reload_plugin_triggers(plugin_id, &effective_triggers);
            tracing::info!(plugin = %plugin_id, "插件配置更新,触发规则已热重载");
        }

        Ok(config.revision)
    }

    /// revision 校验在任何 CM 写之前；DB 条件写失败后恢复旧密钥。
    async fn update_config_with_store(
        &self,
        plugin_id: &str,
        config: &mut crate::config::PluginConfig,
        store: &dyn super::secret_update::SecretStore,
        mut secret_updates: Vec<(String, Option<blink_infra::platform::secret::SecretString>)>,
    ) -> Result<(), String> {
        let _guard = self.update_lock.lock().await;
        let key = format!("plugin:{plugin_id}");
        let baseline = blink_infra::data::config::config_value(&self.pool, &key)
            .await
            .map_err(|_| "plugin_config_read_failed".to_string())?;
        let current: crate::config::PluginConfig = match baseline.as_deref() {
            Some(raw) => serde_json::from_str(raw)
                .map_err(|_| "plugin_config_invalid: 已保存配置无法解析".to_string())?,
            None => self.get_config(plugin_id).unwrap_or_default(),
        };
        if current.revision != config.revision {
            // 回填权威快照，让冲突后的设置页重读也能得到最新代次。
            self.configs
                .write()
                .unwrap()
                .insert(plugin_id.to_string(), current);
            return Err("config_conflict: 插件配置已被其他窗口修改，请重新读取后重试".into());
        }
        // settings 是补丁语义：未提交字段保留，对象原样保存；显式 null 是明确值。
        let mut settings = current.settings.as_object().cloned().unwrap_or_default();
        match &config.settings {
            serde_json::Value::Null => {}
            serde_json::Value::Object(patch) => settings.extend(patch.clone()),
            _ => return Err("invalid_arguments: 插件 settings 必须是对象或 null".into()),
        }
        config.settings = serde_json::Value::Object(settings);
        if plugin_id == "builtin.weather" {
            super::weather_config::validate_settings(&config.settings)?;
        }
        for field in self.secret_field_keys(plugin_id) {
            if let Some(value) = config.settings.get(&field) {
                let value = value
                    .as_str()
                    .ok_or_else(|| "invalid_arguments: 密钥字段必须是文本".to_string())?;
                if !value.trim().is_empty() {
                    secret_updates.push((
                        field.clone(),
                        Some(blink_infra::platform::secret::SecretString::new(
                            value.to_owned(),
                        )),
                    ));
                }
            }
            config.settings[&field] = serde_json::json!("");
        }
        config.revision = current
            .revision
            .checked_add(1)
            .ok_or_else(|| "plugin_config_revision_exhausted".to_string())?;
        let serialized = serde_json::to_string(config)
            .map_err(|_| "plugin_config_serialize_failed".to_string())?;
        let secrets = super::secret_update::apply(store, plugin_id, secret_updates)?;
        let written = blink_infra::data::config::compare_set_config(
            &self.pool,
            &key,
            baseline.as_deref(),
            &serialized,
        )
        .await;
        match written {
            Ok(true) => {}
            other => {
                secrets.rollback()?;
                return Err(if matches!(other, Ok(false)) {
                    "config_conflict: 插件配置已变化，请重新读取后重试"
                } else {
                    "plugin_config_write_failed: 保存失败，已恢复密钥，请重试"
                }
                .into());
            }
        }
        secrets.commit();
        self.configs
            .write()
            .unwrap()
            .insert(plugin_id.to_string(), config.clone());
        tracing::debug!(
            plugin = plugin_id,
            revision = config.revision,
            "插件配置代次已提交"
        );
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
    pub fn get_config(&self, id: &str) -> Option<crate::config::PluginConfig> {
        self.configs.read().unwrap().get(id).cloned()
    }

    /// 获取插件的 manifest。
    pub fn get_manifest(&self, id: &str) -> Option<super::PluginManifest> {
        self.find_plugin(id).map(|p| p.manifest().clone())
    }

    /// 列出所有已加载插件的 manifest（0.8.3 §4.6 设置页 context binding 面板用）。
    ///
    /// 用于设置页遍历 `manifest.triggers` 收集所有 `PluginTrigger::Context`。
    pub fn list_manifests(&self) -> Vec<super::PluginManifest> {
        self.plugin_set()
            .plugins
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
        self.find_plugin(id).and_then(|p| {
            p.manifest()
                .runtime
                .empty_arg_hint
                .as_ref()
                .map(|h| h.resolve(lang))
        })
    }

    /// 更新全局代理配置 + 重置所有插件进程(保存后调用)。
    /// 下次 query 自动用新 env 重启，用户零感知热更新。
    pub async fn update_global_proxy(&self, proxy: Option<(String, String)>) {
        let plugins = self.all_plugins();
        // 先更新每个 PluginHandle 的 proxy 字段(新进程 spawn 时会用它)
        for plugin in &plugins {
            plugin.update_proxy(proxy.clone());
        }
        // 杀掉已启动的旧进程(下次 query 自动用新 proxy 重启)
        for plugin in &plugins {
            plugin.reset_process().await;
        }
        tracing::info!("全局代理配置已更新，所有插件进程已重置");
    }

    /// 取插件 settings(None = 无配置或 settings 为 null)。
    ///
    /// **返回脱敏版**——manifest `secret:true` 字段在 SQLite 里只有空串占位
    /// （真实值在 Credential Manager，见 `resolved_settings`）。设置页回显、
    /// setting_bindings 注入等对前端/AI 暴露的路径必须用本函数。
    pub fn get_settings(&self, id: &str) -> Option<serde_json::Value> {
        self.configs
            .read()
            .unwrap()
            .get(id)
            .map(|c| c.settings.clone())
            .filter(|s| !s.is_null())
    }

    // ── 密钥字段管理（0.25.16）──────────────────────────────────────────────

    /// 插件 manifest 声明的密钥字段 key 列表（`secret:true`），无则空。
    fn secret_field_keys(&self, plugin_id: &str) -> Vec<String> {
        self.get_manifest(plugin_id)
            .map(|m| {
                m.settings_schema
                    .iter()
                    .filter(|f| f.secret)
                    .map(|f| f.key.clone())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 密钥字段落地 CM + settings 脱敏（写入咽喉用，0.25.16）。
    ///
    /// settings 中 `secret:true` 字段出现非空字符串值 → 存 CM
    /// （`blink:plugin:{plugin_id}:{key}`）并把该字段改为空串占位——SQLite 永不落明文。
    /// 空串/缺省 → 不动 CM（前端密钥框不回显，空 = 未修改，见 collectSettings 契约）。
    /// 非 Windows 平台无 secret 持久层，原样放行（与 AI 密钥同策略）。
    ///
    /// 返回转存条数。单条 CM 写失败仅 warn 不阻断——settings 侧仍脱敏（宁丢配置不落明文），
    /// 用户重存即可。
    fn sanitize_config_secrets(
        &self,
        plugin_id: &str,
        config: &mut crate::config::PluginConfig,
    ) -> usize {
        let secret_keys = self.secret_field_keys(plugin_id);
        if secret_keys.is_empty() {
            return 0;
        }
        #[cfg(target_os = "windows")]
        {
            use blink_infra::platform::secret::{SecretRef, SecretString, save_secret};
            let mut moved = 0usize;
            if let Some(obj) = config.settings.as_object_mut() {
                for key in &secret_keys {
                    let non_empty = obj
                        .get(key)
                        .and_then(|v| v.as_str())
                        .is_some_and(|s| !s.trim().is_empty());
                    if !non_empty {
                        continue;
                    }
                    let raw = obj.get(key).and_then(|v| v.as_str()).unwrap_or_default();
                    let r = SecretRef::plugin(plugin_id, key);
                    match save_secret(&r, &SecretString::new(raw.to_string())) {
                        Ok(()) => moved += 1,
                        Err(e) => {
                            tracing::warn!(plugin = %plugin_id, field = %key, error = %e, "插件密钥转存 CM 失败");
                        }
                    }
                    obj.insert(key.clone(), serde_json::Value::String(String::new()));
                }
            }
            if moved > 0 {
                tracing::info!(plugin = %plugin_id, count = moved, "插件密钥字段已转存 CM");
            }
            moved
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (plugin_id, &mut *config);
            0
        }
    }

    /// 取插件 settings 并还原密钥字段明文（**只供喂插件进程的路径**，0.25.16）。
    ///
    /// `get_settings` 脱敏版 + CM 明文覆盖。调用方：`query_subset` /
    /// `PluginCapabilityAdapter`（喂子进程 stdin）。**禁止**用于设置页回显、
    /// setting_bindings 注入等对前端/AI 暴露的路径。CM 无值的占位保持空串（=未配置）。
    pub async fn resolved_settings(&self, id: &str) -> Option<serde_json::Value> {
        self.resolved_settings_snapshot(id).await.0
    }

    /// 与明文 settings 一起返回其配置 revision，供异步查询在完成时校验代次。
    async fn resolved_settings_snapshot(
        &self,
        id: &str,
    ) -> (Option<serde_json::Value>, Option<u64>) {
        // 与 CM/SQLite 提交共用更新锁；查询只拿完整旧/新快照，不能拿旧 Host + 新 Key。
        let _guard = self.update_lock.lock().await;
        let Some(config) = self.get_config(id) else {
            return (None, None);
        };
        let revision = config.revision;
        let mut settings = config.settings;
        if settings.is_null() {
            return (None, Some(revision));
        }
        if let Some(obj) = settings.as_object_mut() {
            obj.insert("settings_revision".into(), serde_json::json!(revision));
        }
        let secret_keys = self.secret_field_keys(id);
        if secret_keys.is_empty() {
            return (Some(settings), Some(revision));
        }
        #[cfg(target_os = "windows")]
        {
            use blink_infra::platform::secret::{SecretRef, load_secret};
            let Some(obj) = settings.as_object_mut() else {
                return (Some(settings), Some(revision));
            };
            for key in &secret_keys {
                if let Ok(secret) = load_secret(&SecretRef::plugin(id, key)) {
                    obj.insert(
                        key.clone(),
                        serde_json::Value::String(secret.expose().to_owned()),
                    );
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        let _ = &secret_keys;
        (Some(settings), Some(revision))
    }

    /// 存量明文密钥搬迁到 CM（启动期一次性，0.25.16；幂等由构造保证）。
    ///
    /// 0.25.16 之前密钥字段明文存在 SQLite `plugin:{id}` 的 settings JSON 里。
    /// 本方法扫描每个插件：`secret:true` 字段有非空存量值 → 复用
    /// `sanitize_config_secrets` 转存 CM → 回写 DB。搬迁后 settings 只剩空串占位，
    /// 再次启动扫描为空自然 no-op——无需 marker。
    pub async fn migrate_secret_fields_to_cm(&self) {
        let plugins = self.all_plugins();
        for plugin in &plugins {
            let id = plugin.id();
            let mut config = match self.configs.read().unwrap().get(id).cloned() {
                Some(c) => c,
                None => continue,
            };
            let has_plain = self.secret_field_keys(id).iter().any(|key| {
                config
                    .settings
                    .get(key)
                    .and_then(|v| v.as_str())
                    .is_some_and(|s| !s.trim().is_empty())
            });
            if !has_plain {
                continue;
            }
            let moved = self.sanitize_config_secrets(id, &mut config);
            if moved == 0 {
                continue;
            }
            match crate::config::set_plugin_config(&self.pool, id, &config).await {
                Ok(()) => {
                    self.configs.write().unwrap().insert(id.to_string(), config);
                    tracing::info!(plugin = %id, count = moved, "存量插件密钥已搬迁到 CM");
                }
                Err(e) => {
                    // DB 回写失败:CM 已有新值,SQLite 明文残留——下次启动重扫重搬（幂等）
                    tracing::warn!(plugin = %id, error = %e, "存量密钥搬迁:回写 DB 失败,下次启动重试");
                }
            }
        }
    }

    /// 读插件密钥字段的首尾掩码（设置页 placeholder 用，0.25.16）。
    ///
    /// 不返回明文——只返回 `format_hint` 结果；未配置返回 None。
    pub fn secret_hint(&self, plugin_id: &str, field_key: &str) -> Option<String> {
        #[cfg(target_os = "windows")]
        {
            use blink_infra::platform::secret::{SecretRef, format_hint, load_secret};
            // 只对 manifest 声明的 secret 字段放行——防任意字段探测
            if !self
                .secret_field_keys(plugin_id)
                .iter()
                .any(|k| k == field_key)
            {
                return None;
            }
            load_secret(&SecretRef::plugin(plugin_id, field_key))
                .ok()
                .map(|s| format_hint(s.expose()))
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = (plugin_id, field_key);
            None
        }
    }

    /// 删除插件密钥字段（设置页「清除」按钮，0.25.16）。幂等——NotFound 视为已删。
    pub async fn clear_secret(
        &self,
        plugin_id: &str,
        field_key: &str,
        expected_revision: u64,
    ) -> Result<(), String> {
        if !self
            .secret_field_keys(plugin_id)
            .iter()
            .any(|k| k == field_key)
        {
            return Err("invalid_arguments: 字段未声明为密钥".into());
        }
        let mut config = self.get_config(plugin_id).unwrap_or_default();
        config.revision = expected_revision;
        config.settings = serde_json::json!({});
        config.settings[field_key] = serde_json::json!("");
        self.update_config_with_store(
            plugin_id,
            &mut config,
            &super::secret_update::PlatformSecretStore,
            vec![(field_key.to_owned(), None)],
        )
        .await
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
        self.plugin_set()
            .plugins
            .iter()
            .find(|p| p.manifest().id == plugin_id)
            .map(|p| p.manifest().name.resolve(lang))
    }
}

/// readiness 的单个 blocker（本地化提示文案随 lang 解析）。
fn blocker(kind: &str, field: Option<&str>, lang: &str) -> serde_json::Value {
    const ZH_HINTS: &[(&str, &str)] = &[
        ("disabled", "插件已禁用，启用后可用"),
        ("missing_secret", "缺少 API 密钥，请在下方配置后再使用"),
        ("missing_dataset", "缺少数据包，安装后可用"),
    ];
    const EN_HINTS: &[(&str, &str)] = &[
        ("disabled", "Plugin is disabled; enable it to use"),
        ("missing_secret", "API keys missing; configure below before use"),
        ("missing_dataset", "Dataset missing; install it to use"),
    ];
    let table = if lang.starts_with("zh") { ZH_HINTS } else { EN_HINTS };
    let hint = table
        .iter()
        .find(|(k, _)| *k == kind)
        .map(|(_, v)| *v)
        .unwrap_or(kind);
    serde_json::json!({ "kind": kind, "field": field, "hint": hint })
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
        context_aware: false,
        color_list_hex: None,
        pinned_until: 0,
    }
}

// ── 单测（0.25.16）────────────────────────────────────────────────────────────
//
// 只覆盖**不触 Credential Manager** 的路径（无 manifest / 无 secret 字段——
// 涉及 CM 读写的 sanitize/迁移属集成路径，与 secret 迁移同策略不自动化，
// 见 spec-backend §二"单测绝不修改真实系统状态"）。

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn resolved_settings_waits_for_a_complete_config_snapshot() {
        let engine = test_engine().await;
        tokio::time::pause();
        let guard = engine.update_lock.lock().await;
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_millis(5),
                engine.resolved_settings("snapshot")
            )
            .await
            .is_err()
        );
        engine.configs.write().unwrap().insert(
            "snapshot".into(),
            crate::config::PluginConfig {
                revision: 3,
                settings: serde_json::json!({"provider":"open_meteo"}),
                ..Default::default()
            },
        );
        drop(guard);
        let settings = engine.resolved_settings("snapshot").await.unwrap();
        assert_eq!(settings["settings_revision"], 3);
        assert_eq!(settings["provider"], "open_meteo");
    }

    async fn test_engine() -> PluginEngine {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:")
            .await
            .expect("内存 SQLite 应可用");
        PluginEngine::new(vec![], pool, None)
    }

    #[tokio::test]
    async fn secret_field_keys_empty_without_manifest() {
        let engine = test_engine().await;
        // 未加载 manifest 的插件无密钥字段
        assert!(engine.secret_field_keys("no.such.plugin").is_empty());
    }

    #[tokio::test]
    async fn resolved_settings_passthrough_when_no_secret_fields() {
        // 无 secret 字段：resolved 与 get_settings 同值（不触 CM 的早退路径）
        let engine = test_engine().await;
        engine.configs.write().unwrap().insert(
            "no.such.plugin".into(),
            crate::config::PluginConfig {
                settings: serde_json::json!({"target_lang": "zh"}),
                ..Default::default()
            },
        );
        assert_eq!(
            engine.resolved_settings("no.such.plugin").await.unwrap(),
            serde_json::json!({"target_lang": "zh", "settings_revision":0})
        );
        // settings 为 null → get_settings 返回 None → resolved 同样 None
        engine.configs.write().unwrap().insert(
            "null.settings".into(),
            crate::config::PluginConfig::default(),
        );
        assert!(engine.resolved_settings("null.settings").await.is_none());
        assert_eq!(
            engine.resolved_settings_snapshot("null.settings").await,
            (None, Some(0)),
            "settings 为 null 时仍需保留配置 revision 用于迟到结果校验"
        );
    }

    #[tokio::test]
    async fn sanitize_config_secrets_noop_without_secret_fields() {
        // 无 secret 字段：settings 原样通过（0 转存）
        let engine = test_engine().await;
        let mut config = crate::config::PluginConfig {
            settings: serde_json::json!({"target_lang": "en"}),
            ..Default::default()
        };
        let moved = engine.sanitize_config_secrets("no.such.plugin", &mut config);
        assert_eq!(moved, 0);
        assert_eq!(config.settings, serde_json::json!({"target_lang": "en"}));
    }

    // ── readiness（0.28.2）───────────────────────────────────────────────────
    //
    // secret 分支的 CM 探测用测试专属 plugin id（readiness.test.*）——
    // secret_hint 只读 load 且条目不存在，不写真实 CM（spec-backend §二）。

    fn readiness_manifest(id: &str, secret: bool) -> std::sync::Arc<crate::plugin::manifest::PluginManifest> {
        use crate::plugin::manifest::{SettingField, SettingType};
        let schema = if secret {
            vec![SettingField {
                key: "api_key".into(),
                kind: SettingType::String,
                title: crate::plugin::LocalizableText::Plain("key".into()),
                description: None,
                default: None,
                options: vec![],
                min: None,
                max: None,
                group: None,
                no_space: false,
                secret: true,
                visible_when: None,
            }]
        } else {
            vec![]
        };
        std::sync::Arc::new(crate::plugin::manifest::PluginManifest {
            schema_version: 1,
            id: id.into(),
            name: crate::plugin::LocalizableText::Plain(id.into()),
            version: "1.0.0".into(),
            host: None,
            protocol_version: 1,
            platforms: vec![],
            description: crate::plugin::LocalizableText::default(),
            builtin: false,
            default_enabled: true,
            runtime: Default::default(),
            triggers: vec![],
            capabilities: vec![],
            settings_schema: schema,
            tools: vec![],
            pages: vec![],
            datasets: vec![],
            args: None,
        })
    }

    async fn engine_with(id: &str, secret: bool) -> PluginEngine {
        let pool = sqlx::SqlitePool::connect("sqlite::memory:").await.unwrap();
        let handle = std::sync::Arc::new(super::PluginHandle::new(
            readiness_manifest(id, secret),
            std::env::temp_dir(),
            None,
        ));
        PluginEngine::new(vec![handle], pool, None)
    }

    #[tokio::test]
    async fn readiness_unknown_plugin_is_none() {
        let engine = test_engine().await;
        assert!(engine.readiness("no.such.plugin", "zh").is_none());
    }

    #[tokio::test]
    async fn readiness_plain_plugin_ready_and_disabled_blocker() {
        let engine = engine_with("readiness.test.plain", false).await;
        let report = engine.readiness("readiness.test.plain", "zh").unwrap();
        assert_eq!(report["ready"], serde_json::json!(true));
        assert_eq!(report["blockers"], serde_json::json!([]));

        // 禁用 → disabled blocker（初始化默认配置后关闭）
        engine.ensure_config_for(&engine.all_plugins()[0]).await;
        let mut cfg = engine.get_config("readiness.test.plain").unwrap();
        cfg.enabled = false;
        engine.configs.write().unwrap().insert(
            "readiness.test.plain".into(),
            cfg,
        );
        let report = engine.readiness("readiness.test.plain", "zh").unwrap();
        assert_eq!(report["ready"], serde_json::json!(false));
        assert_eq!(report["blockers"][0]["kind"], serde_json::json!("disabled"));
        // 英文提示按 lang 解析
        let report_en = engine.readiness("readiness.test.plain", "en").unwrap();
        assert_ne!(report_en["blockers"][0]["hint"], report["blockers"][0]["hint"]);
    }

    #[tokio::test]
    async fn readiness_secret_plugin_missing_and_legacy_plain_configured() {
        let id = "readiness.test.secret";
        let engine = engine_with(id, true).await;
        engine.ensure_config_for(&engine.all_plugins()[0]).await;

        // 全空（占位空串 + CM 无值）→ missing_secret
        let report = engine.readiness(id, "zh").unwrap();
        assert_eq!(report["ready"], serde_json::json!(false));
        assert_eq!(
            report["blockers"][0]["kind"],
            serde_json::json!("missing_secret")
        );

        // 任一字段有非空值（旧明文路径，短路不触 CM）→ 就绪
        let mut cfg = engine.get_config(id).unwrap();
        cfg.settings["api_key"] = serde_json::json!("sk-test");
        engine.configs.write().unwrap().insert(id.into(), cfg);
        let report = engine.readiness(id, "zh").unwrap();
        assert_eq!(report["ready"], serde_json::json!(true));
    }
}

#[cfg(test)]
#[path = "query_tests.rs"]
mod query_tests;
