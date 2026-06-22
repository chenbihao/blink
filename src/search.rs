//! 应用搜索：扫描开始菜单 + nucleo fuzzy 匹配 + 拼音首字母。

use std::collections::HashMap;
use std::path::PathBuf;

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};
use pinyin::ToPinyin;
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

/// 扫描用户 + 系统开始菜单，收集所有 .lnk 条目。
pub fn scan_start_menu() -> Vec<AppEntry> {
    let mut entries = Vec::new();
    if let Ok(appdata) = std::env::var("APPDATA") {
        scan_dir(
            &PathBuf::from(appdata).join("Microsoft/Windows/Start Menu/Programs"),
            &mut entries,
        );
    }
    if let Ok(program_data) = std::env::var("ProgramData") {
        scan_dir(
            &PathBuf::from(program_data).join("Microsoft/Windows/Start Menu/Programs"),
            &mut entries,
        );
    }
    entries
}

fn scan_dir(dir: &PathBuf, entries: &mut Vec<AppEntry>) {
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir(&path, entries);
        } else if path.extension().map_or(false, |ext| ext == "lnk") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("")
                .to_string();
            if !name.is_empty() {
                let pinyin_name = to_pinyin_initials(&name);
                entries.push(AppEntry {
                    name,
                    pinyin_name,
                    lnk_path: path.to_string_lossy().to_string(),
                    is_calc: false,
                });
            }
        }
    }
}

/// 提取拼音首字母（"微信" → "wx"，"WeChat" → "wechat"）。
fn to_pinyin_initials(s: &str) -> String {
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

/// 启动应用：直接打开 lnk 文件，Windows 自动解析并启动对应 exe。
pub fn launch(lnk_path: &str) -> Result<(), String> {
    open::that(lnk_path).map_err(|e| e.to_string())
}
