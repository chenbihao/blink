//! 搜索结果缓存:启动后台预扫开始菜单,内存命中,定时增量刷新。
//!
//! 设计(见 production/0.2-core-plugin-design.md §2.4):
//! - 缓存是「引擎内部数据」,阶段三迁入 StartMenuEngine 时数据结构不变、仅换所有者。
//! - 所有文件 IO(scan_start_menu)都在 spawn_blocking 里跑,绝不阻塞 async runtime。
//! - 失效:定时检查根目录 mtime,变化才全量重扫;定期强制刷新兜底深层变化
//!   (Windows 目录 mtime 只反映直接子项增删,深层变化不传播到根)。

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime};

use super::AppEntry;

/// 定时检查间隔
const CHECK_INTERVAL: Duration = Duration::from_secs(300); // 5 分钟
/// 每 N 次检查强制全量刷新一次(兜底深层目录变化)
const FORCE_REFRESH_EVERY: u32 = 6; // ≈ 半小时

struct CacheState {
    entries: Vec<AppEntry>,
    /// 上次扫描时记录的根目录 mtime(用户开始菜单 / 系统开始菜单)
    root_mtimes: Vec<Option<SystemTime>>,
}

static CACHE: OnceLock<RwLock<CacheState>> = OnceLock::new();
/// 检查计数,用于强制刷新兜底
static CHECK_COUNT: AtomicU32 = AtomicU32::new(0);

fn cache() -> &'static RwLock<CacheState> {
    CACHE.get_or_init(|| {
        RwLock::new(CacheState {
            entries: Vec::new(),
            root_mtimes: Vec::new(),
        })
    })
}

/// 阻塞扫描开始菜单并更新缓存。必须在 spawn_blocking 中调用。
fn scan_into_cache() {
    let entries = super::scan_start_menu();
    let mtimes = super::roots_modified();
    let mut guard = cache().write().unwrap();
    guard.entries = entries;
    guard.root_mtimes = mtimes;
}

/// 启动后台:立即预扫一次 + 定时增量刷新。不阻塞调用方(setup)。
pub fn init() {
    tauri::async_runtime::spawn(async move {
        // 立即预扫(后台)
        let _ = tokio::task::spawn_blocking(scan_into_cache).await;

        loop {
            tokio::time::sleep(CHECK_INTERVAL).await;
            let count = CHECK_COUNT.fetch_add(1, Ordering::SeqCst) + 1;
            // 兜底:每 FORCE_REFRESH_EVERY 次强制全量;否则仅 mtime 变化才扫描
            let force = count % FORCE_REFRESH_EVERY == 0;
            if force || roots_changed_since_last() {
                let _ = tokio::task::spawn_blocking(scan_into_cache).await;
            }
        }
    });
}

/// 根目录 mtime 是否与上次扫描记录不同。
fn roots_changed_since_last() -> bool {
    let current = super::roots_modified();
    let guard = cache().read().unwrap();
    if current.len() != guard.root_mtimes.len() {
        return true;
    }
    current
        .iter()
        .zip(guard.root_mtimes.iter())
        .any(|(a, b)| a != b)
}

/// 获取缓存的 entries 快照(commands 调用)。
///
/// 命中缓存直接返回;若缓存为空(预扫尚未完成),触发一次 spawn_blocking 扫描后返回
/// ——保证首次搜索也有结果。之后都命中内存。
pub async fn get_entries() -> Vec<AppEntry> {
    {
        let guard = cache().read().unwrap();
        if !guard.entries.is_empty() {
            return guard.entries.clone();
        }
    }
    // 首次:缓存空,spawn_blocking 扫描(文件 IO 移出 async)
    let _ = tokio::task::spawn_blocking(scan_into_cache).await;
    cache().read().unwrap().entries.clone()
}
