//! SearchService:多路搜索的路由 + 融合 + 渐进式调度(见 0.2 设计 §2.3 / §2.5)。
//!
//! 0.4 改造:search() 开头调用 `IntentRouter::route()` 决定呈现策略(Takeover/Mixed)。
//! - Takeover:跳过本地引擎,只查命中插件,独占返回区。
//! - Mixed:本地引擎(sync lane)照常召回;命中插件按 surface(Priority/Inline)参与排序。
//!
//! 由 `commands::search_apps` 经 `app.state::<Arc<SearchService>>()` 调用。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use serde::Serialize;
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter};

use crate::context::ContextSnapshot;
use crate::intent::{Candidate, IntentRouter, Route, Surface};
use crate::plugin::PluginEngine;

use super::engine::{Lane, QueryContext, SearchEngine, SearchItem};
use super::{Action, ActionKind, AppEntry};

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
    plugin_engine: Option<Arc<PluginEngine>>,
    router: Arc<dyn IntentRouter>,
    /// 最近一次 query 的 seq,用于丢弃过期 async 增量(emit 前校验)。
    latest_seq: Arc<AtomicU64>,
    /// 唤起时的上下文快照（前台应用、剪贴板等）。
    /// invoke 时更新，search 时读取。RwLock 读可并行，写极少（仅唤起时）。
    snapshot: Arc<RwLock<ContextSnapshot>>,
}

impl SearchService {
    pub fn new(
        app: AppHandle,
        pool: SqlitePool,
        engines: Vec<Arc<dyn SearchEngine>>,
        plugin_engine: Option<Arc<PluginEngine>>,
        router: Arc<dyn IntentRouter>,
    ) -> Self {
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
            plugin_engine,
            router,
            latest_seq: Arc::new(AtomicU64::new(0)),
            snapshot: Arc::new(RwLock::new(ContextSnapshot::default())),
        }
    }

    /// 更新上下文快照（window::invoke 时调用）。
    pub fn update_snapshot(&self, snapshot: ContextSnapshot) {
        let mut guard = self.snapshot.write().unwrap();
        *guard = snapshot;
    }

    /// 启动所有引擎的后台任务(如 StartMenuEngine 预扫)。
    pub fn start(&self) {
        for e in self.sync_engines.iter().chain(self.async_engines.iter()) {
            e.start();
        }
    }

    /// 搜索:先路由 → 按 Takeover/Mixed 分支执行 → 返回首批结果 + spawn 增量。
    pub async fn search(&self, query: &str, seq: u64) -> Vec<AppEntry> {
        self.latest_seq.store(seq, Ordering::SeqCst);

        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }

        let history = crate::history::get_weights(&self.pool).await;
        // 读取上下文快照（读锁，可并行）
        let snapshot = self.snapshot.read().unwrap().clone();
        let search_ctx = QueryContext { history: &history, snapshot: &snapshot };
        let intent_ctx = crate::intent::QueryContext { history: &history, snapshot: &snapshot };
        let route = self.router.route(q, &intent_ctx).await;
        // 过滤禁用插件的路由命中(0.5.1 补全):禁用插件不参与路由,避免 takeover 空白。
        let route = self.filter_disabled(route);

        // 获取插件显示名称的闭包
        let display_name = |id: &str| match &self.plugin_engine {
            Some(pe) => pe.get_display_name(id),
            None => id.strip_prefix("builtin.").unwrap_or(id).to_string(),
        };

        match route {
            Route::Takeover { plugin_id, arg, .. } => {
                // 独占:跳过本地引擎,只查该插件。
                // 先同步返回占位项(带明确的"正在查询"反馈),避免窗口空白,让用户知道命令已被识别。
                self.spawn_takeover(plugin_id.clone(), arg.clone(), seq);
                vec![placeholder_entry(&plugin_id, &display_name(&plugin_id))]
            }
            Route::Mixed { candidates } => {
                // sync lane 照常召回 → 首批。
                let mut items = Vec::new();
                for engine in &self.sync_engines {
                    items.extend(engine.search(q, &search_ctx).await);
                }

                // 分离 priority / inline 候选,准备 async lane。
                let (priority, inline): (Vec<Candidate>, Vec<Candidate>) = candidates
                    .into_iter()
                    .partition(|c| matches!(c.surface, Surface::Priority));

                let plugin_ids: Vec<(String, String)> = priority
                    .iter()
                    .chain(inline.iter())
                    .map(|c| (c.plugin_id.clone(), c.arg.clone()))
                    .collect();

                // 命中的插件：同步返回占位项（加载中反馈），避免窗口空白
                // Priority 插件占位给高 score 置顶，Inline 插件占位给低 score 放后面
                let priority_set: std::collections::HashSet<String> = priority
                    .iter()
                    .map(|c| c.plugin_id.clone())
                    .collect();
                let placeholders: Vec<AppEntry> = plugin_ids
                    .iter()
                    .map(|(id, _)| {
                        let mut entry = placeholder_entry(id, &display_name(id));
                        entry.score = if priority_set.contains(id) { 2.0 } else { -1.0 };
                        entry
                    })
                    .collect();

                if !plugin_ids.is_empty() || !self.async_engines.is_empty() {
                    self.spawn_mixed_lane(
                        q.to_string(),
                        plugin_ids,
                        priority,
                        seq,
                    );
                }

                // 占位项放最后,不抢占 sync lane 结果的首位
                let mut all_items: Vec<AppEntry> = fuse_items(items, RESULT_LIMIT)
                    .into_iter()
                    .map(SearchItem::into_app_entry)
                    .collect();
                all_items.extend(placeholders);
                all_items
            }
        }
    }

    /// 过滤禁用插件的路由命中(0.5.1):禁用插件不参与路由。
    /// - Takeover 命中禁用插件 → 降级空 Mixed(走 Generic 应用搜索),避免窗口空白。
    /// - Mixed 候选 → 剔除禁用插件。
    /// 比「RuleRouter 加 remove API」简洁:路由表保持静态,过滤在结果层(无需重新注入)。
    fn filter_disabled(&self, route: Route) -> Route {
        let Some(pe) = &self.plugin_engine else {
            return route;
        };
        match route {
            Route::Takeover { ref plugin_id, .. } => {
                if pe.is_enabled(plugin_id) {
                    route
                } else {
                    tracing::debug!(plugin = %plugin_id, "禁用插件命中 takeover,降级 Generic");
                    Route::Mixed { candidates: vec![] }
                }
            }
            Route::Mixed { candidates } => Route::Mixed {
                candidates: candidates
                    .into_iter()
                    .filter(|c| pe.is_enabled(&c.plugin_id))
                    .collect(),
            },
        }
    }

    /// Takeover 分支:查询单插件 → emit 增量。
    fn spawn_takeover(&self, plugin_id: String, arg: String, seq: u64) {
        let Some(plugin_engine) = self.plugin_engine.clone() else {
            return;
        };
        let app = self.app.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let snapshot = Arc::clone(&self.snapshot);
        tauri::async_runtime::spawn(async move {
            let snapshot = snapshot.read().unwrap().clone();
            let ctx = crate::plugin::PluginQueryContext::from_snapshot(&snapshot);
            let items = plugin_engine
                .query_subset(&[(plugin_id.clone(), arg)], &ctx)
                .await;
            if seq != latest_seq.load(Ordering::SeqCst) {
                return;
            }
            if items.is_empty() {
                return;
            }
            emit_results(&app, seq, items);
        });
    }

    /// Mixed 分支 async lane:每个引擎/插件单独 spawn,谁先回来就先 emit 增量。
    /// 关键修复:慢插件(如天气)不会阻塞快引擎(如 Everything)。
    fn spawn_mixed_lane(
        &self,
        query: String,
        plugin_ids: Vec<(String, String)>,
        priority_candidates: Vec<Candidate>,
        seq: u64,
    ) {
        let plugin_engine = self.plugin_engine.clone();
        let async_engines = self.async_engines.clone();
        let app = self.app.clone();
        let pool = self.pool.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let snapshot = Arc::clone(&self.snapshot);

        tauri::async_runtime::spawn(async move {
            let history = crate::history::get_weights(&pool).await;
            let snapshot = snapshot.read().unwrap().clone();

            // priority 插件的 id 集合(查询完成后 score 抬高)
            let priority_set: std::collections::HashSet<String> = priority_candidates
                .into_iter()
                .map(|c| c.plugin_id)
                .collect();

            // ── 1. 插件查询任务（独立 spawn）
            if let Some(ref pe) = plugin_engine {
                if !plugin_ids.is_empty() {
                    let plugin_ctx = crate::plugin::PluginQueryContext::from_snapshot(&snapshot);
                    let pe = pe.clone();
                    let plugin_ids = plugin_ids.clone();
                    let app = app.clone();
                    let latest_seq = latest_seq.clone();
                    tauri::async_runtime::spawn(async move {
                        let mut items = pe.query_subset(&plugin_ids, &plugin_ctx).await;
                        // priority 候选 score 抬高,确保置顶
                        for item in &mut items {
                            if priority_set.contains(&item.source) {
                                item.score = item.score.max(0.0) + 1.5;
                            }
                        }
                        if seq == latest_seq.load(Ordering::SeqCst) && !items.is_empty() {
                            tracing::trace!(count = items.len(), "插件查询返回");
                            emit_results(&app, seq, items);
                        }
                    });
                }
            }

            // ── 2. 每个 async 引擎独立 spawn(关键修复:不互相阻塞)
            for engine in async_engines {
                let q = query.clone();
                let app = app.clone();
                let latest_seq = latest_seq.clone();
                let history = history.clone();  // history 是 Arc<HashMap> 内部 move clone
                let snapshot = snapshot.clone();
                tauri::async_runtime::spawn(async move {
                    let ctx = QueryContext { history: &history, snapshot: &snapshot };
                    let items = engine.search(&q, &ctx).await;
                    if seq == latest_seq.load(Ordering::SeqCst) && !items.is_empty() {
                        tracing::trace!(engine = engine.id(), count = items.len(), "async lane 引擎返回");
                        emit_results(&app, seq, items);
                    }
                });
            }
        });
    }
}

/// 融合多引擎结果:去重(按 id)+ 排序 + 截断。纯函数,便于单测。
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
        "builtin" => 1,
        "start_menu" => 2,
        "file" => 3,
        _ => 4,
    }
}

/// emit 增量结果到前端。
fn emit_results(app: &AppHandle, seq: u64, items: Vec<SearchItem>) {
    let entries: Vec<AppEntry> = fuse_items(items, RESULT_LIMIT)
        .into_iter()
        .map(SearchItem::into_app_entry)
        .collect();
    if let Err(e) = app.emit("blink://results", ResultsPayload { seq, items: entries }) {
        tracing::debug!(error = %e, "emit blink://results failed");
    }
}

/// 插件占位项:同步返回,避免窗口空白等待 async 结果。
/// - plugin_id: 插件 id（如 "builtin.weather"），存 source 字段，与插件结果匹配实现自动替换
/// - display_name: 插件中文名称(manifest.name)
fn placeholder_entry(plugin_id: &str, display_name: &str) -> AppEntry {
    AppEntry {
        name: format!("{} 查询中…", display_name),
        pinyin_name: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.0,
        is_placeholder: true,
        source: plugin_id.to_string(),
        description: Some("请稍候".into()),
        action: Action {
            kind: ActionKind::Open,
            hint: None,
            payload: None,
        },
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
