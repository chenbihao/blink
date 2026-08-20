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

/// 判断字符是否为 CJK（中日韩统一表意文字 + 韩文 + 全角符号 + 假名）。
///
/// 0.21.23: 全仓唯一实现（原 domain `token_budget::is_cjk` 与 infra
/// `conversations::is_cjk_char` 手工镜像 + 一致性测试，现下沉单一真源；
/// domain 依赖 infra 合法）。合并 `memory.rs` 和 `prompt.rs` 两套旧 `is_cjk`
/// 的并集，取最宽覆盖。
pub fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x3000..=0x33FF    // CJK 符号和标点 + 假名（平假名/片假名）
        | 0x3400..=0x4DBF  // CJK 扩展 A
        | 0x4E00..=0x9FFF  // CJK 统一表意文字
        | 0xAC00..=0xD7AF  // 韩文音节
        | 0xF900..=0xFAFF  // CJK 兼容表意文字
        | 0xFF00..=0xFFEF  // 半角/全角形式
        | 0x20000..=0x2A6DF // CJK 扩展 B
    )
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

/// 将换行/回车替换为可见转义（`\n`/`\r`），保证字符串在日志中保持单行。
///
/// AI 流式内容（thinking/text）常携带换行，直接作为 tracing 字段会打断日志行；
/// 本函数用于日志字段清洗，不改动原始内容。
pub fn single_line(s: &str) -> String {
    s.replace('\r', "\\r").replace('\n', "\\n")
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

    // ── single_line 测试 ─────────────────────────────────────────────

    #[test]
    fn single_line_escapes_breaks() {
        assert_eq!(single_line("line1\nline2"), "line1\\nline2");
        assert_eq!(single_line("a\r\nb"), "a\\r\\nb");
    }

    #[test]
    fn single_line_preserves_plain_text() {
        assert_eq!(single_line("中文没有换行"), "中文没有换行");
        assert_eq!(single_line(""), "");
    }

    // ── is_cjk 测试（0.21.23 下沉单一真源，承接原双源一致性测试的边界断言）──

    #[test]
    fn is_cjk_boundary_codepoints() {
        let inside = [
            0x3000u32, 0x33FF, 0x3400, 0x4DBF, 0x4E00, 0x9FFF, 0xAC00, 0xD7AF, 0xF900, 0xFAFF,
            0xFF00, 0xFFEF, 0x20000, 0x2A6DF,
        ];
        for cp in inside {
            assert!(is_cjk(char::from_u32(cp).unwrap()), "U+{cp:04X} 应为 CJK");
        }
        let outside = [
            0x2FFFu32, 0x4DC0, 0xA000, 0xABFF, 0xE000, 0xFB00, 0xFEFF, 0xFFF0, 0x10000, 0x1FFFF,
            0x2A6E0,
        ];
        for cp in outside {
            assert!(
                !is_cjk(char::from_u32(cp).unwrap()),
                "U+{cp:04X} 不应为 CJK"
            );
        }
    }

    #[test]
    fn is_cjk_common_scripts() {
        assert!(is_cjk('中'));
        assert!(is_cjk('あ')); // 平假名
        assert!(is_cjk('한')); // 韩文音节
        assert!(is_cjk('，')); // 全角逗号（FF0C）
        assert!(!is_cjk('A'));
        assert!(!is_cjk(' '));
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
