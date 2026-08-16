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
/// 0.8.0 §1.3 加 `Run`：内置动作 id 分派，替代原 `__BLINK_ACTION_XXX__` 魔法串。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ActionKind {
    /// 打开应用/快捷方式/文件
    Open,
    /// 复制到剪贴板
    Copy,
    /// 运行注册好的内置动作（by id），参数走 `Action.run_arg`。
    /// 前端识别 `kind === "run"` → `invoke("run_builtin_action", { id, arg })`。
    Run,
    // 未来：Plugin / Ai
}

/// 结果项的动作描述。
///
/// `rename_all = "camelCase"` 让 `run_id` → `runId`、`run_arg` → `runArg`、
/// `hit_id` → `hitId`，与前端 JS 的 camelCase 访问一致。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Action {
    pub kind: ActionKind,
    /// 可选自定义动作名（插件用，如「安装」），覆盖默认文案；None 用 kind 默认文案。
    /// hint 也可以存 i18n key（如 `"menu.edit"`），前端用 `t(hint)` 渲染。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
    /// 动作 payload:Copy 携带待复制文本;Open 的路径仍在 `AppEntry.lnk_path`
    /// (lnk_path 是 history 主键,不复用)。前端按 kind + payload 执行。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload: Option<String>,
    /// `Run` 动作的 id（内置动作注册表 key，如 `"open_settings"`）。
    /// 仅 `kind == Run` 时有意义；其他 kind 恒为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_id: Option<String>,
    /// `Run` 动作的参数（0.8.0 §1.3 参数化 Action 用）。
    /// 用 `serde_json::Value` 而非 `String`：未来扩到结构化参数（选区文本+目标语言）
    /// 无需再改契约；当前只填 `Value::String(...)` 或 `null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run_arg: Option<serde_json::Value>,
    /// `Copy` 动作的命中回写 id（0.8.5 §6.4）。仅 ClipboardEngine 展开的历史条目非空;
    /// 前端复制成功后 `invoke("record_clipboard_hit", { id })` 频率加权。
    /// CalcEngine / Plugin Copy 恒为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hit_id: Option<String>,
}

impl Default for Action {
    /// 默认 = `Open` 动作、无 hint/payload/run_*。搭配 `..Default::default()` 让新增
    /// 字段时无需修改所有构造点（0.8.0 §1.3 加 run_id/run_arg 时如此）。
    fn default() -> Self {
        Self {
            kind: ActionKind::Open,
            hint: None,
            payload: None,
            run_id: None,
            run_arg: None,
            hit_id: None,
        }
    }
}

/// 应用条目。
///
/// **0.11.0 §3.1 AI 结果视觉形态**：新增 `is_ai_summary` / `is_ai_tool_result`
/// 标记位，区分三种结果形态——AI 总结项（pre-wrap + 24px 徽章）、AI 工具结果项
/// （nowrap 单行 + 12px 小号 AI 图标）、普通结果项（nowrap 单行，现状）。
/// 两者皆 false 时为普通结果项，与查询路径一致。
#[derive(Debug, Clone, Default, Serialize)]
pub struct AppEntry {
    /// 显示名（lnk 文件名去掉 .lnk）
    pub name: String,
    /// 拼音首字母（如 "微信" → "wx"），用于拼音首字母匹配
    pub pinyin_name: String,
    /// 完整拼音（如 "微信" → "weixin"），用于全拼匹配
    pub pinyin_full: String,
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
    /// - AI 结果："ai"（0.17.6 前由 AI_SOURCE 常量提供，AI lane 删除后前端不再收到此 source）
    #[serde(default)]
    pub source: String,
    /// 可执行动作列表（决定 Enter 行为 + 右键菜单展开）。与 description 正交。
    /// 回车执行 `actions[0]`，右键展开全部。空 vec = 纯展示项。
    /// 0.16.1：从单值 `action: Action` 升级为数组，打通 capability 多 actions 全链路。
    #[serde(default)]
    pub actions: Vec<Action>,
    /// 分数构成详情（可选，debug 日志用，前端不显示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub score_detail: Option<String>,
    /// AI 总结项标记（0.11.0 §3.1）——item[0] 的 AI 文本回答，pre-wrap 撑开 + 24px 徽章。
    /// 回车复制总结文本。仅 AI 文本回答项产 text 时为 true（0.17.6 后 AI 走 ChatService，此字段不再由 SearchService 设置）。
    #[serde(default)]
    pub is_ai_summary: bool,
    /// AI 工具结果项标记（0.11.0 §3.1）——item[1..] 的工具返回 items，nowrap 单行 +
    /// 12px 小号 AI 图标。回车执行各自 action（打开/复制）。
    /// 由工具结果投影时为 true（0.17.6 后 AI 走 ChatService，此字段不再由 SearchService 设置）。
    #[serde(default)]
    pub is_ai_tool_result: bool,
    /// 剪贴板图片项标记（0.16.4）——source="clipboard" 且 is_image=true 时，
    /// 前端渲染缩略图而非文本预览；lnk_path 存图片 id（`clipimg_xxx`）。
    #[serde(default)]
    pub is_image: bool,
    /// 多行颜色列表标记（0.20）——剪贴板历史中文本是多行颜色字面量时，
    /// 前端渲染一排小 swatch 而非无 swatch。color_list_hex 存每行 hex 数组。
    #[serde(default)]
    pub is_color_list: bool,
    /// 多行颜色列表的 hex 数组（配合 is_color_list，如 ["#5D5D3C", "#9D646A"]）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub color_list_hex: Vec<String>,
    /// 环境自动填充标记（0.10.8 §11.2 方案 1）——空 query + Context-only 命中的候选。
    ///
    /// 前端 `chordEligible` 通过 `results.hasUserItems()`（过滤掉 context_aware=true
    /// 的项）判断是否允许 chord 提示条显示：仅有"环境自动填充"候选时视为"用户未开始
    /// 交互"，chord 与 Context Ghost 共存不冲突。
    ///
    /// **产地**：仅 `BuiltinEngine` 空 query + Context-only 命中时为 true。keyword 命中 /
    /// 非空 query / 其它引擎的项一律 false。老前端不读此字段视为 false，兼容。
    #[serde(default)]
    pub context_aware: bool,
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{
    launch, parse_lnk_entry, roots_modified, scan_apps_folder, scan_start_menu, start_menu_roots,
};

// 0.14.6 §2.3：icon 模块已迁至 infra/platform/icon.rs

// 多路搜索引擎抽象(0.2.2,见 0.2 设计 §2)
pub(crate) mod engine;
#[allow(unused_imports)] // Lane/QueryContext 等供引擎与 service 内部用
pub use engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

// 统一打分/加权模块（0.6 第一阶段重构）
pub(crate) mod scorer;
#[allow(unused_imports)]
pub use scorer::{
    BuiltinMatch, apply_history, boost_priority, clamp_plugin_score, history_boost,
    normalize_top_relative, placeholder_score,
};

// 具体引擎
mod builtin_engine;
pub mod calc;
mod calc_engine;
pub(crate) mod clipboard_engine;
mod color_engine;
pub mod file_engine;
mod mock_slow_engine;
mod start_menu_engine;
mod system_shortcuts;
use builtin_engine::BuiltinEngine;
use color_engine::ColorEngine;
// BuiltinActionInfo + list_builtin_actions 由 commands::list_builtin_actions 用（设置页）。
// list_builtin_context_bindings 由 commands::list_context_bindings 合并 builtin 一路用（0.11.8）。
// 0.21.13: BuiltinResultAction + find_result_action + find_capability_id 供 command 层
// 显式结果动作与 descriptor → capability target 查找。
#[allow(unused_imports)]
pub use builtin_engine::{
    BuiltinActionInfo, BuiltinResultAction, find_capability_id, find_result_action,
    find_result_action_by_capability_id, list_builtin_actions, list_builtin_context_bindings,
};
use calc_engine::CalcEngine;
use clipboard_engine::ClipboardEngine;
use file_engine::FileEngine;
use mock_slow_engine::MockSlowEngine;
use start_menu_engine::StartMenuEngine;
use system_shortcuts::SystemShortcutEngine;

// 多路搜索服务:路由 + 融合 + 渐进式调度
mod service;
pub use service::{EngineConfigUpdate, SearchResponse, SearchService};

/// 引擎配置集合（三层独立控制）。
pub struct EngineConfigs {
    pub start_menu: crate::domain::config::StartMenuConfig,
    pub file: crate::domain::config::FileSearchConfig,
    pub calc: crate::domain::config::CalcConfig,
}

/// 构造引擎列表(sync: builtin + calc + clipboard + start_menu;async: file)。
/// PluginEngine 0.4 退化为执行器,不再作为 dyn SearchEngine,由 SearchService 直接持有。
///
/// `pool` 用于持 SqlitePool 的引擎（当前仅 ClipboardEngine 0.8.5 §6.4）。
/// 其他引擎无状态或用 config 快照，不接触 pool。
pub fn build_engines(
    configs: EngineConfigs,
    pool: sqlx::SqlitePool,
    cache_pool: sqlx::SqlitePool,
) -> Vec<std::sync::Arc<dyn SearchEngine>> {
    let mut engines: Vec<std::sync::Arc<dyn SearchEngine>> = vec![
        // BuiltinEngine（始终启用，本体功能）
        std::sync::Arc::new(BuiltinEngine),
        // CalcEngine（可配置）
        std::sync::Arc::new(CalcEngine::with_config(configs.calc)),
        // ColorEngine（0.20.3：颜色字面量确定性结果，始终启用）
        std::sync::Arc::new(ColorEngine::new()),
        // ClipboardEngine（0.8.5 §6.4，keyword 剪贴板/clip 触发展开历史）
        std::sync::Arc::new(ClipboardEngine::new(pool, cache_pool)),
        // StartMenuEngine（可配置）
        std::sync::Arc::new(StartMenuEngine::with_config(configs.start_menu)),
        // SystemShortcutEngine（0.17.1：系统快捷方式，始终启用）
        std::sync::Arc::new(SystemShortcutEngine),
    ];

    // FileEngine（可配置，始终创建以支持热更新；search 内部检查 enabled）
    engines.push(std::sync::Arc::new(FileEngine::with_config(configs.file)));

    if MockSlowEngine::enabled() {
        tracing::info!("MockSlowEngine 已启用(BLINK_MOCK_SLOW_ENGINE=1)");
        engines.push(std::sync::Arc::new(MockSlowEngine));
    }
    engines
}

/// 注册本体 engine 的 keyword 规则到 RuleRouter（0.8.5 §6.4）。
///
/// engine keyword 命中 → `Route::EngineTakeover`，独占返回区。
/// 与插件 keyword 表分离——本体 engine 命中优先级恒高于插件（本体自家数据信号更强，
/// "剪贴板" 不会误命中其他插件），route() 中先检查 engine 表再检查 plugin 表。
///
/// **触发词硬编码**：与 BuiltinAction keyword 沿用同策略（0.8.1 决定 keyword 硬编码到 0.9）。
/// 未来跟 BuiltinAction/Plugin 统一 Action trait 时一起走 manifest。
pub fn register_engine_rules(router: &crate::domain::intent::RuleRouter) {
    router.add_engine_rule(
        clipboard_engine::ENGINE_ID.to_string(),
        clipboard_engine::TRIGGERS
            .iter()
            .map(|s| s.to_string())
            .collect(),
    );
}

// 通用逻辑

use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

// 文本归一化原语抽至 `crate::text`(0.4 §4.4),应用搜索与意图共用。
pub use crate::infra::utils::text::{
    pinyin_full as to_pinyin_full, pinyin_initials as to_pinyin_initials,
};

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
    let pattern = Pattern::new(
        &query_lower,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
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
            let score_pinyin_full = {
                let haystack = Utf32Str::new(&e.pinyin_full, &mut buf);
                pattern.score(haystack, &mut matcher)
            };
            let best = match (score_name, score_pinyin, score_pinyin_full) {
                (Some(a), Some(b), Some(c)) => Some(a.max(b).max(c)),
                (Some(a), Some(b), None) => Some(a.max(b)),
                (Some(a), None, Some(c)) => Some(a.max(c)),
                (None, Some(b), Some(c)) => Some(b.max(c)),
                (Some(a), None, None) => Some(a),
                (None, Some(b), None) => Some(b),
                (None, None, Some(c)) => Some(c),
                (None, None, None) => None,
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
            pinyin_full: to_pinyin_full(name),
            lnk_path: lnk.into(),
            is_calc: false,
            score: 0.0,
            is_placeholder: false,
            is_error: false,
            source: String::new(),
            description: Some(lnk.into()),
            actions: vec![Action::default()],
            ..Default::default()
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

    #[test]
    fn matches_by_pinyin_full() {
        let entries = vec![entry("微信", "wechat.lnk"), entry("Word", "word.lnk")];
        // 全拼搜索：wei 应该匹配微信
        let r = fuzzy_score_entries("wei", &entries, 10);
        assert!(
            r.iter().any(|(_, e)| e.name == "微信"),
            "全拼 wei 应匹配微信"
        );

        // 完整全拼：weixin 应该匹配微信
        let r = fuzzy_score_entries("weixin", &entries, 10);
        assert!(
            r.iter().any(|(_, e)| e.name == "微信"),
            "全拼 weixin 应匹配微信"
        );

        // 全拼前缀模糊：weix 应该匹配微信
        let r = fuzzy_score_entries("weix", &entries, 10);
        assert!(
            r.iter().any(|(_, e)| e.name == "微信"),
            "全拼 weix 应匹配微信"
        );
    }
}
