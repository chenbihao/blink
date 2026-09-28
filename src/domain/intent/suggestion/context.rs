//! ContextProducer：环境感知 Suggestion 生产者（0.8.6 §8.1.2；0.24.2 扩为 Translate 域唯一生产者）。
//!
//! 产出 `Kind::Translate` 候选（§3.4 rank 表）：
//! - `translate-query` 0.92：query 为非目标语言自然文本（0.24 §3.3 Query 评分输入）
//! - `translate-selection` 0.92：选区非目标语言（原 context ghost，0.24.2 起放开空 query 闸）
//! - `translate-clipboard` 0.82：剪贴板非目标语言
//!
//! 命中判定委托 `RuleRouter`（context 规则表 + `PluginSettingResolver`）——
//! 翻译插件启用态/binding 黑名单/target_lang 解析都在 router 侧，与
//! 0.8.2 起的 manifest context 路径同源。**非空 query 不再短路**：awareness 派生
//! 候选在非空 query 下参选，最多占 secondary（分层在 Coordinator，§3.4）。

use std::sync::Arc;

use crate::domain::intent::RuleRouter;
use crate::infra::platform::context::AwarenessSnapshot;

use super::producer::SuggestionProducer;
use super::{Suggestion, SuggestionSource};

/// Translate 域 Suggestion 生产者。
pub struct ContextProducer {
    router: Arc<RuleRouter>,
}

impl ContextProducer {
    pub fn new(router: Arc<RuleRouter>) -> Self {
        Self { router }
    }
}

impl SuggestionProducer for ContextProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Context
    }

    /// 产出 Translate 类 Suggestion（query 派生 + awareness 派生）。
    ///
    /// 采纳后自抑制护栏（0.8.8 bugfix）内聚在 router 侧两个构造方法：
    /// query 已命中目标 plugin keyword（如 `翻译 xxx`）→ 不产同义建议，
    /// 避免 Ghost 反复弹出 / 无限 Tab 叠加（§3.3"明确 keyword 走确定路由不叠加"）。
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        let mut out = Vec::new();
        if let Some(sug) = self.router.translate_query_suggestion(query) {
            out.push(sug);
        }
        if let Some(sug) = self.router.context_suggestion(query, snapshot) {
            out.push(sug);
        }
        out
    }
}
