//! 文本归一化原语 —— 应用搜索与意图路由共用。
//!
//! 抽离原因(0.4 §4.4):keyword 匹配与应用搜索 fuzzy 共用同一套归一化(小写 + 拼音首字母),
//! 避免两处散落、逻辑分叉。

use pinyin::ToPinyin;

/// 提取拼音首字母（"微信" → "wx"，"WeChat" → "wechat"）。
pub fn pinyin_initials(s: &str) -> String {
    s.chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() {
                Some(c.to_ascii_lowercase())
            } else {
                c.to_pinyin()
                    .and_then(|p| p.first_letter().to_ascii_lowercase().chars().next())
            }
        })
        .collect()
}

/// keyword / query 的归一化候选:[原文小写, 拼音首字母],过滤空串。
///
/// 应用搜索与意图 keyword 匹配共用此原语——各自匹配策略不同,但归一化一致。
/// 例:`"天气"` → `["天气", "tq"]`;`"WeChat"` → `["wechat", "wechat"]`(去重后只剩一个)。
pub fn normalize_candidates(s: &str) -> Vec<String> {
    let lower = s.to_ascii_lowercase();
    let pinyin = pinyin_initials(s);
    let mut out = Vec::with_capacity(2);
    if !lower.is_empty() {
        out.push(lower);
    }
    if !pinyin.is_empty() && !out.contains(&pinyin) {
        out.push(pinyin);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pinyin_initials_basic() {
        assert_eq!(pinyin_initials("微信"), "wx");
        assert_eq!(pinyin_initials("WeChat"), "wechat");
    }

    #[test]
    fn normalize_candidates_basic() {
        // 中文:小写 + 首拼两个候选
        let c = normalize_candidates("天气");
        assert_eq!(c, vec!["天气", "tq"]);

        // 纯 ASCII:小写与首拼相同,去重后只剩一个
        let c = normalize_candidates("WeChat");
        assert_eq!(c, vec!["wechat"]);
    }
}
