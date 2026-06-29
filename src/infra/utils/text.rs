//! 文本归一化原语 —— 应用搜索与意图路由共用。
//!
//! 抽离原因(0.4 §4.4):keyword 匹配与应用搜索 fuzzy 共用同一套归一化(小写 + 拼音首字母),
//! 避免两处散落、逻辑分叉。

use pinyin::ToPinyin;

/// UTF-8 字符串安全截断扩展（按字符数截断，而非字节数）。
///
/// # 背景
/// Rust 字符串是 UTF-8 编码，中文每个字符占 3 字节。直接 `&s[..50]` 按字节截断
/// 很可能切在多字节字符中间，导致 `panicked at byte index 50 is not a char boundary`。
///
/// # Clippy 防护
/// 本项目已开启 `clippy::string_slice = "deny"`，所有直接按字节切片字符串的写法
/// 都会在编译期被拦截，必须使用本 trait 提供的安全方法。
///
/// # 示例
/// ```
/// use blink::text::StringTruncateExt;
///
/// let s = "这是一段很长的中文文本";
/// assert_eq!(s.truncate_chars(5), "这是一段很...");  // 安全截断 + 省略号
/// assert_eq!(s.take_chars(5), "这是一段很");         // 只截断，不加省略号
/// ```
#[allow(dead_code)] // 工具 trait，测试验证正确性，未来可能使用
pub trait StringTruncateExt {
    /// 安全截断到最多 `max_chars` 个字符，超出部分用 `...` 表示。
    ///
    /// 字符数计算包含 ASCII 和多字节 Unicode 字符，每个中文算 1 个字符。
    fn truncate_chars(&self, max_chars: usize) -> String;

    /// 安全获取前 `n` 个字符，不添加省略号。
    fn take_chars(&self, n: usize) -> String;
}

impl StringTruncateExt for str {
    fn truncate_chars(&self, max_chars: usize) -> String {
        let char_count = self.chars().count();
        if char_count <= max_chars {
            return self.to_string();
        }
        format!("{}...", self.chars().take(max_chars).collect::<String>())
    }

    fn take_chars(&self, n: usize) -> String {
        self.chars().take(n).collect()
    }
}

impl StringTruncateExt for String {
    fn truncate_chars(&self, max_chars: usize) -> String {
        self.as_str().truncate_chars(max_chars)
    }

    fn take_chars(&self, n: usize) -> String {
        self.as_str().take_chars(n)
    }
}

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

/// 提取完整拼音（"微信" → "weixin"，"WeChat" → "wechat"）。
pub fn pinyin_full(s: &str) -> String {
    let mut result = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() {
            result.push(c.to_ascii_lowercase());
        } else if let Some(p) = c.to_pinyin() {
            result.push_str(&p.plain().to_ascii_lowercase());
        }
    }
    result
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
    fn pinyin_full_basic() {
        assert_eq!(pinyin_full("微信"), "weixin");
        assert_eq!(pinyin_full("天气"), "tianqi");
        assert_eq!(pinyin_full("WeChat"), "wechat");
        // 中英混合
        assert_eq!(pinyin_full("VS Code"), "vscode");
        assert_eq!(pinyin_full("微信PC"), "weixinpc");
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

    // ── StringTruncateExt 测试 ────────────────────────────────────────

    #[test]
    fn truncate_chars_ascii() {
        let s = "Hello, World!";
        assert_eq!(s.truncate_chars(5), "Hello...");
        assert_eq!(s.truncate_chars(20), "Hello, World!"); // 不超过长度
        assert_eq!(s.truncate_chars(0), "..."); // 边界：0 字符
    }

    #[test]
    fn truncate_chars_chinese() {
        let s = "这是一段很长的中文文本"; // 共 11 个字符
        assert_eq!(s.truncate_chars(5), "这是一段很..."); // 每个中文算 1 字符
        assert_eq!(s.truncate_chars(11), s); // 刚好等于，不截断
        assert_eq!(s.truncate_chars(15), s); // 超过长度，原样返回
    }

    #[test]
    fn truncate_chars_mixed() {
        let s = "Hello 你好 World 世界";
        assert_eq!(s.truncate_chars(8), "Hello 你好...");
        assert_eq!(s.truncate_chars(20), s);
    }

    #[test]
    fn take_chars_basic() {
        let s = "这是一段很长的中文";
        assert_eq!(s.take_chars(3), "这是一");
        assert_eq!(s.take_chars(10), s);
        assert_eq!("".take_chars(5), "");
    }

    #[test]
    fn take_chars_mixed() {
        let s = "ABC 中文 123";
        assert_eq!(s.take_chars(5), "ABC 中");
    }

    #[test]
    fn take_chars_string_type() {
        // 测试 String 类型也能直接调用
        let s = String::from("测试 String 类型");
        assert_eq!(s.truncate_chars(4), "测试 S...");
        assert_eq!(s.take_chars(2), "测试");
    }

    #[test]
    #[should_panic(expected = "byte index")]
    #[allow(clippy::string_slice)]
    #[cfg(test)]
    fn verify_dangerous_slice_panics() {
        // 验证：直接按字节切中文确实会 panic（反向证明 Clippy lint 的必要性）
        let s = "这是中文";
        let _ = &s[..2]; // 字节 2 不在字符边界
    }
}
