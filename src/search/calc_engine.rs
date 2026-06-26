//! CalcEngine:实时计算引擎(sync lane)。包 [`crate::calc::try_eval`]。
//!
//! 命中(输入为合法算术表达式)产出单条 `Copy` 项,title 形如 `= 2`、text = 结果值。
//! 前端 `actions.js` 复制优先取 `action.payload`(= text);契约见 [`super::engine`]。

use crate::calc;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

pub struct CalcEngine;

#[async_trait::async_trait]
impl SearchEngine for CalcEngine {
    fn id(&self) -> &'static str {
        "calc"
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        match calc::try_eval(query) {
            Some(result) => vec![SearchItem {
                id: format!("calc:{}", query.trim()),
                title: format!("= {result}"),
                subtitle: Some("按 Enter 复制结果".into()),
                score: super::scorer::calc_score(),
                action: SearchAction::Copy { text: result },
                source: "calc".into(),
            }],
            None => Vec::new(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn run(q: &str) -> Vec<SearchItem> {
        let h = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext { history: &h, snapshot: &snapshot };
        // 引擎 search 是 async,但 CalcEngine 内部纯同步,用 block_on 跑测试
        tauri::async_runtime::block_on(CalcEngine.search(q, &ctx))
    }

    #[test]
    fn hit_produces_copy_item() {
        let items = run("1+1");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "= 2");
        assert!(matches!(&items[0].action, SearchAction::Copy { text } if text == "2"));
        assert_eq!(items[0].source, "calc");
    }

    #[test]
    fn miss_returns_empty() {
        assert!(run("hello").is_empty());
        assert!(run("123").is_empty()); // 纯数字不算计算
    }
}
