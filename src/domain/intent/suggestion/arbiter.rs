//! SuggestionArbiter：多源 Suggestion 竞争仲裁（0.8.6 §8.1.2）。
//!
//! 收集所有 `SuggestionProducer` 的产出，按 confidence 竞争选出 top-1。
//! 空/非空 query 的策略差异在此层统一处理（不再散落在 `RuleRouter::best_suggestion` 里）。
//!
//! **RankingHint 独立通道**：arbiter 返回 `(Option<Suggestion>, Option<RankingHint>)`——
//! `ranking_hint` 从 `Suggestion` 结构剥离，由 arbiter 汇总后独立回传 SearchService。
//! 0.8.6 阶段 `Suggestion.ranking_hint` 字段标 `#[deprecated]` 但保留，arbiter 从
//! producer 产出的 Suggestion 中提取。

use std::sync::Arc;

use crate::domain::intent::RankingHint;
use crate::infra::platform::context::AwarenessSnapshot;

#[allow(unused_imports)] // SuggestionSource 仅 #[cfg(test)] 消费
use super::{Suggestion, SuggestionSource};
use super::producer::SuggestionProducer;

/// 多源 Suggestion 竞争仲裁器。
pub struct SuggestionArbiter {
    producers: Vec<Arc<dyn SuggestionProducer>>,
}

impl SuggestionArbiter {
    pub fn new() -> Self {
        Self { producers: Vec::new() }
    }

    /// 注册一个 producer。
    pub fn register(&mut self, producer: Arc<dyn SuggestionProducer>) {
        self.producers.push(producer);
    }

    /// 竞争选出 top-1 Suggestion + 独立 RankingHint。
    ///
    /// **策略**（0.8.6 §8.1.2）：
    /// - 收集所有 producer 的候选
    /// - 按 confidence 降序取最高
    /// - `RankingHint` 从 top-1 Suggestion 中提取（如有）
    ///
    /// **空/非空 query 互斥**（0.8.3 ~ 0.8.5 行为保留）：
    /// - Keyword producer 在空 query 时自然返回空（`compute_hint_scored` 对空 query 返回 None）
    /// - Context producer 在非空 query 时也能产出（0.8.4 §5.3.3 fallback）
    /// - 两路候选在 arbiter 层统一竞争，不再由调用方分支
    ///
    /// **返回**：`(Option<Suggestion>, Option<RankingHint>)`
    /// - Suggestion：前端渲染 Ghost text + Tab 采纳
    /// - RankingHint：独立通道回 SearchService（下一轮 route 的 Surface Booster）
    #[allow(deprecated)] // 读取 Suggestion.ranking_hint 做过渡期剥离，0.9 彻底移除字段后此方法简化
    pub fn best(&self, query: &str, snapshot: &AwarenessSnapshot) -> (Option<Suggestion>, Option<RankingHint>) {
        let mut all: Vec<Suggestion> = Vec::new();
        for producer in &self.producers {
            all.extend(producer.produce(query, snapshot));
        }

        if all.is_empty() {
            return (None, None);
        }

        // 按 confidence 降序取 top-1
        let best = all
            .into_iter()
            .max_by(|a, b| {
                a.confidence
                    .partial_cmp(&b.confidence)
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
            .unwrap(); // safe: all 非空

        // 提取 RankingHint（从 Suggestion 中剥离）
        let hint = best.ranking_hint.clone();
        (Some(best), hint)
    }

    /// producer 数量（调试用）。
    #[allow(dead_code)]
    pub fn producer_count(&self) -> usize {
        self.producers.len()
    }
}

impl Default for SuggestionArbiter {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::platform::context::AwarenessSnapshot;
    use super::super::SuggestionOrigin;

    /// mock producer：固定返回指定 suggestions
    struct MockProducer {
        source: SuggestionSource,
        suggestions: Vec<Suggestion>,
    }

    impl MockProducer {
        fn new(source: SuggestionSource, suggestions: Vec<Suggestion>) -> Self {
            Self { source, suggestions }
        }

        fn empty(source: SuggestionSource) -> Self {
            Self { source, suggestions: Vec::new() }
        }
    }

    impl SuggestionProducer for MockProducer {
        fn source(&self) -> SuggestionSource {
            self.source
        }
        fn produce(&self, _query: &str, _snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
            self.suggestions.clone()
        }
    }

    fn make_sug(source: SuggestionSource, confidence: f64) -> Suggestion {
        Suggestion {
            display: format!("{source:?}"),
            replacement: format!("{source:?} "),
            source,
            confidence,
            prefix_len: 0,
            origin: None,
            ranking_hint: None,
        }
    }

    fn make_sug_with_hint(source: SuggestionSource, confidence: f64, plugin_id: &str) -> Suggestion {
        Suggestion {
            display: format!("{source:?}"),
            replacement: format!("{source:?} "),
            source,
            confidence,
            prefix_len: 0,
            origin: None,
            ranking_hint: Some(RankingHint { boost_plugin_id: plugin_id.to_string() }),
        }
    }

    #[test]
    fn empty_producers_returns_none() {
        let arbiter = SuggestionArbiter::new();
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = arbiter.best("query", &snap);
        assert!(sug.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn single_producer_returns_its_suggestion() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionSource::Keyword, 0.8)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = arbiter.best("fy", &snap);
        assert!(sug.is_some());
        assert_eq!(sug.unwrap().source, SuggestionSource::Keyword);
        assert!(hint.is_none());
    }

    #[test]
    fn highest_confidence_wins() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionSource::Keyword, 0.6)],
        )));
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(SuggestionSource::Context, 0.9)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, _) = arbiter.best("", &snap);
        let s = sug.unwrap();
        assert_eq!(s.source, SuggestionSource::Context);
        assert!((s.confidence - 0.9).abs() < 1e-9);
    }

    #[test]
    fn empty_producer_skipped() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(SuggestionSource::Context, 0.7)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, _) = arbiter.best("", &snap);
        assert_eq!(sug.unwrap().source, SuggestionSource::Context);
    }

    #[test]
    fn all_empty_returns_none() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        arbiter.register(Arc::new(MockProducer::empty(SuggestionSource::Context)));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = arbiter.best("query", &snap);
        assert!(sug.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn ranking_hint_extracted_from_winner() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionSource::Keyword, 0.6)], // 无 hint
        )));
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug_with_hint(SuggestionSource::Context, 0.9, "builtin.translate")],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = arbiter.best("", &snap);
        assert!(sug.is_some());
        let h = hint.unwrap();
        assert_eq!(h.boost_plugin_id, "builtin.translate");
    }

    #[test]
    fn ranking_hint_none_when_winner_has_no_hint() {
        let mut arbiter = SuggestionArbiter::new();
        arbiter.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionSource::Keyword, 0.9)], // 无 hint
        )));
        let snap = AwarenessSnapshot::default();
        let (_, hint) = arbiter.best("fy", &snap);
        assert!(hint.is_none());
    }
}
