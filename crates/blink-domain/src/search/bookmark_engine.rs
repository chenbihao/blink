//! BookmarkEngine：浏览器书签搜索引擎（0.26.13，sync lane）。
//!
//! 后台读 Chromium 系书签文件建**纯内存索引**（不落 cache 库——静态数据、
//! 万条 JSON 解析毫秒级、重启重建无感；内存约每条 200-400B，条目上限默认 50k）。
//! 默认 `bm` keyword 独占搜索（`bm rust` fuzzy 匹配 title/url/文件夹路径），
//! `mix_into_global` 开启后额外参与常规混排（低 source boost，预筛池受限）。
//! 回车执行 `SearchAction::Open { path: url }` → 前端 `launchApp` 打开默认浏览器。
//!
//! **隐私边界**：书签属敏感数据——v1 只服务本地搜索，不注册 Capability、
//! 不进 AI/MCP 出口（将来暴露须带 `sensitive` 标记另立项）。
//!
//! **扫描节奏**（仿 StartMenuEngine `start()`）：启动后延迟首扫（不进 startup
//! 计量与关键路径）、mtime+size 指纹变化触发重扫（书签文件可能被原子重命名
//! 同秒替换，故比 StartMenuEngine 多比对 size）、空闲周期复查、防抖合并
//! （单飞 generation 守卫）、文件 IO 全部 `spawn_blocking`。

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use crate::config::shards::BookmarksConfig;
use crate::intent::KeywordOwner;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use super::weblinks_engine::{KeywordConflict, KeywordConflictKind};

pub const ENGINE_ID: &str = "bookmarks";

/// 独占首屏上限（对齐其他引擎的融合前截断）。
const TAKEOVER_LIMIT: usize = 50;
/// 混排预筛池上限：仅最近 N 条参与 fuzzy（保护热路径，2026-10 定案）。
pub const MIXED_PREFILTER_POOL: usize = 200;
/// 混排返回上限。
const MIXED_LIMIT: usize = 10;
/// 周期复查间隔（对齐 StartMenuEngine CHECK_INTERVAL）。
const CHECK_INTERVAL: Duration = Duration::from_secs(300);
/// 启动后延迟首扫：避开 startup 关键路径与热身期 IO 争用。
const FIRST_SCAN_DELAY: Duration = Duration::from_secs(2);
/// 文件夹递归深度护栏（正常书签树 < 20 层，防御异常深嵌套 JSON）。
const MAX_FOLDER_DEPTH: usize = 64;

// ── 纯函数：Chromium 书签解析 ────────────────────────────────────────────────

/// Chromium `date_added`（1601 epoch 微秒字符串）→ unix 秒。
///
/// 溢出/非数字/负值返回 None（该条仍保留，仅无时间信息）。
pub fn chrome_date_added_to_unix(raw: &str) -> Option<i64> {
    let us: i128 = raw.trim().parse().ok()?;
    const WINDOWS_TO_UNIX_EPOCH_US: i128 = 11_644_473_600_000_000;
    if us < WINDOWS_TO_UNIX_EPOCH_US {
        return None; // 1601 epoch 前无效
    }
    let secs = (us - WINDOWS_TO_UNIX_EPOCH_US) / 1_000_000;
    i64::try_from(secs).ok()
}

/// 解析 `Local State` 的 `profile.info_cache` 锚点，返回全部 profile 目录名。
///
/// 解析失败返回空 Vec——调用方回落 `Default` profile（Chrome 最小安装形态）。
pub fn parse_local_state_profiles(json: &str) -> Vec<String> {
    let value: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let Some(cache) = value["profile"]["info_cache"].as_object() else {
        return Vec::new();
    };
    let mut names: Vec<String> = cache.keys().cloned().collect();
    names.sort();
    names
}

/// 内存索引行。
#[derive(Debug, Clone, PartialEq)]
pub struct BookmarkEntry {
    /// 稳定 id（`{browser}/{profile}/{序号}`，索引代内唯一即可——无历史权重，
    /// `lnk_path` 主键语义不适用，不参与历史加权）。
    pub id: String,
    pub title: String,
    pub url: String,
    /// 文件夹路径（如 `书签栏/开发/Rust`），subtitle 展示 + 参与匹配。
    pub folder_path: String,
    /// 添加时间（unix 秒；源缺失为 0，排序时沉底）。
    pub added_at: i64,
}

/// 递归收集一个书签节点树（`roots.bookmark_bar/other/synced` 同构）。
///
/// `folder` 为当前文件夹栈（不含根名）；`type: "url"` 且带 url 的子节点入库。
/// 预算只计 URL 条目（「条目上限」指书签数，文件夹不计）；预算耗尽后再遇到
/// 的有效条目把 `overflow` 置位（区分「恰好用满」与「真的截断」）。
fn collect_children(
    node: &serde_json::Value,
    folder: &mut Vec<String>,
    prefix: &str,
    remaining: &mut usize,
    overflow: &mut bool,
    out: &mut Vec<BookmarkEntry>,
) {
    if folder.len() >= MAX_FOLDER_DEPTH {
        return;
    }
    let Some(children) = node["children"].as_array() else {
        return;
    };
    for child in children {
        let name = child["name"].as_str().unwrap_or_default().to_string();
        if child["type"].as_str() == Some("folder") {
            folder.push(name);
            collect_children(child, folder, prefix, remaining, overflow, out);
            folder.pop();
            continue;
        }
        let Some(url) = child["url"].as_str() else {
            continue;
        };
        if url.is_empty() {
            continue;
        }
        if *remaining == 0 {
            *overflow = true;
            continue;
        }
        *remaining -= 1;
        let title = if name.is_empty() { url.to_string() } else { name };
        out.push(BookmarkEntry {
            id: format!("{prefix}{}", out.len()),
            title,
            url: url.to_string(),
            folder_path: folder.join("/"),
            added_at: child["date_added"]
                .as_str()
                .and_then(chrome_date_added_to_unix)
                .unwrap_or(0),
        });
    }
}

/// `parse_bookmarks_json` 的结果：条目 + 是否真的截断（预算耗尽后仍有有效条目）。
pub struct ParsedBookmarks {
    pub entries: Vec<BookmarkEntry>,
    pub truncated: bool,
}

/// 解析单个 `Bookmarks` JSON 文件为条目（纯函数，不碰磁盘）。
///
/// 损坏 JSON 返回空——调用方按 profile 降级跳过并 warn。
pub fn parse_bookmarks_json(
    json: &str,
    browser: &str,
    profile: &str,
    max_entries: usize,
) -> ParsedBookmarks {
    let value: serde_json::Value = serde_json::from_str(json).unwrap_or(serde_json::Value::Null);
    let Some(roots) = value["roots"].as_object() else {
        return ParsedBookmarks { entries: Vec::new(), truncated: false };
    };
    let prefix = format!("{browser}/{profile}/");
    let mut out = Vec::new();
    let mut remaining = max_entries;
    let mut overflow = false;
    let mut folder = Vec::new();
    // 三个根固定收录（synced 可能缺席，跳过即可）。
    for key in ["bookmark_bar", "other", "synced"] {
        if let Some(root) = roots.get(key) {
            collect_children(root, &mut folder, &prefix, &mut remaining, &mut overflow, &mut out);
        }
    }
    ParsedBookmarks { entries: out, truncated: overflow }
}

// ── Provider：浏览器数据源 ────────────────────────────────────────────────────

/// 源文件指纹（mtime 毫秒 + size）：书签文件可能被原子重命名同秒替换，
/// 仅比较 mtime 不够（StartMenuEngine 现状），须加 size。
#[derive(Debug, Clone, PartialEq)]
pub struct FileFingerprint {
    pub path: String,
    pub mtime_ms: u128,
    pub size: u64,
}

fn fingerprint_of(path: &Path) -> Option<FileFingerprint> {
    let meta = std::fs::metadata(path).ok()?;
    let mtime_ms = meta
        .modified()
        .ok()?
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_millis();
    Some(FileFingerprint {
        path: path.to_string_lossy().to_string(),
        mtime_ms,
        size: meta.len(),
    })
}

/// 指纹集合是否变化（顺序无关——目录枚举顺序不稳定）。
pub fn fingerprints_changed(current: &[FileFingerprint], cached: &[FileFingerprint]) -> bool {
    if current.len() != cached.len() {
        return true;
    }
    let mut a: Vec<&FileFingerprint> = current.iter().collect();
    let mut b: Vec<&FileFingerprint> = cached.iter().collect();
    a.sort_by_key(|f| &f.path);
    b.sort_by_key(|f| &f.path);
    a != b
}

/// Chromium 系浏览器的 `User Data` 根目录（相对 `%LOCALAPPDATA%`）。
/// Chrome/Edge/Brave 仅根目录不同（§5.14 已定）。
pub fn browser_user_data_dir(base: &Path, browser: &str) -> Option<PathBuf> {
    let rel = match browser.to_ascii_lowercase().as_str() {
        "chrome" => "Google/Chrome/User Data",
        "edge" => "Microsoft/Edge/User Data",
        "brave" => "BraveSoftware/Brave-Browser/User Data",
        _ => return None,
    };
    Some(base.join(rel))
}

/// 枚举某浏览器全部 profile 的书签文件（轻量 metadata，不解析内容）。
/// 返回 (逻辑浏览器 id, profile 名, 文件指纹)；书签文件不存在的 profile 不列。
fn browser_bookmark_files(base: &Path, browser: &str) -> Vec<(String, String, FileFingerprint)> {
    let Some(user_data) = browser_user_data_dir(base, browser) else {
        return Vec::new();
    };
    let profiles = std::fs::read_to_string(user_data.join("Local State"))
        .map(|s| parse_local_state_profiles(&s))
        .unwrap_or_default();
    let profiles = if profiles.is_empty() {
        vec!["Default".to_string()]
    } else {
        profiles
    };
    let browser_id = browser.to_ascii_lowercase();
    profiles
        .into_iter()
        .filter_map(|profile| {
            let fp = fingerprint_of(&user_data.join(&profile).join("Bookmarks"))?;
            Some((browser_id.clone(), profile, fp))
        })
        .collect()
}

/// 一次扫描的结果。
pub struct BookmarkScan {
    pub entries: Vec<BookmarkEntry>,
    pub fingerprints: Vec<FileFingerprint>,
    pub truncated: bool,
}

/// 书签数据源抽象（Firefox places.sqlite 拷贝只读方案留后续子版本）。
pub trait BookmarkProvider: Send + Sync {
    fn id(&self) -> &'static str;
    /// 全量扫描（阻塞 IO，调用方负责放 `spawn_blocking`）。
    ///
    /// `cached` 为上次索引的指纹、`force=true` 表示配置变化须强制重建；
    /// 指纹未变且非强制时返回 None（调用方保持现索引）。
    fn scan_if_changed(
        &self,
        browsers: &[String],
        cached: &[FileFingerprint],
        force: bool,
        max_entries: usize,
    ) -> Option<BookmarkScan>;
}

/// Chromium `Local State` + `Bookmarks` JSON 读取器。
///
/// `base` 为 `%LOCALAPPDATA%`（生产）或测试注入的临时目录。
pub struct ChromiumJsonProvider {
    base: PathBuf,
}

impl ChromiumJsonProvider {
    pub fn with_base(base: PathBuf) -> Self {
        Self { base }
    }
}

impl Default for ChromiumJsonProvider {
    fn default() -> Self {
        // 生产：`%LOCALAPPDATA%`。缺环境变量（极少数服务上下文）时扫描空集。
        let base = std::env::var("LOCALAPPDATA")
            .map(PathBuf::from)
            .unwrap_or_default();
        Self { base }
    }
}

impl BookmarkProvider for ChromiumJsonProvider {
    fn id(&self) -> &'static str {
        "chromium_json"
    }

    fn scan_if_changed(
        &self,
        browsers: &[String],
        cached: &[FileFingerprint],
        force: bool,
        max_entries: usize,
    ) -> Option<BookmarkScan> {
        let files: Vec<(String, String, FileFingerprint)> = browsers
            .iter()
            .flat_map(|b| browser_bookmark_files(&self.base, b))
            .collect();
        let fingerprints: Vec<FileFingerprint> =
            files.iter().map(|(_, _, fp)| fp.clone()).collect();
        if !force && !fingerprints_changed(&fingerprints, cached) {
            return None;
        }
        let mut entries = Vec::new();
        let mut truncated = false;
        for (browser, profile, fp) in &files {
            if entries.len() >= max_entries {
                truncated = true;
                break;
            }
            let path = PathBuf::from(&fp.path);
            let json = match std::fs::read_to_string(&path) {
                Ok(s) => s,
                Err(error) => {
                    // 按 profile 降级跳过：单文件读失败不影响其他 profile。
                    tracing::warn!(path = %path.display(), %error, "书签文件读取失败，该 profile 跳过");
                    continue;
                }
            };
            let budget = max_entries - entries.len();
            let parsed = parse_bookmarks_json(&json, browser, profile, budget);
            truncated |= parsed.truncated;
            entries.extend(parsed.entries);
        }
        if truncated {
            tracing::warn!(count = entries.len(), max = max_entries, "书签条目达上限，超出部分截断");
        }
        Some(BookmarkScan { entries, fingerprints, truncated })
    }
}

// ── 单 keyword 校验（与 weblinks 冲突校验同构） ──────────────────────────────

/// 校验书签触发词：空/含空白、保留前缀 `ai`、被 engine/插件触发词占用。
///
/// 复用 weblinks 的冲突类型（前端按 kind 做 i18n，不解析 reason 字符串）。
pub fn validate_keyword(
    keyword: &str,
    owner_of: &dyn Fn(&str) -> Option<KeywordOwner>,
) -> Result<(), KeywordConflict> {
    let kw = keyword.trim();
    let kind = if kw.is_empty() || kw.chars().any(char::is_whitespace) {
        KeywordConflictKind::Invalid
    } else if kw.eq_ignore_ascii_case("ai") {
        KeywordConflictKind::Reserved
    } else {
        return match owner_of(kw) {
            None => Ok(()),
            Some(owner) => Err(KeywordConflict {
                entry_id: ENGINE_ID.to_string(),
                keyword: kw.to_string(),
                kind: match &owner {
                    KeywordOwner::Engine(_) => KeywordConflictKind::TakenByEngine,
                    KeywordOwner::Plugin(_) => KeywordConflictKind::TakenByPlugin,
                },
                owner: Some(match owner {
                    KeywordOwner::Engine(id) | KeywordOwner::Plugin(id) => id,
                }),
            }),
        };
    };
    Err(KeywordConflict {
        entry_id: ENGINE_ID.to_string(),
        keyword: kw.to_string(),
        kind,
        owner: None,
    })
}

// ── 引擎 ─────────────────────────────────────────────────────────────────────

/// 已发布索引快照（查询只读它，冷启动/重扫不阻塞输入链路）。
#[derive(Default)]
struct IndexState {
    entries: Vec<BookmarkEntry>,
    fingerprints: Vec<FileFingerprint>,
    truncated: bool,
}

struct ScanWork {
    generation: u64,
    building: bool,
    force: bool,
}

/// 设置页状态投影。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BookmarkStatus {
    pub entries: usize,
    pub truncated: bool,
    pub scanning: bool,
}

#[derive(Clone)]
pub struct BookmarkEngine {
    config: Arc<RwLock<BookmarksConfig>>,
    cache: Arc<RwLock<IndexState>>,
    work: Arc<RwLock<ScanWork>>,
    provider: Arc<dyn BookmarkProvider>,
}

impl BookmarkEngine {
    pub fn with_config(config: BookmarksConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            cache: Arc::new(RwLock::new(IndexState::default())),
            work: Arc::new(RwLock::new(ScanWork { generation: 0, building: false, force: true })),
            provider: Arc::new(ChromiumJsonProvider::default()),
        }
    }

    /// 测试构造：注入 provider（临时目录/固定数据）。
    fn with_provider(config: BookmarksConfig, provider: Arc<dyn BookmarkProvider>) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
            cache: Arc::new(RwLock::new(IndexState::default())),
            work: Arc::new(RwLock::new(ScanWork { generation: 0, building: false, force: true })),
            provider,
        }
    }

    /// 更新配置（保存 bookmarks 分片时由 `SearchService::update_engine_config` 转发）。
    ///
    /// 扫描相关字段（enabled/browsers/max_entries）变化 → 强制重扫；
    /// keyword/mix_into_global 只影响路由与召回，无需重扫。
    pub fn update_config(&self, config: BookmarksConfig) {
        let rescan = {
            let mut current = self.config.write().unwrap();
            let rescan = current.enabled != config.enabled
                || current.browsers != config.browsers
                || current.max_entries != config.max_entries;
            *current = config;
            rescan
        };
        if rescan {
            self.work.write().unwrap().force = true;
            self.refresh();
        }
    }

    /// 设置页状态投影（只读，无副作用）。
    pub fn status(&self) -> BookmarkStatus {
        let cache = self.cache.read().unwrap();
        BookmarkStatus {
            entries: cache.entries.len(),
            truncated: cache.truncated,
            scanning: self.work.read().unwrap().building,
        }
    }

    /// 首扫 + 周期复查（`SearchService::start` 调用；IO 不进 startup 计量）。
    fn start_background(&self) {
        let delayed = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(FIRST_SCAN_DELAY).await;
            delayed.refresh();
        });
        let engine = self.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;
                engine.refresh();
            }
        });
    }

    /// 防抖合并：building 期间的新请求置 force，由本次完成后的 stale 检查续扫。
    fn refresh(&self) {
        let (config, generation, force, cached) = {
            let config = self.config.read().unwrap().clone();
            if !config.enabled {
                return;
            }
            let mut state = self.work.write().unwrap();
            if state.building {
                state.force = true;
                return;
            }
            state.building = true;
            let force = state.force;
            state.force = false;
            let cached = self.cache.read().unwrap().fingerprints.clone();
            (config, state.generation, force, cached)
        };
        let engine = self.clone();
        let provider = self.provider.clone();
        tokio::spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                provider.scan_if_changed(&config.browsers, &cached, force, config.max_entries as usize)
            })
            .await;
            match result {
                Ok(Some(scan)) => {
                    if engine.finish(generation, Some(scan)) {
                        engine.refresh();
                    }
                }
                Ok(None) => {
                    // 指纹未变：仅释放单飞标记。
                    engine.finish(generation, None);
                }
                Err(error) => {
                    tracing::warn!(%error, generation, "书签扫描任务失败");
                    engine.finish_failed(generation);
                }
            }
        });
    }

    /// 发布扫描结果；返回 true = generation 过期（配置已变），调用方续扫。
    fn finish(&self, generation: u64, scan: Option<BookmarkScan>) -> bool {
        let mut state = self.work.write().unwrap();
        state.building = false;
        let stale = state.generation != generation;
        if !stale {
            if let Some(scan) = scan {
                let mut cache = self.cache.write().unwrap();
                cache.entries = scan.entries;
                cache.fingerprints = scan.fingerprints;
                cache.truncated = scan.truncated;
            }
        }
        stale
    }

    fn finish_failed(&self, generation: u64) {
        let mut state = self.work.write().unwrap();
        state.building = false;
        // JoinError 极罕见（panic/取消）；置 force 让周期复查重试。
        if state.generation == generation {
            state.force = true;
        }
    }

    fn entries(&self) -> Vec<BookmarkEntry> {
        self.cache.read().unwrap().entries.clone()
    }
}

// ── fuzzy 匹配 ───────────────────────────────────────────────────────────────

/// title/url/文件夹路径 + title 拼音首字母四路 fuzzy，取最高分。
///
/// 复用 search 域的 nucleo 原语（与 `fuzzy_entry_scores` 同构）。
fn fuzzy_score_entry(
    pattern: &nucleo::pattern::Pattern,
    matcher: &mut nucleo::Matcher,
    entry: &BookmarkEntry,
    title_initials: &str,
) -> u32 {
    let mut buf = Vec::new();
    [
        entry.title.as_str(),
        entry.url.as_str(),
        entry.folder_path.as_str(),
        title_initials,
    ]
    .into_iter()
    .map(|field| {
        let haystack = nucleo::Utf32Str::new(field, &mut buf);
        pattern.score(haystack, matcher).unwrap_or(0)
    })
    .max()
    .unwrap_or(0)
}

fn fuzzy_pattern(query: &str) -> (nucleo::pattern::Pattern, nucleo::Matcher) {
    let query_lower = query.to_ascii_lowercase();
    let pattern = nucleo::pattern::Pattern::new(
        &query_lower,
        nucleo::pattern::CaseMatching::Smart,
        nucleo::pattern::Normalization::Smart,
        nucleo::pattern::AtomKind::Fuzzy,
    );
    (pattern, nucleo::Matcher::new(nucleo::Config::DEFAULT))
}

fn open_item(entry: &BookmarkEntry, score: f32) -> SearchItem {
    SearchItem {
        id: entry.id.clone(),
        title: entry.title.clone(),
        subtitle: Some(entry.folder_path.clone()),
        score,
        action: SearchAction::Open { path: entry.url.clone() },
        source: ENGINE_ID.to_string(),
        score_detail: None,
        context_aware: false,
        color_list_hex: None,
    }
}

#[async_trait::async_trait]
impl SearchEngine for BookmarkEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn start(&self) {
        self.start_background();
    }

    /// 默认 keyword 独占（§5.14 已定）。`mix_into_global` 开启即参与混排——
    /// 读运行时配置快照，配置热切换后 Mixed/独占行为即时生效（0.26.13 语义）。
    fn takeover_only(&self) -> bool {
        !self.config.read().unwrap().mix_into_global
    }

    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let config = self.config.read().unwrap().clone();
        if !config.enabled {
            return Vec::new();
        }

        // ── Takeover 路径：keyword 命中独占，query 是剥离后的参数 ──────────
        if let Some(kw) = ctx.takeover_keyword {
            if !kw.trim().eq_ignore_ascii_case(config.keyword.trim()) {
                // 注册词与配置短暂不一致的窗口（重注册竞态）——静默空返回。
                tracing::debug!(keyword = kw, "bookmarks keyword 与配置不一致");
                return Vec::new();
            }
            let entries = self.entries();
            let arg = query.trim();
            if arg.is_empty() {
                // 独占空参：按时间倒序首屏（无历史权重，tie-break date_added）。
                let mut by_time = entries;
                by_time.sort_by(|a, b| b.added_at.cmp(&a.added_at).then_with(|| a.id.cmp(&b.id)));
                return by_time
                    .into_iter()
                    .take(TAKEOVER_LIMIT)
                    .map(|e| open_item(&e, 1.0))
                    .collect();
            }
            return fuzzy_ranked(entries, arg, TAKEOVER_LIMIT);
        }

        // ── Mixed 路径：mix_into_global 开启后参与（预筛池受限） ────────────
        if !config.mix_into_global {
            return Vec::new();
        }
        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let entries = self.entries();
        if entries.is_empty() {
            return Vec::new();
        }
        // 预筛池：最近 MIXED_PREFILTER_POOL 条（按时间倒序），保护热路径。
        let mut pool: Vec<&BookmarkEntry> = entries.iter().collect();
        pool.sort_by(|a, b| b.added_at.cmp(&a.added_at).then_with(|| a.id.cmp(&b.id)));
        pool.truncate(MIXED_PREFILTER_POOL);
        fuzzy_ranked_borrowed(pool, q, MIXED_LIMIT)
    }
}

/// fuzzy 打分 + 排序（独占与混排共用）：分降序、tie-break 新书签优先。
fn fuzzy_ranked(entries: Vec<BookmarkEntry>, query: &str, limit: usize) -> Vec<SearchItem> {
    let pool: Vec<&BookmarkEntry> = entries.iter().collect();
    fuzzy_ranked_borrowed(pool, query, limit)
}

fn fuzzy_ranked_borrowed(pool: Vec<&BookmarkEntry>, query: &str, limit: usize) -> Vec<SearchItem> {
    let (pattern, mut matcher) = fuzzy_pattern(query);
    let mut scored: Vec<(u32, &BookmarkEntry)> = pool
        .into_iter()
        .filter_map(|e| {
            let initials = super::to_pinyin_initials(&e.title);
            let score = fuzzy_score_entry(&pattern, &mut matcher, e, &initials);
            (score > 0).then_some((score, e))
        })
        .collect();
    if scored.is_empty() {
        return Vec::new();
    }
    scored.sort_by(|a, b| {
        b.0.cmp(&a.0)
            .then_with(|| b.1.added_at.cmp(&a.1.added_at))
            .then_with(|| a.1.id.cmp(&b.1.id))
    });
    let best = scored[0].0;
    scored
        .into_iter()
        .take(limit)
        .map(|(raw, e)| open_item(e, raw as f32 / best as f32))
        .collect()
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blink_infra::platform::context::ContextSnapshot;

    fn ctx<'a>(
        history: &'a std::collections::HashMap<String, (i64, i64)>,
        snapshot: &'a ContextSnapshot,
        takeover_keyword: Option<&'a str>,
    ) -> QueryContext<'a> {
        QueryContext {
            history,
            snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
            takeover_keyword,
        }
    }

    async fn run(engine: &BookmarkEngine, query: &str, kw: Option<&str>) -> Vec<SearchItem> {
        let h = std::collections::HashMap::new();
        let snap = ContextSnapshot::default();
        engine.search(query, &ctx(&h, &snap, kw)).await
    }

    fn entry(id: &str, title: &str, url: &str, folder: &str, added: i64) -> BookmarkEntry {
        BookmarkEntry {
            id: id.into(),
            title: title.into(),
            url: url.into(),
            folder_path: folder.into(),
            added_at: added,
        }
    }

    fn engine_with(entries: Vec<BookmarkEntry>) -> BookmarkEngine {
        let e = BookmarkEngine::with_config(BookmarksConfig::default());
        {
            let mut cache = e.cache.write().unwrap();
            cache.entries = entries;
            cache.fingerprints = vec![FileFingerprint {
                path: "fixture".into(),
                mtime_ms: 1,
                size: 1,
            }];
        }
        e
    }

    // ── 时间戳转换 ──────────────────────────────────────────────────────

    #[test]
    fn chrome_epoch_converts_to_unix_seconds() {
        // 1601-01-01 00:00:00 UTC → unix 0
        assert_eq!(chrome_date_added_to_unix("11644473600000000"), Some(0));
        // unix 1767225600（2026-01-01）往返
        assert_eq!(
            chrome_date_added_to_unix(&(1_767_225_600i128 * 1_000_000 + 11_644_473_600_000_000).to_string()),
            Some(1_767_225_600)
        );
        assert_eq!(chrome_date_added_to_unix("0"), None, "1601 前为负值");
        assert_eq!(chrome_date_added_to_unix("abc"), None);
        assert_eq!(chrome_date_added_to_unix(""), None);
        assert_eq!(
            chrome_date_added_to_unix("99999999999999999999999999999"),
            None,
            "秒值溢出 i64"
        );
    }

    // ── Local State / Bookmarks JSON 解析 ───────────────────────────────

    #[test]
    fn local_state_enumerates_profiles_from_info_cache() {
        let json = r#"{
            "profile": {"info_cache": {
                "Profile 2": {"name": "工作"},
                "Default": {"name": "个人"}
            }}
        }"#;
        assert_eq!(parse_local_state_profiles(json), ["Default", "Profile 2"]);
        // 损坏 / 缺字段 → 空（调用方回落 Default）
        assert!(parse_local_state_profiles("not json").is_empty());
        assert!(parse_local_state_profiles("{}").is_empty());
    }

    const SAMPLE_BOOKMARKS: &str = r#"{
        "roots": {
            "bookmark_bar": {
                "name": "书签栏", "type": "folder",
                "children": [
                    {"date_added": "13390000000000000", "name": "Rust 官网", "type": "url", "url": "https://rust-lang.org"},
                    {"date_added": "13390000000000001", "name": "开发", "type": "folder", "children": [
                        {"date_added": "13390000000000002", "name": "docs", "type": "url", "url": "https://doc.rust-lang.org"},
                        {"date_added": "13390000000000003", "name": "", "type": "url", "url": "https://example.com"}
                    ]}
                ]
            },
            "other": {
                "name": "其他书签", "type": "folder",
                "children": [
                    {"date_added": "13390000000000004", "name": "News", "type": "url", "url": "https://news.ycombinator.com"}
                ]
            },
            "synced": {
                "name": "移动设备书签", "type": "folder",
                "children": [
                    {"date_added": "13390000000000005", "name": "Mobile", "type": "url", "url": "https://m.example.com"}
                ]
            }
        },
        "version": 1
    }"#;

    #[test]
    fn bookmarks_json_parses_nested_folders_and_three_roots() {
        let parsed = parse_bookmarks_json(SAMPLE_BOOKMARKS, "chrome", "Default", 100);
        let entries = &parsed.entries;
        assert_eq!(entries.len(), 5);
        assert!(!parsed.truncated);
        assert_eq!(entries[0].title, "Rust 官网");
        assert_eq!(entries[0].folder_path, "");
        assert_eq!(entries[1].folder_path, "开发");
        assert_eq!(entries[1].id, "chrome/Default/1");
        // 空标题回落 URL
        assert_eq!(entries[2].title, "https://example.com");
        // other / synced 根也收集
        assert!(entries.iter().any(|e| e.title == "News"));
        assert!(entries.iter().any(|e| e.title == "Mobile"));
        // 时间转换非 0
        assert!(entries[0].added_at > 1_700_000_000);
    }

    #[test]
    fn bookmarks_json_budget_truncates_url_entries_only() {
        // 上限 2：只计 URL 条目（文件夹「开发」不占预算）→ Rust + docs，且确实截断。
        let parsed = parse_bookmarks_json(SAMPLE_BOOKMARKS, "chrome", "Default", 2);
        assert_eq!(parsed.entries.len(), 2);
        assert_eq!(parsed.entries[0].title, "Rust 官网");
        assert_eq!(parsed.entries[1].title, "docs");
        assert!(parsed.truncated, "预算耗尽后仍有条目");

        // 上限 5：恰好收完 5 条、无溢出 → 不算截断。
        let exact = parse_bookmarks_json(SAMPLE_BOOKMARKS, "chrome", "Default", 5);
        assert_eq!(exact.entries.len(), 5);
        assert!(!exact.truncated, "恰好用满不是截断");
    }

    #[test]
    fn corrupt_bookmarks_json_returns_empty_for_profile_degrade() {
        assert!(parse_bookmarks_json("{ broken", "chrome", "Default", 10).entries.is_empty());
        assert!(parse_bookmarks_json("{}", "chrome", "Default", 10).entries.is_empty());
        // 无 url 字段的节点跳过
        let no_url = r#"{"roots":{"bookmark_bar":{"children":[{"name":"x","type":"url"}]}}}"#;
        assert!(parse_bookmarks_json(no_url, "chrome", "Default", 10).entries.is_empty());
    }

    // ── Provider 扫描（临时目录夹具） ────────────────────────────────────

    #[test]
    fn chromium_provider_scans_multi_browser_multi_profile_with_degrade() {
        let dir = std::env::temp_dir().join(format!("blink-bm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        // chrome：两个 profile，其中一个书签文件损坏（降级跳过）
        let chrome = dir.join("Google/Chrome/User Data");
        std::fs::create_dir_all(chrome.join("Default")).unwrap();
        std::fs::create_dir_all(chrome.join("Profile 1")).unwrap();
        std::fs::write(
            chrome.join("Local State"),
            r#"{"profile":{"info_cache":{"Default":{},"Profile 1":{}}}}"#,
        )
        .unwrap();
        std::fs::write(chrome.join("Default/Bookmarks"), SAMPLE_BOOKMARKS).unwrap();
        std::fs::write(chrome.join("Profile 1/Bookmarks"), "{ corrupt").unwrap();
        // edge：无 Local State（回落 Default），书签文件存在
        let edge = dir.join("Microsoft/Edge/User Data/Default");
        std::fs::create_dir_all(&edge).unwrap();
        std::fs::write(edge.join("Bookmarks"), SAMPLE_BOOKMARKS).unwrap();
        // brave：目录不存在（整体跳过）

        let provider = ChromiumJsonProvider::with_base(dir.clone());
        let scan = provider
            .scan_if_changed(&["chrome".into(), "edge".into(), "brave".into()], &[], true, 100)
            .expect("强制首扫应返回结果");
        // chrome Default 5 条 + edge Default 5 条；Profile 1 损坏降级为 0 条
        assert_eq!(scan.entries.len(), 10);
        assert!(!scan.truncated);
        // 指纹记录存在的文件（损坏文件同样有指纹——指纹跟文件身份，不跟内容可解析性）
        assert_eq!(scan.fingerprints.len(), 3);
        // 条目 id 前缀带浏览器与 profile
        assert!(scan.entries.iter().all(|e| e.id.starts_with("chrome/Default/")
            || e.id.starts_with("edge/Default/")));

        // 指纹未变 → None（周期复查短路，不重解析）
        assert!(
            provider
                .scan_if_changed(&["chrome".into(), "edge".into()], &scan.fingerprints, false, 100)
                .is_none()
        );

        // 条目上限跨浏览器累计
        let capped = provider
            .scan_if_changed(&["chrome".into(), "edge".into()], &[], true, 7)
            .expect("强制扫描");
        assert_eq!(capped.entries.len(), 7);
        assert!(capped.truncated);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn browser_roots_map_chromium_vendors() {
        let base = Path::new("C:/x");
        assert_eq!(
            browser_user_data_dir(base, "chrome"),
            Some(PathBuf::from("C:/x/Google/Chrome/User Data"))
        );
        assert_eq!(
            browser_user_data_dir(base, "EDGE"),
            Some(PathBuf::from("C:/x/Microsoft/Edge/User Data"))
        );
        assert_eq!(
            browser_user_data_dir(base, "brave"),
            Some(PathBuf::from("C:/x/BraveSoftware/Brave-Browser/User Data"))
        );
        assert_eq!(browser_user_data_dir(base, "firefox"), None);
    }

    // ── 指纹与重扫触发 ───────────────────────────────────────────────────

    #[test]
    fn fingerprints_detect_same_second_size_change_and_reorder_stability() {
        let a = [
            FileFingerprint { path: "a".into(), mtime_ms: 1000, size: 10 },
            FileFingerprint { path: "b".into(), mtime_ms: 2000, size: 20 },
        ];
        assert!(!fingerprints_changed(&a, &a));
        // 顺序无关
        let reordered = [a[1].clone(), a[0].clone()];
        assert!(!fingerprints_changed(&a, &reordered));
        // 同 mtime 不同 size（原子重命名同秒替换）→ 变化
        let size_changed = [
            FileFingerprint { path: "a".into(), mtime_ms: 1000, size: 11 },
            a[1].clone(),
        ];
        assert!(fingerprints_changed(&a, &size_changed));
        // 新增/删除文件
        assert!(fingerprints_changed(&a, &[a[0].clone()]));
    }

    // ── 搜索行为 ────────────────────────────────────────────────────────

    #[tokio::test]
    async fn takeover_empty_arg_lists_recent_first() {
        let e = engine_with(vec![
            entry("0", "Old", "https://old.test", "", 100),
            entry("1", "New", "https://new.test", "dev", 200),
            entry("2", "Mid", "https://mid.test", "", 150),
        ]);
        let items = run(&e, "", Some("bm")).await;
        assert_eq!(
            items.iter().map(|i| i.title.clone()).collect::<Vec<_>>(),
            ["New", "Mid", "Old"]
        );
        assert_eq!(items[0].subtitle.as_deref(), Some("dev"));
        assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "https://new.test"));
        assert_eq!(items[0].source, "bookmarks");
    }

    #[tokio::test]
    async fn takeover_fuzzy_matches_title_url_folder_and_pinyin() {
        let e = engine_with(vec![
            entry("0", "Rust 语言官网", "https://rust-lang.org", "开发/语言", 100),
            entry("1", "Hacker News", "https://news.ycombinator.com", "读", 200),
            entry("2", "随便", "https://example.com", "开发", 300),
        ]);
        // 标题
        assert!(run(&e, "rust", Some("bm")).await.iter().any(|i| i.title.contains("Rust")));
        // URL
        assert!(run(&e, "ycom", Some("bm")).await.iter().any(|i| i.title == "Hacker News"));
        // 文件夹路径
        assert!(run(&e, "语言", Some("bm")).await.iter().any(|i| i.title.contains("Rust")));
        // 标题拼音首字母（Rust 语言官网 → rust yygw）
        assert!(run(&e, "yygw", Some("bm")).await.iter().any(|i| i.title.contains("Rust")));
        // 无命中
        assert!(run(&e, "zzzzz", Some("bm")).await.is_empty());
    }

    #[tokio::test]
    async fn takeover_keyword_mismatch_or_disabled_returns_empty() {
        let e = engine_with(vec![entry("0", "A", "https://a.test", "", 1)]);
        assert!(run(&e, "", Some("other")).await.is_empty(), "keyword 不匹配");
        let disabled = BookmarkEngine::with_config(BookmarksConfig {
            enabled: false,
            ..Default::default()
        });
        assert!(run(&disabled, "", Some("bm")).await.is_empty(), "禁用不召回");
    }

    #[tokio::test]
    async fn takeover_only_follows_runtime_mix_config() {
        let e = engine_with(vec![entry("0", "Rust", "https://r.test", "", 1)]);
        assert!(e.takeover_only(), "默认混排关 → 独占");
        // 热切换：配置即时生效
        e.update_config(BookmarksConfig { mix_into_global: true, ..Default::default() });
        assert!(!e.takeover_only(), "混排开后参与 Mixed");
    }

    #[tokio::test]
    async fn mixed_respects_switch_and_pool_limits() {
        let entries: Vec<BookmarkEntry> = (0..300)
            .map(|i| {
                entry(&i.to_string(), &format!("条目{i}"), &format!("https://e{i}.test"), "dev", i)
            })
            .collect();
        let e = engine_with(entries);
        // 混排关：Mixed 路径空返回
        assert!(run(&e, "条目", None).await.is_empty());
        e.update_config(BookmarksConfig { mix_into_global: true, ..Default::default() });
        let items = run(&e, "条目", None).await;
        assert!(!items.is_empty());
        assert!(items.len() <= MIXED_LIMIT, "返回上限 10: {}", items.len());
        // 预筛池只含最近 200 条：最老的第 0 条不该命中
        assert!(items.iter().all(|i| i.title != "条目0"));
        // 最高分归一
        assert!((items[0].score - 1.0).abs() < f32::EPSILON);
    }

    #[tokio::test]
    async fn fuzzy_tie_break_prefers_recent_bookmark() {
        let e = engine_with(vec![
            entry("0", "docs", "https://a.test", "", 100),
            entry("1", "docs", "https://b.test", "", 900),
        ]);
        let items = run(&e, "docs", Some("bm")).await;
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].id, "1", "同分 tie-break 新书签优先");
    }

    // ── keyword 校验 ────────────────────────────────────────────────────

    #[test]
    fn validate_keyword_rejects_invalid_reserved_and_taken() {
        assert!(validate_keyword("bm", &|_| None).is_ok());
        assert!(matches!(
            validate_keyword("go od", &|_| None).unwrap_err().kind,
            KeywordConflictKind::Invalid
        ));
        assert!(matches!(
            validate_keyword("", &|_| None).unwrap_err().kind,
            KeywordConflictKind::Invalid
        ));
        assert!(matches!(
            validate_keyword("ai", &|_| None).unwrap_err().kind,
            KeywordConflictKind::Reserved
        ));
        let err = validate_keyword("clip", &|_| Some(KeywordOwner::Engine("clipboard".into())))
            .unwrap_err();
        assert!(matches!(err.kind, KeywordConflictKind::TakenByEngine));
        assert_eq!(err.owner.as_deref(), Some("clipboard"));
        let err = validate_keyword(
            "翻译",
            &|_| Some(KeywordOwner::Plugin("builtin.translate".into())),
        )
        .unwrap_err();
        assert!(matches!(err.kind, KeywordConflictKind::TakenByPlugin));
    }

    // ── 扫描生命周期 ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn scan_populates_index_and_status() {
        let entries = vec![entry("0", "A", "https://a.test", "", 1)];
        let provider = Arc::new(CheckableProvider {
            entries: entries.clone(),
            scanned: Arc::new(RwLock::new(0)),
        });
        let e = BookmarkEngine::with_provider(BookmarksConfig::default(), provider.clone());
        assert_eq!(e.status().entries, 0);
        e.refresh();
        // 异步扫描——轮询等待完成（上限 2s）。
        for _ in 0..100 {
            if !e.work.read().unwrap().building && e.status().entries > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let st = e.status();
        assert_eq!(st.entries, 1);
        assert!(!st.scanning);
        assert_eq!(run(&e, "", Some("bm")).await.len(), 1);
    }

    /// 记录扫描次数的 provider（验证指纹短路不重扫）。
    struct CheckableProvider {
        entries: Vec<BookmarkEntry>,
        scanned: Arc<RwLock<i32>>,
    }
    impl BookmarkProvider for CheckableProvider {
        fn id(&self) -> &'static str {
            "checkable"
        }
        fn scan_if_changed(
            &self,
            _browsers: &[String],
            _cached: &[FileFingerprint],
            force: bool,
            _max: usize,
        ) -> Option<BookmarkScan> {
            if !force && !_cached.is_empty() {
                return None; // 指纹短路
            }
            *self.scanned.write().unwrap() += 1;
            Some(BookmarkScan {
                entries: self.entries.clone(),
                fingerprints: vec![FileFingerprint { path: "x".into(), mtime_ms: 1, size: 1 }],
                truncated: false,
            })
        }
    }

    #[tokio::test]
    async fn periodic_unchanged_fingerprints_skip_rescan() {
        let provider = Arc::new(CheckableProvider {
            entries: vec![entry("0", "A", "https://a.test", "", 1)],
            scanned: Arc::new(RwLock::new(0)),
        });
        let e = BookmarkEngine::with_provider(BookmarksConfig::default(), provider.clone());
        e.refresh();
        for _ in 0..100 {
            if !e.work.read().unwrap().building && e.status().entries > 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*provider.scanned.read().unwrap(), 1, "首扫一次");
        // 指纹未变：第二次 refresh 走短路，不重扫。
        e.refresh();
        for _ in 0..100 {
            if !e.work.read().unwrap().building {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_eq!(*provider.scanned.read().unwrap(), 1, "指纹未变不重扫");
    }

    #[tokio::test]
    async fn building_guard_debounces_and_stale_generation_discards() {
        let e = engine_with(vec![]);
        // 手动置 building：模拟在扫——新 refresh 只置 force 不再入扫。
        e.work.write().unwrap().building = true;
        e.refresh();
        assert!(e.work.read().unwrap().building, "单飞期间不重入");
        // 模拟扫描期间配置变化推进 generation：旧结果按 stale 丢弃。
        e.work.write().unwrap().generation += 1;
        let stale_scan = BookmarkScan {
            entries: vec![entry("x", "old", "https://old.test", "", 1)],
            fingerprints: vec![],
            truncated: false,
        };
        assert!(e.finish(0, Some(stale_scan)), "generation 过期返回 stale");
        assert_eq!(e.status().entries, 0, "过期结果不入索引");
    }

    /// 内存量级粗测（验收记录用）：5 万条索引构建 + 估算条目堆占用。
    #[test]
    fn fifty_thousand_entries_index_footprint() {
        let entries: Vec<BookmarkEntry> = (0..50_000)
            .map(|i| {
                entry(
                    &format!("chrome/Default/{i}"),
                    &format!("书签标题示例第{i}条"),
                    &format!("https://example.com/path/{i}"),
                    "书签栏/开发",
                    1_700_000_000 + i as i64,
                )
            })
            .collect();
        let start = std::time::Instant::now();
        let e = engine_with(entries);
        let build = start.elapsed();
        // 粗估堆占用：String 容量逐条求和（debug 下近似）。
        let bytes: usize = e
            .cache
            .read()
            .unwrap()
            .entries
            .iter()
            .map(|en| {
                en.id.capacity()
                    + en.title.capacity()
                    + en.url.capacity()
                    + en.folder_path.capacity()
                    + std::mem::size_of::<BookmarkEntry>()
            })
            .sum();
        tracing::info!(
            entries = 50_000,
            build_ms = build.as_millis() as u64,
            approx_heap_mb = bytes / 1024 / 1024,
            "书签索引内存量级（debug 单测粗估）"
        );
        // 万条级解析毫秒级、5 万条索引构建秒级以内（CI 机器余量 30s）。
        assert!(build < Duration::from_secs(30));
    }
}
