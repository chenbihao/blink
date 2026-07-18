//! `search_files` Capability（0.9.7 Step 2）。
//!
//! 搜索文件 → `Items`。包装 `FileEngine`，复用其 config 分流（everything/local/auto）。
//!
//! **包装策略**：持有 `FileEngine` 实例，调其 `SearchEngine::search()` trait 方法。
//! FileEngine 的 config 分流 + Everything HTTP + 本地 fallback 全部白嫖。
//!
//! **SearchItem → ItemResult 转换**：payload 放 `{ "path": "..." }`，
//! 主窗口可据此打开文件，AI 可读 JSON 上下文。

use std::collections::HashMap;
use std::sync::Arc;

use serde_json::{Value, json};
use tauri::Manager;

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};
use crate::domain::search::engine::{QueryContext, SearchEngine};
use crate::domain::search::file_engine::FileEngine;
use crate::infra::platform::context::ContextSnapshot;

/// `search_files` — 按关键词搜索本地文件。
///
/// 入参：`{ "query": "...", "max_results": 20 }`（max_results 可选，默认跟随 config）。
/// 出参：`Items { items: Vec<ItemResult> }`。
pub struct SearchFiles {
    engine: FileEngine,
}

impl SearchFiles {
    pub fn new() -> Self {
        Self {
            engine: FileEngine::new(),
        }
    }
}

impl Default for SearchFiles {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl Capability for SearchFiles {
    fn id(&self) -> &str {
        "search_files"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "search_files".into(),
            description: "按关键词搜索本地文件（Everything / 本地目录），返回匹配文件列表。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词（文件名/路径片段）"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "最大返回数（可选，默认跟随配置）",
                        "default": 20
                    }
                },
                "required": ["query"]
            }),
            ..Default::default()
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

        // 铁则 1 前置检查：deadline 已过则直接返回，不启动搜索。
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "search_files 截止时刻已过，不启动搜索".into(),
            });
        }

        // 从 SQLite 加载用户真实 FileSearchConfig（而非默认值）。
        // 每次 invoke 都加载——SQLite KV 查询快（<1ms），且保证配置热更新生效。
        let pool = ctx.app_handle.state::<sqlx::SqlitePool>();
        let mut fs_config = crate::app::config::get_file_search_config(&pool).await;
        // AI 可通过 args 覆盖 max_results
        if let Some(max) = args.get("max_results").and_then(Value::as_u64) {
            fs_config.max_results = max as u32;
        }
        self.engine.update_config(fs_config).await;

        // 构造最小 QueryContext——FileEngine::search 不消费 ctx 字段（签名 _ctx），
        // 但 trait 要求传，给空的即可（与 service.rs spawn_mixed_lane 同模式）。
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let disabled: Vec<String> = Vec::new();
        let search_ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
        };

        // 铁则 1：用 ctx.deadline 包裹 search——Everything 不响应时不在 deadline 上挂死。
        // FileEngine 内部已有 3s reqwest 超时，deadline 是第二道防线（AI 总预算）。
        let items = tokio::time::timeout_at(
            ctx.deadline_or_far_future(),
            self.engine.search(query, &search_ctx),
        )
        .await
        .map_err(|_| CapabilityError::Timeout {
            detail: format!("search_files 超时（query: {query}）"),
        })?;

        // SearchItem → ItemResult（payload 放 path，前端/AI 各取所需）
        let results: Vec<_> = items
            .into_iter()
            .map(|item| {
                // 从 SearchAction::Open { path } 提取路径
                let path = match &item.action {
                    crate::domain::search::engine::SearchAction::Open { path } => {
                        Some(path.clone())
                    }
                    _ => None,
                };
                crate::domain::capability::ItemResult {
                    title: item.title,
                    subtitle: item.subtitle,
                    payload: path.map(|p| json!({ "path": p })).unwrap_or(json!({})),
                    score: Some(item.score),
                }
            })
            .collect();

        tracing::debug!(query = %query, count = results.len(), "search_files 完成");
        Ok(CapabilityResult::Items { items: results })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(SearchFiles::new()) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_search_files() {
        assert_eq!(SearchFiles::new().id(), "search_files");
    }

    #[test]
    fn schema_requires_query() {
        let s = SearchFiles::new().schema();
        assert_eq!(s.name, "search_files");
        let required = s.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("query")));
    }

    #[test]
    fn schema_has_optional_max_results() {
        let s = SearchFiles::new().schema();
        assert_eq!(s.parameters["properties"]["max_results"]["type"], "integer");
        // max_results 不在 required 里
        let required = s.parameters["required"].as_array().unwrap();
        assert!(!required.contains(&json!("max_results")));
    }
}
