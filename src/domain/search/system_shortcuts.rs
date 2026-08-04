//! 系统快捷方式搜索源（0.17.1 §3.7）。
//!
//! 硬编码 ~10 项常用系统快捷方式（回收站、控制面板、环境变量等），
//! 作为内置 SearchItem 注入搜索引擎。不扫描注册表——系统快捷方式是有限集合，
//! 不值得复杂度。
//!
//! 执行路径走 `open::that()`（`windows.rs`），天然支持 `shell:::{GUID}`、
//! `shell:RecycleBinFolder`、`.cpl` 文件——执行层零改动。
//!
//! 图标方案：
//! - shell 文件夹（回收站/控制面板/此电脑/网络）：`SHCreateItemFromParsingName` 直接解析
//!   shell 路径提取图标；若失败，`get_icon_png` 内部有 stock icon fallback。
//! - .cpl/.msc/.exe 文件：`extract_icon_png` 从文件提取图标（已有路径）。
//!
//! 拼音匹配：现有搜索双路匹配（原始名 + 拼音首字母）自动覆盖中文名称，
//! `pinyin_name` / `pinyin_full` 在 `to_search_item()` 时生成。

use super::engine::{QueryContext, SearchAction, SearchEngine, SearchItem};
use super::scorer::{history_boost, normalize_top_relative};
use super::{to_pinyin_full, to_pinyin_initials, fuzzy_score_entries, Action, AppEntry};

/// 单项系统快捷方式定义。
struct SystemShortcut {
    /// 中文名称
    name_zh: &'static str,
    /// 英文名称
    name_en: &'static str,
    /// 执行路径（shell:xxx / .cpl / .msc / .exe）
    exec_path: &'static str,
    /// Win32 stock icon ID（SHSTOCKICONID），用于 shell 文件夹图标。
    /// None = 从 exec_path 文件提取图标（.cpl/.msc/.exe 路径）。
    #[allow(dead_code)]
    stock_icon_id: Option<u32>,
}

/// 内置系统快捷方式列表（10 项）。
///
/// stock_icon_id 值来自 SHSTOCKICONID 枚举：
/// - SIID_RECYCLER = 0x1F (31) — 回收站
/// - SIID_CONTROLPANEL = 0x1E (30) — 控制面板
/// - SIID_PC = 0x34 (52) — 此电脑
/// - SIID_NETWORK = 0x1D (29) — 网络
const SHORTCUTS: &[SystemShortcut] = &[
    SystemShortcut {
        name_zh: "回收站",
        name_en: "Recycle Bin",
        exec_path: "shell:RecycleBinFolder",
        stock_icon_id: Some(0x1F),
    },
    SystemShortcut {
        name_zh: "控制面板",
        name_en: "Control Panel",
        exec_path: "shell:ControlPanelFolder",
        stock_icon_id: Some(0x1E),
    },
    SystemShortcut {
        name_zh: "环境变量",
        name_en: "Environment Variables",
        exec_path: "rundll32.exe sysdm.cpl,EditEnvironmentVariables",
        stock_icon_id: None,
    },
    SystemShortcut {
        name_zh: "此电脑",
        name_en: "This PC",
        exec_path: "shell:MyComputerFolder",
        stock_icon_id: Some(0x34),
    },
    SystemShortcut {
        name_zh: "网络",
        name_en: "Network",
        exec_path: "shell:NetworkFolder",
        stock_icon_id: Some(0x1D),
    },
    SystemShortcut {
        name_zh: "程序和功能",
        name_en: "Programs and Features",
        exec_path: "appwiz.cpl",
        stock_icon_id: None,
    },
    SystemShortcut {
        name_zh: "系统属性",
        name_en: "System Properties",
        exec_path: "sysdm.cpl",
        stock_icon_id: None,
    },
    SystemShortcut {
        name_zh: "设备管理器",
        name_en: "Device Manager",
        exec_path: "devmgmt.msc",
        stock_icon_id: None,
    },
    SystemShortcut {
        name_zh: "任务管理器",
        name_en: "Task Manager",
        exec_path: "taskmgr.exe",
        stock_icon_id: None,
    },
    SystemShortcut {
        name_zh: "服务",
        name_en: "Services",
        exec_path: "services.msc",
        stock_icon_id: None,
    },
];

/// 搜索结果上限（融合前截断）。
const ENGINE_LIMIT: usize = 20;

/// 系统快捷方式搜索引擎（sync lane）。
///
/// 无状态、无配置——硬编码列表始终启用。
/// 搜索逻辑与 StartMenuEngine 一致：nucleo fuzzy 双路匹配（名称 + 拼音），
/// top-relative 归一化 + 历史加权。
pub struct SystemShortcutEngine;

#[async_trait::async_trait]
impl SearchEngine for SystemShortcutEngine {
    fn id(&self) -> &'static str {
        "system_shortcut"
    }

    fn lane(&self) -> super::engine::Lane {
        super::engine::Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        if query.is_empty() {
            return Vec::new();
        }

        // 按 QueryContext.language 选择显示名称；language 默认 "zh"
        // 同时把另一语言名称拼入 pinyin 字段，使中英文均可搜索命中（§3.7 验收要求）
        let entries: Vec<AppEntry> = SHORTCUTS
            .iter()
            .map(|s| {
                let (name, other_name) = if ctx.language.starts_with("en") {
                    (s.name_en, s.name_zh)
                } else {
                    (s.name_zh, s.name_en)
                };
                system_shortcut_to_entry(name, other_name, s.exec_path)
            })
            .collect();

        let scored = fuzzy_score_entries(query, &entries, ENGINE_LIMIT);
        normalize_to_items(scored, ctx.history)
    }
}

/// 把 SystemShortcut 转成 AppEntry（用于 fuzzy 打分）。
///
/// `other_name` 是另一语言的名称，拼入 pinyin 字段使中英文均可搜索命中。
/// 例如 zh 模式下 name="回收站", other_name="Recycle Bin"，
/// pinyin_name = "hs recycle bin"，pinyin_full = "huishouzhan recycle bin"。
fn system_shortcut_to_entry(name: &str, other_name: &str, exec_path: &str) -> AppEntry {
    // 把另一语言名称拼入拼音字段，使其也参与 fuzzy 匹配
    let pinyin_name = format!("{} {}", to_pinyin_initials(name), other_name.to_lowercase());
    let pinyin_full = format!("{} {}", to_pinyin_full(name), other_name.to_lowercase());
    AppEntry {
        name: name.to_string(),
        pinyin_name,
        pinyin_full,
        lnk_path: exec_path.to_string(),
        description: Some(exec_path.to_string()),
        actions: vec![Action::default()],
        source: "system".to_string(),
        ..Default::default()
    }
}

/// 把 (raw_score, AppEntry) 列表 top-relative 归一化为 SearchItem。
///
/// 与 StartMenuEngine::normalize_to_items 逻辑一致，
/// source 标记为 "system"。
fn normalize_to_items(
    scored: Vec<(u32, AppEntry)>,
    history: &std::collections::HashMap<String, (i64, i64)>,
) -> Vec<SearchItem> {
    let mut normalized: Vec<(AppEntry, f32)> =
        scored.into_iter().map(|(raw, e)| (e, raw as f32)).collect();
    normalize_top_relative(&mut normalized);

    normalized
        .into_iter()
        .map(|(e, base_score)| {
            let (hit_count, last_used_at) = history.get(&e.lnk_path).copied().unwrap_or((0, 0));
            let hist_boost = history_boost(hit_count, last_used_at);
            let score = base_score + hist_boost;
            let detail = format!("fuzzy={:.2} hist=+{:.2}", base_score, hist_boost);
            SearchItem {
                id: e.lnk_path.clone(),
                title: e.name,
                subtitle: e.description,
                score,
                action: SearchAction::Open { path: e.lnk_path },
                source: "system".into(),
                score_detail: Some(detail),
                context_aware: false,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::platform::context::ContextSnapshot;
    use std::collections::HashMap;

    fn make_ctx<'a>(
        history: &'a HashMap<String, (i64, i64)>,
        snapshot: &'a ContextSnapshot,
    ) -> QueryContext<'a> {
        QueryContext {
            history,
            snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
        }
    }

    #[tokio::test]
    async fn search_recycle_bin_by_chinese() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let results = engine.search("回收站", &ctx).await;
        assert!(results.iter().any(|r| r.title == "回收站"));
    }

    #[tokio::test]
    async fn search_recycle_bin_by_pinyin() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        // 拼音首字母 "hs" 应匹配 "回收站"
        let results = engine.search("hs", &ctx).await;
        assert!(results.iter().any(|r| r.title == "回收站"));
    }

    #[tokio::test]
    async fn search_control_panel_by_english() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let results = engine.search("control", &ctx).await;
        assert!(
            results.iter().any(|r| r.title == "控制面板" || r.title == "Control Panel"),
            "应能搜到控制面板"
        );
    }

    #[tokio::test]
    async fn search_task_manager() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let results = engine.search("任务管理器", &ctx).await;
        assert!(results.iter().any(|r| r.title == "任务管理器"));
    }

    #[tokio::test]
    async fn search_empty_query_returns_nothing() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let results = engine.search("", &ctx).await;
        assert!(results.is_empty());
    }

    #[tokio::test]
    async fn search_no_match_returns_empty() {
        let engine = SystemShortcutEngine;
        let history = HashMap::new();
        let snapshot = ContextSnapshot::default();
        let ctx = make_ctx(&history, &snapshot);
        let results = engine.search("zzzzz", &ctx).await;
        assert!(results.is_empty());
    }

    #[test]
    fn all_shortcuts_have_exec_path() {
        for s in SHORTCUTS {
            assert!(!s.exec_path.is_empty(), "exec_path 不能为空");
        }
    }

    #[test]
    fn shortcut_count_is_ten() {
        assert_eq!(SHORTCUTS.len(), 10, "应有 10 项系统快捷方式");
    }
}
