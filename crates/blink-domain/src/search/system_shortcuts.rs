//! 常用系统入口与可选自动发现，共用内存目录和应用搜索配置。
use super::{
    engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem},
    scorer::history_boost,
    system_entries::SystemEntries,
};
use crate::config::StartMenuConfig;
pub struct SystemShortcutEngine {
    pub entries: SystemEntries,
}
impl SystemShortcutEngine {
    pub fn with_config(config: StartMenuConfig) -> Self {
        Self {
            entries: SystemEntries::new(config),
        }
    }
}
#[async_trait::async_trait]
impl SearchEngine for SystemShortcutEngine {
    fn id(&self) -> &'static str {
        "system_shortcut"
    }
    fn lane(&self) -> Lane {
        Lane::Sync
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn start(&self) {
        self.entries.start();
    }
    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        self.entries
            .search(query, ctx.language)
            .into_iter()
            .map(|(entry, base_score)| {
                let (hits, last) = ctx
                    .history
                    .get(&entry.history_key)
                    .copied()
                    .unwrap_or((0, 0));
                SearchItem {
                    id: entry.id.clone(),
                    title: entry.title,
                    subtitle: Some(if ctx.language.starts_with("en") {
                        "Windows system entry".into()
                    } else {
                        "Windows 系统入口".into()
                    }),
                    score: base_score + history_boost(hits, last),
                    action: SearchAction::SystemEntry {
                        entry_id: entry.id,
                        icon_path: entry.icon,
                    },
                    source: "system".into(),
                    score_detail: None,
                    context_aware: false,
                    color_list_hex: None,
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    #[tokio::test]
    async fn system_result_projects_identity_and_lazy_icon_and_keeps_old_history_boost() {
        if !cfg!(windows) {
            return;
        }
        let engine = SystemShortcutEngine::with_config(StartMenuConfig::default());
        let mut history = HashMap::new();
        history.insert(
            "rundll32.exe sysdm.cpl,EditEnvironmentVariables".into(),
            (10, chrono::Utc::now().timestamp()),
        );
        let snapshot = blink_infra::platform::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
        };
        let result = engine.search("编辑账户的环境变量", &ctx).await.remove(0);
        assert!(result.score > 1.0);
        let projected = result.into_app_entry();
        assert!(projected.lnk_path.is_empty());
        assert_eq!(projected.icon_path.as_deref(), Some("rundll32.exe"));
        assert_eq!(
            projected.actions[0].run_id.as_deref(),
            Some("open_system_entry")
        );
        assert_eq!(
            projected.actions[0].run_arg,
            Some(serde_json::json!({"entry_id":"system:environment_variables"}))
        );
    }
}
