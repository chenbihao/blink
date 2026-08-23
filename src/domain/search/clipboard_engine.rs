//! ClipboardEngine：剪贴板历史入口（0.8.5 §6.4，sync lane，EngineTakeover 目标）。
//!
//! **触发路径**：intent 侧 `RuleRouter::route` 命中 engine keyword（剪贴板/clip/jtb/jiantieban）
//! → 产 `Route::EngineTakeover { engine_id: "clipboard", arg }` → SearchService 分派到本 engine。
//! 传入的 query 已经是**剥离 keyword 后的参数**（如 `"剪贴板 hello"` → engine 收到 `"hello"`）。
//!
//! **engine 只负责按 arg 展开**：
//! - 空 arg → `query_recent_meta(pool, 9)` top-N 元数据（对齐 Alt+1~9）
//! - 非空 arg → `query_recent_days_meta(pool, 30, 500)` + nucleo fuzzy 匹配 preview
//! - 每条 → `SearchAction::LazyCopy { hit_id }`，搜索路径不预载 text，激活时前端按需拉取
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
use crate::infra::data::clipboard::{ClipboardMeta, query_recent_days_meta, query_recent_meta};
use crate::infra::data::clipboard_images::{
    ClipboardImageListItem, query_recent_image_list, search_image_list,
};

/// Engine id — 对应 `SearchEngine::id()` 与 `Route::EngineTakeover.engine_id`。
pub const ENGINE_ID: &str = "clipboard";

/// 触发关键词。原文匹配（Exact / Prefix + 空格）算命中——
/// engine 场景不做首拼弱信号派生（本体数据信号天然强，"剪贴板" 不会误命中）。
pub const TRIGGERS: &[&str] = &["剪贴板", "clip", "jtb", "jiantieban"];

/// display_pages 默认值（与 ClipboardConfig::display_pages 默认值一致 = 3）。
const DEFAULT_DISPLAY_PAGES: usize = 3;
/// display_pages 范围下限/上限。
const DISPLAY_PAGES_MIN: usize = 1;
const DISPLAY_PAGES_MAX: usize = 20;
/// 默认 page_size（与 SearchConfig::page_size 默认值一致）。
const DEFAULT_PAGE_SIZE: usize = 9;
/// effective_limit 下限保护：至少返回 1 条。
const EFFECTIVE_LIMIT_MIN: usize = 1;
/// effective_limit 上限保护：防一次拉太多拖慢 UI。
const EFFECTIVE_LIMIT_MAX: usize = 400;

/// 搜索候选池上限默认值（与 `ClipboardConfig::candidate_limit` 默认值一致）。
const DEFAULT_CANDIDATE_LIMIT: usize = 500;
/// 搜索候选池下限/上限。下限 50 防几乎无结果，上限 5000 防内存/IO 过载。
const CANDIDATE_LIMIT_MIN: usize = 50;
const CANDIDATE_LIMIT_MAX: usize = 5000;

/// 副行预览截断字符数（避免长文本挤爆 UI；前端本身也会截）。
const PREVIEW_MAX_CHARS: usize = 60;

pub struct ClipboardEngine {
    pool: SqlitePool,
    /// cache 库——clipboard_images 表所在（0.16.4 图片历史）。
    cache_pool: SqlitePool,
    /// UI 语言快照,用于 subtitle 时间描述 zh/en 切换（0.8.5.1 §6.6）。
    /// 与 SearchService.language 联动:`SearchService::update_language` 转发到本 engine。
    language: Arc<RwLock<String>>,
    /// 剪贴板模式一次加载几页（`display_pages` 配置项）。
    /// 与 `SearchService` 联动：`set_config("clipboard_config")` 时 downcast 转发到本 engine。
    display_pages: Arc<RwLock<usize>>,
    /// 搜索结果每页条数快照（`page_size`，来自 SearchConfig）。
    /// effective_limit = display_pages × page_size。
    page_size: Arc<RwLock<usize>>,
    /// 搜索候选池上限快照（`candidate_limit` 配置项）。
    /// 控制搜索时拉多少条元数据做 fuzzy 匹配。默认 500。
    candidate_limit: Arc<RwLock<usize>>,
    /// 搜索保留天数；0 表示不按时间过滤。
    retention_days: Arc<RwLock<u32>>,
}

impl ClipboardEngine {
    pub fn new(pool: SqlitePool, cache_pool: SqlitePool) -> Self {
        Self {
            pool,
            cache_pool,
            language: Arc::new(RwLock::new("zh".to_string())),
            display_pages: Arc::new(RwLock::new(DEFAULT_DISPLAY_PAGES)),
            page_size: Arc::new(RwLock::new(DEFAULT_PAGE_SIZE)),
            candidate_limit: Arc::new(RwLock::new(DEFAULT_CANDIDATE_LIMIT)),
            retention_days: Arc::new(RwLock::new(30)),
        }
    }

    /// 更新 UI 语言快照（SearchService::update_language 转发）。
    pub fn update_language(&self, lang: String) {
        *self.language.write().unwrap() = lang;
    }

    /// 更新剪贴板模式加载页数（设置页 `clipboard_config` 保存时转发）。
    /// 自动 clamp 到 `[DISPLAY_PAGES_MIN, DISPLAY_PAGES_MAX]`，非法值兜底默认值。
    pub fn update_display_pages(&self, pages: u32) {
        let clamped = if (DISPLAY_PAGES_MIN..=DISPLAY_PAGES_MAX).contains(&(pages as usize)) {
            pages as usize
        } else {
            DEFAULT_DISPLAY_PAGES
        };
        *self.display_pages.write().unwrap() = clamped;
    }

    /// 更新搜索结果每页条数（`SearchConfig::page_size` 变化时转发）。
    /// effective_limit = display_pages × page_size。
    pub fn update_page_size(&self, page_size: u32) {
        let clamped = if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size as usize
        };
        *self.page_size.write().unwrap() = clamped;
    }

    /// 计算 effective limit = display_pages × page_size，钳制到 [1, 400]。
    fn effective_limit(&self) -> usize {
        let pages = *self.display_pages.read().unwrap();
        let ps = *self.page_size.read().unwrap();
        let limit = pages.saturating_mul(ps);
        limit.clamp(EFFECTIVE_LIMIT_MIN, EFFECTIVE_LIMIT_MAX)
    }

    /// 更新搜索候选池上限（设置页 `clipboard_config` 保存时转发）。
    /// 自动 clamp 到 `[CANDIDATE_LIMIT_MIN, CANDIDATE_LIMIT_MAX]`，非法值兜底默认值。
    pub fn update_candidate_limit(&self, limit: u32) {
        let clamped = if (CANDIDATE_LIMIT_MIN..=CANDIDATE_LIMIT_MAX).contains(&(limit as usize)) {
            limit as usize
        } else {
            DEFAULT_CANDIDATE_LIMIT
        };
        *self.candidate_limit.write().unwrap() = clamped;
    }

    pub fn update_retention_days(&self, days: u32) {
        *self.retention_days.write().unwrap() = days;
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
    /// - 空串 → 拉最近 top-N（元数据，不含 text）
    /// - 非空 → fuzzy search（元数据，不含 text）
    ///
    /// **不预载完整 text**：搜索路径只携带 `id` + `preview`（80 字符截断），
    /// 完整 text 在用户激活时通过 `get_clipboard_text` command 按需拉取。
    /// 这将搜索路径 JSON 从 MB 级降到 KB 级。
    ///
    /// **不再检测 keyword**：0.8.5 §6.4 后此 engine 只在 EngineTakeover 路径被调用；
    /// route 层保证 Mixed 分支不会调到本 engine。
    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let arg = query.trim();
        let limit = self.effective_limit() as i64;
        let candidate_limit = *self.candidate_limit.read().unwrap() as i64;
        let retention_days = *self.retention_days.read().unwrap();
        let lang = self.language.read().unwrap().clone();
        let t0 = std::time::Instant::now();

        // 并行查询文本元数据 + 图片元数据（两个不同的 SQLite pool，无竞争）。
        // 不加载 text / thumb_blob——前端通过 get_clipboard_text / blink-clipimg 协议按需懒加载。
        let (text_metas, image_items) = if arg.is_empty() {
            // Alt+C 场景：两条简单 LIMIT 查询并行（不含 text 列）
            let t_text = std::time::Instant::now();
            let t_img = std::time::Instant::now();
            let (text, images) = tokio::join!(
                query_recent_meta(&self.pool, limit),
                query_recent_image_list(&self.cache_pool, limit)
            );
            tracing::trace!(
                text_count = text.len(),
                image_count = images.len(),
                text_ms = t_text.elapsed().as_millis() as u64,
                image_ms = t_img.elapsed().as_millis() as u64,
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "ClipboardEngine: 空查询 DB 并行完成"
            );
            (text, images)
        } else {
            // 搜索场景：文本 fuzzy + 图片 SQL LIKE 并行
            // 文本：按配置保留期拉元数据（0=全部，不含 text），nucleo 匹配 preview
            // 图片：SQL LIKE 过滤 source_app / source_path（不下全量再 Rust 过滤）
            let text_query = async {
                if retention_days == 0 {
                    query_recent_meta(&self.pool, candidate_limit).await
                } else {
                    query_recent_days_meta(&self.pool, retention_days, candidate_limit).await
                }
            };
            let (text_metas, image_items) =
                tokio::join!(text_query, search_image_list(&self.cache_pool, arg, limit));
            // nucleo fuzzy match on preview（不含 text）
            let text_matched = fuzzy_match_metas(&text_metas, arg, limit as usize);
            (text_matched, image_items)
        };

        let t1 = std::time::Instant::now();

        // 合并文本 + 图片，按 created_at 倒序
        let mut combined: Vec<(i64, ClipboardEntry)> = Vec::new();
        for meta in text_metas {
            combined.push((meta.created_at, ClipboardEntry::Text(meta)));
        }
        for item in image_items {
            combined.push((item.created_at, ClipboardEntry::Image(item)));
        }
        combined.sort_by_key(|(created_at, entry)| {
            std::cmp::Reverse((*created_at, id_recency(entry.id())))
        });

        let combined_len_before_take = combined.len();
        let result: Vec<SearchItem> = combined
            .into_iter()
            .enumerate()
            .map(|(i, (_, entry))| match entry {
                ClipboardEntry::Text(meta) => to_search_item(meta, i, &lang),
                ClipboardEntry::Image(meta) => to_image_search_item(meta, i, &lang),
            })
            .take(limit as usize)
            .collect();
        tracing::trace!(
            total = combined_len_before_take,
            returned = result.len(),
            db_ms = t0.elapsed().as_millis() as u64,
            merge_ms = t1.elapsed().as_millis() as u64,
            "ClipboardEngine: search 完成"
        );
        result
    }
}

/// ClipboardMeta → SearchItem（延迟加载版）。
///
/// - `title` 用 preview 的截断版（原文含换行则去掉），主行清爽
/// - `subtitle` "3 分钟前 · N chars"（副行时间提示）
/// - `score` 基线 0.9 起按名次递减 0.02，确保后端排序稳定（前端仍会按 score sort）
/// - `action = LazyCopy { hit_id }`——搜索路径不预载 text，激活时前端按需拉取
///
/// **与旧 `to_search_item` 的区别**：不再携带 `text`，改用 `LazyCopy` 变体。
/// 副行的 chars 计数用 `preview.chars().count()`（preview 是 80 字符截断版，
/// 精确计数需激活后拉取 text 才有——但副行只是提示性文案，截断值可接受）。
fn to_search_item(meta: ClipboardMeta, index: usize, lang: &str) -> SearchItem {
    let title = preview_line(&meta.preview);
    let subtitle = format_subtitle(&meta, lang);
    let score = (0.9 - index as f32 * 0.02).max(0.5);

    // 0.20：检测多行颜色列表（如截图配色面板"每行一个"复制格式）。
    // 对 preview 跑 parse_color_list，命中则给 SearchItem 附加 hex 数组，
    // 前端渲染一排小 swatch。name 仍保留 preview_line 截断版。
    let color_list_hex = crate::domain::color::parse_color_list(&meta.preview)
        .map(|results| results.iter().map(|r| r.hex.clone()).collect());

    SearchItem {
        id: format!("clipboard:{}", meta.id),
        title,
        subtitle: Some(subtitle),
        score,
        action: SearchAction::LazyCopy { hit_id: meta.id },
        source: "clipboard".into(),
        score_detail: Some(format!("clip=0.9-{:.2}", index as f32 * 0.02)),
        context_aware: false,
        color_list_hex,
    }
}

/// 文本/图片条目的统一容器（合并排序用）。
enum ClipboardEntry {
    Text(ClipboardMeta),
    Image(ClipboardImageListItem),
}

impl ClipboardEntry {
    fn id(&self) -> &str {
        match self {
            Self::Text(meta) => &meta.id,
            Self::Image(meta) => &meta.id,
        }
    }
}

fn id_recency(id: &str) -> u128 {
    id.rsplit('_')
        .next()
        .and_then(|suffix| suffix.parse().ok())
        .unwrap_or(0)
}

/// ClipboardImageListItem → SearchItem（0.16.4）。
///
/// - `title` = "图片 {W}x{H} ({source_desc})"
///   - 0.17.9：source_desc 拼接逻辑——有 source_path 时「文件名 · 应用名」，无则仅应用名
///   - 自写入标签（`blink:*` key）经 `resolve_source_desc` 转中英文文案
/// - `subtitle` 时间描述
/// - `action` = `RunAction { id: "copy_clipboard_image", arg: image_id }`
///   前端 actions.js 识别此 id，调 `copy_clipboard_image` 后端命令写回系统剪贴板
/// - `source` = "clipboard"（与文本项同 source，前端白名单已含）
///
/// **不含 thumb_blob**：缩略图由前端通过 blink-clipimg 协议按需懒加载，
/// 搜索路径无需加载 ~50KB/条的 BLOB 数据。
fn to_image_search_item(meta: ClipboardImageListItem, index: usize, lang: &str) -> SearchItem {
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
        color_list_hex: None,
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
        Some("blink:screenshot") => {
            if is_zh {
                "截图".to_string()
            } else {
                "Screenshot".to_string()
            }
        }
        Some("blink:repost") => {
            // 历史回贴 skip_persist=true 不会入库，此处仅防御
            if is_zh {
                "回贴".to_string()
            } else {
                "Repost".to_string()
            }
        }
        Some("blink:app") => "Blink".to_string(),
        Some("blink:ai") => "AI".to_string(),
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
fn format_image_subtitle(meta: &ClipboardImageListItem, lang: &str) -> String {
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

/// 生成单行预览：换行符转为可见标记 ⏎（U+23CE），其余控制字符替换为空格，
/// 压缩连续空白（⏎ 不被当作空白压缩），字符数截断。
///
/// 视觉效果：`foo\nbar` → `foo ⏎ bar`，用户能从预览看出换行位置，
/// 同时保持单行布局不变。`\r\n` 视为一次换行（先归一化为 `\n`）。
fn preview_line(raw: &str) -> String {
    // ⏎ (U+23CE) — 换行可见标记
    const NEWLINE_MARKER: char = '\u{23CE}';

    // 第一步：归一化 \r\n → \n，独立 \r → \n（统一换行表示）。
    let normalized = raw.replace("\r\n", "\n").replace('\r', "\n");

    // 第二步：将换行符 \n 替换为 ⏎，其余控制字符替换为空格。
    let flat: String = normalized
        .chars()
        .map(|c| {
            if c == '\n' {
                NEWLINE_MARKER
            } else if c.is_control() {
                ' '
            } else {
                c
            }
        })
        .collect();

    // 第三步：压缩连续空白（仅空格），⏎ 不被当作空白压缩。
    // ⏎ 两侧的空格各保留一个，视觉效果 `foo ⏎ bar`。
    // 连续多个 ⏎（如原文本 \n\n）各自保留，因为每个代表一次换行。
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
///
/// **注意**：chars 计数基于 `preview`（80 字符截断版），非完整 text。
/// 精确计数需激活后拉取 text 才有——但副行只是提示性文案，截断值可接受。
fn format_subtitle(meta: &ClipboardMeta, lang: &str) -> String {
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
    let char_count = meta.preview.chars().count();
    format!("{time_desc} · {char_count} chars")
}

/// nucleo fuzzy 匹配 `ClipboardMeta` 列表，按 preview 字段打分。
///
/// 与 `infra::data::clipboard::search` 逻辑一致，但作用于 `ClipboardMeta`（不含 text）。
/// 返回按 score 降序排列的 top-N `ClipboardMeta`。
fn fuzzy_match_metas(metas: &[ClipboardMeta], query: &str, limit: usize) -> Vec<ClipboardMeta> {
    use nucleo::pattern::{AtomKind, CaseMatching, Normalization, Pattern};
    use nucleo::{Config, Matcher, Utf32Str};

    let query_lower = query.to_ascii_lowercase();
    let mut matcher = Matcher::new(Config::DEFAULT);
    let pattern = Pattern::new(
        &query_lower,
        CaseMatching::Smart,
        Normalization::Smart,
        AtomKind::Fuzzy,
    );
    let mut buf = Vec::new();

    let mut scored: Vec<(u32, ClipboardMeta)> = metas
        .iter()
        .filter_map(|meta| {
            let haystack = Utf32Str::new(&meta.preview, &mut buf);
            let score = pattern.score(haystack, &mut matcher)?;
            Some((score, meta.clone()))
        })
        .collect();

    scored.sort_by_key(|x| std::cmp::Reverse(x.0));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, meta)| meta)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_line_shows_newline_marker() {
        // \n → ⏎（无额外空格时直接拼接）
        assert_eq!(preview_line("hello\nworld"), "hello⏎world");
        // \r\n 先归一化为 \n → 单个 ⏎，两侧原有空格压缩后各保留一个
        // "  a\r\n  b  " → 归一化 "  a\n  b  " → 映射 "  a⏎  b  "
        // → 压缩 " a⏎ b " → trim "a⏎ b"
        assert_eq!(preview_line("  a\r\n  b  "), "a⏎ b");
        // 纯 \r 归一化为 \n → 单个 ⏎
        assert_eq!(preview_line("x\ry"), "x⏎y");
    }

    #[test]
    fn preview_line_multiple_newlines() {
        // 连续换行各显示一个 ⏎（无空格时直接拼接）
        assert_eq!(preview_line("a\n\nb"), "a⏎⏎b");
        // \r\n\r\n → 归一化为 \n\n → 两个 ⏎
        assert_eq!(preview_line("a\r\n\r\nb"), "a⏎⏎b");
        // 有空格时两侧各保留一个
        assert_eq!(preview_line("a \n b"), "a ⏎ b");
    }

    #[test]
    fn preview_line_no_newline() {
        assert_eq!(preview_line("hello world"), "hello world");
        assert_eq!(preview_line("  hello  "), "hello");
    }

    #[test]
    fn preview_line_control_chars_still_become_space() {
        // 制表符等控制字符仍替换为空格
        assert_eq!(preview_line("a\tb"), "a b");
        assert_eq!(preview_line("a\u{0007}b"), "a b"); // BEL
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
        let meta = ClipboardMeta {
            id: "clip_123".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp(),
            source_app: None,
            hit_count: 0,
        };
        let si = to_search_item(meta, 0, "zh");
        assert_eq!(si.id, "clipboard:clip_123");
        assert_eq!(si.source, "clipboard");
        if let SearchAction::LazyCopy { hit_id } = &si.action {
            assert_eq!(hit_id, "clip_123");
        } else {
            panic!("expected LazyCopy action");
        }
        assert!((si.score - 0.9).abs() < 1e-6);
    }

    #[test]
    fn score_decreases_with_index() {
        let mk = |i: usize| {
            let meta = ClipboardMeta {
                id: format!("id_{i}"),
                preview: "x".into(),
                created_at: 0,
                source_app: None,
                hit_count: 0,
            };
            to_search_item(meta, i, "zh").score
        };
        assert!(mk(0) > mk(1));
        assert!(mk(1) > mk(2));
        // 下限保护
        assert!(mk(100) >= 0.5);
    }

    #[test]
    fn format_subtitle_recent_zh() {
        let meta = ClipboardMeta {
            id: "x".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp() - 30,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&meta, "zh").starts_with("刚刚"));
        assert!(format_subtitle(&meta, "zh").ends_with("5 chars"));
    }

    #[test]
    fn format_subtitle_recent_en() {
        let meta = ClipboardMeta {
            id: "x".into(),
            preview: "hello".into(),
            created_at: chrono::Utc::now().timestamp() - 30,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&meta, "en").starts_with("just now"));
        assert!(format_subtitle(&meta, "en").ends_with("5 chars"));
    }

    #[test]
    fn format_subtitle_minutes_zh() {
        let meta = ClipboardMeta {
            id: "x".into(),
            preview: "ab".into(),
            created_at: chrono::Utc::now().timestamp() - 300,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&meta, "zh").contains("分钟前"));
    }

    #[test]
    fn format_subtitle_minutes_en() {
        let meta = ClipboardMeta {
            id: "x".into(),
            preview: "ab".into(),
            created_at: chrono::Utc::now().timestamp() - 300,
            source_app: None,
            hit_count: 0,
        };
        assert!(format_subtitle(&meta, "en").contains("min ago"));
    }

    #[test]
    fn display_pages_defaults_to_3() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        assert_eq!(*engine.display_pages.read().unwrap(), DEFAULT_DISPLAY_PAGES);
        assert_eq!(DEFAULT_DISPLAY_PAGES, 3);
    }

    #[test]
    fn update_display_pages_accepts_valid_range() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        engine.update_display_pages(1);
        assert_eq!(*engine.display_pages.read().unwrap(), 1);
        engine.update_display_pages(20);
        assert_eq!(*engine.display_pages.read().unwrap(), 20);
    }

    #[test]
    fn update_display_pages_clamps_out_of_range_to_default() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        engine.update_display_pages(0);
        assert_eq!(*engine.display_pages.read().unwrap(), DEFAULT_DISPLAY_PAGES);
        engine.update_display_pages(9999);
        assert_eq!(*engine.display_pages.read().unwrap(), DEFAULT_DISPLAY_PAGES);
    }

    #[test]
    fn effective_limit_is_pages_times_page_size() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        // 默认 3 页 × 9 条/页 = 27
        assert_eq!(engine.effective_limit(), 27);
        engine.update_display_pages(5);
        engine.update_page_size(9);
        // 5 × 9 = 45
        assert_eq!(engine.effective_limit(), 45);
        engine.update_display_pages(20);
        engine.update_page_size(20);
        // 20 × 20 = 400 (at upper clamp)
        assert_eq!(engine.effective_limit(), 400);
    }

    #[test]
    fn update_page_size_zero_falls_back() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        engine.update_page_size(0);
        assert_eq!(*engine.page_size.read().unwrap(), DEFAULT_PAGE_SIZE);
    }

    #[test]
    fn retention_days_is_hot_updatable() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        engine.update_retention_days(0);
        assert_eq!(*engine.retention_days.read().unwrap(), 0);
        engine.update_retention_days(90);
        assert_eq!(*engine.retention_days.read().unwrap(), 90);
    }

    #[test]
    fn id_recency_parses_text_and_image_ids() {
        assert_eq!(id_recency("clip_123"), 123);
        assert_eq!(id_recency("clipimg_456"), 456);
        assert_eq!(id_recency("legacy"), 0);
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
        let meta = ClipboardImageListItem {
            id: "img_test".into(),
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
        let meta = ClipboardImageListItem {
            id: "img_test2".into(),
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
        let meta = ClipboardImageListItem {
            id: "img_test3".into(),
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
