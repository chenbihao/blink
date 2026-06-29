//! 文本类型判定：纯字符串逻辑，无平台依赖，可单测。
//!
//! 0.8.0 §1.3：`BuiltinEngine::search` 与 `intent::RuleRouter` 共用同一套判定，避免两处实现。
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
    let host_no_port = host_only.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_only);
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
    if bytes.len() >= 3 && (bytes[0] == b'\\' || bytes[0] == b'/') && (bytes[1] == b'\\' || bytes[1] == b'/') {
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
        assert!(!is_url("http://foo"));  // 无 TLD 的短标识
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
}
