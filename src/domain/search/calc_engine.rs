//! CalcEngine:实时计算引擎(sync lane)。包 [`super::calc::try_eval`]。
//!
//! 命中(输入为合法算术表达式)产出单条 `Copy` 项,title 形如 `= 2`、text = 结果值。
//! 前端 `actions.js` 复制优先取 `action.payload`(= text);契约见 [`super::engine`]。

use std::sync::{Arc, RwLock};

use super::calc;
use crate::domain::config::CalcConfig;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

pub struct CalcEngine {
    config: Arc<RwLock<CalcConfig>>,
}

impl CalcEngine {
    pub fn with_config(config: CalcConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// 更新配置（供 SearchService 调用）。
    pub fn update_config(&self, config: CalcConfig) {
        let mut cfg = self.config.write().unwrap();
        *cfg = config;
    }
}

#[async_trait::async_trait]
impl SearchEngine for CalcEngine {
    fn id(&self) -> &'static str {
        "calc"
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        // 检查是否启用
        {
            let cfg = self.config.read().unwrap();
            if !cfg.enabled {
                tracing::trace!("CalcEngine: 已禁用，跳过");
                return Vec::new();
            }
        }

        match calc::try_eval(query) {
            Some(result) => vec![SearchItem {
                id: format!("calc:{}", query.trim()),
                title: format!("= {result}"),
                subtitle: Some("按 Enter 复制结果".into()),
                score: super::scorer::calc_score(),
                action: SearchAction::Copy {
                    text: result,
                    hit_id: None,
                },
                source: "calc".into(),
                score_detail: Some("calc=1.0".into()),
                context_aware: false,
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    async fn run(q: &str) -> Vec<SearchItem> {
        let engine = CalcEngine::with_config(CalcConfig::default());
        let h = HashMap::new();
        let snapshot = crate::infra::platform::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &h,
            snapshot: &snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
        };
        // 0.14.7 W1: 改用 #[tokio::test]，domain 不再依赖 tauri runtime
        engine.search(q, &ctx).await
    }

    #[tokio::test]
    async fn hit_produces_copy_item() {
        let items = run("1+1").await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "= 2");
        assert!(
            matches!(&items[0].action, SearchAction::Copy { text, hit_id: None } if text == "2")
        );
        assert_eq!(items[0].source, "calc");
    }

    #[tokio::test]
    async fn miss_returns_empty() {
        assert!(run("hello").await.is_empty());
        assert!(run("123").await.is_empty()); // 纯数字不算计算
    }
}
