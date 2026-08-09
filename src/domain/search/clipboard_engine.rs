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
use crate::infra::data::clipboard_images::{ClipboardImageMeta, query_recent_images};

/// Engine id — 对应 `SearchEngine::id()` 与 `Route::EngineTakeover.engine_id`。
pub const ENGINE_ID: &str = "clipboard";

/// 触发关键词。原文匹配（Exact / Prefix + 空格）算命中——
/// engine 场景不做首拼弱信号派生（本体数据信号天然强，"剪贴板" 不会误命中）。
pub const TRIGGERS: &[&str] = &["剪贴板", "clip", "jtb", "jiantieban"];

/// 单次展示条数默认值（与 `ClipboardConfig::display_count` 默认值一致）。
const DEFAULT_DISPLAY_COUNT: usize = 30;
/// 单次展示条数下限/上限。下限 1 防"啥也不显示"，上限 200 防一次拉太多拖慢 UI。
const DISPLAY_COUNT_MIN: usize = 1;
const DISPLAY_COUNT_MAX: usize = 200;

/// 副行预览截断字符数（避免长文本挤爆 UI；前端本身也会截）。
const PREVIEW_MAX_CHARS: usize = 60;

pub struct ClipboardEngine {
    pool: SqlitePool,
    /// cache 库——clipboard_images 表所在（0.16.4 图片历史）。
    cache_pool: SqlitePool,
    /// UI 语言快照,用于 subtitle 时间描述 zh/en 切换（0.8.5.1 §6.6）。
    /// 与 SearchService.language 联动:`SearchService::update_language` 转发到本 engine。
    language: Arc<RwLock<String>>,
    /// 单次展示条数快照（`display_count` 配置项）。
    /// 与 `SearchService` 联动：`set_config("clipboard_config")` 时 downcast 转发到本 engine。
    display_count: Arc<RwLock<usize>>,
}

impl ClipboardEngine {
    pub fn new(pool: SqlitePool, cache_pool: SqlitePool) -> Self {
        Self {
            pool,
            cache_pool,
            language: Arc::new(RwLock::new("zh".to_string())),
            display_count: Arc::new(RwLock::new(DEFAULT_DISPLAY_COUNT)),
        }
    }

    /// 更新 UI 语言快照（SearchService::update_language 转发）。
    pub fn update_language(&self, lang: String) {
        *self.language.write().unwrap() = lang;
    }

    /// 更新单次展示条数（设置页 `clipboard_config` 保存时转发）。
    /// 自动 clamp 到 `[DISPLAY_COUNT_MIN, DISPLAY_COUNT_MAX]`，非法值兜底默认值。
    pub fn update_display_count(&self, count: u32) {
        let clamped = if (DISPLAY_COUNT_MIN..=DISPLAY_COUNT_MAX).contains(&(count as usize)) {
            count as usize
        } else {
            DEFAULT_DISPLAY_COUNT
        };
        *self.display_count.write().unwrap() = clamped;
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
        let limit = *self.display_count.read().unwrap() as i64;
        let lang = self.language.read().unwrap().clone();

        // 查文本历史
        let text_items = if arg.is_empty() {
            query_recent(&self.pool, limit).await
        } else {
            search_history(&self.pool, arg, limit).await
        };

        // 查图片历史（0.16.4）——空 arg 拉最近；非空 arg 对 source_app 和稳定标题做包含匹配
        let image_items = if arg.is_empty() {
            query_recent_images(&self.cache_pool, limit).await
        } else {
            // 对图片的 source_app、source_path 和稳定标题做包含匹配
            let all_images = query_recent_images(&self.cache_pool, 200).await;
            let arg_lower = arg.to_lowercase();
            all_images
                .into_iter()
                .filter(|meta| {
                    let title = format!("图片 {}x{}", meta.width, meta.height);
                    let haystack = match &meta.source_app {
                        Some(app) => format!(
                            "{} {} {}",
                            title,
                            app,
                            meta.source_path.as_deref().unwrap_or("")
                        ),
                        None => format!("{} {}", title, meta.source_path.as_deref().unwrap_or("")),
                    };
                    haystack.to_lowercase().contains(&arg_lower)
                })
                .take(limit as usize)
                .collect()
        };

        // 合并文本 + 图片，按 created_at 倒序
        let mut combined: Vec<(i64, ClipboardEntry)> = Vec::new();
        for item in text_items {
            combined.push((item.created_at, ClipboardEntry::Text(item)));
        }
        for item in image_items {
            combined.push((item.created_at, ClipboardEntry::Image(item)));
        }
        combined.sort_by(|a, b| b.0.cmp(&a.0));

        combined
            .into_iter()
            .enumerate()
            .map(|(i, (_, entry))| match entry {
                ClipboardEntry::Text(item) => to_search_item(item, i, &lang),
                ClipboardEntry::Image(meta) => to_image_search_item(meta, i, &lang),
            })
            .take(limit as usize)
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

/// 文本/图片条目的统一容器（合并排序用）。
enum ClipboardEntry {
    Text(ClipboardItem),
    Image(ClipboardImageMeta),
}

/// ClipboardImageMeta → SearchItem（0.16.4）。
///
/// - `title` = "图片 {W}x{H} ({source_desc})"
///   - 0.17.9：source_desc 拼接逻辑——有 source_path 时「文件名 · 应用名」，无则仅应用名
///   - 自写入标签（`blink:*` key）经 `resolve_source_desc` 转中英文文案
/// - `subtitle` 时间描述
/// - `action` = `RunAction { id: "copy_clipboard_image", arg: image_id }`
///   前端 actions.js 识别此 id，调 `copy_clipboard_image` 后端命令写回系统剪贴板
/// - `source` = "clipboard"（与文本项同 source，前端白名单已含）
fn to_image_search_item(meta: ClipboardImageMeta, index: usize, lang: &str) -> SearchItem {
    let is_zh = lang == "zh";
    let app_desc = resolve_source_desc(meta.source_app.as_deref(), is_zh);
    let source_desc = match &meta.source_path {
        Some(path) if !path.is_empty() => format!("{path} · {app_desc}"),
        _ => app_desc,
    };
    let title = if is_zh {
        format!("图片 {}x{} ({})", meta.width, meta.height, source_desc)
    } else {
        format!("Image {}x{} ({})", meta.width, meta.height, source_desc)
    };
    let subtitle = format_image_subtitle(&meta, lang);
    let score = (0.9 - index as f32 * 0.02).max(0.5);

    SearchItem {
        id: format!("clipboard:{}", meta.id),
        title,
        subtitle: Some(subtitle),
        score,
        action: SearchAction::RunAction {
            id: "copy_clipboard_image".into(),
            arg: Some(serde_json::Value::String(meta.id.clone())),
        },
        source: "clipboard".into(),
        score_detail: Some(format!("clipimg=0.9-{:.2}", index as f32 * 0.02)),
        context_aware: false,
    }
}

/// 0.17.9：将 `source_app` 字段解析为展示文案。
///
/// 自写入标签（`blink:screenshot` / `blink:repost` / `blink:app` / `blink:ai`）转中英文文案；
/// 进程名（如 `chrome.exe`）原样返回；None/空 →「未知」/「unknown」。
///
/// **不改 DB schema、不改前端契约**——映射纯在展示层完成。
fn resolve_source_desc(source_app: Option<&str>, is_zh: bool) -> String {
    match source_app {
        Some(s) if s == "blink:screenshot" => {
            if is_zh {
                "截图".to_string()
            } else {
                "Screenshot".to_string()
            }
        }
        Some(s) if s == "blink:repost" => {
            // 历史回贴 skip_persist=true 不会入库，此处仅防御
            if is_zh {
                "回贴".to_string()
            } else {
                "Repost".to_string()
            }
        }
        Some(s) if s == "blink:app" => "Blink".to_string(),
        Some(s) if s == "blink:ai" => {
            if is_zh {
                "AI".to_string()
            } else {
                "AI".to_string()
            }
        }
        Some(s) if !s.is_empty() => s.to_string(),
        _ => {
            if is_zh {
                "未知".to_string()
            } else {
                "unknown".to_string()
            }
        }
    }
}
fn format_image_subtitle(meta: &ClipboardImageMeta, lang: &str) -> String {
    let now = chrono::Utc::now().timestamp();
    let elapsed = (now - meta.created_at).max(0);
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
    // P3-#27 fix: zh/en 分支格式完全一样，消除死代码 if
    format!("{time_desc} · {}x{}", meta.width, meta.height)
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

    #[test]
    fn display_count_defaults_to_30() {
        // 无 pool 也能构造——display_count 是内存快照，构造时不读 DB。
        // connect_lazy 需要 Tokio runtime，测试里用阻塞 runtime 兜底。
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        assert_eq!(*engine.display_count.read().unwrap(), DEFAULT_DISPLAY_COUNT);
        assert_eq!(DEFAULT_DISPLAY_COUNT, 30);
    }

    #[test]
    fn update_display_count_accepts_valid_range() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        engine.update_display_count(5);
        assert_eq!(*engine.display_count.read().unwrap(), 5);
        engine.update_display_count(200);
        assert_eq!(*engine.display_count.read().unwrap(), 200);
    }

    #[test]
    fn update_display_count_clamps_out_of_range_to_default() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        // 0 = 下限外 → 兜底默认值
        engine.update_display_count(0);
        assert_eq!(*engine.display_count.read().unwrap(), DEFAULT_DISPLAY_COUNT);
        // 超上限 → 兜底默认值
        engine.update_display_count(9999);
        assert_eq!(*engine.display_count.read().unwrap(), DEFAULT_DISPLAY_COUNT);
    }

    // ── 0.17.9 resolve_source_desc 单测 ──────────────────────────────────

    #[test]
    fn resolve_source_desc_screenshot_zh() {
        assert_eq!(resolve_source_desc(Some("blink:screenshot"), true), "截图");
    }

    #[test]
    fn resolve_source_desc_screenshot_en() {
        assert_eq!(
            resolve_source_desc(Some("blink:screenshot"), false),
            "Screenshot"
        );
    }

    #[test]
    fn resolve_source_desc_repost_zh() {
        assert_eq!(resolve_source_desc(Some("blink:repost"), true), "回贴");
    }

    #[test]
    fn resolve_source_desc_ai_zh() {
        assert_eq!(resolve_source_desc(Some("blink:ai"), true), "AI");
    }

    #[test]
    fn resolve_source_desc_app_is_not_misattributed_to_ai() {
        assert_eq!(resolve_source_desc(Some("blink:app"), true), "Blink");
        assert_eq!(resolve_source_desc(Some("blink:app"), false), "Blink");
    }

    #[test]
    fn resolve_source_desc_ai_en() {
        assert_eq!(resolve_source_desc(Some("blink:ai"), false), "AI");
    }

    #[test]
    fn resolve_source_desc_process_name_passthrough() {
        // 进程名原样返回
        assert_eq!(resolve_source_desc(Some("chrome.exe"), true), "chrome.exe");
        assert_eq!(
            resolve_source_desc(Some("explorer.exe"), false),
            "explorer.exe"
        );
    }

    #[test]
    fn resolve_source_desc_none_returns_unknown() {
        assert_eq!(resolve_source_desc(None, true), "未知");
        assert_eq!(resolve_source_desc(None, false), "unknown");
    }

    #[test]
    fn resolve_source_desc_empty_string_returns_unknown() {
        assert_eq!(resolve_source_desc(Some(""), true), "未知");
        assert_eq!(resolve_source_desc(Some(""), false), "unknown");
    }

    #[test]
    fn to_image_search_item_with_source_path() {
        let meta = ClipboardImageMeta {
            id: "img_test".into(),
            thumb_blob: vec![1, 2, 3],
            width: 1920,
            height: 1080,
            created_at: chrono::Utc::now().timestamp(),
            source_app: Some("explorer.exe".into()),
            source_path: Some("photo.jpg".into()),
        };
        let si = to_image_search_item(meta, 0, "zh");
        assert!(
            si.title.contains("photo.jpg · explorer.exe"),
            "标题应含文件名 · 应用名: {}",
            si.title
        );
    }

    #[test]
    fn to_image_search_item_without_source_path() {
        let meta = ClipboardImageMeta {
            id: "img_test2".into(),
            thumb_blob: vec![1, 2, 3],
            width: 800,
            height: 600,
            created_at: chrono::Utc::now().timestamp(),
            source_app: Some("chrome.exe".into()),
            source_path: None,
        };
        let si = to_image_search_item(meta, 0, "zh");
        assert!(
            si.title.contains("chrome.exe"),
            "标题应含应用名: {}",
            si.title
        );
        assert!(
            !si.title.contains("·"),
            "无文件名不应有 · 分隔符: {}",
            si.title
        );
    }

    #[test]
    fn to_image_search_item_with_screenshot_label() {
        let meta = ClipboardImageMeta {
            id: "img_test3".into(),
            thumb_blob: vec![1, 2, 3],
            width: 2560,
            height: 1440,
            created_at: chrono::Utc::now().timestamp(),
            source_app: Some("blink:screenshot".into()),
            source_path: None,
        };
        let si = to_image_search_item(meta, 0, "zh");
        assert!(
            si.title.contains("截图"),
            "blink:screenshot 应解析为「截图」: {}",
            si.title
        );
    }
}
