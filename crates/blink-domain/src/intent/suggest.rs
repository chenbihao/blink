//! Autosuggestion / Ghost Text 计算（0.8.1 §2.4）。
//!
//! 拼音与首拼只作匹配别名；展示和采纳始终回到注册的真实关键词。
//! 原文/完整拼音精确命中且已带空格或参数时不再补全；原文完整命中
//! 仅提示采纳键，Tab 追加空格。其余命中显示真实关键词并保留用户参数。
//! 同分按注册顺序取首个；不改变路由、仲裁或统一的 Tab 采纳协议。

use serde::Serialize;

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

/// Ghost text 补全提示（前端渲染灰色行内 overlay）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionHint {
    /// Tab 后的完整替换文本（keyword + " " + arg；无参时末尾带空格）。
    pub replacement: String,
    /// UI 显示注册的原文关键词（拼音仅用于匹配）。
    pub display: String,
    /// 用户已输入部分的长度（字节，前端渲染灰色补全时对齐用）。
    pub prefix_len: usize,
}

/// 从 keyword 表 + query 算 hint。
///
/// - 返回 None 的情况：query 已完整命中且带尾空格/参数 / 所有 keyword 分数低于阈值 / query 空。
/// - keyword 列表 `(原文, pinyin_full)`；`pinyin_full` 为空时（如纯 ASCII keyword）
///   `原文小写` 与 `pinyin_full` 视为同一候选，不重复打分。
///
/// 算法（一次遍历，统一决策）：
/// 1. 拆 keyword_part vs arg_part（首段空白分割）。
/// 2. 对每个 keyword 展开 `[原文小写, pinyin_full]` 两个 target，逐个：
///    - **精确等值**（`keyword_part == target`）：优先级最高（分数 ∞，用 `f64::INFINITY` 标记）。
///    - **fuzzy 部分匹配**：nucleo 打分归一化后过阈值即入选。
/// 3. 全局取最高分对应的原文关键词和精确标记。
/// 4. 决策：
///    - `is_exact` + 已带尾空格/参数 → None（已进 Takeover）。
///    - 原文精确命中 + 无尾内容 → Tab-only hint（display=""）。
///    - 拼音或 fuzzy 命中 → 展示并补全原文关键词，保留参数。
pub fn compute_hint(
    keywords: &[(String, String)],
    query: &str,
    min_score: f64,
) -> Option<CompletionHint> {
    compute_hint_scored(keywords, query, min_score).map(|(h, _)| h)
}

/// 带 fuzzy 分数的孪生 API（0.8.3 §4.13 P0-2）。
///
/// **动机**：`CompletionHint` 公开字段不含 score（避免序列化冗余给前端），
/// 但 `best_suggestion` 内部竞争需要 `Suggestion.confidence`——直接从这里取。
///
/// 返回 `(hint, score)`：
/// - `is_exact` 命中 → `score = 1.0`（精确等值天然满信心，压过任何 fuzzy）
/// - `fuzzy` 命中 → 归一化后的原始 fuzzy 分（`[min_score, 1.0]`）
/// - 其他 → None
pub fn compute_hint_scored(
    keywords: &[(String, String)],
    query: &str,
    min_score: f64,
) -> Option<(CompletionHint, f64)> {
    // 空 query 不出 hint（避免 shown 事件后满屏"→ fanyi"）
    let trimmed = query.trim_start();
    if trimmed.is_empty() {
        return None;
    }

    // 拆 keyword 部分 vs 参数部分。split_whitespace 第一段作 keyword_part，
    // 其余保留在 arg_part（不做归一化，原样透传）。
    let (keyword_part, arg_part) = split_first_word(query);
    let keyword_lower = keyword_part.to_ascii_lowercase();

    // 是否已带尾空格/参数：`fanyi ` 或 `fanyi hello` 说明已进 Takeover 语义。
    // trim_start 后长度 > keyword_part 字节长度即认为"已经跨过 keyword 边界"。
    let has_trailing = trimmed.len() > keyword_part.len();

    // nucleo pattern 只构一次。
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        &keyword_lower,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();

    // (score, canonical_keyword, is_exact_equal)。精确等值用 f64::INFINITY 保证盖过任何 fuzzy 分。
    let mut best: Option<(f64, String, bool)> = None;

    for (orig, full) in keywords {
        // 展开候选 target：原文小写 + pinyin_full。去重（纯 ASCII keyword 两者相等）。
        let orig_lower = orig.to_ascii_lowercase();
        let mut targets: Vec<&str> = Vec::with_capacity(2);
        if !orig_lower.is_empty() {
            targets.push(orig_lower.as_str());
        }
        if !full.is_empty() && full != &orig_lower {
            targets.push(full.as_str());
        }

        for target in targets {
            // 精确等值：优先级最高。
            if keyword_lower == target {
                if best
                    .as_ref()
                    .map(|(s, _, _)| *s < f64::INFINITY)
                    .unwrap_or(true)
                {
                    best = Some((f64::INFINITY, orig.clone(), true));
                }
                continue;
            }

            // fuzzy 打分。
            let raw_score = {
                let haystack = Utf32Str::new(target, &mut buf);
                pattern.score(haystack, &mut matcher)
            };
            let Some(raw) = raw_score else { continue };
            // 归一化：raw 上限 ≈ keyword_lower.chars() * SCORE_MATCH(16)。夹到 [0,1]。
            let denom = (keyword_lower.chars().count() as f64) * 16.0;
            let norm = if denom > 0.0 {
                (raw as f64 / denom).min(1.0)
            } else {
                0.0
            };
            if norm < min_score {
                continue;
            }
            if best.as_ref().map(|(s, _, _)| norm > *s).unwrap_or(true) {
                best = Some((norm, orig.clone(), false));
            }
        }
    }

    let (raw_score, target, is_exact) = best?;

    // 精确等值 + 已带尾空格/参数 → 已进 Takeover，不出 hint。
    if is_exact && has_trailing {
        return None;
    }

    // 原文精确命中才只显示采纳键；拼音命中显示真实关键词。
    if is_exact && keyword_part.eq_ignore_ascii_case(&target) {
        return Some((
            CompletionHint {
                replacement: format!("{keyword_part} "),
                display: String::new(),
                prefix_len: query.len(),
            },
            1.0,
        ));
    }

    // fuzzy / 拼音命中统一补全注册原文，避免暴露内部拼音形式。
    let replacement = if arg_part.is_empty() {
        format!("{target} ")
    } else {
        format!("{target} {arg_part}")
    };

    Some((
        CompletionHint {
            replacement,
            display: target,
            prefix_len: query.len(),
        },
        if is_exact { 1.0 } else { raw_score },
    ))
}

/// 按空白拆分首段：`"fy hello world"` → `("fy", "hello world")`。
/// 保留原样大小写（keyword_lower 单独在调用处处理）。
fn split_first_word(s: &str) -> (&str, &str) {
    let s = s.trim_start();
    match s.find(char::is_whitespace) {
        Some(idx) => (&s[..idx], s[idx..].trim_start()),
        None => (s, ""),
    }
}

// ── 单测 ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造测试用 keyword 表。翻译（含中文）+ 天气 + translate（纯 ASCII）。
    fn sample_keywords() -> Vec<(String, String)> {
        vec![
            ("翻译".to_string(), "fanyi".to_string()),
            ("天气".to_string(), "tianqi".to_string()),
            ("translate".to_string(), "translate".to_string()),
        ]
    }

    #[test]
    fn pinyin_aliases_show_real_keywords_and_keep_finite_score() {
        let keywords = vec![
            ("剪贴板".into(), "jiantieban".into()),
            ("clip".into(), "clip".into()),
            ("jtb".into(), "jtb".into()),
            ("jiantieban".into(), "jiantieban".into()),
        ];
        for query in ["jt", "jiantie", "jiantieban"] {
            let (hint, score) = compute_hint_scored(&keywords, query, 0.7).unwrap();
            assert_eq!(hint.display, "剪贴板", "{query}");
            assert_eq!(hint.replacement, "剪贴板 ");
            assert!(score.is_finite() && score <= 1.0);
        }
        let hint = compute_hint(&keywords, "cl file  名称", 0.7).unwrap();
        assert_eq!(hint.display, "clip");
        assert_eq!(hint.replacement, "clip file  名称");
        let hint = compute_hint(&keywords, "jtb", 0.7).unwrap();
        assert_eq!(hint.display, "");
        assert_eq!(hint.replacement, "jtb ");
    }

    #[test]
    fn initials_no_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fy", 0.7).expect("should hint");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 "); // 尾空格
    }

    #[test]
    fn initials_with_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fy hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 hello");
    }

    #[test]
    fn partial_pinyin_with_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fan hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 hello");
    }

    #[test]
    fn partial_pinyin_multi_char() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fany hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "翻译");
    }

    #[test]
    fn full_form_original_with_arg_no_hint() {
        let kws = sample_keywords();
        // 已完整 + 带参 → 已进 Takeover，无 hint
        assert_eq!(compute_hint(&kws, "翻译 hello", 0.7), None);
    }

    #[test]
    fn full_form_pinyin_with_arg_no_hint() {
        let kws = sample_keywords();
        // 已完整 + 带参 → 已进 Takeover，无 hint
        assert_eq!(compute_hint(&kws, "fanyi hello", 0.7), None);
    }

    #[test]
    fn full_form_pinyin_no_arg_shows_canonical_keyword() {
        // 完整拼音仍展示真实关键词，避免把内部别名当作规范写法。
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fanyi", 0.7).expect("should show canonical keyword");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 ");
    }

    #[test]
    fn full_form_original_no_arg_gives_tab_only_hint() {
        // 中文原文命中同理
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "翻译", 0.7).expect("should have tab-only hint");
        assert_eq!(hint.display, "");
        assert_eq!(hint.replacement, "翻译 ");
    }

    #[test]
    fn full_form_with_trailing_space_no_hint() {
        // "fanyi " 已有尾空格 → 已进 Takeover，不再提示
        let kws = sample_keywords();
        assert_eq!(compute_hint(&kws, "fanyi ", 0.7), None);
        assert_eq!(compute_hint(&kws, "翻译 ", 0.7), None);
    }

    #[test]
    fn no_match_returns_none() {
        let kws = sample_keywords();
        assert_eq!(compute_hint(&kws, "chrome", 0.7), None);
    }

    #[test]
    fn empty_query_returns_none() {
        let kws = sample_keywords();
        assert_eq!(compute_hint(&kws, "", 0.7), None);
        assert_eq!(compute_hint(&kws, "   ", 0.7), None);
    }

    #[test]
    fn threshold_gate() {
        let kws = sample_keywords();
        // 阈值高于归一化上限（1.0）→ 一切命中被拦截
        assert_eq!(compute_hint(&kws, "fy hello", 1.5), None);
        assert_eq!(compute_hint(&kws, "fan hello", 1.5), None);
    }

    #[test]
    fn chinese_keyword_initials() {
        let kws = sample_keywords();
        // 天气首拼 "tq"
        let hint = compute_hint(&kws, "tq 北京", 0.7).expect("should hint");
        assert_eq!(hint.display, "天气");
        assert_eq!(hint.replacement, "天气 北京");
    }

    #[test]
    fn picks_highest_score() {
        // fy 更贴 fanyi 而非 fanyu（虚构 keyword，同 f 开头）
        let kws = vec![
            ("翻译".to_string(), "fanyi".to_string()),
            ("废鱼".to_string(), "feiyu".to_string()),
        ];
        let hint = compute_hint(&kws, "fy hello", 0.7).expect("should hint");
        // "fy" 对 "fanyi" 分数更高（首尾字母都命中），应选 fanyi
        assert_eq!(hint.display, "翻译");
    }

    #[test]
    fn prefix_len_equals_query_len() {
        let kws = sample_keywords();
        let query = "fy hello";
        let hint = compute_hint(&kws, query, 0.7).expect("should hint");
        assert_eq!(hint.prefix_len, query.len());
    }

    #[test]
    fn no_arg_replacement_trailing_space() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fy", 0.7).expect("should hint");
        assert!(hint.replacement.ends_with(' '));
    }

    // ── 0.8.1 补丁：中文原文候选 + 多 keyword 优先级 + 提前返回修复 ──

    #[test]
    fn chinese_prefix_matches_original() {
        // 输 "翻" → 中文原文候选参与 fuzzy → 命中 "翻译"。
        // display 用命中的原文（不是 pinyin_full），跟用户输入模态一致。
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "翻", 0.7).expect("should hint chinese");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 ");
    }

    #[test]
    fn chinese_prefix_with_arg() {
        // 中文前缀带参：replacement 也走原文（打中文补中文）。
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "翻 hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 hello");
    }

    #[test]
    fn cal_and_calendar_no_early_return() {
        // 关键 case：cal 是 calc 的完整形式，也是 calendar 的前缀。
        // 修复前：命中 cal 立即返回 Tab-only，看不到 calendar 建议。
        // 修复后：cal 精确等值分数(∞) > calendar fuzzy 分数，仍返回 Tab-only——
        //         但决策路径统一，不再有"提前 return 短路"结构性 bug。
        let kws = vec![
            ("cal".to_string(), "cal".to_string()),
            ("calendar".to_string(), "calendar".to_string()),
        ];
        let hint = compute_hint(&kws, "cal", 0.7).expect("should hint");
        // 精确等值优先：display="" 的 Tab-only hint（cal 本身是完整 keyword）
        assert_eq!(hint.display, "");
        assert_eq!(hint.replacement, "cal ");
    }

    #[test]
    fn partial_prefix_when_no_exact_hit() {
        // 但如果 query 不是任何 keyword 的精确形式，就应该走 fuzzy 找最像的。
        // 输 "cale" → 不精确命中 cal，走 fuzzy → 命中 calendar。
        let kws = vec![
            ("cal".to_string(), "cal".to_string()),
            ("calendar".to_string(), "calendar".to_string()),
        ];
        let hint = compute_hint(&kws, "cale", 0.7).expect("should hint calendar");
        assert_eq!(hint.display, "calendar");
        assert_eq!(hint.replacement, "calendar ");
    }

    #[test]
    fn exact_beats_fuzzy_across_keywords() {
        // "fanyi" 精确等值命中"翻译"的 pinyin_full → 显示原文，
        // 不应被其他 keyword 的 fuzzy 分数"抢走"（精确用 f64::INFINITY 顶格）。
        let kws = vec![
            ("翻译".to_string(), "fanyi".to_string()),
            // 构造一个 fuzzy 也能贴到 fanyi 的 keyword
            ("fan_prefix".to_string(), "fanprefix".to_string()),
        ];
        let hint = compute_hint(&kws, "fanyi", 0.7).expect("should show canonical keyword");
        assert_eq!(hint.display, "翻译");
        assert_eq!(hint.replacement, "翻译 ");
    }

    #[test]
    fn multi_keyword_same_initials_first_wins() {
        // 首拼碰撞（tq 同时是 天气 / 探亲）：picks_highest 用严格 `>`，
        // 分数相等时先注册者赢——本层不做历史加权，接受这个确定性但不"完美"的行为。
        let kws = vec![
            ("天气".to_string(), "tianqi".to_string()),
            ("探亲".to_string(), "tanqin".to_string()),
        ];
        let hint = compute_hint(&kws, "tq", 0.7).expect("should hint");
        // 先注册的"天气" pinyin_full=tianqi 胜出
        assert_eq!(hint.display, "天气");
    }
}
