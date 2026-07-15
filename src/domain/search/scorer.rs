//! 统一的打分/加权模块（0.6 第一阶段重构）。
//!
//! # 设计原则
//!
//! 所有加权系数集中在此管理，避免分散在各引擎的硬编码魔法值。
//! 每个系数都有明确的设计意图说明，方便后续调参和理解。
//!
//! # 分数区间设计（第一阶段保持向后兼容）
//!
//! | 结果类型       | 分数区间 | 说明                                  |
//! |----------------|----------|---------------------------------------|
//! | 计算结果       | 1.0      | 精确命中，置信度最高                  |
//! | Priority 插件  | >1.0     | 通过 BOOST_PRIORITY 加成置顶          |
//! | 内置动作       | 1.2~2.0  | 硬编码分阶，默认高于应用              |
//! | 应用           | [0,1]    | fuzzy 分 + 历史加权后 top-relative    |
//! | 文件           | [0,0.5]  | 刻意压低，避免干扰应用搜索            |
//! | Inline 插件    | [0,1]    | 插件自行返回，clamp 限制              |
//!
//! # 历史加权公式
//!
//! ```text
//! bonus = ln(hit_count + 1) * HISTORY_BOOST_MAX
//! ```
//!
//! - hit=0   → bonus=0
//! - hit=1   → bonus≈0.21×HISTORY_BOOST_MAX
//! - hit=10  → bonus≈0.76×HISTORY_BOOST_MAX
//! - hit=100 → bonus≈1.38×HISTORY_BOOST_MAX （收益递减）
//!
//! 对数曲线的好处：常用应用有明显加成，但不会因为用了几百次就碾压一切。

use std::collections::HashMap;

// ── 加权系数（可理解为"旋钮"）──────────────────────────────────────────────

/// 历史加权的系数（归一化分数单位）。
///
/// 设计值 0.3：用了 10 次的应用比从未用过的同名应用高约 0.72 分，
/// 足以让常用应用排前，但对数曲线保证不会碾压一切。
const HISTORY_BOOST_COEFF: f32 = 0.3;

/// 历史加权的上限（防止用了几千次的应用彻底垄断第一名）。
///
/// 设计值 0.8：即使天天用，最多也就加 0.8 分，
/// 精确匹配的新应用（1.0 分）仍有可能超过它。
const HISTORY_BOOST_CEILING: f32 = 0.8;

/// 历史权重半衰期（天）。
///
/// 设计值 14 天：两周前用过的应用权重减半，三个月前的几乎不影响排序。
const HISTORY_HALF_LIFE_DAYS: f64 = 14.0;

/// 时间衰减系数 λ = ln(2) / 半衰期。
const HISTORY_DECAY_LAMBDA: f64 = std::f64::consts::LN_2 / HISTORY_HALF_LIFE_DAYS;

/// 内置动作的基础权重系数。
///
/// 设计值 1.2：内置动作默认比普通应用优先级高，
/// 但 0.8 分的精确匹配应用（如名字完全一样）仍能反超它。
#[allow(dead_code)] // 设计常量，当前实现使用硬编码值
const BUILTIN_BASE_WEIGHT: f32 = 1.2;

/// Priority 插件的置顶加成。
///
/// 设计值 1.5：让命中的 Priority 插件稳稳排在应用前面，
/// 同时保留插件内部的排序关系（score 高的插件仍在前面）。
const PRIORITY_BOOST: f32 = 1.5;

/// Priority 占位符的分数（确保它在结果最顶端）。
///
/// 设计值 3.0：应用最高理论分 = 1.0 (基础) + 0.8 (历史上限) + 0.4 (source) = 2.2
/// 3.0 有充足余量，确保占位符永远在第一位。
const PLACEHOLDER_PRIORITY_SCORE: f32 = 3.0;

/// Inline 占位符的分数（确保它在结果末尾，不干扰正常结果）。
const PLACEHOLDER_INLINE_SCORE: f32 = -1.0;

// ── 公共 API ────────────────────────────────────────────────────────────

/// 计算历史加权加分（含时间衰减）。
///
/// # 参数
/// - `hit_count`: 历史命中次数（从 history 表读取）
/// - `last_used_at`: 最后使用时间（Unix 时间戳秒）
///
/// # 返回
/// 加分值，范围 `[0, HISTORY_BOOST_CEILING]`，直接加到基础分数上。
///
/// # 设计意图
/// 对数曲线让常用应用脱颖而出，但收益递减——用了 10 次 vs 100 次的差异
/// 远小于 0 次 vs 1 次的差异。同时有上限保护，避免用了几千次的应用彻底垄断。
///
/// 0.7.5 新增时间衰减：半衰期 14 天，两周前权重减半，三个月前几乎不影响。
pub fn history_boost(hit_count: i64, last_used_at: i64) -> f32 {
    if hit_count <= 0 {
        return 0.0;
    }

    // 计算时间衰减因子
    let now = chrono::Utc::now().timestamp();
    let days_since_last_use = ((now - last_used_at).max(0) as f64) / 86400.0;
    let decay_factor = (-HISTORY_DECAY_LAMBDA * days_since_last_use).exp();

    let boost = ((hit_count as f64 + 1.0).ln() as f32) * HISTORY_BOOST_COEFF;
    let decayed = boost * decay_factor as f32;
    decayed.min(HISTORY_BOOST_CEILING)
}

/// 给 SearchItem 应用历史加权。
///
/// # 参数
/// - `score`: 原始匹配分
/// - `item_id`: 去重键（Open 用路径，Builtin 用 `builtin:{id}`）
/// - `history`: 历史权重表 (lnk_path -> (hit_count, last_used_at))
///
/// # 返回
/// 加权后的分数
pub fn apply_history(score: f32, item_id: &str, history: &HashMap<String, (i64, i64)>) -> f32 {
    let (hit_count, last_used_at) = history.get(item_id).copied().unwrap_or((0, 0));
    score + history_boost(hit_count, last_used_at)
}

/// 内置动作的匹配质量 → 分数转换。
///
/// # 设计意图
/// 内置动作不需要走 nucleo fuzzy（条目少且关键词明确），
/// 用简单分阶即可，但要保证和应用的归一化分数可比。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BuiltinMatch {
    /// 标题完全包含查询词（最高优先级）
    TitleContains,
    /// 关键词精确相等（如 "sz" == "sz"）
    KeywordExact,
    /// 关键词前缀匹配（如 "设" 匹配 "设置"）
    KeywordPrefix,
}

impl BuiltinMatch {
    /// 转换为归一化可比的分数。
    ///
    /// 设计目标：
    /// - TitleContains(2.0) > 应用最高分(1.0) + 最大历史加成(0.4)
    /// - KeywordExact(1.5) 也高于普通应用
    /// - KeywordPrefix(1.2) 略高于应用基线
    pub fn score(self) -> f32 {
        match self {
            BuiltinMatch::TitleContains => 2.0,
            BuiltinMatch::KeywordExact => 1.5,
            BuiltinMatch::KeywordPrefix => 1.2,
        }
    }
}

/// Priority 插件置顶加成。
///
/// # 设计意图
/// 让 Priority 插件（如 "天气"、"翻译"）在 Mixed 模式下优先显示，
/// 同时保留插件内部的排序关系（插件自己返回的 score 仍有意义）。
pub fn boost_priority(score: f32) -> f32 {
    score.max(0.0) + PRIORITY_BOOST
}

/// 占位符分数生成。
///
/// # 参数
/// - `is_priority`: 是否为 Priority 插件的占位符
///
/// # 返回
/// Priority 给高分置顶，Inline 给负分垫底
pub fn placeholder_score(is_priority: bool) -> f32 {
    if is_priority {
        PLACEHOLDER_PRIORITY_SCORE
    } else {
        PLACEHOLDER_INLINE_SCORE
    }
}

/// 归一化辅助：把原始分列表按最高分归一到 [0,1]。
///
/// StartMenuEngine 用的 top-relative 归一化。
/// 空列表 / 全零分 返回原序列（分数置 0）。
pub fn normalize_top_relative<T>(items: &mut [(T, f32)]) {
    let max_score = items.iter().map(|(_, s)| *s).fold(0.0f32, f32::max);

    if max_score > 0.0 {
        for (_, s) in items {
            *s /= max_score;
        }
    }
}

/// 插件 score clamp：确保插件返回的分数在合法范围 [0,1] 内。
///
/// # 设计意图
/// 插件是第三方代码，不能让它返回 100 分碾压所有本地结果。
/// clamp 是基本的安全边界。
pub fn clamp_plugin_score(score: f32) -> f32 {
    score.clamp(0.0, 1.0)
}

// ── 各引擎专用的打分公式 ──────────────────────────────────────────────

/// 计算结果的固定分数。
///
/// # 设计意图
/// 计算是精确匹配，置信度 100%，永远给满分。
pub const fn calc_score() -> f32 {
    1.0
}

/// 文件搜索的指数衰减打分。
///
/// # 参数
/// - `rank`: 排名（0 开始，0 = 第一个结果）
///
/// # 设计值
/// - 系数 0.25：确保文件分数远低于应用
/// - 底数 0.95：指数衰减，前几名差距大，后面差距小
/// - 地板 0.05：避免最后几名分数太低看不见
pub fn file_search_score(rank: usize) -> f32 {
    const BASE: f32 = 0.95;
    const COEFF: f32 = 0.25;
    const FLOOR: f32 = 0.05;
    (BASE.powi(rank as i32) * COEFF).max(FLOOR)
}

/// source 优先级排名（小 = 靠前）。
///
/// # 排序逻辑
/// calc > builtin > start_menu > file > 插件
///
/// # 用途
/// 1. fuse_items 二级排序（同分时的 tie-break）
/// 2. bake_source_boost 把优先级 baked 进分数（前端 merge 也能正确区分）
pub fn source_rank(source: &str) -> u8 {
    match source {
        "calc" => 0,
        "builtin" => 1,
        "start_menu" => 2,
        "file" => 3,
        _ => 4, // 插件默认最低
    }
}

/// 把 source 优先级 baked 进分数，用于跨来源排序。
///
/// # 原理
/// 给分数叠加 `SOURCE_BOOST_STEP * (4 - rank)`：
/// - calc(0) → +0.8
/// - builtin(1) → +0.6
/// - start_menu(2) → +0.4
/// - file(3) → +0.2
/// - 插件(4) → +0.0
///
/// 步长 0.2 确保来源优先级在排序中有足够区分度，同时不碾压真实匹配质量差异
/// （如 start_menu 精确匹配 1.0+0.4=1.4 仍低于 builtin 精确匹配 1.5+0.6=2.1）。
const SOURCE_BOOST_STEP: f32 = 0.2;

pub fn bake_source_boost(score: f32, source: &str) -> f32 {
    let rank = source_rank(source) as f32;
    score + (4.0 - rank) * SOURCE_BOOST_STEP
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn history_boost_curve() {
        let now = chrono::Utc::now().timestamp();
        // hit = 0 → 0
        assert!((history_boost(0, now) - 0.0).abs() < 1e-6);
        // hit = 1, 今天使用 → ln(2) × 0.3 ≈ 0.2079（衰减因子≈1.0）
        assert!((history_boost(1, now) - 0.207_944_16).abs() < 1e-2);
        // hit = 10, 今天使用 → ln(11) × 0.3 ≈ 0.7194
        assert!((history_boost(10, now) - 0.719_368_66).abs() < 1e-2);
        // hit = 100, 今天使用 → 被上限截断到 0.8
        assert!((history_boost(100, now) - 0.8).abs() < 1e-6);
        // 单调性检查：用得越多加成越高（上限以内）
        assert!(history_boost(10, now) > history_boost(1, now));
        assert!(history_boost(1, now) > history_boost(0, now));

        // 时间衰减测试：14天前使用权重减半
        let two_weeks_ago = now - 14 * 86400;
        let recent_boost = history_boost(10, now);
        let old_boost = history_boost(10, two_weeks_ago);
        // 14天前的加成应该约为当前的一半（衰减因子≈0.5）
        assert!(old_boost < recent_boost * 0.6, "14天前权重应明显衰减");
        assert!(old_boost > recent_boost * 0.4, "14天前权重不应完全消失");
    }

    #[test]
    fn apply_history_looks_up_by_id() {
        let now = chrono::Utc::now().timestamp();
        let mut h = HashMap::new();
        h.insert("a".to_string(), (10, now));
        h.insert("b".to_string(), (0, now));
        // 有历史的加分
        assert!(apply_history(0.5, "a", &h) > 0.5);
        // 没历史的不变
        assert!((apply_history(0.5, "b", &h) - 0.5).abs() < 1e-6);
        // 不在表里的也不变
        assert!((apply_history(0.5, "c", &h) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn builtin_match_ordering() {
        // 确保分阶正确
        assert!(BuiltinMatch::TitleContains.score() > BuiltinMatch::KeywordExact.score());
        assert!(BuiltinMatch::KeywordExact.score() > BuiltinMatch::KeywordPrefix.score());
        // 前缀分仍然 > 应用基线 1.0
        assert!(BuiltinMatch::KeywordPrefix.score() > 1.0);
    }

    #[test]
    fn boost_priority_never_negative() {
        // 插件给负分也会变成 PRIORITY_BOOST
        assert!((boost_priority(-0.5) - PRIORITY_BOOST).abs() < 1e-6);
        // 正常分数叠加
        assert!((boost_priority(0.5) - (0.5 + PRIORITY_BOOST)).abs() < 1e-6);
    }

    #[test]
    fn placeholder_score_values() {
        assert!((placeholder_score(true) - PLACEHOLDER_PRIORITY_SCORE).abs() < 1e-6);
        assert!((placeholder_score(false) - PLACEHOLDER_INLINE_SCORE).abs() < 1e-6);
        // Priority 占位 > 普通应用最高分
        assert!(placeholder_score(true) > 1.0);
        // Inline 占位 < 0，排最后
        assert!(placeholder_score(false) < 0.0);
    }

    #[test]
    fn normalize_top_relative_basic() {
        let mut items = vec![("a", 200.0), ("b", 100.0)];
        normalize_top_relative(&mut items);
        assert!((items[0].1 - 1.0).abs() < 1e-6);
        assert!((items[1].1 - 0.5).abs() < 1e-6);
    }

    #[test]
    fn normalize_top_relative_zero_max() {
        let mut items = vec![("a", 0.0), ("b", 0.0)];
        normalize_top_relative(&mut items);
        assert!((items[0].1 - 0.0).abs() < 1e-6);
        assert!((items[1].1 - 0.0).abs() < 1e-6);
    }

    #[test]
    fn clamp_plugin_score_enforces_bounds() {
        assert!((clamp_plugin_score(1.5) - 1.0).abs() < 1e-6);
        assert!((clamp_plugin_score(-0.5) - 0.0).abs() < 1e-6);
        assert!((clamp_plugin_score(0.5) - 0.5).abs() < 1e-6);
    }

    #[test]
    fn calc_score_is_one() {
        assert!((calc_score() - 1.0).abs() < 1e-6);
    }

    #[test]
    fn file_search_score_decays() {
        // 第一名 0.25
        assert!((file_search_score(0) - 0.25).abs() < 1e-6);
        // 第二名 0.95 × 0.25 = 0.2375
        assert!((file_search_score(1) - 0.2375).abs() < 1e-4);
        // 单调性：排名越靠后分数越低
        assert!(file_search_score(0) > file_search_score(1));
        assert!(file_search_score(1) > file_search_score(2));
        // 不低于地板值 0.05
        for i in 0..200 {
            assert!(file_search_score(i) >= 0.05 - 1e-6);
        }
    }

    #[test]
    fn source_rank_order() {
        assert_eq!(source_rank("calc"), 0);
        assert_eq!(source_rank("builtin"), 1);
        assert_eq!(source_rank("start_menu"), 2);
        assert_eq!(source_rank("file"), 3);
        assert_eq!(source_rank("plugin"), 4);
        assert_eq!(source_rank("unknown"), 4);
    }

    #[test]
    fn bake_source_boost_adds_correct_offset() {
        // calc 加 0.8 (step=0.2, rank=0, (4-0)*0.2)
        assert!((bake_source_boost(1.0, "calc") - 1.8).abs() < 1e-6);
        // builtin 加 0.6
        assert!((bake_source_boost(1.0, "builtin") - 1.6).abs() < 1e-6);
        // start_menu 加 0.4
        assert!((bake_source_boost(1.0, "start_menu") - 1.4).abs() < 1e-6);
        // file 加 0.2
        assert!((bake_source_boost(1.0, "file") - 1.2).abs() < 1e-6);
        // 插件加 0.0
        assert!((bake_source_boost(1.0, "plugin") - 1.0).abs() < 1e-6);
    }
}
