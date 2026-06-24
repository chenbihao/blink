//! BuiltinEngine: Blink 内置动作引擎（sync lane）。
//!
//! 内置动作不经过插件进程，直接在 core 内执行，响应速度最快。
//! 包含：设置、锁屏、关机/重启/睡眠、清空历史等系统操作。

use super::engine::{QueryContext, SearchAction, SearchEngine, SearchItem};

/// 内置动作定义。
struct BuiltinAction {
    /// 唯一标识
    id: &'static str,
    /// 主显示标题
    title: &'static str,
    /// 副标题/说明
    subtitle: &'static str,
    /// 匹配关键词（拼音首字母也会自动匹配，如 "设置" → "sz"）
    keywords: &'static [&'static str],
    /// 动作类型
    kind: ActionKind,
}

/// 动作类型枚举（避免 Box<dyn FnOnce>，纯数据驱动）。
enum ActionKind {
    /// 打开设置窗口
    OpenSettings,
    /// 锁定工作站
    LockWorkstation,
    /// 关机
    Shutdown,
    /// 重启
    Restart,
    /// 睡眠
    Sleep,
    /// 清空搜索历史
    ClearHistory,
}

/// 内置动作注册表。
///
/// 新增动作只需在这里添加条目，无需修改其他代码。
const ACTIONS: &[BuiltinAction] = &[
    BuiltinAction {
        id: "open_settings",
        title: "打开设置",
        subtitle: "Blink 偏好设置",
        keywords: &["设置", "settings", "sz", "偏好", "配置"],
        kind: ActionKind::OpenSettings,
    },
    BuiltinAction {
        id: "lock",
        title: "锁定电脑",
        subtitle: "Lock Workstation",
        keywords: &["锁定", "lock", "锁屏", "sd"],
        kind: ActionKind::LockWorkstation,
    },
    BuiltinAction {
        id: "shutdown",
        title: "关机",
        subtitle: "Shutdown",
        keywords: &["关机", "shutdown", "gj"],
        kind: ActionKind::Shutdown,
    },
    BuiltinAction {
        id: "restart",
        title: "重启",
        subtitle: "Restart",
        keywords: &["重启", "restart", "cq"],
        kind: ActionKind::Restart,
    },
    BuiltinAction {
        id: "sleep",
        title: "睡眠",
        subtitle: "Sleep",
        keywords: &["睡眠", "sleep", "sm"],
        kind: ActionKind::Sleep,
    },
    BuiltinAction {
        id: "clear_history",
        title: "清空搜索历史",
        subtitle: "清除所有应用启动记录",
        keywords: &["清空历史", "clear history", "qkls", "清除历史"],
        kind: ActionKind::ClearHistory,
    },
];

/// 内置动作引擎。
pub struct BuiltinEngine;

#[async_trait::async_trait]
impl SearchEngine for BuiltinEngine {
    fn id(&self) -> &'static str {
        "builtin"
    }

    fn lane(&self) -> super::engine::Lane {
        super::engine::Lane::Sync
    }

    /// 搜索内置动作：query 匹配关键词 → 返回结果。
    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let q = query.trim().to_lowercase();
        if q.is_empty() {
            return Vec::new();
        }

        let mut items = Vec::new();
        for action in ACTIONS {
            // 1. 精确匹配 title 或 keyword
            let mut matched = false;
            let mut score = 0.0f32;

            // 标题精确匹配（最高优先级）
            if action.title.to_lowercase().contains(&q) {
                matched = true;
                score = 1.0;
            }

            // 关键词匹配（包含首字母）
            if !matched {
                for kw in action.keywords {
                    let kw_lower = kw.to_lowercase();
                    // 精确匹配关键词
                    if kw_lower == q {
                        matched = true;
                        score = 0.9;
                        break;
                    }
                    // 前缀匹配（如 "设" 匹配 "设置"）
                    if kw_lower.starts_with(&q) {
                        matched = true;
                        score = 0.7;
                        break;
                    }
                }
            }

            if matched {
                items.push(action_to_search_item(action, score));
            }
        }

        items
    }
}

/// BuiltinAction → SearchItem 转换。
fn action_to_search_item(action: &BuiltinAction, score: f32) -> SearchItem {
    let action_kind = match action.kind {
        // 使用 Open path 作为动作标识（虽然不是真的打开文件，但前端对 Open 有完整支持）
        // path 为特殊标识，前端 commands::launch_app 识别后执行对应动作
        ActionKind::OpenSettings => SearchAction::Open {
            path: "__BLINK_ACTION_OPEN_SETTINGS__".to_string(),
        },
        ActionKind::LockWorkstation => SearchAction::Open {
            path: "__BLINK_ACTION_LOCK__".to_string(),
        },
        ActionKind::Shutdown => SearchAction::Open {
            path: "__BLINK_ACTION_SHUTDOWN__".to_string(),
        },
        ActionKind::Restart => SearchAction::Open {
            path: "__BLINK_ACTION_RESTART__".to_string(),
        },
        ActionKind::Sleep => SearchAction::Open {
            path: "__BLINK_ACTION_SLEEP__".to_string(),
        },
        ActionKind::ClearHistory => SearchAction::Open {
            path: "__BLINK_ACTION_CLEAR_HISTORY__".to_string(),
        },
    };

    SearchItem {
        id: format!("builtin:{}", action.id),
        title: action.title.to_string(),
        subtitle: Some(action.subtitle.to_string()),
        score,
        action: action_kind,
        source: "builtin".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn search_settings() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
        };

        let items = tauri::async_runtime::block_on(engine.search("设置", &ctx));
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "打开设置");
        assert!(items[0].score > 0.0);
    }

    #[test]
    fn search_lock() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
        };

        let items = tauri::async_runtime::block_on(engine.search("锁定", &ctx));
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "锁定电脑");
    }

    #[test]
    fn search_pinyin_initial() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
        };

        // 首字母 "sz" 匹配 "设置"
        let items = tauri::async_runtime::block_on(engine.search("sz", &ctx));
        assert!(!items.is_empty());
        assert_eq!(items[0].title, "打开设置");
    }

    #[test]
    fn search_no_match() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
        };

        let items = tauri::async_runtime::block_on(engine.search("xyzabc123", &ctx));
        assert!(items.is_empty());
    }

    #[test]
    fn search_empty_query() {
        let engine = BuiltinEngine;
        let history = HashMap::new();
        let snapshot = crate::context::ContextSnapshot::default();
        let ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
        };

        let items = tauri::async_runtime::block_on(engine.search("", &ctx));
        assert!(items.is_empty());
    }
}
