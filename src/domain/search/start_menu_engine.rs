//! StartMenuEngine:开始菜单应用搜索引擎(sync lane)。
//!
//! 持有应用索引缓存(从原 `cache.rs` 迁入为字段,见 0.2 设计 §2.4——数据结构不变,
//! 仅把所有者从模块全局换成引擎字段)。`start()` 启动后台预扫 + 定时增量刷新;
//! search 时对缓存做 fuzzy 打分 + top-relative 归一化(§2.3)。
//!
//! 所有文件 IO(scan_start_menu)都在 `spawn_blocking` 里跑,绝不阻塞 async runtime。
//!
//! 0.7.5 增量更新策略：
//! - 根目录 mtime 无变化 → 直接用缓存
//! - 根目录 mtime 有变化 → 增量扫描（遍历目录对比缓存，仅增删变化项）
//! - 连续 10 次增量后 → 强制执行一次全量扫描兜底
//! - 每 2 小时 → 后台静默全量刷新

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, Instant, SystemTime};

use crate::app::config::StartMenuConfig;

use super::AppEntry;
use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use super::scorer::normalize_top_relative;

/// 搜索结果上限(融合前每引擎各自截断)。
const ENGINE_LIMIT: usize = 50;
/// 定时检查间隔。
const CHECK_INTERVAL: Duration = Duration::from_secs(300); // 5 分钟
/// 连续增量次数后强制全量刷新。
const FORCE_FULL_AFTER_INCREMENTAL: u32 = 10;
/// 全量刷新间隔（2 小时）。
const FULL_REFRESH_INTERVAL: Duration = Duration::from_secs(7200);

/// 缓存条目（含文件元数据，用于增量对比）。
#[derive(Debug, Clone)]
struct CachedAppEntry {
    entry: AppEntry,
    path: String,
    mtime: SystemTime,
}

/// 缓存内容(应用索引 + 根目录 mtime 快照 + 增量状态)。
struct CacheState {
    entries: Vec<AppEntry>,
    /// 带元数据的缓存条目（用于增量对比）。
    cached_entries: Vec<CachedAppEntry>,
    /// 上次扫描时记录的根目录 mtime(用户开始菜单 / 系统开始菜单)。
    root_mtimes: Vec<Option<SystemTime>>,
    /// 已连续增量次数。
    incremental_count: u32,
    /// 上次全量刷新时间。
    last_full_refresh: Option<Instant>,
}

pub struct StartMenuEngine {
    cache: Arc<RwLock<CacheState>>,
    /// 配置（运行时可更新）。
    config: Arc<RwLock<StartMenuConfig>>,
}

impl StartMenuEngine {
    pub fn with_config(config: StartMenuConfig) -> Self {
        StartMenuEngine {
            cache: Arc::new(RwLock::new(CacheState {
                entries: Vec::new(),
                cached_entries: Vec::new(),
                root_mtimes: Vec::new(),
                incremental_count: 0,
                last_full_refresh: None,
            })),
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// 更新配置（供 SearchService 调用），并立即触发重新扫描。
    pub fn update_config(&self, config: StartMenuConfig) {
        let mut cfg = self.config.write().unwrap();
        *cfg = config;
        drop(cfg); // 释放锁，避免死锁

        // 配置变更后立即触发全量扫描（后台异步，不阻塞）
        let cache = Arc::clone(&self.cache);
        let config = Arc::clone(&self.config);
        tauri::async_runtime::spawn(async move {
            let (depth, include_uwp) = {
                let cfg = config.read().unwrap();
                (cfg.scan_depth, cfg.include_uwp)
            };
            let _ = tokio::task::spawn_blocking(move || {
                full_scan_into_cache(&cache, depth, include_uwp)
            })
            .await;
        });
    }

    /// 启动后台:立即预扫一次 + 定时增量刷新。不阻塞调用方。
    fn start_background(&self) {
        let cache = Arc::clone(&self.cache);
        let config = Arc::clone(&self.config);
        tauri::async_runtime::spawn(async move {
            // 立即预扫(后台，全量)
            let (depth, include_uwp) = {
                let cfg = config.read().unwrap();
                (cfg.scan_depth, cfg.include_uwp)
            };
            let c = Arc::clone(&cache);
            let _ =
                tokio::task::spawn_blocking(move || full_scan_into_cache(&c, depth, include_uwp))
                    .await;

            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;

                // 检查配置是否启用
                {
                    let cfg = config.read().unwrap();
                    if !cfg.enabled {
                        tracing::trace!("StartMenuEngine: 已禁用，跳过定时刷新");
                        continue;
                    }
                }

                // 检查是否需要全量刷新
                let need_full = {
                    let guard = cache.read().unwrap();
                    // 连续增量次数达到阈值
                    let incremental_exceeded =
                        guard.incremental_count >= FORCE_FULL_AFTER_INCREMENTAL;
                    // 距离上次全量刷新超过 2 小时
                    let time_for_full = guard
                        .last_full_refresh
                        .map(|t| t.elapsed() >= FULL_REFRESH_INTERVAL)
                        .unwrap_or(true);
                    incremental_exceeded || time_for_full
                };

                let (depth, include_uwp) = {
                    let cfg = config.read().unwrap();
                    (cfg.scan_depth, cfg.include_uwp)
                };
                if need_full {
                    let c = Arc::clone(&cache);
                    let _ = tokio::task::spawn_blocking(move || {
                        full_scan_into_cache(&c, depth, include_uwp)
                    })
                    .await;
                } else if roots_changed_since_last(&cache) {
                    // mtime 变化 → 增量扫描（.lnk 部分增量，UWP 部分全量重建）
                    let c = Arc::clone(&cache);
                    let _ = tokio::task::spawn_blocking(move || {
                        incremental_scan(&c, depth, include_uwp)
                    })
                    .await;
                }
            }
        });
    }

    /// 获取缓存的 entries 快照。命中直接返回;缓存空(预扫未完成)则 spawn_blocking
    /// 扫一次后返回——保证首次搜索也有结果。
    async fn get_entries(&self) -> Vec<AppEntry> {
        {
            let guard = self.cache.read().unwrap();
            if !guard.entries.is_empty() {
                return guard.entries.clone();
            }
        }
        let (depth, include_uwp) = {
            let cfg = self.config.read().unwrap();
            (cfg.scan_depth, cfg.include_uwp)
        };
        let c = Arc::clone(&self.cache);
        let _ =
            tokio::task::spawn_blocking(move || full_scan_into_cache(&c, depth, include_uwp)).await;
        self.cache.read().unwrap().entries.clone()
    }
}

/// 全量扫描开始菜单并更新缓存。必须在 spawn_blocking 中调用。
fn full_scan_into_cache(cache: &RwLock<CacheState>, scan_depth: u32, include_uwp: bool) {
    let start = Instant::now();
    let mut entries = super::scan_start_menu(scan_depth);

    // 合并 UWP/MSIX 应用
    if include_uwp {
        let uwp_entries = super::scan_apps_folder();
        let existing_names: std::collections::HashSet<String> =
            entries.iter().map(|e| e.name.to_lowercase()).collect();
        for entry in uwp_entries {
            // 去重：同名应用保留 .lnk 版本（路径更具体，右键菜单功能更完整）
            if !existing_names.contains(&entry.name.to_lowercase()) {
                entries.push(entry);
            }
        }
    }

    let mtimes = super::roots_modified();
    let elapsed = start.elapsed();

    let mut guard = cache.write().unwrap();
    guard.entries = entries.clone();
    guard.root_mtimes = mtimes;
    guard.incremental_count = 0;
    guard.last_full_refresh = Some(Instant::now());

    tracing::debug!(
        count = entries.len(),
        elapsed_ms = elapsed.as_millis(),
        "开始菜单全量扫描完成"
    );
}

/// 增量扫描：对比缓存中的文件，仅增删变化项。必须在 spawn_blocking 中调用。
fn incremental_scan(cache: &RwLock<CacheState>, max_depth: u32, include_uwp: bool) {
    let start = Instant::now();

    // 读取当前缓存的文件路径集合
    let cached_paths: HashMap<String, SystemTime> = {
        let guard = cache.read().unwrap();
        guard
            .cached_entries
            .iter()
            .map(|e| (e.path.clone(), e.mtime))
            .collect()
    };

    // 扫描当前目录，收集所有 .lnk 文件的 (path, mtime)
    let current_files = scan_current_files(max_depth);

    // 找出新增和变化的文件
    let mut new_entries = Vec::new();
    for (path, mtime) in &current_files {
        if let Some(cached_mtime) = cached_paths.get(path) {
            // 文件存在且 mtime 未变化，保留
            if cached_mtime == mtime {
                continue;
            }
        }
        // 新增或变化的文件，需要解析
        if let Some(entry) = super::parse_lnk_entry(path) {
            new_entries.push(CachedAppEntry {
                entry,
                path: path.clone(),
                mtime: *mtime,
            });
        }
    }

    // 找出删除的文件
    let current_paths: std::collections::HashSet<String> = current_files.keys().cloned().collect();
    let removed_count = cached_paths
        .keys()
        .filter(|p| !current_paths.contains(*p))
        .count();

    // 更新缓存
    {
        let mut guard = cache.write().unwrap();

        // 保留未变化的 .lnk 条目
        let mut updated_cached: Vec<CachedAppEntry> = guard
            .cached_entries
            .iter()
            .filter(|e| {
                current_paths.contains(&e.path) && cached_paths.get(&e.path) == Some(&e.mtime)
            })
            .cloned()
            .collect();

        // 添加新条目
        updated_cached.extend(new_entries);

        // 从 .lnk 结果构建 entries 列表
        let mut entries: Vec<AppEntry> = updated_cached.iter().map(|e| e.entry.clone()).collect();

        // UWP 应用：每次增量时全量重建（无 mtime 可比对，数量少速度快）
        if include_uwp {
            let uwp_entries = super::scan_apps_folder();
            let existing_names: std::collections::HashSet<String> =
                entries.iter().map(|e| e.name.to_lowercase()).collect();
            for entry in uwp_entries {
                if !existing_names.contains(&entry.name.to_lowercase()) {
                    entries.push(entry);
                }
            }
        }

        guard.entries = entries;
        guard.cached_entries = updated_cached;
        guard.root_mtimes = super::roots_modified();
        guard.incremental_count += 1;
    }

    let elapsed = start.elapsed();
    tracing::debug!(
        added = current_files.len() - cached_paths.len() + removed_count,
        removed = removed_count,
        elapsed_ms = elapsed.as_millis(),
        "开始菜单增量扫描完成"
    );
}

/// 扫描当前开始菜单目录，返回所有 .lnk 文件的 (path, mtime)。递归扫描子目录。
fn scan_current_files(max_depth: u32) -> HashMap<String, SystemTime> {
    let mut files = HashMap::new();
    let roots = super::start_menu_roots();

    for root in roots {
        if !root.exists() {
            continue;
        }
        scan_dir_recursive(&root, &mut files, max_depth, 0);
    }

    files
}

/// 递归扫描目录，收集 .lnk 文件的 (path, mtime)。
fn scan_dir_recursive(
    dir: &std::path::Path,
    files: &mut HashMap<String, SystemTime>,
    max_depth: u32,
    current_depth: u32,
) {
    if current_depth >= max_depth {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        let path = entry.path();
        if path.is_dir() {
            scan_dir_recursive(&path, files, max_depth, current_depth + 1);
        } else if path.extension().map_or(false, |ext| ext == "lnk") {
            if let Ok(meta) = std::fs::metadata(&path) {
                if let Ok(mtime) = meta.modified() {
                    files.insert(path.to_string_lossy().to_string(), mtime);
                }
            }
        }
    }
}

/// 根目录 mtime 是否与上次扫描记录不同。
fn roots_changed_since_last(cache: &RwLock<CacheState>) -> bool {
    let current = super::roots_modified();
    let guard = cache.read().unwrap();
    if current.len() != guard.root_mtimes.len() {
        return true;
    }
    current
        .iter()
        .zip(guard.root_mtimes.iter())
        .any(|(a, b)| a != b)
}

#[async_trait::async_trait]
impl SearchEngine for StartMenuEngine {
    fn id(&self) -> &'static str {
        "start_menu"
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

    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        // 检查是否启用
        {
            let cfg = self.config.read().unwrap();
            if !cfg.enabled {
                tracing::trace!("StartMenuEngine: 已禁用，跳过");
                return Vec::new();
            }
        }

        if query.is_empty() {
            return Vec::new();
        }
        let entries = self.get_entries().await;
        let scored = super::fuzzy_score_entries(query, &entries, ENGINE_LIMIT);
        normalize_to_items(scored, ctx.history)
    }
}

/// 把 `(raw_score, AppEntry)` 列表 top-relative 归一化为 `SearchItem`。
///
/// 流程：raw 分 → top-relative 归一化到 [0,1] → 统一历史加权。
/// 历史加权使用 `scorer::history_boost`，与 Builtin/Calc/File/Plugin 共用同一公式。
fn normalize_to_items(
    scored: Vec<(u32, AppEntry)>,
    history: &HashMap<String, (i64, i64)>,
) -> Vec<SearchItem> {
    // 先转成 f32 元组，用统一归一化函数处理
    let mut normalized: Vec<(AppEntry, f32)> =
        scored.into_iter().map(|(raw, e)| (e, raw as f32)).collect();
    normalize_top_relative(&mut normalized);

    normalized
        .into_iter()
        .map(|(e, base_score)| {
            let (hit_count, last_used_at) = history.get(&e.lnk_path).copied().unwrap_or((0, 0));
            let hist_boost = super::scorer::history_boost(hit_count, last_used_at);
            let score = base_score + hist_boost;
            let detail = format!("fuzzy={:.2} hist=+{:.2}", base_score, hist_boost);
            SearchItem {
                id: e.lnk_path.clone(),
                title: e.name,
                subtitle: e.description,
                score,
                action: SearchAction::Open { path: e.lnk_path },
                source: "start_menu".into(),
                score_detail: Some(detail),
                context_aware: false,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::search::Action;

    fn entry(name: &str, lnk: &str) -> AppEntry {
        AppEntry {
            name: name.into(),
            pinyin_name: String::new(),
            pinyin_full: String::new(),
            lnk_path: lnk.into(),
            is_calc: false,
            score: 0.0,
            is_placeholder: false,
            is_error: false,
            source: String::new(),
            description: Some(lnk.into()),
            action: Action::default(),
            ..Default::default()
        }
    }

    #[test]
    fn top_relative_normalization() {
        let scored = vec![(200u32, entry("A", "a")), (100u32, entry("B", "b"))];
        let history = HashMap::new();
        let items = normalize_to_items(scored, &history);
        assert_eq!(items[0].score, 1.0); // 最高分归一为 1.0
        assert_eq!(items[1].score, 0.5);
        assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "a"));
    }

    #[test]
    fn zero_max_yields_zero_scores() {
        let scored = vec![(0u32, entry("A", "a"))];
        let history = HashMap::new();
        let items = normalize_to_items(scored, &history);
        assert_eq!(items[0].score, 0.0);
    }

    #[test]
    fn history_boost_applied_after_normalization() {
        let now = chrono::Utc::now().timestamp();
        let scored = vec![(200u32, entry("A", "a")), (100u32, entry("B", "b"))];
        let mut history = HashMap::new();
        history.insert("b".to_string(), (10, now)); // B 有 10 次历史，最近使用
        let items = normalize_to_items(scored, &history);
        // A: 1.0 + 0 (无历史) = 1.0
        assert!((items[0].score - 1.0).abs() < 1e-6);
        // B: 0.5 + ln(11)*0.3 ≈ 0.5 + 0.719 = 1.219
        assert!(
            items[1].score > items[0].score,
            "history should boost B above A"
        );
    }
}
