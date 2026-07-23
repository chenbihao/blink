//! `search_apps` Capability（0.11.2 改进 5）。
//!
//! 搜索本机应用 → `Items`。**共享** SearchService 已实例化的 `StartMenuEngine`，
//! 不重复扫描（与 `search_files` 自持 `FileEngine` 实例不同）。
//!
//! **不破四域墙**：只返回数据不执行打开。用户回车走 `Open`（与查询路径一致），
//! 或 AI 调 `open_path`（Turn 2 链式，0.11.4 回流）才穿过信任边界。
//!
//! **sensitive=true**：读应用列表属隐私敏感数据，0.12 MCP server 暴露时需授权。

use std::sync::Arc;

use serde_json::{Value, json};
use tauri::Manager;

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};
use crate::domain::search::SearchService;
use crate::domain::search::engine::SearchAction;

/// `search_apps` — 按关键词搜索本机应用（开始菜单）。
///
/// 入参：`{ "query": "vscode", "max_results": 5 }`（max_results 可选，默认 5）。
/// 出参：`Items { items: Vec<ItemResult> }`，payload 含 `{path, name, score}`。
///
/// **复用**：StartMenuEngine 的缓存 + 增量扫描 + fuzzy 打分全部白嫖
/// （通过 SearchService 共享实例，不持独立引擎）。
pub struct SearchApps;

impl Default for SearchApps {
    fn default() -> Self {
        SearchApps
    }
}

#[async_trait::async_trait]
impl Capability for SearchApps {
    fn id(&self) -> &str {
        "search_apps"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "search_apps".into(),
            description: "搜索本机应用（开始菜单），返回匹配应用列表含路径。用户说「打开 VSCode」时用此工具查找应用路径。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "应用名关键词（支持中英文/拼音首字母）"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "最大返回数（可选，默认 5）",
                        "default": 5
                    }
                },
                "required": ["query"]
            }),
            sensitive: true, // §2.3 读应用列表属隐私敏感数据
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let query = args.get("query").and_then(Value::as_str).ok_or_else(|| {
            CapabilityError::InvalidArgs {
                detail: "缺少 query 参数".into(),
            }
        })?;

        if query.trim().is_empty() {
            return Err(CapabilityError::InvalidArgs {
                detail: "query 不能为空".into(),
            });
        }

        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "search_apps 截止时刻已过，不启动搜索".into(),
            });
        }

        // max_results 默认 5（§3.3 D5 从源头限制 AI 拿到的结果数，省 token）
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|n| n as usize)
            .unwrap_or(5);

        // 通过 AppHandle 拿 SearchService（共享 StartMenuEngine 实例）
        let search_service = ctx.app_handle.state::<Arc<SearchService>>();

        // 铁则 1：用 deadline 包裹 search
        let items = tokio::time::timeout_at(
            ctx.deadline_or_far_future(),
            search_service.search_apps_for_capability(query, max_results),
        )
        .await
        .map_err(|_| CapabilityError::Timeout {
            detail: format!("search_apps 超时（query: {query}）"),
        })?;

        // SearchItem → ItemResult
        // payload 放 {path, name, score}——AI 读路径+置信度，用户回车走 Open
        let results: Vec<_> = items
            .into_iter()
            .map(|item| {
                let path = match &item.action {
                    SearchAction::Open { path } => Some(path.clone()),
                    _ => None,
                };
                let mut payload = json!({});
                if let Some(ref p) = path {
                    payload["path"] = json!(p);
                }
                payload["name"] = json!(item.title);
                payload["score"] = json!(item.score);

                crate::domain::capability::ItemResult {
                    title: item.title,
                    subtitle: item.subtitle,
                    payload,
                    score: Some(item.score),
                }
            })
            .collect();

        tracing::debug!(query = %query, count = results.len(), "search_apps 完成");
        Ok(CapabilityResult::Items { items: results })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(SearchApps) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_search_apps() {
        assert_eq!(SearchApps.id(), "search_apps");
    }

    #[test]
    fn schema_requires_query() {
        let s = SearchApps.schema();
        assert_eq!(s.name, "search_apps");
        let required = s.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }

    #[test]
    fn schema_has_optional_max_results() {
        let s = SearchApps.schema();
        assert_eq!(s.parameters["properties"]["max_results"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["max_results"]["default"], 5);
        let required = s.parameters["required"].as_array().unwrap();
        assert!(!required.contains(&json!("max_results")));
    }

    #[test]
    fn schema_sensitive_is_true() {
        // 0.11.2 §2.3: search_apps 标 sensitive=true（读应用列表属隐私敏感数据）
        let s = SearchApps.schema();
        assert!(s.sensitive, "search_apps 必须 sensitive=true");
    }

    #[test]
    fn schema_description_mentions_app_search() {
        let s = SearchApps.schema();
        assert!(s.description.contains("应用"));
    }
}
