//! SearchService:多路搜索的路由 + 融合 + 渐进式调度(见 0.2 设计 §2.3 / §2.5)。
//!
//! 0.4 改造:search() 开头调用 `IntentRouter::route()` 决定呈现策略(Takeover/Mixed)。
//! - Takeover:跳过本地引擎,只查命中插件,独占返回区。
//! - Mixed:本地引擎(sync lane)照常召回;命中插件按 surface(Priority/Inline)参与排序。
//!
//! 由 `commands::search_apps` 经 `app.state::<Arc<SearchService>>()` 调用（0.14.6 后通过 DomainEnv）。

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Instant;

use crate::domain::event::EventPort;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::domain::ai::registry::AIProviderRegistry;
use crate::domain::event_names::EventNames;
use crate::domain::intent::suggestion::coordinator::{
    CoordinateInput, SuggestionCoordinator, SuggestionRuntimeConfig,
};
use crate::domain::intent::suggestion::fatigue::SuggestionFatigue;
use crate::domain::intent::{
    Candidate, IntentRouter, RankingHint, Route, RouteSummary, SuggestionAvailability,
    SuggestionKind, SuggestionSet, SuggestionSource, Surface,
};
use crate::domain::plugin::PluginEngine;
use crate::infra::platform::context::ContextSnapshot;
use crate::infra::utils::perf::ai_slo;

use super::AppEntry;
use super::engine::{Lane, QueryContext, SearchEngine, SearchItem};
use super::scorer::{boost_priority, placeholder_score, source_rank};

// 0.17.6: AI lane 常量已随 SearchService AI 路径删除（AI_DEFAULT_HARD_TIMEOUT_MS /
// TURN2_TIMEOUT_MIN_MS / TURN2_TIMEOUT_MAX_MS / TURN2_FALLBACK_DELAY_MS）。
// 主窗口 AI 改走 ChatService，超时由 AIConfig::slo_hard_timeout_ms 管理。

/// 近期建议环形记录容量（0.24.3 §3.6 遥测关联用，有界防膨胀）。
const RECENT_SUGGESTIONS_CAP: usize = 32;

/// 建议槽位（遥测记录用；对应前端 §3.6 上报的 slot 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SuggestionSlot {
    Primary,
    Secondary,
}

impl SuggestionSlot {
    fn as_str(&self) -> &'static str {
        match self {
            SuggestionSlot::Primary => "primary",
            SuggestionSlot::Secondary => "secondary",
        }
    }
}

/// 遥测关联记录：一次搜索产出的一个槽位候选（不存原文）。
#[derive(Debug, Clone)]
struct RecentSuggestion {
    revision: u64,
    id: String,
    kind: SuggestionKind,
    producer: SuggestionSource,
    slot: SuggestionSlot,
}

/// 同步搜索返回契约（0.8.3 §4.3；0.24.1 suggestion 迁移 SuggestionSet）——
/// `SearchService::search` / `search_apps` command 出口。
///
/// 在 `Vec<AppEntry>` 之外挂 `suggestion` 独立通道（0.24.1 起为带 revision 的
/// `SuggestionSet` 双槽位集合）：
/// - Completion 类：首拼命中（`fy` → `fanyi`）或 fuzzy 部分拼音（`fan hello` → `fanyi hello`）
/// - Translate 类：空 query + 选中英文 → 翻译建议（Tab 采纳）
/// - AskAi 类：gating 过筛且无其他命中 → "按 Tab 问 AI"
///
/// 用户按 Tab/Shift+Tab 后前端执行 `suggestion.primary/secondary.action`
/// （0.24 §3.5：RouteQuery=改 query 重搜 / EnterAiMode=进 AI 模式）。
/// `revision` = 本次 search seq，前端与本地 seq 比对拒绝过期采纳（§3.6）。
/// `blink://results` 增量事件不带 suggestion（同步首次返回已给过）。
///
/// **契约说明**：这是内部 API（前后端锁版本，同版本编译）。`entries` 必填；rename 会导致
/// 前端 crash。序列化为 camelCase：`suggestion` 直接 `suggestion`，`SuggestionKind` 序列化为
/// camelCase 字符串（`completion` / `translate` / `askAi`）。
///
/// **兼容说明**：0.8.1 的 `completion_hint` 字段已删除；0.8.2 的 `fetchContextSuggestions` 前端通道
/// 也已废弃（见 §4.13 P0-1）——空 query 现在也走 search 接口拿 suggestion。
#[derive(Serialize, Clone)]
#[serde(rename_all = "camelCase")]
pub struct SearchResponse {
    pub entries: Vec<AppEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<SuggestionSet>,
}

/// async lane 增量结果的事件 payload(emit "blink://results")。
#[derive(Serialize, Clone)]
struct ResultsPayload {
    seq: u64,
    items: Vec<AppEntry>,
}

/// 引擎配置更新枚举（用于运行时热更新）。
pub enum EngineConfigUpdate {
    StartMenu(crate::domain::config::StartMenuConfig),
    Calc(crate::domain::config::CalcConfig),
    File(crate::domain::config::FileSearchConfig),
}

pub struct SearchService {
    env: Arc<dyn EventPort>,
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
    /// 是否允许从主搜索框通过 `clip/剪贴板` 召回历史。Alt+C 专用模式不受此开关影响。
    clipboard_search_enabled: Arc<AtomicBool>,
    /// 用户禁用的内置动作 id 列表（0.8.0 §1.3）。
    /// BuiltinEngine 通过 QueryContext 只读，设置页保存时经 `update_disabled_builtin_actions`
    /// 热更新。读多写少，用 RwLock；每次 search 短时 read 不阻塞。
    disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
    /// 用户禁用的 context binding key 列表（`{target_id}::{trigger_key}` 格式，0.8.3 §4.6）。
    /// 0.11.8 起同时被 `RuleRouter`（manifest context）和 `BuiltinEngine`（内置动作 context）
    /// 消费——前者在 `apply_context_disable_list` 里独立持有副本，后者经 QueryContext 读。
    /// 读多写少，用 RwLock；每次 search 短时 read 不阻塞。
    disabled_context_bindings: Arc<RwLock<Vec<String>>>,
    /// Suggestion 运行时配置快照（0.24.2：autosuggest 开关/阈值 + secondary 门槛）。
    /// 与 `KeywordProducer` 共享同一 cell——`update_suggestion_config` 热更新时
    /// producer 侧同步生效（0.8.6 §8.1.2 的 min_score 共享机制扩展）。
    suggestion_runtime: Arc<RwLock<SuggestionRuntimeConfig>>,
    /// Suggestion 协调器（0.24.2：实例从 RuleRouter 迁入，编排层持有）。
    /// producers 在 main.rs wiring 时注册（Keyword/Context/Ai 三源）。
    suggestion_coordinator: SuggestionCoordinator,
    /// 界面语言快照（0.8.1）。用于把 `empty_arg_hint` / 未来其他 `LocalizableText`
    /// 解析成当前语言字符串。热更新走 `update_language`，与 AppConfig.language 同步。
    language: Arc<RwLock<String>>,
    /// 上一轮 coordinate 产出的 RankingHint 快照（0.8.4 §5.3.1 Surface Booster）。
    /// route() 下一轮读此值做 surface boost——跨轮反馈滞后一轮,0.8.4 同步阶段可接受
    /// （0.9 AI 异步化后失效,见 0.8 文档 §5.6）。
    last_ranking_hint: Arc<Mutex<Option<RankingHint>>>,
    /// 会话级建议降频计数器（0.24.3 §3.8）——任一采纳/主窗口隐藏清零
    /// （`record_adoption` / `reset_suggestion_session`）。
    suggestion_fatigue: Arc<Mutex<SuggestionFatigue>>,
    /// 近期建议环形记录（0.24.3 §3.6 遥测关联）：采纳上报的 revision → (id, kind, slot)。
    /// 有界（32 条），只存 id/kind 不存原文。
    recent_suggestions: Mutex<std::collections::VecDeque<RecentSuggestion>>,
    /// AI Provider registry cell（0.9.2 Phase 5b setter 注入；0.24.2 起与
    /// `AiProducer` 共享同一 cell——availability 判定与 produce 分类读同一份）。
    ///
    /// **为什么用 setter 注入而不是 `new` 参数**:`main.rs` 先建 search_service、
    /// 后建 ai_registry —— 构造顺序倒挂,setter 规避不动 wiring 顺序。
    /// setup 早期(setter 未调)读到 None → AskAi 候选被 availability 排除,无害。
    ai_registry: Arc<RwLock<Option<Arc<AIProviderRegistry>>>>,
}

impl SearchService {
    /// wiring 构造（参数即依赖装配清单，main.rs 唯一调用点——打包成 struct 反而
    /// 隐藏依赖关系，8 参对 wiring 构造可接受）。
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        env: Arc<dyn EventPort>,
        pool: SqlitePool,
        engines: Vec<Arc<dyn SearchEngine>>,
        plugin_engine: Arc<PluginEngine>,
        router: Arc<dyn IntentRouter>,
        suggestion_runtime: Arc<RwLock<SuggestionRuntimeConfig>>,
        suggestion_coordinator: SuggestionCoordinator,
        ai_registry: Arc<RwLock<Option<Arc<AIProviderRegistry>>>>,
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
            env,
            pool,
            sync_engines,
            async_engines,
            plugin_engine,
            router,
            latest_seq: Arc::new(AtomicU64::new(0)),
            snapshot: Arc::new(RwLock::new(ContextSnapshot::default())),
            max_results: Arc::new(AtomicUsize::new(50)),
            clipboard_search_enabled: Arc::new(AtomicBool::new(true)),
            disabled_builtin_actions: Arc::new(RwLock::new(Vec::new())),
            disabled_context_bindings: Arc::new(RwLock::new(Vec::new())),
            suggestion_runtime,
            suggestion_coordinator,
            language: Arc::new(RwLock::new("zh".to_string())),
            last_ranking_hint: Arc::new(Mutex::new(None)),
            suggestion_fatigue: Arc::new(Mutex::new(SuggestionFatigue::new())),
            recent_suggestions: Mutex::new(std::collections::VecDeque::new()),
            ai_registry,
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

    /// 读取当前 awareness 快照中的选区文本（0.16.9）。
    ///
    /// 供 chord E/S 在空闲态（空 query、无结果）解析上下文用。
    /// 返回 trim 后的选区文本，无选区时返回 None。
    pub fn get_selection_text(&self) -> Option<String> {
        use crate::infra::platform::context::AwarenessSource;
        let guard = self.snapshot.read().unwrap();
        guard
            .find_text(AwarenessSource::Selection)
            .map(|v| v.text.to_string())
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

    /// 更新 Suggestion 运行时配置（0.8.1 §2.5 建立，0.24.2 扩展 secondary 门槛）。
    /// 启动时读一次 SuggestionConfig 注入；设置页开关/滑块调整时命令层调此方法。
    /// 写共享 cell——`KeywordProducer` 侧同步生效。
    pub fn update_suggestion_config(&self, cfg: SuggestionRuntimeConfig) {
        *self.suggestion_runtime.write().unwrap() = cfg;
        tracing::debug!(?cfg, "Suggestion 运行时配置已热更新");
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

    /// 更新 ClipboardEngine 的加载页数（设置页 `clipboard_config` 保存时转发）。
    /// downcast 模式同 `update_language`。
    pub fn update_clipboard_display_pages(&self, pages: u32) {
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_display_pages(pages);
            }
        }
        tracing::debug!(pages, "ClipboardEngine 加载页数已热更新");
    }

    /// 更新 ClipboardEngine 的每页条数（SearchConfig::page_size 变化时转发）。
    /// effective_limit = display_pages × page_size。
    pub fn update_clipboard_page_size(&self, page_size: u32) {
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_page_size(page_size);
            }
        }
        tracing::debug!(page_size, "ClipboardEngine 每页条数已热更新");
    }

    /// 更新 ClipboardEngine 的搜索候选池上限（设置页 `clipboard_config` 保存时转发）。
    pub fn update_clipboard_candidate_limit(&self, limit: u32) {
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_candidate_limit(limit);
            }
        }
        tracing::debug!(limit, "ClipboardEngine 候选池上限已热更新");
    }

    /// 热更新主搜索框的剪贴板历史召回门禁。Alt+C 仍可显式打开历史。
    pub fn update_clipboard_search_enabled(&self, enabled: bool) {
        self.clipboard_search_enabled
            .store(enabled, Ordering::Relaxed);
        tracing::debug!(enabled, "ClipboardEngine 主搜索召回开关已热更新");
    }

    /// 更新剪贴板搜索保留期；0 表示搜索全部历史。
    pub fn update_clipboard_retention_days(&self, days: u32) {
        for engine in &self.sync_engines {
            if let Some(clip) = engine
                .as_any()
                .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
            {
                clip.update_retention_days(days);
            }
        }
        tracing::debug!(days, "ClipboardEngine 搜索保留期已热更新");
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
                    language: "zh",
                };
                let items = engine.search(query, &search_ctx).await;
                return items.into_iter().take(max_results).collect();
            }
        }
        Vec::new()
    }

    /// 剪贴板模式直接搜索（bypass SearchService pipeline）。
    ///
    /// Alt+C 进入剪贴板模式后，前端直接调此方法，不经过 route / get_weights / Mixed 分派。
    /// 只走 ClipboardEngine，返回已转换的 `AppEntry` 列表。
    ///
    /// 0.20.1: 不使用 `max_results` 截断——ClipboardEngine 内部已按
    /// `effective_limit = display_pages × page_size` 截断，普通搜索的 `max_results`
    /// 不再干扰剪贴板模式的结果上限。
    pub async fn search_clipboard_mode(&self, query: &str) -> Vec<AppEntry> {
        let arg = query.trim();
        let t0 = std::time::Instant::now();

        for engine in &self.sync_engines {
            if engine.id() == "clipboard" {
                let history = std::collections::HashMap::new();
                let snapshot = ContextSnapshot::default();
                let disabled: Vec<String> = Vec::new();
                let disabled_ctx: Vec<String> = Vec::new();
                let language = self.language.read().unwrap().clone();
                let search_ctx = QueryContext {
                    history: &history,
                    snapshot: &snapshot,
                    disabled_builtin_actions: &disabled,
                    disabled_context_bindings: &disabled_ctx,
                    language: &language,
                };
                let items = engine.search(arg, &search_ctx).await;
                let t1 = std::time::Instant::now();
                // 0.20.1: 不使用 max_results 截断，engine 内部已截断
                let entries: Vec<AppEntry> =
                    items.into_iter().map(SearchItem::into_app_entry).collect();
                tracing::trace!(
                    query = %arg,
                    count = entries.len(),
                    engine_ms = t0.elapsed().as_millis() as u64,
                    fuse_ms = t1.elapsed().as_millis() as u64,
                    "search_clipboard_mode: 完成"
                );
                return entries;
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

        // 路由决策——keyword 检测不需要 history 权重，先用空表路由。
        // 这样 EngineTakeover（如"剪贴板"→ClipboardEngine）可以跳过 get_weights 全表扫描。
        let ranking_hint = self.last_ranking_hint.lock().unwrap().clone();
        let empty_history = std::collections::HashMap::new();
        let route = self
            .router
            .route(q, &empty_history, ranking_hint.as_ref())
            .await;
        let route = self.filter_route(route);

        // 只在 Mixed 路径加载 history——sync 引擎（StartMenuEngine 等）用 history 权重
        // 做频率加权排序。EngineTakeover / AiTrigger / 空 query 的引擎都不消费 history。
        let history = if q.is_empty()
            || matches!(
                route,
                Route::EngineTakeover { .. } | Route::AiTrigger { .. }
            ) {
            empty_history
        } else {
            crate::infra::data::history::get_weights(&self.pool).await
        };
        let snapshot = self.snapshot.read().unwrap().clone();
        let disabled = self.disabled_builtin_actions.read().unwrap().clone();
        let disabled_ctx = self.disabled_context_bindings.read().unwrap().clone();
        let language = self.language.read().unwrap().clone();
        let search_ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
            disabled_context_bindings: &disabled_ctx,
            language: &language,
        };

        // Suggestion（0.24.2 §3.7：编排顺序即防环——Route 先算、Coordinator 后算，
        // route_summary 是编排层传值不是域依赖）。
        // AiTrigger 路由的 ai-trigger 候选也由 Coordinator 从 route_summary 构造，
        // SearchService 不再有独立 AI 覆盖分支（0.9.2 Phase 5b 后置注入已删）。
        let suggestion = self.compute_suggestion(query, &snapshot, &route, seq);

        // 按 Route 分派到四个 executor（0.8.6 §8.2.1 拆 God Method）
        let entries = match route {
            Route::Takeover { plugin_id, arg, .. } => self.exec_takeover(plugin_id, arg, seq),
            Route::EngineTakeover { engine_id, arg } => {
                self.exec_engine_takeover(engine_id, arg, &search_ctx).await
            }
            // AI 前缀触发：只产 Suggestion，让前端 Ghost + Tab 触发。
            // 不直接调用 AI——用户输入过程中不应立即消耗 token，
            // 需要用户按 Tab 显式确认后才真正调用 AI（0.17.6 后走 chat_prompt ephemeral）。
            Route::AiTrigger { .. } => vec![],
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

    /// Suggestion 协调（0.8.6 §8.2.1 提取；0.24.2 重写为 Coordinator 唯一真源）。
    ///
    /// 组装 `CoordinateInput`（route_summary / availability / runtime config）调用
    /// `SuggestionCoordinator::coordinate`，回填 revision 组装 `SuggestionSet`。
    /// RankingHint 由 coordinate 返回值直接写入 `last_ranking_hint`
    /// （下一轮 route 的 Surface Booster，0.8.4 §5.3.1）。
    fn compute_suggestion(
        &self,
        query: &str,
        snapshot: &ContextSnapshot,
        route: &Route,
        seq: u64,
    ) -> Option<SuggestionSet> {
        let runtime = *self.suggestion_runtime.read().unwrap();
        let input = CoordinateInput {
            query,
            snapshot,
            route_summary: RouteSummary::from(route),
            availability: SuggestionAvailability {
                ai_available: self.ai_available(),
            },
            config: runtime,
        };
        let mut fatigue = self.suggestion_fatigue.lock().unwrap();
        let (outcome, hint) = self.suggestion_coordinator.coordinate(&input, &mut fatigue);
        drop(fatigue);
        *self.last_ranking_hint.lock().unwrap() = hint;
        if outcome.primary.is_none() && outcome.secondary.is_none() {
            return None;
        }
        // 遥测关联记录（§3.6）：revision → 槽位候选的有界环形表
        {
            let mut ring = self.recent_suggestions.lock().unwrap();
            if let Some(sug) = &outcome.primary {
                ring.push_back(RecentSuggestion {
                    revision: seq,
                    id: sug.id.clone(),
                    kind: sug.kind,
                    producer: sug.source,
                    slot: SuggestionSlot::Primary,
                });
            }
            if let Some(sug) = &outcome.secondary {
                ring.push_back(RecentSuggestion {
                    revision: seq,
                    id: sug.id.clone(),
                    kind: sug.kind,
                    producer: sug.source,
                    slot: SuggestionSlot::Secondary,
                });
            }
            while ring.len() > RECENT_SUGGESTIONS_CAP {
                ring.pop_front();
            }
        }
        Some(SuggestionSet {
            revision: seq,
            primary: outcome.primary,
            secondary: outcome.secondary,
        })
    }

    /// 采纳上报（0.24.3 §3.6 单向遥测）：fire-and-forget，best-effort 记日志。
    ///
    /// - `matched`：revision + slot + id 与近期环形记录吻合（前端 seq 校验通过时
    ///   通常为 true；false = 过期上报/跨会话残留，只记不纠错——0.24 采纳零执行，
    ///   无需服务端强校验）。
    /// - 任一次采纳 → 降频计数全量清零（§3.8）。
    /// - **不记 query/选区原文**，只有 producer/kind/slot/revision 匹配与否。
    pub fn record_adoption(&self, id: &str, slot: &str, revision: u64) {
        let entry = {
            let ring = self.recent_suggestions.lock().unwrap();
            ring.iter()
                .find(|r| r.revision == revision && r.slot.as_str() == slot && r.id == id)
                .map(|r| (r.producer, r.kind))
        };
        let matched = entry.is_some();
        // 未匹配（过期上报/跨会话残留）时 producer/kind 如实记 None，不猜
        let producer = entry.map(|(p, _)| p);
        let kind = entry.map(|(_, k)| k);
        let latest = self.latest_seq.load(Ordering::SeqCst);
        self.suggestion_fatigue.lock().unwrap().clear();
        tracing::info!(
            id = %id,
            producer = ?producer,
            kind = ?kind,
            slot = %slot,
            revision,
            is_latest_revision = revision == latest,
            matched,
            "suggestion adopted"
        );
    }

    /// 建议会话重置（0.24.3 §3.8）：主窗口隐藏时清零降频计数 + 遥测环形表
    /// （每次唤起是新会话）。前端 lifecycle HIDDEN 钩子触发。
    pub fn reset_suggestion_session(&self) {
        self.suggestion_fatigue.lock().unwrap().clear();
        self.recent_suggestions.lock().unwrap().clear();
        tracing::debug!("suggestion session 已重置（窗口隐藏）");
    }

    /// AI 可用性（0.24 §3.7 availability 表）：registry 已注入且 `AIConfig.enabled`。
    fn ai_available(&self) -> bool {
        let reg = self
            .ai_registry
            .read()
            .expect("ai_registry lock poisoned")
            .clone();
        reg.is_some_and(|r| r.config_snapshot().enabled)
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
        if engine_id == super::clipboard_engine::ENGINE_ID
            && !self.clipboard_search_enabled.load(Ordering::Relaxed)
        {
            tracing::debug!("ClipboardEngine 主搜索召回已关闭");
            return Vec::new();
        }
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

        // AI lane 触发不在这里判——AI 走 Tab 显式触发：AskAi 候选由 AiProducer
        // 产出、Coordinator 仲裁进 SearchResponse.suggestion（0.24.2 收敛）。

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

        // ── AI Ghost 由 Coordinator 产出（0.24.2：AiProducer → coordinate → SuggestionSet）──
        // 不在 exec_mixed 自动 spawn AI:边打字连续 spawn 会浪费 token + h2 stream 堆积。
        // 用户看到 Ghost "按 Tab 问 AI" 后显式按 Tab → 前端 invoke `chat_prompt` command
        // → ChatService::prompt(ephemeral) → 单次 spawn。

        let elapsed = search_start.elapsed().as_secs_f64() * 1000.0;
        crate::infra::utils::perf::record(
            crate::infra::utils::perf::MetricCategory::SearchEngine,
            "total",
            elapsed,
            None,
        );

        all_items
    }

    /// 过滤不满足前置条件的路由命中(0.5.1):禁用插件 + 参数过短。
    /// - Takeover 命中禁用/短参 → 降级空 Mixed(走 Generic 应用搜索),避免窗口空白。
    /// - Mixed 候选 → 剔除禁用/短参插件。
    ///
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
            // 本体 engine 由本体决定生死；ClipboardEngine 的主搜索召回开关在下方单独门禁。
            // 带参 engine 不设参数长度门槛
            // （剪贴板搜索 "a" 一个字符也合理，engine 自决定 fuzzy 阈值）。
            Route::EngineTakeover { ref engine_id, .. }
                if engine_id == super::clipboard_engine::ENGINE_ID
                    && !self.clipboard_search_enabled.load(Ordering::Relaxed) =>
            {
                tracing::debug!("ClipboardEngine 主搜索召回已关闭,降级普通搜索");
                Route::Mixed { candidates: vec![] }
            }
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
        let app = self.env.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let snapshot = Arc::clone(&self.snapshot);
        let max_results = Arc::clone(&self.max_results);
        tokio::spawn(async move {
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
            emit_results(app.as_ref(), seq, items, limit, Some(&plugin_id));
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
        let app = self.env.clone();
        let pool = self.pool.clone();
        let latest_seq = Arc::clone(&self.latest_seq);
        let snapshot = Arc::clone(&self.snapshot);
        let max_results = Arc::clone(&self.max_results);

        tokio::spawn(async move {
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
                tokio::spawn(async move {
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
                        emit_results(app.as_ref(), seq, items, limit, empty_source);
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
                tokio::spawn(async move {
                    // async lane 引擎（file/mock）不消费 disabled_builtin_actions /
                    // disabled_context_bindings；这两个字段仅 BuiltinEngine（sync lane）读，
                    // 此处传空 slice 满足契约。
                    let ctx = QueryContext {
                        history: &history,
                        snapshot: &snapshot,
                        disabled_builtin_actions: &[],
                        disabled_context_bindings: &[],
                        language: "zh",
                    };
                    let items = engine.search(&q, &ctx).await;
                    if seq == latest_seq.load(Ordering::SeqCst) && !items.is_empty() {
                        tracing::trace!(
                            engine = engine.id(),
                            count = items.len(),
                            "async lane 引擎返回"
                        );
                        emit_results(app.as_ref(), seq, items, limit, None);
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
    env: &dyn EventPort,
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
            actions: vec![],
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
    if let Err(e) = crate::domain::event::emit_serialized(
        env,
        EventNames::RESULTS,
        &ResultsPayload {
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
        actions: vec![],
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
        actions: vec![],
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
            color_list_hex: None,
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
}
