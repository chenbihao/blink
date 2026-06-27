//! StartMenuEngine:开始菜单应用搜索引擎(sync lane)。
//!
//! 持有应用索引缓存(从原 `cache.rs` 迁入为字段,见 0.2 设计 §2.4——数据结构不变,
//! 仅把所有者从模块全局换成引擎字段)。`start()` 启动后台预扫 + 定时增量刷新;
//! search 时对缓存做 fuzzy 打分 + top-relative 归一化(§2.3)。
//!
//! 所有文件 IO(scan_start_menu)都在 `spawn_blocking` 里跑,绝不阻塞 async runtime。
//! 失效:定时检查根目录 mtime,变化才全量重扫;每 N 次强制刷新兜底深层目录变化
//! (Windows 目录 mtime 只反映直接子项增删)。

use std::collections::HashMap;
use std::sync::{Arc, RwLock};
use std::time::{Duration, SystemTime};

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};
use super::scorer::normalize_top_relative;
use super::AppEntry;

/// 搜索结果上限(融合前每引擎各自截断)。
const ENGINE_LIMIT: usize = 50;
/// 定时检查间隔。
const CHECK_INTERVAL: Duration = Duration::from_secs(300); // 5 分钟
/// 每 N 次检查强制全量刷新一次(兜底深层目录变化)。
const FORCE_REFRESH_EVERY: u32 = 6; // ≈ 半小时

/// 缓存内容(应用索引 + 根目录 mtime 快照)。
struct CacheState {
    entries: Vec<AppEntry>,
    /// 上次扫描时记录的根目录 mtime(用户开始菜单 / 系统开始菜单)。
    root_mtimes: Vec<Option<SystemTime>>,
}

pub struct StartMenuEngine {
    cache: Arc<RwLock<CacheState>>,
}

impl StartMenuEngine {
    pub fn new() -> Self {
        StartMenuEngine {
            cache: Arc::new(RwLock::new(CacheState {
                entries: Vec::new(),
                root_mtimes: Vec::new(),
            })),
        }
    }

    /// 启动后台:立即预扫一次 + 定时增量刷新。不阻塞调用方。
    fn start_background(&self) {
        let cache = Arc::clone(&self.cache);
        tauri::async_runtime::spawn(async move {
            // 立即预扫(后台)
            let c = Arc::clone(&cache);
            let _ = tokio::task::spawn_blocking(move || scan_into_cache(&c)).await;

            let mut check_count: u32 = 0;
            loop {
                tokio::time::sleep(CHECK_INTERVAL).await;
                check_count = check_count.wrapping_add(1);
                // 兜底:每 FORCE_REFRESH_EVERY 次强制全量;否则仅 mtime 变化才扫描
                let force = check_count % FORCE_REFRESH_EVERY == 0;
                if force || roots_changed_since_last(&cache) {
                    let c = Arc::clone(&cache);
                    let _ = tokio::task::spawn_blocking(move || scan_into_cache(&c)).await;
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
        let c = Arc::clone(&self.cache);
        let _ = tokio::task::spawn_blocking(move || scan_into_cache(&c)).await;
        self.cache.read().unwrap().entries.clone()
    }
}

/// 阻塞扫描开始菜单并更新缓存。必须在 spawn_blocking 中调用。
fn scan_into_cache(cache: &RwLock<CacheState>) {
    let entries = super::scan_start_menu();
    let mtimes = super::roots_modified();
    let mut guard = cache.write().unwrap();
    guard.entries = entries;
    guard.root_mtimes = mtimes;
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

    fn start(&self) {
        self.start_background();
    }

    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
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
fn normalize_to_items(scored: Vec<(u32, AppEntry)>, history: &HashMap<String, i64>) -> Vec<SearchItem> {
    // 先转成 f32 元组，用统一归一化函数处理
    let mut normalized: Vec<(AppEntry, f32)> = scored
        .into_iter()
        .map(|(raw, e)| (e, raw as f32))
        .collect();
    normalize_top_relative(&mut normalized);

    normalized
        .into_iter()
        .map(|(e, base_score)| {
            let hit_count = history.get(&e.lnk_path).copied().unwrap_or(0);
            let hist_boost = super::scorer::history_boost(hit_count);
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
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::{Action, ActionKind};

    fn entry(name: &str, lnk: &str) -> AppEntry {
        AppEntry {
            name: name.into(),
            pinyin_name: String::new(),
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
        let scored = vec![(200u32, entry("A", "a")), (100u32, entry("B", "b"))];
        let mut history = HashMap::new();
        history.insert("b".to_string(), 10); // B 有 10 次历史
        let items = normalize_to_items(scored, &history);
        // A: 1.0 + 0 (无历史) = 1.0
        assert!((items[0].score - 1.0).abs() < 1e-6);
        // B: 0.5 + ln(11)*0.3 ≈ 0.5 + 0.719 = 1.219
        assert!(items[1].score > items[0].score, "history should boost B above A");
    }
}
