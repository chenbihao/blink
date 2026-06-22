//! 应用搜索：平台接口 + 通用逻辑。
//!
//! 平台特定实现（如 Windows 开始菜单扫描）在对应平台模块中。
//!
//! TODO: 方案 B - 平台抽象 trait
//!
//! 当需要支持多平台时，可以将搜索抽象为 trait：
//!
//! ```rust
//! pub trait AppScanner {
//!     fn scan_apps(&self) -> Vec<AppEntry>;
//!     fn launch_app(&self, path: &str) -> Result<(), String>;
//! }
//!
//! // 每个平台实现自己的 AppScanner
//! pub struct WindowsAppScanner { /* 开始菜单扫描 */ }
//! pub struct MacosAppScanner { /* Spotlight/Applications */ }
//! pub struct LinuxAppScanner { /* xdg/desktop files */ }
//! ```

use serde::Serialize;

/// 应用条目。
#[derive(Debug, Clone, Serialize)]
pub struct AppEntry {
    /// 显示名（lnk 文件名去掉 .lnk）
    pub name: String,
    /// 拼音首字母（如 "微信" → "wx"），用于拼音首字母匹配
    pub pinyin_name: String,
    /// lnk 文件完整路径
    pub lnk_path: String,
    /// 是否为计算结果（前端可据此显示特殊样式）
    #[serde(default)]
    pub is_calc: bool,
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{scan_start_menu, launch};

// 通用逻辑

use std::collections::HashMap;

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};
use pinyin::ToPinyin;

/// 提取拼音首字母（"微信" → "wx"，"WeChat" → "wechat"）。
pub fn to_pinyin_initials(s: &str) -> String {
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

/// nucleo fuzzy 搜索，同时匹配原始名和拼音首字母，取最高分，融合历史权重，返回 top-N。
pub fn fuzzy_search(
    query: &str,
    entries: &[AppEntry],
    history: &HashMap<String, i64>,
    limit: usize,
) -> Vec<AppEntry> {
    if query.is_empty() {
        return entries.iter().take(limit).cloned().collect();
    }
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(query, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &AppEntry)> = entries
        .iter()
        .filter_map(|e| {
            let score_name = {
                let haystack = Utf32Str::new(&e.name, &mut buf);
                pattern.score(haystack, &mut matcher)
            };
            let score_pinyin = {
                let haystack = Utf32Str::new(&e.pinyin_name, &mut buf);
                pattern.score(haystack, &mut matcher)
            };
            let best = match (score_name, score_pinyin) {
                (Some(a), Some(b)) => Some(a.max(b)),
                (Some(a), None) => Some(a),
                (None, Some(b)) => Some(b),
                (None, None) => None,
            };
            best.map(|s| {
                let hit = history.get(&e.lnk_path).copied().unwrap_or(0) as f64;
                let bonus = (hit + 1.0).ln() * 100.0;
                (s + bonus as u32, e)
            })
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, e)| e.clone())
        .collect()
}
