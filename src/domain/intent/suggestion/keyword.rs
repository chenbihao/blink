//! KeywordProducer：输入补全 Suggestion 生产者（0.8.6 §8.1.2）。
//!
//! 从 keyword 规则表收集 `(原文, pinyin_full)` 二元组，
//! 用 `suggest::compute_hint_scored` 做 fuzzy 匹配产出 Suggestion。

use std::sync::{Arc, RwLock};

use crate::domain::intent::RuleRouter;
use crate::infra::platform::context::AwarenessSnapshot;
use crate::domain::intent::suggest;

use super::{Suggestion, SuggestionSource};
use super::producer::SuggestionProducer;

/// Keyword Suggestion 生产者。
///
/// 持有 `Arc<RuleRouter>` 以动态调用 `collect_suggest_keywords()`——
/// 每次 `produce` 都从当前 keyword 规则表收集，自动覆盖插件热更新。
///
/// `min_score` 通过 `Arc<RwLock<f64>>` 与 `SearchService` 共享——
/// 设置页热更新 autosuggest_min_score 时两侧同步生效，无需额外通知。
pub struct KeywordProducer {
    router: Arc<RuleRouter>,
    min_score: Arc<RwLock<f64>>,
}

impl KeywordProducer {
    /// 构造 KeywordProducer。
    ///
    /// `min_score` 是共享引用——外部（SearchService）可随时写入新值，
    /// `produce` 每次读取最新阈值。
    pub fn from_router(router: Arc<RuleRouter>, min_score: Arc<RwLock<f64>>) -> Self {
        Self { router, min_score }
    }
}

impl SuggestionProducer for KeywordProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Keyword
    }

    #[allow(deprecated)] // 构造 Suggestion 时填充 ranking_hint: None，0.9 彻底移除字段后简化
    fn produce(&self, query: &str, _snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        let min_score = *self.min_score.read().unwrap();
        let keywords = self.router.collect_suggest_keywords();
        let Some((hint, score)) = suggest::compute_hint_scored(&keywords, query, min_score) else {
            return Vec::new();
        };
        vec![Suggestion {
            display: hint.display,
            replacement: hint.replacement,
            source: SuggestionSource::Keyword,
            confidence: score.min(1.0),
            prefix_len: hint.prefix_len,
            origin: None,
            ranking_hint: None,
        }]
    }
}
