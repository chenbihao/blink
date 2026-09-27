//! SuggestionCoordinator：多源 Suggestion 协调仲裁（0.8.6 §8.1.2 建立，0.24.1 更名演进）。
//!
//! 收集所有 `SuggestionProducer` 的产出，按 rank_score 竞争选出 top-1。
//! 空/非空 query 的策略差异在此层统一处理（不再散落在 `RuleRouter::best_suggestion` 里）。
//!
//! **0.24.1 过渡签名**：`coordinate(query, snapshot)` 仍是 top-1 语义——SearchService
//! 组装 `SuggestionSet { primary, revision: seq }`，secondary 恒 None。
//! 0.24.2 扩展为 `CoordinateInput`（route_summary / availability / config），
//! 承接 eligibility、分层、按 Kind 去重与双槽选取（0.24 §3.4 / §3.7）。
//!
//! **RankingHint 独立通道**：coordinator 返回 `(Option<Suggestion>, Option<RankingHint>)`——
//! hint 从 producer 产出的 Suggestion 中提取（0.24.1 仍走 deprecated 字段过渡，
//! 0.24.2 随 producer 协议重构改由 produce 返回值独立携带）。

use std::sync::Arc;

use crate::domain::intent::RankingHint;
use crate::infra::platform::context::AwarenessSnapshot;

use super::producer::SuggestionProducer;
#[allow(unused_imports)] // SuggestionSource 仅 #[cfg(test)] 消费
use super::{Suggestion, SuggestionSource};

/// 多源 Suggestion 协调器（原 `SuggestionArbiter`，0.24.1 更名）。
pub struct SuggestionCoordinator {
    producers: Vec<Arc<dyn SuggestionProducer>>,
}

impl SuggestionCoordinator {
    pub fn new() -> Self {
        Self {
            producers: Vec::new(),
        }
    }

    /// 注册一个 producer。
    pub fn register(&mut self, producer: Arc<dyn SuggestionProducer>) {
        self.producers.push(producer);
    }

    /// 竞争选出 top-1 Suggestion + 独立 RankingHint（0.24.1 过渡签名）。
    ///
    /// **策略**（0.8.6 §8.1.2）：
    /// - 收集所有 producer 的候选
    /// - 按 rank_score 降序取最高
    /// - `RankingHint` 从 top-1 Suggestion 中提取（如有）
    ///
    /// **空/非空 query 互斥**（0.8.3 ~ 0.8.5 行为保留）：
    /// - Keyword producer 在空 query 时自然返回空（`compute_hint_scored` 对空 query 返回 None）
    /// - Context producer 在非空 query 时也能产出（0.8.4 §5.3.3 fallback）
    /// - 两路候选在 coordinator 层统一竞争，不再由调用方分支
    ///
    /// **返回**：`(Option<Suggestion>, Option<RankingHint>)`
    /// - Suggestion：前端渲染 Ghost text + Tab 采纳
    /// - RankingHint：独立通道回 SearchService（下一轮 route 的 Surface Booster）
    #[allow(deprecated)] // 读取 Suggestion.ranking_hint 做过渡期剥离，0.24.2 producer 协议重构后移除
    pub fn coordinate(
        &self,
        query: &str,
        snapshot: &AwarenessSnapshot,
    ) -> (Option<Suggestion>, Option<RankingHint>) {
        let mut all: Vec<Suggestion> = Vec::new();
        for producer in &self.producers {
            all.extend(producer.produce(query, snapshot));
        }

        if all.is_empty() {
            return (None, None);
        }

        // 按 rank_score 降序取 top-1（0.24.1 起排序字段从 confidence 改名 rank_score，
        // 数值沿用旧 confidence——排序行为不变，§3.4 归一在 0.24.2）
        let best = all
            .into_iter()
            .max_by(|a, b| {
                a.rank_score
                    .partial_cmp(&b.rank_score)
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

impl Default for SuggestionCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::platform::context::AwarenessSnapshot;

    use super::super::{SuggestionAction, SuggestionKind};

    /// mock producer：固定返回指定 suggestions
    struct MockProducer {
        source: SuggestionSource,
        suggestions: Vec<Suggestion>,
    }

    impl MockProducer {
        fn new(source: SuggestionSource, suggestions: Vec<Suggestion>) -> Self {
            Self {
                source,
                suggestions,
            }
        }

        fn empty(source: SuggestionSource) -> Self {
            Self {
                source,
                suggestions: Vec::new(),
            }
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

    #[allow(deprecated)]
    fn make_sug(kind: SuggestionKind, rank_score: f64) -> Suggestion {
        Suggestion {
            id: format!("test-{rank_score}"),
            kind,
            action: SuggestionAction::RouteQuery {
                query: format!("{rank_score} "),
            },
            rank_score,
            display: format!("{rank_score}"),
            prefix_len: 0,
            origin: None,
            ranking_hint: None,
        }
    }

    #[allow(deprecated)]
    fn make_sug_with_hint(kind: SuggestionKind, rank_score: f64, plugin_id: &str) -> Suggestion {
        Suggestion {
            ranking_hint: Some(RankingHint {
                boost_plugin_id: plugin_id.to_string(),
            }),
            ..make_sug(kind, rank_score)
        }
    }

    #[test]
    fn empty_producers_returns_none() {
        let coordinator = SuggestionCoordinator::new();
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = coordinator.coordinate("query", &snap);
        assert!(sug.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn single_producer_returns_its_suggestion() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionKind::Completion, 0.8)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = coordinator.coordinate("fy", &snap);
        assert!(sug.is_some());
        assert_eq!(sug.unwrap().id, "test-0.8");
        assert!(hint.is_none());
    }

    #[test]
    fn highest_rank_score_wins() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionKind::Completion, 0.6)],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(SuggestionKind::Translate, 0.9)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, _) = coordinator.coordinate("", &snap);
        let s = sug.unwrap();
        assert_eq!(s.kind, SuggestionKind::Translate);
        assert!((s.rank_score - 0.9).abs() < 1e-9);
    }

    #[test]
    fn empty_producer_skipped() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(SuggestionKind::Translate, 0.7)],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, _) = coordinator.coordinate("", &snap);
        assert!((sug.unwrap().rank_score - 0.7).abs() < 1e-9);
    }

    #[test]
    fn all_empty_returns_none() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Context)));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = coordinator.coordinate("query", &snap);
        assert!(sug.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn ranking_hint_extracted_from_winner() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionKind::Completion, 0.6)], // 无 hint
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug_with_hint(
                SuggestionKind::Translate,
                0.9,
                "builtin.translate",
            )],
        )));
        let snap = AwarenessSnapshot::default();
        let (sug, hint) = coordinator.coordinate("", &snap);
        assert!(sug.is_some());
        let h = hint.unwrap();
        assert_eq!(h.boost_plugin_id, "builtin.translate");
    }

    #[test]
    fn ranking_hint_none_when_winner_has_no_hint() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(SuggestionKind::Completion, 0.9)], // 无 hint
        )));
        let snap = AwarenessSnapshot::default();
        let (_, hint) = coordinator.coordinate("fy", &snap);
        assert!(hint.is_none());
    }
}
