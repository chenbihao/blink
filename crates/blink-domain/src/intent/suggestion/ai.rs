//! AiProducer：AskAi 类 Suggestion 生产者（0.24 §5.3 从 SearchService 后置注入迁入）。
//!
//! 产出 §3.4 rank 表的 AskAi 候选：
//! - `ai-query` 0.80：query 为目标语言自然文本
//! - `ai-selection` 0.72：选区为目标语言自然文本
//! - `ai-foreign` 0.62：query/选区为外语自然文本（已有翻译主建议时作备选；
//!   与 `ai-query` 依分类天然互斥，Kind 去重保序）
//! - `ai-fallback` 0.55：其他合法自然语言（仅 query 派生；secondary 门槛挡其进第二槽，
//!   无其他候选时可独占 primary——保持"任何过筛 query 都能问 AI"的旧行为）
//!
//! `ai-trigger`（"ai " 前缀强信号）不经本 producer——arg 来自路由结果，
//! 由 Coordinator 从 `RouteSummary::AiTrigger` 构造（§3.7 编排层传值）。
//!
//! **Producer 纯度铁则**（§3.7）：produce 只读 query + snapshot；"AI Provider 是否
//! 可用"的判定不在这里——Coordinator eligibility 按 `availability.ai_available` 排除。
//! producer 侧读 registry 只为拿 `AiGate` 阈值做文本分类（与 KeywordProducer 读
//! min_score 同性质：共享配置快照，非可用性判定）。

use std::sync::{Arc, RwLock};

use crate::ai::gating::{self, AiGate, GateOutcome, NaturalTextClass};
use crate::ai::registry::AIProviderRegistry;
use crate::intent::RuleRouter;
use blink_infra::platform::context::{AwarenessSnapshot, AwarenessSource};

use super::producer::SuggestionProducer;
use super::{
    Suggestion, SuggestionAction, SuggestionKind, SuggestionOrigin, SuggestionSource,
    text_fingerprint,
};

/// AskAi Suggestion 生产者。
///
/// - `registry`：与 `SearchService::set_ai_registry` 共享同一 cell——registry 后置
///   注入（构造顺序倒挂），produce 每次读取最新值，未注入时安静返回空。
/// - `router`：读翻译绑定的 `target_lang`（选区/外语分类需要目标语言）。
pub struct AiProducer {
    registry: Arc<RwLock<Option<Arc<AIProviderRegistry>>>>,
    router: Arc<RuleRouter>,
}

impl AiProducer {
    pub fn new(
        registry: Arc<RwLock<Option<Arc<AIProviderRegistry>>>>,
        router: Arc<RuleRouter>,
    ) -> Self {
        Self { registry, router }
    }
}

impl SuggestionProducer for AiProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Ai
    }

    #[allow(deprecated)] // 构造 Suggestion 时填充 ranking_hint: None（§3.7 冻结 produce 签名，过渡通道保留）
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        // registry 未注入（setup 早期）→ 无法拿 AiGate 阈值，安静跳过
        let Some(reg) = self
            .registry
            .read()
            .expect("ai registry lock poisoned")
            .clone()
        else {
            return Vec::new();
        };
        let cfg = reg.config_snapshot();
        let gate = AiGate::from(&cfg);

        let target = self.router.suggestion_target_lang();
        let make = |id: &str, text: &str, rank: f64, origin: Option<SuggestionOrigin>| Suggestion {
            id: id.to_string(),
            kind: SuggestionKind::AskAi,
            action: SuggestionAction::EnterAiMode {
                prompt: text.to_string(),
                selection: None,
            },
            rank_score: rank,
            // AskAi 行文案由前端按 kind 经 i18n 覆盖（suggestion-bar rowText），
            // 此处 display 不被消费，保留中文占位以兼容仍读 display 的旧前端路径。
            display: "按 Tab 问 AI".to_string(),
            prefix_len: 0,
            origin,
            fingerprint: text_fingerprint(text),
            source: SuggestionSource::Ai,
            ranking_hint: None,
        };

        let mut out = Vec::new();
        let q = query.trim();
        // 0.28.28：本次唤起的有效划词快照（组合提问候选的材料）
        let selection_text = snapshot
            .find_text(AwarenessSource::Selection)
            .map(|v| v.text.clone())
            .filter(|t| !t.trim().is_empty());
        if !q.is_empty() {
            // AskAi 候选过 should_invoke_ai 总门禁（enabled + 结构性筛子）；
            // 分类用 classify_query（不含总开关——翻译候选不被 AI 开关连坐）。
            let ai_gate_ok = gating::should_invoke_ai(q, &gate) == GateOutcome::Invoke;
            if ai_gate_ok {
                match gating::classify_query(q, &target, &gate) {
                    NaturalTextClass::TargetNatural => out.push(make("ai-query", q, 0.80, None)),
                    NaturalTextClass::ForeignNatural => out.push(make("ai-foreign", q, 0.62, None)),
                    NaturalTextClass::NaturalFallback => {
                        out.push(make("ai-fallback", q, 0.55, None))
                    }
                    NaturalTextClass::Unclassified => {}
                }
                // 0.28.28：有输入且有有效划词 → 组合提问候选（Tab 主建议）：
                // 输入问题 + 划词快照成对与「仅用输入」变体占槽（Coordinator 配对）。
                // 材料分类沿用选区候选口径；与输入相同内容不重复组合；
                // origin=None（query 池，路由命中时被 3d 规则排除——确定性意图优先）。
                if let Some(sel) = &selection_text
                    && sel.trim() != q
                    && matches!(
                        gating::classify_awareness_text(sel, &target),
                        NaturalTextClass::TargetNatural | NaturalTextClass::ForeignNatural
                    )
                {
                    let combined = Suggestion {
                        id: "ai-combined".to_string(),
                        kind: SuggestionKind::AskAi,
                        action: SuggestionAction::EnterAiMode {
                            prompt: q.to_string(),
                            selection: Some(sel.to_string()),
                        },
                        rank_score: 0.81,
                        display: "按 Tab 问 AI".to_string(),
                        prefix_len: 0,
                        origin: None,
                        fingerprint: text_fingerprint(&format!("{q}\u{1}{sel}")),
                        source: SuggestionSource::Ai,
                        ranking_hint: None,
                    };
                    out.push(combined);
                }
            }
        } else if let Some(view) = snapshot.find_text(AwarenessSource::Selection) {
            match gating::classify_awareness_text(view.text, &target) {
                NaturalTextClass::TargetNatural => {
                    out.push(make(
                        "ai-selection",
                        view.text,
                        0.72,
                        Some(SuggestionOrigin::Selection),
                    ));
                }
                NaturalTextClass::ForeignNatural => {
                    out.push(make(
                        "ai-foreign",
                        view.text,
                        0.62,
                        Some(SuggestionOrigin::Selection),
                    ));
                }
                // 选区无兜底类（§3.4：兜底仅 query 派生）；不可分类同理跳过
                NaturalTextClass::NaturalFallback | NaturalTextClass::Unclassified => {}
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ai_config::AIConfig;
    use crate::context::trigger::TextSource;
    use crate::plugin::{ManifestSurfaceHint, PluginSettingResolver};

    /// 测试用 resolver：translate 插件 target_lang=zh。
    struct ZhResolver;
    impl PluginSettingResolver for ZhResolver {
        fn get_string(&self, _plugin_id: &str, _key: &str) -> Option<String> {
            Some("zh".into())
        }
        fn is_enabled(&self, _plugin_id: &str) -> bool {
            true
        }
    }

    /// 构造带翻译绑定（target=zh）的 router + 指定 enabled 状态的 AiProducer。
    fn producer_with_ai(enabled: bool) -> AiProducer {
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        router.add_context_rule(
            "builtin.translate".into(),
            crate::context::trigger::ContextTrigger::TextIsNonTargetLang {
                source: TextSource::SelectionThenClipboard,
            },
            ManifestSurfaceHint::Priority,
        );
        router.set_setting_resolver(Arc::new(ZhResolver));

        let mut cfg = AIConfig::default();
        cfg.enabled = enabled;
        let registry = Arc::new(AIProviderRegistry::from_config(
            crate::ai::default_factory(),
            &cfg,
        ));
        let cell = Arc::new(RwLock::new(Some(registry)));
        AiProducer::new(cell, router)
    }

    fn snap_with_selection(text: &str) -> AwarenessSnapshot {
        let mut snap = AwarenessSnapshot::default();
        snap.upsert_text(AwarenessSource::Selection, Some(text.to_string()));
        snap
    }

    fn find<'a>(sugs: &'a [Suggestion], id: &str) -> &'a Suggestion {
        sugs.iter()
            .find(|s| s.id == id)
            .unwrap_or_else(|| panic!("missing {id}"))
    }

    #[test]
    fn empty_registry_cell_yields_nothing() {
        // setup 早期 registry 未注入 → 安静返回空
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        let cell: Arc<RwLock<Option<Arc<AIProviderRegistry>>>> = Arc::new(RwLock::new(None));
        let producer = AiProducer::new(cell, router);
        assert!(
            producer
                .produce("hello world foo", &AwarenessSnapshot::default())
                .is_empty()
        );
    }

    #[test]
    fn disabled_registry_yields_nothing() {
        // 总开关关闭 → AskAi 候选一律不产（availability 之外的第二道门）
        let producer = producer_with_ai(false);
        assert!(
            producer
                .produce("帮我看看这段话", &AwarenessSnapshot::default())
                .is_empty()
        );
    }

    #[test]
    fn english_query_yields_ai_foreign() {
        let producer = producer_with_ai(true);
        let sugs = producer.produce("hello world foo bar", &AwarenessSnapshot::default());
        let sug = find(&sugs, "ai-foreign");
        assert!((sug.rank_score - 0.62).abs() < 1e-9);
        assert!(sug.origin.is_none());
        assert!(
            matches!(&sug.action, crate::intent::SuggestionAction::EnterAiMode { prompt, .. }
            if prompt == "hello world foo bar")
        );
    }

    #[test]
    fn chinese_query_yields_ai_query() {
        let producer = producer_with_ai(true);
        let sugs = producer.produce("你用的是什么模型", &AwarenessSnapshot::default());
        let sug = find(&sugs, "ai-query");
        assert!((sug.rank_score - 0.80).abs() < 1e-9);
        assert_eq!(sug.kind, crate::intent::SuggestionKind::AskAi);
    }

    #[test]
    fn mixed_without_target_script_yields_fallback() {
        let producer = producer_with_ai(true);
        let sugs = producer.produce("hello こんにちは", &AwarenessSnapshot::default());
        let sug = find(&sugs, "ai-fallback");
        assert!((sug.rank_score - 0.55).abs() < 1e-9);
    }

    #[test]
    fn unclassifiable_query_yields_nothing() {
        let producer = producer_with_ai(true);
        assert!(
            producer
                .produce("chrome", &AwarenessSnapshot::default())
                .is_empty()
        );
        assert!(
            producer
                .produce("12345", &AwarenessSnapshot::default())
                .is_empty()
        );
        assert!(
            producer
                .produce("", &AwarenessSnapshot::default())
                .is_empty()
        );
    }

    #[test]
    fn chinese_selection_yields_ai_selection() {
        let producer = producer_with_ai(true);
        let sugs = producer.produce("", &snap_with_selection("帮我看看这段话"));
        let sug = find(&sugs, "ai-selection");
        assert!((sug.rank_score - 0.72).abs() < 1e-9);
        assert_eq!(
            sug.origin,
            Some(crate::intent::SuggestionOrigin::Selection)
        );
    }

    #[test]
    fn english_selection_yields_ai_foreign_with_origin() {
        let producer = producer_with_ai(true);
        let sugs = producer.produce("", &snap_with_selection("this is selected text"));
        let sug = find(&sugs, "ai-foreign");
        assert_eq!(
            sug.origin,
            Some(crate::intent::SuggestionOrigin::Selection)
        );
    }

    #[test]
    fn query_and_selection_yield_combined_pair() {
        // 0.28.28：query 英文 + 选区中文 → 组合候选（ai-combined，携带划词材料）
        // 与「仅输入」变体（ai-foreign）成对产出；纯划词候选（ai-selection）不再
        // 并存——成对占槽由 Coordinator 承担，producer 只负责成对产出。
        let producer = producer_with_ai(true);
        let sugs = producer.produce("hello world foo", &snap_with_selection("帮我看看这段话"));
        let combined = sugs
            .iter()
            .find(|s| s.id == "ai-combined")
            .expect("query+selection 应产出组合候选");
        assert!((combined.rank_score - 0.81).abs() < 1e-9);
        assert!(matches!(
            &combined.action,
            SuggestionAction::EnterAiMode { prompt, selection: Some(sel) }
            if prompt == "hello world foo" && sel == "帮我看看这段话"
        ));
        assert!(combined.origin.is_none());
        assert!(sugs.iter().any(|s| s.id == "ai-foreign"), "仅输入变体成对产出");
        assert!(!sugs.iter().any(|s| s.id == "ai-selection"), "组合场景不重复产纯划词候选");
    }

    #[test]
    fn combined_skipped_when_selection_equals_query() {
        // 划词与输入内容相同 → 不组合（材料重复无意义），仅常规 query 候选
        let producer = producer_with_ai(true);
        let sugs = producer.produce("帮我看看这段话", &snap_with_selection("帮我看看这段话"));
        assert!(!sugs.iter().any(|s| s.id == "ai-combined"));
        assert!(sugs.iter().any(|s| s.id == "ai-query"));
    }

    #[test]
    fn selection_only_still_yields_ai_selection() {
        // 0.28.28 回归：空 query 时纯划词候选语义不变
        let producer = producer_with_ai(true);
        let sugs = producer.produce("", &snap_with_selection("帮我看看这段话"));
        assert!(sugs.iter().any(|s| s.id == "ai-selection"));
        assert!(!sugs.iter().any(|s| s.id == "ai-combined"), "无输入不产组合候选");
    }
}
