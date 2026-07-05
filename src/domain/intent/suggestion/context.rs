//! ContextProducer：环境感知 Suggestion 生产者（0.8.6 §8.1.2）。
//!
//! 从 Context 规则表（选区/剪贴板/前台应用）产出 Suggestion。
//! 委托 `RuleRouter::context_suggestion` 完成命中判定 + 文本构建。

use std::sync::Arc;

use crate::domain::intent::RuleRouter;
use crate::infra::platform::context::AwarenessSnapshot;

use super::{Suggestion, SuggestionSource};
use super::producer::SuggestionProducer;

/// Context Suggestion 生产者。
///
/// 持有 `Arc<RuleRouter>` 以访问 context 规则表和 `PluginSettingResolver`。
/// `produce` 内部委托 `RuleRouter::context_suggestion`——
/// 空/非空 query 都可能产出 Context Ghost（0.8.4 §5.3.3）。
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

    fn produce(&self, _query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        // context_suggestion 内部做 match_context_hits + confidence 竞争 + 文本构建
        match self.router.context_suggestion(snapshot) {
            Some(sug) => vec![sug],
            None => Vec::new(),
        }
    }
}
