//! SearchService:多路搜索的路由 + 融合 + 渐进式调度(见 0.2 设计 §2.3 / §2.5)。
//!
//! - 持有引擎(按 lane 分两组),**不持有任何缓存**(缓存归引擎,§2.4)。
//! - `search(query, seq)`:sync lane 引擎顺序召回 → 融合排序 → 转 `AppEntry` 同步返回首批;
//!   同时 spawn async lane → 完成后 `emit("blink://results",{seq,items})` 增量推送(步骤7)。
//! - 取消:持 `latest_seq`,async emit 前校验,过期则丢弃(步骤7;真正 cancel 传播在 0.3)。
//!
//! 由 `commands::search_apps` 经 `app.state::<Arc<SearchService>>()` 调用。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use serde::Serialize;
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter};

use super::engine::{Lane, QueryContext, SearchEngine, SearchItem};
use super::AppEntry;

/// 融合后返回前端的结果上限。
const RESULT_LIMIT: usize = 50;

/// async lane 增量结果的事件 payload(emit "blink://results")。
#[derive(Serialize, Clone)]
struct ResultsPayload {
    seq: u64,
    items: Vec<AppEntry>,
}

pub struct SearchService {
    app: AppHandle,
    pool: SqlitePool,
    sync_engines: Vec<Arc<dyn SearchEngine>>,
    async_engines: Vec<Arc<dyn SearchEngine>>,
    /// 最近一次 query 的 seq,用于丢弃过期 async 增量(emit 前校验)。
    latest_seq: Arc<AtomicU64>,
}

impl SearchService {
    /// 用给定引擎构造。引擎按 `lane()` 分组。
    pub fn new(app: AppHandle, pool: SqlitePool, engines: Vec<Arc<dyn SearchEngine>>) -> Self {
        let mut sync_engines = Vec::new();
        let mut async_engines = Vec::new();
        for e in engines {
            match e.lane() {
                Lane::Sync => sync_engines.push(e),
                Lane::Async => async_engines.push(e),
            }
        }
        SearchService {
            app,
            pool,
            sync_engines,
            async_engines,
            latest_seq: Arc::new(AtomicU64::new(0)),
        }
    }

    /// 启动所有引擎的后台任务(如 StartMenuEngine 预扫)。
    pub fn start(&self) {
        for e in self.sync_engines.iter().chain(self.async_engines.iter()) {
            e.start();
        }
    }

    /// 搜索:返回 sync lane 融合后的首批结果(`AppEntry` 形状)同步返回;
    /// 同时 spawn async lane,完成后 emit("blink://results", {seq, items}) 增量推送。
    pub async fn search(&self, query: &str, seq: u64) -> Vec<AppEntry> {
        self.latest_seq.store(seq, Ordering::SeqCst);

        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }

        let history = crate::history::get_weights(&self.pool).await;
        let ctx = QueryContext { history: &history };

        let mut items = Vec::new();
        for engine in &self.sync_engines {
            items.extend(engine.search(q, &ctx).await);
        }

        // async lane:不阻塞首批,完成后 emit 增量(0.2.2 仅 mock 填充;真插件在 0.3)。
        self.spawn_async_lane(q.to_string(), seq);

        fuse_items(items, RESULT_LIMIT)
            .into_iter()
            .map(SearchItem::into_app_entry)
            .collect()
    }

    /// spawn async lane 任务:跑慢引擎 → 融合 → seq 校验 → emit 增量。
    /// seq 校验双保险:后端 `latest_seq`(query 被新 query 取代则丢弃)+ 前端 seq 比对。
    fn spawn_async_lane(&self, query: String, seq: u64) {
        if self.async_engines.is_empty() {
            return;
        }
        let engines = self.async_engines.clone();
        let app = self.app.clone();
        let pool = self.pool.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        tauri::async_runtime::spawn(async move {
            let history = crate::history::get_weights(&pool).await;
            let ctx = QueryContext { history: &history };
            let mut items = Vec::new();
            for engine in &engines {
                let found = engine.search(&query, &ctx).await;
                tracing::trace!(engine = engine.id(), count = found.len(), "async lane 引擎返回");
                items.extend(found);
            }
            // 过期丢弃:用户已发起更新的 query
            if seq != latest_seq.load(Ordering::SeqCst) {
                return;
            }
            if items.is_empty() {
                return;
            }
            let entries: Vec<AppEntry> = fuse_items(items, RESULT_LIMIT)
                .into_iter()
                .map(SearchItem::into_app_entry)
                .collect();
            if let Err(e) = app.emit("blink://results", ResultsPayload { seq, items: entries }) {
                tracing::debug!(error = %e, "emit blink://results failed");
            }
        });
    }
}

/// 融合多引擎结果:去重(按 id)+ 排序 + 截断。纯函数,便于单测(设计 §7 B6)。
///
/// 排序:score 降序 → source tie-break(calc > start_menu > 其他)。去重保留先出现的
/// (引擎按注册顺序召回,calc 在前)。
fn fuse_items(items: Vec<SearchItem>, limit: usize) -> Vec<SearchItem> {
    use std::collections::HashSet;
    let mut seen = HashSet::new();
    let mut deduped: Vec<SearchItem> = Vec::with_capacity(items.len());
    for item in items {
        if seen.insert(item.id.clone()) {
            deduped.push(item);
        }
    }
    deduped.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| source_rank(&a.source).cmp(&source_rank(&b.source)))
    });
    deduped.truncate(limit);
    deduped
}

/// source 优先级(小=靠前):calc 最高,start_menu 次之,其余(插件/mock)垫后。
fn source_rank(source: &str) -> u8 {
    match source {
        "calc" => 0,
        "start_menu" => 1,
        _ => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::engine::SearchAction;

    fn item(id: &str, score: f32, source: &str) -> SearchItem {
        SearchItem {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            score,
            action: SearchAction::Open { path: id.into() },
            source: source.into(),
        }
    }

    #[test]
    fn dedupe_keeps_first_by_id() {
        let items = vec![item("a", 0.9, "start_menu"), item("a", 0.5, "start_menu")];
        let r = fuse_items(items, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].score, 0.9); // 先出现的保留
    }

    #[test]
    fn sorts_by_score_desc() {
        let items = vec![item("a", 0.3, "start_menu"), item("b", 0.8, "start_menu")];
        let r = fuse_items(items, 10);
        assert_eq!(r[0].id, "b");
        assert_eq!(r[1].id, "a");
    }

    #[test]
    fn calc_wins_tie_break_on_equal_score() {
        // 同分时 calc 排在 start_menu 前
        let items = vec![item("app", 1.0, "start_menu"), item("calc:1+1", 1.0, "calc")];
        let r = fuse_items(items, 10);
        assert_eq!(r[0].source, "calc");
    }

    #[test]
    fn truncates_to_limit() {
        let items = (0..10).map(|i| item(&format!("e{i}"), 0.5, "start_menu")).collect();
        let r = fuse_items(items, 3);
        assert_eq!(r.len(), 3);
    }
}
