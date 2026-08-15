//! 确定性颜色字面量解析（0.20.3）。
//!
//! 支持 `#RGB`、`#RGBA`、`#RRGGBB`、`#RRGGBBAA`、`rgb()/rgba()`、`hsl()/hsla()` 常用格式。
//! 不支持 CSS 命名色、渐变、变量引用、长文本中的局部颜色。
//!
//! 所有输出统一使用 RGBA8（u8 四元组）和 canonical HEX/RGB/HSL。
//! 舍入策略统一使用 half-away-from-zero，Rust/JS fixture 必须一致。
//!
//! **架构约束**：本模块是纯函数，不依赖 tauri/async/IO，可独立单测。

use serde::{Deserialize, Serialize};

/// 颜色解析结果，包含原始文本、RGBA8 和 canonical 三格式。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ColorResult {
    /// 原始输入文本（trim 后）
    pub original: String,
    /// RGBA8 色值
    pub rgba: Rgba8,
    /// Canonical HEX（大写，如 `#FF0000` 或 `#FF000080` 含 alpha）
    pub hex: String,
    /// Canonical RGB（如 `rgb(255, 0, 0)` 或 `rgb(255, 0, 0, 0.502)` 含 alpha < 1）
    pub rgb: String,
    /// Canonical HSL（如 `hsl(0, 100%, 50%)` 或 `hsl(0, 100%, 50%, 0.502)` 含 alpha < 1）
    pub hsl: String,
    /// alpha 通道浮点值 [0.0, 1.0]
    pub alpha: f32,
}

/// RGBA8 色值（u8 四元组）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Rgba8 {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Rgba8 {
    pub const fn new(r: u8, g: u8, b: u8, a: u8) -> Self {
        Self { r, g, b, a }
    }

    /// alpha 浮点值 [0.0, 1.0]
    pub fn alpha_f32(self) -> f32 {
        self.a as f32 / 255.0
    }
}

// ── 公共解析入口 ─────────────────────────────────────────────────────────

/// 尝试将完整 trim 后的字符串解析为颜色字面量。
///
/// 仅当完整文本可解析时返回 `Some`，不支持长文本中的局部颜色提取。
/// 空字符串、非法格式返回 `None`。
pub fn parse(input: &str) -> Option<ColorResult> {
    let trimmed = input.trim();
    if trimmed.is_empty() {
        return None;
    }

    // 快速首字符过滤，避免逐个正则试
    let first = trimmed.as_bytes().first()?;

    let rgba = if first == &b'#' {
        parse_hex(trimmed)?
    } else {
        let lower = trimmed.to_ascii_lowercase();
        if lower.starts_with("rgb") {
            parse_rgb_like(trimmed)?
        } else if lower.starts_with("hsl") {
            parse_hsl_like(trimmed)?
        } else {
            return None;
        }
    };

    // 确认不是误匹配（如 "rgbhello"）——parse 函数内部已做结构校验，
    // 此处只确认成功即可。

    Some(build_color_result(trimmed, rgba))
}

/// 从 RGBA8 构建 canonical 输出。
pub fn build_color_result(original: &str, rgba: Rgba8) -> ColorResult {
    let alpha = rgba.alpha_f32();
    ColorResult {
        original: original.to_string(),
        rgba,
        hex: to_hex(rgba),
        rgb: to_rgb(rgba),
        hsl: to_hsl(rgba),
        alpha,
    }
}

// ── HEX 解析 ──────────────────────────────────────────────────────────────

fn parse_hex(s: &str) -> Option<Rgba8> {
    let hex = &s[1..]; // skip '#'
    // 多字节字符的字节长度会误命中 3/4/6/8 分支，随后的固定步长切片
    // 会切进 UTF-8 字符中间导致 panic（如 "#你好" 恰为 6 字节）
    if !hex.is_ascii() {
        return None;
    }
    match hex.len() {
        3 => {
            // #RGB → #RRGGBB
            let r = hex_nibble(hex.as_bytes()[0])?;
            let g = hex_nibble(hex.as_bytes()[1])?;
            let b = hex_nibble(hex.as_bytes()[2])?;
            Some(Rgba8::new(r * 17, g * 17, b * 17, 255))
        }
        4 => {
            // #RGBA
            let r = hex_nibble(hex.as_bytes()[0])?;
            let g = hex_nibble(hex.as_bytes()[1])?;
            let b = hex_nibble(hex.as_bytes()[2])?;
            let a = hex_nibble(hex.as_bytes()[3])?;
            Some(Rgba8::new(r * 17, g * 17, b * 17, a * 17))
        }
        6 => {
            // #RRGGBB
            let r = hex_byte(&hex[0..2])?;
            let g = hex_byte(&hex[2..4])?;
            let b = hex_byte(&hex[4..6])?;
            Some(Rgba8::new(r, g, b, 255))
        }
        8 => {
            // #RRGGBBAA
            let r = hex_byte(&hex[0..2])?;
            let g = hex_byte(&hex[2..4])?;
            let b = hex_byte(&hex[4..6])?;
            let a = hex_byte(&hex[6..8])?;
            Some(Rgba8::new(r, g, b, a))
        }
        _ => None,
    }
}

#[inline]
fn hex_nibble(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[inline]
fn hex_byte(s: &str) -> Option<u8> {
    let bytes = s.as_bytes();
    if bytes.len() != 2 {
        return None;
    }
    let hi = hex_nibble(bytes[0])?;
    let lo = hex_nibble(bytes[1])?;
    Some(hi * 16 + lo)
}

// ── rgb()/rgba() 解析 ────────────────────────────────────────────────────

fn parse_rgb_like(s: &str) -> Option<Rgba8> {
    let inner = extract_function_args(s, "rgb")?;
    // rgb(a, b, c) or rgba(a, b, c, d)
    let parts: Vec<&str> = inner.split(',').collect();
    match parts.len() {
        3 => {
            let r = parse_channel(parts[0])?;
            let g = parse_channel(parts[1])?;
            let b = parse_channel(parts[2])?;
            Some(Rgba8::new(clamp_u8(r), clamp_u8(g), clamp_u8(b), 255))
        }
        4 => {
            let r = parse_channel(parts[0])?;
            let g = parse_channel(parts[1])?;
            let b = parse_channel(parts[2])?;
            let a = parse_alpha(parts[3])?;
            Some(Rgba8::new(
                clamp_u8(r),
                clamp_u8(g),
                clamp_u8(b),
                alpha_to_u8(a),
            ))
        }
        _ => None,
    }
}

/// 解析 rgb 通道值：整数 0-255 或百分比 0%-100%
fn parse_channel(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.ends_with('%') {
        let pct_str = &s[..s.len() - 1];
        let pct: f64 = pct_str.parse().ok()?;
        if !(0.0..=100.0).contains(&pct) {
            return None; // 非法百分比
        }
        // half-away-from-zero 舍入
        let v = pct / 100.0 * 255.0;
        Some(v)
    } else {
        let v: f64 = s.parse().ok()?;
        if !(0.0..=255.0).contains(&v) {
            return None;
        }
        Some(v)
    }
}

/// 解析 alpha 值：0-1 浮点
fn parse_alpha(s: &str) -> Option<f64> {
    let s = s.trim();
    let v: f64 = s.parse().ok()?;
    if !(0.0..=1.0).contains(&v) {
        return None;
    }
    Some(v)
}

// ── hsl()/hsla() 解析 ────────────────────────────────────────────────────

fn parse_hsl_like(s: &str) -> Option<Rgba8> {
    let inner = extract_function_args(s, "hsl")?;
    let parts: Vec<&str> = inner.split(',').collect();
    match parts.len() {
        3 => {
            let h = parse_hue(parts[0])?;
            let s = parse_percent(parts[1])?;
            let l = parse_percent(parts[2])?;
            let (r, g, b) = hsl_to_rgb(h, s, l);
            Some(Rgba8::new(r, g, b, 255))
        }
        4 => {
            let h = parse_hue(parts[0])?;
            let s = parse_percent(parts[1])?;
            let l = parse_percent(parts[2])?;
            let a = parse_alpha(parts[3])?;
            let (r, g, b) = hsl_to_rgb(h, s, l);
            Some(Rgba8::new(r, g, b, alpha_to_u8(a)))
        }
        _ => None,
    }
}

/// 解析色相值：支持任意有限浮点（含负数和 >360），对 360 取模
fn parse_hue(s: &str) -> Option<f64> {
    let s = s.trim();
    let v: f64 = s.parse().ok()?;
    // "NaN"/"inf" 能通过 parse，但 rem_euclid 只会产出垃圾值
    if !v.is_finite() {
        return None;
    }
    Some(v)
}

/// 解析百分比通道（如 "50%"），返回 0-100 浮点
fn parse_percent(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.ends_with('%') {
        let pct: f64 = s[..s.len() - 1].parse().ok()?;
        if !(0.0..=100.0).contains(&pct) {
            return None;
        }
        Some(pct)
    } else {
        None
    }
}

// ── 函数参数提取 ─────────────────────────────────────────────────────────

/// 从 `rgb(...)` 或 `rgba(...)` 或 `hsl(...)` 或 `hsla(...)` 中提取括号内参数字符串。
/// `fn_name` = "rgb" 或 "hsl"（不含 a 后缀）
fn extract_function_args(s: &str, fn_name: &str) -> Option<String> {
    // 检查前缀（fn_name 或 fn_name + "a"）
    let lower = s.to_ascii_lowercase();
    let prefix = if lower.starts_with(&format!("{}a(", fn_name)) {
        format!("{}a(", fn_name)
    } else if lower.starts_with(&format!("{}(", fn_name)) {
        format!("{}(", fn_name)
    } else {
        return None;
    };

    // 使用小写版本定位括号
    let start = prefix.len();
    let rest = &s[start..];

    // 查找右括号——必须恰好出现在末尾（trim 后）
    let end = rest.rfind(')')?;
    // 右括号后面不能有其他字符（除了空格已在 trim 中去掉）
    if end != rest.len() - 1 {
        return None;
    }
    Some(rest[..end].to_string())
}

// ── 颜色转换 ─────────────────────────────────────────────────────────────

/// half-away-from-zero 舍入
#[inline]
fn round_half_away(v: f64) -> f64 {
    if v >= 0.0 {
        (v + 0.5).floor()
    } else {
        (v - 0.5).ceil()
    }
}

#[inline]
fn clamp_u8(v: f64) -> u8 {
    round_half_away(v).clamp(0.0, 255.0) as u8
}

#[inline]
fn alpha_to_u8(a: f64) -> u8 {
    round_half_away(a * 255.0).clamp(0.0, 255.0) as u8
}

/// HSL → RGB 转换。h 为角度，s/l 为 0-100。
fn hsl_to_rgb(h_deg: f64, s_pct: f64, l_pct: f64) -> (u8, u8, u8) {
    let h = h_deg.rem_euclid(360.0) / 360.0;
    let s = s_pct / 100.0;
    let l = l_pct / 100.0;

    let q = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let p = 2.0 * l - q;

    let r = hue_to_rgb(p, q, h + 1.0 / 3.0);
    let g = hue_to_rgb(p, q, h);
    let b = hue_to_rgb(p, q, h - 1.0 / 3.0);

    (
        clamp_u8(r * 255.0),
        clamp_u8(g * 255.0),
        clamp_u8(b * 255.0),
    )
}

fn hue_to_rgb(p: f64, q: f64, mut t: f64) -> f64 {
    if t < 0.0 {
        t += 1.0;
    }
    if t > 1.0 {
        t -= 1.0;
    }
    if t < 1.0 / 6.0 {
        return p + (q - p) * 6.0 * t;
    }
    if t < 0.5 {
        return q;
    }
    if t < 2.0 / 3.0 {
        return p + (q - p) * (2.0 / 3.0 - t) * 6.0;
    }
    p
}

// ── Canonical 输出 ───────────────────────────────────────────────────────

/// 生成 canonical HEX（大写）。
/// alpha < 255 时输出 8 位，否则 6 位。
pub fn to_hex(rgba: Rgba8) -> String {
    if rgba.a < 255 {
        format!(
            "#{:02X}{:02X}{:02X}{:02X}",
            rgba.r, rgba.g, rgba.b, rgba.a
        )
    } else {
        format!("#{:02X}{:02X}{:02X}", rgba.r, rgba.g, rgba.b)
    }
}

/// 生成 canonical RGB 字符串。
/// alpha < 1 时输出 `rgba(...)` 格式（兼容 CSS），alpha 四舍五入到 3 位小数。
pub fn to_rgb(rgba: Rgba8) -> String {
    let alpha = rgba.alpha_f32();
    if alpha < 1.0 {
        // 格式化 alpha：最多 3 位小数，去掉尾部 0
        let a_str = format_alpha(alpha);
        format!("rgb({}, {}, {}, {})", rgba.r, rgba.g, rgba.b, a_str)
    } else {
        format!("rgb({}, {}, {})", rgba.r, rgba.g, rgba.b)
    }
}

/// 生成 canonical HSL 字符串。
/// alpha < 1 时输出 `hsla(...)` 格式（兼容 CSS）。
pub fn to_hsl(rgba: Rgba8) -> String {
    let (h, s, l) = rgb_to_hsl(rgba.r, rgba.g, rgba.b);
    let alpha = rgba.alpha_f32();
    if alpha < 1.0 {
        let a_str = format_alpha(alpha);
        format!("hsl({}, {}%, {}%, {})", h, s, l, a_str)
    } else {
        format!("hsl({}, {}%, {}%)", h, s, l)
    }
}

/// RGB → HSL 转换，返回 (h_angle: i32, s_pct: f64, l_pct: f64)。
/// h 范围 0-360（整数），s/l 范围 0-100（保留 1 位小数，但作为 f64 返回）。
fn rgb_to_hsl(r: u8, g: u8, b: u8) -> (i32, f64, f64) {
    let r = r as f64 / 255.0;
    let g = g as f64 / 255.0;
    let b = b as f64 / 255.0;

    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;

    let l = (max + min) / 2.0;

    let s = if delta == 0.0 {
        0.0
    } else if l < 0.5 {
        delta / (max + min)
    } else {
        delta / (2.0 - max - min)
    };

    let h = if delta == 0.0 {
        0.0
    } else {
        let h_raw = if r == max {
            (g - b) / delta
        } else if g == max {
            2.0 + (b - r) / delta
        } else {
            4.0 + (r - g) / delta
        };
        h_raw * 60.0
    };

    // h 取模 360，再 half-away-from-zero 舍入到整数
    let h_mod = h.rem_euclid(360.0);
    let h_int = round_half_away(h_mod) as i32;
    let h_int = h_int % 360;

    // s 和 l 保留 1 位小数，half-away-from-zero 舍入
    let s_pct = (round_half_away(s * 1000.0) / 10.0) as f64;
    let l_pct = (round_half_away(l * 1000.0) / 10.0) as f64;

    (h_int, s_pct, l_pct)
}

/// 格式化 alpha 浮点为字符串：最多 3 位小数，去掉尾部 0。
/// 如 0.5 → "0.5"，0.333 → "0.333"，0.0 → "0"，1.0 → "1"
fn format_alpha(a: f32) -> String {
    if a == 0.0 {
        return "0".to_string();
    }
    if a == 1.0 {
        return "1".to_string();
    }
    // 保留 3 位小数，去掉尾部 0
    let s = format!("{:.3}", a);
    let s = s.trim_end_matches('0').trim_end_matches('.');
    s.to_string()
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_hex3() {
        let r = parse("#fff").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 255, 255, 255));
        assert_eq!(r.hex, "#FFFFFF");
    }

    #[test]
    fn parse_hex4() {
        let r = parse("#abcd").unwrap();
        assert_eq!(r.rgba, Rgba8::new(170, 187, 204, 221));
    }

    #[test]
    fn parse_hex6() {
        let r = parse("#ff0000").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
        assert_eq!(r.hex, "#FF0000");
    }

    #[test]
    fn parse_hex8() {
        let r = parse("#ff0000aa").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 170));
        assert_eq!(r.hex, "#FF0000AA");
    }

    #[test]
    fn parse_rgb_int() {
        let r = parse("rgb(255, 0, 0)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_rgb_percent() {
        let r = parse("rgb(100%, 0%, 0%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_rgb_float() {
        let r = parse("rgb(127.5, 0, 0)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(128, 0, 0, 255));
    }

    #[test]
    fn parse_rgba() {
        let r = parse("rgba(255, 0, 0, 0.5)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 128));
    }

    #[test]
    fn parse_hsl() {
        let r = parse("hsl(0, 100%, 50%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_hsl_cyan() {
        let r = parse("hsl(180, 50%, 50%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(64, 191, 191, 255));
    }

    #[test]
    fn parse_hsla() {
        let r = parse("hsla(0, 100%, 50%, 0.5)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 128));
    }

    #[test]
    fn parse_hsl_negative_hue() {
        let r = parse("hsl(-120, 100%, 50%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(0, 0, 255, 255));
    }

    #[test]
    fn parse_hsl_720_hue() {
        let r = parse("hsl(720, 100%, 50%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_hsl_360_hue() {
        let r = parse("hsl(360, 100%, 50%)").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_trailing_space() {
        let r = parse("#ff0000 ").unwrap();
        assert_eq!(r.rgba, Rgba8::new(255, 0, 0, 255));
    }

    #[test]
    fn parse_surrounding_space() {
        let r = parse(" rgb(0, 0, 0) ").unwrap();
        assert_eq!(r.rgba, Rgba8::new(0, 0, 0, 255));
    }

    #[test]
    fn parse_illegal_short_hex() {
        assert!(parse("#a").is_none());
    }

    #[test]
    fn parse_illegal_multibyte_hex() {
        // "#你好" 恰为 6 字节，曾误命中 #RRGGBB 分支后 panic
        assert!(parse("#你好").is_none());
        assert!(parse("#红色的").is_none());
        assert!(parse("#色").is_none());
        assert!(parse("#顔色").is_none());
    }

    #[test]
    fn parse_illegal_nonfinite_hue() {
        assert!(parse("hsl(NaN, 100%, 50%)").is_none());
        assert!(parse("hsl(inf, 100%, 50%)").is_none());
        assert!(parse("hsla(-inf, 100%, 50%, 0.5)").is_none());
    }

    #[test]
    fn parse_illegal_hex_chars() {
        assert!(parse("#gggggg").is_none());
    }

    #[test]
    fn parse_illegal_rgb_overflow() {
        assert!(parse("rgb(256, 0, 0)").is_none());
    }

    #[test]
    fn parse_illegal_rgb_negative() {
        assert!(parse("rgb(-1, 0, 0)").is_none());
    }

    #[test]
    fn parse_illegal_non_color() {
        assert!(parse("hello world").is_none());
    }

    #[test]
    fn parse_illegal_named_color() {
        assert!(parse("red").is_none());
    }

    #[test]
    fn parse_illegal_app_name() {
        assert!(parse("calc").is_none());
        assert!(parse("settings").is_none());
    }

    #[test]
    fn parse_empty() {
        assert!(parse("").is_none());
        assert!(parse("   ").is_none());
    }

    #[test]
    fn canonical_hex_uppercase() {
        let r = parse("#ff0000").unwrap();
        assert_eq!(r.hex, "#FF0000");
    }

    #[test]
    fn canonical_rgb_no_alpha() {
        let r = parse("#ff0000").unwrap();
        assert_eq!(r.rgb, "rgb(255, 0, 0)");
    }

    #[test]
    fn canonical_rgb_with_alpha() {
        let r = parse("rgba(255, 0, 0, 0.5)").unwrap();
        assert_eq!(r.rgb, "rgb(255, 0, 0, 0.502)");
    }

    #[test]
    fn canonical_hsl_no_alpha() {
        let r = parse("hsl(0, 100%, 50%)").unwrap();
        assert_eq!(r.hsl, "hsl(0, 100%, 50%)");
    }

    #[test]
    fn canonical_hsl_with_alpha() {
        let r = parse("hsla(0, 100%, 50%, 0.5)").unwrap();
        assert_eq!(r.hsl, "hsl(0, 100%, 50%, 0.502)");
    }

    #[test]
    fn gray_round_trip() {
        let r = parse("#808080").unwrap();
        assert_eq!(r.hsl, "hsl(0, 0%, 50.2%)");
    }

    #[test]
    fn fully_transparent() {
        let r = parse("#00000000").unwrap();
        assert_eq!(r.rgba, Rgba8::new(0, 0, 0, 0));
        assert_eq!(r.alpha, 0.0);
    }

    #[test]
    fn fully_opaque_hex8() {
        let r = parse("#000000ff").unwrap();
        assert_eq!(r.rgba, Rgba8::new(0, 0, 0, 255));
        assert_eq!(r.alpha, 1.0);
        // fully opaque → 6-digit hex
        assert_eq!(r.hex, "#000000");
    }

    #[test]
    fn alpha_337() {
        let r = parse("rgba(255, 255, 255, 0.337)").unwrap();
        assert_eq!(r.rgba.a, 86);
        assert!((r.alpha - 0.3372549).abs() < 0.001);
    }

    // ── Fixture 驱动测试：Rust/JS 共享 color-literals.json ───────────────
    //
    // 读取 `frontend/js/shared/fixtures/color-literals.json`，对每个 case
    // 验证 Rust parse() 的 RGBA、HEX、RGB、HSL 输出与 fixture 期望一致。
    // 这确保 Rust 与 JS 端对同一输入产出完全相同的 canonical 输出。

    #[derive(serde::Deserialize)]
    struct FixtureRgba {
        r: Option<u8>,
        g: Option<u8>,
        b: Option<u8>,
        a: Option<u8>,
    }

    #[derive(serde::Deserialize)]
    struct FixtureCase {
        input: String,
        hex: Option<String>,
        rgb: Option<String>,
        hsl: Option<String>,
        #[serde(default)]
        rgba: Option<FixtureRgba>,
        #[allow(dead_code)]
        kind: String,
        #[allow(dead_code)]
        alpha: Option<f64>,
    }

    #[derive(serde::Deserialize)]
    struct Fixture {
        cases: Vec<FixtureCase>,
    }

    #[test]
    fn fixture_consistency() {
        let fixture_path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("frontend/js/shared/fixtures/color-literals.json");

        // 环境守卫：fixture 文件不存在时跳过（CI 可能没有前端目录）
        if !fixture_path.exists() {
            eprintln!("跳过 fixture 测试：{} 不存在", fixture_path.display());
            return;
        }

        let content = std::fs::read_to_string(&fixture_path)
            .unwrap_or_else(|e| panic!("读取 fixture 失败: {e}"));
        let fixture: Fixture = serde_json::from_str(&content)
            .unwrap_or_else(|e| panic!("解析 fixture JSON 失败: {e}"));

        let pass_count = fixture.cases.iter().filter(|c| c.rgba.is_some()).count();
        let illegal_count = fixture.cases.len() - pass_count;
        let mut checked = 0;

        for case in &fixture.cases {
            if case.rgba.is_none() {
                // 非法输入：Rust 应返回 None
                assert!(parse(&case.input).is_none(),
                    "非法输入 \"{}\" 应返回 None，但 parse() 返回了 Some", case.input);
                continue;
            }

            let result = parse(&case.input)
                .unwrap_or_else(|| panic!("合法输入 \"{}\" 应返回 Some，但 parse() 返回了 None", case.input));

            let expected = case.rgba.as_ref().unwrap();
            assert_eq!(result.rgba.r, expected.r.unwrap(),
                "r mismatch for \"{}\"", case.input);
            assert_eq!(result.rgba.g, expected.g.unwrap(),
                "g mismatch for \"{}\"", case.input);
            assert_eq!(result.rgba.b, expected.b.unwrap(),
                "b mismatch for \"{}\"", case.input);
            assert_eq!(result.rgba.a, expected.a.unwrap(),
                "a mismatch for \"{}\"", case.input);

            assert_eq!(result.hex, case.hex.as_deref().unwrap(),
                "hex mismatch for \"{}\"", case.input);
            assert_eq!(result.rgb, case.rgb.as_deref().unwrap(),
                "rgb mismatch for \"{}\"", case.input);
            assert_eq!(result.hsl, case.hsl.as_deref().unwrap(),
                "hsl mismatch for \"{}\"", case.input);

            checked += 1;
        }

        assert_eq!(checked, pass_count,
            "fixture 中合法 case 数量不匹配");
        eprintln!(
            "color fixture 测试通过: total={}, pass={}, illegal={}",
            fixture.cases.len(), pass_count, illegal_count
        );
    }
}
