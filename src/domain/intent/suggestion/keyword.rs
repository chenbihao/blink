//! KeywordProducer：输入补全 Suggestion 生产者（0.8.6 §8.1.2）。
//!
//! 从 keyword 规则表收集 `(原文, pinyin_full)` 二元组，
//! 用 `suggest::compute_hint_scored` 做 fuzzy 匹配产出 Suggestion。

use std::sync::{Arc, RwLock};

use crate::domain::intent::RuleRouter;
use crate::domain::intent::suggest;
use crate::infra::platform::context::AwarenessSnapshot;

use super::coordinator::SuggestionRuntimeConfig;
use super::producer::SuggestionProducer;
use super::{Suggestion, SuggestionSource, text_fingerprint};

/// Keyword Suggestion 生产者。
///
/// 持有 `Arc<RuleRouter>` 以动态调用 `collect_suggest_keywords()`——
/// 每次 `produce` 都从当前 keyword 规则表收集，自动覆盖插件热更新。
///
/// `runtime` 通过共享 `Arc<RwLock<SuggestionRuntimeConfig>>` 与 `SearchService`
/// 同步——设置页热更新时两侧同步生效，无需额外通知。
pub struct KeywordProducer {
    router: Arc<RuleRouter>,
    runtime: Arc<RwLock<SuggestionRuntimeConfig>>,
}

impl KeywordProducer {
    /// 构造 KeywordProducer。
    ///
    /// `runtime` 是共享引用——外部（SearchService）可随时写入新配置，
    /// `produce` 每次读取最新阈值。
    pub fn from_router(
        router: Arc<RuleRouter>,
        runtime: Arc<RwLock<SuggestionRuntimeConfig>>,
    ) -> Self {
        Self { router, runtime }
    }
}

impl SuggestionProducer for KeywordProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Keyword
    }

    #[allow(deprecated)] // 构造 Suggestion 时填充 ranking_hint: None（§3.7 冻结 produce 签名，过渡通道保留）
    fn produce(&self, query: &str, _snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        use super::{SuggestionAction, SuggestionKind};

        let min_score = self.runtime.read().unwrap().min_score;
        let keywords = self.router.collect_suggest_keywords();
        let Some((hint, fuzzy)) = suggest::compute_hint_scored(&keywords, query, min_score) else {
            return Vec::new();
        };
        // §3.4 rank 表：补全 = 0.70 + 0.28 × fuzzy（0.70～0.98）。
        // compute_hint_scored 对 exact 命中给 1.0 → 0.98 封顶，fuzzy ∈ [min_score, 1.0]。
        let fuzzy = fuzzy.min(1.0);
        vec![Suggestion {
            id: "completion-keyword".to_string(),
            kind: SuggestionKind::Completion,
            action: SuggestionAction::RouteQuery {
                query: hint.replacement,
            },
            rank_score: 0.70 + 0.28 * fuzzy,
            display: hint.display,
            prefix_len: hint.prefix_len,
            origin: None,
            fingerprint: text_fingerprint(query),
            source: SuggestionSource::Keyword,
            ranking_hint: None,
        }]
    }
}
