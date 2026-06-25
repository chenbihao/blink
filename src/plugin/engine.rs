//! PluginEngine(见 §3.5):聚合所有 builtin 插件的查询执行器。
//!
//! 0.4 退化为纯执行器:不再自匹配(`matching_plugins`/`match_keyword` 上移至 RuleRouter),
//! 改为接收「要查哪些插件 + 各自 arg」的指令列表,直接查询对应插件进程。
//! SearchService 在 `route()` 后按 `Route::Takeover`/`Route::Mixed` 调用 `query_subset`。

use std::sync::Arc;

use tokio::task::JoinSet;

use crate::search::engine::{SearchAction, SearchItem};

use super::process::PluginHandle;
use super::protocol::{PluginAction, PluginItem, PluginQueryContext};

pub struct PluginEngine {
    plugins: Vec<Arc<PluginHandle>>,
}

impl PluginEngine {
    pub fn new(plugins: Vec<Arc<PluginHandle>>) -> Self {
        PluginEngine { plugins }
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

                serde_json::json!({
                    "id": manifest.id,
                    "name": manifest.name,
                    "version": manifest.version,
                    "description": manifest.description,
                    "triggers": triggers,
                    "enabled": true,
                })
            })
            .collect()
    }

    /// 按给定候选列表查询插件。每个候选 = (plugin_id, arg)。
    /// 多插件并发,各自内部有 timeout 兜底。结果顺序无关——融合层按 score 重排。
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
            let Some(plugin) = self.find_plugin(id) else {
                tracing::debug!(plugin_id = %id, "query_subset: 插件未找到");
                continue;
            };
            let plugin_id = id.clone();
            let arg = arg.clone();
            let context = context.clone();
            set.spawn(async move {
                match plugin.query(&arg, &context).await {
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
