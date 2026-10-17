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
use blink_infra::data::clipboard::{
    ClipboardCursor, ClipboardDisplay, ClipboardMeta, query_display_prefixes,
    query_recent_days_meta, query_recent_days_meta_after, query_recent_meta,
    query_recent_meta_after,
};
use blink_infra::data::clipboard_files::{
    ClipboardFileMeta, ClipboardFileSearchRow, query_file_search_candidates,
    query_recent_file_list, query_recent_file_list_after,
};
use blink_infra::data::clipboard_images::{
    ClipboardImageListItem, query_recent_image_list, query_recent_image_list_after,
    search_image_list, search_image_list_after,
};

/// Engine id — 对应 `SearchEngine::id()` 与 `Route::EngineTakeover.engine_id`。
pub const ENGINE_ID: &str = "clipboard";

/// 触发关键词。原文匹配（Exact / Prefix + 空格）算命中——
/// engine 场景不做首拼弱信号派生（本体数据信号天然强，"剪贴板" 不会误命中）。
pub const TRIGGERS: &[&str] = &["剪贴板", "clip", "jtb", "jiantieban"];

/// 默认 page_size（与 SearchConfig::page_size 默认值一致）。
const DEFAULT_PAGE_SIZE: usize = 9;
/// 0.25.29：首屏 = page_size × 2（首屏 + 预取一屏），后续流式每批由前端传 batch。
const FIRST_BATCH_PAGES: usize = 2;
/// 单批下限保护：至少返回 1 条。
const EFFECTIVE_LIMIT_MIN: usize = 1;
/// 单批上限保护：防一次拉太多拖慢 UI（0.25.29 起是单批钳制，
/// 会话总量保护在前端 2000 条，不再承担一次性上限职责）。
const EFFECTIVE_LIMIT_MAX: usize = 400;

/// 搜索候选池上限默认值（与 `ClipboardConfig::candidate_limit` 默认值一致）。
const DEFAULT_CANDIDATE_LIMIT: usize = 500;
/// 搜索候选池下限/上限。下限 50 防几乎无结果，上限 5000 防内存/IO 过载。
const CANDIDATE_LIMIT_MIN: usize = 50;
const CANDIDATE_LIMIT_MAX: usize = 5000;

/// 剪贴板模式类型筛选（0.25.23）。
///
/// 文本、图片与文件分表存储，筛选下推为「只查询选中的表」，无需按行过滤。
/// 文件按条目身份分类：文件路径文本仍为 Text，文件条目中的 PNG 仍为 File。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipKind {
    All,
    Text,
    Image,
    File,
}

impl ClipKind {
    /// 解析前端传入的 kind 参数；未知/缺失回落 All（不因脏参数丢结果）。
    pub fn parse(raw: Option<&str>) -> Self {
        match raw {
            Some("text") => Self::Text,
            Some("image") => Self::Image,
            Some("file") => Self::File,
            _ => Self::All,
        }
    }
}

/// 0.25.29：流式分页结果——本批条目 + 推进游标 + 是否可能还有下一批。
///
/// `next_cursor` 是本批最后一条的 `(created_at, id)`；`has_more` 由
/// 「探测多拉 1 条后仍有剩余」判定。两者一致：`has_more == false` 时
/// `next_cursor` 为 None（前端不再发起追加）。
#[derive(Debug, Clone)]
pub struct ClipboardPageResult {
    pub items: Vec<SearchItem>,
    pub next_cursor: Option<ClipboardCursor>,
    pub has_more: bool,
}

pub struct ClipboardEngine {
    pool: SqlitePool,
    /// cache 库——clipboard_images 表所在（0.16.4 图片历史）。
    cache_pool: SqlitePool,
    /// UI 语言快照,用于 subtitle 时间描述 zh/en 切换（0.8.5.1 §6.6）。
    /// 与 `SearchService` 联动:`SearchService::update_language` 转发到本 engine。
    language: Arc<RwLock<String>>,
    /// 搜索结果每页条数快照（`page_size`，来自 SearchConfig）。
    /// 0.25.29：首屏 = page_size × 2（display_pages 已退役）。
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
            page_size: Arc::new(RwLock::new(DEFAULT_PAGE_SIZE)),
            candidate_limit: Arc::new(RwLock::new(DEFAULT_CANDIDATE_LIMIT)),
            retention_days: Arc::new(RwLock::new(30)),
        }
    }

    /// 更新 UI 语言快照（SearchService::update_language 转发）。
    pub fn update_language(&self, lang: String) {
        *self.language.write().unwrap() = lang;
    }

    /// 更新搜索结果每页条数（`SearchConfig::page_size` 变化时转发）。
    /// 0.25.29：首屏 = page_size × 2、流式每批 page_size。
    pub fn update_page_size(&self, page_size: u32) {
        let clamped = if page_size == 0 {
            DEFAULT_PAGE_SIZE
        } else {
            page_size as usize
        };
        *self.page_size.write().unwrap() = clamped;
    }

    /// 首屏加载条数 = page_size × 2，钳制到 [1, 400]。
    fn first_batch_limit(&self) -> usize {
        let ps = *self.page_size.read().unwrap();
        let limit = ps.saturating_mul(FIRST_BATCH_PAGES);
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

    /// 剪贴板模式搜索（0.25.23 起带类型筛选；0.25.29 起带 keyset 游标分页）。
    ///
    /// kind 只决定查哪张表：Text 跳过图片查询、Image 跳过文本查询（分表存储，
    /// 筛选即下推），All 与历史行为一致（两表并行合并按时间倒序）。
    ///
    /// 游标协议：`cursor` 是上一批末条的 `(created_at, id)`，两表共用（排序键
    /// 同构）；SQL 条件与 Rust 合并比较器统一为 `(created_at, id 字符串)` 序。
    /// 每表探测性多拉 1 条判定 `has_more`，实际只返回 `batch` 条。
    /// 监听器持续入库顶置不影响分页（keyset 抗漂移），禁用 OFFSET。
    pub async fn search_page(
        &self,
        arg: &str,
        kind: ClipKind,
        cursor: Option<ClipboardCursor>,
        batch: usize,
    ) -> ClipboardPageResult {
        // batch 钳制：0 → 按位置兜底（首批 = 首屏两页，追加 = 一页 page_size），
        // 上限 400 防脏参数拖垮 UI
        let batch = if batch == 0 {
            if cursor.is_none() {
                self.first_batch_limit()
            } else {
                *self.page_size.read().unwrap()
            }
        } else {
            batch.clamp(EFFECTIVE_LIMIT_MIN, EFFECTIVE_LIMIT_MAX)
        };
        let probe = batch + 1; // 探测 has_more 的多拉量
        let candidate_limit = *self.candidate_limit.read().unwrap() as i64;
        let retention_days = *self.retention_days.read().unwrap();
        let lang = self.language.read().unwrap().clone();
        let t0 = std::time::Instant::now();

        // 并行查询文本 + 图片 + 文件元数据（pool 间无竞争；文件与文本同池但
        // SQLite 串行排队即可，查询轻量）。不加载 text / thumb_blob / 全量路径数组——
        // 前端通过 get_clipboard_text / blink-clipimg 协议按需懒加载。
        // 文件条目（clipboard_files，history 库）在 All 混排或 File 独立查询。
        let (text_metas, image_items, file_metas) = if arg.is_empty() {
            // 最近历史场景：三条简单 LIMIT 查询并行（不含 text 列）
            let (text, images, files) = tokio::join!(
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::Text) {
                        Vec::new()
                    } else {
                        match &cursor {
                            Some(c) => query_recent_meta_after(&self.pool, c, probe as i64).await,
                            None => query_recent_meta(&self.pool, probe as i64).await,
                        }
                    }
                },
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::Image) {
                        Vec::new()
                    } else {
                        match &cursor {
                            Some(c) => {
                                query_recent_image_list_after(&self.cache_pool, c, probe as i64)
                                    .await
                            }
                            None => query_recent_image_list(&self.cache_pool, probe as i64).await,
                        }
                    }
                },
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::File) {
                        Vec::new()
                    } else {
                        match &cursor {
                            Some(c) => {
                                query_recent_file_list_after(&self.pool, c, probe as i64).await
                            }
                            None => query_recent_file_list(&self.pool, probe as i64).await,
                        }
                    }
                }
            );
            tracing::trace!(
                text_count = text.len(),
                image_count = images.len(),
                file_count = files.len(),
                elapsed_ms = t0.elapsed().as_millis() as u64,
                "ClipboardEngine: 空查询 DB 并行完成"
            );
            (text, images, files)
        } else {
            // 搜索场景：文本 fuzzy（游标下推到候选池 SQL）+ 图片 SQL LIKE +
            // 文件有界候选窗口按各路径 fuzzy（不以 LIKE 丢弃非连续命中）并行
            let text_query = async {
                match &cursor {
                    Some(c) => {
                        if retention_days == 0 {
                            query_recent_meta_after(&self.pool, c, candidate_limit).await
                        } else {
                            query_recent_days_meta_after(
                                &self.pool,
                                retention_days,
                                c,
                                candidate_limit,
                            )
                            .await
                        }
                    }
                    None => {
                        if retention_days == 0 {
                            query_recent_meta(&self.pool, candidate_limit).await
                        } else {
                            query_recent_days_meta(&self.pool, retention_days, candidate_limit)
                                .await
                        }
                    }
                }
            };
            let (text_metas, image_items, file_rows) = tokio::join!(
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::Text) {
                        Vec::new()
                    } else {
                        text_query.await
                    }
                },
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::Image) {
                        Vec::new()
                    } else {
                        match &cursor {
                            Some(c) => {
                                search_image_list_after(&self.cache_pool, arg, c, probe as i64)
                                    .await
                            }
                            None => search_image_list(&self.cache_pool, arg, probe as i64).await,
                        }
                    }
                },
                async {
                    if !matches!(kind, ClipKind::All | ClipKind::File) {
                        Vec::new()
                    } else {
                        query_file_search_candidates(
                            &self.pool,
                            cursor.as_ref(),
                            candidate_limit,
                            retention_days,
                        )
                        .await
                    }
                }
            );
            // nucleo fuzzy match on preview（不含 text）；多拉 1 条探测 has_more
            let text_matched = fuzzy_match_metas(&text_metas, arg, probe);
            // 文件：候选数有界，匹配后取 probe，逐条解析即弃完整路径数组
            let file_matched = fuzzy_match_files(file_rows, arg, probe);
            (text_matched, image_items, file_matched)
        };

        let t1 = std::time::Instant::now();

        // 合并文本 + 图片 + 文件，按 (created_at, id 字符串) 倒序——与各表 SQL 的
        // ORDER BY created_at DESC, id DESC 逐字对齐（keyset 不重不漏的前提）。
        let mut combined: Vec<(i64, String, ClipboardEntry)> = Vec::new();
        for meta in text_metas {
            combined.push((meta.created_at, meta.id.clone(), ClipboardEntry::Text(meta)));
        }
        for item in image_items {
            combined.push((
                item.created_at,
                item.id.clone(),
                ClipboardEntry::Image(item),
            ));
        }
        for meta in file_metas {
            combined.push((meta.created_at, meta.id.clone(), ClipboardEntry::File(meta)));
        }
        combined.sort_by_key(|(created_at, id, _)| std::cmp::Reverse((*created_at, id.clone())));

        let has_more = combined.len() > batch;
        let page_end = combined.len().min(batch);
        let next_cursor = if has_more {
            combined
                .get(page_end - 1)
                .map(|(created_at, id, _)| ClipboardCursor {
                    created_at: *created_at,
                    id: id.clone(),
                })
        } else {
            None
        };
        // Hydrate only this returned batch, never the full fuzzy candidate pool.
        // Read a bounded display prefix so CSS can clip to the actual row width.
        let display_ids: Vec<&str> = combined
            .iter()
            .take(page_end)
            .filter_map(|(_, _, entry)| match entry {
                ClipboardEntry::Text(meta) => Some(meta.id.as_str()),
                _ => None,
            })
            .collect();
        let display = match query_display_prefixes(&self.pool, &display_ids).await {
            Ok(display) => display,
            Err(error) => {
                tracing::warn!(%error, "读取剪贴板展示前缀失败，回退摘要");
                Default::default()
            }
        };
        let result: Vec<SearchItem> = combined
            .into_iter()
            .take(page_end)
            .enumerate()
            .map(|(i, (_, _, entry))| match entry {
                ClipboardEntry::Text(meta) => {
                    let prefix = display.get(&meta.id);
                    to_search_item(meta, i, &lang, prefix)
                }
                ClipboardEntry::Image(meta) => to_image_search_item(meta, i, &lang),
                ClipboardEntry::File(meta) => to_file_search_item(meta, i, &lang),
            })
            .collect();
        tracing::trace!(
            batch,
            returned = result.len(),
            has_more,
            db_ms = t0.elapsed().as_millis() as u64,
            merge_ms = t1.elapsed().as_millis() as u64,
            "ClipboardEngine: search_page 完成"
        );
        ClipboardPageResult {
            items: result,
            next_cursor,
            has_more,
        }
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
    ///
    /// 0.25.29：实现主体移入 `search_page`（keyset 游标 + 类型筛选），
    /// keyword 路径恒为 All 首屏（首屏 = page_size × 2）。
    async fn search(&self, query: &str, _ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        self.search_page(query.trim(), ClipKind::All, None, self.first_batch_limit())
            .await
            .items
    }
}

/// ClipboardMeta → SearchItem（延迟加载版）。
///
/// - `title` 用本批展示前缀，换行可视化，省略由前端宽度决定
/// - `subtitle` "3 分钟前 · N chars"（副行时间提示）
/// - `score` 基线 0.9 起按名次递减 0.02，确保后端排序稳定（前端仍会按 score sort）
/// - `action = LazyCopy { hit_id }`——搜索路径不预载 text，激活时前端按需拉取
///
/// **与旧 `to_search_item` 的区别**：不再携带 `text`，改用 `LazyCopy` 变体。
/// 副行字数来自 DB length(text)，不以摘要长度冒充正文长度。
fn to_search_item(
    meta: ClipboardMeta,
    index: usize,
    lang: &str,
    display: Option<&ClipboardDisplay>,
) -> SearchItem {
    let title = preview_line(display.map_or(meta.preview.as_str(), |d| d.prefix.as_str()));
    let subtitle = format_subtitle(&meta, lang, display.map(|d| d.char_count));
    let score = (0.9 - index as f32 * 0.02).max(0.5);

    // 0.20：检测多行颜色列表（如截图配色面板"每行一个"复制格式）。
    // 对 preview 跑 parse_color_list，命中则给 SearchItem 附加 hex 数组，
    // 前端渲染一排小 swatch；仍用匹配摘要检测，不改变颜色识别范围。
    let color_list_hex = crate::color::parse_color_list(&meta.preview)
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

/// 文本/图片/文件条目的统一容器（合并排序用）。
enum ClipboardEntry {
    Text(ClipboardMeta),
    Image(ClipboardImageListItem),
    File(ClipboardFileMeta),
}

/// ClipboardFileMeta → SearchItem（0.26.9）。
///
/// - `title`：单文件用文件名；多文件「首文件名 +N」（数量语义与语言无关）
/// - `subtitle` 「N 分钟前 · M 个文件 · 悬浮查看」（zh/en）
/// - `action` = `RunAction { id: "copy_clipboard_files", arg: 条目id }`
///   前端识别后走 `run_builtin_action` 写回 CF_HDROP（Capability 绑定条目引用，
///   不接受 UI 任意路径列表）
/// - `source` = "clipboard"（前端白名单已含）
fn to_file_search_item(meta: ClipboardFileMeta, index: usize, lang: &str) -> SearchItem {
    let title = if meta.file_count > 1 {
        format!("{} +{}", meta.first_name, meta.file_count - 1)
    } else {
        meta.first_name.clone()
    };
    let subtitle = format_file_subtitle(&meta, lang);
    let score = (0.9 - index as f32 * 0.02).max(0.5);

    SearchItem {
        id: format!("clipboard:{}", meta.id),
        title,
        subtitle: Some(subtitle),
        score,
        action: SearchAction::RunAction {
            id: "copy_clipboard_files".into(),
            // 参数为 Capability schema 最终形态（{id}）；前端通用 run 分支直接
            // 走 run_builtin_action 写回 CF_HDROP
            arg: Some(serde_json::json!({ "id": meta.id })),
        },
        source: "clipboard".into(),
        score_detail: Some(format!("clipfile=0.9-{:.2}", index as f32 * 0.02)),
        context_aware: false,
        color_list_hex: None,
    }
}

/// 文件条目副行「时间 · N 个文件 · 悬浮查看」（0.26.9）。
fn format_file_subtitle(meta: &ClipboardFileMeta, lang: &str) -> String {
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
    if is_zh {
        format!("{} · {} 个文件 · 悬浮查看", time_desc, meta.file_count)
    } else {
        format!("{} · {} files · hover to view", time_desc, meta.file_count)
    }
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
    // 入库统一转 PNG（png_blob），后缀展示真实存储格式。图片预览提示常显（缩略图
    // 之外永远有更多可看）；文本侧由前端在标题溢出时才追加提示（0.26.8）。
    let hint = if is_zh {
        "左侧悬浮查看图片"
    } else {
        "hover left for image"
    };
    format!(
        "{time_desc} · {}x{} · PNG · {hint}",
        meta.width, meta.height
    )
}

/// 生成单行预览：换行符转为可见标记 ⏎（U+23CE），其余控制字符替换为空格，
/// 压缩连续空白（⏎ 不被当作空白压缩），不按字符数追加省略号。
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
    out.trim().to_string()
}

/// 生成副行"N 分钟前 · M chars" / "N min ago · M chars"（0.8.5.1 §6.6 i18n）。
///
/// zh/en 双语。lang 传入 "zh" 或 "en"（其他视作 en fallback）。
/// "chars" 单位跨语言保留——ASCII 字数国际化收益低,且 CJK 字符计数概念相通。
///
/// 展示前缀查询失败时不以摘要长度冒充完整字数。悬浮提示由前端在标题溢出时追加（0.26.8）。
fn format_subtitle(meta: &ClipboardMeta, lang: &str, char_count: Option<usize>) -> String {
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
    char_count.map_or(time_desc.clone(), |count| {
        format!("{time_desc} · {count} chars")
    })
}

/// nucleo fuzzy 匹配 `ClipboardMeta` 列表，按 preview 字段打分。
///
/// 与 `infra::data::clipboard::search` 逻辑一致，但作用于 `ClipboardMeta`（不含 text）。
/// 匹配仅决定命中；截断前按时间/id 排序，与分页游标保持同序。
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

    let mut matched: Vec<ClipboardMeta> = metas
        .iter()
        .filter_map(|meta| {
            let haystack = Utf32Str::new(&meta.preview, &mut buf);
            pattern.score(haystack, &mut matcher)?;
            Some(meta.clone())
        })
        .collect();

    matched.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| b.id.cmp(&a.id))
    });
    matched.into_iter().take(limit).collect()
}

/// 文件候选按各路径做 fuzzy 匹配。
///
/// 候选窗口有界（`candidate_limit`）；此处逐条解析 `paths_json`、对每条
/// 完整路径（含文件名）匹配，任一命中即保留，解析后即弃不常驻。
/// 输入行已按时间倒序，take `limit` 即可。
fn fuzzy_match_files(
    rows: Vec<ClipboardFileSearchRow>,
    query: &str,
    limit: usize,
) -> Vec<ClipboardFileMeta> {
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

    let matched: Vec<ClipboardFileMeta> = rows
        .into_iter()
        .filter_map(|row| {
            let paths: Vec<String> = serde_json::from_str(&row.paths_json).ok()?;
            let hit = paths.iter().any(|path| {
                let haystack = Utf32Str::new(path, &mut buf);
                pattern.score(haystack, &mut matcher).is_some()
            });
            if !hit {
                return None;
            }
            Some(ClipboardFileMeta {
                id: row.id,
                file_count: row.file_count,
                first_name: row.first_name,
                created_at: row.created_at,
                source_app: row.source_app,
            })
        })
        .collect();
    matched.into_iter().take(limit).collect()
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
    fn preview_line_leaves_ellipsis_to_frontend_width() {
        let long: String = "a".repeat(100);
        let out = preview_line(&long);
        assert_eq!(out, long);
        assert_eq!(preview_line(&"中文".repeat(100)), "中文".repeat(100));
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
        let si = to_search_item(meta, 0, "zh", None);
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
            to_search_item(meta, i, "zh", None).score
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
        assert!(format_subtitle(&meta, "zh", Some(500)).starts_with("刚刚"));
        assert!(format_subtitle(&meta, "zh", Some(500)).ends_with("500 chars"));
        assert!(!format_subtitle(&meta, "zh", None).contains("chars"));
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
        assert!(format_subtitle(&meta, "en", Some(5)).starts_with("just now"));
        assert!(format_subtitle(&meta, "en", Some(5)).ends_with("5 chars"));
    }

    #[test]
    fn format_image_subtitle_appends_png_and_hover_hint() {
        let meta = ClipboardImageListItem {
            id: "img".into(),
            width: 800,
            height: 600,
            created_at: chrono::Utc::now().timestamp() - 30,
            source_app: None,
            source_path: None,
        };
        assert!(format_image_subtitle(&meta, "zh").ends_with("800x600 · PNG · 左侧悬浮查看图片"));
        assert!(
            format_image_subtitle(&meta, "en").ends_with("800x600 · PNG · hover left for image")
        );
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
        assert!(format_subtitle(&meta, "zh", Some(2)).contains("分钟前"));
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
        assert!(format_subtitle(&meta, "en", Some(2)).contains("min ago"));
    }

    // ── 0.25.29：display_pages 已退役，首屏 = page_size × 2 ───────────────

    #[test]
    fn first_batch_limit_is_two_pages() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let cache_pool =
            rt.block_on(async { sqlx::SqlitePool::connect_lazy("sqlite::memory:").unwrap() });
        let engine = ClipboardEngine::new(pool, cache_pool);
        // 默认 9 条/页 × 2 = 18
        assert_eq!(engine.first_batch_limit(), 18);
        engine.update_page_size(7);
        assert_eq!(engine.first_batch_limit(), 14);
        // 超大 page_size 钳制到 400
        engine.update_page_size(500);
        assert_eq!(engine.first_batch_limit(), 400);
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

    // ── 0.25.23 ClipKind 类型筛选 ────────────────────────────────────────

    #[test]
    fn clip_kind_parse_known_values() {
        assert_eq!(ClipKind::parse(None), ClipKind::All);
        assert_eq!(ClipKind::parse(Some("text")), ClipKind::Text);
        assert_eq!(ClipKind::parse(Some("image")), ClipKind::Image);
        assert_eq!(ClipKind::parse(Some("file")), ClipKind::File);
    }

    #[test]
    fn clip_kind_parse_unknown_falls_back_to_all() {
        // 脏参数不丢结果：未知值/空串/大小写不一律回落 All
        assert_eq!(ClipKind::parse(Some("")), ClipKind::All);
        assert_eq!(ClipKind::parse(Some("audio")), ClipKind::All);
        assert_eq!(ClipKind::parse(Some("TEXT")), ClipKind::All);
    }

    /// 建带数据的内存 engine：2 条文本（needle 命中一条）+ 2 张图片
    /// （source_app 含 needle 命中一张）。max_connections(1) 保证同一内存库
    /// 不被第二条连接孤立（sqlite::memory: 按连接隔离）。
    async fn engine_with_mixed_data() -> ClipboardEngine {
        let text_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard::init_db(&text_pool)
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        for (id, text, at) in [
            ("clip_100", "needle in haystack", now),
            ("clip_101", "plain text", now - 10),
        ] {
            blink_infra::data::clipboard::save_item(
                &text_pool,
                &blink_infra::data::clipboard::ClipboardItem {
                    id: id.into(),
                    text: text.into(),
                    preview: text.into(),
                    created_at: at,
                    source_app: None,
                    hit_count: 0,
                },
            )
            .await
            .unwrap();
        }

        let cache_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard_images::init_db(&cache_pool)
            .await
            .unwrap();
        for (id, app, at) in [
            ("clipimg_200", "needleapp.exe", now),
            ("clipimg_201", "otherapp.exe", now - 10),
        ] {
            blink_infra::data::clipboard_images::save_image(
                &cache_pool,
                &blink_infra::data::clipboard_images::ClipboardImage {
                    id: id.into(),
                    png_blob: vec![1],
                    thumb_blob: vec![2],
                    width: 10,
                    height: 10,
                    sha256: format!("sha_{id}"),
                    created_at: at,
                    source_app: Some(app.into()),
                    source_path: None,
                },
            )
            .await
            .unwrap();
        }

        ClipboardEngine::new(text_pool, cache_pool)
    }

    /// 0.26.9：文件条目的 action 是 Capability 形态 `{id}`（前端通用 run 分支
    /// 直接走 run_builtin_action），不能是裸字符串。
    #[test]
    fn to_file_search_item_uses_capability_arg_shape() {
        let meta = ClipboardFileMeta {
            id: "clipfile_1".into(),
            file_count: 2,
            first_name: "a.txt".into(),
            created_at: chrono::Utc::now().timestamp(),
            source_app: None,
        };
        let si = to_file_search_item(meta, 0, "zh");
        assert!(matches!(
            &si.action,
            SearchAction::RunAction { id, arg: Some(serde_json::Value::Object(map)) }
                if id == "copy_clipboard_files" && map.get("id").and_then(|v| v.as_str()) == Some("clipfile_1")
        ));
        assert_eq!(si.title, "a.txt +1", "多文件标题为首文件名 +N");
    }

    #[tokio::test]
    async fn file_search_fuzzy_candidates_survive_nonmatching_rows_and_pagination() {
        let engine = engine_with_mixed_data().await;
        blink_infra::data::clipboard_files::init_db(&engine.pool)
            .await
            .unwrap();
        let now = chrono::Utc::now().timestamp();
        for (id, paths, at) in [
            ("match-a", vec![r"C:\docs\foo_bar.txt".to_string()], now - 2),
            (
                "match-b",
                vec![
                    r"C:\first.txt".to_string(),
                    r"C:\docs\feature_build_test.txt".to_string(),
                ],
                now - 3,
            ),
        ] {
            blink_infra::data::clipboard_files::save_file(
                &engine.pool,
                &blink_infra::data::clipboard_files::ClipboardFileItem {
                    id: id.into(),
                    paths,
                    created_at: at,
                    source_app: None,
                },
            )
            .await
            .unwrap();
        }
        // 命中条目位于单批 probe 之外，仍应从 candidate_limit 窗口找回。
        for i in 0..12 {
            blink_infra::data::clipboard_files::save_file(
                &engine.pool,
                &blink_infra::data::clipboard_files::ClipboardFileItem {
                    id: format!("noise-{i}"),
                    paths: vec![format!(r"C:\noise\item-{i}.png")],
                    created_at: now + i,
                    source_app: None,
                },
            )
            .await
            .unwrap();
        }
        for kind in [ClipKind::All, ClipKind::File] {
            let page = engine.search_page("fbt", kind, None, 1).await;
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].id, "clipboard:match-a");
            assert!(page.has_more);
            let next = engine.search_page("fbt", kind, page.next_cursor, 1).await;
            assert_eq!(next.items.len(), 1);
            assert_eq!(next.items[0].id, "clipboard:match-b");
            assert!(!next.has_more);
            let path = engine
                .search_page(r"C:\docs\foo_bar.txt", kind, None, 2)
                .await;
            assert_eq!(path.items.len(), 1, "Windows 路径不受 JSON 反斜杠转义影响");
            assert_eq!(path.items[0].id, "clipboard:match-a");
        }
        assert!(
            engine
                .search_page("fbt", ClipKind::Text, None, 2)
                .await
                .items
                .is_empty()
        );
    }

    #[tokio::test]
    async fn file_kind_filters_by_entry_type_and_pages_without_duplicates() {
        let engine = engine_with_mixed_data().await;
        blink_infra::data::clipboard_files::init_db(&engine.pool)
            .await
            .unwrap();
        // 已有文本/图片命中 needle，但文件表为空时文件筛选必须为空。
        for query in ["", "needle"] {
            let empty = engine.search_page(query, ClipKind::File, None, 1).await;
            assert!(empty.items.is_empty());
            assert!(!empty.has_more);
            assert!(empty.next_cursor.is_none());
        }
        let now = chrono::Utc::now().timestamp();
        for (id, paths) in [
            ("clipfile_9", vec![r"C:\needle.png".into()]),
            ("clipfile_2", vec![r"C:\needle-folder".into()]),
            (
                "clipfile_10",
                vec![r"C:\other.txt".into(), r"C:\needle.txt".into()],
            ),
        ] {
            blink_infra::data::clipboard_files::save_file(
                &engine.pool,
                &blink_infra::data::clipboard_files::ClipboardFileItem {
                    id: id.into(),
                    paths,
                    created_at: now,
                    source_app: None,
                },
            )
            .await
            .unwrap();
        }
        for (query, text_count, image_count) in [("", 2, 2), ("needle", 1, 1)] {
            for (kind, count) in [
                (ClipKind::All, text_count + image_count + 3),
                (ClipKind::Text, text_count),
                (ClipKind::Image, image_count),
            ] {
                assert_eq!(
                    engine.search_page(query, kind, None, 20).await.items.len(),
                    count
                );
            }
            let mut ids = Vec::new();
            let mut cursor = None;
            for batch in 0..3 {
                let page = engine.search_page(query, ClipKind::File, cursor, 1).await;
                assert_eq!(page.items.len(), 1, "文件筛选每批只返回一个文件条目");
                assert!(matches!(&page.items[0].action,
                    SearchAction::RunAction { id, .. } if id == "copy_clipboard_files"));
                ids.push(page.items[0].id.clone());
                assert_eq!(page.has_more, batch < 2);
                assert_eq!(page.next_cursor.is_some(), batch < 2);
                cursor = page.next_cursor;
            }
            assert_eq!(
                ids,
                [
                    "clipboard:clipfile_9",
                    "clipboard:clipfile_2",
                    "clipboard:clipfile_10"
                ],
                "PNG、目录、多文件都按文件身份返回，同秒游标按字符串序不重不漏"
            );
        }
    }

    fn is_image_item(item: &SearchItem) -> bool {
        matches!(
            item.action,
            SearchAction::RunAction {
                id: ref run_id,
                ..
            } if run_id == "copy_clipboard_image"
        )
    }

    #[tokio::test]
    async fn displayed_batch_uses_original_prefix_instead_of_legacy_summary() {
        let engine = engine_with_mixed_data().await;
        let text = "i".repeat(160);
        blink_infra::data::clipboard::save_item(
            &engine.pool,
            &blink_infra::data::clipboard::ClipboardItem {
                id: "fresh".into(),
                text: text.clone(),
                preview: "i".repeat(80) + "...",
                created_at: chrono::Utc::now().timestamp() + 5,
                source_app: None,
                hit_count: 0,
            },
        )
        .await
        .unwrap();
        let page = engine.search_page("", ClipKind::Text, None, 1).await;
        assert_eq!(page.items.len(), 1);
        assert_eq!(
            page.items[0].title, text,
            "no artificial 60/80-character ellipsis"
        );
        assert!(
            page.items[0]
                .subtitle
                .as_ref()
                .unwrap()
                .ends_with("160 chars")
        );
        assert!(
            matches!(&page.items[0].action, SearchAction::LazyCopy { hit_id } if hit_id == "fresh")
        );
    }

    #[tokio::test]
    async fn kind_all_returns_text_and_images() {
        let engine = engine_with_mixed_data().await;
        let items = engine.search_page("", ClipKind::All, None, 0).await.items;
        assert_eq!(items.len(), 2 + 2, "全量应合并文本与图片");
        assert_eq!(items.iter().filter(|x| is_image_item(x)).count(), 2);
    }

    #[tokio::test]
    async fn kind_text_skips_image_table() {
        let engine = engine_with_mixed_data().await;
        let items = engine.search_page("", ClipKind::Text, None, 0).await.items;
        assert_eq!(items.len(), 2, "文本筛选只返回文本项");
        assert!(items.iter().all(|x| !is_image_item(x)));
    }

    #[tokio::test]
    async fn kind_image_skips_text_table() {
        let engine = engine_with_mixed_data().await;
        let items = engine.search_page("", ClipKind::Image, None, 0).await.items;
        assert_eq!(items.len(), 2, "图片筛选只返回图片项");
        assert!(items.iter().all(is_image_item));
    }

    #[tokio::test]
    async fn kind_filter_applies_to_fuzzy_search() {
        let engine = engine_with_mixed_data().await;
        // "needle" 同时命中文本 preview 与图片 source_app
        let text_hits = engine
            .search_page("needle", ClipKind::Text, None, 0)
            .await
            .items;
        assert_eq!(text_hits.len(), 1);
        assert!(!is_image_item(&text_hits[0]));

        let image_hits = engine
            .search_page("needle", ClipKind::Image, None, 0)
            .await
            .items;
        assert_eq!(image_hits.len(), 1);
        assert!(is_image_item(&image_hits[0]));
    }

    // ── 0.25.29 流式游标分页 ─────────────────────────────────────────────

    /// has_more 与 next_cursor 恒同真假；跨批遍历不重不漏（小集合：2 文本 + 2 图片）。
    #[tokio::test]
    async fn search_page_small_set_streams_exactly_once() {
        let engine = engine_with_mixed_data().await;
        let mut collected: Vec<String> = Vec::new();
        let mut cursor = None;
        loop {
            let page = engine.search_page("", ClipKind::All, cursor, 2).await;
            assert_eq!(
                page.has_more,
                page.next_cursor.is_some(),
                "has_more 与 next_cursor 必须同真假"
            );
            collected.extend(page.items.iter().map(|x| x.id.clone()));
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(collected.len(), 4);
        let unique: std::collections::HashSet<_> = collected.iter().cloned().collect();
        assert_eq!(unique.len(), 4, "跨批不得重复");
    }

    /// All 模式下两表同游标归并、每批凑满 batch 条；10 条文本流式遍历恰好 10 条。
    #[tokio::test]
    async fn search_page_fills_batch_and_terminates() {
        let text_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard::init_db(&text_pool)
            .await
            .unwrap();
        let base = 10_000i64;
        for i in 0..10 {
            let at = base - i; // 递减时间 → id clip_{i} 与时间序一致
            blink_infra::data::clipboard::save_item(
                &text_pool,
                &blink_infra::data::clipboard::ClipboardItem {
                    id: format!("clip_{i}"),
                    text: format!("text {i}"),
                    preview: format!("text {i}"),
                    created_at: at,
                    source_app: None,
                    hit_count: 0,
                },
            )
            .await
            .unwrap();
        }
        let engine = ClipboardEngine::new(text_pool, unused_cache_pool().await);

        // 0.25.29 修复回归：batch=0（前端缺省）按位置兜底——首批 = 首屏两页
        // （page_size 9 × 2 = 18 > 10 全返回），追加批 = 一页 9 条
        let first = engine.search_page("", ClipKind::All, None, 0).await;
        assert_eq!(first.items.len(), 10, "首批兜底 18 条钳制到实际 10 条");
        assert!(!first.has_more, "数据取尽");

        let mut collected: Vec<String> = Vec::new();
        let mut cursor = None;
        let mut batches = 0;
        loop {
            let page = engine.search_page("", ClipKind::All, cursor, 3).await;
            batches += 1;
            collected.extend(page.items.iter().map(|x| x.id.clone()));
            // 每批凑满 3 条（末批 10 % 3 = 1 条），验证「归并取满一批」语义
            let expect = if page.has_more { 3 } else { 10 % 3 };
            assert_eq!(page.items.len(), expect, "批 {batches} 应凑满");
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(batches, 4, "3+3+3+1");
        assert_eq!(collected.len(), 10);
        assert_eq!(collected[0], "clipboard:clip_0");
        assert_eq!(collected[9], "clipboard:clip_9");
        let unique: std::collections::HashSet<_> = collected.iter().cloned().collect();
        assert_eq!(unique.len(), 10, "跨批不得重复");
    }

    /// batch=0 的追加批兜底为一页（cursor=Some 区分首批/追加）。
    #[tokio::test]
    async fn search_page_zero_batch_append_defaults_to_one_page() {
        let text_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard::init_db(&text_pool)
            .await
            .unwrap();
        let base = 30_000i64;
        for i in 0..30 {
            let at = base - i;
            blink_infra::data::clipboard::save_item(
                &text_pool,
                &blink_infra::data::clipboard::ClipboardItem {
                    id: format!("clip_{i}"),
                    text: format!("text {i}"),
                    preview: format!("text {i}"),
                    created_at: at,
                    source_app: None,
                    hit_count: 0,
                },
            )
            .await
            .unwrap();
        }
        let engine = ClipboardEngine::new(text_pool, unused_cache_pool().await);

        // 首批 batch=0 → 18 条（两页）
        let first = engine.search_page("", ClipKind::All, None, 0).await;
        assert_eq!(first.items.len(), 18, "首批缺省 = 首屏两页");
        assert!(first.has_more);
        let cursor = first.next_cursor.unwrap();
        // 追加 batch=0 → 9 条（一页）
        let second = engine.search_page("", ClipKind::All, Some(cursor), 0).await;
        assert_eq!(second.items.len(), 9, "追加缺省 = 一页 page_size");
        assert_eq!(second.items[0].id, "clipboard:clip_18", "追加从游标后继续");
    }

    /// 带 query 的流式：fuzzy 命中集跨游标分批不重不漏。
    #[tokio::test]
    async fn search_page_fuzzy_streams_without_dupes() {
        let text_pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard::init_db(&text_pool)
            .await
            .unwrap();
        // 带 query 路径走 retention_days（默认 30 天）窗口，数据必须新鲜
        let base = chrono::Utc::now().timestamp();
        for i in 0..5 {
            let at = base - i;
            blink_infra::data::clipboard::save_item(
                &text_pool,
                &blink_infra::data::clipboard::ClipboardItem {
                    id: format!("clip_{i}"),
                    text: if i == 0 {
                        "n e e d l e".into()
                    } else {
                        format!("needle {i}")
                    },
                    preview: if i == 0 {
                        "n e e d l e".into()
                    } else {
                        format!("needle {i}")
                    },
                    created_at: at,
                    source_app: None,
                    hit_count: 0,
                },
            )
            .await
            .unwrap();
        }
        let engine = ClipboardEngine::new(text_pool, unused_cache_pool().await);

        let mut collected: Vec<String> = Vec::new();
        let mut cursor = None;
        loop {
            let page = engine
                .search_page("needle", ClipKind::Text, cursor, 2)
                .await;
            collected.extend(page.items.iter().map(|x| x.id.clone()));
            match page.next_cursor {
                Some(c) => cursor = Some(c),
                None => break,
            }
        }
        assert_eq!(collected.len(), 5);
        let unique: std::collections::HashSet<_> = collected.iter().cloned().collect();
        assert_eq!(unique.len(), 5, "fuzzy 流式不得重复");
        assert_eq!(
            collected,
            (0..5)
                .map(|i| format!("clipboard:clip_{i}"))
                .collect::<Vec<_>>(),
            "弱匹配的新条目不能被较旧高分匹配挤出后由游标跳过"
        );
    }

    /// 测试用空 cache pool（图片表无数据场景；All 合并只出文本）。
    async fn unused_cache_pool() -> SqlitePool {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard_images::init_db(&pool)
            .await
            .unwrap();
        pool
    }
}
