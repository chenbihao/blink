//! Autosuggestion / Ghost Text 计算（0.8.1 §2.4）。
//!
//! **发现工具，不是召回工具**：ghost text 让用户"看到"规范的 keyword 形式，
//! 不参与 route 匹配。部分拼音（`fan hello`）不进 route（保持严格），但走
//! 独立 fuzzy 通道生成 hint（`→ fanyi`）。
//!
//! 输入契约（`compute_hint`）：
//! - `keywords`：`(原文, pinyin_full)` 二元组列表，由 `RuleRouter::collect_suggest_keywords`
//!   从 keyword 规则表收集。regex 规则跳过（无"完整形式"概念）。
//! - `query`：用户当前输入原始文本（**未 trim**——保留末尾空格；`fanyi ` 与 `fanyi`
//!   语义不同——前者是"参数等待中"，后者是纯 keyword）。
//! - `min_score`：nucleo fuzzy 归一化分（`[0,1]`）阈值，默认 0.7。
//!
//! 输出：`Option<CompletionHint>`。
//! - 命中"已是完整形式"且已有尾空格/参数（`fanyi ` / `fanyi hello`）→ None（已进 Takeover）。
//! - 命中"已是完整形式"但无尾空格（`fanyi` / `翻译`）→ `display = ""` 的空 hint；
//!   前端只显示 `<kbd>Tab</kbd>` 按钮，Tab 后追加空格触发 Takeover。
//! - 否则对每个 keyword 展开两个 target（原文小写 + pinyin_full），取全体最高 fuzzy 分
//!   且过阈值那一个（`翻` → `翻译`；`fan` → `fanyi`）。
//!
//! **候选展开与底层的对齐**：`match_keyword`（route 层）会把一个 keyword 展成 3 个候选
//! （原文 / pinyin_full / pinyin_initials）用 严格等值+前缀 匹配；本文件是它的"模糊镜像"，
//! 只用前 2 个候选做 fuzzy 打分——initials 不重复喂入（`fy` 对 `fanyi` 的 fuzzy 分已够高，
//! 单独加会导致 `fy` 对 `fy` / `fanyi` 都打满分，picks_highest 平局随机化）。
//!
//! **`replacement` / `display` 语义**：跟着"命中最高分的那个 target"走。
//! - 输 `fy` → 命中 target `fanyi` → `display="fanyi"`，`replacement="fanyi "` 或 `"fanyi {arg}"`。
//! - 输 `翻` → 命中 target `翻译` → `display="翻译"`，`replacement="翻译 "` 或 `"翻译 {arg}"`。
//! - 已完整场景（`fanyi` / `翻译` 精确等值）→ `display=""`，前端只渲染 `<kbd>Tab</kbd>`
//!   按钮（示意"按 Tab 进入插件参数模式"），`replacement="{keyword_part} "`。
//!
//! **歧义策略**：多个 keyword 打分相同（例如 `tq` 同时是"天气/探亲"首拼）时，按 keyword
//! 注册顺序取第一个（`>` 严格比较）。历史加权是后续 iteration 的事，本层不做。

use serde::Serialize;

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

/// Ghost text 补全提示（前端渲染灰色行内 overlay）。
#[derive(Debug, Clone, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CompletionHint {
    /// Tab 后的完整替换文本（keyword + " " + arg；无参时末尾带空格）。
    pub replacement: String,
    /// UI 显示的"更规范的" keyword 形式（pinyin_full 或原文）。
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
/// 3. 全局取最高分那一个 (best_target, is_exact)。
/// 4. 决策：
///    - `is_exact` + 已带尾空格/参数 → None（已进 Takeover）。
///    - `is_exact` + 无尾内容 → Tab-only hint（display=""）。
///    - fuzzy 命中 → 常规 hint（display=target，replacement=`target ` 或 `target {arg}`）。
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

    // (score, target, is_exact_equal)。精确等值用 f64::INFINITY 保证盖过任何 fuzzy 分。
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
                    best = Some((f64::INFINITY, target.to_string(), true));
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
                best = Some((norm, target.to_string(), false));
            }
        }
    }

    let (raw_score, target, is_exact) = best?;

    // 精确等值 + 已带尾空格/参数 → 已进 Takeover，不出 hint。
    if is_exact && has_trailing {
        return None;
    }

    // 精确等值 + 无尾内容 → Tab-only hint（display=""）。
    // replacement 用 keyword_part（保留用户原样的大小写/中文，仅追加空格）。
    if is_exact {
        return Some((
            CompletionHint {
                replacement: format!("{keyword_part} "),
                display: String::new(),
                prefix_len: query.len(),
            },
            1.0,
        ));
    }

    // 常规 fuzzy hint：replacement 跟着命中的 target 走。
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
        raw_score,
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
    fn initials_no_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fy", 0.7).expect("should hint");
        assert_eq!(hint.display, "fanyi");
        assert_eq!(hint.replacement, "fanyi "); // 尾空格
    }

    #[test]
    fn initials_with_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fy hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "fanyi");
        assert_eq!(hint.replacement, "fanyi hello");
    }

    #[test]
    fn partial_pinyin_with_arg() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fan hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "fanyi");
        assert_eq!(hint.replacement, "fanyi hello");
    }

    #[test]
    fn partial_pinyin_multi_char() {
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fany hello", 0.7).expect("should hint");
        assert_eq!(hint.display, "fanyi");
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
    fn full_form_pinyin_no_arg_gives_tab_only_hint() {
        // 已完整 + 无尾内容 → 返回 display="" 的 Tab-only hint（提示按 Tab 进入参数模式）
        let kws = sample_keywords();
        let hint = compute_hint(&kws, "fanyi", 0.7).expect("should have tab-only hint");
        assert_eq!(hint.display, "");
        assert_eq!(hint.replacement, "fanyi ");
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
        assert_eq!(hint.display, "tianqi");
        assert_eq!(hint.replacement, "tianqi 北京");
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
        assert_eq!(hint.display, "fanyi");
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
        // "fanyi" 精确等值命中"翻译"的 pinyin_full → 走 Tab-only 分支，
        // 不应被其他 keyword 的 fuzzy 分数"抢走"（精确用 f64::INFINITY 顶格）。
        let kws = vec![
            ("翻译".to_string(), "fanyi".to_string()),
            // 构造一个 fuzzy 也能贴到 fanyi 的 keyword
            ("fan_prefix".to_string(), "fanprefix".to_string()),
        ];
        let hint = compute_hint(&kws, "fanyi", 0.7).expect("should have tab-only");
        assert_eq!(hint.display, "");
        assert_eq!(hint.replacement, "fanyi ");
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
        assert_eq!(hint.display, "tianqi");
    }
}
