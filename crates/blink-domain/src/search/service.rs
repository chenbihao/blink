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

use crate::event::EventPort;
use serde::Serialize;
use sqlx::SqlitePool;

use crate::ai::registry::AIProviderRegistry;
use crate::event_names::EventNames;
use crate::intent::suggestion::coordinator::{
    CoordinateInput, SuggestionCoordinator, SuggestionRuntimeConfig,
};
use crate::intent::suggestion::fatigue::SuggestionFatigue;
use crate::intent::{
    Candidate, IntentRouter, RankingHint, Route, RouteSummary, SuggestionAvailability,
    SuggestionKind, SuggestionSet, SuggestionSource, Surface,
};
use crate::plugin::PluginEngine;
use blink_infra::platform::context::ContextSnapshot;
use blink_infra::utils::perf::ai_slo;

use super::AppEntry;
use super::engine::{Lane, QueryContext, SearchEngine, SearchItem};
use super::scorer::{boost_priority, placeholder_score, source_rank};
use super::{Action, ActionKind};

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

/// 文件搜索数据源统一状态（设置页一次拉取：用户实例 + 托管实例）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct FileSearchStatus {
    /// 托管实例快照（installed/running/port/version/lastError/restartBackoffMs）。
    #[serde(flatten)]
    pub managed: super::everything_runtime::RuntimeSnapshot,
    /// 用户 Everything 的 HTTP 端口（当前配置）。
    pub user_port: u16,
    /// 用户实例可用性：None = 未探测；Some(true/false) = 最近探测结果
    /// （live 探测由 `probe_everything` 显式触发，此处只读引擎内存态）。
    pub user_available: Option<bool>,
    pub user_probing: bool,
    pub data_source: String,
    pub active_source: String,
}

/// 引擎配置更新枚举（用于运行时热更新）。
pub enum EngineConfigUpdate {
    StartMenu(crate::config::StartMenuConfig),
    Calc(crate::config::CalcConfig),
    File(crate::config::FileSearchConfig),
    /// 0.26.12：快捷超链。engine 配置更新只覆盖内存快照；
    /// keyword 规则重注册由 app 层另行调用 `register_weblinks_rules`。
    WebLinks(crate::config::WebLinksConfig),
    /// 0.26.13：浏览器书签。keyword 规则重注册由 app 层另行调用
    /// `register_bookmark_rules`；扫描相关字段变化由 engine 自行强制重扫。
    Bookmarks(crate::config::BookmarksConfig),
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
    /// 快照代次（0.27.3）——`update_snapshot` 时递增。show 后的异步回写
    /// （UIA 选区 / Shell 文件选中）携带冻结时的代次，回写前核对，
    /// 唤起 A→关闭→B 场景旧结果不覆盖新快照。
    snapshot_generation: Arc<AtomicU64>,
    /// 融合后返回前端的最大结果数（AppConfig.max_results 热更新，搜索热路径零 IO）。
    max_results: Arc<AtomicUsize>,
    /// 是否允许从主搜索框通过 `clip/剪贴板` 召回历史。Alt+C 专用模式不受此开关影响。
    clipboard_search_enabled: Arc<AtomicBool>,
    /// 用户禁用的内置动作 id 列表（0.8.0 §1.3）。
    /// BuiltinEngine 通过 QueryContext 只读，设置页保存时经 `update_disabled_builtin_actions`
    /// 热更新。0.24.8 起与 `ContextProducer` 共享同一 cell（打开类建议的禁用一致性：
    /// result 侧与建议侧同源判定）。读多写少，用 RwLock；每次 search 短时 read 不阻塞。
    disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
    /// 用户禁用的 context binding key 列表（`{target_id}::{trigger_key}` 格式，0.8.3 §4.6）。
    /// 0.11.8 起同时被 `RuleRouter`（manifest context）和 `BuiltinEngine`（内置动作 context）
    /// 消费——前者在 `apply_context_disable_list` 里独立持有副本，后者经 QueryContext 读；
    /// 0.24.8 起 `ContextProducer` 共享本 cell（awareness 派生打开类建议的 binding 粒度闸）。
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
        disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
        disabled_context_bindings: Arc<RwLock<Vec<String>>>,
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
            snapshot_generation: Arc::new(AtomicU64::new(0)),
            max_results: Arc::new(AtomicUsize::new(50)),
            clipboard_search_enabled: Arc::new(AtomicBool::new(true)),
            disabled_builtin_actions,
            disabled_context_bindings,
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
    ///
    /// 0.27.3：写入即递增快照代次（**在写锁内 bump**，与快照写入原子化）——
    /// show 后异步回写（UIA 选区 / Shell 文件选中）持有冻结代次，回写前在
    /// 持锁后经 `*_if_fresh` 复核，旧代次结果整体丢弃，且「检查后拿锁前」
    /// 窗口内插入的新 invoke 不会漏判（bump 与写入同锁序）。
    pub fn update_snapshot(&self, snapshot: ContextSnapshot) {
        let mut guard = self.snapshot.write().unwrap();
        *guard = snapshot;
        self.snapshot_generation.fetch_add(1, Ordering::SeqCst);
        drop(guard);
    }

    /// 当前快照代次（invoke 编排冻结异步回写凭证用，0.27.3）。
    pub fn current_snapshot_generation(&self) -> u64 {
        self.snapshot_generation.load(Ordering::SeqCst)
    }

    /// 读取当前 awareness 快照中的选区文本（0.16.9）。
    ///
    /// 供 chord E/S 在空闲态（空 query、无结果）解析上下文用。
    /// 返回 trim 后的选区文本，无选区时返回 None。
    pub fn get_selection_text(&self) -> Option<String> {
        use blink_infra::platform::context::AwarenessSource;
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
    ///
    /// 0.27.3：带代次核验版本——`generation` 为冻结时 `current_snapshot_generation()`，
    /// 不匹配（期间发生过新 invoke / 快照重写）则丢弃并返回 false。复核在持写锁后
    /// 进行（bump 与写入同锁序，无「检查后拿锁前」竞态窗口）。
    pub fn update_selected_text_if_fresh(
        &self,
        generation: u64,
        text: Option<String>,
        captured_at: Option<Instant>,
    ) -> bool {
        use blink_infra::platform::context::AwarenessSource;
        let mut guard = self.snapshot.write().unwrap();
        if self.current_snapshot_generation() != generation {
            tracing::debug!(
                generation,
                current = self.current_snapshot_generation(),
                "选区回写被代次保护丢弃"
            );
            return false;
        }
        guard.upsert_text_with_time(AwarenessSource::Selection, text, captured_at);
        true
    }

    /// 写回文件选中状态（0.27.3 Shell COM worker 回填，带代次核验）。
    ///
    /// `None` 显式清空本代文件选中（开关翻转/来源失效）；代次不匹配返回 false
    /// 且不写。调用方（window_orchestrator）已完成来源 PID/开关/敏感门控复核。
    pub fn update_file_selection_if_fresh(
        &self,
        generation: u64,
        state: Option<blink_infra::platform::selection::files::FileSelectionState>,
    ) -> bool {
        let mut guard = self.snapshot.write().unwrap();
        if self.current_snapshot_generation() != generation {
            tracing::debug!(
                generation,
                current = self.current_snapshot_generation(),
                "文件选中回写被代次保护丢弃"
            );
            return false;
        }
        guard.file_selection = state;
        true
    }

    /// 读取当前文件选中项的文件路径（0.27.9——附件签发的「选中项确认」
    /// 可信来源核验：只有出现在当前快照中的路径才允许签发附件 ref）。
    ///
    /// 只投影普通文件（目录不进——§3.2 目录仅参与本地动作）。
    pub fn current_file_selection_paths(&self) -> Vec<std::path::PathBuf> {
        let guard = self.snapshot.read().unwrap();
        guard
            .file_selection
            .as_ref()
            .map(|sel| {
                sel.items
                    .iter()
                    .filter(|item| !item.is_dir)
                    .map(|item| std::path::PathBuf::from(&item.path))
                    .collect()
            })
            .unwrap_or_default()
    }

    /// 写回剪贴板文本（clipboard listener 检测到剪贴板变化时调用）。
    ///
    /// **动机**：`update_snapshot` 只在热键 invoke 时执行一次;主窗口保持打开时
    /// 用户复制/剪切新内容,snapshot 里的 Clipboard 项会陈旧,导致 Context ghost /
    /// AI 四筛子读到旧值。与 `update_selected_text` 对称补上局部刷新入口。
    ///
    /// **调用侧门控**：调用方（`main.rs` 注册的 clipboard hook）负责三重门控——
    /// `ContextConfig.enabled` / `clipboard_enabled` / 前台敏感应用检查,与
    /// `context::collect()` 逻辑对齐,避免密码管理器 Ctrl+C 悄悄进 snapshot。
    /// 本方法不做门控,只负责 upsert;`None` 时清空同 source 项。
    pub fn update_clipboard_text(&self, text: Option<String>) {
        use blink_infra::platform::context::AwarenessSource;
        let mut guard = self.snapshot.write().unwrap();
        guard.upsert_text(AwarenessSource::Clipboard, text);
    }

    /// 清除快照中的可用更新标记（0.26.20）。
    ///
    /// 用户 dismiss 版本提示或关闭「自动检查更新」时由 app 层调用：本窗口会话
    /// 内迟到的搜索响应（awareness-updated retrigger 等）不得再返回该建议——
    /// 下一次唤起由 `window_orchestrator` 按最新状态重新盖章。
    pub fn clear_available_update(&self) {
        let mut guard = self.snapshot.write().unwrap();
        guard.available_update = None;
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

    /// 0.26.10：禁用列表只读快照（`get_clipboard_item_actions` 派生过滤用，
    /// 与建议侧/result 侧同源，设置页保存热生效）。
    pub fn disabled_builtin_actions_snapshot(&self) -> Option<Vec<String>> {
        self.disabled_builtin_actions
            .read()
            .ok()
            .map(|guard| guard.clone())
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
        if let Some(entries) = self.system_entries() {
            entries.update_language(language);
        }
        tracing::debug!("SearchService 界面语言已热更新");
    }

    /// 更新 ClipboardEngine 的每页条数（SearchConfig::page_size 变化时转发）。
    /// 0.25.29：首屏 = page_size × 2、流式每批 page_size（display_pages 已退役）。
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
    /// 支持的 engine_id: "start_menu", "calc", "file", "weblinks", "bookmarks"
    pub async fn update_engine_config(&self, engine_id: &str, config: EngineConfigUpdate) {
        if let EngineConfigUpdate::StartMenu(cfg) = &config {
            for engine in &self.sync_engines {
                if let Some(system) = engine
                    .as_any()
                    .downcast_ref::<super::system_shortcuts::SystemShortcutEngine>()
                {
                    system.entries.update_config(cfg.clone());
                }
            }
        }
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
                    EngineConfigUpdate::WebLinks(cfg) => {
                        if let Some(web) = engine
                            .as_any()
                            .downcast_ref::<super::weblinks_engine::WebEngine>()
                        {
                            web.update_config(cfg);
                            tracing::debug!(engine = engine_id, "WebEngine 配置已热更新");
                        }
                    }
                    EngineConfigUpdate::Bookmarks(cfg) => {
                        if let Some(bm) = engine
                            .as_any()
                            .downcast_ref::<super::bookmark_engine::BookmarkEngine>()
                        {
                            bm.update_config(cfg);
                            tracing::debug!(engine = engine_id, "BookmarkEngine 配置已热更新");
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

        let mut all = Vec::new();
        let language = self.language.read().unwrap().clone();
        for engine in &self.sync_engines {
            if matches!(engine.id(), "start_menu" | "system_shortcut") {
                let history = std::collections::HashMap::new();
                let snapshot = ContextSnapshot::default();
                let disabled: Vec<String> = Vec::new();
                let disabled_ctx: Vec<String> = Vec::new();
                let search_ctx = QueryContext {
                    history: &history,
                    snapshot: &snapshot,
                    disabled_builtin_actions: &disabled,
                    disabled_context_bindings: &disabled_ctx,
                    language: &language,
                    takeover_keyword: None,
                };
                let items = engine.search(query, &search_ctx).await;
                all.extend(items);
            }
        }
        fuse_items(all, max_results)
    }

    pub fn system_entries(&self) -> Option<super::system_entries::SystemEntries> {
        self.sync_engines.iter().find_map(|engine| {
            engine
                .as_any()
                .downcast_ref::<super::system_shortcuts::SystemShortcutEngine>()
                .map(|e| e.entries.clone())
        })
    }

    /// 书签引擎状态投影（0.26.13 设置页：条目数/截断/扫描中）。无引擎实例返回 None。
    pub fn bookmarks_status(&self) -> Option<super::bookmark_engine::BookmarkStatus> {
        self.sync_engines
            .iter()
            .find_map(|engine| {
                engine
                    .as_any()
                    .downcast_ref::<super::bookmark_engine::BookmarkEngine>()
            })
            .map(|e| e.status())
    }

    pub fn refresh_bookmarks(&self) -> Option<super::bookmark_engine::BookmarkStatus> {
        self.sync_engines.iter().find_map(|engine| {
            engine.as_any().downcast_ref::<super::bookmark_engine::BookmarkEngine>()
        }).map(|engine| engine.refresh_now())
    }

    /// 托管 Everything 实例治理器（0.26.14：退出清理/设置页状态/启停共用）。
    /// 无 FileEngine 实例返回 None（理论不可达——build_engines 恒创建）。
    pub fn everything_runtime(&self) -> Option<super::everything_runtime::EverythingRuntime> {
        self.async_engines.iter().find_map(|engine| {
            engine
                .as_any()
                .downcast_ref::<super::file_engine::FileEngine>()
                .map(|fe| fe.everything_runtime().clone())
        })
    }

    /// 文件搜索数据源统一状态（0.26.14：设置页单一次拉取——用户实例 + 托管
    /// 实例 + 当前 data_source，替代分散的探测/状态接口）。
    pub async fn file_search_status(&self) -> Option<FileSearchStatus> {
        let engine = self.async_engines.iter().find_map(|e| {
            e.as_any().downcast_ref::<super::file_engine::FileEngine>()
        })?;
        let (port, available, probing) = engine.user_everything_state().await;
        let (data_source, active_source) = engine.source_status().await;
        Some(FileSearchStatus {
            managed: engine.everything_runtime().snapshot(),
            user_port: port,
            user_available: available,
            user_probing: probing,
            data_source,
            active_source,
        })
    }

    pub async fn reprobe_everything(&self) {
        if let Some(engine) = self.async_engines.iter().find_map(|e| {
            e.as_any().downcast_ref::<super::file_engine::FileEngine>()
        }) {
            engine.reprobe_everything().await;
        }
    }
    pub fn resolve_system_entry(&self, id: &str) -> Option<super::system_entries::Entry> {
        self.system_entries()?.resolve(id)
    }

    /// 剪贴板模式直接搜索（bypass SearchService pipeline）。
    ///
    /// Alt+C 进入剪贴板模式后，前端直接调此方法，不经过 route / get_weights / Mixed 分派。
    /// 只走 ClipboardEngine，返回流式分页结果（0.25.29 游标协议）。
    ///
    /// 0.20.1: 不使用 `max_results` 截断——ClipboardEngine 内部已按单批钳制截断，
    /// 普通搜索的 `max_results` 不干扰剪贴板模式的结果上限。
    ///
    /// 0.25.23: `kind` 类型筛选（全部/文本/图片）下推到 engine——文本与图片分表，
    /// 筛选即跳过另一张表的查询；未知值回落 All。
    ///
    /// 0.25.29: keyset 游标分页（`search_clipboard` command 透传 cursor/batch）。
    pub async fn search_clipboard_page(
        &self,
        query: &str,
        kind: Option<&str>,
        cursor: Option<blink_infra::data::clipboard::ClipboardCursor>,
        batch: usize,
    ) -> super::clipboard_engine::ClipboardPageResult {
        let arg = query.trim();
        let clip_kind = super::clipboard_engine::ClipKind::parse(kind);
        let t0 = std::time::Instant::now();

        for engine in &self.sync_engines {
            if engine.id() == "clipboard" {
                let page = match engine
                    .as_any()
                    .downcast_ref::<super::clipboard_engine::ClipboardEngine>()
                {
                    Some(clipboard_engine) => {
                        clipboard_engine
                            .search_page(arg, clip_kind, cursor, batch)
                            .await
                    }
                    // 理论不可达：id == "clipboard" 的 engine 就是 ClipboardEngine。
                    // 兜底走 trait search（恒 All 首屏），不让类型分支丢结果。
                    None => {
                        let items = {
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
                                takeover_keyword: None,
                            };
                            engine.search(arg, &search_ctx).await
                        };
                        super::clipboard_engine::ClipboardPageResult {
                            items,
                            next_cursor: None,
                            has_more: false,
                        }
                    }
                };
                tracing::trace!(
                    query = %arg,
                    count = page.items.len(),
                    has_more = page.has_more,
                    engine_ms = t0.elapsed().as_millis() as u64,
                    "search_clipboard_page: 完成"
                );
                return page;
            }
        }
        super::clipboard_engine::ClipboardPageResult {
            items: Vec::new(),
            next_cursor: None,
            has_more: false,
        }
    }

    /// 搜索:先路由 → 按 Takeover/Mixed 分支执行 → 返回首批结果 + spawn 增量。
    ///
    /// 空 query 场景（0.8.0 §1.3；0.24.8 纯建议化修订）：跳过 intent 路由 + 插件；
    /// 引擎均无空 query 召回（BuiltinEngine 的 Context-only 分支已移交 Suggestion lane，
    /// 环境感知的打开类走建议槽 Tab 采纳）——空 query 的 entries 恒为空，
    /// 建议由 `compute_suggestion` 独立产出。
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
            blink_infra::data::history::get_weights(&self.pool).await
        };
        let snapshot = self.snapshot.read().unwrap().clone();
        let disabled = self.disabled_builtin_actions.read().unwrap().clone();
        let disabled_ctx = self.disabled_context_bindings.read().unwrap().clone();
        let language = self.language.read().unwrap().clone();
        // 0.26.12：EngineTakeover 命中的触发词经 QueryContext 下传——多 keyword
        // 引擎（weblinks）据此定位条目；Mixed/AiTrigger 恒 None。
        let takeover_keyword = match &route {
            Route::EngineTakeover { keyword, .. } => keyword.clone(),
            _ => None,
        };
        let search_ctx = QueryContext {
            history: &history,
            snapshot: &snapshot,
            disabled_builtin_actions: &disabled,
            disabled_context_bindings: &disabled_ctx,
            language: &language,
            takeover_keyword: takeover_keyword.as_deref(),
        };

        // Suggestion（0.24.2 §3.7：编排顺序即防环——Route 先算、Coordinator 后算，
        // route_summary 是编排层传值不是域依赖）。
        // AiTrigger 路由的 ai-trigger 候选也由 Coordinator 从 route_summary 构造，
        // SearchService 不再有独立 AI 覆盖分支（0.9.2 Phase 5b 后置注入已删）。
        let suggestion = self.compute_suggestion(query, &snapshot, &route, seq);

        // 按 Route 分派到四个 executor（0.8.6 §8.2.1 拆 God Method）
        let entries = match route {
            Route::Takeover { plugin_id, arg, .. } => {
                self.exec_takeover(plugin_id, arg, seq, q)
            }
            Route::EngineTakeover { engine_id, arg, .. } => {
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
    fn empty_arg_hint(&self, id: &str, arg: &crate::intent::ExecArg) -> Option<String> {
        if !arg.is_none() {
            return None;
        }
        let lang = self.language.read().unwrap().clone();
        self.plugin_engine.get_empty_arg_hint(id, &lang)
    }

    /// Takeover executor（0.8.6 §8.2.1）：插件独占返回区。
    /// `q` 为 trim 后的原始查询——用于 args.hints 引导项还原触发 keyword。
    fn exec_takeover(
        &self,
        plugin_id: String,
        arg: crate::intent::ExecArg,
        seq: u64,
        q: &str,
    ) -> Vec<AppEntry> {
        if let Some(hint_text) = self.empty_arg_hint(&plugin_id, &arg) {
            tracing::debug!(plugin = %plugin_id, "empty_arg_hint 命中 Takeover，跳过插件查询");
            return vec![empty_arg_hint_entry(
                &plugin_id,
                &self.display_name(&plugin_id),
                hint_text,
            )];
        }
        // 0.26.17：args.hints 引导项——arg 为空（参数发现）或为某 hint 的严格
        // 前缀（缩小候选）时在占位项后合成；插件查询照常发起（空参默认查询有
        // 意义；无效参的最终失败仍如实显示错误行）。
        let guides = self.arg_guide_entries(q, arg.as_str(), Some(&plugin_id));
        self.spawn_takeover(plugin_id.clone(), arg.to_plugin_string(), seq);
        let mut entries = vec![placeholder_entry(
            &plugin_id,
            &self.display_name(&plugin_id),
        )];
        entries.extend(guides);
        entries
    }

    /// args.hints 列表引导项（0.26.17）：Takeover（绑定 `plugin_filter`，keyword
    /// 从 query 剥掉显式 arg 还原）与 Mixed（`plugin_filter=None`，query 恰为某
    /// 声明 hints 的 keyword 原文/全拼——Exact 混排候选场景，如裸 `ip`）两路共用。
    ///
    /// 数据源为 `router.arg_hint_rules()`（keyword↔hints join 真源，含用户自定义
    /// 触发词）；引导项回写**注册原文 keyword**（用户输拼音/别名也回规范形式）。
    /// arg 为空展示全部 hints，为严格前缀时缩小到匹配项。
    fn arg_guide_entries(
        &self,
        q: &str,
        arg: Option<&str>,
        plugin_filter: Option<&str>,
    ) -> Vec<AppEntry> {
        let rules = self.router.arg_hint_rules();
        if rules.is_empty() {
            return Vec::new();
        }
        // keyword 部分：Takeover 带参时剥掉 arg；无参/混排时整段（trim 尾空格）
        let (keyword_part, arg_str) = match arg {
            Some(a) => (
                q.strip_suffix(a.trim()).map(str::trim_end).unwrap_or(q),
                a.trim(),
            ),
            None => (q.trim_end(), ""),
        };
        if keyword_part.is_empty() {
            return Vec::new();
        }
        let kw_lower = keyword_part.to_ascii_lowercase();
        let lang = self.language.read().unwrap().clone();
        let mut out = Vec::new();
        for rule in &rules {
            if let Some(pid) = plugin_filter
                && pid != rule.plugin_id
            {
                continue;
            }
            // keyword 精确命中（原文小写或全拼）——首拼弱信号不引导（用户尚未
            // 提交 keyword，由既有 Ghost 补全先教 keyword 本身）
            let matched = kw_lower == rule.keyword.to_ascii_lowercase()
                || (!rule.pinyin_full.is_empty() && kw_lower == rule.pinyin_full);
            if !matched {
                continue;
            }
            let plugin_name = self.display_name(&rule.plugin_id);
            for hint in &rule.hints {
                if !arg_str.is_empty() && !hint.matches_prefix(arg_str) {
                    continue;
                }
                out.push(arg_guide_entry(
                    &rule.plugin_id,
                    &format!("{} {}", rule.keyword, hint.value),
                    &hint.desc.resolve(&lang),
                    &plugin_name,
                ));
            }
            // 首个命中规则即止：同 keyword 双注册属边缘配置，不重复引导
            break;
        }
        out
    }

    /// EngineTakeover executor（0.8.6 §8.2.1）：本体 engine 独占。
    async fn exec_engine_takeover(
        &self,
        engine_id: String,
        arg: crate::intent::ExecArg,
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

        // 0.26.17：args.hints 引导项（Exact 混排场景）——query 恰为声明参数提示的
        // keyword（裸 `ip` / `ip `：Exact 命中是 Priority 候选，走 Mixed 而非
        // Takeover）；占位项在插件真实结果到达后被清，引导项保留并沉到其下。
        let arg_guides = self.arg_guide_entries(q, None, None);
        all_items.extend(hint_entries);
        all_items.extend(arg_guides);
        all_items.extend(placeholders);

        // ── AI Ghost 由 Coordinator 产出（0.24.2：AiProducer → coordinate → SuggestionSet）──
        // 不在 exec_mixed 自动 spawn AI:边打字连续 spawn 会浪费 token + h2 stream 堆积。
        // 用户看到 Ghost "按 Tab 问 AI" 后显式按 Tab → 前端 invoke `chat_prompt` command
        // → ChatService::prompt(ephemeral) → 单次 spawn。

        let elapsed = search_start.elapsed().as_secs_f64() * 1000.0;
        blink_infra::utils::perf::record(
            blink_infra::utils::perf::MetricCategory::SearchEngine,
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
            let ctx = crate::plugin::PluginQueryContext::from_snapshot(&snapshot);
            // takeover 查询链路诊断（debug 级，与相邻防抖/async lane 日志同级；
            // 排查"查询中"占位与结果同屏类问题时按此判断查询是否执行/emit
            // 是否被 seq 过期吞掉——根因已由前端迟到占位丢弃修复）
            tracing::debug!(plugin = %plugin_id, seq, arg_len = arg.chars().count(), "takeover 查询开始");
            let items = plugin_engine
                .query_subset(&[(plugin_id.clone(), arg)], &ctx)
                .await;
            if seq != latest_seq.load(Ordering::SeqCst) {
                tracing::debug!(plugin = %plugin_id, seq, "takeover 查询返回但 seq 已过期，不 emit");
                return;
            }
            tracing::debug!(plugin = %plugin_id, seq, count = items.len(), "takeover 查询返回，即将 emit");
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
            let history = blink_infra::data::history::get_weights(&pool).await;
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
                    crate::plugin::PluginQueryContext::from_snapshot(&snapshot);
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
                        takeover_keyword: None,
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
    if let Err(e) = crate::event::emit_serialized(
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

/// args.hints 引导项（0.26.17）：可执行的参数引导条目。
/// - 主行显示将执行的查询（`ip refresh`），副行显示 desc（缺省回退插件显示名）
/// - `Route` 动作 payload 携带完整目标查询（尾随空格——采纳即触发该参数查询，
///   与 keyword Tab-only hint 的 `"fanyi "` 同式）
/// - 分数 0.3：低于插件真实结果（非负分 + 占位替换后置顶），高于错误行（负分），
///   增量结果到达后自然沉到真实结果之下（is_placeholder=false，不被清理）
fn arg_guide_entry(plugin_id: &str, query: &str, desc: &str, plugin_name: &str) -> AppEntry {
    AppEntry {
        name: query.to_string(),
        pinyin_name: String::new(),
        pinyin_full: String::new(),
        lnk_path: String::new(),
        is_calc: false,
        score: 0.3,
        is_placeholder: false,
        is_error: false,
        source: plugin_id.to_string(),
        description: Some(if desc.is_empty() {
            plugin_name.to_string()
        } else {
            desc.to_string()
        }),
        actions: vec![Action {
            kind: ActionKind::Route,
            payload: Some(format!("{query} ")),
            ..Action::default()
        }],
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::search::engine::SearchAction;

    #[tokio::test]
    async fn engine_completion_reaches_serialized_search_response_and_tab_route() {
        struct Events;
        impl EventPort for Events {
            fn emit(&self, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
            fn emit_to(&self, _: &str, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE history (lnk_path TEXT, hit_count INTEGER, last_used_at INTEGER)")
            .execute(&pool).await.unwrap();
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        let links = crate::config::WebLinksConfig::seed();
        super::super::register_weblinks_rules(&router, &links);
        super::super::register_bookmark_rules(&router, &crate::config::BookmarksConfig::default());
        let runtime = Arc::new(RwLock::new(SuggestionRuntimeConfig::default()));
        let mut coordinator = SuggestionCoordinator::new();
        coordinator.register(Arc::new(crate::intent::suggestion::keyword::KeywordProducer::from_router(
            router.clone(), runtime.clone(),
        )));
        let service = SearchService::new(
            Arc::new(Events), pool.clone(),
            vec![Arc::new(super::super::WebEngine::with_config(links))],
            Arc::new(PluginEngine::new(vec![], pool, None)), router, runtime, coordinator,
            Arc::new(RwLock::new(None)), Arc::new(RwLock::new(vec![])), Arc::new(RwLock::new(vec![])),
        );
        for (query, display, replacement) in [("bil", "bili", "bili "), ("bili", "", "bili "), ("bm", "", "bm ")] {
            for seq in 1..=6 {
                let response = service.search(query, seq).await;
                let json = serde_json::to_value(&response).unwrap();
                assert_eq!(json["suggestion"]["revision"], seq);
                let hint = &json["suggestion"]["primary"];
                assert_eq!(hint["kind"], "completion", "{query}: {json}");
                assert_eq!(hint["display"], display);
                assert_eq!(hint["action"]["routeQuery"]["query"], replacement);
            }
        }
        let response = service.search("bili ", 20).await;
        assert!(response.suggestion.is_none());
        assert_eq!(response.entries[0].lnk_path, "https://www.bilibili.com/");
    }

    /// 0.26.17：声明 args.hints 的插件合成参数引导项（Takeover + Mixed 两路）。
    /// 空参展示全部 hints；前缀参缩小到匹配项；完整参/无 hints 不合成。
    /// 引导项 Route 动作携带 `{keyword} {value} `（尾随空格），副行显示 desc。
    #[tokio::test]
    async fn arg_hints_synthesize_route_guide_entries_both_paths() {
        struct Events;
        impl EventPort for Events {
            fn emit(&self, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
            fn emit_to(&self, _: &str, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE history (lnk_path TEXT, hit_count INTEGER, last_used_at INTEGER)")
            .execute(&pool).await.unwrap();

        // 构造带 args.hints 的插件（exec 指向不存在的 exe——查询在后台任务失败，
        // 不影响本测试断言的同步返回批）
        let manifest: crate::plugin::PluginManifest = serde_json::from_str(r#"{
            "schema_version": 1, "id": "builtin.ip", "name": "IP", "version": "0",
            "runtime": {"exec": "never-start.exe"},
            "triggers": [{"type": "keyword", "keyword": "ip"}],
            "capabilities": ["query"],
            "args": {"hints": [
                {"value": "refresh", "aliases": ["刷新"], "desc": {"zh": "刷新出口检测结果", "en": "Refresh"}},
                {"value": "local", "aliases": ["本地"], "desc": "仅本机地址"}
            ]}
        }"#).unwrap();
        let plugin = Arc::new(crate::plugin::PluginHandle::new(
            Arc::new(manifest), std::path::PathBuf::new(), None,
        ));
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        router.add_keyword_rule(
            "builtin.ip".into(), "ip".into(),
            crate::intent::Surface::Auto, crate::intent::SurfaceView::List,
        );
        // main.rs 启动接线同款：hints 注册进 router（搜索侧唯一真源）
        router.set_plugin_args_hints(
            "builtin.ip",
            vec![
                crate::plugin::ArgHint {
                    value: "refresh".into(),
                    aliases: vec!["刷新".into()],
                    desc: crate::plugin::LocalizableText::Localized(
                        [("zh".to_string(), "刷新出口检测结果".to_string()),
                         ("en".to_string(), "Refresh".to_string())].into_iter().collect()),
                },
                crate::plugin::ArgHint {
                    value: "local".into(),
                    aliases: vec!["本地".into()],
                    desc: crate::plugin::LocalizableText::Plain("仅本机地址".into()),
                },
            ],
        );
        let runtime = Arc::new(RwLock::new(SuggestionRuntimeConfig::default()));
        let coordinator = SuggestionCoordinator::new();
        let service = SearchService::new(
            Arc::new(Events), pool.clone(), vec![],
            Arc::new(PluginEngine::new(vec![plugin], pool, None)),
            router, runtime, coordinator,
            Arc::new(RwLock::new(None)), Arc::new(RwLock::new(vec![])), Arc::new(RwLock::new(vec![])),
        );

        // ① "ip "（Exact 混排候选）：占位项 + refresh/local 两条引导项
        let response = service.search("ip ", 1).await;
        assert_eq!(response.entries.len(), 3, "{:?}", response.entries.iter().map(|e| (e.name.as_str(), e.is_placeholder)).collect::<Vec<_>>());
        let guides: Vec<&AppEntry> = response.entries.iter().filter(|e| !e.is_placeholder).collect();
        assert_eq!(guides.len(), 2);
        assert_eq!(guides[0].name, "ip refresh");
        assert_eq!(guides[0].description.as_deref(), Some("刷新出口检测结果"));
        assert_eq!(guides[1].name, "ip local");
        assert_eq!(guides[1].description.as_deref(), Some("仅本机地址"));
        // Route 动作 wire 契约：kind=route + payload 带尾随空格目标查询
        let json = serde_json::to_value(&response).unwrap();
        let guide_idx: Vec<usize> = response.entries.iter().enumerate()
            .filter(|(_, e)| !e.is_placeholder).map(|(i, _)| i).collect();
        assert_eq!(json["entries"][guide_idx[0]]["actions"][0]["kind"], "route");
        assert_eq!(json["entries"][guide_idx[0]]["actions"][0]["payload"], "ip refresh ");
        assert_eq!(json["entries"][guide_idx[1]]["actions"][0]["payload"], "ip local ");

        // ② "ip r"（前缀参 Takeover）：只保留 refresh 引导；占位仍在（查询照常发起）
        let response = service.search("ip r", 2).await;
        assert_eq!(response.entries.len(), 2, "{:?}", response.entries.iter().map(|e| e.name.as_str()).collect::<Vec<_>>());
        assert!(response.entries[0].is_placeholder);
        assert_eq!(response.entries[1].name, "ip refresh");

        // ③ "ip 刷"（别名前缀）：命中 refresh，回写规范 value
        let response = service.search("ip 刷", 3).await;
        assert_eq!(response.entries.len(), 2);
        assert_eq!(response.entries[1].name, "ip refresh");

        // ④ "ip refresh"（完整参）：无引导项（用户已输入目标参数）
        let response = service.search("ip refresh", 4).await;
        assert_eq!(response.entries.len(), 1);
        assert!(response.entries[0].is_placeholder);

        // ⑤ "ip xyz"（不匹配参数）：无引导项，查询照常（错误如实显示）
        let response = service.search("ip xyz", 5).await;
        assert_eq!(response.entries.len(), 1);
    }

    /// 0.26.17：未声明 args.hints 的插件 Takeover 行为不变（无引导项）。
    #[tokio::test]
    async fn takeover_without_arg_hints_keeps_placeholder_only() {
        struct Events;
        impl EventPort for Events {
            fn emit(&self, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
            fn emit_to(&self, _: &str, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new().max_connections(1)
            .connect("sqlite::memory:").await.unwrap();
        sqlx::query("CREATE TABLE history (lnk_path TEXT, hit_count INTEGER, last_used_at INTEGER)")
            .execute(&pool).await.unwrap();
        let manifest: crate::plugin::PluginManifest = serde_json::from_str(
            r#"{"schema_version":1,"id":"builtin.translate","name":"翻译","version":"0",
                "runtime":{"exec":"never-start.exe"},
                "triggers":[{"type":"keyword","keyword":"翻译"}]}"#,
        )
        .unwrap();
        let plugin = Arc::new(crate::plugin::PluginHandle::new(
            Arc::new(manifest), std::path::PathBuf::new(), None,
        ));
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        router.add_keyword_rule(
            "builtin.translate".into(), "翻译".into(),
            crate::intent::Surface::Auto, crate::intent::SurfaceView::List,
        );
        let service = SearchService::new(
            Arc::new(Events), pool.clone(), vec![],
            Arc::new(PluginEngine::new(vec![plugin], pool, None)),
            router, Arc::new(RwLock::new(SuggestionRuntimeConfig::default())),
            SuggestionCoordinator::new(),
            Arc::new(RwLock::new(None)), Arc::new(RwLock::new(vec![])), Arc::new(RwLock::new(vec![])),
        );
        // 空参/带参均只有占位项（既有行为）
        for query in ["翻译 ", "翻译 hello"] {
            let response = service.search(query, 1).await;
            assert_eq!(response.entries.len(), 1, "{query}");
            assert!(response.entries[0].is_placeholder);
        }
    }

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
            pinned_until: 0,
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


#[cfg(test)]
mod generation_tests {
    //! 0.27.3 快照代次保护：show 后异步回写（UIA 选区 / Shell 文件选中）
    //! 带冻结代次，`update_snapshot`（新一次 invoke）后旧结果必须被丢弃。

    use super::*;


    async fn minimal_service() -> SearchService {
        struct Events;
        impl EventPort for Events {
            fn emit(&self, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
            fn emit_to(&self, _: &str, _: &str, _: serde_json::Value) -> Result<(), String> { Ok(()) }
        }
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        sqlx::query("CREATE TABLE history (lnk_path TEXT, hit_count INTEGER, last_used_at INTEGER)")
            .execute(&pool)
            .await
            .unwrap();
        let router = Arc::new(crate::intent::RuleRouter::new(true));
        let runtime = Arc::new(RwLock::new(SuggestionRuntimeConfig::default()));
        SearchService::new(
            Arc::new(Events),
            pool,
            vec![],
            Arc::new(PluginEngine::new(vec![], sqlx::sqlite::SqlitePool::connect_lazy("sqlite::memory:").unwrap(), None)),
            router,
            runtime,
            SuggestionCoordinator::new(),
            Arc::new(RwLock::new(None)),
            Arc::new(RwLock::new(vec![])),
            Arc::new(RwLock::new(vec![])),
        )
    }

    fn file_state(generation: u64) -> blink_infra::platform::selection::files::FileSelectionState {
        use blink_infra::platform::selection::files::{FileSelectionItem, FileSelectionState};
        FileSelectionState {
            items: vec![FileSelectionItem {
                path: "C:/tmp/a.md".into(),
                is_dir: false,
                display_name: "a.md".into(),
            }],
            source_pid: 42,
            source_generation: generation,
            collected_at: std::time::Instant::now(),
            truncated: false,
        }
    }

    /// 新代次回写成功；随后旧代次回写被拒（invoke A→B 场景）。
    #[tokio::test]
    async fn stale_generation_write_is_rejected() {
        let service = minimal_service().await;

        // invoke A：写快照 → 冻结代次
        service.update_snapshot(ContextSnapshot::default());
        let gen_a = service.current_snapshot_generation();

        // invoke B（A 的异步采集仍在途）：update_snapshot 递增代次
        service.update_snapshot(ContextSnapshot::default());
        let gen_b = service.current_snapshot_generation();
        assert!(gen_b > gen_a, "update_snapshot 必须递增代次");

        // A 的迟到回写 → 拒绝
        assert!(
            !service.update_file_selection_if_fresh(gen_a, Some(file_state(gen_a))),
            "旧代次文件选中回写必须被拒绝"
        );
        assert!(
            !service.update_selected_text_if_fresh(gen_a, Some("stale".into()), None),
            "旧代次选区回写必须被拒绝"
        );

        // B 的回写 → 成功落快照
        assert!(service.update_file_selection_if_fresh(gen_b, Some(file_state(gen_b))));
        assert!(service.update_selected_text_if_fresh(gen_b, Some("fresh".into()), None));
        {
            let guard = service.snapshot.read().unwrap();
            assert!(guard.file_selection.is_some(), "新代次回写应落快照");
            assert_eq!(guard.find_text(blink_infra::platform::context::AwarenessSource::Selection).map(|v| v.text), Some("fresh"));
        }
    }

    /// 空结果清理：新代次的 None 回写清空本代文件选中。
    #[tokio::test]
    async fn fresh_none_clears_file_selection() {
        let service = minimal_service().await;
        service.update_snapshot(ContextSnapshot::default());
        let generation = service.current_snapshot_generation();
        assert!(service.update_file_selection_if_fresh(generation, Some(file_state(generation))));
        assert!(service.update_file_selection_if_fresh(generation, None));
        assert!(service.snapshot.read().unwrap().file_selection.is_none());
    }
}
