//! `search_clipboard_history` Capability（0.11.5 改进 6）。
//!
//! 搜索剪贴板历史 → `Items`。直接查 `clipboard` 表（与 ClipboardEngine 同表独立逻辑）。
//!
//! **与 `read_clipboard` 的边界**：read = 当前剪贴板（快，无 DB 查询）；
//! search_history = 历史检索（需 DB，带 query 模糊匹配）。两者并存，AI 按场景选。
//!
//! **sensitive=true**：读剪贴板历史属隐私敏感数据，0.12 MCP server 暴露时需授权。
//!
//! **不破四域墙**：只读历史（Awareness 数据的持久化投影），不写入、不执行。
//! 用户回车复制才穿过边界。
//!
//! **改进 9 SearchCapability trait**：search_files / search_apps / 本 Capability
//! 三者数据获取方式差异太大（自持 FileEngine / 共享 StartMenuEngine / 直接查 DB），
//! 共同逻辑仅 deadline 检查几行，抽 trait 价值有限，先记债。等第四个搜索 Capability
//! 出现时再评估——继续遵循"避免过早设计"原则。

use std::sync::Arc;

use serde_json::{Value, json};
use tauri::Manager;

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};
use crate::infra::data::clipboard::{query_recent, search as search_history};

/// `search_clipboard_history` — 搜索剪贴板历史记录。
///
/// 入参：`{ "query": "", "max_results": 30 }`（query 空=最近 N 条）。
/// 出参：`Items`，每项 payload 含 `{id, text, kind}`。
pub struct SearchClipboardHistory;

impl Default for SearchClipboardHistory {
    fn default() -> Self {
        SearchClipboardHistory
    }
}

#[async_trait::async_trait]
impl Capability for SearchClipboardHistory {
    fn id(&self) -> &str {
        "search_clipboard_history"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "search_clipboard_history".into(),
            description: "搜索剪贴板历史记录。用户说「我刚才复制的那个链接」时用此工具。query 为空返回最近 N 条，非空做模糊搜索。每项 payload 含 id+text+kind，用户回车复制。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "搜索关键词（空=最近 N 条）"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "最大返回数（可选，默认 30，对齐 retention_days 30 天保留期）",
                        "default": 30
                    }
                }
                // query 可选（空=最近 N 条），故无 required 数组
            }),
            sensitive: true, // §2.3 读剪贴板历史属隐私敏感数据
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "search_clipboard_history 截止时刻已过".into(),
            });
        }

        let query = args.get("query").and_then(Value::as_str).unwrap_or("");
        // max_results 默认 30（与 retention_days=30 对齐，查近 30 天的记录）
        let max_results = args
            .get("max_results")
            .and_then(Value::as_u64)
            .map(|n| n as i64)
            .unwrap_or(30);

        let pool = &ctx
            .app_handle
            .state::<crate::infra::data::DbPools>()
            .history;

        // 铁则 1：用 deadline 包裹 DB 查询
        let items = tokio::time::timeout_at(ctx.deadline_or_far_future(), async {
            if query.trim().is_empty() {
                query_recent(&pool, max_results).await
            } else {
                search_history(&pool, query, max_results).await
            }
        })
        .await
        .map_err(|_| CapabilityError::Timeout {
            detail: format!("search_clipboard_history 超时（query: {query}）"),
        })?;

        // ClipboardItem → ItemResult
        // data: {id, text, kind} —— AI 读 id+text，用户回车复制（Copy 动作）
        let results: Vec<_> = items
            .into_iter()
            .map(|item| {
                let data = json!({
                    "id": item.id,
                    "text": item.text,
                    "kind": "text" // 当前 clipboard 表只存 text；0.12 扩展 image/file 时补 kind 分类
                });
                crate::domain::capability::ItemResult {
                    data,
                    desc: Some(format_relative_time(item.created_at)),
                    actions: vec![crate::domain::capability::ItemAction::Copy { pointer: Some("$.text".into()) }],
                }
            })
            .collect();

        tracing::debug!(query = %query, count = results.len(), "search_clipboard_history 完成");
        Ok(CapabilityResult::Items { items: results })
    }
}

/// 相对时间格式化（简单版，与 ClipboardEngine::format_subtitle 同语义但独立实现）。
///
/// 不复用 ClipboardEngine 的方法（它是 SearchEngine trait 实现，方法私有）。
/// 若未来需 zh/en 切换，抽 helper 供两者共用（文档 §2.6 ★ 复用 kind 分类）。
fn format_relative_time(created_at: i64) -> String {
    let now = chrono::Utc::now().timestamp();
    let diff = now - created_at;
    if diff < 0 {
        // 未来时间（时钟回拨）→ 显示"刚刚"避免负数
        "刚刚".into()
    } else if diff < 60 {
        "刚刚".into()
    } else if diff < 3600 {
        format!("{} 分钟前", diff / 60)
    } else if diff < 86400 {
        format!("{} 小时前", diff / 3600)
    } else {
        format!("{} 天前", diff / 86400)
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(SearchClipboardHistory) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_search_clipboard_history() {
        assert_eq!(SearchClipboardHistory.id(), "search_clipboard_history");
    }

    #[test]
    fn schema_query_is_optional() {
        // query 可选（空=最近 N 条），故无 required 数组
        let s = SearchClipboardHistory.schema();
        assert!(s.parameters.get("required").is_none(), "query 应可选");
        assert_eq!(s.parameters["properties"]["query"]["type"], "string");
    }

    #[test]
    fn schema_has_optional_max_results() {
        let s = SearchClipboardHistory.schema();
        assert_eq!(s.parameters["properties"]["max_results"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["max_results"]["default"], 30);
    }

    #[test]
    fn schema_sensitive_is_true() {
        let s = SearchClipboardHistory.schema();
        assert!(s.sensitive, "search_clipboard_history 必须 sensitive=true");
    }

    #[test]
    fn format_relative_time_recent_is_just_now() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now), "刚刚");
        assert_eq!(format_relative_time(now - 30), "刚刚");
    }

    #[test]
    fn format_relative_time_minutes() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 120), "2 分钟前");
        assert_eq!(format_relative_time(now - 1800), "30 分钟前");
    }

    #[test]
    fn format_relative_time_hours() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 7200), "2 小时前");
    }

    #[test]
    fn format_relative_time_days() {
        let now = chrono::Utc::now().timestamp();
        assert_eq!(format_relative_time(now - 172800), "2 天前");
    }

    #[test]
    fn format_relative_time_future_clamps_to_just_now() {
        // 时钟回拨 → 显示"刚刚"避免负数
        let future = chrono::Utc::now().timestamp() + 100;
        assert_eq!(format_relative_time(future), "刚刚");
    }
}
