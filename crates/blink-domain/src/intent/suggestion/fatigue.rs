//! 建议降频（0.24 §3.8 会话级抑制开关）。
//!
//! **归属**：疲劳抑制归建议域——疲劳不是翻译专属（AskAi 同样常驻），插件也看不到
//! impression/adopt 信号，数据流决定归属。
//!
//! **计数键** = `(kind, origin, source_text 指纹)` 的会话内哈希（不落盘，与日志不记
//! 原文的隐私口径一致）；带指纹避免剪贴板换内容后误伤。
//!
//! **触发**：同键连续 **3 次未采纳** → 本会话抑制该候选；阈值硬编码，只留开/关
//! 一个配置（`SuggestionConfig.suppress_repeated`，默认开）。
//!
//! **清零**：任一次建议被采纳，或主窗口隐藏（每次唤起是新会话）。
//!
//! 计数器由 SearchService 持有、主窗口隐藏钩子清零；本模块纯函数化，表驱动测试。

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use super::Suggestion;

/// 同键连续未采纳次数达到该值后，本会话抑制该候选（§3.8 硬编码阈值）。
pub const SUPPRESS_THRESHOLD: u32 = 3;

/// 会话级降频计数器（进程内，不落盘）。
///
/// 键为 `fatigue_key` 混合哈希；值为该候选连续未采纳的曝光次数。
#[derive(Debug, Default)]
pub struct SuggestionFatigue {
    counters: HashMap<u64, u32>,
}

impl SuggestionFatigue {
    pub fn new() -> Self {
        Self::default()
    }

    /// 该候选是否已被本会话抑制（连续未采纳曝光 ≥ `SUPPRESS_THRESHOLD`）。
    pub fn is_suppressed(&self, key: u64) -> bool {
        self.counters
            .get(&key)
            .is_some_and(|n| *n >= SUPPRESS_THRESHOLD)
    }

    /// 记录一次曝光（候选进入可见槽位），返回更新后的连续未采纳次数。
    pub fn record_impression(&mut self, key: u64) -> u32 {
        let n = self.counters.entry(key).or_insert(0);
        *n = n.saturating_add(1);
        *n
    }

    /// 全量清零——任一次建议被采纳，或主窗口隐藏（新会话）。
    pub fn clear(&mut self) {
        self.counters.clear();
    }

    /// 当前计数条数（测试断言用）。
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        self.counters.is_empty()
    }
}

/// 构造候选的降频计数键：`(kind, origin, fingerprint)` 混合哈希。
///
/// - kind 区分"同一段文本的翻译建议 vs AI 建议"（疲劳不互相连坐）
/// - origin 区分"同一文本来自选区还是剪贴板"
/// - fingerprint 区分"剪贴板换内容了"（换内容即新键，计数自然重置）
pub(crate) fn fatigue_key(s: &Suggestion) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.kind.hash(&mut hasher);
    s.origin.hash(&mut hasher);
    s.fingerprint.hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::intent::{SuggestionAction, SuggestionKind, SuggestionOrigin};

    fn key(seed: u64) -> u64 {
        // 直接用 seed 当键（fatigue_key 的混合逻辑单测见下）
        seed
    }

    #[test]
    fn suppressed_after_three_unadopted_impressions() {
        let mut f = SuggestionFatigue::new();
        let k = key(1);
        assert!(!f.is_suppressed(k), "首次曝光前不抑制");
        assert_eq!(f.record_impression(k), 1);
        assert!(!f.is_suppressed(k), "第 1 次曝光后仍可见");
        assert_eq!(f.record_impression(k), 2);
        assert!(!f.is_suppressed(k), "第 2 次曝光后仍可见");
        assert_eq!(f.record_impression(k), 3);
        // 第 3 次曝光本身仍可见；第 4 轮 coordinate 起被抑制
        assert!(f.is_suppressed(k), "连续 3 次未采纳后抑制");
    }

    #[test]
    fn adoption_clears_all_counters() {
        let mut f = SuggestionFatigue::new();
        for k in [key(1), key(2), key(3)] {
            f.record_impression(k);
            f.record_impression(k);
            f.record_impression(k);
            assert!(f.is_suppressed(k));
        }
        f.clear();
        for k in [key(1), key(2), key(3)] {
            assert!(!f.is_suppressed(k), "采纳后全量清零");
        }
        assert!(f.is_empty());
    }

    #[test]
    fn window_hide_clears_all_counters() {
        // 隐藏清零与采纳清零走同一入口（§3.8 清零条件）
        let mut f = SuggestionFatigue::new();
        let k = key(7);
        for _ in 0..SUPPRESS_THRESHOLD {
            f.record_impression(k);
        }
        assert!(f.is_suppressed(k));
        f.clear();
        assert!(!f.is_suppressed(k));
    }

    #[test]
    fn distinct_keys_do_not_interfere() {
        // 指纹隔离：剪贴板换内容（新键）→ 不被旧内容的计数误伤
        let mut f = SuggestionFatigue::new();
        let old = key(1);
        for _ in 0..SUPPRESS_THRESHOLD {
            f.record_impression(old);
        }
        assert!(f.is_suppressed(old));
        assert!(!f.is_suppressed(key(2)), "新内容不继承旧计数");
    }

    /// fatigue_key 混合维度：kind / origin / fingerprint 任一不同 → 键不同。
    #[allow(deprecated)]
    fn make(
        kind: SuggestionKind,
        origin: Option<SuggestionOrigin>,
        fingerprint: u64,
    ) -> Suggestion {
        Suggestion {
            id: "test".to_string(),
            kind,
            action: SuggestionAction::RouteQuery {
                query: String::new(),
            },
            rank_score: 0.9,
            display: String::new(),
            prefix_len: 0,
            origin,
            fingerprint,
            source: crate::intent::SuggestionSource::Ai,
            ranking_hint: None,
        }
    }

    #[test]
    fn fatigue_key_mixes_kind_origin_fingerprint() {
        use crate::intent::SuggestionKind as K;
        let base = make(K::Translate, Some(SuggestionOrigin::Clipboard), 42);
        assert_eq!(fatigue_key(&base), fatigue_key(&base), "同候选键稳定");
        assert_ne!(
            fatigue_key(&base),
            fatigue_key(&make(K::AskAi, Some(SuggestionOrigin::Clipboard), 42)),
            "kind 不同 → 键不同"
        );
        assert_ne!(
            fatigue_key(&base),
            fatigue_key(&make(K::Translate, Some(SuggestionOrigin::Selection), 42)),
            "origin 不同 → 键不同"
        );
        assert_ne!(
            fatigue_key(&base),
            fatigue_key(&make(K::Translate, Some(SuggestionOrigin::Clipboard), 43)),
            "指纹不同 → 键不同"
        );
    }
}
