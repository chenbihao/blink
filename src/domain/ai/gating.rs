//! 未命中过滤决策树(§3.6 四筛子)——纯函数,决定"这个 query 该不该进 AI 路径"。
//!
//! **零副作用 / 零 IO / 零 async**——纯输入输出,便于全 case 单测覆盖。
//!
//! ## 为什么把决策树抽成独立纯函数
//!
//! 1. **验证成本低**:决策树是 AI 路径的第一道门,漏筛=白烧 token,错筛=用户以为坏了。
//!    纯函数天然可穷举 case,单测能钉死所有边界。
//! 2. **接入层薄**:`SearchService::exec_mixed` 只调一次 `should_invoke_ai`,
//!    不掺业务逻辑,后续 spike 调阈值只改本文件。
//! 3. **§3.6 阈值 spike 缓冲区**:0.9.2 用文档默认阈值(`min_query_len=4` 等),
//!    实际数据来后 spike 调优 —— 改 `AiGate::from` 或字段默认即可,签名不变。
//!
//! ## 决策树顺序
//!
//! ```text
//! 1. !enabled || !allow_intent_routing → Disabled     (总开关)
//! 2. respect_awareness_url_path && (is_url || is_file_path) → UrlOrPath
//! 3. len < min_query_len                              → TooShort
//! 4. require_whitespace && !contains(' ')             → NoWhitespace
//! 5. exclude_pure_numeric && all_ascii_digits         → PureNumeric
//! 6. otherwise                                        → Invoke
//! ```
//!
//! **注意**:rule_router / builtin / plugin_keyword 命中筛子**不在本函数**——
//! 那些依赖 `Route` 异步结果,由 SearchService 在接入层判(candidates 空才补 AI)。
//! 本函数只管**query 本身长啥样**这一维度。
//!
//! ## URL/路径判定的口径收窄
//!
//! 第 2 条用 **query 本身**判 URL/路径,不是 awareness 选区——
//! 选区里有 URL 是"翻译这段"场景常见组合,拦掉 AI 反而不合理;
//! 而 query 本身就是 URL 说明用户想"打开链接"(内置动作已命中),AI 不必再插一脚。

use crate::app::ai_config::AIConfig;
use crate::domain::context::probe;

/// 决策树输入——从 `AIConfig` 采出的最小闭包,不依赖 registry。
///
/// 抽这一层是为了让 `should_invoke_ai` 单测能构造任意组合的 gate 输入,
/// 不必造出完整的 `AIConfig`(带 providers/tiers 等无关字段)。
#[derive(Debug, Clone, Copy)]
pub struct AiGate {
    pub enabled: bool,
    pub allow_intent_routing: bool,
    pub min_query_len: u8,
    pub min_query_len_cjk: u8,
    pub require_whitespace: bool,
    pub exclude_pure_numeric: bool,
    pub respect_awareness_url_path: bool,
}

impl From<&AIConfig> for AiGate {
    fn from(cfg: &AIConfig) -> Self {
        Self {
            enabled: cfg.enabled,
            allow_intent_routing: cfg.allow_intent_routing,
            min_query_len: cfg.min_query_len,
            min_query_len_cjk: cfg.min_query_len_cjk,
            require_whitespace: cfg.require_whitespace,
            exclude_pure_numeric: cfg.exclude_pure_numeric,
            respect_awareness_url_path: cfg.respect_awareness_url_path,
        }
    }
}

/// 决策结果——`Fallback` 携带原因供 SLO 埋点(0.9.2 先只在 tracing::debug 打)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateOutcome {
    /// 通过所有筛子,可以调 AI。
    Invoke,
    /// 未通过,原因见 `FallbackReason`——回退常规 fuzzy。
    Fallback(FallbackReason),
}

/// 未通过筛子的原因——枚举而非字符串,方便 SLO 分组统计。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FallbackReason {
    /// 总开关关(enabled=false 或 allow_intent_routing=false)。
    Disabled,
    /// query 本身是 URL / 文件路径,交给内置动作处理。
    UrlOrPath,
    /// query 太短(字符数 < min_query_len,按 char 数不是 byte)。
    TooShort,
    /// require_whitespace=true 但 query 不含空格。
    NoWhitespace,
    /// exclude_pure_numeric=true 且 query 全是 ASCII 数字。
    PureNumeric,
}

/// 判断 query 是否该进 AI 路径。
///
/// **输入契约**:
/// - `q` **必须已 trim**——SearchService 传 `query.trim()`;本函数不再 trim。
/// - `gate` 由 `AIConfig` 生成。
pub fn should_invoke_ai(q: &str, gate: &AiGate) -> GateOutcome {
    // ① 总开关:两个都要 true 才继续(§5.3 严格 opt-in + intent_routing 二级开关)
    if !gate.enabled || !gate.allow_intent_routing {
        return GateOutcome::Fallback(FallbackReason::Disabled);
    }

    // ② URL/路径:交给"打开链接/打开路径"内置动作,不烧 AI
    if gate.respect_awareness_url_path && (probe::is_url(q) || probe::is_file_path(q)) {
        return GateOutcome::Fallback(FallbackReason::UrlOrPath);
    }

    // ③ 长度:按字符数(中文一字算一 char,不是 3 byte)
    // CJK 用独立阈值(默认 2)——"翻译"=2 char 是完整意图,不该被英文分词习惯的筛子误伤
    let min_len = if contains_cjk(q) {
        gate.min_query_len_cjk
    } else {
        gate.min_query_len
    };
    if q.chars().count() < min_len as usize {
        return GateOutcome::Fallback(FallbackReason::TooShort);
    }

    // ④ 必含空格:"打错一个字"(如 "fanyi")不该触发 AI,让 fuzzy 覆盖
    //    **CJK 豁免**:中日韩自然写作不需要空格分词——"你用的是什么模型"9 个字明显是完整意图,
    //    不能被英文分词习惯的筛子误伤。含至少一个 CJK 字符视为满足"结构化 query"条件。
    if gate.require_whitespace && !q.contains(' ') && !contains_cjk(q) {
        return GateOutcome::Fallback(FallbackReason::NoWhitespace);
    }

    // ⑤ 纯数字:计算器/端口号场景,不烧 AI
    if gate.exclude_pure_numeric && !q.is_empty() && q.chars().all(|c| c.is_ascii_digit()) {
        return GateOutcome::Fallback(FallbackReason::PureNumeric);
    }

    GateOutcome::Invoke
}

/// 是否含 CJK 字符(中日韩字符)——决定筛子 ④ 是否豁免"必含空格"。
///
/// **覆盖范围**:
/// - `U+4E00..U+9FFF` 基本汉字(覆盖 99% 现代中文)
/// - `U+3400..U+4DBF` CJK 扩展 A(生僻字,如"叒")
/// - `U+3040..U+309F` 日文平假名
/// - `U+30A0..U+30FF` 日文片假名
/// - `U+AC00..U+D7AF` 韩文谚文音节
///
/// **不覆盖**:CJK 扩展 B/C/D/E(需 surrogate pair,极罕用)、CJK 部首/兼容表意等。
/// 对 Blink 主流场景已足够——生僻扩展字触不触发 AI 差别可忽略。
///
/// **为什么不用 unicode-general-category crate**:那需要额外依赖 + 大小写表,
/// 对我们只是"过筛子"用途太重;简单 range 检查零成本 + 单测钉死行为。
fn contains_cjk(s: &str) -> bool {
    s.chars().any(|c| {
        matches!(c,
            '\u{4E00}'..='\u{9FFF}'   // CJK 基本汉字
            | '\u{3400}'..='\u{4DBF}' // CJK 扩展 A
            | '\u{3040}'..='\u{309F}' // 日文平假名
            | '\u{30A0}'..='\u{30FF}' // 日文片假名
            | '\u{AC00}'..='\u{D7AF}' // 韩文谚文音节
        )
    })
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 全放行 gate——所有筛子最松,便于单独测某一条。
    fn permissive_gate() -> AiGate {
        AiGate {
            enabled: true,
            allow_intent_routing: true,
            min_query_len: 1,
            min_query_len_cjk: 1,
            require_whitespace: false,
            exclude_pure_numeric: false,
            respect_awareness_url_path: false,
        }
    }

    /// 文档默认 gate——0.9.2 阶段的实际生产配置。
    fn default_gate() -> AiGate {
        AiGate {
            enabled: true,
            allow_intent_routing: true,
            min_query_len: 4,
            min_query_len_cjk: 2,
            require_whitespace: true,
            exclude_pure_numeric: true,
            respect_awareness_url_path: true,
        }
    }

    #[test]
    fn disabled_when_master_switch_off() {
        let mut g = default_gate();
        g.enabled = false;
        assert_eq!(
            should_invoke_ai("翻译 hello world", &g),
            GateOutcome::Fallback(FallbackReason::Disabled)
        );
    }

    #[test]
    fn disabled_when_intent_routing_off() {
        let mut g = default_gate();
        g.allow_intent_routing = false;
        assert_eq!(
            should_invoke_ai("翻译 hello world", &g),
            GateOutcome::Fallback(FallbackReason::Disabled)
        );
    }

    #[test]
    fn url_falls_back_when_respect_enabled() {
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("https://example.com/foo", &g),
            GateOutcome::Fallback(FallbackReason::UrlOrPath)
        );
    }

    #[test]
    fn file_path_falls_back_when_respect_enabled() {
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("C:\\Users\\foo\\bar.txt", &g),
            GateOutcome::Fallback(FallbackReason::UrlOrPath)
        );
    }

    #[test]
    fn url_passes_when_respect_disabled() {
        // 覆盖组合:respect_awareness_url_path=false 时 URL 该被后续筛子处理
        // (URL 通常无空格 → 会在 require_whitespace 筛子被拦,但不是 UrlOrPath 拦)
        let mut g = default_gate();
        g.respect_awareness_url_path = false;
        // 用一个有空格的 URL-like 串,让 URL 判定放行 URL 但空格筛子放行
        // https://a.b 本身无空格 → 应被空格筛子拦
        assert_eq!(
            should_invoke_ai("https://a.b/c", &g),
            GateOutcome::Fallback(FallbackReason::NoWhitespace)
        );
    }

    #[test]
    fn too_short_by_char_count_not_byte_count() {
        // 英文"ab"= 2 chars,min=4 时应判 TooShort
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("ab", &g),
            GateOutcome::Fallback(FallbackReason::TooShort)
        );
        // 4 char 英文含空格通过("ab cd")
        let mut g2 = default_gate();
        g2.min_query_len = 3;
        assert_eq!(should_invoke_ai("ab cd", &g2), GateOutcome::Invoke);
    }

    #[test]
    fn cjk_uses_shorter_threshold() {
        // "翻译"= 2 char CJK,min_query_len_cjk=2 → 通过(旧 min_query_len=4 时代会被拦)
        let g = default_gate();
        assert_eq!(should_invoke_ai("翻译", &g), GateOutcome::Invoke);
        // "翻"= 1 char CJK,min_query_len_cjk=2 → TooShort
        assert_eq!(
            should_invoke_ai("翻", &g),
            GateOutcome::Fallback(FallbackReason::TooShort)
        );
    }

    #[test]
    fn cjk_threshold_independent_of_english_threshold() {
        // 英文 min=4,CJK min=2——互不干扰
        let g = default_gate();
        // "ab"= 2 char 英文 → TooShort(英 文阈值 4)
        assert_eq!(
            should_invoke_ai("ab", &g),
            GateOutcome::Fallback(FallbackReason::TooShort)
        );
        // "翻译"= 2 char CJK → Invoke(CJK 阈值 2)
        assert_eq!(should_invoke_ai("翻译", &g), GateOutcome::Invoke);
    }

    #[test]
    fn no_whitespace_falls_back_when_required() {
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("fanyihello", &g),
            GateOutcome::Fallback(FallbackReason::NoWhitespace)
        );
    }

    #[test]
    fn cjk_query_exempt_from_whitespace_requirement() {
        // 铁则:中文自然写作不需要空格分词——"你用的是什么模型"应通过,不该被误伤
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("你用的是什么模型", &g),
            GateOutcome::Invoke
        );
        // 4 字中文也过(min_query_len=4)
        assert_eq!(should_invoke_ai("翻译这句话", &g), GateOutcome::Invoke);
    }

    #[test]
    fn cjk_single_char_still_hits_too_short() {
        // "翻"= 1 char CJK,min_query_len_cjk=2 → TooShort
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("翻", &g),
            GateOutcome::Fallback(FallbackReason::TooShort)
        );
    }

    #[test]
    fn japanese_and_korean_also_exempt() {
        let g = default_gate();
        // 日文含平假名/片假名
        assert_eq!(should_invoke_ai("これは日本語", &g), GateOutcome::Invoke);
        // 韩文谚文音节
        assert_eq!(should_invoke_ai("한국어테스트", &g), GateOutcome::Invoke);
    }

    #[test]
    fn ascii_only_still_needs_whitespace() {
        // ASCII 场景仍要求空格(不受 CJK 豁免影响)
        let g = default_gate();
        assert_eq!(
            should_invoke_ai("whatisrust", &g),
            GateOutcome::Fallback(FallbackReason::NoWhitespace)
        );
    }

    #[test]
    fn whitespace_not_required_passes_no_space_query() {
        let mut g = default_gate();
        g.require_whitespace = false;
        assert_eq!(should_invoke_ai("fanyihello", &g), GateOutcome::Invoke);
    }

    #[test]
    fn pure_ascii_digits_fall_back_when_excluded() {
        let mut g = default_gate();
        g.require_whitespace = false; // 只测数字筛子,让空格筛子先放行
        assert_eq!(
            should_invoke_ai("12345", &g),
            GateOutcome::Fallback(FallbackReason::PureNumeric)
        );
    }

    #[test]
    fn digit_plus_letter_not_pure_numeric() {
        // "1234a"5 char 含字母 → 通过纯数字筛子
        let mut g = default_gate();
        g.require_whitespace = false;
        assert_eq!(should_invoke_ai("1234a", &g), GateOutcome::Invoke);
    }

    #[test]
    fn normal_intent_query_invokes_ai() {
        // 生产场景:"翻译 hello world"5 char 含空格,非 URL/数字
        assert_eq!(
            should_invoke_ai("翻译 hello world", &default_gate()),
            GateOutcome::Invoke
        );
    }

    #[test]
    fn permissive_gate_lets_anything_through_except_disabled() {
        let g = permissive_gate();
        assert_eq!(should_invoke_ai("x", &g), GateOutcome::Invoke);
        assert_eq!(should_invoke_ai("123", &g), GateOutcome::Invoke);
        assert_eq!(should_invoke_ai("nowhite", &g), GateOutcome::Invoke);
    }

    #[test]
    fn empty_query_is_too_short_not_pure_numeric() {
        // 边界:空 query chars=0 < min 任何值 → TooShort(先于 PureNumeric)
        let mut g = default_gate();
        g.min_query_len = 1;
        assert_eq!(
            should_invoke_ai("", &g),
            GateOutcome::Fallback(FallbackReason::TooShort)
        );
    }

    #[test]
    fn ai_gate_derives_from_config_faithfully() {
        // AIConfig::default 是"全关",生成的 gate 决策必然 Disabled
        let cfg = AIConfig::default();
        let gate = AiGate::from(&cfg);
        assert!(!gate.enabled);
        assert_eq!(
            should_invoke_ai("翻译 hello world", &gate),
            GateOutcome::Fallback(FallbackReason::Disabled)
        );
    }

    #[test]
    fn decision_tree_order_disabled_beats_all() {
        // 即使命中 URL/太短/纯数字,Disabled 优先返回
        // (语义:关的时候不该暴露"为什么被拦",省得用户 debug 半天)
        let mut g = default_gate();
        g.enabled = false;
        assert_eq!(
            should_invoke_ai("https://a.b/c", &g),
            GateOutcome::Fallback(FallbackReason::Disabled)
        );
        assert_eq!(
            should_invoke_ai("ab", &g),
            GateOutcome::Fallback(FallbackReason::Disabled)
        );
    }
}
