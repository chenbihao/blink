//! 文本类型判定：纯字符串逻辑，无平台依赖，可单测。
//!
//! 0.8.0 §1.3：`BuiltinEngine::search` 与 `intent::RuleRouter` 共用同一套判定，避免两处实现。
//! 0.8.2 §3.3：加 `detect_lang` + `needs_translation`，用于翻译插件 Context 感知路由。
//!
//! **原则**：宁可保守（漏判）不可激进（错判）——错判会让"打开链接"配到不是 URL 的字符串上，
//! 出现异常候选反而伤害体验；漏判只是无收益，用户可以自己输入关键词兜底。

/// 是否为 URL。
///
/// 判定标准：常见 scheme + `://` 前缀 + **host 结构合理**。
///
/// **保守收敛**（0.8.0 §1.3 调整）：
/// - 只认 http/https/file/ftp/ftps 五种 scheme；不认 mailto:/tel:/data:/javascript: 等
///   （即使真是 URL，"用系统默认打开"也未必是用户期望）。
/// - `http`/`https`/`ftp`/`ftps` 后必须有 **host**——含 `.` 或恰为 `localhost` 才算——
///   排除 `http://` / `http://foo` 之类残缺串；避免"打开链接"配无效 URL 蒙混通过。
/// - `file://` 特殊放行（`file:///C:/foo` 无 `.`），只校验前缀后至少还有内容。
/// - 不做 RFC 3986 完整校验：那样代价大且反直觉，宁可宽一点让极边缘 URL 通过，
///   也不接受 `http://缺` 这种 host 明显不合法的串。
///
/// 注意：`s` 应为**已 trim** 的完整文本；剪贴板文本会先在采集层 trim。
#[allow(dead_code)] // Task 7 双路匹配 + Task 8 参数化 Action 引入后被读
pub fn is_url(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 2048 {
        // 2048 是常见浏览器 URL 上限；超出多半是误粘代码/日志
        return false;
    }
    // 内部不允许有换行/空白——多行文本不是单个 URL
    if s.chars().any(|c| c.is_whitespace()) {
        return false;
    }
    let lower = s.to_ascii_lowercase();

    // file:// 单独处理——本地文件 URL 通常无 host、无 `.`
    if let Some(rest) = lower.strip_prefix("file://") {
        return !rest.is_empty();
    }

    // 其余 scheme：抽出 authority 段（host[:port][/path...]）做启发校验
    let rest = if let Some(r) = lower.strip_prefix("https://") {
        r
    } else if let Some(r) = lower.strip_prefix("http://") {
        r
    } else if let Some(r) = lower.strip_prefix("ftps://") {
        r
    } else if let Some(r) = lower.strip_prefix("ftp://") {
        r
    } else {
        return false;
    };

    // host 段 = `://` 后到第一个 `/`、`?`、`#` 之前
    let host = rest
        .split(|c| c == '/' || c == '?' || c == '#')
        .next()
        .unwrap_or("");

    if host.is_empty() {
        return false; // http:// 光溜溜
    }
    // 剥离用户信息 `user:pass@`
    let host_only = host.rsplit('@').next().unwrap_or("");
    if host_only.is_empty() {
        return false;
    }
    // IPv6 字面量：`[::1]` 或 `[::1]:port`——里面的 `:` 不是端口分隔符，先在这里放行
    if host_only.starts_with('[') {
        // 找匹配的 `]`；简单校验：`]` 存在且之后要么结尾要么是 `:port`
        if let Some(close_idx) = host_only.find(']') {
            let after_close = &host_only[close_idx + 1..];
            return close_idx > 1 && (after_close.is_empty() || after_close.starts_with(':'));
        }
        return false;
    }
    // 剥离端口 `:port`（IPv4/domain）
    let host_no_port = host_only
        .rsplit_once(':')
        .map(|(h, _)| h)
        .unwrap_or(host_only);
    if host_no_port.is_empty() {
        return false;
    }
    // 允许：localhost / 含点的域名
    if host_no_port == "localhost" {
        return true;
    }
    // 至少要有一个点，且点前后都有字符（排除 `.` `foo.` `.bar`）
    let dot_idx = match host_no_port.find('.') {
        Some(i) => i,
        None => return false,
    };
    dot_idx > 0 && dot_idx < host_no_port.len() - 1
}

/// 是否为 Windows 文件路径。
///
/// 认可格式：
/// - 盘符绝对路径：`C:\foo\bar` / `D:/foo`（正反斜杠都认）
/// - UNC 路径：`\\server\share\...`
/// - 长路径前缀：`\\?\C:\...`
///
/// **不认**：相对路径（`.\foo`、`foo\bar.txt`）—— 无法确定基路径，前端"打开目录"会歧义。
/// **不检查文件是否存在**：这是纯字符串判定，让调用方决定要不要 fs::exists 校验。
#[allow(dead_code)] // Task 7 双路匹配 + Task 8 参数化 Action 引入后被读
pub fn is_file_path(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 32767 {
        // Windows MAX_PATH 长路径上限
        return false;
    }
    // 多行不是单个路径
    if s.contains('\n') || s.contains('\r') {
        return false;
    }
    let bytes = s.as_bytes();

    // UNC / 长路径前缀：以 \\ 开头（\\server\share 或 \\?\...）
    if bytes.len() >= 3
        && (bytes[0] == b'\\' || bytes[0] == b'/')
        && (bytes[1] == b'\\' || bytes[1] == b'/')
    {
        // 至少要有 \\x\y 结构，x 非空
        // 简化：只要 \\ 后跟非分隔符字符就通过
        return bytes[2] != b'\\' && bytes[2] != b'/';
    }

    // 盘符绝对路径：X:\ 或 X:/（X 为字母）
    if bytes.len() >= 3
        && bytes[0].is_ascii_alphabetic()
        && bytes[1] == b':'
        && (bytes[2] == b'\\' || bytes[2] == b'/')
    {
        return true;
    }

    false
}

// ── 语言检测与"值得翻译"判定（0.8.2 §3.3）───────────────────────────────────

// ── 0.20.0 结构化文本负向过滤 ──────────────────────────────────────────────
//
// 在 needs_translation 前加入分类型 is_non_prose_structured_text：
// 颜色、UUID、hash、版本、IP/端口、邮箱、CSS 声明/变量、短 JSON、命令参数、纯代码标识符。
// 每类为独立纯函数，禁止总括式"像代码"正则。
// 显式"翻译 …"前缀不受抑制——用户明确要求翻译时永远放行。

/// 判断文本是否为颜色字面量（0.20.0）。
///
/// 认可格式：`#RGB`、`#RGBA`、`#RRGGBB`、`#RRGGBBAA`、`rgb()/rgba()`、`hsl()/hsla()`。
fn is_color_literal(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    // HEX 颜色：# 后 3/4/6/8 位十六进制
    if let Some(rest) = s.strip_prefix('#') {
        return matches!(rest.len(), 3 | 4 | 6 | 8) && rest.chars().all(|c| c.is_ascii_hexdigit());
    }
    let lower = s.to_ascii_lowercase();
    // rgb()/rgba()
    if let Some(rest) = lower.strip_prefix("rgb(") {
        return rest.ends_with(')') && rest.trim_end_matches(')').split(',').count() >= 3;
    }
    if let Some(rest) = lower.strip_prefix("rgba(") {
        return rest.ends_with(')') && rest.trim_end_matches(')').split(',').count() == 4;
    }
    // hsl()/hsla()
    if let Some(rest) = lower.strip_prefix("hsl(") {
        return rest.ends_with(')') && rest.trim_end_matches(')').split(',').count() >= 3;
    }
    if let Some(rest) = lower.strip_prefix("hsla(") {
        return rest.ends_with(')') && rest.trim_end_matches(')').split(',').count() == 4;
    }
    false
}

/// 判断文本是否为 UUID（0.20.0）。
fn is_uuid(s: &str) -> bool {
    let s = s.trim();
    // 标准 UUID: 8-4-4-4-12 = 36 字符（含连字符）
    if s.len() == 36 {
        return s.chars().enumerate().all(|(i, c)| {
            if i == 8 || i == 13 || i == 18 || i == 23 {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        });
    }
    // 无连字符的 UUID: 32 字符纯十六进制
    if s.len() == 32 {
        return s.chars().all(|c| c.is_ascii_hexdigit());
    }
    false
}

/// 判断文本是否为 hash 值（0.20.0）。
///
/// 认可：MD5(32)、SHA1(40)、SHA256(64)、SHA512(128) 纯十六进制。
fn is_hash(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // 常见 hash 长度前缀（git short hash 等）+ 完整长度
    let valid_lengths = [7, 8, 32, 40, 64, 128];
    valid_lengths.contains(&s.len()) && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 判断文本是否为版本号（0.20.0）。
///
/// 认可：`1.0`、`v1.2.3`、`2.0.0-beta`、`1.2.3-rc.1` 等。
fn is_version_string(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    // 去除可选的 v/V 前缀
    let s = s.strip_prefix(['v', 'V']).unwrap_or(s);
    // 必须以数字开头
    if !s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    // 核心版本号：数字.数字.数字...（至少两个数字段）
    let core = s
        .split(|c: char| !c.is_ascii_digit() && c != '.')
        .next()
        .unwrap_or("");
    let dot_count = core.matches('.').count();
    if dot_count < 1 {
        return false;
    }
    // 每个点分隔段必须是数字
    core.split('.')
        .all(|seg| !seg.is_empty() && seg.chars().all(|c| c.is_ascii_digit()))
}

/// 判断文本是否为 IP 地址 / IP:端口（0.20.0）。
fn is_ip_or_port(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 64 {
        return false;
    }
    // IPv4:port 或 IPv4（含 '.' 才可能是 IPv4；纯数字走下方端口分支）
    if s.contains('.') {
        let host = s.rsplit_once(':').map(|(h, _)| h).unwrap_or(s);
        let port = s.rsplit_once(':').map(|(_, p)| p);
        // IPv4 校验
        let parts: Vec<&str> = host.split('.').collect();
        if parts.len() == 4
            && parts.iter().all(|p| {
                !p.is_empty()
                    && p.len() <= 3
                    && p.chars().all(|c| c.is_ascii_digit())
                    && p.parse::<u8>().is_ok()
            })
        {
            // 如果有端口，校验端口 1-65535
            if let Some(p) = port {
                return !p.is_empty()
                    && p.len() <= 5
                    && p.chars().all(|c| c.is_ascii_digit())
                    && p.parse::<u32>().is_ok_and(|v| v <= 65535);
            }
            return true;
        }
    }
    // IPv6 简单检测：含多个冒号且只含十六进制和冒号
    if s.matches(':').count() >= 2
        && s.chars()
            .all(|c| c.is_ascii_hexdigit() || c == ':' || c == '.')
    {
        return true;
    }
    false
}

/// 判断文本是否为邮箱地址（0.20.0）。
fn is_email(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 320 {
        return false;
    }
    // 简单校验：local@domain，local 和 domain 非空，domain 含点
    let at_pos = match s.find('@') {
        Some(i) if i > 0 => i,
        _ => return false,
    };
    let domain = &s[at_pos + 1..];
    if domain.is_empty() || !domain.contains('.') {
        return false;
    }
    // 不含空格
    !s.chars().any(|c| c.is_whitespace())
}

/// 判断文本是否为 CSS 声明或变量（0.20.0）。
///
/// 认可：`--var: value;`、`property: value;`、`--var-name`。
fn is_css_declaration(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    // CSS 变量定义：--var: value;
    if s.starts_with("--") && s.contains(':') {
        return true;
    }
    // CSS 声明：property: value; （含冒号和分号，无空格开头）
    if s.contains(':') && s.contains(';') && !s.starts_with("http") {
        // 排除 URL（http: 已被排除）
        let before_colon = s.split(':').next().unwrap_or("");
        // CSS 属性名只含字母和连字符
        return !before_colon.is_empty()
            && before_colon
                .chars()
                .all(|c| c.is_ascii_alphabetic() || c == '-');
    }
    // 纯 CSS 变量引用：--var-name
    if s.starts_with("--") && s.len() > 4 && !s.contains(' ') {
        return s[2..]
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-');
    }
    false
}

/// 判断文本是否为短 JSON（0.20.0）。
fn is_short_json(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 1024 {
        return false;
    }
    // 必须以 { 或 [ 开头
    if !s.starts_with('{') && !s.starts_with('[') {
        return false;
    }
    // 必须以 } 或 ] 结尾（对应）
    let close = if s.starts_with('{') { '}' } else { ']' };
    if !s.ends_with(close) {
        return false;
    }
    // 粗略校验：引号数量为偶数
    let quote_count = s.matches('"').count();
    quote_count % 2 == 0
}

/// 判断文本是否为命令参数（0.20.0）。
///
/// 认可：`--flag`、`-f value`、`--opt=value`、`npm install -g` 等。
fn is_command_args(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 512 {
        return false;
    }
    // 必须包含至少一个 -- 或 - 开头的 flag
    let words: Vec<&str> = s.split_whitespace().collect();
    if words.is_empty() {
        return false;
    }
    // 至少一个 word 以 -- 或 - 开头（但不是单个 - ）
    let has_flag = words.iter().any(|w| {
        (w.starts_with("--") && w.len() > 2)
            || (w.starts_with('-') && w.len() > 1 && !w.starts_with("--"))
    });
    if !has_flag {
        return false;
    }
    // 所有 word 不含自然语言常见模式（如连续的 a/an/the + 空格 + 单词）
    // 简单策略：如果文本中非 flag 的 word 超过 5 个自然语言词，则不算命令参数
    let non_flag_words = words.iter().filter(|w| !w.starts_with('-')).count();
    non_flag_words <= 10
}

/// 判断文本是否为纯代码标识符（0.20.0）。
///
/// 认可：`camelCase`、`snake_case`、`PascalCase`、`SCREAMING_SNAKE`、`module.function` 等。
fn is_code_identifier(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() || s.len() > 128 {
        return false;
    }
    // 不能含空格（代码标识符无空格）
    if s.contains(' ') {
        return false;
    }
    // 不能含自然语言标点（句号、逗号、问号、感叹号）
    if s.chars().any(|c| matches!(c, '.' | ',' | '?' | '!')) {
        // 但允许 . 用于 module.function 形式
        // 如果含 . 以外的自然语言标点 → 不是纯标识符
        if s.chars().any(|c| matches!(c, ',' | '?' | '!')) {
            return false;
        }
    }
    // 所有字符必须是字母、数字、下划线、点、$、#（类名前缀）
    if !s
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.' || c == '$' || c == '#')
    {
        return false;
    }
    // 不能以数字开头（纯数字已被 Empty 分类挡住）
    if s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        return false;
    }
    // 至少含一个下划线或点或大小写混合（区分于普通英文单词）
    let has_underscore = s.contains('_');
    let has_dot = s.contains('.');
    let has_upper = s.chars().any(|c| c.is_ascii_uppercase());
    let has_lower = s.chars().any(|c| c.is_ascii_lowercase());
    let has_mixed_case = has_upper && has_lower;
    // 纯小写字母+数字且无下划线/点 → 普通英文词，不算代码标识符
    // 除非长度较短且有数字（如 `var2`）
    let has_digit = s.chars().any(|c| c.is_ascii_digit());
    has_underscore || has_dot || has_mixed_case || (has_digit && s.len() <= 8)
}

/// 判断文本是否为结构化非自然语言文本（0.20.0）。
///
/// 分类型检测：颜色、UUID、hash、版本、IP/端口、邮箱、CSS 声明/变量、
/// 短 JSON、命令参数、纯代码标识符。
///
/// **显式"翻译 …"不受抑制**：如果文本以"翻译"或"translate"开头（不区分大小写），
/// 直接返回 false——用户明确要求翻译时永远放行。
pub fn is_non_prose_structured_text(s: &str) -> bool {
    let s = s.trim();
    if s.is_empty() {
        return false;
    }
    // 显式"翻译 …"前缀不受抑制
    let lower = s.to_ascii_lowercase();
    if lower.starts_with("翻译")
        || lower.starts_with("translate ")
        || lower.starts_with("translate\t")
    {
        return false;
    }
    // 分类型检测（短路 OR）
    is_color_literal(s)
        || is_uuid(s)
        || is_hash(s)
        || is_version_string(s)
        || is_ip_or_port(s)
        || is_email(s)
        || is_css_declaration(s)
        || is_short_json(s)
        || is_command_args(s)
        || is_code_identifier(s)
}

/// 文本主要字符集。用最简单的字符集分档覆盖 95%+ 场景，不引 `whatlang` / `lingua`
/// （多 200KB+ 依赖，0.8.x 阶段不划算）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextLang {
    /// 中日韩汉字（简繁）
    Cjk,
    /// 拉丁字母（英、法、德、西……本层不细分）
    Latin,
    /// 日文平假名/片假名
    Kana,
    /// 韩文谚文
    Hangul,
    /// 多字符集混合（例如"hello 世界"）
    Mixed,
    /// 无可分类字符（纯数字 / 纯标点 / 空串）
    Empty,
}

/// 判定文本的主要字符集。
///
/// 判定策略：统计 4 类"可分类字符"（cjk / latin / kana / hangul）的占比，
/// 空白与标点/数字/符号不计入。
/// - 全部可分类字符都是同一族 → 对应 TextLang
/// - 出现两族以上 → `Mixed`
/// - 无任何可分类字符 → `Empty`
pub fn detect_lang(s: &str) -> TextLang {
    let mut cjk = 0usize;
    let mut latin = 0usize;
    let mut kana = 0usize;
    let mut hangul = 0usize;
    for c in s.chars() {
        if is_cjk(c) {
            cjk += 1;
        } else if is_latin_letter(c) {
            latin += 1;
        } else if is_kana(c) {
            kana += 1;
        } else if is_hangul(c) {
            hangul += 1;
        }
        // 数字/标点/空白/emoji 等不计入
    }
    let mut families = 0;
    if cjk > 0 {
        families += 1;
    }
    if latin > 0 {
        families += 1;
    }
    if kana > 0 {
        families += 1;
    }
    if hangul > 0 {
        families += 1;
    }
    match families {
        0 => TextLang::Empty,
        1 => {
            if cjk > 0 {
                TextLang::Cjk
            } else if latin > 0 {
                TextLang::Latin
            } else if kana > 0 {
                TextLang::Kana
            } else {
                TextLang::Hangul
            }
        }
        _ => {
            // 特例：CJK + Kana 视为**日文**（日文正文本来就 CJK 汉字 + 假名混排），
            // 避免"日本語"这类日文短句被误判为 Mixed 后放弃翻译。
            if kana > 0 && hangul == 0 && latin == 0 {
                TextLang::Kana
            } else {
                TextLang::Mixed
            }
        }
    }
}

/// 判断该文本相对于用户目标语言"值得翻译"。
///
/// **内建护栏**（0.8.2 §3.3 与 P0-2 决策）：任一命中直接返回 `false`
/// 1. `is_url(s)` → false （复制的 URL 不该被翻译；与 0.8.0「打开链接」内置动作互斥）
/// 2. `is_file_path(s)` → false （Windows 路径同理）
/// 3. `trim().chars().count() < 3` → false （极短文本无价值，且 IME 中间态可能命中）
/// 4. 无任何可翻译字符（`detect_lang` 返回 `Empty`）→ false （纯数字 / 纯符号 / 纯标点）
/// 5. `target == "auto"` → false （调用方应先解析 `auto` 为具体语言再传；此处兜底保守）
/// 6. **多行文本首行是 URL / 文件路径** → false（review #12 修复：
///    `is_url("https://a.com\nhttps://b.com")` 因内含 whitespace 恒 false，但纯 Latin 会
///    让 `detect_lang = Latin` 误触发翻译。取首行再判 URL/路径把这类场景挡住）
///
/// 通过护栏后：按字符集分档，检测语言与目标语言不同族 → true。
/// `Mixed` 视为已含目标语言，不再触发翻译。
///
/// **target 由调用方传入**（`RuleRouter` 通过 `PluginSettingResolver` 读插件 `target_lang`，
/// `auto` 回退 `AppConfig.language`）；本函数只是纯计算，不查任何 config。
pub fn needs_translation(s: &str, target: &str) -> bool {
    let s = s.trim();
    // 护栏 3：短文本
    if s.chars().count() < 3 {
        return false;
    }
    // 护栏 1/2：URL / 文件路径
    if is_url(s) || is_file_path(s) {
        return false;
    }
    // 护栏 6：多行 URL / 路径列表（首行判定）——多行文本 is_url 因 whitespace 恒 false，
    // 首行是链接的场景（"复制多个链接"）不该被翻译。
    if s.contains('\n') || s.contains('\r') {
        let first = s
            .split(|c| c == '\n' || c == '\r')
            .next()
            .unwrap_or("")
            .trim();
        if is_url(first) || is_file_path(first) {
            return false;
        }
    }
    // 0.20.0 护栏 7：结构化非自然语言文本负向过滤
    // 颜色、UUID、hash、版本、IP/端口、邮箱、CSS 声明/变量、短 JSON、命令参数、纯代码标识符。
    // 显式"翻译 …"前缀不受抑制（is_non_prose_structured_text 内部已处理）。
    if is_non_prose_structured_text(s) {
        return false;
    }
    // 护栏 5：auto 兜底
    if target == "auto" || target.is_empty() {
        return false;
    }
    // 护栏 4：无可翻译字符
    let lang = detect_lang(s);
    if lang == TextLang::Empty {
        return false;
    }
    // 0.20.0：显式"翻译 …"/"translate …"前缀绕过 Mixed 抑制——
    // 用户明确要求翻译时，即使 Mixed 也应放行。
    let lower = s.to_ascii_lowercase();
    let explicit_translate = lower.starts_with("翻译")
        || lower.starts_with("translate ")
        || lower.starts_with("translate\t");
    if explicit_translate {
        return lang != TextLang::Empty;
    }
    // 分档
    match (target, lang) {
        ("zh", TextLang::Latin | TextLang::Kana | TextLang::Hangul) => true,
        ("en", TextLang::Cjk | TextLang::Kana | TextLang::Hangul) => true,
        ("ja", TextLang::Cjk | TextLang::Latin | TextLang::Hangul) => true,
        ("ko", TextLang::Cjk | TextLang::Latin | TextLang::Kana) => true,
        // 同族 / Mixed / 未知 target → 不触发
        _ => false,
    }
}

fn is_cjk(c: char) -> bool {
    matches!(c,
        '\u{4E00}'..='\u{9FFF}'       // CJK Unified Ideographs
        | '\u{3400}'..='\u{4DBF}'     // CJK Ext A
        | '\u{20000}'..='\u{2A6DF}'   // CJK Ext B
        | '\u{F900}'..='\u{FAFF}'     // CJK Compatibility Ideographs
    )
}

fn is_latin_letter(c: char) -> bool {
    matches!(c,
        'a'..='z' | 'A'..='Z'
        | '\u{00C0}'..='\u{024F}'     // Latin Extended-A/B/Supplement（覆盖法/德/西/北欧）
    )
}

fn is_kana(c: char) -> bool {
    matches!(c,
        '\u{3040}'..='\u{309F}'       // Hiragana
        | '\u{30A0}'..='\u{30FF}'     // Katakana
        | '\u{31F0}'..='\u{31FF}'     // Katakana Ext
    )
}

fn is_hangul(c: char) -> bool {
    matches!(c,
        '\u{AC00}'..='\u{D7AF}'       // Hangul Syllables
        | '\u{1100}'..='\u{11FF}'     // Hangul Jamo
        | '\u{3130}'..='\u{318F}'     // Hangul Compatibility Jamo
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- is_url ----

    #[test]
    fn url_http_https() {
        assert!(is_url("http://example.com"));
        assert!(is_url("https://example.com/path?q=1"));
        assert!(is_url("HTTPS://EXAMPLE.COM")); // 大小写不敏感
    }

    #[test]
    fn url_file_and_ftp() {
        assert!(is_url("file:///C:/foo"));
        assert!(is_url("ftp://ftp.example.com"));
        assert!(is_url("ftps://ftp.example.com"));
    }

    #[test]
    fn url_trims_whitespace() {
        assert!(is_url("  https://example.com  "));
    }

    #[test]
    fn url_rejects_non_url() {
        assert!(!is_url(""));
        assert!(!is_url("example.com")); // 无 scheme
        assert!(!is_url("www.example.com"));
        assert!(!is_url("//example.com"));
        assert!(!is_url("mailto:x@y.com")); // 保守：不认 mailto
        assert!(!is_url("javascript:alert(1)"));
        assert!(!is_url("just some text"));
    }

    #[test]
    fn url_rejects_incomplete_scheme() {
        // 0.8.0 §1.3 收紧：host 结构不合理 → 拒绝
        assert!(!is_url("http://"));
        assert!(!is_url("https://"));
        assert!(!is_url("ftp://"));
        assert!(!is_url("http://缺")); // 无 TLD 的中文单字
        assert!(!is_url("http://foo")); // 无 TLD 的短标识
        assert!(!is_url("http://.com")); // 点前无字符
        assert!(!is_url("http://foo.")); // 点后无字符
    }

    #[test]
    fn url_accepts_localhost_and_ipv6() {
        // 特例：localhost 不需要点
        assert!(is_url("http://localhost"));
        assert!(is_url("http://localhost:8080"));
        assert!(is_url("http://localhost:8080/path"));
        // IPv6 字面量：粗略认可
        assert!(is_url("http://[::1]"));
        assert!(is_url("http://[::1]:8080/path"));
    }

    #[test]
    fn url_accepts_userinfo_and_port() {
        assert!(is_url("https://user:pass@example.com"));
        assert!(is_url("https://user@example.com:8080/path"));
    }

    #[test]
    fn url_file_scheme_wildly_allowed() {
        // file:// 只校验前缀后有内容——本地路径可能没有点或 host
        assert!(is_url("file:///C:/foo"));
        assert!(is_url("file://server/share"));
        assert!(!is_url("file://")); // 完全空还是拒绝
    }

    #[test]
    fn url_rejects_multiline() {
        assert!(!is_url("https://a.com\nhttps://b.com"));
        assert!(!is_url("https://a.com b.com"));
    }

    #[test]
    fn url_rejects_too_long() {
        let long = "https://".to_string() + &"a".repeat(3000);
        assert!(!is_url(&long));
    }

    // ---- is_file_path ----

    #[test]
    fn file_path_drive_absolute() {
        assert!(is_file_path("C:\\foo\\bar"));
        assert!(is_file_path("D:/foo/bar.txt"));
        assert!(is_file_path("C:\\"));
        assert!(is_file_path("z:\\a")); // 小写盘符也认
    }

    #[test]
    fn file_path_unc() {
        assert!(is_file_path("\\\\server\\share"));
        assert!(is_file_path("\\\\server\\share\\file.txt"));
        assert!(is_file_path("//server/share")); // 正斜杠 UNC 也认
    }

    #[test]
    fn file_path_long_prefix() {
        assert!(is_file_path("\\\\?\\C:\\very\\long\\path"));
    }

    #[test]
    fn file_path_trims_whitespace() {
        assert!(is_file_path("  C:\\foo  "));
    }

    #[test]
    fn file_path_rejects_relative() {
        assert!(!is_file_path("foo\\bar"));
        assert!(!is_file_path(".\\foo"));
        assert!(!is_file_path("..\\foo"));
        assert!(!is_file_path("foo.txt"));
    }

    #[test]
    fn file_path_rejects_non_path() {
        assert!(!is_file_path(""));
        assert!(!is_file_path("just text"));
        assert!(!is_file_path("http://example.com")); // URL 不是文件路径
        assert!(!is_file_path("C")); // 缺 : 和分隔符
        assert!(!is_file_path("C:")); // 缺分隔符
    }

    #[test]
    fn file_path_rejects_multiline() {
        assert!(!is_file_path("C:\\a\nC:\\b"));
        assert!(!is_file_path("C:\\a\r\nC:\\b"));
    }

    #[test]
    fn file_path_rejects_bare_double_slash() {
        // \\\ 三个反斜杠：不算合法 UNC (server 名为空)
        assert!(!is_file_path("\\\\\\foo"));
    }

    // ---- detect_lang ----

    #[test]
    fn detect_lang_empty_and_whitespace() {
        assert_eq!(detect_lang(""), TextLang::Empty);
        assert_eq!(detect_lang("   "), TextLang::Empty);
        assert_eq!(detect_lang("!@#$%"), TextLang::Empty);
        assert_eq!(detect_lang("12345"), TextLang::Empty);
    }

    #[test]
    fn detect_lang_pure_latin() {
        assert_eq!(detect_lang("hello world"), TextLang::Latin);
        assert_eq!(detect_lang("The quick brown fox"), TextLang::Latin);
        // 带标点/数字/空白也仍视为 Latin(标点不参与家族计数)
        assert_eq!(detect_lang("Hello, World! (2024)"), TextLang::Latin);
        // 拉丁扩展(法/德/西)
        assert_eq!(detect_lang("café"), TextLang::Latin);
        assert_eq!(detect_lang("naïve für"), TextLang::Latin);
    }

    #[test]
    fn detect_lang_pure_cjk() {
        assert_eq!(detect_lang("你好世界"), TextLang::Cjk);
        assert_eq!(detect_lang("中文测试。"), TextLang::Cjk);
    }

    #[test]
    fn detect_lang_pure_kana() {
        // 平假名
        assert_eq!(detect_lang("こんにちは"), TextLang::Kana);
        // 片假名
        assert_eq!(detect_lang("カタカナ"), TextLang::Kana);
    }

    #[test]
    fn detect_lang_cjk_plus_kana_is_kana() {
        // 日文正文本来就是汉字 + 假名混排：不该判 Mixed
        // 但纯 CJK 汉字词（如 "日本語"）应判 Cjk，见 detect_lang_pure_cjk
        assert_eq!(detect_lang("私は学生です"), TextLang::Kana);
        assert_eq!(detect_lang("東京タワー"), TextLang::Kana);
        assert_eq!(detect_lang("これは日本語のテスト"), TextLang::Kana);
    }

    #[test]
    fn detect_lang_pure_hangul() {
        assert_eq!(detect_lang("안녕하세요"), TextLang::Hangul);
        assert_eq!(detect_lang("한국어 테스트"), TextLang::Hangul);
    }

    #[test]
    fn detect_lang_mixed() {
        // 中英混排 → Mixed
        assert_eq!(detect_lang("hello 世界"), TextLang::Mixed);
        // 中韩 → Mixed
        assert_eq!(detect_lang("한국 中国"), TextLang::Mixed);
        // Kana + Latin → Mixed（review #11：Kana+CJK 是日文正文，此处才是真正的中英混排式 Mixed）
        assert_eq!(detect_lang("ハローHello"), TextLang::Mixed);
        // CJK+Kana + Latin → Mixed（三族并存，非纯日文）
        assert_eq!(detect_lang("これは Hello です"), TextLang::Mixed);
    }

    #[test]
    fn detect_lang_emoji_only_is_empty() {
        assert_eq!(detect_lang("🎉🚀✨"), TextLang::Empty);
    }

    // ---- needs_translation ----

    #[test]
    fn needs_translation_basic_latin_to_zh() {
        // 英文 + target=zh → 需要翻译
        assert!(needs_translation("hello world", "zh"));
        assert!(needs_translation("The quick brown fox jumps", "zh"));
    }

    #[test]
    fn needs_translation_same_family_no_trigger() {
        // 中文 + target=zh → 不需要
        assert!(!needs_translation("你好世界", "zh"));
        // 英文 + target=en → 不需要
        assert!(!needs_translation("hello world", "en"));
    }

    #[test]
    fn needs_translation_cjk_to_en() {
        assert!(needs_translation("你好世界", "en"));
    }

    #[test]
    fn needs_translation_japanese_to_zh() {
        // 日文（汉字+假名）→ target=zh 需要翻译（Kana 分类不等于 Cjk）
        assert!(needs_translation("日本語のテスト", "zh"));
    }

    #[test]
    fn needs_translation_korean_to_zh() {
        assert!(needs_translation("안녕하세요", "zh"));
    }

    #[test]
    fn needs_translation_target_ja_ko() {
        // target=ja：英文触发
        assert!(needs_translation("hello world", "ja"));
        // target=ja：中文触发（不同族）
        assert!(needs_translation("你好世界", "ja"));
        // target=ja：日文本身不触发（Kana 视为 ja）
        assert!(!needs_translation("こんにちは世界", "ja"));
        // target=ko：韩文不触发
        assert!(!needs_translation("안녕하세요 반가워요", "ko"));
        // target=ko：英文触发
        assert!(needs_translation("hello world", "ko"));
    }

    // ---- needs_translation 护栏：URL / 文件路径 ----

    #[test]
    fn needs_translation_url_guard() {
        // 复制 URL 不该触发翻译（P0-2 关键回归：与内置动作「打开链接」互斥）
        assert!(!needs_translation(
            "https://github.com/anthropics/xxx",
            "zh"
        ));
        assert!(!needs_translation("http://example.com", "zh"));
        assert!(!needs_translation(
            "https://user:pass@example.com/path?q=1",
            "zh"
        ));
        assert!(!needs_translation("file:///C:/foo", "zh"));
    }

    #[test]
    fn needs_translation_file_path_guard() {
        assert!(!needs_translation(r"C:\Users\a\file.txt", "zh"));
        assert!(!needs_translation("D:/foo/bar.py", "zh"));
        assert!(!needs_translation(r"\\server\share\file", "zh"));
    }

    // ---- needs_translation 护栏：短文本 ----

    #[test]
    fn needs_translation_short_text_guard() {
        // <3 字符 → 不触发（IME 中间态保护）
        assert!(!needs_translation("hi", "zh"));
        assert!(!needs_translation("a", "zh"));
        assert!(!needs_translation("", "zh"));
        assert!(!needs_translation("  ", "zh"));
        // 3 字符边界：正好通过
        assert!(needs_translation("the", "zh"));
        assert!(needs_translation("你好啊", "en"));
    }

    // ---- needs_translation 护栏：纯符号/数字 ----

    #[test]
    fn needs_translation_no_translatable_char_guard() {
        // Empty 分类 → 不触发
        assert!(!needs_translation("12345", "zh"));
        assert!(!needs_translation("!@#$%^", "zh"));
        assert!(!needs_translation("123 456", "zh"));
        assert!(!needs_translation("🎉🚀✨", "zh"));
    }

    // ---- needs_translation 护栏：auto / 空 target ----

    #[test]
    fn needs_translation_auto_target_returns_false() {
        // auto 由调用方替换后再传；此处兜底 false
        assert!(!needs_translation("hello world", "auto"));
        assert!(!needs_translation("你好世界", "auto"));
        // 空 target 也保守
        assert!(!needs_translation("hello world", ""));
    }

    #[test]
    fn needs_translation_unknown_target_returns_false() {
        // 未支持的 target 保守 false（不激进触发翻译）
        assert!(!needs_translation("hello world", "de"));
        assert!(!needs_translation("hello world", "fr"));
    }

    // ---- needs_translation 护栏：Mixed ----

    #[test]
    fn needs_translation_mixed_is_false() {
        // "hello 世界" 已经既有目标语言又有源语言 → 用户可能已经在双语环境，不激进翻译
        assert!(!needs_translation("hello 世界", "zh"));
        assert!(!needs_translation("hello 世界", "en"));
    }

    // ---- needs_translation 护栏：多行 URL / 路径列表（review #12）----

    #[test]
    fn needs_translation_multiline_url_list_guard() {
        // 场景：复制了两条 URL 到剪贴板，中间有换行
        // is_url 因内含 whitespace 恒 false → 若无首行护栏，detect_lang=Latin + target=zh 会误触发
        assert!(!needs_translation(
            "https://github.com/a/b\nhttps://github.com/c/d",
            "zh",
        ));
        // 首行是 URL、后续是英文说明 → 也不该触发
        assert!(!needs_translation(
            "https://example.com\nSome English description here",
            "zh",
        ));
        // \r\n 换行也算
        assert!(!needs_translation("https://a.com\r\nhttps://b.com", "zh",));
        // 多行文件路径同理
        assert!(!needs_translation(
            "C:\\Users\\a\\file.txt\nD:\\other.txt",
            "zh",
        ));
    }

    #[test]
    fn needs_translation_multiline_normal_text_ok() {
        // 首行不是 URL / 路径的多行英文，应该照常触发
        assert!(needs_translation("hello world\nsecond line here", "zh",));
        // 首行是普通英文，后续含 URL —— 依旧触发（护栏只挡"首行是链接"）
        assert!(needs_translation(
            "check this out\nhttps://example.com",
            "zh",
        ));
    }

    // ---- 0.20.0 结构化文本负向过滤 ----

    #[test]
    fn structured_text_color_literals() {
        // 正例
        assert!(is_non_prose_structured_text("#fff"));
        assert!(is_non_prose_structured_text("#FF0000"));
        assert!(is_non_prose_structured_text("#1a2b3c4d"));
        assert!(is_non_prose_structured_text("rgb(255, 0, 0)"));
        assert!(is_non_prose_structured_text("rgba(0,0,0,0.5)"));
        assert!(is_non_prose_structured_text("hsl(120, 100%, 50%)"));
        // 反例（自然语言）
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the color is red"));
        assert!(!is_non_prose_structured_text("I like blue"));
        assert!(!is_non_prose_structured_text("RGB is a color model"));
        assert!(!is_non_prose_structured_text("this is a test"));
    }

    #[test]
    fn structured_text_uuid() {
        // 正例
        assert!(is_non_prose_structured_text(
            "550e8400-e29b-41d4-a716-446655440000"
        ));
        assert!(is_non_prose_structured_text(
            "12345678123456781234567812345678"
        ));
        assert!(is_non_prose_structured_text(
            "deadbeef-dead-beef-dead-beefdeadbeef"
        ));
        assert!(is_non_prose_structured_text(
            "00000000-0000-0000-0000-000000000000"
        ));
        assert!(is_non_prose_structured_text(
            "FFFFFFFF-FFFF-FFFF-FFFF-FFFFFFFFFFFF"
        ));
        // 反例
        assert!(!is_non_prose_structured_text("this is a uuid"));
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the id is 12345"));
        assert!(!is_non_prose_structured_text("a test string"));
        assert!(!is_non_prose_structured_text("some random text"));
    }

    #[test]
    fn structured_text_hash() {
        // 正例
        assert!(is_non_prose_structured_text(
            "098f6bcd4621d373cade4e832627b4f6"
        ));
        assert!(is_non_prose_structured_text("a94a8fef"));
        assert!(is_non_prose_structured_text(
            "da39a3ee5e6b4b0d3255bfef95601890afd80709"
        ));
        assert!(is_non_prose_structured_text(
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        ));
        assert!(is_non_prose_structured_text("abc1234"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the hash is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some random text"));
        assert!(!is_non_prose_structured_text("a short string"));
    }

    #[test]
    fn structured_text_version() {
        // 正例
        assert!(is_non_prose_structured_text("1.0"));
        assert!(is_non_prose_structured_text("v1.2.3"));
        assert!(is_non_prose_structured_text("2.0.0-beta"));
        assert!(is_non_prose_structured_text("1.2.3-rc.1"));
        assert!(is_non_prose_structured_text("10.5.2"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("version one point zero"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("the number is 5"));
        assert!(!is_non_prose_structured_text("some text here"));
    }

    #[test]
    fn structured_text_ip_port() {
        // 正例
        assert!(is_non_prose_structured_text("192.168.1.1"));
        assert!(is_non_prose_structured_text("10.0.0.1:8080"));
        assert!(is_non_prose_structured_text("127.0.0.1"));
        assert!(is_non_prose_structured_text("255.255.255.0:443"));
        assert!(is_non_prose_structured_text("::1"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the IP is local"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("version 1.2.3"));
    }

    #[test]
    fn structured_text_email() {
        // 正例
        assert!(is_non_prose_structured_text("user@example.com"));
        assert!(is_non_prose_structured_text("test.user@domain.org"));
        assert!(is_non_prose_structured_text("a@b.co"));
        assert!(is_non_prose_structured_text("name+surname@company.io"));
        assert!(is_non_prose_structured_text("admin@sub.domain.com"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the email is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("contact us please"));
    }

    #[test]
    fn structured_text_css_declaration() {
        // 正例
        assert!(is_non_prose_structured_text("--accent: #ff0000;"));
        assert!(is_non_prose_structured_text("color: red;"));
        assert!(is_non_prose_structured_text("--my-var"));
        assert!(is_non_prose_structured_text("background-color: #fff;"));
        assert!(is_non_prose_structured_text("margin: 0 auto;"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the CSS is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("color is red"));
    }

    #[test]
    fn structured_text_short_json() {
        // 正例
        assert!(is_non_prose_structured_text(r#"{"key": "value"}"#));
        assert!(is_non_prose_structured_text(r#"[1, 2, 3]"#));
        assert!(is_non_prose_structured_text(r#"{"a": 1, "b": 2}"#));
        assert!(is_non_prose_structured_text(
            r#"{"nested": {"inner": true}}"#
        ));
        assert!(is_non_prose_structured_text(r#"["a", "b"]"#));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the JSON is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("a plain string"));
    }

    #[test]
    fn structured_text_command_args() {
        // 正例
        assert!(is_non_prose_structured_text("--help"));
        assert!(is_non_prose_structured_text("npm install -g"));
        assert!(is_non_prose_structured_text("--output=xml"));
        assert!(is_non_prose_structured_text("-v --verbose"));
        assert!(is_non_prose_structured_text("git commit -m"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the command is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("a short string"));
    }

    #[test]
    fn structured_text_code_identifier() {
        // 正例
        assert!(is_non_prose_structured_text("camelCaseVar"));
        assert!(is_non_prose_structured_text("snake_case_var"));
        assert!(is_non_prose_structured_text("PascalCase"));
        assert!(is_non_prose_structured_text("SCREAMING_SNAKE"));
        assert!(is_non_prose_structured_text("module.function"));
        assert!(is_non_prose_structured_text("obj.method2"));
        // 反例
        assert!(!is_non_prose_structured_text("hello world"));
        assert!(!is_non_prose_structured_text("the identifier is valid"));
        assert!(!is_non_prose_structured_text("this is a test"));
        assert!(!is_non_prose_structured_text("some text here"));
        assert!(!is_non_prose_structured_text("just a word"));
    }

    #[test]
    fn structured_text_explicit_translate_not_suppressed() {
        // 显式"翻译 …"前缀不受抑制
        assert!(!is_non_prose_structured_text("翻译 hello world"));
        assert!(!is_non_prose_structured_text("翻译 #ff0000"));
        assert!(!is_non_prose_structured_text("translate this code"));
        assert!(!is_non_prose_structured_text("translate --help"));
        assert!(!is_non_prose_structured_text("翻译 user@example.com"));
    }

    #[test]
    fn needs_translation_structured_text_guard() {
        // 结构化文本不应触发翻译
        assert!(!needs_translation("#ff0000", "zh"));
        assert!(!needs_translation(
            "550e8400-e29b-41d4-a716-446655440000",
            "zh"
        ));
        assert!(!needs_translation("192.168.1.1", "zh"));
        assert!(!needs_translation("user@example.com", "zh"));
        assert!(!needs_translation("--help", "zh"));
        // 但显式"翻译 …"前缀放行
        assert!(needs_translation("翻译 hello world", "zh"));
        assert!(needs_translation("translate this text", "zh"));
    }
}
