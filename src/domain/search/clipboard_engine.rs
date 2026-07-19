//! ClipboardEngine：剪贴板历史入口（0.8.5 §6.4，sync lane，EngineTakeover 目标）。
//!
//! **触发路径**：intent 侧 `RuleRouter::route` 命中 engine keyword（剪贴板/clip/jtb/jiantieban）
//! → 产 `Route::EngineTakeover { engine_id: "clipboard", arg }` → SearchService 分派到本 engine。
//! 传入的 query 已经是**剥离 keyword 后的参数**（如 `"剪贴板 hello"` → engine 收到 `"hello"`）。
//!
//! **engine 只负责按 arg 展开**：
//! - 空 arg → `query_recent(pool, 9)` top-N（对齐 Alt+1~9）
//! - 非空 arg → `search_history(pool, arg, 9)` fuzzy 搜索
//! - 每条 → `SearchAction::Copy { text, hit_id: Some(id) }`，激活时前端复制 + 回写命中
//!
//! **不做 keyword 检测**：完全由 route 层判定。engine 收到什么参数就按什么展开——这样：
//! - engine 边界清爽（关键字派发在 intent 域，engine 只做数据召回）
//! - 未来 AI/云端 Router 想复用 ClipboardEngine 时无需绕开硬编码触发词
//!
//! **为何独立 Engine 而非 BuiltinAction 或 builtin 插件**：
//! - BuiltinAction 是"一 action ↔ 一 SearchItem"心智，不适合"展开 top-N"
//! - 做成 builtin 插件要绕 subprocess JSONL IPC——本体自家 sqlite 数据没必要
//! - 独立 sync engine 与 CalcEngine 心智一致（keyword-triggered、一次展开若干条）
//!
//! **触发词硬编码**：与 BuiltinAction keyword 沿用同一策略（0.8.1 决定 keyword 硬编码到 0.9）。
//!
//! **id 契约**：`clipboard:{item.id}` 全局去重，避免与 calc:/plugin: 冲突。
//! **source**：`"clipboard"`——前端 results.js `pluginsReturned` 白名单已加此值。

use std::sync::{Arc, RwLock};

use sqlx::SqlitePool;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use crate::infra::data::clipboard::{ClipboardItem, query_recent, search as search_history};

/// Engine id — 对应 `SearchEngine::id()` 与 `Route::EngineTakeover.engine_id`。
pub const ENGINE_ID: &str = "clipboard";

/// 触发关键词。原文匹配（Exact / Prefix + 空格）算命中——
/// engine 场景不做首拼弱信号派生（本体数据信号天然强，"剪贴板" 不会误命中）。
pub const TRIGGERS: &[&str] = &["剪贴板", "clip", "jtb", "jiantieban"];

/// 展开条数——对齐 Alt+1~9 单页容量，一屏一次到位。
const RESULT_LIMIT: usize = 9;

/// 副行预览截断字符数（避免长文本挤爆 UI；前端本身也会截）。
const PREVIEW_MAX_CHARS: usize = 60;

pub struct ClipboardEngine {
    pool: SqlitePool,
    /// UI 语言快照,用于 subtitle 时间描述 zh/en 切换（0.8.5.1 §6.6）。
    /// 与 SearchService.language 联动:`SearchService::update_language` 转发到本 engine。
    language: Arc<RwLock<String>>,
}

impl ClipboardEngine {
    pub fn new(pool: SqlitePool) -> Self {
        Self {
            pool,
            language: Arc::new(RwLock::new("zh".to_string())),
        }
    }

    /// 更新 UI 语言快照（SearchService::update_language 转发）。
    pub fn update_language(&self, lang: String) {
        *self.language.write().unwrap() = lang;
    }
}

#[async_trait::async_trait]
impl SearchEngine for ClipboardEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// 只在 EngineTakeover 分派下工作——Mixed 分支跳过（0.8.5 §6.4 修正）。
    /// 否则任意非 keyword 输入都会 fuzzy 搜整个剪贴板历史，污染常规查询结果。
    fn takeover_only(&self) -> bool {
        true
    }

    /// query 是**已剥离触发词后的参数**（由 intent 层负责剥离）。
    /// - 空串 → 拉最近 top-N
    /// - 非空 → fuzzy search
    ///
    /// **不再检测 keyword**：0.8.5 §6.4 后此 engine 只在 EngineTakeover 路径被调用；
    /// route 层保证 Mixed 分支不会调到本 engine。
    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let arg = query.trim();
        let items = if arg.is_empty() {
            query_recent(&self.pool, RESULT_LIMIT as i64).await
        } else {
            search_history(&self.pool, arg, RESULT_LIMIT as i64).await
        };
        let lang = self.language.read().unwrap().clone();

        items
            .into_iter()
            .enumerate()
            .map(|(i, item)| to_search_item(item, i, &lang))
            .collect()
    }
}

/// ClipboardItem → SearchItem。
///
/// - `title` 用 preview 的截断版（原文含换行则去掉），主行清爽
/// - `subtitle` "3 分钟前 · N chars"（副行时间提示）
/// - `score` 基线 0.9 起按名次递减 0.02，确保后端排序稳定（前端仍会按 score sort）
/// - `hit_id = Some(item.id)` → 前端复制成功后回写 `record_clipboard_hit`
fn to_search_item(item: ClipboardItem, index: usize, lang: &str) -> SearchItem {
    let title = preview_line(&item.preview);
    let subtitle = format_subtitle(&item, lang);
    let score = (0.9 - index as f32 * 0.02).max(0.5);

    SearchItem {
        id: format!("clipboard:{}", item.id),
        title,
        subtitle: Some(subtitle),
        score,
        action: SearchAction::Copy {
            text: item.text,
            hit_id: Some(item.id),
        },
        source: "clipboard".into(),
        score_detail: Some(format!("clip=0.9-{:.2}", index as f32 * 0.02)),
        context_aware: false,
    }
}

/// 生成单行预览：去换行 + 压缩连续空白 + 字符数截断。
fn preview_line(raw: &str) -> String {
    let flat: String = raw
        .chars()
        .map(|c| {
            if c.is_control() || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect();
    // 压缩连续空白
    let mut out = String::with_capacity(flat.len());
    let mut prev_space = false;
    for c in flat.chars() {
        if c == ' ' {
            if !prev_space {
                out.push(' ');
            }
            prev_space = true;
        } else {
            out.push(c);
            prev_space = false;
        }
    }
    let out = out.trim();
    if out.chars().count() <= PREVIEW_MAX_CHARS {
        out.to_string()
    } else {
        let truncated: String = out.chars().take(PREVIEW_MAX_CHARS).collect();
        format!("{}…", truncated)
    }
}

/// 生成副行"N 分钟前 · M chars" / "N min ago · M chars"（0.8.5.1 §6.6 i18n）。
///
/// zh/en 双语。lang 传入 "zh" 或 "en"（其他视作 en fallback）。
/// "chars" 单位跨语言保留——ASCII 字数国际化收益低,且 CJK 字符计数概念相通。
fn format_subtitle(item: &ClipboardItem, lang: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let elapsed = (now - item.created_at).max(0);
    let is_zh = lang == "zh";
    let time_desc = if elapsed < 60 {
        if is_zh {
            "刚刚".to_string()
        } else {
            "just now".to_string()
        }
    } else if elapsed < 3600 {
        let n = elapsed / 60;
        if is_zh {
            format!("{n} 分钟前")
        } else {
            format!("{n} min ago")
        }
    } else if elapsed < 86400 {
        let n = elapsed / 3600;
        if is_zh {
            format!("{n} 小时前")
        } else {
            format!("{n} h ago")
        }
    } else {
        let n = elapsed / 86400;
        if is_zh {
            format!("{n} 天前")
        } else {
            format!("{n} d ago")
        }
    };
    let char_count = item.text.chars().count();
    format!("{time_desc} · {char_count} chars")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_line_flattens_newlines() {
        assert_eq!(preview_line("hello\nworld"), "hello world");
        assert_eq!(preview_line("  a\r\n  b  "), "a b");
    }

    #[test]
    fn preview_line_truncates_long() {
        let long: String = "a".repeat(100);
        let out = preview_line(&long);
        assert!(out.ends_with('…'));
        assert_eq!(out.chars().count(), PREVIEW_MAX_CHARS + 1); // +1 for …
    }

    #[test]
    fn to_search_item_carries_hit_id() {
        let item = ClipboardItem {
            id: "clip_123".into(),
            text: "hello".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp(),
            source_app: None,
            hit_count: 0,
        };
        let si = to_search_item(item, 0, "zh");
        assert_eq!(si.id, "clipboard:clip_123");
        assert_eq!(si.source, "clipboard");
        if let SearchAction::Copy { text, hit_id } = &si.action {
            assert_eq!(text, "hello");
            assert_eq!(hit_id.as_deref(), Some("clip_123"));
        } else {
            panic!("expected Copy action");
        }
        assert!((si.score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn score_decreases_with_index() {
        let mk = |i: usize| {
            let item = ClipboardItem {
                id: format!("id_{i}"),
                text: "x".into(),
                preview: "x".into(),
                created_at: 0,
                source_app: None,
                hit_count: 0,
            };
            to_search_item(item, i, "zh").score
        };
        assert!(mk(0) > mk(1));
        assert!(mk(1) > mk(2));
        // 下限保护
        assert!(mk(100) >= 0.5);
    }

    #[test]
    fn format_subtitle_recent_zh() {
        let item = ClipboardItem {
            id: "x".into(),
            text: "hello".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp() - 30,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&item, "zh").starts_with("刚刚"));
        assert!(format_subtitle(&item, "zh").ends_with("5 chars"));
    }

    #[test]
    fn format_subtitle_recent_en() {
        let item = ClipboardItem {
            id: "x".into(),
            text: "hello".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp() - 30,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&item, "en").starts_with("just now"));
        assert!(format_subtitle(&item, "en").ends_with("5 chars"));
    }

    #[test]
    fn format_subtitle_minutes_zh() {
        let item = ClipboardItem {
            id: "x".into(),
            text: "ab".into(),
            preview: "ab".into(),
            created_at: chrono::Utc::now().timestamp() - 300,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&item, "zh").contains("分钟前"));
    }

    #[test]
    fn format_subtitle_minutes_en() {
        let item = ClipboardItem {
            id: "x".into(),
            text: "ab".into(),
            preview: "ab".into(),
            created_at: chrono::Utc::now().timestamp() - 300,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&item, "en").contains("min ago"));
    }
}
