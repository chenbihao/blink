//! PluginEngine(见 §3.5):聚合所有 builtin 插件,作为 async lane 的一路召回源。
//!
//! 对 SearchService 透明(一个 `dyn SearchEngine`,id="plugin",lane=Async)。
//! search 时按 query 匹配各插件的 keyword trigger(精确/前缀),命中则查询对应插件,
//! 结果转 SearchItem。多插件结果合并返回。
//!
//! 本切片:keyword 命中只决定「是否查该插件」,不做独占(独占语义留给 RuleRouter §4.3)。

use std::sync::Arc;

use crate::search::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use crate::search::to_pinyin_initials;

use super::manifest::PluginTrigger;
use super::process::PluginHandle;
use super::protocol::{PluginAction, PluginItem};

pub struct PluginEngine {
    plugins: Vec<Arc<PluginHandle>>,
}

impl PluginEngine {
    pub fn new(plugins: Vec<Arc<PluginHandle>>) -> Self {
        PluginEngine { plugins }
    }
}

/// 一次命中:命中的插件 + 传给插件的参数(前缀命中时为余下文本,精确命中时为空)。
struct Match<'a> {
    plugin: &'a Arc<PluginHandle>,
    arg: String,
}

#[async_trait::async_trait]
impl SearchEngine for PluginEngine {
    fn id(&self) -> &'static str {
        "plugin"
    }

    fn lane(&self) -> Lane {
        Lane::Async
    }

    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let matches = self.matching_plugins(query);
        if matches.is_empty() {
            return Vec::new();
        }
        let mut items = Vec::new();
        for m in matches {
            match m.plugin.query(&m.arg).await {
                Ok(plugin_items) => {
                    for it in plugin_items {
                        items.push(to_search_item(m.plugin.id(), it));
                    }
                }
                Err(e) => {
                    tracing::warn!(plugin = %m.plugin.id(), error = %e, "插件查询失败");
                }
            }
        }
        items
    }
}

impl PluginEngine {
    /// 找出 query 命中的插件(遍历各插件 keyword trigger)。
    fn matching_plugins(&self, query: &str) -> Vec<Match<'_>> {
        let q = query.trim();
        let mut matches = Vec::new();
        for plugin in &self.plugins {
            for trigger in &plugin.manifest().triggers {
                let PluginTrigger::Keyword { keyword, .. } = trigger else {
                    continue; // regex 本切片不实现
                };
                if let Some(arg) = match_keyword(q, keyword) {
                    matches.push(Match { plugin, arg });
                    break; // 一个插件命中一次即可
                }
            }
        }
        matches
    }
}

/// keyword 匹配(§4.2):精确(query==keyword)或前缀带参(`keyword ` 开头,余下为参数)。
/// keyword 同时按原文小写和拼音首字母两种归一化尝试,使中文 keyword(如"天气")
/// 支持首拼输入(`tq`)。命中返回插件参数(精确→空串;前缀→余下文本,保留原大小写)。
fn match_keyword(query: &str, keyword: &str) -> Option<String> {
    let q_lower = query.to_ascii_lowercase();
    let candidates = [keyword.to_ascii_lowercase(), to_pinyin_initials(keyword)];
    for kw in candidates.iter().filter(|k| !k.is_empty()) {
        if q_lower == *kw {
            return Some(String::new());
        }
        let prefix = format!("{kw} ");
        if q_lower.starts_with(&prefix) {
            // keyword 与分隔符均为 ASCII,字节偏移与原串一致;用原 query 切片保留参数大小写。
            return Some(query[prefix.len()..].trim().to_string());
        }
    }
    None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_keyword_match() {
        assert_eq!(match_keyword("echo", "echo"), Some(String::new()));
    }

    #[test]
    fn prefix_keyword_match_keeps_arg_case() {
        assert_eq!(match_keyword("echo Hello", "echo"), Some("Hello".to_string()));
    }

    #[test]
    fn no_match() {
        assert_eq!(match_keyword("echobar", "echo"), None);
        assert_eq!(match_keyword("chrome", "echo"), None);
    }

    #[test]
    fn pinyin_initials_keyword() {
        // 中文 keyword "天气" 首拼 "tq" 命中
        assert_eq!(match_keyword("tq 北京", "天气"), Some("北京".to_string()));
    }
}
