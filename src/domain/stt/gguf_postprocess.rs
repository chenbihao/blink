//! GGUF worker 输出的文本后处理（0.22.7）。
//!
//! 旧 Python server 的 `_postprocess_text`（`resources/stt/funasr/blink_stt_server.py`）
//! 随 PyTorch 链路删除后，其语义迁入本模块——worker 的 C++ 入口只做
//! detokenize（SenseVoice 标签剥离由上游 C++ `--keep-tags=false` 默认处理），
//! emoji/事件描述/CJK 间空格清理由 Rust 传输层在收到文本后统一执行。
//!
//! 三步（与 Python 版语义一致）：
//! 1. 去除 emoji（SenseVoice 偶发插入）；
//! 2. 去除事件描述（如 "(大笑)(掌声)"）；
//! 3. 去除 CJK 字符间空格（Paraformer 字符级 tokenizer 副产物）。

/// emoji 与符号区段（与旧 Python `_EMOJI_PATTERN` 一致）。
fn is_emoji_char(c: char) -> bool {
    matches!(c,
        '\u{1F600}'..='\u{1F64F}'   // emoticons
        | '\u{1F300}'..='\u{1F5FF}' // symbols & pictographs
        | '\u{1F680}'..='\u{1F6FF}' // transport & map
        | '\u{1F1E0}'..='\u{1F1FF}' // flags
        | '\u{2700}'..='\u{27BF}'   // dingbats
        | '\u{1F900}'..='\u{1F9FF}' // supplemental symbols
        | '\u{2600}'..='\u{26FF}'   // misc symbols
        | '\u{1FA00}'..='\u{1FA6F}' // chess
        | '\u{1FA70}'..='\u{1FAFF}' // symbols extended-a
    )
}

/// SenseVoice 事件描述（括号形式）关键词（与旧 Python `_EVENT_DESC_PATTERN` 一致）。
const EVENT_DESC_KEYWORDS: &[&str] = &[
    "大笑",
    "小笑",
    "掌声",
    "音乐",
    "噪音",
    "哭泣",
    "叹气",
    "咳嗽",
    "呼吸",
    "背景音",
    "无声",
    "笑声",
    "哭声",
    "欢呼声",
    "尖叫声",
    "说话声",
    "敲击声",
    "响铃声",
    "爆竹声",
    "狗叫声",
    "猫叫声",
    "鸟叫声",
    "水声",
    "风声",
    "雷声",
    "引擎声",
    "键盘声",
    "电话铃声",
    "门铃声",
    "脚步声",
];

fn is_event_desc_token(inner: &str) -> bool {
    EVENT_DESC_KEYWORDS.contains(&inner)
}

/// 中日韩字符判定（CJK 统一表意/扩展A + 假名 + 谚文）。
fn is_cjk_char(c: char) -> bool {
    matches!(c,
        '\u{4e00}'..='\u{9fff}'
        | '\u{3400}'..='\u{4dbf}'
        | '\u{3040}'..='\u{30ff}'
        | '\u{ac00}'..='\u{d7af}')
}

/// GGUF worker 文本后处理：去 emoji → 去事件描述 → 去 CJK 间空格 → trim。
///
/// 纯函数，可独立测试。
pub fn gguf_postprocess(raw: &str) -> String {
    // 1. 去 emoji
    let no_emoji: String = raw.chars().filter(|c| !is_emoji_char(*c)).collect();

    // 2. 去事件描述（括号形式，半角/全角括号皆识别）
    let mut no_events = String::with_capacity(no_emoji.len());
    let mut chars = no_emoji.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '(' || c == '（' {
            // 收集到匹配的右括号（单层即可——事件描述不含嵌套）
            let mut inner = String::new();
            let mut closed = false;
            for inner_c in chars.by_ref() {
                if inner_c == ')' || inner_c == '）' {
                    closed = true;
                    break;
                }
                inner.push(inner_c);
            }
            if closed && is_event_desc_token(inner.trim()) {
                continue; // 丢弃整个事件描述
            }
            no_events.push(c);
            no_events.push_str(&inner);
            if closed {
                no_events.push(')');
            }
        } else {
            no_events.push(c);
        }
    }

    // 3. 去 CJK 字符间空格
    let chars_vec: Vec<char> = no_events.chars().collect();
    let mut cleaned = String::with_capacity(chars_vec.len());
    for (i, &c) in chars_vec.iter().enumerate() {
        if c.is_whitespace() {
            let prev_cjk = i > 0 && is_cjk_char(chars_vec[i - 1]);
            let next_cjk = i + 1 < chars_vec.len() && is_cjk_char(chars_vec[i + 1]);
            if prev_cjk && next_cjk {
                continue; // CJK 之间的空白删除
            }
        }
        cleaned.push(c);
    }

    cleaned.trim().to_string()
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_emoji() {
        assert_eq!(gguf_postprocess("你好😊世界"), "你好世界");
    }

    #[test]
    fn strips_event_descriptions() {
        assert_eq!(gguf_postprocess("今天天气不错(大笑)"), "今天天气不错");
        assert_eq!(gguf_postprocess("（掌声）谢谢大家"), "谢谢大家");
        assert_eq!(gguf_postprocess("开始(掌声)然后(大笑)结束"), "开始然后结束");
    }

    #[test]
    fn keeps_normal_parentheses() {
        // 非事件词的括号内容保留
        assert_eq!(
            gguf_postprocess("他说(悄悄话)然后走了"),
            "他说(悄悄话)然后走了"
        );
    }

    #[test]
    fn strips_cjk_inner_spaces() {
        // Paraformer 字符级 tokenizer 副产物
        assert_eq!(
            gguf_postprocess("那 我 现 在 能 输 入 了 吗"),
            "那我现在能输入了吗"
        );
        // CJK 与拉丁之间的空格保留
        assert_eq!(gguf_postprocess("你好 world 你好"), "你好 world 你好");
    }

    #[test]
    fn normal_text_unchanged() {
        assert_eq!(
            gguf_postprocess("Hello, this is a test."),
            "Hello, this is a test."
        );
        assert_eq!(gguf_postprocess("  你好世界。  "), "你好世界。");
    }

    #[test]
    fn empty_and_pure_emoji() {
        assert_eq!(gguf_postprocess(""), "");
        assert_eq!(gguf_postprocess("😄😂"), "");
    }

    #[test]
    fn combined_case() {
        assert_eq!(
            gguf_postprocess("我要 买 机 票(大笑)😄去北京"),
            "我要买机票去北京"
        );
    }
}
