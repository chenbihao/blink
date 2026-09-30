//! 系统语言检测：首次运行按系统语言决定默认 UI 语言。
//!
//! 中文系区域（zh-CN / zh-TW / zh-HK …）→ "zh"，其余 → "en"（通用兜底）。
//! 仅首次运行用一次（config 表为空时，见 `config::init_config`），用户在设置页
//! 改过后以用户选择为准——不会因系统语言变化而反复横跳。
//!
//! 平台特定实现（GetUserDefaultLocaleName）在 `windows.rs`。

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::detect_system_language;

/// 纯逻辑：locale 字符串（BCP47，如 "zh-CN"）→ "zh" | "en"。抽出便于单测。
pub fn language_from_locale(locale: &str) -> String {
    if locale.to_ascii_lowercase().starts_with("zh") {
        "zh".to_string()
    } else {
        "en".to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zh_variants_map_to_zh() {
        for s in ["zh-CN", "zh-TW", "zh-HK", "zh", "ZH-cn", "zh-Hans"] {
            assert_eq!(language_from_locale(s), "zh", "{s} 应判为 zh");
        }
    }

    #[test]
    fn non_zh_maps_to_en() {
        for s in ["en-US", "ja-JP", "fr-FR", "", "en"] {
            assert_eq!(language_from_locale(s), "en", "{s} 应判为 en");
        }
    }
}
