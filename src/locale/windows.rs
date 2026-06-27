//! Windows 实现：GetUserDefaultLocaleName 取用户区域名（BCP47，如 "zh-CN"），
//! 交 `language_from_locale` 归类。

use super::language_from_locale;

/// LOCALE_NAME_MAX_LENGTH（含 null 终止）。
const LOCALE_BUF: usize = 85;

/// 检测系统默认语言（zh / en）。取用户区域格式；取不到则 warn 并回退 en。
pub fn detect_system_language() -> String {
    use windows::Win32::Globalization::GetUserDefaultLocaleName;

    let mut buf = [0u16; LOCALE_BUF];
    // SAFETY：buf 是足够大的可写缓冲；API 仅写入 UTF-16 + null 终止，不读入参内容。
    let len = unsafe { GetUserDefaultLocaleName(&mut buf) };
    if len > 0 {
        let end = (len as usize).min(buf.len());
        let locale = String::from_utf16_lossy(&buf[..end]);
        return language_from_locale(locale.trim_end_matches('\0'));
    }
    tracing::warn!("GetUserDefaultLocaleName 失败，默认语言回退 en");
    "en".to_string()
}
