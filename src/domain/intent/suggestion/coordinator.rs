//! SuggestionCoordinator：多源 Suggestion 协调仲裁（0.8.6 §8.1.2 建立，0.24.2 演进）。
//!
//! 收集所有 `SuggestionProducer` 的产出，按 0.24 §3.4 执行五步管线：
//!
//! ```text
//! eligibility（route/availability 过滤，记 filter reason）
//!   → 分层（非空 query 时 query 派生优先于 awareness 派生）
//!   → 按 Kind 去重（每种 SuggestionKind 只留最高分）
//!   → 排序（rank_score 降序，层内稳定）
//!   → 双槽选取（secondary 需 ≥ secondary_min_rank）
//! ```
//!
//! **编排顺序即防环**（§3.7）：Route 先算、Coordinator 后算，`RouteSummary` 是编排层
//! 传值不是域依赖；AI 兜底的"路由未命中"判定在这里做 eligibility，不进 Producer。
//!
//! **RankingHint 独立通道**：coordinator 返回 `(CoordinateOutput, Option<RankingHint>)`
//! ——hint 从 primary Suggestion 提取（0.24.2 仍走 deprecated 字段过渡，
//! §3.7 冻结 produce 签名不变，故该内部通道保留）。

use std::sync::Arc;

use crate::domain::intent::RankingHint;
use crate::infra::platform::context::AwarenessSnapshot;

use super::producer::SuggestionProducer;
use super::{
    RouteSummary, Suggestion, SuggestionAction, SuggestionAvailability, SuggestionKind,
    text_fingerprint,
};

/// Coordinator 运行时配置投影（0.24 §3.4 / §3.8；0.24.5 §5.6 加展示策略开关组）。
///
/// 从 `SuggestionConfig` 投影出的热路径快照（零 IO），由编排层持有并热更新；
/// KeywordProducer 共享同一 cell 读取 `min_score`。
#[derive(Debug, Clone, Copy)]
pub struct SuggestionRuntimeConfig {
    pub autosuggest_enabled: bool,
    /// Keyword fuzzy 归一阈值（`autosuggest_min_score`）。
    pub min_score: f64,
    /// Secondary 槽门槛——精确挡掉 0.55 兜底（第二槽也该是有把握的建议）。
    pub secondary_min_rank: f64,
    /// 建议降频开关（§3.8，默认开）：同键连续 3 次未采纳 → 本会话抑制。
    pub suppress_repeated: bool,
    /// 展示策略开关组（0.24.5 §5.6 设置投影，默认全开）：
    /// - `completion_enabled`：Completion 影子候选
    /// - `context_suggestion_enabled`：awareness 派生候选（选区/剪贴板）
    /// - `ai_suggestion_enabled`：AskAi 候选（Provider 可用性之上的建议域独立闸）
    /// - `secondary_enabled`：secondary 槽（关闭时只出 primary 单槽）
    pub completion_enabled: bool,
    pub context_suggestion_enabled: bool,
    pub ai_suggestion_enabled: bool,
    pub secondary_enabled: bool,
}

impl Default for SuggestionRuntimeConfig {
    fn default() -> Self {
        Self {
            autosuggest_enabled: true,
            min_score: 0.7,
            secondary_min_rank: 0.60,
            suppress_repeated: true,
            completion_enabled: true,
            context_suggestion_enabled: true,
            ai_suggestion_enabled: true,
            secondary_enabled: true,
        }
    }
}

/// eligibility 过滤原因（0.24 §5.4 可观测性）——只记类别不记原文。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterReason {
    /// 总开关关闭（autosuggest_enabled=false）。
    AutosuggestDisabled,
    /// query 派生的 Translate/AskAi 候选被确定路由/非空候选排除。
    RouteHit,
    /// AI Provider 未配置或未启用。
    AiUnavailable,
    /// 降频抑制：同 (kind, origin, 指纹) 连续 3 次未采纳（§3.8）。
    FatigueSuppressed,
    /// 展示策略开关组（0.24.5 §5.6）：对应候选类被用户在设置页关闭。
    CompletionDisabled,
    ContextSuggestionDisabled,
    AiSuggestionDisabled,
    /// secondary 槽被关闭（不进 eligibility 过滤，双槽选取阶段短路）。
    SecondaryDisabled,
}

impl std::fmt::Display for FilterReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FilterReason::AutosuggestDisabled => write!(f, "autosuggest_disabled"),
            FilterReason::RouteHit => write!(f, "route_hit"),
            FilterReason::AiUnavailable => write!(f, "ai_unavailable"),
            FilterReason::FatigueSuppressed => write!(f, "fatigue_suppressed"),
            FilterReason::CompletionDisabled => write!(f, "completion_disabled"),
            FilterReason::ContextSuggestionDisabled => write!(f, "context_suggestion_disabled"),
            FilterReason::AiSuggestionDisabled => write!(f, "ai_suggestion_disabled"),
            FilterReason::SecondaryDisabled => write!(f, "secondary_disabled"),
        }
    }
}

/// coordinate 输入（0.24 §3.7）。`query` 未 trim（保留尾空格语义给 KeywordProducer）。
pub struct CoordinateInput<'a> {
    pub query: &'a str,
    pub snapshot: &'a AwarenessSnapshot,
    pub route_summary: RouteSummary,
    pub availability: SuggestionAvailability,
    pub config: SuggestionRuntimeConfig,
}

/// coordinate 输出槽位（revision 由编排层回填组装 `SuggestionSet`）。
#[derive(Debug, Clone, Default)]
pub struct CoordinateOutput {
    pub primary: Option<Suggestion>,
    pub secondary: Option<Suggestion>,
}

/// 多源 Suggestion 协调器（原 `SuggestionArbiter`，0.24.1 更名；实例由编排层持有）。
pub struct SuggestionCoordinator {
    producers: Vec<Arc<dyn SuggestionProducer>>,
}

impl SuggestionCoordinator {
    pub fn new() -> Self {
        Self {
            producers: Vec::new(),
        }
    }

    /// 注册一个 producer。注册顺序即同分稳定排序的优先序（Keyword → Context → AI）。
    pub fn register(&mut self, producer: Arc<dyn SuggestionProducer>) {
        self.producers.push(producer);
    }

    /// 五步管线：eligibility → 分层 → Kind 去重 → 排序 → 双槽（0.24 §3.4；
    /// 0.24.3 §3.8 加降频过滤与曝光计数）。
    ///
    /// `fatigue` 是编排层持有的会话级降频计数器（§3.8：SearchService 持有、
    /// 隐藏/采纳清零）；`suppress_repeated=false` 时只计数不抑制，便于观测。
    ///
    /// 返回 `(CoordinateOutput, Option<RankingHint>)`——hint 取 primary 的
    /// （§3.7；primary 无 hint 时为 None，secondary 的 hint 不上反馈通道）。
    pub fn coordinate(
        &self,
        input: &CoordinateInput<'_>,
        fatigue: &mut super::fatigue::SuggestionFatigue,
    ) -> (CoordinateOutput, Option<RankingHint>) {
        // ── 0. 总开关：autosuggest_enabled=false → 全部候选不产（快速短路）──
        if !input.config.autosuggest_enabled {
            tracing::debug!(reason = %FilterReason::AutosuggestDisabled, "suggestion 过滤");
            return (CoordinateOutput::default(), None);
        }

        // ── 1. AiTrigger 特例：route 已显式触发 AI，Suggestion 恒为 ai-trigger ──
        //    "ai " 前缀是强信号，独占 primary（rank 1.0），其余候选不参选。
        if let RouteSummary::AiTrigger { arg } = &input.route_summary {
            if !input.availability.ai_available {
                tracing::debug!(
                    reason = %FilterReason::AiUnavailable,
                    "suggestion 过滤：ai-trigger 因 AI 不可用被排除"
                );
                return (CoordinateOutput::default(), None);
            }
            let sug = ai_trigger_suggestion(arg);
            let key = super::fatigue::fatigue_key(&sug);
            let n = fatigue.record_impression(key);
            tracing::debug!(id = %sug.id, slot = "primary", unadopted = n, "suggestion impression");
            return (
                CoordinateOutput {
                    primary: Some(sug),
                    secondary: None,
                },
                None,
            );
        }

        // ── 2. 收集候选（producer.source() 盖章身份，供可观测性日志）──
        let mut all: Vec<Suggestion> = Vec::new();
        for producer in &self.producers {
            let source = producer.source();
            all.extend(
                producer
                    .produce(input.query, input.snapshot)
                    .into_iter()
                    .map(|mut s| {
                        s.source = source;
                        s
                    }),
            );
        }
        if all.is_empty() {
            return (CoordinateOutput::default(), None);
        }

        // ── 3. eligibility（§3.7：路由未命中判定与能力可用性都在这里，不进 Producer）──
        let routed = matches!(input.route_summary, RouteSummary::Routed);
        let kept: Vec<Suggestion> = all
            .into_iter()
            .filter(|s| {
                // 3a. 展示策略：Completion 影子被设置页关闭（0.24.5 §5.6）
                if s.kind == SuggestionKind::Completion && !input.config.completion_enabled {
                    tracing::debug!(
                        id = %s.id,
                        producer = ?s.source,
                        reason = %FilterReason::CompletionDisabled,
                        "suggestion 过滤"
                    );
                    return false;
                }
                // 3b. 展示策略：awareness 派生候选（选区/剪贴板）被"环境建议"关闭。
                //     query 派生候选不受影响——它是输入意图不是环境。
                if s.origin.is_some() && !input.config.context_suggestion_enabled {
                    tracing::debug!(
                        id = %s.id,
                        producer = ?s.source,
                        reason = %FilterReason::ContextSuggestionDisabled,
                        "suggestion 过滤"
                    );
                    return false;
                }
                // 3c. AI 可用性 + 展示策略：AskAi 候选统一受 AI Provider 总闸
                //     与建议域独立闸（关建议不连坐 AI 功能本身）
                if s.kind == SuggestionKind::AskAi {
                    if !input.availability.ai_available {
                        tracing::debug!(
                            id = %s.id,
                            producer = ?s.source,
                            reason = %FilterReason::AiUnavailable,
                            "suggestion 过滤"
                        );
                        return false;
                    }
                    if !input.config.ai_suggestion_enabled {
                        tracing::debug!(
                            id = %s.id,
                            producer = ?s.source,
                            reason = %FilterReason::AiSuggestionDisabled,
                            "suggestion 过滤"
                        );
                        return false;
                    }
                }
                // 3d. 路由未命中：query 派生（origin=None）的 Translate/AskAi 候选
                //     在已有确定路由/非空候选时排除——"已明确命中的路由不得被翻译抢占"。
                //     Completion 豁免（输入延伸，非意图建议，且 exact/带参命中天然不产 hint）。
                if routed
                    && s.origin.is_none()
                    && matches!(s.kind, SuggestionKind::Translate | SuggestionKind::AskAi)
                {
                    tracing::debug!(
                        id = %s.id,
                        producer = ?s.source,
                        reason = %FilterReason::RouteHit,
                        "suggestion 过滤"
                    );
                    return false;
                }
                // 3e. 降频抑制（§3.8）：同 (kind, origin, 指纹) 连续 3 次未采纳。
                if input.config.suppress_repeated
                    && fatigue.is_suppressed(super::fatigue::fatigue_key(s))
                {
                    tracing::debug!(
                        id = %s.id,
                        producer = ?s.source,
                        reason = %FilterReason::FatigueSuppressed,
                        "suggestion 过滤"
                    );
                    return false;
                }
                true
            })
            .collect();
        if kept.is_empty() {
            return (CoordinateOutput::default(), None);
        }

        // ── 4. 分层（§3.4：分层先于分数）──
        //    非空 query 时 query 派生（Completion / 翻译 Query / AI-Query）优先于
        //    awareness 派生（选区/剪贴板）。显式化为两层有序池：
        //    query 池在前、awareness 池在后，层内按 rank_score 降序（稳定排序）。
        //    这同时实现"输入即意图表达"（0.8.4）：非空 query 下 awareness 候选
        //    最多占 secondary，不抢 primary。
        let mut query_pool: Vec<Suggestion> = Vec::new();
        let mut awareness_pool: Vec<Suggestion> = Vec::new();
        for s in kept {
            if s.origin.is_none() {
                query_pool.push(s);
            } else {
                awareness_pool.push(s);
            }
        }
        sort_by_rank(&mut query_pool);
        sort_by_rank(&mut awareness_pool);

        // ── 5. 按 Kind 去重 → 双槽（0.24.9 修订：非空 query 下 awareness 不抢 primary）──
        //    层序串接按 Kind 去重（每 Kind 留首个=层序内最高分）不变；变化在 primary
        //    的担任资格：非空 query 时 primary 必须来自 query 池——§4 注释"输入即
        //    意图表达"的完整落地。query 池空则 primary 轮空，最佳 awareness 候选
        //    降级 secondary。修复实测反馈：剪贴板建议在用户开始输入后仍盘踞
        //    primary，直到降频 3 连击才消失——"打几个字才收起"的假延迟实为
        //    降频计数在充当隐藏机制。空 query 无 query 派生候选，awareness 照旧
        //    担任 primary（0.8.3 空形态不变）。
        let query_empty = input.query.trim().is_empty();
        let primary = if query_empty {
            query_pool
                .first()
                .or_else(|| awareness_pool.first())
                .cloned()
        } else {
            query_pool.first().cloned()
        };
        // secondary：层序中与 primary Kind 互异的次位（primary 轮空时即层序首位）
        let secondary = query_pool
            .iter()
            .chain(awareness_pool.iter())
            .find(|s| primary.as_ref().map_or(true, |p| p.kind != s.kind))
            .cloned();

        let mut output = CoordinateOutput::default();
        output.primary = primary;
        // secondary 双闸（0.24.5 §5.6）：展示开关 + rank 门槛
        if let Some(second) = secondary {
            if !input.config.secondary_enabled {
                tracing::debug!(
                    id = %second.id,
                    reason = %FilterReason::SecondaryDisabled,
                    "secondary 槽被设置页关闭"
                );
            } else if second.rank_score >= input.config.secondary_min_rank {
                output.secondary = Some(second);
            }
        }

        // ── 6. 曝光计数（§3.8 降频输入；§5.4 可观测性——曝光→采纳转换率的分子侧）
        //    只对进入可见槽位的候选计数（被过滤/去重的不算曝光）。
        for (slot, sug) in [
            ("primary", &output.primary),
            ("secondary", &output.secondary),
        ] {
            if let Some(sug) = sug {
                let n = fatigue.record_impression(super::fatigue::fatigue_key(sug));
                tracing::debug!(
                    id = %sug.id,
                    kind = ?sug.kind,
                    producer = ?sug.source,
                    slot,
                    unadopted = n,
                    "suggestion impression"
                );
            }
        }

        // RankingHint 从 primary 提取（deprecated 过渡通道，§3.7 冻结 produce 签名）
        #[allow(deprecated)]
        let hint = output.primary.as_ref().and_then(|s| s.ranking_hint.clone());

        tracing::debug!(
            primary = ?output.primary.as_ref().map(|s| (&s.id, s.kind, s.rank_score)),
            secondary = ?output.secondary.as_ref().map(|s| (&s.id, s.kind, s.rank_score)),
            "suggestion coordinate 完成"
        );

        (output, hint)
    }
}

impl Default for SuggestionCoordinator {
    fn default() -> Self {
        Self::new()
    }
}

/// 层内稳定排序：rank_score 降序（同分保持 producer 注册序——Keyword → Context → AI）。
fn sort_by_rank(pool: &mut [Suggestion]) {
    pool.sort_by(|a, b| {
        b.rank_score
            .partial_cmp(&a.rank_score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
}

/// ai-trigger 候选构造（0.24 §3.7：arg 来自 route_summary，编排层传值）。
///
/// "ai " 前缀显式触发跳过 gating 四筛子（前缀本身是强信号），rank 1.0 独占 primary；
/// AI 可用性已在调用点由 eligibility 把关。
#[allow(deprecated)]
fn ai_trigger_suggestion(arg: &str) -> Suggestion {
    Suggestion {
        id: "ai-trigger".to_string(),
        kind: SuggestionKind::AskAi,
        action: SuggestionAction::EnterAiMode {
            prompt: arg.to_string(),
        },
        rank_score: 1.0,
        display: "按 Tab 问 AI".to_string(),
        prefix_len: 0,
        origin: None,
        fingerprint: text_fingerprint(arg),
        source: super::SuggestionSource::Ai,
        ranking_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::super::{SuggestionOrigin, SuggestionSource};
    use super::*;
    use crate::infra::platform::context::AwarenessSnapshot;

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

    /// 默认可用性 + 开放路由的输入。
    fn input<'a>(query: &'a str, snapshot: &'a AwarenessSnapshot) -> CoordinateInput<'a> {
        CoordinateInput {
            query,
            snapshot,
            route_summary: RouteSummary::Open,
            availability: SuggestionAvailability { ai_available: true },
            config: SuggestionRuntimeConfig::default(),
        }
    }

    #[allow(deprecated)]
    fn make_sug(
        id: &str,
        kind: SuggestionKind,
        rank_score: f64,
        origin: Option<SuggestionOrigin>,
    ) -> Suggestion {
        Suggestion {
            id: id.to_string(),
            kind,
            action: SuggestionAction::RouteQuery {
                query: format!("{id} "),
            },
            rank_score,
            display: id.to_string(),
            prefix_len: 0,
            origin,
            // 按 id 派生指纹：不同 id 的候选天然不同键（降频互不连坐的测试前提）。
            // source 由 coordinator 收集时按 producer.source() 重新盖章，此处占位即可。
            fingerprint: text_fingerprint(id),
            source: SuggestionSource::Ai,
            ranking_hint: None,
        }
    }

    #[allow(deprecated)]
    fn make_sug_with_hint(kind: SuggestionKind, rank_score: f64, plugin_id: &str) -> Suggestion {
        Suggestion {
            ranking_hint: Some(RankingHint {
                boost_plugin_id: plugin_id.to_string(),
            }),
            ..make_sug("hinted", kind, rank_score, None)
        }
    }

    fn snap() -> AwarenessSnapshot {
        AwarenessSnapshot::default()
    }

    fn fresh_fatigue() -> super::super::fatigue::SuggestionFatigue {
        super::super::fatigue::SuggestionFatigue::new()
    }

    // ── 基础行为（0.8.6 存量回归）──────────────────────────────────────────

    #[test]
    fn empty_producers_returns_none() {
        let coordinator = SuggestionCoordinator::new();
        let s = snap();
        let (out, hint) = coordinator.coordinate(&input("query", &s), &mut fresh_fatigue());
        assert!(out.primary.is_none());
        assert!(out.secondary.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn single_producer_returns_its_suggestion() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion",
                SuggestionKind::Completion,
                0.9,
                None,
            )],
        )));
        let s = snap();
        let (out, hint) = coordinator.coordinate(&input("fy", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "completion");
        assert!(out.secondary.is_none());
        assert!(hint.is_none());
    }

    /// coordinator 收集候选时按 `producer.source()` 盖章身份（§6.1 可观测性：
    /// filter/impression/adoption 日志的 producer 字段数据源）。
    #[test]
    fn producer_source_stamped_on_candidates() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug("t-sel", SuggestionKind::Translate, 0.92, None)],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().source, SuggestionSource::Context);
    }

    #[test]
    fn highest_rank_score_wins_within_layer() {
        // 同层（均 awareness 派生）竞争：分数高者胜
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![
                make_sug(
                    "clip",
                    SuggestionKind::Translate,
                    0.82,
                    Some(SuggestionOrigin::Clipboard),
                ),
                make_sug(
                    "sel",
                    SuggestionKind::Translate,
                    0.92,
                    Some(SuggestionOrigin::Selection),
                ),
            ],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "sel");
        // 同 Kind 去重：clipboard 候选不占 secondary
        assert!(out.secondary.is_none());
    }

    #[test]
    fn empty_producer_skipped() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "t-sel",
                SuggestionKind::Translate,
                0.92,
                Some(SuggestionOrigin::Selection),
            )],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        assert!((out.primary.unwrap().rank_score - 0.92).abs() < 1e-9);
    }

    #[test]
    fn all_empty_returns_none() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Keyword)));
        coordinator.register(Arc::new(MockProducer::empty(SuggestionSource::Context)));
        let s = snap();
        let (out, hint) = coordinator.coordinate(&input("query", &s), &mut fresh_fatigue());
        assert!(out.primary.is_none());
        assert!(hint.is_none());
    }

    #[test]
    fn ranking_hint_extracted_from_primary() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion",
                SuggestionKind::Completion,
                0.6,
                None,
            )], // 无 hint
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            // Completion 在 query 池先占 primary？——不：Completion 0.6 是 query 派生，
            // Translate 带 hint 是 awareness 派生；分层让 query 池先选。
            // 此测试要验证 hint 从 primary 提取，故让 hint 挂在 query 池胜者上。
            vec![make_sug_with_hint(
                SuggestionKind::Translate,
                0.92,
                "builtin.translate",
            )],
        )));
        let s = snap();
        // 空 query → 无 query 池候选 → hinted Translate 为 primary，hint 提取
        let (out, hint) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "hinted");
        assert_eq!(hint.unwrap().boost_plugin_id, "builtin.translate");
    }

    #[test]
    fn ranking_hint_none_when_primary_has_no_hint() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion",
                SuggestionKind::Completion,
                0.98,
                None,
            )],
        )));
        let s = snap();
        let (out, hint) = coordinator.coordinate(&input("fy", &s), &mut fresh_fatigue());
        assert!(out.primary.is_some());
        assert!(hint.is_none());
    }

    // ── 0.24 §3.4 表驱动：分层 / 去重 / 双槽 / 门槛 ─────────────────────────

    /// §3.4 分层回归：低分补全 + 英文剪贴板 → 补全 Primary（awareness 不抢 primary）。
    #[test]
    fn layering_low_completion_beats_clipboard_translate() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion-keyword",
                SuggestionKind::Completion,
                0.75,
                None,
            )],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "translate-clipboard",
                SuggestionKind::Translate,
                0.82,
                Some(SuggestionOrigin::Clipboard),
            )],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("fa", &s), &mut fresh_fatigue());
        let p = out.primary.unwrap();
        assert_eq!(p.id, "completion-keyword");
        let sec = out.secondary.unwrap();
        assert_eq!(sec.id, "translate-clipboard");
    }

    /// 双槽 + Kind 去重：英文 query → 翻译 Primary、AI Secondary（§6.1 验收）。
    #[test]
    fn dual_slot_translate_primary_ai_secondary() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![
                make_sug("translate-query", SuggestionKind::Translate, 0.92, None),
                make_sug("ai-foreign", SuggestionKind::AskAi, 0.62, None),
            ],
        )));
        let s = snap();
        let (out, _) =
            coordinator.coordinate(&input("hello world foo bar", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "translate-query");
        assert_eq!(out.secondary.unwrap().id, "ai-foreign");
    }

    /// secondary 门槛 0.60：0.55 兜底永不占第二槽（但可独占 primary）。
    #[test]
    fn secondary_min_rank_blocks_fallback() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-query", SuggestionKind::AskAi, 0.80, None)],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "ai-fallback",
                SuggestionKind::Translate,
                0.55,
                None,
            )],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("你好世界测试", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "ai-query");
        assert!(
            out.secondary.is_none(),
            "0.55 < secondary_min_rank 0.60，不得占第二槽"
        );

        // 无其他候选时 0.55 可独占 primary
        let mut coordinator2 = SuggestionCoordinator::new();
        coordinator2.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-fallback", SuggestionKind::AskAi, 0.55, None)],
        )));
        let (out2, _) = coordinator2.coordinate(&input("hello 世界", &s), &mut fresh_fatigue());
        assert_eq!(out2.primary.unwrap().id, "ai-fallback");
        assert!(out2.secondary.is_none());
    }

    /// eligibility RouteHit：确定路由下 query 派生 Translate/AskAi 排除，Completion 豁免。
    #[test]
    fn routed_suppresses_query_derived_but_not_completion() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion-keyword",
                SuggestionKind::Completion,
                0.9,
                None,
            )],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "translate-query",
                SuggestionKind::Translate,
                0.92,
                None,
            )],
        )));
        let s = snap();
        let mut i = input("fy", &s);
        i.route_summary = RouteSummary::Routed;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "completion-keyword");
        assert!(out.secondary.is_none(), "translate-query 被 RouteHit 过滤");
    }

    /// eligibility AiUnavailable：AI 关闭时 AskAi 全灭（含 awareness 派生的 ai-selection）。
    #[test]
    fn ai_unavailable_filters_all_askai() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![
                make_sug("ai-query", SuggestionKind::AskAi, 0.80, None),
                make_sug(
                    "ai-selection",
                    SuggestionKind::AskAi,
                    0.72,
                    Some(SuggestionOrigin::Selection),
                ),
            ],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "translate-selection",
                SuggestionKind::Translate,
                0.92,
                Some(SuggestionOrigin::Selection),
            )],
        )));
        let s = snap();
        let mut i = input("", &s);
        i.availability.ai_available = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "translate-selection");
        assert!(out.secondary.is_none());
    }

    /// AiTrigger 路由：恒为 ai-trigger 独占 primary，其余候选不参选。
    #[test]
    fn ai_trigger_route_yields_dedicated_suggestion() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion-keyword",
                SuggestionKind::Completion,
                0.98,
                None,
            )],
        )));
        let s = snap();
        let mut i = input("ai hello", &s);
        i.route_summary = RouteSummary::AiTrigger {
            arg: "hello".to_string(),
        };
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        let p = out.primary.unwrap();
        assert_eq!(p.id, "ai-trigger");
        assert_eq!(p.kind, SuggestionKind::AskAi);
        assert!(matches!(p.action, SuggestionAction::EnterAiMode { prompt } if prompt == "hello"));
        assert!(out.secondary.is_none());
    }

    /// AiTrigger + AI 不可用 → 无建议（尊重总开关，沿用 0.9.x ai-trigger 行为）。
    #[test]
    fn ai_trigger_respects_availability() {
        let coordinator = SuggestionCoordinator::new();
        let s = snap();
        let mut i = input("ai hello", &s);
        i.route_summary = RouteSummary::AiTrigger {
            arg: "hello".to_string(),
        };
        i.availability.ai_available = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert!(out.primary.is_none());
    }

    /// 总开关关闭 → 全灭。
    #[test]
    fn autosuggest_disabled_short_circuits() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug(
                "completion-keyword",
                SuggestionKind::Completion,
                0.98,
                None,
            )],
        )));
        let s = snap();
        let mut i = input("fy", &s);
        i.config.autosuggest_enabled = false;
        let (out, hint) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert!(out.primary.is_none());
        assert!(hint.is_none());
    }

    // ── 0.24.3 §3.8 降频集成 ─────────────────────────────────────────────

    /// 同候选连续 3 次曝光未采纳 → 第 4 次 coordinate 起被抑制。
    #[test]
    fn fatigue_suppresses_after_three_impressions() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-query", SuggestionKind::AskAi, 0.80, None)],
        )));
        let s = snap();
        let mut fatigue = fresh_fatigue();
        for round in 1..=3 {
            let (out, _) = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
            assert!(
                out.primary.is_some(),
                "第 {round} 次曝光仍可见（连续 3 次后才抑制）"
            );
        }
        let (out, _) = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        assert!(out.primary.is_none(), "第 4 轮起被降频抑制");
        assert!(out.secondary.is_none());
    }

    /// 采纳清零后恢复（clear 由编排层在 record_adoption 时调用）。
    #[test]
    fn fatigue_clear_restores_candidate() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-query", SuggestionKind::AskAi, 0.80, None)],
        )));
        let s = snap();
        let mut fatigue = fresh_fatigue();
        for _ in 0..3 {
            let _ = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        }
        let (out, _) = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        assert!(out.primary.is_none());
        // 任一次采纳 → 全量清零（编排层入口，此处直接验纯逻辑效果）
        fatigue.clear();
        let (out, _) = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        assert!(out.primary.is_some(), "清零后候选恢复");
    }

    /// suppress_repeated=false 只计数不抑制。
    #[test]
    fn fatigue_disabled_by_config() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-query", SuggestionKind::AskAi, 0.80, None)],
        )));
        let s = snap();
        let mut fatigue = fresh_fatigue();
        let mut i = input("帮我看看这段话", &s);
        i.config.suppress_repeated = false;
        for _ in 0..5 {
            let _ = coordinator.coordinate(&i, &mut fatigue);
        }
        let (out, _) = coordinator.coordinate(&i, &mut fatigue);
        assert!(out.primary.is_some(), "开关关闭时不抑制（计数继续供观测）");
    }

    /// 不同指纹不互相连坐（剪贴板换内容 → 新键）。
    #[test]
    fn fatigue_key_isolation_by_fingerprint() {
        // 同一 MockProducer 每轮换候选（fingerprint 不同——mock 固定 0，这里直接验
        // 纯逻辑：键不同不抑制），coordinator 层由 fatigue.rs 的键混合测试覆盖。
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![
                make_sug("ai-query", SuggestionKind::AskAi, 0.80, None),
                make_sug("ai-fallback", SuggestionKind::AskAi, 0.55, None),
            ],
        )));
        let s = snap();
        let mut fatigue = fresh_fatigue();
        // ai-query 每轮都进 primary；ai-fallback 被 Kind 去重（无曝光 → 不抑制）
        for _ in 0..4 {
            let _ = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        }
        // 只剩 ai-fallback 可产时（ai-query 已抑制），其计数为 0 → 不被连坐
        let (out, _) = coordinator.coordinate(&input("帮我看看这段话", &s), &mut fatigue);
        assert_eq!(out.primary.as_ref().unwrap().id, "ai-fallback");
    }

    /// 槽位数量 ≤ 2 且 Kind 不重复（§6.1 验收：Primary/Secondary 按 Kind 去重）。
    #[test]
    fn at_most_two_slots_with_distinct_kinds() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![
                make_sug(
                    "translate-selection",
                    SuggestionKind::Translate,
                    0.92,
                    Some(SuggestionOrigin::Selection),
                ),
                make_sug(
                    "translate-clipboard",
                    SuggestionKind::Translate,
                    0.82,
                    Some(SuggestionOrigin::Clipboard),
                ),
                make_sug(
                    "ai-selection",
                    SuggestionKind::AskAi,
                    0.72,
                    Some(SuggestionOrigin::Selection),
                ),
            ],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        let p = out.primary.unwrap();
        let sec = out.secondary.unwrap();
        assert_eq!(p.id, "translate-selection");
        assert_eq!(sec.id, "ai-selection");
        assert_ne!(p.kind, sec.kind);
    }

    // ── 0.24.5 §5.6 展示策略开关组 ────────────────────────────────────────

    /// completion_enabled=false：Completion 候选全灭，awareness 候选顶上。
    /// 0.24.9 修订：非空 query 下 query 池空 → primary 轮空，awareness 降级
    /// secondary（此前 awareness 直接顶 primary——正是"输入后建议盘踞 primary"
    /// 的同一缺口，补全 completion 闸的镜像用例）。
    #[test]
    fn completion_disabled_filters_completion_only() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug("completion-keyword", SuggestionKind::Completion, 0.98, None)],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "translate-clipboard",
                SuggestionKind::Translate,
                0.82,
                Some(SuggestionOrigin::Clipboard),
            )],
        )));
        let s = snap();
        let mut i = input("fa", &s);
        i.config.completion_enabled = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert!(
            out.primary.is_none(),
            "非空 query 且 query 池空 → primary 轮空"
        );
        assert_eq!(
            out.secondary.unwrap().id,
            "translate-clipboard",
            "awareness 最佳降级 secondary"
        );
    }

    /// 0.24.9：非空 query 且 query 池空 → awareness 不抢 primary（输入即意图表达）。
    /// 场景即实测反馈：剪贴板 URL 建议 0.95，输入任意文本后应立即让出 primary
    /// （此前的隐藏靠降频 3 连击，表现为"打几个字才收起"的假延迟）。
    #[test]
    fn nonempty_query_awareness_demoted_to_secondary() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "open-url-clipboard",
                SuggestionKind::OpenUrl,
                0.95,
                Some(SuggestionOrigin::Clipboard),
            )],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("xyz", &s), &mut fresh_fatigue());
        assert!(out.primary.is_none(), "非空 query 下 awareness 不抢 primary");
        assert_eq!(out.secondary.unwrap().id, "open-url-clipboard");

        // 空 query：同一候选照旧担任 primary（唤起即环境建议的既有形态不变）
        let (out, _) = coordinator.coordinate(&input("", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "open-url-clipboard");
    }

    /// 0.24.9：query 池非空时行为不变——query 派生 primary、awareness 最多 secondary。
    #[test]
    fn nonempty_query_with_query_pool_keeps_layering() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Keyword,
            vec![make_sug("completion-keyword", SuggestionKind::Completion, 0.75, None)],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "open-url-clipboard",
                SuggestionKind::OpenUrl,
                0.95,
                Some(SuggestionOrigin::Clipboard),
            )],
        )));
        let s = snap();
        let (out, _) = coordinator.coordinate(&input("fa", &s), &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "completion-keyword");
        assert_eq!(out.secondary.unwrap().id, "open-url-clipboard");
    }

    /// context_suggestion_enabled=false：awareness 派生候选全灭；
    /// query 派生（translate-query）不受影响——它是输入意图不是环境。
    #[test]
    fn context_suggestion_disabled_filters_awareness_only() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![
                make_sug("translate-query", SuggestionKind::Translate, 0.92, None),
                make_sug(
                    "translate-clipboard",
                    SuggestionKind::Translate,
                    0.82,
                    Some(SuggestionOrigin::Clipboard),
                ),
                make_sug(
                    "ai-selection",
                    SuggestionKind::AskAi,
                    0.72,
                    Some(SuggestionOrigin::Selection),
                ),
            ],
        )));
        let s = snap();
        let mut i = input("hello world", &s);
        i.config.context_suggestion_enabled = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "translate-query");
        assert!(
            out.secondary.is_none(),
            "awareness 派生的 ai-selection 被环境建议开关过滤"
        );
    }

    /// ai_suggestion_enabled=false：AskAi 候选全灭（Provider 可用也不出），
    /// Translate 不连坐。
    #[test]
    fn ai_suggestion_disabled_filters_askai_only() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Ai,
            vec![make_sug("ai-query", SuggestionKind::AskAi, 0.80, None)],
        )));
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![make_sug(
                "translate-selection",
                SuggestionKind::Translate,
                0.92,
                Some(SuggestionOrigin::Selection),
            )],
        )));
        let s = snap();
        let mut i = input("", &s);
        i.config.ai_suggestion_enabled = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "translate-selection");
        assert!(out.secondary.is_none(), "ai-query 被建议域独立闸过滤");
    }

    /// secondary_enabled=false：只出 primary 单槽（secondary 门槛无关）。
    #[test]
    fn secondary_disabled_yields_primary_only() {
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(MockProducer::new(
            SuggestionSource::Context,
            vec![
                make_sug("translate-query", SuggestionKind::Translate, 0.92, None),
                make_sug("ai-query", SuggestionKind::AskAi, 0.80, None),
            ],
        )));
        let s = snap();
        let mut i = input("hello world foo bar", &s);
        i.config.secondary_enabled = false;
        let (out, _) = coordinator.coordinate(&i, &mut fresh_fatigue());
        assert_eq!(out.primary.unwrap().id, "translate-query");
        assert!(out.secondary.is_none(), "secondary 槽被设置页关闭");
    }
}
