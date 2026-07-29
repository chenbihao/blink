//! SearchService:多路搜索的路由 + 融合 + 渐进式调度(见 0.2 设计 §2.3 / §2.5)。
//!
//! 0.4 改造:search() 开头调用 `IntentRouter::route()` 决定呈现策略(Takeover/Mixed)。
//! - Takeover:跳过本地引擎,只查命中插件,独占返回区。
//! - Mixed:本地引擎(sync lane)照常召回;命中插件按 surface(Priority/Inline)参与排序。
//!
//! 由 `commands::search_apps` 经 `app.state::<Arc<SearchService>>()` 调用。

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use serde::Serialize;
use sqlx::SqlitePool;
use tauri::{AppHandle, Emitter, Manager};

use crate::app::ai_config::{Tier, ToolResultFeedback};
use crate::domain::ai::gating::{AiGate, GateOutcome, should_invoke_ai};
use crate::domain::ai::message::{ChatMessage, CompletionRequest};
use crate::domain::ai::provider::{AIError, AIProvider, StreamChunk};
use crate::domain::ai::registry::AIProviderRegistry;
use crate::domain::capability::{
    CapabilityError, CapabilityRegistry, CapabilityResult, InvokeContext,
};
use crate::domain::execution::{
    ActionContext, ActionOutcome, ActionRegistry, ActionSchema, DangerClass,
};
use crate::domain::intent::{Candidate, IntentRouter, RankingHint, Route, Suggestion, Surface};
use crate::domain::plugin::PluginEngine;
use crate::infra::platform::context::ContextSnapshot;
use crate::infra::utils::perf::ai_slo;

use super::engine::{Lane, QueryContext, SearchEngine, SearchItem};
use super::scorer::{boost_priority, placeholder_score, source_rank};
use super::{Action, ActionKind, AppEntry};

// ── AI 调用相关常量（0.11 review L5：把魔法数字抽常量与文档对齐）─────────────────

/// AI 总预算硬超时（毫秒）——`AIConfig::slo_hard_timeout_ms` 缺省时的兜底值。
/// 对齐文档 §3.3「单次路由调用硬超时」默认 20s。
const AI_DEFAULT_HARD_TIMEOUT_MS: u32 = 20_000;

/// Turn 2 回流独立超时下限（毫秒）——即使总预算已耗尽，也保证 Turn 2 至少有 5s
/// 完成总结/链式调用。文档 §2.2.5「Turn 2 独立超时：5-15s」。
const TURN2_TIMEOUT_MIN_MS: u32 = 5_000;

/// Turn 2 回流独立超时上限（毫秒）。文档 §2.2.5。
const TURN2_TIMEOUT_MAX_MS: u32 = 15_000;

/// Turn 2 超时降级后展示 Turn 1 结果前的短暂延迟（毫秒）——让"AI 回答较慢"占位
/// 文案有时间被用户看到，避免一闪而过。文档 §3.4。
const TURN2_FALLBACK_DELAY_MS: u64 = 300;

/// 同步搜索返回契约（0.8.3 §4.3）——`SearchService::search` / `search_apps` command 出口。
///
/// 在 `Vec<AppEntry>` 之外挂 `suggestion` 独立通道（0.8.3 起替代 0.8.1 的 `completion_hint`）：
/// - Keyword 类：首拼命中（`fy` → `fanyi`）或 fuzzy 部分拼音（`fan hello` → `fanyi hello`）
/// - Context 类：空 query + 选中英文 → 翻译 Ghost（Tab 采纳）
///
/// 用户按 Tab 后前端把输入替换为 `suggestion.replacement`，触发下一轮搜索。
/// `blink://results` 增量事件不带 suggestion（同步首次返回已给过）。
///
/// **契约说明**：这是内部 API（前后端锁版本，同版本编译）。`entries` 必填；rename 会导致
/// 前端 crash。序列化为 camelCase：`suggestion` 直接 `suggestion`，`SuggestionSource` 序列化为
/// camelCase 字符串（`keyword` / `context`）。
///
/// **兼容说明**：0.8.1 的 `completion_hint` 字段已删除；0.8.2 的 `fetchContextSuggestions` 前端通道
/// 也已废弃（见 §4.13 P0-1）——空 query 现在也走 search 接口拿 suggestion。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub entries: Vec<AppEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<Suggestion>,
}

/// async lane 增量结果的事件 payload(emit "blink://results")。
#[derive(Serialize, Clone)]
struct ResultsPayload {
    seq: u64,
    items: Vec<AppEntry>,
}

/// 引擎配置更新枚举（用于运行时热更新）。
pub enum EngineConfigUpdate {
    StartMenu(crate::app::config::StartMenuConfig),
    Calc(crate::app::config::CalcConfig),
    File(crate::app::config::FileSearchConfig),
}

pub struct SearchService {
    app: AppHandle,
    pool: SqlitePool,
    sync_engines: Vec<Arc<dyn SearchEngine>>,
    async_engines: Vec<Arc<dyn SearchEngine>>,
    plugin_engine: Arc<PluginEngine>,
    router: Arc<dyn IntentRouter>,
    /// 最近一次 query 的 seq,用于丢弃过期 async 增量(emit 前校验)。
    latest_seq: Arc<AtomicU64>,
    /// 唤起时的上下文快照（前台应用、剪贴板等）。
    /// invoke 时更新，search 时读取。RwLock 读可并行，写极少（仅唤起时）。
    snapshot: Arc<RwLock<ContextSnapshot>>,
    /// 融合后返回前端的最大结果数（AppConfig.max_results 热更新，搜索热路径零 IO）。
    max_results: Arc<AtomicUsize>,
    /// 用户禁用的内置动作 id 列表（0.8.0 §1.3）。
    /// BuiltinEngine 通过 QueryContext 只读，设置页保存时经 `update_disabled_builtin_actions`
    /// 热更新。读多写少，用 RwLock；每次 search 短时 read 不阻塞。
    disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
    /// 用户禁用的 context binding key 列表（`{target_id}::{trigger_key}` 格式，0.8.3 §4.6）。
    /// 0.11.8 起同时被 `RuleRouter`（manifest context）和 `BuiltinEngine`（内置动作 context）
    /// 消费——前者在 `apply_context_disable_list` 里独立持有副本，后者经 QueryContext 读。
    /// 读多写少，用 RwLock；每次 search 短时 read 不阻塞。
    disabled_context_bindings: Arc<RwLock<Vec<String>>>,
    /// Autosuggestion 配置快照（0.8.1 §2.5）。热更新走 `update_autosuggest_config`。
    /// - `enabled`: 关闭时 `search()` 恒返回 `completion_hint: None`（快速短路，不算 fuzzy）。
    /// - `min_score`: `RuleRouter::suggest_completion` 阈值（默认 0.7）。
    autosuggest: Arc<RwLock<AutosuggestState>>,
    /// 界面语言快照（0.8.1）。用于把 `empty_arg_hint` / 未来其他 `LocalizableText`
    /// 解析成当前语言字符串。热更新走 `update_language`，与 AppConfig.language 同步。
    language: Arc<RwLock<String>>,
    /// 上一轮 best_suggestion 产出的 RankingHint 快照（0.8.4 §5.3.1 Surface Booster）。
    /// route() 下一轮读此值做 surface boost——跨轮反馈滞后一轮,0.8.4 同步阶段可接受
    /// （0.9 AI 异步化后失效,见 0.8 文档 §5.6）。
    last_ranking_hint: Arc<Mutex<Option<RankingHint>>>,
    /// 共享的 min_score 阈值（0.8.6 §8.1.2）。
    /// 与 `KeywordProducer` 共享同一份 `Arc<RwLock<f64>>`——
    /// `update_autosuggest_config` 热更新时写入此引用，producer 侧同步生效。
    min_score_shared: Arc<RwLock<f64>>,
    /// AI Provider registry（0.9.2 Phase 5b setter 注入）。
    ///
    /// **为什么用 setter 注入而不是 `new` 参数**:`main.rs:256` 先建 search_service、
    /// `:308` 后建 ai_registry —— 构造顺序倒挂,setter 规避不动 wiring 顺序。
    /// setup 早期(setter 未调)读到 None → 跳过 AI lane → fallback fuzzy,无害。
    ai_registry: Arc<RwLock<Option<Arc<AIProviderRegistry>>>>,
}

#[derive(Clone, Copy)]
struct AutosuggestState {
    enabled: bool,
    min_score: f64,
}

impl Default for AutosuggestState {
    fn default() -> Self {
        Self {
            enabled: true,
            min_score: 0.7,
        }
    }
}

impl SearchService {
    pub fn new(
        app: AppHandle,
        pool: SqlitePool,
        engines: Vec<Arc<dyn SearchEngine>>,
        plugin_engine: Arc<PluginEngine>,
        router: Arc<dyn IntentRouter>,
        min_score_shared: Arc<RwLock<f64>>,
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
            max_results: Arc::new(AtomicUsize::new(50)),
            disabled_builtin_actions: Arc::new(RwLock::new(Vec::new())),
            disabled_context_bindings: Arc::new(RwLock::new(Vec::new())),
            autosuggest: Arc::new(RwLock::new(AutosuggestState::default())),
            language: Arc::new(RwLock::new("zh".to_string())),
            last_ranking_hint: Arc::new(Mutex::new(None)),
            min_score_shared,
            ai_registry: Arc::new(RwLock::new(None)),
        }
    }

    /// 注入 AI Provider registry(0.9.2 Phase 5b)。
    ///
    /// **调用位置**:`main.rs::setup` 在构造完 ai_registry 后调用一次。
    /// 未调用时 `ai_registry` 为 None → search() 走 AI lane 时安静跳过。
    ///
    /// **热更新**:registry.reload 由 `set_config('ai_config')` 触发,持有的
    /// `Arc<AIProviderRegistry>` 引用不变,`reload` 内部改池,SearchService 无感。
    pub fn set_ai_registry(&self, registry: Arc<AIProviderRegistry>) {
        *self.ai_registry.write().expect("ai_registry lock poisoned") = Some(registry);
        tracing::info!(target: ai_slo::TARGET, "AI registry 已注入 SearchService");
    }

    /// 更新上下文快照（window::invoke 时调用）。
    pub fn update_snapshot(&self, snapshot: ContextSnapshot) {
        let mut guard = self.snapshot.write().unwrap();
        *guard = snapshot;
    }

    /// 写回选中文本（后台 UIA 抓取完成后调用，0.8.0 §1.1）。
    ///
    /// 与 update_snapshot 分离：选区抓取是异步的（spawn_blocking），晚于快照写入，
    /// 抓到后单独回填选区文本，避免覆盖整份快照丢掉同时刻采的剪贴板/前台信息。
    ///
    /// 0.8.3 收尾：走 `AwarenessSnapshot::upsert_text` —— 找到 Selection 项就替换,
    /// 否则 append；None 时删除同 source 项。
    pub fn update_selected_text(&self, text: Option<String>, captured_at: Option<Instant>) {
        use crate::infra::platform::context::AwarenessSource;
        let mut guard = self.snapshot.write().unwrap();
        guard.upsert_text_with_time(AwarenessSource::Selection, text, captured_at);
    }

    /// 写回剪贴板文本（clipboard listener 检测到剪贴板变化时调用）。
    ///
    /// **动机**：`update_snapshot` 只在热键 invoke 时执行一次；主窗口保持打开时
    /// 用户复制/剪切新内容,snapshot 里的 Clipboard 项会陈旧,导致 Context ghost /
    /// AI 四筛子读到旧值。与 `update_selected_text` 对称补上局部刷新入口。
    ///
    /// **调用侧门控**：调用方（`main.rs` 注册的 clipboard hook）负责三重门控——
    /// `ContextConfig.enabled` / `clipboard_enabled` / 前台敏感应用检查,与
    /// `context::collect()` 逻辑对齐,避免密码管理器 Ctrl+C 悄悄进 snapshot。
    /// 本方法不做门控,只负责 upsert;`None` 时清空同 source 项。
    pub fn update_clipboard_text(&self, text: Option<String>) {
        use crate::infra::platform::context::AwarenessSource;
        let mut guard = self.snapshot.write().unwrap();
        guard.upsert_text(AwarenessSource::Clipboard, text);
    }

    /// 更新最大结果数（update_general_config 时调用）。热更新，搜索热路径零 IO。
    pub fn update_max_results(&self, n: usize) {
        // 下限保护：0 视为默认 20，避免结果全空
        let clamped = if n == 0 { 50 } else { n };
        self.max_results.store(clamped, Ordering::SeqCst);
        tracing::debug!(max_results = clamped, "SearchService max_results 已热更新");
    }

    /// 更新禁用的内置动作列表（0.8.0 §1.3）。
    ///
    /// 启动时从 `AppConfig.disabled_builtin_actions` 初始化一次；设置页勾选/取消 disable
    /// 后调用触发 SearchService 热更新——下一次 search 立即生效，无需重启。
    pub fn update_disabled_builtin_actions(&self, disabled: Vec<String>) {
        let mut guard = self.disabled_builtin_actions.write().unwrap();
        *guard = disabled;
        tracing::debug!(count = guard.len(), "内置动作 disable 列表已热更新");
    }

    /// 更新 Autosuggestion 配置（0.8.1 §2.5）。
    /// 启动时读一次 AppConfig 注入；设置页开关/滑块调整时命令层调此方法。
    pub fn update_autosuggest_config(&self, enabled: bool, min_score: f64) {
        let mut guard = self.autosuggest.write().unwrap();
        *guard = AutosuggestState { enabled, min_score };
        // 同步到共享引用——KeywordProducer 侧同步生效
        *self.min_score_shared.write().unwrap() = min_score;
        tracing::debug!(enabled, min_score, "Autosuggest 配置已热更新");
    }

    /// 更新 context binding 禁用列表（0.8.3 §4.6）。
    ///
    /// 启动时读一次 `AppConfig.disabled_context_bindings` 注入；设置页勾选/取消后经
    /// 命令层调此方法。转发至 `RuleRouter::apply_context_disable_list`——`RuleRouter` 内部
    /// 用 `HashSet` 存 key，`match_context_hits` 命中即跳过。
    pub fn update_disabled_context_bindings(&self, keys: Vec<String>) {
        // 0.11.8：RuleRouter 消费 manifest context binding；SearchService 本字段
        // 喂给 QueryContext 让 BuiltinEngine 读（内置动作 context binding 黑名单）。
        *self.disabled_context_bindings.write().unwrap() = keys.clone();
        self.router.apply_context_disable_list(keys);
        tracing::debug!("SearchService context binding 禁用列表已转发至 router + 本地缓存");
    }

    /// 更新界面语言快照（0.8.1）。启动时读一次 AppConfig.language 注入；
    /// 设置页切换语言时命令层调此方法。用于把插件 manifest 里的 `empty_arg_hint`
    /// 等 `LocalizableText` 解析成当前语言。
    ///
    /// 0.8.2 §3.4：同时转发到 `IntentRouter::set_app_language`——`RuleRouter` 用来
    /// 支持 `TextIsNonTargetLang` 中 `target_lang="auto"` 的回退。
    ///
    /// 0.8.5.1 §6.6：同时转发到 ClipboardEngine——subtitle 时间描述 zh/en 切换。
    pub fn update_language(&self, language: String) {
        {
            let mut guard = self.language.write().unwrap();
            *guard = language.clone();
        }
        self.router.set_app_language(language.clone());
        // 转发到 ClipboardEngine（sync lane 里唯一持 language 的 engine）
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_language(language.clone());
            }
        }
        tracing::debug!("SearchService 界面语言已热更新");
    }

    /// 更新 ClipboardEngine 的单次展示条数（设置页 `clipboard_config` 保存时转发）。
    /// downcast 模式同 `update_language`。
    pub fn update_clipboard_display_count(&self, count: u32) {
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_display_count(count);
            }
        }
        tracing::debug!(count, "ClipboardEngine 展示条数已热更新");
    }

    /// 启动所有引擎的后台任务(如 StartMenuEngine 预扫)。
    pub fn start(&self) {
        for e in self.sync_engines.iter().chain(self.async_engines.iter()) {
            e.start();
        }
    }

    /// 更新指定引擎的配置（运行时热更新）。
    /// 支持的 engine_id: "start_menu", "calc", "file"
    pub async fn update_engine_config(&self, engine_id: &str, config: EngineConfigUpdate) {
        let engines = self.sync_engines.iter().chain(self.async_engines.iter());
        for engine in engines {
            if engine.id() == engine_id {
                match config {
                    EngineConfigUpdate::StartMenu(cfg) => {
                        if let Some(sm) = engine
                            .as_any()
                            .downcast_ref::<super::start_menu_engine::StartMenuEngine>()
                        {
                            sm.update_config(cfg);
                            tracing::debug!(engine = engine_id, "StartMenuEngine 配置已热更新");
                        }
                    }
                    EngineConfigUpdate::Calc(cfg) => {
                        if let Some(calc) = engine
                            .as_any()
                            .downcast_ref::<super::calc_engine::CalcEngine>()
                        {
                            calc.update_config(cfg);
                            tracing::debug!(engine = engine_id, "CalcEngine 配置已热更新");
                        }
                    }
                    EngineConfigUpdate::File(cfg) => {
                        if let Some(file) = engine
                            .as_any()
                            .downcast_ref::<super::file_engine::FileEngine>()
                        {
                            file.update_config(cfg).await;
                            tracing::debug!(engine = engine_id, "FileEngine 配置已热更新");
                        }
                    }
                }
                return;
            }
        }
        tracing::warn!(engine = engine_id, "未找到引擎，无法更新配置");
    }

    /// 供 search_apps Capability 调用——共享 StartMenuEngine 实例,不重复扫描（0.11.2 改进 5）。
    ///
    /// **设计动机**：`search_files` Capability 自持 `FileEngine` 实例；
    /// `search_apps` 若也自持 `StartMenuEngine` 会重复扫描（后台预扫 + 定时刷新），
    /// 浪费资源 + 缓存不一致。故通过此方法共享 SearchService 已实例化的引擎。
    ///
    /// 返回原始 `SearchItem` 列表，Capability 自己投影成 `ItemResult`
    /// （与 `search_files` 同模式，payload 放结构化数据）。
    pub async fn search_apps_for_capability(
        &self,
        query: &str,
        max_results: usize,
    ) -> Vec<SearchItem> {
        if query.trim().is_empty() {
            return Vec::new();
        }

        for engine in &self.sync_engines {
            if engine.id() == "start_menu" {
                let history = std::collections::HashMap::new();
                let snapshot = ContextSnapshot::default();
                let disabled: Vec<String> = Vec::new();
                let disabled_ctx: Vec<String> = Vec::new();
                let search_ctx = QueryContext {
                    history: &history,
                    snapshot: &snapshot,
                    disabled_builtin_actions: &disabled,
                    disabled_context_bindings: &disabled_ctx,
                };
                let items = engine.search(query, &search_ctx).await;
                return items.into_iter().take(max_results).collect();
            }
        }
        Vec::new()
    }

    /// 搜索:先路由 → 按 Takeover/Mixed 分支执行 → 返回首批结果 + spawn 增量。
    ///
    /// 空 query 场景（0.8.0 §1.3）：跳过 intent 路由 + 插件；仅让 sync lane 内置引擎
    /// 走 Context-only 分支（例如"打开链接"依剪贴板 URL 出现）。其他引擎不参与。
    ///
    /// 0.8.1 §2.5：返回类型改为 `SearchResponse { entries, completion_hint }`——
    /// 非空 query 时同步算 ghost text（`RuleRouter::suggest_completion`），首次返回带一次；
    /// 增量 emit 事件不带 hint（前端已渲染）。
    ///
    /// **`query` 与 `q` 的语义分工**（本函数内多分支使用，务必区分）：
    /// - `q = query.trim()`：给 route / 引擎 / 空 query 判定 / 早退分支用——搜索匹配语义上
    ///   `"foo "` 与 `"foo"` 等价。
    /// - `query`（未 trim 的原文）：**只**传给 `suggest_completion`——尾空格是"参数等待中"
    ///   的语义信号（`fanyi ` 已进 Takeover 参数模式，不再需要 ghost；`fanyi` 未按空格，
    ///   给一个空 display 的 hint 让前端渲染 `<kbd>Tab</kbd>` 按钮）。
    pub async fn search(&self, query: &str, seq: u64) -> SearchResponse {
        let search_start = std::time::Instant::now();
        self.latest_seq.store(seq, Ordering::SeqCst);

        let q = query.trim();
        let history = crate::infra::data::history::get_weights(&self.pool).await;
        let snapshot = self.snapshot.read().unwrap().clone();
        let disabled = self.disabled_builtin_actions.read().unwrap().clone();
        let disabled_ctx = self.disabled_context_bindings.read().unwrap().clone();
        let search_ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
            disabled_context_bindings: &disabled_ctx,
        };

        // 路由决策（0.8.4：route 断 Awareness 依赖）
        let ranking_hint = self.last_ranking_hint.lock().unwrap().clone();
        let route = self.router.route(q, &history, ranking_hint.as_ref()).await;
        let route = self.filter_route(route);

        // Suggestion（0.8.3 / 0.8.6 arbiter）
        let mut suggestion = self.compute_suggestion(query, &snapshot);

        // ── 0.9.2 Phase 5b:AI Ghost Suggestion 覆盖 ─────────────────────
        // Tab 显式触发的核心机制:在**Keyword/Context 都没命中**且**过筛子**时,
        // 产 `SuggestionSource::Ai` Ghost,让用户按 Tab 显式触发 AI。
        // 避免:边打字边 spawn AI 造成的连续调用浪费。
        //
        // **触发条件**(与 §3.6 未命中过滤铁则一致):
        // 1. Keyword/Context Suggestion 未命中(suggestion.is_none())
        // 2. Route = Mixed{candidates: []}(无 plugin/engine 规则命中,filter 后)
        //    Takeover/EngineTakeover 天然命中规则,不覆盖
        // 3. gating 四筛子过
        //
        // Takeover 被 filter 降级成 Mixed{[]} 时也允许走 AI——用户被拦了没得选,
        // 正是需要 AI 帮忙的场景。
        if suggestion.is_none()
            && matches!(&route, Route::Mixed { candidates } if candidates.is_empty())
        {
            suggestion = self.maybe_ai_suggestion(q);
        }

        // 按 Route 分派到四个 executor（0.8.6 §8.2.1 拆 God Method）
        let entries = match route {
            Route::Takeover { plugin_id, arg, .. } => self.exec_takeover(plugin_id, arg, seq),
            Route::EngineTakeover { engine_id, arg } => {
                self.exec_engine_takeover(engine_id, arg, &search_ctx).await
            }
            // AI 前缀触发：产 AI Suggestion，让前端 Ghost + Tab 触发。
            // 不直接调用 trigger_ai——用户输入过程中不应立即消耗 token，
            // 需要用户按 Tab 显式确认后才真正调用 AI。
            Route::AiTrigger { arg } => {
                suggestion = self.make_ai_suggestion(&arg);
                vec![]
            }
            Route::Mixed { candidates } => {
                self.exec_mixed(q, candidates, seq, &search_ctx, search_start)
                    .await
            }
        };

        SearchResponse {
            entries,
            suggestion,
        }
    }

    /// 若过 gating 且 AI registry 就绪,产 AI Ghost Suggestion(0.9.2 Phase 5b)。
    ///
    /// display="按 Tab 问 AI",replacement=原 query(前端见 `source==="ai"` 走独立
    /// invoke `trigger_ai` 路径,**不**触发新一轮 search,避免"采纳后又搜索"的循环)。
    fn maybe_ai_suggestion(&self, q: &str) -> Option<Suggestion> {
        use crate::domain::intent::SuggestionSource;

        // 空 query 直接排除——AI Ghost 强绑非空 query
        if q.is_empty() {
            return None;
        }

        let reg = self
            .ai_registry
            .read()
            .expect("ai_registry lock poisoned")
            .clone()?;
        let cfg = reg.config_snapshot();
        let gate = AiGate::from(&cfg);
        match should_invoke_ai(q, &gate) {
            GateOutcome::Invoke => {
                // display 提示文案由前端 i18n 决定,后端只填英文占位;
                // 但当前前端 ghost.js 直接读 display——先填中文,待 0.9.2 第二步统一 i18n
                #[allow(deprecated)]
                Some(Suggestion {
                    display: "按 Tab 问 AI".to_string(),
                    replacement: q.to_string(),
                    source: SuggestionSource::Ai,
                    confidence: 0.5,
                    prefix_len: 0,
                    origin: None,
                    ranking_hint: None,
                })
            }
            GateOutcome::Fallback(reason) => {
                if matches!(reason, crate::domain::ai::gating::FallbackReason::Disabled) {
                    tracing::trace!(target: ai_slo::TARGET, ?reason, "AI Ghost 未触发");
                } else {
                    tracing::debug!(target: ai_slo::TARGET, ?reason, query = %q, "AI Ghost 未触发");
                }
                None
            }
        }
    }

    /// AI 前缀触发专用 Suggestion 构造（0.9.x）。
    ///
    /// 用户输入 "ai xxx" 显式触发，跳过 gating 四筛子——"ai" 前缀本身就是强信号。
    /// 返回的 Suggestion 让前端 Ghost 渲染 "按 Tab 问 AI"，用户按 Tab 后走
    /// `acceptCurrent() → invoke("trigger_ai")` 路径。
    ///
    /// **为什么不直接调 trigger_ai**：输入过程中不应立即消耗 token，
    /// 需要用户按 Tab 显式确认后才真正调用 AI（与 Ghost Tab 触发机制统一）。
    fn make_ai_suggestion(&self, arg: &str) -> Option<Suggestion> {
        use crate::domain::intent::SuggestionSource;

        let reg = self
            .ai_registry
            .read()
            .expect("ai_registry lock poisoned")
            .clone()?;
        // 检查 AI 是否启用——用户显式触发也要尊重总开关
        let cfg = reg.config_snapshot();
        if !cfg.enabled {
            tracing::trace!(target: ai_slo::TARGET, "AiTrigger: AI 未启用，跳过");
            return None;
        }

        // display 提示文案由前端 i18n 决定,后端只填英文占位;
        // 但当前前端 ghost.js 直接读 display——先填中文,待 0.9.2 第二步统一 i18n
        #[allow(deprecated)]
        Some(Suggestion {
            display: "按 Tab 问 AI".to_string(),
            replacement: arg.to_string(),
            source: SuggestionSource::Ai,
            confidence: 1.0, // AI 前缀触发是强信号，confidence 高于兜底的 0.5
            prefix_len: 0,
            origin: None,
            ranking_hint: None,
        })
    }

    /// Suggestion 计算（从 search() 提取，0.8.6 §8.2.1）。
    fn compute_suggestion(
        &self,
        query: &str,
        snapshot: &crate::infra::platform::context::ContextSnapshot,
    ) -> Option<Suggestion> {
        let cfg = *self.autosuggest.read().unwrap();
        if !cfg.enabled {
            return None;
        }
        let sug = self.router.best_suggestion(query, snapshot, cfg.min_score);
        *self.last_ranking_hint.lock().unwrap() = self.router.take_last_ranking_hint();
        sug
    }

    /// 插件显示名称查找。空插件场景（无 manifest 命中）自动回退到剥离 `builtin.` 前缀。
    fn display_name(&self, id: &str) -> String {
        // PluginEngine::get_display_name 内部即：find_plugin.map(name).unwrap_or(id.to_string())
        // 缺 manifest 时会返 `id` 原样（含 `builtin.` 前缀）—— 保留旧行为专门剥掉前缀
        // （旧 None 分支的语义）。
        let name = self.plugin_engine.get_display_name(id);
        if name == id {
            id.strip_prefix("builtin.").unwrap_or(id).to_string()
        } else {
            name
        }
    }

    /// 空参数引导文案（0.8.1）：manifest 配置了 `empty_arg_hint` 且 arg 空时返回引导文本。
    fn empty_arg_hint(&self, id: &str, arg: &crate::domain::intent::ExecArg) -> Option<String> {
        if !arg.is_none() {
            return None;
        }
        let lang = self.language.read().unwrap().clone();
        self.plugin_engine.get_empty_arg_hint(id, &lang)
    }

    /// Takeover executor（0.8.6 §8.2.1）：插件独占返回区。
    fn exec_takeover(
        &self,
        plugin_id: String,
        arg: crate::domain::intent::ExecArg,
        seq: u64,
    ) -> Vec<AppEntry> {
        if let Some(hint_text) = self.empty_arg_hint(&plugin_id, &arg) {
            tracing::debug!(plugin = %plugin_id, "empty_arg_hint 命中 Takeover，跳过插件查询");
            return vec![empty_arg_hint_entry(
                &plugin_id,
                &self.display_name(&plugin_id),
                hint_text,
            )];
        }
        self.spawn_takeover(plugin_id.clone(), arg.to_plugin_string(), seq);
        vec![placeholder_entry(
            &plugin_id,
            &self.display_name(&plugin_id),
        )]
    }

    /// EngineTakeover executor（0.8.6 §8.2.1）：本体 engine 独占。
    async fn exec_engine_takeover(
        &self,
        engine_id: String,
        arg: crate::domain::intent::ExecArg,
        search_ctx: &QueryContext<'_>,
    ) -> Vec<AppEntry> {
        let arg_str = arg.to_plugin_string();
        let mut items = Vec::new();
        for engine in &self.sync_engines {
            if engine.id() == engine_id {
                items.extend(engine.search(&arg_str, search_ctx).await);
                break;
            }
        }
        if items.is_empty() {
            tracing::debug!(engine = %engine_id, "engine takeover 未产出结果，可能是空历史/无匹配");
        }
        let limit = self.max_results.load(Ordering::SeqCst);
        fuse_items(items, limit)
            .into_iter()
            .map(SearchItem::into_app_entry)
            .collect()
    }

    /// Mixed executor（0.8.6 §8.2.1）：sync 引擎 + async 插件混排。
    async fn exec_mixed(
        &self,
        q: &str,
        candidates: Vec<Candidate>,
        seq: u64,
        search_ctx: &QueryContext<'_>,
        search_start: std::time::Instant,
    ) -> Vec<AppEntry> {
        // sync lane 召回（跳过 takeover_only engine）
        let mut items = Vec::new();
        for engine in &self.sync_engines {
            if engine.takeover_only() {
                continue;
            }
            items.extend(engine.search(q, search_ctx).await);
        }

        // 拆分：empty_arg_hint 命中的候选 vs 需要 async 查询的候选
        let mut hint_entries: Vec<AppEntry> = Vec::new();
        let candidates: Vec<Candidate> = candidates
            .into_iter()
            .filter_map(|c| match self.empty_arg_hint(&c.plugin_id, &c.arg) {
                Some(hint_text) => {
                    tracing::debug!(plugin = %c.plugin_id, surface = ?c.surface, "empty_arg_hint 命中 Mixed 候选，跳过插件查询");
                    let mut entry = empty_arg_hint_entry(&c.plugin_id, &self.display_name(&c.plugin_id), hint_text);
                    entry.score = placeholder_score(matches!(c.surface, Surface::Priority));
                    hint_entries.push(entry);
                    None
                }
                None => Some(c),
            })
            .collect();

        // 分离 priority / inline，准备 async lane
        let (priority, inline): (Vec<Candidate>, Vec<Candidate>) = candidates
            .into_iter()
            .partition(|c| matches!(c.surface, Surface::Priority));

        let plugin_ids: Vec<(String, String)> = priority
            .iter()
            .chain(inline.iter())
            .map(|c| (c.plugin_id.clone(), c.arg.to_plugin_string()))
            .collect();

        let priority_set: std::collections::HashSet<String> =
            priority.iter().map(|c| c.plugin_id.clone()).collect();
        let placeholders: Vec<AppEntry> = plugin_ids
            .iter()
            .map(|(id, _)| {
                let mut entry = placeholder_entry(id, &self.display_name(id));
                entry.score = placeholder_score(priority_set.contains(id));
                entry
            })
            .collect();

        // AI lane 触发不在这里判——0.9.2 起 AI 走 Tab 显式触发,在 search() 主入口
        // 通过 maybe_ai_suggestion 覆盖 SearchResponse.suggestion 出 Ghost。

        if !plugin_ids.is_empty() || !self.async_engines.is_empty() {
            self.spawn_mixed_lane(q.to_string(), plugin_ids, priority, seq);
        }

        let limit = self.max_results.load(Ordering::SeqCst);
        let mut all_items: Vec<AppEntry> = fuse_items(items, limit)
            .into_iter()
            .map(SearchItem::into_app_entry)
            .collect();

        all_items.extend(hint_entries);
        all_items.extend(placeholders);

        // ── 0.9.2 AI Ghost 已在 `search()` 主入口通过 `maybe_ai_suggestion` 覆盖 ──
        // 不在 exec_mixed 自动 spawn AI:边打字连续 spawn 会浪费 token + h2 stream 堆积。
        // 用户看到 Ghost "按 Tab 问 AI" 后显式按 Tab → 前端 invoke `trigger_ai` command
        // → SearchService::trigger_ai → 单次 spawn。

        let elapsed = search_start.elapsed().as_secs_f64() * 1000.0;
        crate::infra::utils::perf::record(
            crate::infra::utils::perf::MetricCategory::SearchEngine,
            "total",
            elapsed,
            None,
        );

        all_items
    }

    /// 显式触发 AI(0.9.2 Phase 5b Tab 显式触发)——由 `trigger_ai` command 调用。
    ///
    /// 单次 spawn,复用 `spawn_mixed_lane` 模式:
    /// - `tauri::async_runtime::spawn` 独立 task
    /// - **同步 emit AI placeholder** 走 `blink://results`,让 UI <100ms 见到"AI 思考中…"
    /// - AI 完成后 emit 真结果替换占位
    ///
    /// **前置**:调用方需已过 gating 筛子(前端见 Ghost 才允许按 Tab)。
    /// 若 registry 为 None(setup 未完成)或 resolve NotConfigured → 静默 clear + 返 Ok。
    pub fn trigger_ai(&self, query: String, seq: u64) {
        let Some(registry) = self
            .ai_registry
            .read()
            .expect("ai_registry lock poisoned")
            .clone()
        else {
            tracing::debug!(target: ai_slo::TARGET, "trigger_ai: registry 未就绪,忽略");
            return;
        };
        // 记住这次 seq 作为最新——后续 emit 用此校验(避免和 search_apps 的 seq 混串)
        self.latest_seq.store(seq, Ordering::SeqCst);

        // 立即 emit 占位:让 UI 在 Tab 按下瞬间就有反馈
        emit_ai_result(&self.app, seq, ai_placeholder_entry());

        self.spawn_ai_lane(query, registry, seq);
    }

    /// AI lane(0.9.2 Phase 5b)——独立 spawn,不阻塞主链路。
    ///
    /// 复用 `spawn_mixed_lane` 模式:
    /// - `tauri::async_runtime::spawn` 独立 task
    /// - seq 校验丢弃过期结果
    /// - `emit_results` 前端自动 merge 替换占位(`source="ai"` 一致)
    ///
    /// **§6.4 兜底铁则**:任何 Err 分支都 emit 空清占位 + 打 SLO,不 panic、
    /// 不影响已同步返回的 fuzzy 主结果。
    ///
    /// **日志政策**(0.9.2 优化):每个调用两条 event
    /// - 起始 1 条 `debug`——包含 provider/tier/model/timeout,方便对上号
    /// - 结束 1 条 `info`(成功)或 `warn`(失败)——包含 elapsed/first_token_ms/结果
    /// 不再逐字段拆散、不打"发起 → 收到 → 映射"三条,让 grep 出的日志一目了然。
    fn spawn_ai_lane(&self, query: String, registry: Arc<AIProviderRegistry>, seq: u64) {
        let app = self.app.clone();
        let pool = self.pool.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let lang = self
            .language
            .read()
            .expect("language lock poisoned")
            .clone();
        tauri::async_runtime::spawn(async move {
            // resolve(Tier::Router) —— 空档降级链在 registry.resolve 内部走
            let (provider, actual_tier) = match registry.resolve(Tier::Router) {
                Ok(t) => t,
                Err(AIError::NotConfigured) => {
                    // 无 provider 池:清占位,不打 SLO(不算真调用)
                    tracing::debug!(
                        target: ai_slo::TARGET,
                        "AI: 未配置或档位悬空,清占位"
                    );
                    emit_ai_clear(&app, seq, Some("AI 未配置或档位悬空"));
                    return;
                }
                Err(e) => {
                    tracing::warn!(target: ai_slo::TARGET, "AI resolve 失败: {e}");
                    emit_ai_clear(&app, seq, Some(&format!("AI 错误: {e}")));
                    return;
                }
            };
            // provider 上下文——所有 SLO 日志都要带,方便用户自诊断"哪个供应商慢/错"
            let provider_kind = provider.kind();
            let provider_model = provider.model_id().to_string();

            let cfg = registry.config_snapshot();
            let timeout_ms = cfg
                .slo_hard_timeout_ms
                .unwrap_or(AI_DEFAULT_HARD_TIMEOUT_MS);

            // 0.9.7 Step 4: 聚合 tools 列表 = Action 分组 + 插件独立 + Capability 独立
            let action_reg = app.state::<Arc<ActionRegistry>>();
            let cap_reg = app.state::<Arc<CapabilityRegistry>>();
            let tools =
                crate::domain::execution::group::build_aggregated_tools(&action_reg, &cap_reg);

            // 0.11.1 §2.3b: 参数 schema 动态注入插件 settings——
            // 插件 tool 的 schema 根据用户已配置的 settings 动态调整：
            // required → optional + 注入 default + description 增强。
            // 让 AI 在有默认城市时不再追问用户"查什么城市"（D2 修复）。
            //
            // 0.11.3 改进 4: 同时收集 manifest `tools[].hint` 字段，
            // 供 prompt::routing_system_prompt 拼入 system prompt 工具描述段。
            //
            // 0.11.4 改进 2 §3.2: 同时收集 manifest `tools[].progress_hint` 字段，
            // 供 Turn 2 回流占位文案动态化（"AI 正在{progress_hint}…"）。
            let plugin_engine = app.state::<Arc<PluginEngine>>();
            let mut plugin_bindings: std::collections::HashMap<
                String,
                (String, std::collections::HashMap<String, String>),
            > = std::collections::HashMap::new();
            let mut plugin_hints: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            let mut plugin_progress_hints: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            for ph in plugin_engine.all_plugins() {
                let manifest = ph.manifest();
                for td in &manifest.tools {
                    let id = crate::domain::plugin::plugin_tool_id(&manifest.id, &td.name);
                    if let Some(bindings) = &td.setting_bindings {
                        plugin_bindings.insert(id.clone(), (manifest.id.clone(), bindings.clone()));
                    }
                    if let Some(hint) = &td.hint {
                        plugin_hints.insert(id.clone(), hint.clone());
                    }
                    if let Some(progress_hint) = &td.progress_hint {
                        plugin_progress_hints.insert(id, progress_hint.clone());
                    }
                }
            }
            let tools: Vec<ActionSchema> = tools
                .into_iter()
                .map(|schema| {
                    if let Some((plugin_id, bindings)) = plugin_bindings.get(&schema.name) {
                        let settings = plugin_engine.get_settings(plugin_id);
                        crate::domain::execution::group::inject_plugin_settings(
                            schema,
                            settings.as_ref(),
                            bindings,
                        )
                    } else {
                        schema
                    }
                })
                .collect();
            let tools_count = tools.len();

            // 0.11.3 改进 4: system prompt 从 ai/prompt.rs 统一生成，
            // 工具列表含参数摘要 + 插件 hint，构建时估算 token 数超阈值 warn。
            let prompt_infos =
                crate::domain::ai::prompt::build_prompt_infos(tools.clone(), &plugin_hints);
            let system_prompt =
                crate::domain::ai::prompt::routing_system_prompt(&prompt_infos, &lang);

            // 0.9.7 Step 4 铁则 1: AI lane 派给 Capability 的预算 = AI 总预算 - 已耗时间。
            // handle_ai_tool_calls 收到此 deadline 后构造 InvokeContext 传给 Capability。
            let ai_start = std::time::Instant::now();
            let ai_deadline = Some(ai_start + std::time::Duration::from_millis(timeout_ms as u64));

            // 0.11.4 改进 2: 构造 Turn2Context（§2.2 两轮 complete 协议）。
            let feedback_config = cfg.ai_tool_result_feedback;
            let turn2_ctx = Turn2Context {
                provider: Arc::clone(&provider),
                provider_kind,
                provider_model: provider_model.clone(),
                should_run: feedback_config.should_run(provider_kind),
                feedback_config,
                prompt_infos: prompt_infos.clone(),
                user_query: query.clone(),
                tools: tools.clone(),
                pool,
                lang: lang.clone(),
                deadline: ai_deadline,
                progress_hints: plugin_progress_hints,
            };

            let req = CompletionRequest {
                messages: vec![
                    ChatMessage::system(&system_prompt),
                    ChatMessage::user(&query),
                ],
                tools,
                max_tokens: None,
                temperature: Some(0.0),
                timeout_ms: Some(timeout_ms),
            };

            // 起始日志:一行说清"哪个 provider + 什么档 + 什么模型 + 多长超时 + 几个 tool"
            let use_streaming = cfg.streaming;
            tracing::debug!(
                target: ai_slo::TARGET,
                "AI → {:?}/{} tier={:?} timeout={}ms qlen={} tools={} streaming={}",
                provider_kind,
                provider_model,
                actual_tier,
                timeout_ms,
                query.chars().count(),
                tools_count,
                use_streaming,
            );

            // start 已在 ai_deadline 计算前定义为 ai_start
            let start = ai_start;

            if use_streaming {
                // ── 流式路径:provider.stream() + channel 逐 chunk emit ──
                let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
                let provider_clone = Arc::clone(&provider);
                let stream_future = async move { provider_clone.stream(req, tx).await };

                // spawn 流式 producer;主 task 做 consumer + emit
                let producer_handle = tauri::async_runtime::spawn(stream_future);

                let mut accumulated = String::new();

                // 逐 chunk 消费
                while let Some(chunk) = rx.recv().await {
                    // seq 校验:用户已输入新 query → 丢弃后续 chunk
                    if seq != latest_seq.load(Ordering::SeqCst) {
                        tracing::trace!(target: ai_slo::TARGET, "AI stream 过期,丢弃 seq={seq}");
                        // abort producer task
                        producer_handle.abort();
                        return;
                    }

                    match chunk {
                        StreamChunk::Text(text) => {
                            accumulated.push_str(&text);
                            emit_ai_stream(&app, seq, &text, &accumulated, false);
                        }
                        StreamChunk::Done { tool_calls, .. } => {
                            let elapsed = start.elapsed().as_millis() as u32;
                            let text_len = accumulated.chars().count();
                            tracing::info!(
                                target: ai_slo::TARGET,
                                "AI ← {:?}/{} stream ok elapsed={}ms text={}chars tool_calls={}",
                                provider_kind,
                                provider_model,
                                elapsed,
                                text_len,
                                tool_calls.len(),
                            );

                            // 处理 tool_calls(与非流式路径一致)
                            // ★ 先处理 tool_calls 再决定是否发 done=true——
                            //   tool-call 路径自己 emit 最终结果(confirm/done/clear),
                            //   不需要 done=true 提前把 placeholder 变成可复制文本,
                            //   否则 Dangerous 确认卡片会插入新卡而非替换占位。
                            if !tool_calls.is_empty() {
                                handle_ai_tool_calls(
                                    &app,
                                    seq,
                                    &tool_calls,
                                    &accumulated,
                                    &lang,
                                    &latest_seq,
                                    ai_deadline,
                                    &turn2_ctx,
                                )
                                .await;
                            } else {
                                // 纯文本回答——先发 done=true 再发可复制结果
                                emit_ai_stream(&app, seq, "", &accumulated, true);
                                if !accumulated.trim().is_empty() {
                                    emit_ai_result(&app, seq, ai_result_entry(accumulated));
                                }
                            }
                            return;
                        }
                    }
                }

                // channel 关闭但没收到 Done → producer 出错了
                let producer_result = producer_handle.await;
                let elapsed = start.elapsed().as_millis() as u32;
                match producer_result {
                    Ok(Ok(())) => {
                        // 不该走到这里(正常应收到 Done),兜底发结果
                        tracing::warn!(target: ai_slo::TARGET, "AI stream 结束但未收到 Done");
                        if !accumulated.trim().is_empty() {
                            emit_ai_stream(&app, seq, "", &accumulated, true);
                            emit_ai_result(&app, seq, ai_result_entry(accumulated));
                        } else {
                            emit_ai_clear(&app, seq, None);
                        }
                    }
                    Ok(Err(e)) => {
                        tracing::warn!(
                            target: ai_slo::TARGET,
                            "AI ← {:?}/{} stream ERR elapsed={}ms: {}",
                            provider_kind, provider_model, elapsed, e,
                        );
                        emit_ai_clear(&app, seq, Some(&format!("{e}")));
                    }
                    Err(join_err) => {
                        tracing::warn!(
                            target: ai_slo::TARGET,
                            "AI stream task panic: {}",
                            join_err,
                        );
                        emit_ai_clear(&app, seq, Some("AI 内部错误"));
                    }
                }
            } else {
                // ── 非流式路径:provider.complete() 一次性返回 ──
                let result = provider.complete(req).await;
                let elapsed = start.elapsed().as_millis() as u32;

                // seq 校验:用户已输入新 query → 丢弃结果
                if seq != latest_seq.load(Ordering::SeqCst) {
                    tracing::trace!(target: ai_slo::TARGET, "AI 结果过期,丢弃 seq={seq}");
                    return;
                }

                // 结束日志:成功/失败合并到一处,字段与起始日志对得上
                match result {
                    Ok(resp) => {
                        let text_len = resp.text.as_ref().map(|s| s.chars().count()).unwrap_or(0);
                        let tc_count = resp.tool_calls.len();
                        tracing::info!(
                            target: ai_slo::TARGET,
                            "AI ← {:?}/{} ok elapsed={}ms first_token={}ms text={}chars tool_calls={}",
                            provider_kind,
                            provider_model,
                            elapsed,
                            resp.first_token_ms,
                            text_len,
                            tc_count,
                        );

                        // 0.9.3:处理 tool_call（支持分组解析）
                        if !resp.tool_calls.is_empty() {
                            handle_ai_tool_calls(
                                &app,
                                seq,
                                &resp.tool_calls,
                                resp.text.as_deref().unwrap_or(""),
                                &lang,
                                &latest_seq,
                                ai_deadline,
                                &turn2_ctx,
                            )
                            .await;
                            return;
                        }

                        // 纯文本回答(无 tool_call)
                        match resp.text.filter(|t| !t.trim().is_empty()) {
                            Some(text) => emit_ai_result(&app, seq, ai_result_entry(text)),
                            None => emit_ai_clear(&app, seq, None),
                        }
                    }
                    Err(AIError::Timeout) => {
                        tracing::warn!(
                            target: ai_slo::TARGET,
                            "AI ← {:?}/{} TIMEOUT elapsed={}ms (fallback→fuzzy)",
                            provider_kind,
                            provider_model,
                            elapsed,
                        );
                        emit_ai_clear(&app, seq, Some("AI 调用超时"));
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: ai_slo::TARGET,
                            "AI ← {:?}/{} ERR elapsed={}ms (fallback→fuzzy): {}",
                            provider_kind,
                            provider_model,
                            elapsed,
                            e,
                        );
                        emit_ai_clear(&app, seq, Some(&format!("{e}")));
                    }
                }
            }
        });
    }

    /// 过滤不满足前置条件的路由命中(0.5.1):禁用插件 + 参数过短。
    /// - Takeover 命中禁用/短参 → 降级空 Mixed(走 Generic 应用搜索),避免窗口空白。
    /// - Mixed 候选 → 剔除禁用/短参插件。
    /// 比「RuleRouter 加 API」简洁:路由表保持静态,过滤在结果层(无需重新注入)。
    ///
    /// **空参 Takeover 的 policy**（0.8.1 复审）：`arg == ""` 时**不做**过滤，让路由继续走
    /// 到 spawn——插件收到空 arg 查询自己决定语义（如天气插件返回默认城市、翻译插件历史等）。
    /// 若插件本身"空参无意义"（如翻译需要文本、搜索需要关键词），应在 manifest 里配
    /// `empty_arg_hint`——`SearchService::search` 会在 spawn 之前拦下并合成静态引导 entry
    /// （节省一次 IPC + 支持 i18n）。这两条路径共同承担"空参场景"，此 filter 不介入。
    ///
    /// **0.8.2 §3.4 Context 命中同样生效**：`Candidate` 里不区分 keyword/context 来源,
    /// filter 只看 `plugin_id` + `arg`——禁用插件 / min_arg_length 过滤链自动覆盖
    /// Context 命中的候选,无需额外分支。
    fn filter_route(&self, route: Route) -> Route {
        let pe = &self.plugin_engine;
        match route {
            Route::Takeover {
                ref plugin_id,
                ref arg,
                ..
            } => {
                // 检查禁用
                if !pe.is_enabled(plugin_id) {
                    tracing::debug!(plugin = %plugin_id, "禁用插件命中 takeover,降级 Generic");
                    return Route::Mixed { candidates: vec![] };
                }
                // 检查 min_arg_length:仅对带参前缀命中生效(参数太短降级,避免占位符死态)。
                // Exact 命中(arg 为 None)跳过检查——无参触发使用插件默认配置(如天气用默认城市)。
                let min_len = pe.get_min_arg_length(plugin_id);
                let arg_len = arg.char_len();
                if min_len > 0 && arg.is_explicit() && arg_len < min_len {
                    tracing::debug!(plugin = %plugin_id, %arg_len, min_len, "参数过短命中 takeover,降级 Generic");
                    return Route::Mixed { candidates: vec![] };
                }
                route
            }
            // EngineTakeover 不走 plugin 的 enabled / min_arg_length 检查——
            // 本体 engine 由本体决定生死（如 ClipboardEngine 通过 ClipboardService cfg.enabled
            // 控制监听器；search 阶段无第二重开关）。带参 engine 也不设参数长度门槛
            // （剪贴板搜索 "a" 一个字符也合理，engine 自决定 fuzzy 阈值）。
            Route::EngineTakeover { .. } => route,
            // AiTrigger 同 EngineTakeover——"ai" 是本体保留前缀，不走 plugin 检查。
            Route::AiTrigger { .. } => route,
            Route::Mixed { candidates } => Route::Mixed {
                candidates: candidates
                    .into_iter()
                    .filter(|c| pe.is_enabled(&c.plugin_id))
                    .filter(|c| {
                        // Exact 命中(arg 为 None)跳过 min_arg_length 检查
                        let min_len = pe.get_min_arg_length(&c.plugin_id);
                        if c.arg.is_none() {
                            return true; // 无参触发，用默认配置
                        }
                        let arg_len = c.arg.char_len();
                        min_len == 0 || arg_len >= min_len
                    })
                    .collect(),
            },
        }
    }

    /// Takeover 分支:查询单插件 → emit 增量。
    /// 即使插件返回空结果也要 emit(空 items 通知前端清除占位符,避免永远转圈)。
    fn spawn_takeover(&self, plugin_id: String, arg: String, seq: u64) {
        let plugin_engine = self.plugin_engine.clone();
        let debounce_ms = plugin_engine.get_debounce_ms(&plugin_id);
        let app = self.app.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let snapshot = Arc::clone(&self.snapshot);
        let max_results = Arc::clone(&self.max_results);
        tauri::async_runtime::spawn(async move {
            // 防抖:等待连续输入停止后再查询
            if debounce_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(debounce_ms)).await;
                if seq != latest_seq.load(Ordering::SeqCst) {
                    tracing::trace!(plugin = %plugin_id, debounce_ms, "takeover 防抖:seq 已过期,跳过");
                    return;
                }
            }
            let snapshot = snapshot.read().unwrap().clone();
            let ctx = crate::domain::plugin::PluginQueryContext::from_snapshot(&snapshot);
            let items = plugin_engine
                .query_subset(&[(plugin_id.clone(), arg)], &ctx)
                .await;
            if seq != latest_seq.load(Ordering::SeqCst) {
                return;
            }
            // Takeover:即使空结果也要 emit,让前端清除占位符
            let limit = max_results.load(Ordering::SeqCst);
            emit_results(&app, seq, items, limit, Some(&plugin_id));
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
        let max_results = Arc::clone(&self.max_results);

        tauri::async_runtime::spawn(async move {
            let history = crate::infra::data::history::get_weights(&pool).await;
            let snapshot = snapshot.read().unwrap().clone();
            let limit = max_results.load(Ordering::SeqCst);

            // priority 插件的 id 集合(查询完成后 score 抬高)
            let priority_set: std::collections::HashSet<String> = priority_candidates
                .into_iter()
                .map(|c| c.plugin_id)
                .collect();

            // ── 1. 插件查询任务（独立 spawn，支持 per-plugin 防抖）
            if !plugin_ids.is_empty() {
                let plugin_ctx =
                    crate::domain::plugin::PluginQueryContext::from_snapshot(&snapshot);
                let pe = plugin_engine.clone();
                let plugin_ids = plugin_ids.clone();
                let app = app.clone();
                let latest_seq = latest_seq.clone();
                // 取所有命中插件中最大的 debounce_ms（同一批查询共享一个 task）
                let max_debounce = plugin_ids
                    .iter()
                    .map(|(id, _)| pe.get_debounce_ms(id))
                    .max()
                    .unwrap_or(0);
                tauri::async_runtime::spawn(async move {
                    // 防抖:等待连续输入停止后再查询,避免每次按键都触发网络请求
                    if max_debounce > 0 {
                        tokio::time::sleep(std::time::Duration::from_millis(max_debounce)).await;
                        if seq != latest_seq.load(Ordering::SeqCst) {
                            tracing::trace!(debounce_ms = max_debounce, "插件防抖:seq 已过期,跳过");
                            return;
                        }
                    }
                    let mut items = pe.query_subset(&plugin_ids, &plugin_ctx).await;
                    // priority 候选 score 抬高,确保置顶
                    for item in &mut items {
                        if priority_set.contains(&item.source) {
                            item.score = boost_priority(item.score);
                        }
                    }
                    if seq == latest_seq.load(Ordering::SeqCst) {
                        // 即使空结果也要 emit,让前端清除占位符
                        tracing::trace!(count = items.len(), "插件查询返回");
                        // 插件查询：空结果时用第一个 plugin_id 作为来源
                        let empty_source = plugin_ids.first().map(|(id, _)| id.as_str());
                        emit_results(&app, seq, items, limit, empty_source);
                    }
                });
            }

            // ── 2. 每个 async 引擎独立 spawn(关键修复:不互相阻塞)
            //     跳过 takeover_only（0.8.5 §6.4）——同 sync 分支的语义对齐。
            //     目前无 takeover_only 的 async engine，此过滤是防御性对齐 trait 语义。
            for engine in async_engines {
                if engine.takeover_only() {
                    continue;
                }
                let q = query.clone();
                let app = app.clone();
                let latest_seq = latest_seq.clone();
                let history = history.clone(); // history 是 Arc<HashMap> 内部 move clone
                let snapshot = snapshot.clone();
                tauri::async_runtime::spawn(async move {
                    // async lane 引擎（file/mock）不消费 disabled_builtin_actions /
                    // disabled_context_bindings；这两个字段仅 BuiltinEngine（sync lane）读，
                    // 此处传空 slice 满足契约。
                    let ctx = QueryContext {
                        history: &history,
                        snapshot: &snapshot,
                        disabled_builtin_actions: &[],
                        disabled_context_bindings: &[],
                    };
                    let items = engine.search(&q, &ctx).await;
                    if seq == latest_seq.load(Ordering::SeqCst) && !items.is_empty() {
                        tracing::trace!(
                            engine = engine.id(),
                            count = items.len(),
                            "async lane 引擎返回"
                        );
                        emit_results(&app, seq, items, limit, None);
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

// source_rank / bake_source_boost 已统一移到 scorer.rs

/// emit 增量结果到前端。
/// 即使 items 为空也会 emit（空结果需要通知前端清除占位符）。
/// empty_source: 空结果时携带的来源 plugin_id，用于前端只清除对应占位符。
fn emit_results(
    app: &AppHandle,
    seq: u64,
    items: Vec<SearchItem>,
    limit: usize,
    empty_source: Option<&str>,
) {
    let entries: Vec<AppEntry> = if items.is_empty() {
        // 空结果:发送一个标记项让前端知道该插件已返回(清除占位符)
        // 用特殊 score=-2 标记,前端 merge 后会被排序到最后但保留来源信息
        let source = empty_source.unwrap_or("empty_result");
        tracing::debug!(source = %source, "emit 空结果标记");
        vec![AppEntry {
            name: String::new(),
            pinyin_name: String::new(),
            pinyin_full: String::new(),
            lnk_path: String::new(),
            is_calc: false,
            score: -2.0,
            is_placeholder: true, // 保留占位标记,前端用它清除占位符
            is_error: false,
            source: source.into(),
            description: None,
            action: Action::default(),
            ..Default::default()
        }]
    } else {
        fuse_items(items, limit)
            .into_iter()
            .map(SearchItem::into_app_entry)
            .collect()
    };
    for (i, item) in entries.iter().enumerate() {
        if item.is_error {
            tracing::debug!(index = i, score = %format!("{:.4}", item.score), source = %item.source, "增量结果: 插件错误信息");
        } else if item.name.is_empty() {
            tracing::debug!("增量结果: 空结果标记(清除占位符)");
        } else {
            let detail = item.score_detail.as_deref().unwrap_or("");
            tracing::trace!(
                index = i,
                score = if detail.is_empty() {
                    format!("{:.4}", item.score)
                } else {
                    format!("{:.4} ({})", item.score, detail)
                },
                source = %item.source,
                name = %item.name,
                "增量结果项"
            );
        }
    }
    if let Err(e) = app.emit(
        "blink://results",
        ResultsPayload {
            seq,
            items: entries,
        },
    ) {
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
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.0,
        is_placeholder: true,
        is_error: false,
        source: plugin_id.to_string(),
        description: Some("请稍候".into()),
        action: Action::default(),
        ..Default::default()
    }
}

/// 空参数引导项（0.8.1）：manifest 声明了 `empty_arg_hint` 且用户 arg 为空时，
/// 框架合成的静态展示项。相比 placeholder：
/// - `is_placeholder = false`（不是"查询中"，不会被 async 增量替换）
/// - `action = Action::default()` 前端点击/回车无操作（`lnk_path` 空 = 无路径可开）
/// - `name` 即引导文案（"输入文本开始翻译"），`description` 承载插件显示名
///
/// 前端 `results.js` 现有渲染分支 (`Action.kind=Open` + `lnk_path` 空 → 纯展示) 天然支持，
/// 无需前端改动。
fn empty_arg_hint_entry(plugin_id: &str, display_name: &str, hint: String) -> AppEntry {
    AppEntry {
        name: hint,
        pinyin_name: String::new(),
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.0,
        is_placeholder: false,
        is_error: false,
        source: plugin_id.to_string(),
        description: Some(display_name.to_string()),
        action: Action::default(),
        ..Default::default()
    }
}

// ── AI lane 辅助(0.9.2 Phase 5b)─────────────────────────────────────────

// 0.11.3 改进 4: build_routing_prompt 已迁移到 crate::domain::ai::prompt::routing_system_prompt。
// 工具列表增强（含参数摘要 + 插件 hint）+ token 监控（超 1500 warn）均在 prompt 模块内。

/// 解析 AI tool_call → 具体 Action + 解析后参数。
///
/// **聚合 tool**：name="system_action", arguments={action:"lock"} → (LockAction, {})
/// **独立 tool**：name="builtin_translate_translate", arguments={text:"hello"} → (PluginAction, {text:"hello"})
///
/// 返回 None 表示未找到对应 Action。
fn resolve_tool_call(
    tc: &crate::domain::ai::message::ToolCall,
    registry: &Arc<ActionRegistry>,
) -> Option<(Arc<dyn crate::domain::execution::Action>, serde_json::Value)> {
    use crate::domain::execution::group;

    // 检查是否命中分组
    if group::find_group(&tc.name).is_some() {
        // 从 arguments 中提取 action 字段
        let action_id = tc.arguments.get("action")?.as_str()?;
        let action = registry.get(action_id)?;

        // 移除 action 字段，剩余参数透传
        let mut args = tc.arguments.clone();
        if let Some(obj) = args.as_object_mut() {
            obj.remove("action");
        }

        Some((action, args))
    } else {
        // 独立 tool，直接查找
        let action = registry.get(&tc.name)?;
        Some((action, tc.arguments.clone()))
    }
}

/// 解析展示名称（用于日志和前端显示）。
///
/// 聚合 tool: "system_action" + action="lock" → "lock"
/// 独立 tool: "builtin_translate_translate" → "builtin_translate_translate"
fn resolve_display_name(
    tc: &crate::domain::ai::message::ToolCall,
    action: &Arc<dyn crate::domain::execution::Action>,
) -> String {
    use crate::domain::execution::group;

    if group::find_group(&tc.name).is_some() {
        // 聚合 tool，用具体 action id
        action.id().to_string()
    } else {
        // 独立 tool，用原始 name
        tc.name.clone()
    }
}

/// AI source 标记——占位与结果统一用此值,前端 `results.js` 现有 merge 按 source
/// 匹配替换占位(与 plugin placeholder 同机制,零前端改动)。
pub(crate) const AI_SOURCE: &str = "ai";

/// AI 占位项——`exec_mixed` 同步返回,<100ms 就绪(§3.3 首视觉反馈)。
///
/// - `source="ai"` 与真结果一致,前端自动替换
/// - `is_placeholder=true` 触发前端占位样式
/// - `action=Default`(Open + 空 lnk_path) → 前端点击/回车无操作(placeholder 不可执行)
pub(crate) fn ai_placeholder_entry() -> AppEntry {
    AppEntry {
        name: "AI 正在回答…".into(),
        pinyin_name: String::new(),
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.5, // 中位:可见但不抢首;真结果 0.7 会略高
        is_placeholder: true,
        is_error: false,
        source: AI_SOURCE.into(),
        // 占位状态**只保留一个信号源**——name 已足够表达"AI 在想",
        // 再叠一行 "请稍候" 是冗余。真结果时才用 description 承载"回车复制"提示。
        description: None,
        action: Action::default(),
        ..Default::default()
    }
}

/// AI 真结果项——回车/点击复制回答全文到剪贴板。
///
/// **为什么用 `ActionKind::Copy`**(0.9.2 第一步不引新 kind):
/// - 最接近"用户拿走这条文本"语义
/// - 不引入新 `ActionKind::Ai` 变体(留 0.9.3 tool_call 执行链路时统一考虑)
/// - `Action.payload` 已为"携带待复制文本"设计,完美匹配
///
/// **name 不截断**:前端 `.ai-item` 走多行展开样式(0.9.2 §6.4),完整文本存进
/// `name` 让 CSS `white-space: pre-wrap` 自然渲染。`payload` 也是完整文本,
/// Copy 动作复制全文(两处冗余但语义清晰,不引结构复杂度)。
pub(crate) fn ai_result_entry(text: String) -> AppEntry {
    use crate::domain::search::ActionKind;
    AppEntry {
        name: text.clone(),
        pinyin_name: String::new(),
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.7, // 略高于普通 fuzzy 的默认位次,让 AI 回答在无 rule 命中时置顶
        is_placeholder: false,
        is_error: false,
        source: AI_SOURCE.into(),
        description: Some("回车复制回答".into()),
        action: Action {
            kind: ActionKind::Copy,
            payload: Some(text),
            hint: Some("复制回答".into()),
            ..Default::default()
        },
        is_ai_summary: true, // §3.1 AI 总结项——前端 pre-wrap 撑开 + 24px 徽章
        ..Default::default()
    }
}

/// emit AI 结果——单条 AppEntry 走 `blink://results`,前端 merge 按 `source="ai"`
/// 替换 placeholder。
///
/// 不复用 `emit_results`:那个吃 `Vec<SearchItem>` 走 fuse_items(会重排序),
/// AI 只有一条,直接构造 payload emit 更直白。
fn emit_ai_result(app: &AppHandle, seq: u64, entry: AppEntry) {
    if let Err(e) = app.emit(
        "blink://results",
        ResultsPayload {
            seq,
            items: vec![entry],
        },
    ) {
        tracing::debug!(error = %e, "emit AI result failed");
    }
}

/// emit AI 多条结果——Capability `Items` 返回时用（如 search_files 返回文件列表）。
///
/// 前端 merge 按 `source="ai"` 整体替换 placeholder——多条结果
/// 在前端渲染为可选列表，Alt+1 打开第一条。
fn emit_ai_result_multi(app: &AppHandle, seq: u64, entries: Vec<AppEntry>) {
    if let Err(e) = app.emit(
        "blink://results",
        ResultsPayload {
            seq,
            items: entries,
        },
    ) {
        tracing::debug!(error = %e, "emit AI multi-result failed");
    }
}

/// AI 流式 chunk 事件 payload——每个 Text chunk emit 一次,前端增量拼接展示。
#[derive(Clone, Serialize)]
struct AiStreamPayload {
    seq: u64,
    /// 增量文本片段
    delta: String,
    /// 累积全文(前端直接替换 name,不用自己拼接)
    accumulated: String,
    /// 是否为最后一条(流结束)
    done: bool,
}

/// emit AI 流式 chunk —— `blink://ai-stream` 事件。
fn emit_ai_stream(app: &AppHandle, seq: u64, delta: &str, accumulated: &str, done: bool) {
    if let Err(e) = app.emit(
        "blink://ai-stream",
        AiStreamPayload {
            seq,
            delta: delta.to_string(),
            accumulated: accumulated.to_string(),
            done,
        },
    ) {
        tracing::debug!(error = %e, "emit AI stream failed");
    }
}

/// emit AI 清占位——发一个 `source="ai"` 的空标记项,前端识别后移除占位行。
///
/// 用途:超时/供应商错/密钥缺失/AI 返回空文本——所有"没有真结果"的分支都清占位,
/// 避免"AI 思考中…"永久转圈。
///
/// 若传 `error_msg`,前端展示为橙色错误项(不可点击),用户能看到失败原因。
fn emit_ai_clear(app: &AppHandle, seq: u64, error_msg: Option<&str>) {
    if let Some(msg) = error_msg {
        // 错误项:is_error=true,前端渲染为橙色警告(复用插件 error-item 样式)
        let error_entry = AppEntry {
            name: msg.to_string(),
            pinyin_name: String::new(),
            pinyin_full: String::new(),
            lnk_path: String::new(),
            is_calc: false,
            score: 0.5,
            is_placeholder: false,
            is_error: true,
            source: AI_SOURCE.into(),
            description: None,
            action: Action::default(),
            ..Default::default()
        };
        if let Err(e) = app.emit(
            "blink://results",
            ResultsPayload {
                seq,
                items: vec![error_entry],
            },
        ) {
            tracing::debug!(error = %e, "emit AI error failed");
        }
    } else {
        // 无错误信息时走原逻辑:空标记清占位
        let clear_marker = AppEntry {
            name: String::new(),
            pinyin_name: String::new(),
            pinyin_full: String::new(),
            lnk_path: String::new(),
            is_calc: false,
            score: -2.0,
            is_placeholder: true,
            is_error: false,
            source: AI_SOURCE.into(),
            description: None,
            action: Action::default(),
            ..Default::default()
        };
        if let Err(e) = app.emit(
            "blink://results",
            ResultsPayload {
                seq,
                items: vec![clear_marker],
            },
        ) {
            tracing::debug!(error = %e, "emit AI clear failed");
        }
    }
}

/// 处理 AI tool_calls —— 流式/非流式共用的执行逻辑。
///
/// **0.9.7 Step 4**: 先查 CapabilityRegistry,命中则走 Capability 分支;
/// 未命中再走 Action 解析。
///
/// **0.11.4 改进 2**: 接收 Turn2Context,使用 Turn 1 结果→Turn 2 回流机制。
/// - Capability/Safe Action: execute_*_for_turn1 → write_audit(turn=1) → dispatch_turn1_result
/// - Dangerous: emit_ai_confirm (不执行,无审计)
/// - 未知: fallback_text (不执行,无审计)
///
/// Action 路径: 解析 tool_call → (Action, 参数),按 DangerClass 分支:
/// - Safe:执行并返回 ToolExecutionResult → dispatch_turn1_result 决定直通或 Turn 2
/// - Dangerous:emit 确认卡片,等用户 Enter/Esc
/// - 未知 action:回退到文本回答(若有)
async fn handle_ai_tool_calls(
    app: &AppHandle,
    seq: u64,
    tool_calls: &[crate::domain::ai::message::ToolCall],
    fallback_text: &str,
    _lang: &str,
    latest_seq: &AtomicU64,
    deadline: Option<Instant>,
    turn2_ctx: &Turn2Context,
) {
    let tc = &tool_calls[0]; // 主窗口只取第一个

    // §3.2: Turn 1 工具执行前 emit progress_hint 占位文案
    // 与 Turn 2 (handle_turn2_tool_call) 对齐——用户在工具执行期间看到阶段文案变化
    let progress_hint = derive_progress_hint(&tc.name, "", &turn2_ctx.progress_hints);
    emit_ai_result(
        app,
        seq,
        ai_progress_placeholder(format!("AI 正在{progress_hint}…")),
    );

    // 0.9.7 Step 4: 先查 Capability——Capability 优先于 Action
    let cap_reg = app.state::<Arc<CapabilityRegistry>>();
    if cap_reg.get(&tc.name).is_some() {
        if let Some(result) =
            execute_capability_for_turn1(app, seq, tc, &cap_reg, latest_seq, deadline).await
        {
            // 写审计日志 (turn=1)
            write_audit(
                &turn2_ctx.pool,
                &result.tool_name,
                &result.arguments,
                &result.result_summary,
                turn2_ctx.provider_kind.as_serde_str(),
                &turn2_ctx.provider_model,
                1,
            )
            .await;
            // 分发结果（直通或 Turn 2）
            dispatch_turn1_result(app, seq, result, Some(turn2_ctx), latest_seq).await;
        }
        return;
    }

    // Action 路径
    let action_reg = app.state::<Arc<ActionRegistry>>();
    let resolved = resolve_tool_call(tc, &action_reg);

    match resolved {
        Some((action, args)) => match action.danger_class() {
            DangerClass::Safe => {
                // 执行 Safe Action → ToolExecutionResult
                let result = execute_action_for_turn1(
                    app,
                    seq,
                    tc,
                    &action,
                    args,
                    &turn2_ctx.lang,
                    latest_seq,
                )
                .await;

                // 写审计日志 (turn=1)
                write_audit(
                    &turn2_ctx.pool,
                    &result.tool_name,
                    &result.arguments,
                    &result.result_summary,
                    turn2_ctx.provider_kind.as_serde_str(),
                    &turn2_ctx.provider_model,
                    1,
                )
                .await;

                // 分发结果（直通或 Turn 2）
                dispatch_turn1_result(app, seq, result, Some(turn2_ctx), latest_seq).await;
            }
            DangerClass::Dangerous => {
                tracing::info!(
                    target: ai_slo::TARGET,
                    "AI tool_call 需确认: {} (Dangerous)",
                    tc.name,
                );
                let title = action.title().resolve(&turn2_ctx.lang).to_string();
                let display_name = resolve_display_name(tc, &action);
                emit_ai_confirm(app, seq, &display_name, &args, &title);
            }
        },
        None => {
            tracing::warn!(
                target: ai_slo::TARGET,
                "AI tool_call 未知动作: {},回退文本",
                tc.name,
            );
            match fallback_text.trim() {
                t if !t.is_empty() => {
                    emit_ai_result(app, seq, ai_result_entry(t.to_string()));
                }
                _ => emit_ai_clear(app, seq, Some(&format!("AI 调用了未知动作: {}", tc.name))),
            }
        }
    }
}

// ── 0.9.7 Step 4: Capability 调用 + 前端投影 ────────────────────────────────

/// `CapabilityResult` → `Vec<AppEntry>` 前端投影（0.9.7 Step 4）。
///
/// 消费方决定投影形态——主窗口模式走此函数（前端展示）;
/// AI multi-turn 走 `CapabilityResult::to_rig_tool_result()`（0.10）;
/// CLI 走 stdout（0.11）。Capability 层零分支。
fn capability_result_to_entries(result: &CapabilityResult) -> Vec<AppEntry> {
    match result {
        CapabilityResult::Text { content } => {
            vec![ai_result_entry(content.clone())]
        }
        CapabilityResult::Items { items } => items_to_entries(items),
        CapabilityResult::Blob { mime, bytes } => {
            // Blob → 展示摘要信息（0.10 多模态才把图片喂回 AI）
            let size_kb = bytes.len() as f64 / 1024.0;
            let size_text = if size_kb >= 1024.0 {
                format!("{:.1} MB", size_kb / 1024.0)
            } else {
                format!("{:.1} KB", size_kb)
            };
            vec![AppEntry {
                name: format!("✓ 已获取 {} ({})", mime, size_text),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.7,
                is_placeholder: false,
                is_error: false,
                source: AI_SOURCE.into(),
                description: Some("AI 已获取数据".into()),
                action: Action::default(),
                ..Default::default()
            }]
        }
        CapabilityResult::Done { summary } => {
            vec![AppEntry {
                name: format!("✓ {}", summary),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.7,
                is_placeholder: false,
                is_error: false,
                source: AI_SOURCE.into(),
                description: Some("AI 已执行此能力".into()),
                action: Action::default(),
                ..Default::default()
            }]
        }
    }
}

/// AI 路径工具结果项上限（0.11.0 §3.3 D5）。
/// 与查询路径 PAGE_SIZE 9 有区分，AI 更聚焦——省 token + 视觉不爆。
const AI_TOOL_ITEMS_LIMIT: usize = 5;

/// `ItemResult` 列表 → 前端 `AppEntry` 列表（0.11.0 改进 1 统一投影）。
///
/// **统一投影路径**：Action 路径（`PluginActionAdapter` → `ActionOutcome::Items`）与
/// Capability 路径（`CapabilityResult::Items`）共用此函数，避免"插件 Items 走 A 路径、
/// Capability Items 走 B 路径"的分叉（文档 §2.1 ★ 投影路径统一）。
///
/// **标记位**（§3.1）：每个 item 标 `is_ai_tool_result = true`——前端 nowrap 单行 +
/// 12px 小号 AI 图标，与查询路径结果视觉可区分。
///
/// **payload → action 投影**：
/// - 有 `path` → `ActionKind::Open`（打开应用/文件）
/// - 有 `text` → `ActionKind::Copy`（复制文本）
/// - 都没有 → `Action::default()`（纯展示，回车无操作）
///
/// **上限截断**（§3.3 D5）：最多 5 条，超出追加文字项 `还有 N 条，按 ↓ 查看全部`。
/// AI 总结项（item[0]）不计入上限——调用方在调用前单独构造 summary entry。
fn items_to_entries(items: &[crate::domain::capability::ItemResult]) -> Vec<AppEntry> {
    if items.is_empty() {
        return vec![];
    }

    let limit = AI_TOOL_ITEMS_LIMIT.min(items.len());
    let mut entries: Vec<AppEntry> = items
        .iter()
        .take(limit)
        .map(|item| {
            // 从 payload 提取 path（如果有）→ Open 动作；否则尝试 text → Copy
            let path = item.payload.get("path").and_then(|v| v.as_str());
            let text = item.payload.get("text").and_then(|v| v.as_str());
            AppEntry {
                name: item.title.clone(),
                lnk_path: path.unwrap_or("").to_string(),
                score: item.score.unwrap_or(0.5),
                is_placeholder: false,
                is_error: false,
                source: AI_SOURCE.into(),
                description: item.subtitle.clone(),
                action: if path.is_some() {
                    Action {
                        kind: ActionKind::Open,
                        ..Default::default()
                    }
                } else if let Some(t) = text {
                    Action {
                        kind: ActionKind::Copy,
                        payload: Some(t.to_string()),
                        ..Default::default()
                    }
                } else {
                    Action::default()
                },
                is_ai_tool_result: true,
                ..Default::default()
            }
        })
        .collect();

    // §3.3 D5：超出上限追加文字提示项
    let remaining = items.len().saturating_sub(limit);
    if remaining > 0 {
        entries.push(AppEntry {
            name: format!("还有 {} 条，按 ↓ 查看全部", remaining),
            score: -1.0,          // 排序到最后
            is_placeholder: true, // 前端识别为提示项
            source: AI_SOURCE.into(),
            ..Default::default()
        });
    }

    entries
}

/// AI tool_call 执行成功项——展示执行结果。
///
/// 与 `ai_result_entry` 类似但语义不同:
/// - 有执行结果(如 get_ip 返回 IP 地址)→ 展示结果文本,回车可复制
/// - 无执行结果(如 open_url)→ 展示"已执行 {动作名}"
///
/// `lang` 透传自 Turn2Context.lang,让英文界面用户看到英文动作名（0.11 review B4 修复）。
fn ai_action_done_entry(
    action: &dyn crate::domain::execution::Action,
    outcome: &crate::domain::execution::ActionOutcome,
    lang: &str,
) -> AppEntry {
    use crate::domain::execution::ActionOutcome;
    use crate::domain::search::ActionKind;

    let title = action.title().resolve(lang);

    // 从 ActionOutcome 提取结果文本
    let result_text = match outcome {
        ActionOutcome::Copy { text, .. } => Some(text.clone()),
        ActionOutcome::Open { path } => Some(format!("已打开: {path}")),
        ActionOutcome::Emit { .. } => None, // 副作用型,无文本结果
        ActionOutcome::Nop => None,
    };

    match result_text {
        Some(text) if !text.is_empty() => {
            // 有结果文本 → 展示结果,回车可复制(与 ai_result_entry 一致)
            AppEntry {
                name: text.clone(),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.7,
                is_placeholder: false,
                is_error: false,
                source: AI_SOURCE.into(),
                description: Some(format!("✓ {title} · 回车复制")),
                action: Action {
                    kind: ActionKind::Copy,
                    payload: Some(text),
                    hint: Some("复制结果".into()),
                    ..Default::default()
                },
                ..Default::default()
            }
        }
        _ => {
            // 无结果文本 → 展示"已执行"
            AppEntry {
                name: format!("✓ 已执行：{title}"),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.7,
                is_placeholder: false,
                is_error: false,
                source: AI_SOURCE.into(),
                description: Some("AI 已执行此动作".into()),
                action: Action::default(),
                ..Default::default()
            }
        }
    }
}

/// AI Dangerous 动作确认请求——emit 到前端,展示确认卡片等用户 Enter/Esc。
///
/// **事件**: `blink://ai-confirm-action`
/// **payload**: `{ seq, actionName, actionTitle, arguments, dangerClass }`
fn emit_ai_confirm(
    app: &AppHandle,
    seq: u64,
    action_name: &str,
    arguments: &serde_json::Value,
    title: &str,
) {
    #[derive(serde::Serialize, Clone)]
    struct ConfirmPayload {
        seq: u64,
        action_name: String,
        action_title: String,
        arguments: serde_json::Value,
        danger_class: String,
    }
    let payload = ConfirmPayload {
        seq,
        action_name: action_name.to_string(),
        action_title: title.to_string(),
        arguments: arguments.clone(),
        danger_class: "Dangerous".to_string(),
    };
    if let Err(e) = app.emit("blink://ai-confirm-action", payload) {
        tracing::debug!(error = %e, "emit AI confirm failed");
    }
}

// ── 0.11.4 改进 2: 结果回流 AI (Turn 2) ──────────────────────────────────────
//
// 文档 §2.2 两轮 complete 协议:
//   Turn 1: complete(messages=[system, user], tools=[all]) → tool_call_1 → execute → result_1
//   Turn 2: complete(messages=[system, user, assistant(tool_call_1), tool(result_1)], tools=[safe_only])
//          → (a) text answer → emit 文本回答(总结),结束
//          (b) tool_call_2(safe) → execute → emit 执行结果,结束
//          (c) tool_call_2(dangerous) → emit 确认卡片,结束
//
// 三态配置 (§2.2.2 D2): ai_tool_result_feedback = Auto(默认) / On / Off
// Auto: 本地模型开 + 云端模型关 (0.11 所有 provider 云端 → 等同 Off)
//
// 产品体验 (§3.2-3.7): 占位文案动态化 / Turn 1 结果不提前 emit / 超时降级 / 错误展示 /
// 自动执行反馈 / 回流开关体验一致性。

/// Turn 1 工具执行结果——承载执行后的所有信息，供 Turn 2 回流消费或直接 emit 前端。
///
/// **设计意图**：把"执行工具"和"emit 结果"解耦——回流开启时 Turn 1 结果不 emit，
/// 喂回 AI 做 Turn 2；回流关闭时直接 emit。`ToolExecutionResult` 是两条路径的公共中间态。
#[derive(Debug)]
struct ToolExecutionResult {
    /// 工具名（如 `builtin.weather:get_weather` / `open_path`）。
    tool_name: String,
    /// tool_call_id（关联 Tool 消息用）。
    tool_call_id: String,
    /// 参数 JSON（审计日志用）。
    arguments: serde_json::Value,
    /// 投影成 Tool 消息内容（喂回 AI 用，§2.2.3 回流内容投影）。
    tool_message_content: String,
    /// 前端展示的 entries（emit 用）。
    entries: Vec<AppEntry>,
    /// 结果摘要（审计日志用）。
    result_summary: String,
    /// 是否执行成功（false = 执行错误，entries 含错误项）。
    success: bool,
}

/// Turn 2 回流上下文——由 `spawn_ai_lane` 构造，传给 `handle_ai_tool_calls`。
///
/// 若 `should_run` 为 false，则 `handle_ai_tool_calls` 走单轮直通（现状）。
/// 若为 true，Turn 1 执行后进入 Turn 2 回流。
struct Turn2Context {
    provider: Arc<dyn AIProvider>,
    provider_kind: crate::app::ai_config::ProviderKind,
    provider_model: String,
    /// 是否实际运行 Turn 2（`ToolResultFeedback::should_run(provider_kind)`）。
    should_run: bool,
    /// 原始配置值（区分 Auto+cloud vs Off，决定是否追加提示文案 §3.7）。
    feedback_config: ToolResultFeedback,
    /// Turn 2 prompt 工具信息（全量，Turn 2 时过滤 safe_only 子集）。
    prompt_infos: Vec<crate::domain::ai::prompt::ToolPromptInfo>,
    /// 用户原始输入（Turn 2 messages 需要）。
    user_query: String,
    /// 全量 tools（Turn 2 过滤 safe_only）。
    tools: Vec<ActionSchema>,
    /// SQLite 连接池（审计日志写入用）。
    pool: SqlitePool,
    lang: String,
    /// AI 总预算 deadline（Turn 2 从中派生独立超时）。
    deadline: Option<Instant>,
    /// progress_hint 映射（§3.2 占位文案用）。
    progress_hints: std::collections::HashMap<String, String>,
}

/// 从 manifest/CapabilitySchema/ActionSchema 派生 progress_hint（§3.2）。
///
/// 优先取 manifest `tools[].progress_hint`；缺失时用 description 前 8 字 + `…`。
fn derive_progress_hint(
    tool_name: &str,
    description: &str,
    explicit_hints: &std::collections::HashMap<String, String>,
) -> String {
    if let Some(hint) = explicit_hints.get(tool_name) {
        if !hint.is_empty() {
            return hint.clone();
        }
    }
    // 回退: description 前 8 字 + …
    let chars: Vec<char> = description.chars().take(8).collect();
    if chars.is_empty() {
        return "处理中".to_string();
    }
    let prefix: String = chars.into_iter().collect();
    // 如果 description 超过 8 字，加 …
    if description.chars().count() > 8 {
        format!("{prefix}…")
    } else {
        prefix
    }
}

/// 投影 `ActionOutcome` → 结果摘要（审计日志用）。
pub(crate) fn outcome_to_summary(outcome: &ActionOutcome) -> String {
    match outcome {
        ActionOutcome::Copy { text, .. } => {
            let truncated = truncate_summary_for_audit(text);
            format!("Copy: {truncated}")
        }
        ActionOutcome::Open { path } => format!("Open: {path}"),
        ActionOutcome::Emit { event, .. } => format!("Emit: {event}"),
        ActionOutcome::Nop => "Nop".to_string(),
    }
}

/// 投影 `CapabilityResult` → 结果摘要（审计日志用）。
fn capability_result_to_summary(result: &CapabilityResult) -> String {
    match result {
        CapabilityResult::Text { content } => {
            format!("Text: {}", truncate_summary_for_audit(content))
        }
        CapabilityResult::Items { items } => format!("Items({} 项)", items.len()),
        CapabilityResult::Blob { mime, bytes } => {
            format!("Blob: {} ({} bytes)", mime, bytes.len())
        }
        CapabilityResult::Done { summary } => format!("Done: {summary}"),
    }
}

/// 审计摘要截断（200 字符，比 audit 表的 500 更短，避免日志过长）。
fn truncate_summary_for_audit(s: &str) -> String {
    const MAX: usize = 200;
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= MAX {
        return s.to_string();
    }
    let truncated: String = chars.iter().take(MAX).collect();
    format!("{truncated}…")
}

/// 写审计日志（封装 `ai_audit::save_audit_log`，不阻塞主流程）。
///
/// `caller` 固定为 `"internal"`——此函数仅用于 AI tool call / 用户确认执行的审计。
/// 外部 MCP client 调用的审计走 `BlinkMcpServer` 直接调 `save_audit_log`（caller = "mcp_external"）。
async fn write_audit(
    pool: &SqlitePool,
    tool_name: &str,
    arguments: &serde_json::Value,
    result_summary: &str,
    provider_kind: &str,
    model_id: &str,
    turn: u8,
) {
    crate::infra::data::ai_audit::save_audit_log(
        pool,
        tool_name,
        arguments,
        result_summary,
        provider_kind,
        model_id,
        turn,
        "internal",
    )
    .await;
}

/// 执行 Capability 并返回 `ToolExecutionResult`（抽取自 `handle_capability_call`）。
///
/// 返回 `None` = seq 过期 / Cancelled（不 emit，不回流）。
/// 返回 `Some(result)` = 执行完成（成功或失败），由调用方决定 emit 或回流。
async fn execute_capability_for_turn1(
    app: &AppHandle,
    seq: u64,
    tc: &crate::domain::ai::message::ToolCall,
    cap_registry: &Arc<CapabilityRegistry>,
    latest_seq: &AtomicU64,
    deadline: Option<Instant>,
) -> Option<ToolExecutionResult> {
    // 铁则 2: seq 校验——用户已切走 → None
    if seq != latest_seq.load(Ordering::SeqCst) {
        tracing::trace!(
            target: ai_slo::TARGET,
            capability = %tc.name,
            "Capability seq 过期(开始前),丢弃"
        );
        return None;
    }

    tracing::info!(
        target: ai_slo::TARGET,
        capability = %tc.name,
        args = %tc.arguments,
        "AI tool_call → Capability invoke"
    );

    let ctx = InvokeContext {
        app_handle: app,
        deadline,
    };

    let result = cap_registry
        .invoke(&tc.name, tc.arguments.clone(), &ctx)
        .await;

    // 铁则 2: seq 再次校验
    if seq != latest_seq.load(Ordering::SeqCst) {
        tracing::trace!(
            target: ai_slo::TARGET,
            capability = %tc.name,
            "Capability seq 过期(完成后),丢弃"
        );
        return None;
    }

    match result {
        Ok(cap_result) => {
            let entries = capability_result_to_entries(&cap_result);
            let tool_message = crate::domain::capability::rig_tool_result_to_text(
                &cap_result.to_rig_tool_result(),
            );
            let summary = capability_result_to_summary(&cap_result);

            if entries.is_empty() {
                Some(ToolExecutionResult {
                    tool_name: tc.name.clone(),
                    tool_call_id: tc.id.clone(),
                    arguments: tc.arguments.clone(),
                    tool_message_content: tool_message,
                    entries: vec![],
                    result_summary: "空结果".to_string(),
                    success: true,
                })
            } else {
                Some(ToolExecutionResult {
                    tool_name: tc.name.clone(),
                    tool_call_id: tc.id.clone(),
                    arguments: tc.arguments.clone(),
                    tool_message_content: tool_message,
                    entries,
                    result_summary: summary,
                    success: true,
                })
            }
        }
        Err(CapabilityError::Cancelled) => {
            tracing::trace!(
                target: ai_slo::TARGET,
                capability = %tc.name,
                "Capability cancelled"
            );
            None
        }
        Err(e) => {
            tracing::warn!(
                target: ai_slo::TARGET,
                capability = %tc.name,
                error = %e,
                "Capability invoke 失败"
            );
            let error_msg = format!("能力调用失败: {e}");
            let error_entry = AppEntry {
                name: error_msg.clone(),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.5,
                is_placeholder: false,
                is_error: true,
                source: AI_SOURCE.into(),
                description: None,
                action: Action::default(),
                ..Default::default()
            };
            Some(ToolExecutionResult {
                tool_name: tc.name.clone(),
                tool_call_id: tc.id.clone(),
                arguments: tc.arguments.clone(),
                tool_message_content: format!("错误: {e}"),
                entries: vec![error_entry],
                result_summary: format!("错误: {error_msg}"),
                success: false,
            })
        }
    }
}

/// 执行 Safe Action 并返回 `ToolExecutionResult`。
///
/// **before-seq 校验**（0.11 review W1）：Action 路径含 `open_path` 这类有副作用的动作，
/// 不能像 Capability（只读）那样靠下游 `dispatch_turn1_result` 兜底——执行前先校验 seq，
/// 过期 query 直接放弃执行，避免对已切走的 query 触发副作用（如打开文件）。
async fn execute_action_for_turn1(
    app: &AppHandle,
    seq: u64,
    tc: &crate::domain::ai::message::ToolCall,
    action: &Arc<dyn crate::domain::execution::Action>,
    args: serde_json::Value,
    lang: &str,
    latest_seq: &AtomicU64,
) -> ToolExecutionResult {
    // before-seq 校验：用户已切到新 query → 不执行（副作用动作尤其重要）
    if seq != latest_seq.load(Ordering::SeqCst) {
        tracing::trace!(
            target: ai_slo::TARGET,
            tool = %tc.name,
            "Turn 1 Action 执行前 seq 过期,跳过（避免对过期 query 执行副作用）"
        );
        return ToolExecutionResult {
            tool_name: tc.name.clone(),
            tool_call_id: tc.id.clone(),
            arguments: args,
            tool_message_content: "查询已过期,跳过执行".to_string(),
            entries: vec![],
            result_summary: "查询已过期,跳过执行".to_string(),
            success: false,
        };
    }

    let cx = ActionContext::from_arguments(app, args.clone());
    match action.execute(&cx).await {
        Ok(outcome) => {
            tracing::info!(
                target: ai_slo::TARGET,
                tool_call_id = %tc.id,
                "AI tool_call 执行成功: {} args={}",
                tc.name, args,
            );
            let tool_message =
                crate::domain::capability::rig_tool_result_to_text(&outcome.to_rig_tool_result());
            let summary = outcome_to_summary(&outcome);

            // 构造前端 entries（0.13.7：Items 变体已删，Action 路径统一走 done entry）
            let entries = vec![ai_action_done_entry(action.as_ref(), &outcome, lang)];

            ToolExecutionResult {
                tool_name: tc.name.clone(),
                tool_call_id: tc.id.clone(),
                arguments: args,
                tool_message_content: tool_message,
                entries,
                result_summary: summary,
                success: true,
            }
        }
        Err(e) => {
            tracing::warn!(
                target: ai_slo::TARGET,
                "AI tool_call 执行失败: {} err={}",
                tc.name, e,
            );
            let error_msg = format!("动作执行失败: {e}");
            let error_entry = AppEntry {
                name: error_msg.clone(),
                pinyin_name: String::new(),
                pinyin_full: String::new(),
                lnk_path: String::new(),
                is_calc: false,
                score: 0.5,
                is_placeholder: false,
                is_error: true,
                source: AI_SOURCE.into(),
                description: None,
                action: Action::default(),
                ..Default::default()
            };
            // seq 已在函数入口的 before-seq 校验中消费（review W1）；
            // 此处 action.execute 已发生但失败，构造错误 entry 供下游 emit。
            ToolExecutionResult {
                tool_name: tc.name.clone(),
                tool_call_id: tc.id.clone(),
                arguments: args,
                tool_message_content: format!("错误: {e}"),
                entries: vec![error_entry],
                result_summary: format!("错误: {error_msg}"),
                success: false,
            }
        }
    }
}

/// 分发 Turn 1 结果——回流开启则进 Turn 2，否则直接 emit。
///
/// **回流关闭** (§3.7): 若 `feedback_config == Auto`（用户未显式选择），
/// 工具 items 的 description 追加 `(原始数据,可开启回流获得 AI 总结)`。
async fn dispatch_turn1_result(
    app: &AppHandle,
    seq: u64,
    result: ToolExecutionResult,
    turn2_ctx: Option<&Turn2Context>,
    latest_seq: &AtomicU64,
) {
    tracing::debug!(
        target: ai_slo::TARGET,
        tool = %result.tool_name,
        success = result.success,
        entries_count = result.entries.len(),
        "Turn 1 工具执行完成"
    );

    // 先清流式占位/残留
    emit_ai_clear(app, seq, None);

    // seq 校验——用户可能已切走
    if seq != latest_seq.load(Ordering::SeqCst) {
        tracing::trace!(target: ai_slo::TARGET, "Turn 1 结果分发时 seq 过期,丢弃");
        return;
    }

    let Some(ctx) = turn2_ctx else {
        // 无 Turn 2 上下文 → 直接 emit（0.11.3 之前的现状）
        emit_turn1_result(app, seq, &result);
        return;
    };

    if !ctx.should_run {
        // 回流关闭 → 直接 emit Turn 1 结果
        // §3.7: Auto + 云端时追加提示文案（用户未显式关闭，告知可开启回流）
        if ctx.feedback_config == ToolResultFeedback::Auto {
            emit_turn1_result_with_hint(app, seq, &result);
        } else {
            emit_turn1_result(app, seq, &result);
        }
        return;
    }

    // 回流开启 → 进入 Turn 2
    run_turn2_feedback(app, seq, result, ctx, latest_seq).await;
}

/// 直接 emit Turn 1 结果（现状逻辑）。
fn emit_turn1_result(app: &AppHandle, seq: u64, result: &ToolExecutionResult) {
    if result.entries.is_empty() {
        emit_ai_clear(app, seq, Some("工具返回空结果"));
    } else {
        emit_ai_result_multi(app, seq, result.entries.clone());
    }
}

/// emit Turn 1 结果 + 追加回流提示文案（§3.7）。
fn emit_turn1_result_with_hint(app: &AppHandle, seq: u64, result: &ToolExecutionResult) {
    if result.entries.is_empty() {
        emit_ai_clear(app, seq, Some("工具返回空结果"));
        return;
    }

    let hint_suffix = "(原始数据,可开启回流获得 AI 总结)";
    let entries: Vec<AppEntry> = result
        .entries
        .iter()
        .map(|e| {
            let mut e = e.clone();
            if !e.is_placeholder && !e.is_error {
                e.description = match &e.description {
                    Some(d) => Some(format!("{d} {hint_suffix}")),
                    None => Some(hint_suffix.to_string()),
                };
            }
            e
        })
        .collect();
    emit_ai_result_multi(app, seq, entries);
}

/// 运行 Turn 2 回流（§2.2.1 两轮 complete 协议）。
///
/// 流程:
/// 1. emit 占位 "AI 正在思考…"
/// 2. 构造 Turn 2 messages: [system(feedback_prompt), user, assistant(tool_call_1), tool(result_1)]
/// 3. tools = safe_only（过滤 DangerClass::Safe）
/// 4. 调用 provider.complete() 或 stream
/// 5. 处理三种情况: text / safe tool_call / dangerous tool_call
/// 6. 超时降级: emit "AI 回答较慢,已展示原始结果" + Turn 1 结果
async fn run_turn2_feedback(
    app: &AppHandle,
    seq: u64,
    turn1_result: ToolExecutionResult,
    ctx: &Turn2Context,
    latest_seq: &AtomicU64,
) {
    // §3.4: Turn 1 结果不提前 emit，用户全程看占位文案变化
    emit_ai_result(app, seq, ai_progress_placeholder("AI 正在思考…".into()));

    // seq 校验
    if seq != latest_seq.load(Ordering::SeqCst) {
        tracing::trace!(target: ai_slo::TARGET, "Turn 2 开始前 seq 过期,丢弃");
        return;
    }

    // Turn 2 tools = safe_only（过滤出 DangerClass::Safe 的 tool）
    // §2.2.1: Turn 2 允许 AI 再调一次 Safe tool（如 file_action → open_path），实现 tool chain
    let action_reg = app.state::<Arc<ActionRegistry>>();
    let cap_reg = app.state::<Arc<CapabilityRegistry>>();

    use crate::domain::execution::group;

    // 过滤 safe_tools 的同时收集对应的 safe tool name 集合
    let safe_tool_names: std::collections::HashSet<String> = ctx
        .tools
        .iter()
        .filter(|schema| {
            // Capability 的 tool 默认 Safe（只读数据）
            if cap_reg.get(&schema.name).is_some() {
                return true;
            }

            // 分组 tool（如 file_action / system_action / blink_action）：
            // 检查分组内**所有** action 是否都是 Safe。
            // resolve_tool_call 传空 arguments 无法解析分组（缺 action 字段），
            // 所以对分组 tool 直接遍历其 action_ids 查 danger_class。
            if let Some(g) = group::find_group(&schema.name) {
                return g.action_ids.iter().all(|action_id| {
                    action_reg
                        .get(action_id)
                        .map(|a| a.danger_class() == DangerClass::Safe)
                        .unwrap_or(false)
                });
            }

            // 独立 Action tool（非分组）直接查 danger_class
            if let Some(action) = action_reg.get(&schema.name) {
                return action.danger_class() == DangerClass::Safe;
            }

            false
        })
        .map(|s| s.name.clone())
        .collect();

    let safe_tools: Vec<ActionSchema> = ctx
        .tools
        .iter()
        .filter(|s| safe_tool_names.contains(&s.name))
        .cloned()
        .collect();

    // 0.11.6: Turn 2 system prompt 需要拼接 safe tool 列表，
    // 让 AI 知道有哪些工具可以链式调用（如 open_path 打开搜到的应用）
    let safe_prompt_infos: Vec<crate::domain::ai::prompt::ToolPromptInfo> = ctx
        .prompt_infos
        .iter()
        .filter(|p| safe_tool_names.contains(&p.name))
        .cloned()
        .collect();
    let feedback_prompt =
        crate::domain::ai::prompt::tool_result_feedback_prompt(&safe_prompt_infos, &ctx.lang);

    // 构造 Turn 2 messages
    // §2.2.1: messages = [system(feedback_prompt), user, assistant(tool_call_1), tool(result_1)]
    // 用 serde_json::json! 构造,避免 tool_name 含特殊字符破坏 JSON
    let assistant_content = serde_json::json!({
        "name": turn1_result.tool_name,
        "arguments": turn1_result.arguments,
    })
    .to_string();
    let messages = vec![
        ChatMessage::system(&feedback_prompt),
        ChatMessage::user(&ctx.user_query),
        ChatMessage::assistant_tool_call(&turn1_result.tool_call_id, &assistant_content),
        ChatMessage::tool(
            &turn1_result.tool_call_id,
            &turn1_result.tool_message_content,
        ),
    ];

    // Turn 2 独立超时预算（从总预算派生）
    let turn2_timeout = ctx.deadline.map(|d| {
        let remaining = d.saturating_duration_since(Instant::now());
        // 文档 §2.2.5：至少 TURN2_TIMEOUT_MIN_MS，最多 TURN2_TIMEOUT_MAX_MS
        let ms = remaining.as_millis() as u32;
        ms.clamp(TURN2_TIMEOUT_MIN_MS, TURN2_TIMEOUT_MAX_MS)
    });

    let req = CompletionRequest {
        messages,
        tools: safe_tools.clone(),
        max_tokens: None,
        temperature: Some(0.0),
        timeout_ms: turn2_timeout,
    };

    tracing::debug!(
        target: ai_slo::TARGET,
        provider = ?ctx.provider_kind,
        model = %ctx.provider_model,
        tools = safe_tools.len(),
        timeout_ms = ?turn2_timeout,
        "Turn 2 回流 streaming 发起"
    );

    // §3.4: Turn 2 text 总结走流式输出（与 Turn 1 文本回答体验一致）
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let provider_clone = Arc::clone(&ctx.provider);
    let stream_future = async move { provider_clone.stream(req, tx).await };
    let producer_handle = tauri::async_runtime::spawn(stream_future);

    let turn2_start = Instant::now();
    let mut accumulated = String::new();

    // 逐 chunk 消费
    while let Some(chunk) = rx.recv().await {
        // seq 校验:用户已输入新 query → 丢弃后续 chunk
        if seq != latest_seq.load(Ordering::SeqCst) {
            tracing::trace!(target: ai_slo::TARGET, "Turn 2 stream 过期,丢弃 seq={seq}");
            producer_handle.abort();
            return;
        }

        match chunk {
            StreamChunk::Text(text) => {
                accumulated.push_str(&text);
                emit_ai_stream(app, seq, &text, &accumulated, false);
            }
            StreamChunk::Done { tool_calls, .. } => {
                let elapsed = turn2_start.elapsed().as_millis() as u32;
                tracing::info!(
                    target: ai_slo::TARGET,
                    provider = ?ctx.provider_kind,
                    model = %ctx.provider_model,
                    elapsed_ms = elapsed,
                    text_chars = accumulated.chars().count(),
                    tool_calls = tool_calls.len(),
                    "Turn 2 回流 stream 完成"
                );

                if !tool_calls.is_empty() {
                    // 情况 B/C: tool_call_2 → 先清流式占位,再处理 tool chain
                    emit_ai_stream(app, seq, "", &accumulated, true);
                    let tc2 = &tool_calls[0];
                    handle_turn2_tool_call(app, seq, tc2, ctx, latest_seq).await;
                } else {
                    // 情况 A: text answer → 流式结束,发 done=true + 可复制结果
                    if accumulated.trim().is_empty() {
                        // Turn 2 返回空 → 降级展示 Turn 1 结果
                        tracing::warn!(
                            target: ai_slo::TARGET,
                            "Turn 2 返回空文本,降级展示 Turn 1 结果"
                        );
                        emit_ai_clear(app, seq, None);
                        emit_turn1_result(app, seq, &turn1_result);
                    } else {
                        emit_ai_stream(app, seq, "", &accumulated, true);
                        emit_ai_result(app, seq, ai_result_entry(accumulated));
                    }
                }
                return;
            }
        }
    }

    // channel 关闭但没收到 Done → producer 出错了
    let elapsed = turn2_start.elapsed().as_millis() as u32;
    let producer_result = producer_handle.await;

    // 0.11 review W2: Turn 2 失败降级路径补审计日志（turn=2）。
    // 此前只 Turn 1 自动执行写 turn=1 审计，Turn 2 失败时审计表会显示"工具只调了 1 次"，
    // 对事后排查 tool chain 失败场景不利。这里统一在降级分支返回失败 summary，循环结束后写审计。
    let turn2_outcome_summary: Option<String> = match producer_result {
        Ok(Ok(())) => {
            // 不该走到这里(正常应收到 Done),兜底处理
            tracing::warn!(target: ai_slo::TARGET, "Turn 2 stream 结束但未收到 Done");
            let summary = if accumulated.trim().is_empty() {
                "Turn 2 stream 提前结束（无文本）".to_string()
            } else {
                format!(
                    "Turn 2 stream 提前结束（部分文本: {}chars）",
                    accumulated.chars().count()
                )
            };
            emit_ai_clear(app, seq, None);
            if !accumulated.trim().is_empty() {
                emit_ai_stream(app, seq, "", &accumulated, true);
                emit_ai_result(app, seq, ai_result_entry(accumulated));
            } else {
                emit_turn1_result(app, seq, &turn1_result);
            }
            Some(summary)
        }
        Ok(Err(AIError::Timeout)) => {
            // §3.4 超时降级: 占位变 "AI 回答较慢,已展示原始结果" + Turn 1 结果直接 emit
            tracing::warn!(
                target: ai_slo::TARGET,
                provider = ?ctx.provider_kind,
                elapsed_ms = elapsed,
                "Turn 2 回流超时,降级展示 Turn 1 结果"
            );
            emit_ai_clear(app, seq, None);
            emit_ai_result(
                app,
                seq,
                ai_progress_placeholder("AI 回答较慢,已展示原始结果".into()),
            );
            // 短暂延迟后展示 Turn 1 结果
            tokio::time::sleep(std::time::Duration::from_millis(TURN2_FALLBACK_DELAY_MS)).await;
            emit_turn1_result(app, seq, &turn1_result);
            Some(format!("Turn 2 超时（{elapsed}ms）"))
        }
        Ok(Err(e)) => {
            tracing::warn!(
                target: ai_slo::TARGET,
                provider = ?ctx.provider_kind,
                elapsed_ms = elapsed,
                error = %e,
                "Turn 2 回流失败,降级展示 Turn 1 结果"
            );
            emit_ai_clear(app, seq, None);
            emit_turn1_result(app, seq, &turn1_result);
            Some(format!("Turn 2 失败: {e}"))
        }
        Err(e) => {
            // JoinError（task 被 abort 或 panic）
            tracing::warn!(
                target: ai_slo::TARGET,
                provider = ?ctx.provider_kind,
                elapsed_ms = elapsed,
                error = %e,
                "Turn 2 回流 producer task 异常,降级展示 Turn 1 结果"
            );
            emit_ai_clear(app, seq, None);
            emit_turn1_result(app, seq, &turn1_result);
            Some(format!("Turn 2 producer task 异常: {e}"))
        }
    };

    // 写 Turn 2 失败降级审计（turn=2，summary 记失败原因）
    if let Some(summary) = turn2_outcome_summary {
        write_audit(
            &ctx.pool,
            &turn1_result.tool_name,
            &turn1_result.arguments,
            &summary,
            ctx.provider_kind.as_serde_str(),
            &ctx.provider_model,
            2,
        )
        .await;
    }
}

/// 处理 Turn 2 的 tool_call（情况 B: safe tool chain / 情况 C: dangerous → 确认卡片）。
///
/// §2.2.6 安全边界:
/// - `open_path` 在 Turn 2 保持自动执行（D4）
/// - `open_url` 在 Turn 2 降级为需确认（防止打开恶意网址）
/// - Dangerous tool → emit 确认卡片
async fn handle_turn2_tool_call(
    app: &AppHandle,
    seq: u64,
    tc: &crate::domain::ai::message::ToolCall,
    ctx: &Turn2Context,
    latest_seq: &AtomicU64,
) {
    let cap_reg = app.state::<Arc<CapabilityRegistry>>();

    // §3.2: 更新占位文案为 Turn 2 工具的 progress_hint
    let progress_hint = derive_progress_hint(&tc.name, "", &ctx.progress_hints);
    emit_ai_result(
        app,
        seq,
        ai_progress_placeholder(format!("AI 正在{progress_hint}…")),
    );

    // Capability 优先
    if cap_reg.get(&tc.name).is_some() {
        let result =
            execute_capability_for_turn1(app, seq, tc, &cap_reg, latest_seq, ctx.deadline).await;
        match result {
            Some(r) => {
                // 写审计日志 (turn=2)
                write_audit(
                    &ctx.pool,
                    &tc.name,
                    &tc.arguments,
                    &r.result_summary,
                    ctx.provider_kind.as_serde_str(),
                    &ctx.provider_model,
                    2,
                )
                .await;
                // emit 执行结果（review L4：空 entries 不先 emit clear 再 emit error，
                // 直接根据是否有 entries 选 clear-with-msg 或 result_multi——避免双 emit 抖动）
                if r.entries.is_empty() {
                    emit_ai_clear(app, seq, Some("工具返回空结果"));
                } else {
                    emit_ai_clear(app, seq, None);
                    emit_ai_result_multi(app, seq, r.entries);
                }
            }
            None => {
                // seq 过期或 cancelled
            }
        }
        return;
    }

    // Action 路径
    let action_reg = app.state::<Arc<ActionRegistry>>();
    let resolved = resolve_tool_call(tc, &action_reg);

    match resolved {
        Some((action, args)) => {
            // §2.2.6: open_url 在 Turn 2 降级为需确认
            let is_open_url = action.id() == "open_url";

            match action.danger_class() {
                DangerClass::Safe if !is_open_url => {
                    // 自动执行（D4: open_path 保持自动）
                    let result = execute_action_for_turn1(
                        app, seq, tc, &action, args, &ctx.lang, latest_seq,
                    )
                    .await;

                    // 写审计日志 (turn=2)
                    write_audit(
                        &ctx.pool,
                        &tc.name,
                        &tc.arguments,
                        &result.result_summary,
                        ctx.provider_kind.as_serde_str(),
                        &ctx.provider_model,
                        2,
                    )
                    .await;

                    // §3.6: 自动执行反馈规范
                    emit_ai_clear(app, seq, None);
                    emit_turn2_action_result(app, seq, &result, action.as_ref(), &ctx.lang);
                }
                _ => {
                    // Dangerous 或 open_url → emit 确认卡片
                    tracing::info!(
                        target: ai_slo::TARGET,
                        tool = %tc.name,
                        "Turn 2 tool_call 需确认 (Dangerous 或 open_url 降级)"
                    );
                    let title = action.title().resolve(&ctx.lang).to_string();
                    let display_name = resolve_display_name(tc, &action);
                    emit_ai_confirm(app, seq, &display_name, &args, &title);
                }
            }
        }
        None => {
            // 未知 action → 降级展示 Turn 1 结果
            tracing::warn!(
                target: ai_slo::TARGET,
                tool = %tc.name,
                "Turn 2 tool_call 未知动作,降级"
            );
            emit_ai_clear(app, seq, Some(&format!("AI 调用了未知动作: {}", tc.name)));
        }
    }
}

/// Turn 2 自动执行 safe tool 后的 emit（§3.6 自动执行反馈规范）。
///
/// - 有结果文本: item[0] = 执行结果（如 "已打开 VSCode"），description 告知"AI 自动打开"
/// - 无结果文本: item[0] = "已执行：{action}"
fn emit_turn2_action_result(
    app: &AppHandle,
    seq: u64,
    result: &ToolExecutionResult,
    action: &dyn crate::domain::execution::Action,
    lang: &str,
) {
    let title = action.title().resolve(lang).to_string();

    if result.entries.is_empty() {
        // 无结果 → "已执行"
        let entry = AppEntry {
            name: format!("✓ 已执行：{title}"),
            pinyin_name: String::new(),
            pinyin_full: String::new(),
            lnk_path: String::new(),
            is_calc: false,
            score: 0.7,
            is_placeholder: false,
            is_error: false,
            source: AI_SOURCE.into(),
            description: Some("AI 自动执行 · 如非预期可手动撤销".into()),
            action: Action::default(),
            ..Default::default()
        };
        emit_ai_result(app, seq, entry);
    } else {
        // 有结果 → 第一项加"AI 自动执行"描述
        let mut entries = result.entries.clone();
        if let Some(first) = entries.first_mut() {
            first.description = match &first.description {
                Some(d) => Some(format!("{d} · AI 自动执行")),
                None => Some("AI 自动执行 · 如非预期可手动关闭".into()),
            };
        }
        emit_ai_result_multi(app, seq, entries);
    }
}

/// 构造进度占位项（§3.2 占位文案规范）。
fn ai_progress_placeholder(text: String) -> AppEntry {
    AppEntry {
        name: text,
        pinyin_name: String::new(),
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.5,
        is_placeholder: true,
        is_error: false,
        source: AI_SOURCE.into(),
        description: None,
        action: Action::default(),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::search::engine::SearchAction;

    fn item(id: &str, score: f32, source: &str) -> SearchItem {
        SearchItem {
            id: id.into(),
            title: id.into(),
            subtitle: None,
            score,
            action: SearchAction::Open { path: id.into() },
            source: source.into(),
            score_detail: None,
            context_aware: false,
        }
    }

    #[test]
    fn dedupe_keeps_first_by_id() {
        let items = vec![item("a", 0.9, "start_menu"), item("a", 0.5, "start_menu")];
        let r = fuse_items(items, 10);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].score, 0.9); // 先出现的保留
    }

    // ── AI lane entry 构造 ─────────────────────────────────────────────

    #[test]
    fn ai_placeholder_entry_has_ai_source_and_is_placeholder() {
        let e = ai_placeholder_entry();
        assert_eq!(e.source, AI_SOURCE);
        assert!(e.is_placeholder);
        assert!(!e.name.is_empty(), "占位应显示提示文案");
        assert_eq!(e.action.kind as u8, Action::default().kind as u8);
        // placeholder score 中位:比真结果 0.7 低,比 -2.0 清标记高
        assert!(e.score > -1.0 && e.score < 0.7);
    }

    #[test]
    fn ai_result_entry_uses_copy_kind_with_full_text_payload() {
        use crate::domain::search::ActionKind;
        let text = "Hello, this is an AI answer.".to_string();
        let e = ai_result_entry(text.clone());
        assert_eq!(e.source, AI_SOURCE);
        assert!(!e.is_placeholder);
        assert!(matches!(e.action.kind, ActionKind::Copy));
        assert_eq!(e.action.payload.as_deref(), Some(text.as_str()));
        assert_eq!(e.name, text, "短文本 name 应完整");
    }

    #[test]
    fn ai_result_entry_keeps_full_text_in_name_and_payload() {
        // 0.9.2 §6.4:前端 .ai-item 走多行展开样式,name 不再截断,承担渲染;
        // payload 依旧是完整文本,Copy 动作复制全文。
        let long = "a".repeat(500);
        let e = ai_result_entry(long.clone());
        assert_eq!(e.name, long, "name 应保留完整文本供前端多行渲染");
        assert_eq!(e.action.payload.as_deref(), Some(long.as_str()));
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
        let items = vec![
            item("app", 1.0, "start_menu"),
            item("calc:1+1", 1.0, "calc"),
        ];
        let r = fuse_items(items, 10);
        assert_eq!(r[0].source, "calc");
    }

    #[test]
    fn truncates_to_limit() {
        let items = (0..10)
            .map(|i| item(&format!("e{i}"), 0.5, "start_menu"))
            .collect();
        let r = fuse_items(items, 3);
        assert_eq!(r.len(), 3);
    }

    // ── 0.9.7 Step 4: capability_result_to_entries 前端投影测试 ─────────────

    #[test]
    fn cap_text_projects_to_copy_entry() {
        let r = CapabilityResult::Text {
            content: "hello world".into(),
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert!(!entries[0].is_placeholder);
        assert_eq!(entries[0].source, AI_SOURCE);
        // Text → Copy 动作（复用 ai_result_entry）
        assert!(matches!(entries[0].action.kind, ActionKind::Copy));
    }

    #[test]
    fn cap_items_projects_to_open_entries_with_path() {
        use serde_json::json;
        let r = CapabilityResult::Items {
            items: vec![
                crate::domain::capability::ItemResult {
                    title: "report.pdf".into(),
                    subtitle: Some("C:\\docs".into()),
                    payload: json!({ "path": "C:\\docs\\report.pdf" }),
                    score: Some(0.9),
                },
                crate::domain::capability::ItemResult {
                    title: "notes.txt".into(),
                    subtitle: None,
                    payload: json!({ "path": "D:\\notes.txt" }),
                    score: Some(0.5),
                },
            ],
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 2);
        // 第一项：有 path → Open 动作
        assert_eq!(entries[0].name, "report.pdf");
        assert_eq!(entries[0].lnk_path, "C:\\docs\\report.pdf");
        assert!(matches!(entries[0].action.kind, ActionKind::Open));
        assert_eq!(entries[0].score, 0.9);
        // 第二项
        assert_eq!(entries[1].name, "notes.txt");
        assert_eq!(entries[1].lnk_path, "D:\\notes.txt");
    }

    #[test]
    fn cap_items_empty_returns_empty_vec() {
        let r = CapabilityResult::Items { items: vec![] };
        let entries = capability_result_to_entries(&r);
        assert!(entries.is_empty());
    }

    #[test]
    fn cap_blob_projects_to_summary_entry() {
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x89; 1024], // 1KB
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.contains("image/png"));
        assert!(entries[0].name.contains("KB"));
    }

    #[test]
    fn cap_done_projects_to_summary_entry() {
        let r = CapabilityResult::Done {
            summary: "已写入文本".into(),
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.contains("已写入文本"));
    }

    // ── 边界测试 ──────────────────────────────────────────────────────────

    #[test]
    fn cap_items_without_path_uses_default_action() {
        // Items 的 payload 不含 path → lnk_path 空、action 走 Default（Open kind）
        use serde_json::json;
        let r = CapabilityResult::Items {
            items: vec![crate::domain::capability::ItemResult {
                title: "进程信息".into(),
                subtitle: Some("PID: 1234".into()),
                payload: json!({ "pid": 1234 }), // 无 path 字段
                score: None,
            }],
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].lnk_path, ""); // 无 path → 空 lnk_path
        assert!(entries[0].action.payload.is_none()); // 无 payload
        // Action::default() kind = Open，但 lnk_path 空 → 前端 open 空路径走 no-op
        assert!(matches!(entries[0].action.kind, ActionKind::Open));
    }

    #[test]
    fn cap_blob_large_size_shows_mb() {
        // Blob > 1MB → 名称含 "MB" 而非 "KB"
        let r = CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: vec![0x00; 2 * 1024 * 1024], // 2MB
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].name.contains("MB"));
        assert!(!entries[0].name.contains("KB"));
    }

    #[test]
    fn cap_items_none_subtitle_yields_none_description() {
        // subtitle = None → AppEntry.description = None
        use serde_json::json;
        let r = CapabilityResult::Items {
            items: vec![crate::domain::capability::ItemResult {
                title: "file.txt".into(),
                subtitle: None,
                payload: json!({ "path": "C:\\file.txt" }),
                score: Some(0.5),
            }],
        };
        let entries = capability_result_to_entries(&r);
        assert_eq!(entries.len(), 1);
        assert!(entries[0].description.is_none());
    }
}
