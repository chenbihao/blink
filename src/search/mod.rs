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

/// 结果项可执行的动作类型（决定 Enter 行为 + 提示栏文案）。
/// 与 is_calc（产生方式/样式标识）正交：计算结果 is_calc=true 且 action.kind=Copy。
/// 0.2.2 迁 SearchItem 时并入带 payload 的 SearchAction（见 0.2 设计 §2.1）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// 打开应用/快捷方式/文件
    Open,
    /// 复制到剪贴板
    Copy,
    // 未来：Plugin / Ai
}

/// 结果项的动作描述。
#[derive(Debug, Clone, Serialize)]
pub struct Action {
    pub kind: ActionKind,
    /// 可选自定义动作名（插件用，如「安装」），覆盖默认文案；None 用 kind 默认文案。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 动作 payload:Copy 携带待复制文本;Open 的路径仍在 `AppEntry.lnk_path`
    /// (lnk_path 是 history 主键,不复用)。前端按 kind + payload 执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
}

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
    /// 描述副行（应用/快捷方式 → 路径；计算 → 提示文案；未来插件自定义）。
    /// 由结果生产者（scan_dir/calc/未来 SearchEngine）填充，前端浅色小字展示。
    #[serde(default)]
    pub description: Option<String>,
    /// 排序分数(归一化 0.0..=1.0),前端 merge 时重排用。后端 fuse 时已排序,此字段供
    /// 前端在增量到达后与既有结果重新排序(0.4 priority 置顶)。
    #[serde(default)]
    pub score: f32,
    /// 占位项标记(takeover 时先同步返回,真实结果到达后前端自动移除)。
    #[serde(default)]
    pub is_placeholder: bool,
    /// 错误信息标记(插件返回错误时显示提示，不可选中执行)。
    #[serde(default)]
    pub is_error: bool,
    /// 来源(引擎 id / plugin id),前端增量 merge 时用：
    /// - 引擎结果："start_menu"、"file"、"calc"
    /// - 插件结果：plugin_id（如 "builtin.weather"）
    /// - 插件占位：同 plugin_id（与插件结果匹配实现自动替换）
    #[serde(default)]
    pub source: String,
    /// 可执行动作（决定 Enter 行为 + 提示栏文案）。与 description 正交。
    pub action: Action,
    /// 分数构成详情（可选，debug 日志用，前端不显示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_detail: Option<String>,
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{scan_start_menu, scan_apps_folder, launch, roots_modified, start_menu_roots, parse_lnk_entry};

#[cfg(target_os = "windows")]
pub mod icon;

// 多路搜索引擎抽象(0.2.2,见 0.2 设计 §2)
pub(crate) mod engine;
#[allow(unused_imports)] // Lane/QueryContext 等供引擎与 service 内部用
pub use engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

// 统一打分/加权模块（0.6 第一阶段重构）
pub(crate) mod scorer;
#[allow(unused_imports)]
pub use scorer::{apply_history, boost_priority, BuiltinMatch, clamp_plugin_score, history_boost, normalize_top_relative, placeholder_score};

// 具体引擎
mod builtin_engine;
mod calc_engine;
pub mod file_engine;
mod mock_slow_engine;
mod start_menu_engine;
use builtin_engine::BuiltinEngine;
use calc_engine::CalcEngine;
use file_engine::FileEngine;
use mock_slow_engine::MockSlowEngine;
use start_menu_engine::StartMenuEngine;

// 多路搜索服务:路由 + 融合 + 渐进式调度
mod service;
pub use service::{SearchService, EngineConfigUpdate};

/// 引擎配置集合（三层独立控制）。
pub struct EngineConfigs {
    pub start_menu: crate::config::StartMenuConfig,
    pub file: crate::config::FileSearchConfig,
    pub calc: crate::config::CalcConfig,
}

/// 构造引擎列表(sync: builtin + calc + start_menu;async: file)。
/// PluginEngine 0.4 退化为执行器,不再作为 dyn SearchEngine,由 SearchService 直接持有。
pub fn build_engines(configs: EngineConfigs) -> Vec<std::sync::Arc<dyn SearchEngine>> {
    let mut engines: Vec<std::sync::Arc<dyn SearchEngine>> = vec![
        // BuiltinEngine（始终启用，本体功能）
        std::sync::Arc::new(BuiltinEngine),
        // CalcEngine（可配置）
        std::sync::Arc::new(CalcEngine::with_config(configs.calc)),
        // StartMenuEngine（可配置）
        std::sync::Arc::new(StartMenuEngine::with_config(configs.start_menu)),
    ];

    // FileEngine（可配置，总开关关闭时不加载）
    if configs.file.enabled {
        engines.push(std::sync::Arc::new(FileEngine::with_config(configs.file)));
    }

    if MockSlowEngine::enabled() {
        tracing::info!("MockSlowEngine 已启用(BLINK_MOCK_SLOW_ENGINE=1)");
        engines.push(std::sync::Arc::new(MockSlowEngine));
    }
    engines
}

// 通用逻辑

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

// 文本归一化原语抽至 `crate::text`(0.4 §4.4),应用搜索与意图共用。
pub use crate::text::pinyin_initials as to_pinyin_initials;

/// fuzzy 打分核心：返回 `(nucleo raw 分, 条目)`，按分降序、取 top-N。
///
/// 只负责 nucleo fuzzy 匹配，**不含历史加权**——历史统一在归一化后由
/// `scorer::apply_history` 处理（与 Builtin/Calc/File/Plugin 共用同一公式）。
/// raw 分数供引擎做 top-relative 归一化。空 query 返回前 limit 条（分数置 0）。
/// 由 `StartMenuEngine` 调用(SearchService 接管搜索后,这是唯一打分入口)。
pub fn fuzzy_score_entries(
    query: &str,
    entries: &[AppEntry],
    limit: usize,
) -> Vec<(u32, AppEntry)> {
    if query.is_empty() {
        return entries.iter().take(limit).map(|e| (0, e.clone())).collect();
    }
    // 查询转小写：确保大写 "WX" 能匹配小写 "wx" 的拼音首字母
    let query_lower = query.to_ascii_lowercase();
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(&query_lower, CaseMatching::Smart, Normalization::Smart, AtomKind::Fuzzy);
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
            best.map(|s| (s, e))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(s, e)| (s, e.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(name: &str, lnk: &str) -> AppEntry {
        AppEntry {
            name: name.into(),
            pinyin_name: to_pinyin_initials(name),
            lnk_path: lnk.into(),
            is_calc: false,
            score: 0.0,
            is_placeholder: false,
            is_error: false,
            source: String::new(),
            description: Some(lnk.into()),
            action: Action {
                kind: ActionKind::Open,
                hint: None,
                payload: None,
            },
            score_detail: None,
        }
    }

    #[test]
    fn pinyin_initials_basic() {
        assert_eq!(to_pinyin_initials("微信"), "wx");
        assert_eq!(to_pinyin_initials("WeChat"), "wechat");
    }

    #[test]
    fn empty_query_returns_prefix_with_zero_score() {
        let entries = vec![entry("Alpha", "a"), entry("Beta", "b"), entry("Gamma", "c")];
        let r = fuzzy_score_entries("", &entries, 2);
        assert_eq!(r.len(), 2);
        assert!(r.iter().all(|(s, _)| *s == 0));
        assert_eq!(r[0].1.name, "Alpha");
    }

    #[test]
    fn matches_by_pinyin_initials() {
        let entries = vec![entry("微信", "wechat.lnk"), entry("Word", "word.lnk")];
        let r = fuzzy_score_entries("wx", &entries, 10);
        assert!(r.iter().any(|(_, e)| e.name == "微信"));
    }

    #[test]
    fn pinyin_initials_case_insensitive() {
        let entries = vec![entry("微信", "wechat.lnk")];
        // 小写匹配
        let r_lower = fuzzy_score_entries("wx", &entries, 10);
        assert!(!r_lower.is_empty());
        // 大写匹配
        let r_upper = fuzzy_score_entries("WX", &entries, 10);
        assert!(!r_upper.is_empty());
        // 混合大小写也匹配
        let r_mixed = fuzzy_score_entries("Wx", &entries, 10);
        assert!(!r_mixed.is_empty());
        // 分数应该相同
        assert_eq!(r_lower[0].0, r_upper[0].0);
        assert_eq!(r_lower[0].0, r_mixed[0].0);
    }
}
