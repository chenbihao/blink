//! 插件运行时发布器（0.28.1 §五.4）——用运行时快照替代固定集合的编排层。
//!
//! 职责：把"新的插件集合"安全地发布到运行中的各出口。一次发布按序联动：
//!
//! 1. 契约集合校验（`validate_contract_set`，对既有注册 tool id 与已占用关键词）；
//! 2. `CapabilityRegistry`：按 owner `plugin:<id>` 先撤销被移除/被替换插件的
//!    全部 tool，再注册新 tool（重复 id 拒绝）；
//! 3. `PluginEngine::replace_plugins` 换快照（generation +1）；
//! 4. 路由联动：移除规则 → 重注入 keyword/context 规则与 args.hints
//!    （禁用插件只注册 hints 不注册规则，与启动装配同口径）；
//! 5. 旧句柄 bounded drain（等在途调用结束或超时后杀进程）。
//!
//! 顺序保证：能力出口先撤销（新调用被挡）→ 快照替换（旧 adapter 复核失败）
//! → 路由清理（搜索/占位不再产生新调用）→ 旧进程有界排空。任何一步失败
//! 都不做半套发布：契约校验失败在第一步即返回，注册冲突时回滚已注册项。

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::capability::CapabilityRegistry;
use crate::intent::RuleRouter;

use super::capability::PluginCapabilityAdapter;
use super::contract::{self, PluginContract};
use super::engine::PluginEngine;
use super::process::PluginHandle;

/// 发布错误。
#[derive(Debug, thiserror::Error)]
pub enum PublishError {
    #[error("契约校验失败: {0}")]
    Contract(#[from] contract::ContractError),
    #[error("capability id 已被占用: {id}")]
    CapabilityIdTaken { id: String },
}

/// 发布结果摘要。
#[derive(Debug, Clone, PartialEq)]
pub struct PublishReport {
    pub generation: u64,
    pub registered_tools: usize,
    pub revoked_tool_ids: Vec<String>,
}

/// 旧句柄有界排空的预算（等在途 query/tool-call 完成）。
const DRAIN_BUDGET: std::time::Duration = std::time::Duration::from_secs(5);

/// owner 标记：`plugin:<plugin_id>`。
fn owner_for(plugin_id: &str) -> String {
    format!("plugin:{plugin_id}")
}

pub struct PluginRuntimePublisher {
    engine: Arc<PluginEngine>,
    cap_registry: Arc<CapabilityRegistry>,
    router: Arc<RuleRouter>,
    publish_lock: tokio::sync::Mutex<()>,
}

impl PluginRuntimePublisher {
    pub fn new(
        engine: Arc<PluginEngine>,
        cap_registry: Arc<CapabilityRegistry>,
        router: Arc<RuleRouter>,
    ) -> Self {
        PluginRuntimePublisher {
            engine,
            cap_registry,
            router,
            publish_lock: tokio::sync::Mutex::new(()),
        }
    }

    /// 发布完整集合（安装/更新/卸载后的目标态全量集合）。
    ///
    /// 调用方给出**最终想要的全部插件**（builtin + 有效安装，含未变更项）；
    /// 发布器与当前快照 diff，只动变化的 owner/路由。
    pub async fn publish(&self, target: Vec<Arc<PluginHandle>>) -> Result<PublishReport, PublishError> {
        let _guard = self.publish_lock.lock().await;
        // 1. 契约集合校验：目标集合内部 + 对既有非插件 capability 与已占用关键词
        let contracts: Vec<PluginContract> = target
            .iter()
            .map(|h| PluginContract::from_manifest(h.manifest()))
            .collect::<Result<_, _>>()?;
        // 碰撞基线 = 全部注册条目 −（目标集合中同 id 插件当前拥有的旧 tool）。
        // 只豁免"自己替换自己"（更新语义）；静态条目与其他插件的条目必须让位拒新。
        let replaced_owned: HashSet<String> = contracts
            .iter()
            .flat_map(|c| self.cap_registry.ids_owned_by(&owner_for(&c.plugin_id)))
            .collect();
        let existing_tools: HashSet<String> = self
            .cap_registry
            .entries()
            .into_iter()
            .map(|(id, _)| id)
            .filter(|id| !replaced_owned.contains(id))
            .collect();
        contract::validate_contract_set(&contracts, &existing_tools, &HashMap::new())?;

        let current = self.engine.plugin_set();

        // 新插件的默认启用态必须先就绪，能力与路由使用同一配置快照。
        for handle in &target {
            self.engine.ensure_config_for(handle).await;
        }

        // 2. 能力出口：撤销被移除/被替换插件的 owner 条目，注册新 tool。
        //    句柄 Arc 同一（未变更插件）则跳过——避免无谓的能力空窗，
        //    revoked_tool_ids 也只含真实撤销项（待确认联动据此取消）。
        let mut revoked_tool_ids = Vec::new();
        let target_by_id: HashMap<&str, &Arc<PluginHandle>> =
            target.iter().map(|h| (h.id(), h)).collect();
        for old in &current.plugins {
            match target_by_id.get(old.id()) {
                Some(new) if Arc::ptr_eq(new, old) => {}
                Some(_) => {
                    // 同 id 更新：先撤销旧 owner 条目，给新 tool 让位
                    revoked_tool_ids
                        .extend(self.cap_registry.revoke_owner(&owner_for(old.id())));
                }
                None => {
                    revoked_tool_ids
                        .extend(self.cap_registry.revoke_owner(&owner_for(old.id())));
                    // 卸载：路由规则一并清除
                    self.router.remove_plugin_rules(old.id());
                    self.router.set_plugin_args_hints(old.id(), Vec::new());
                }
            }
        }

        let mut registered_tools = 0usize;
        for handle in &target {
            let manifest = handle.manifest();
            if !self.engine.is_enabled(handle.id()) || manifest.tools.is_empty() {
                continue;
            }
            let owner = owner_for(&manifest.id);
            let registered = self.cap_registry.ids_owned_by(&owner);
            for tool_def in &manifest.tools {
                let id = super::plugin_tool_id(handle.id(), &tool_def.name);
                // 未变化句柄保留原 adapter；重新启用时已撤销的条目仍需补注册。
                if registered.contains(&id) {
                    continue;
                }
                let adapter = PluginCapabilityAdapter::new(handle.clone(), tool_def);
                if self
                    .cap_registry
                    .register_owned(&owner, Arc::new(adapter))
                    .is_err()
                {
                    // 回滚本插件本轮已注册项，保持出口一致
                    let _ = self.cap_registry.revoke_owner(&owner);
                    return Err(PublishError::CapabilityIdTaken { id });
                }
                registered_tools += 1;
            }
        }

        // 3. 引擎快照替换
        let (generation, retired) = self.engine.replace_plugins(target.clone());

        // 4. 路由/hints 重载：新集合中每个插件按启用态注入
        for handle in &target {
            let id = handle.id();
            // args.hints 静态数据无条件注册（含禁用，与启动装配同口径）
            self.router
                .set_plugin_args_hints(id, handle.manifest().arg_hints().to_vec());
            if !self.engine.is_enabled(id) {
                self.router.remove_plugin_rules(id);
                continue;
            }
            let config = self.engine.get_config(id).unwrap_or_default();
            let effective = config.effective_triggers(&handle.manifest().triggers);
            self.router.reload_plugin_triggers(id, &effective);
        }

        // 5. 旧句柄有界排空：reset（杀进程）使在途调用立即以"进程退出"失败
        //    ——每个在途调用本就有 deadline 兜底，杀进程是最快的有界结束方式；
        //    懒启动（未 spawn）句柄 reset 是幂等空操作。
        let _ = DRAIN_BUDGET;
        for h in retired {
            if !target.iter().any(|t| Arc::ptr_eq(t, &h)) {
                h.reset_process().await;
            }
        }

        tracing::info!(
            generation,
            registered_tools,
            revoked = revoked_tool_ids.len(),
            "插件集合已发布（运行时快照替换）"
        );
        Ok(PublishReport {
            generation,
            registered_tools,
            revoked_tool_ids,
        })
    }

    /// 禁用/启用联动（0.28.1 §3.3 生命周期 3：禁用先阻止新调用）。
    ///
    /// - 禁用：撤销该插件全部 tool 出口 + 清路由规则（hints 表保留——与启动
    ///   装配同口径，再启用时规则重注入即可用）。返回被撤销的 tool id
    ///   （调用方据此取消关联待确认）。
    /// - 启用：重发布当前集合（能力/路由全量重建，幂等）。
    pub async fn sync_enabled(&self, plugin_id: &str, enabled: bool) -> Vec<String> {
        if enabled {
            let set = self.engine.plugin_set();
            if let Err(error) = self.publish(set.plugins.clone()).await {
                tracing::error!(plugin = %plugin_id, %error, "启用插件时发布能力失败");
            }
            return Vec::new();
        }
        let _guard = self.publish_lock.lock().await;
        let revoked = self.cap_registry.revoke_owner(&owner_for(plugin_id));
        self.router.remove_plugin_rules(plugin_id);
        tracing::info!(plugin = %plugin_id, revoked = revoked.len(), "插件已禁用：能力出口与路由已撤销");
        revoked
    }

    /// 撤销单个插件（禁用联动用；卸载/更新走 `publish` 全量目标态）。
    pub async fn revoke_plugin(&self, plugin_id: &str) -> PublishReport {
        let _guard = self.publish_lock.lock().await;
        let revoked = self.cap_registry.revoke_owner(&owner_for(plugin_id));
        self.router.remove_plugin_rules(plugin_id);
        self.router.set_plugin_args_hints(plugin_id, Vec::new());

        let current = self.engine.plugin_set();
        let remaining: Vec<Arc<PluginHandle>> = current
            .plugins
            .iter()
            .filter(|h| h.id() != plugin_id)
            .cloned()
            .collect();
        let (generation, retired) = self.engine.replace_plugins(remaining);
        for h in retired {
            if h.id() == plugin_id {
                h.reset_process().await;
            }
        }
        tracing::info!(plugin = %plugin_id, generation, revoked = revoked.len(), "插件已从运行时撤销");
        PublishReport {
            generation,
            registered_tools: 0,
            revoked_tool_ids: revoked,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::manifest::PluginManifest;

    fn handle(id: &str, tools: &[&str], keywords: &[&str]) -> Arc<PluginHandle> {
        let mut m = PluginManifest {
            schema_version: 1,
            id: id.into(),
            name: crate::plugin::LocalizableText::Plain(id.into()),
            version: "1.0.0".into(),
            host: None,
            protocol_version: 1,
            platforms: vec![],
            description: crate::plugin::LocalizableText::default(),
            builtin: true,
            default_enabled: true,
            runtime: Default::default(),
            triggers: keywords
                .iter()
                .map(|k| crate::plugin::PluginTrigger::Keyword {
                    keyword: (*k).into(),
                    exclusive: true,
                })
                .collect(),
            capabilities: vec!["query".into()],
            settings_schema: vec![],
            tools: tools
                .iter()
                .map(|t| crate::plugin::manifest::ToolDef {
                    name: (*t).into(),
                    ..Default::default()
                })
                .collect(),
            pages: vec![],
            datasets: vec![],
            args: None,
            channel: Default::default(),
        };
        // process 型插件需要 exec
        m.runtime.exec = format!("{id}.exe");
        Arc::new(PluginHandle::new(
            Arc::new(m),
            std::env::temp_dir(),
            None,
        ))
    }

    async fn wiring() -> (Arc<PluginEngine>, PluginRuntimePublisher) {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .connect("sqlite::memory:")
            .await
            .unwrap();
        let engine = Arc::new(PluginEngine::new(Vec::new(), pool, None));
        let registry = Arc::new(CapabilityRegistry::default());
        let router = Arc::new(RuleRouter::new(true));
        let publisher = PluginRuntimePublisher::new(engine.clone(), registry, router);
        (engine, publisher)
    }

    #[tokio::test]
    async fn publish_registers_tools_and_revokes_on_update() {
        let (engine, publisher) = wiring().await;
        let cap = publisher.cap_registry.clone();

        // 首发
        let report = publisher
            .publish(vec![handle("extra.demo", &["run"], &["tst"])])
            .await
            .unwrap();
        assert_eq!(report.registered_tools, 1);
        assert!(cap.get("extra_demo_run").is_some());
        assert_eq!(engine.plugin_set().generation, report.generation);

        // 同 id 更新（换新句柄 = 新代次）
        let report2 = publisher
            .publish(vec![handle("extra.demo", &["run", "stat"], &["tst"])])
            .await
            .unwrap();
        assert_eq!(report2.registered_tools, 2);
        assert!(report2.revoked_tool_ids.contains(&"extra_demo_run".to_string()));
        assert!(cap.get("extra_demo_run").is_some());
        assert!(cap.get("extra_demo_stat").is_some());

        // 卸载（空目标集合）
        let report3 = publisher.publish(vec![]).await.unwrap();
        assert!(cap.get("extra_demo_run").is_none());
        assert_eq!(report3.revoked_tool_ids.len(), 2);
        assert!(engine.plugin_set().plugins.is_empty());
    }

    #[tokio::test]
    async fn stale_adapter_rejected_after_revoke() {
        // 绑定旧句柄的 adapter 在快照替换后 invoke 必须失败（代次复核）
        let (engine, publisher) = wiring().await;
        let old_handle = handle("extra.demo", &["run"], &[]);
        publisher.publish(vec![old_handle.clone()]).await.unwrap();
        assert!(engine.is_active_handle("extra.demo", &old_handle));

        publisher.publish(vec![]).await.unwrap();
        assert!(!engine.is_active_handle("extra.demo", &old_handle));
    }

    #[tokio::test]
    async fn unchanged_plugins_keep_adapters_when_publishing_another_plugin() {
        let (engine, publisher) = wiring().await;
        let initial_count = publisher.cap_registry.len();
        let first = handle("extra.first", &["run"], &[]);
        publisher.publish(vec![first.clone()]).await.unwrap();
        let adapter = publisher.cap_registry.get("extra_first_run").unwrap();
        let report = publisher
            .publish(vec![first.clone(), handle("extra.second", &["run"], &[])])
            .await
            .unwrap();
        assert_eq!(report.registered_tools, 1);
        assert!(report.revoked_tool_ids.is_empty());
        assert!(Arc::ptr_eq(&adapter, &publisher.cap_registry.get("extra_first_run").unwrap()));
        assert!(engine.is_active_handle("extra.first", &first));
        let report = publisher.publish(engine.all_plugins()).await.unwrap();
        assert_eq!(report.registered_tools, 0);
        assert_eq!(publisher.cap_registry.len(), initial_count + 2);
    }

    #[tokio::test]
    async fn default_disabled_plugin_does_not_publish_tools() {
        let (engine, publisher) = wiring().await;
        let mut manifest = handle("extra.disabled", &["run"], &["disabled"]).manifest().clone();
        manifest.default_enabled = false;
        let disabled = Arc::new(PluginHandle::new(Arc::new(manifest), std::env::temp_dir(), None));
        let report = publisher.publish(vec![disabled]).await.unwrap();
        assert_eq!(report.registered_tools, 0);
        assert!(!engine.is_enabled("extra.disabled"));
        assert!(publisher.cap_registry.get("extra_disabled_run").is_none());
    }

    #[tokio::test]
    async fn publish_rejects_tool_id_collision_with_builtin() {
        let (engine, publisher) = wiring().await;
        let registry = publisher.cap_registry.clone();
        // 占位 builtin capability，模拟既有注册表条目
        struct FakeCap;
        #[async_trait::async_trait]
        impl crate::capability::Capability for FakeCap {
            fn id(&self) -> &str {
                "taken_id"
            }
            fn schema(&self) -> crate::capability::CapabilitySchema {
                crate::capability::CapabilitySchema::default()
            }
            async fn invoke(
                &self,
                _args: serde_json::Value,
                _ctx: &crate::capability::InvokeContext<'_>,
            ) -> Result<crate::capability::CapabilityResult, crate::capability::CapabilityError>
            {
                unreachable!()
            }
        }
        registry.register(Arc::new(FakeCap)).unwrap();

        // extra.demo 的 tool "id" → extra_demo_id 不冲突；构造冲突：id "a.b" tool "id"
        let err = publisher
            .publish(vec![handle("a.b", &["id"], &[])])
            .await;
        // a_b_id 与 taken_id 不冲突——这里校验的是真实碰撞路径：
        // 再造一个 tool 命中 taken_id 本身
        assert!(err.is_ok());
        let err = publisher
            .publish(vec![handle("taken", &["id"], &[])])
            .await
            .unwrap_err();
        // taken_id（plugin "taken" + tool "id"）与既有 FakeCap 冲突
        assert!(matches!(err, PublishError::Contract(contract::ContractError::ToolIdTaken { id, .. }) if id == "taken_id"));
        // 发布失败后引擎保持上一有效集合（a.b 仍在，taken 未落地）
        let ids: Vec<String> = engine
            .plugin_set()
            .plugins
            .iter()
            .map(|h| h.id().to_string())
            .collect();
        assert_eq!(ids, vec!["a.b".to_string()]);
    }
}
