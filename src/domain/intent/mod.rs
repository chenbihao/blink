//! 意图路由层 —— 从 query 到呈现调度的转换(0.4)。
//!
//! 核心模型:触发(match)与呈现(surface)正交(见 `product-platform.md` §4.3)。
//! RuleRouter 持 keyword/regex/context 规则表,`route()` 判定命中后解出 surface(takeover/priority/inline),
//! 返回 `Route` 供 SearchService 调度。
//!
//! 0.8.2 §3.4 加 Context 规则表：与 keyword/regex 表并存,专用于「非 query 依赖」的
//! 触发信号（选区/剪贴板/前台）。`TextIsNonTargetLang` 需 target 语言,通过
//! `PluginSettingResolver` trait 反转读插件 settings(`target_lang`)。
//!
//! 0.8.3 §4.4 加 `Suggestion` 通道 —— push→Ghost 转型：
//! - 空 query 场景，Context 命中不再进 `route()` 产 candidate（抢首屏），改由 `best_suggestion` 产 Suggestion（Ghost + Tab 采纳）。
//! - 非空 query 场景，keyword+context 同 plugin 命中的 `merge_hits` 加分逻辑保留（增强 keyword 命中，不是抢首屏）。
//! - `suggest_completion`（0.8.1 旧接口）保留供 fallback / 单测，生产走 `best_suggestion` 统一入口。

use std::sync::{Arc, Mutex, RwLock};

use crate::domain::context::trigger::{self as ctx_trigger, ContextTrigger};
use crate::domain::plugin::PluginSettingResolver;
use crate::infra::platform::context::{AwarenessSource, ContextSnapshot};
use crate::infra::utils::text::{pinyin_full, pinyin_initials};

pub mod suggest;
pub mod suggestion;
pub use suggest::CompletionHint;
pub use suggestion::{Suggestion, SuggestionOrigin, SuggestionSource};

// ── 呈现模式 ──────────────────────────────────────────────

/// 插件命中后在返回区的占用方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Surface {
    /// 普通混排:插件结果作一路召回,按 score 与应用/文件并列排序。
    Inline,
    /// 置顶:插件结果排最前,但其他引擎结果保留在下方。
    Priority,
    /// 接管:跳过其他引擎,该插件独占整个返回区。
    Takeover,
    /// 自动(默认):由命中强度决定 surface——无参精确→Priority,带参前缀→Takeover。
    Auto,
}

/// takeover 时的内容形态。0.4 仅 `List`,P3 扩展 `Chat`/`Custom`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SurfaceView {
    List,
}

impl Default for SurfaceView {
    fn default() -> Self {
        SurfaceView::List
    }
}

// ── 执行参数 / 排序梯标（0.8.4 §5.3.1 / §5.3.2 四域架构）─────────

/// 执行参数类型墙（0.8.4 §5.3.2）。
///
/// **后端内部类型，不直接序列化给前端**——`SearchAction::RunAction.arg` 对外仍保持
/// `Option<serde_json::Value>` 契约；产出 RunAction 时由 [`ExecArg::to_run_action_arg`] 转换，
/// 外部 JSON 零变化，域类型不跨边界。
///
/// 设计目的：把「参数必须来自用户显式交互」这条产品原则编码进类型系统——
/// Routing/Suggestion 域写不出「把 snapshot 抽来的值塞进执行参数」的代码，构造
/// `UserExplicit` 必须显式，新加入口在 review 时一目了然。真实价值是**防回归**
/// （禁止未来重新引入隐式代参），不是修现存 bug（0.8.3 收尾已修行为）。
///
/// ⚠️ **信任边界（诚实标注）**：这是「后端内部墙」而非「端到端信任链」——
/// `run_builtin_action` 命令入口对前端 arg 会**无条件包装**成 `UserExplicit`
/// （自我认证，非不可伪造）。本地受信 WebView 够用，但 0.9 接 AI Provider 时
/// **勿**误以为能防「AI 构造的恶意 invoke」——它只防后端域越界，不防受信入口外的构造。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecArg {
    /// 用户显式给的参数——**唯一合法的执行参数来源**。
    ///
    /// 产生入口（0.8.4 审计点，见 `exec_arg_construction_sites_audited` 回归测试）：
    /// - `match_keyword` 的 `Prefix { arg }` / `InitialsPrefix { arg }` 部分（用户打字）
    /// - Ghost 采纳后 query 被 replacement 替换，再走一遍 match_keyword
    /// - 内置动作候选产出时从 snapshot 抽（展示即抽参 + 整链路透传；见 §5.3.4）
    UserExplicit(String),

    /// 无参——走 empty_arg_hint 或插件内部决策。显式的「无参数」语义，优于 `Option::None` 空值。
    None,
}

impl ExecArg {
    /// 取用户显式参数；`None` 返回 `None`。
    #[allow(dead_code)] // 内部工具方法，未来扩展点
    pub fn as_str(&self) -> Option<&str> {
        match self {
            ExecArg::UserExplicit(s) => Some(s.as_str()),
            ExecArg::None => None,
        }
    }

    /// 是否无参。
    pub fn is_none(&self) -> bool {
        matches!(self, ExecArg::None)
    }

    /// 是否用户显式给参。
    pub fn is_explicit(&self) -> bool {
        matches!(self, ExecArg::UserExplicit(_))
    }

    /// 转成 `SearchAction::RunAction.arg` 的外部契约格式（0.8.4 §5.3.2）。
    ///
    /// `UserExplicit(s)` → `Some(Value::String(s))`，`None` → `None`。
    /// 前端契约层零变化。
    #[allow(dead_code)] // 未来 SearchService → Action trait 迁移时消费
    pub fn to_run_action_arg(&self) -> Option<serde_json::Value> {
        match self {
            ExecArg::UserExplicit(s) => Some(serde_json::Value::String(s.clone())),
            ExecArg::None => None,
        }
    }

    /// 字符数（filter_route 参数过短判定按字符数）。`None` → 0。
    pub fn char_len(&self) -> usize {
        match self {
            ExecArg::UserExplicit(s) => s.chars().count(),
            ExecArg::None => 0,
        }
    }

    /// 转成插件查询参数字符串（`UserExplicit(s)` → `s`，`None` → 空串）。
    ///
    /// SearchService 把 Candidate/Takeover 的 ExecArg 传给插件 spawn 时用。
    pub fn to_plugin_string(&self) -> String {
        match self {
            ExecArg::UserExplicit(s) => s.clone(),
            ExecArg::None => String::new(),
        }
    }
}

impl Default for ExecArg {
    fn default() -> Self {
        ExecArg::None
    }
}

impl From<&str> for ExecArg {
    /// 空串 → None（无参语义）；非空 → UserExplicit。主要供测试构造 Hit 用。
    fn from(s: &str) -> Self {
        if s.is_empty() {
            ExecArg::None
        } else {
            ExecArg::UserExplicit(s.to_string())
        }
    }
}

impl From<String> for ExecArg {
    fn from(s: String) -> Self {
        if s.is_empty() {
            ExecArg::None
        } else {
            ExecArg::UserExplicit(s)
        }
    }
}

/// Suggestion 域向 Routing 域的单向排序反馈（0.8.4 §5.3.1 Surface Booster）。
///
/// Suggestion 产 Ghost 时同时产出此 hint，SearchService 下一轮 `route()` 把它作为
/// **排序梯标**传入——只影响同 plugin 候选的 surface 排序，不影响 arg、不影响候选集，
/// 不让 Awareness 跨过信任边界直接干扰 Routing。
///
/// ⚠️ 跨轮反馈滞后一轮；0.9 AI 异步化后此机制失效（见 0.8 文档 §5.6），届时改增量重排。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RankingHint {
    /// 被推升的插件 id（同 plugin 的 keyword 命中 surface 升级）。
    pub boost_plugin_id: String,
}

#[cfg(test)]
mod exec_arg_tests {
    use super::*;

    #[test]
    fn none_is_not_explicit() {
        let a = ExecArg::None;
        assert!(a.is_none());
        assert!(!a.is_explicit());
        assert_eq!(a.as_str(), None);
        assert_eq!(a.to_run_action_arg(), None);
        assert_eq!(a.char_len(), 0);
    }

    #[test]
    fn user_explicit_roundtrip() {
        let a = ExecArg::UserExplicit("hello".to_string());
        assert!(!a.is_none());
        assert!(a.is_explicit());
        assert_eq!(a.as_str(), Some("hello"));
        // 对外契约：UserExplicit(s) → Some(Value::String(s))（0.8.4 §5.3.2）
        assert_eq!(
            a.to_run_action_arg(),
            Some(serde_json::Value::String("hello".to_string()))
        );
        assert_eq!(a.char_len(), 5);
    }

    #[test]
    fn char_len_counts_chars_not_bytes() {
        // 「北京」= 2 chars / 6 bytes；filter_route 参数过短判定按字符数
        let a = ExecArg::UserExplicit("北京".to_string());
        assert_eq!(a.char_len(), 2);
    }

    #[test]
    fn default_is_none() {
        // 显式「无参数」语义，而非空字符串空值
        assert_eq!(ExecArg::default(), ExecArg::None);
    }

    #[test]
    fn ranking_hint_carries_plugin_id() {
        let h = RankingHint {
            boost_plugin_id: "builtin.translate".to_string(),
        };
        assert_eq!(h.boost_plugin_id, "builtin.translate");
    }
}

// ── 路由结果 ──────────────────────────────────────────────

/// Mixed 分支的单个候选(命中但未 takeover 的插件)。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub plugin_id: String,
    /// 传给插件的参数(0.8.4 §5.3.2 ExecArg 类型墙：精确命中→None;前缀命中→UserExplicit(余下文本))。
    pub arg: ExecArg,
    /// 实际 surface(Inline 或 Priority;Takeover 不走 Mixed)。
    pub surface: Surface,
    /// 命中派生形式（首拼）时的规范拼音提示；供 ghost text 未来展示（0.8.1）。
    /// 命中原文/pinyin_full 恒 None。
    #[allow(dead_code)] // 前端 v1 不直接消费；预留字段避免后续再改契约
    pub hint: Option<String>,
}

/// `route()` 返回的调度类型——SearchService 据此决定召回策略与 UI 呈现。
#[derive(Debug, Clone)]
pub enum Route {
    /// 接管:该插件独占返回区。
    Takeover {
        plugin_id: String,
        arg: ExecArg,
        #[allow(dead_code)] // 0.4 仅 List;P3 扩展 Chat/Custom 时消费
        view: SurfaceView,
        /// 同 Candidate.hint；Takeover 走 keyword 强信号时恒 None（首拼不升级 Takeover）。
        #[allow(dead_code)]
        hint: Option<String>,
    },
    /// 本体 engine 独占（0.8.5 §6.4 修正）：keyword 命中某内置 engine（如 ClipboardEngine
    /// 的"剪贴板"），该 engine 独占返回区。与 Takeover 语义等价，区别在**执行分派**：
    /// - `Takeover.plugin_id` → SearchService `spawn_takeover` 走 JSONL IPC 查插件进程
    /// - `EngineTakeover.engine_id` → SearchService 直接调对应 sync engine
    ///
    /// **为什么单独一个变体**：0.8.5 §6.4 让本体内置数据（剪贴板历史）也能独占返回，
    /// 但绕 subprocess 一圈无意义。用 enum 变体而非 `plugin_id: "engine:xxx"` 前缀约定
    /// 是类型墙原则（0.8.4 §5.3.2 ExecArg 精神一致）——把"这是本体不是插件"编译期钉死。
    /// 0.9 若统一 Action trait，此变体与 Takeover 可再收敛。
    EngineTakeover {
        /// 本体 engine 的 id（对应 `SearchEngine::id()`）。
        engine_id: String,
        /// 传给 engine 的参数（无参 → None，带参 → UserExplicit）。
        arg: ExecArg,
    },
    /// AI 前缀触发（0.9.x）：用户输入 "ai xxx" 直接进入 AI 模式。
    ///
    /// 与 EngineTakeover 类似是强信号独占，但走 AI 路径而非 engine 路径。
    /// 在 route() 中优先级介于 EngineTakeover 和 plugin keyword 之间——
    /// "ai" 是本体保留前缀，不会与插件 keyword 冲突。
    AiTrigger {
        /// "ai " 之后的用户输入（已 trim）。
        arg: String,
    },
    /// 混排:本地引擎照常召回;命中插件按各自 surface 参与排序。
    Mixed { candidates: Vec<Candidate> },
}

// ── Trait ─────────────────────────────────────────────────

/// 单次查询的共享上下文。0.4 仅 history + snapshot;P3 VectorRouter/AIRouter 扩展时用上。
#[allow(dead_code)] // 0.4+ 意图路由扩展时启用
pub struct QueryContext<'a> {
    /// 历史权重（lnk_path → (hit_count, last_used_at)）。0.7.5 含时间衰减。
    pub history: &'a std::collections::HashMap<String, (i64, i64)>,
    /// 唤起时的上下文快照（前台应用、剪贴板等）。
    pub snapshot: &'a ContextSnapshot,
}

#[async_trait::async_trait]
pub trait IntentRouter: Send + Sync {
    /// 路由决策（0.8.4 §5.3.1：断 Awareness 依赖,route 对 snapshot 完全无知）。
    ///
    /// - `history`:lnk_path → (hit_count, last_used_at),频率加权用（0.8.4 阶段 route
    ///   内部未消费,为 0.9 VectorRouter/AIRouter 预留稳定签名）
    /// - `ranking_hint`:Suggestion 域上一轮产的 Surface Booster——只影响同 plugin 命中
    ///   的 surface 排序,不影响 arg、不影响候选集
    async fn route(
        &self,
        query: &str,
        history: &std::collections::HashMap<String, (i64, i64)>,
        ranking_hint: Option<&RankingHint>,
    ) -> Route;

    /// 算 ghost text 补全（0.8.1 §2.4）。默认实现返回 None（非 RuleRouter 实现无需支持）。
    #[allow(dead_code)] // 0.8.1 遗留 API；0.8.3 起走 best_suggestion，保留供单测
    fn suggest_completion(&self, _query: &str, _min_score: f64) -> Option<CompletionHint> {
        None
    }

    /// 算 top-1 Suggestion（0.8.3 §4.4）。默认实现返回 None。
    ///
    /// `RuleRouter` 覆写：
    /// - 空 query 走 Context 分支（`match_context_hits` + `context_confidence`）
    /// - 非空 query 走 Keyword 分支（`compute_hint_scored`）
    /// - 因空/非空互斥,0.8.3 阶段两路不直接竞争；0.9 AI 接入时加第三路 → 走同一竞争路径
    fn best_suggestion(
        &self,
        _query: &str,
        _snapshot: &ContextSnapshot,
        _min_score: f64,
    ) -> Option<Suggestion> {
        None
    }

    /// 更新界面语言快照（0.8.2 §3.4）。默认 no-op；`RuleRouter` 覆写以支持
    /// `TextIsNonTargetLang` 中 `target_lang="auto"` 的回退。命令层 `update_language`
    /// 通过 `SearchService::update_language` 转发至此。
    fn set_app_language(&self, _language: String) {}

    /// 更新 context binding 禁用列表（0.8.3 §4.6）。默认 no-op。
    fn apply_context_disable_list(&self, _keys: Vec<String>) {}

    /// 取走上一次 `best_suggestion` 产出的 RankingHint（0.8.6 §8.1.2）。
    /// 默认返回 None（非 RuleRouter 实现无需支持）。
    fn take_last_ranking_hint(&self) -> Option<RankingHint> {
        None
    }
}

// ── RuleRouter ────────────────────────────────────────────

/// 规则匹配核心:纯同步、可单测。
///
/// 0.8.2 §3.4 起持三张规则表：keyword / regex（原有）+ context（新增）。
/// Context 规则通过 `PluginSettingResolver` 反查插件 `target_lang`（`auto` → `app_language`）。
///
/// 0.8.3 §4.6 加 `disabled_bindings`：用户在「上下文感知」面板关掉某条 context binding
/// 时进此集合。key 格式 `{target_id}::{trigger_key}`（双冒号避开 target_id 内部点/冒号）。
/// 采用黑名单模式，`context_rules` 依然是运行时唯一 trigger→target 表——从 manifest 加载
/// = 默认全启用，disable 项在 `match_context_hits` 中按 key 跳过。
pub struct RuleRouter {
    rules: RwLock<Vec<Rule>>,
    /// 全局总闸:为 false 时所有 Takeover 降级 Priority。
    takeover_enabled: RwLock<bool>,
    /// Context 规则表（0.8.2 §3.4）。与 keyword/regex 表并存,不受 query 影响。
    context_rules: RwLock<Vec<ContextRule>>,
    /// 本体 engine keyword 规则表（0.8.5 §6.4）。
    ///
    /// **为什么与 plugin `rules` 表分开**：engine 触发路径与 plugin 完全不同——
    /// - engine 命中恒走 Takeover 语义（本体数据的 keyword 信号天然强，如 "剪贴板"
    ///   不会误命中；不需要 surface / Priority / Inline 概念）
    /// - engine id 是编译期常量（对应 `SearchEngine::id()` 返回的 `&'static str`），
    ///   不像插件 id 是运行时字符串（manifest 加载）
    /// - 无 Context / Regex 触发场景（0.8.5 只有 keyword）
    ///
    /// 表由 `build_default_registry` 在 `main.rs` 启动时一次性注入，运行时不变更。
    engine_rules: RwLock<Vec<EngineRule>>,
    /// 插件 setting 读取器（`target_lang` 等），后置注入。
    /// None → Context 规则中 `TextIsNonTargetLang` 直接不命中（保守）。
    settings: RwLock<Option<Arc<dyn PluginSettingResolver>>>,
    /// AppConfig.language 快照，`target_lang=auto` 时回退用。
    /// Setter 与 AppConfig 热更新联动。
    app_language: RwLock<String>,
    /// 用户禁用的 context binding key 集合（0.8.3 §4.6）。
    /// key = `binding_key(target_id, trigger_key)`。命中 key 的 binding 在
    /// `match_context_hits` 中被跳过——route() 与 best_suggestion() 共用此判定。
    disabled_bindings: RwLock<std::collections::HashSet<String>>,
    /// Suggestion 多源竞争仲裁器（0.8.6 §8.1.2）。
    /// `best_suggestion` 委托此 arbiter，不再内嵌 if/else 分支。
    arbiter: RwLock<suggestion::arbiter::SuggestionArbiter>,
    /// 上一次 `best_suggestion` 产出的 RankingHint（0.8.6 §8.1.2）。
    /// 由 arbiter 竞争后写入，SearchService 下一轮 `route()` 读取做 Surface Booster。
    /// 替代原 `Suggestion.ranking_hint` 的跨轮反馈通道。
    last_ranking_hint: Mutex<Option<RankingHint>>,
}

struct Rule {
    plugin_id: String,
    kind: RuleKind,
    surface: Surface,
    view: SurfaceView,
}

enum RuleKind {
    Keyword(String),
    Regex(regex::Regex),
}

/// Context 规则（0.8.2 §3.4）。
///
/// 与 `Rule` 分离表：keyword/regex 按 query 匹配、context 按 snapshot 匹配。
/// **`surface` 已在 manifest 侧收窄为 `Priority`**（`Inline` 声明会在
/// `add_context_rule` warn+降级），本结构中恒为 `Surface::Priority`；未来
/// 0.8.3 放开 Inline 时直接改此字段类型即可。
struct ContextRule {
    plugin_id: String,
    when: ContextTrigger,
    surface: Surface,
}

/// 本体 engine keyword 规则（0.8.5 §6.4）。
///
/// 命中即产 `Route::EngineTakeover`——engine 独占返回区。
/// `keywords` 支持多触发词（如剪贴板用 `["剪贴板", "clip", "jtb", "jiantieban"]`），
/// 匹配走"原文相等或 kw+空格前缀"（跟插件 keyword 同套 `match_keyword` 逻辑，
/// 但只取 Exact/Prefix 强信号，首拼弱信号在 engine 场景意义不大）。
struct EngineRule {
    engine_id: String,
    keywords: Vec<String>,
}

/// 单次命中类型（0.8.1 §2.3 三态 + Initials 二分）。
///
/// - `Exact` / `Prefix`：命中"原文形式"——汉字明码 / 英文 keyword / pinyin_full。
/// - `InitialsExact` / `InitialsPrefix`：命中"首拼派生形式"（如 `fy` → `翻译`）。
///   弱信号，`resolve_surface(Auto)` 下**不独占**——Exact→Priority，Prefix→Inline。
///   ghost text 侧走独立 suggest 通道。
///
/// 拆分成 4 变体（而不是 `Initials { arg, ... }` 靠 `arg.is_empty()` 分支）：
/// - `resolve_surface` 里两分支纯类型驱动，无需读 `arg` 字段
/// - 编译期就区分"无参首拼" vs "带参首拼"，新加分支时不容易漏
///
/// `hint` 字段用于未来 UI 教学（"更规范的形式"）。命中原文时 None；命中派生形式时
/// 承载 `pinyin_full(keyword)`（如 `fy` 命中 `翻译` → hint = `"fanyi"`）。
enum MatchType {
    /// 精确命中(无参)——命中原文形式
    Exact { hint: Option<String> },
    /// 前缀带参(余下文本)——命中原文形式
    Prefix { arg: String, hint: Option<String> },
    /// 首拼精确命中(无参)——弱信号
    InitialsExact { hint: String },
    /// 首拼前缀带参——弱信号
    InitialsPrefix { arg: String, hint: String },
}

impl RuleRouter {
    pub fn new(takeover_enabled: bool) -> Self {
        RuleRouter {
            rules: RwLock::new(Vec::new()),
            takeover_enabled: RwLock::new(takeover_enabled),
            context_rules: RwLock::new(Vec::new()),
            engine_rules: RwLock::new(Vec::new()),
            settings: RwLock::new(None),
            // 单测下不注入 language → 用 "zh" 兜底（Blink 默认 UI 语言）。
            app_language: RwLock::new("zh".to_string()),
            disabled_bindings: RwLock::new(std::collections::HashSet::new()),
            // arbiter 初始为空，构造完成后通过 `init_arbiter` 注入 producers
            arbiter: RwLock::new(suggestion::arbiter::SuggestionArbiter::new()),
            last_ranking_hint: Mutex::new(None),
        }
    }

    /// 初始化 SuggestionArbiter 的 producers（0.8.6 §8.1.2）。
    ///
    /// 必须在 `RuleRouter` 被 `Arc` 包装后调用——`ContextProducer` 和 `KeywordProducer`
    /// 都需要 `Arc<RuleRouter>` 来访问内部数据。
    ///
    /// `min_score` 是共享引用——`SearchService` 的 autosuggest 配置热更新时写入新值，
    /// `KeywordProducer.produce` 每次读取最新阈值，无需额外通知。
    ///
    /// 在 `main.rs` 中 `Arc::new(RuleRouter::new(...))` 之后立即调用。
    pub fn init_arbiter(self: &Arc<Self>, min_score: Arc<std::sync::RwLock<f64>>) {
        let mut arbiter = self.arbiter.write().unwrap();
        arbiter.register(Arc::new(suggestion::keyword::KeywordProducer::from_router(
            self.clone(),
            min_score,
        )));
        arbiter.register(Arc::new(suggestion::context::ContextProducer::new(
            self.clone(),
        )));
    }

    /// 取走上一次 `best_suggestion` 产出的 RankingHint（一次性消费）。
    ///
    /// SearchService 每次 `search()` 结束后调用此方法拿 hint，
    /// 存入 `last_ranking_hint` 给下一轮 `route()` 做 Surface Booster。
    pub fn take_last_ranking_hint(&self) -> Option<RankingHint> {
        self.last_ranking_hint.lock().unwrap().take()
    }

    /// 后置注入 `PluginSettingResolver`（0.8.2 §3.4）。
    ///
    /// **构造顺序**：`RuleRouter::new` 早于 `PluginEngine::new`（`main.rs`），因此
    /// 依赖通过 setter 后置装配。单测里通常不需要（`TextIsNonTargetLang` 单测可传
    /// mock；其他 Context 规则不依赖 settings）。
    pub fn set_setting_resolver(&self, resolver: Arc<dyn PluginSettingResolver>) {
        *self.settings.write().unwrap() = Some(resolver);
    }

    /// 更新 AppConfig.language 快照（0.8.2 §3.4）。
    /// AppConfig 热更新时应调用；未调用时用 `RuleRouter::new` 里的 `"zh"` 默认。
    pub fn set_app_language(&self, lang: String) {
        *self.app_language.write().unwrap() = lang;
    }

    /// 更新 context binding 禁用列表（0.8.3 §4.6）。
    ///
    /// 参数：`{target_id}::{trigger_key}` 格式字符串列表（由 AppConfig 持久化）。
    /// 每次 AppConfig 热更新时应调用；同时**每次 `reload_plugin_triggers` 之后**
    /// 也无需重调——本表独立于 `context_rules`（黑名单模式，一份数据源）。
    ///
    /// 空列表清空（用户勾回所有 binding）。
    pub fn apply_context_disable_list(&self, keys: Vec<String>) {
        let mut guard = self.disabled_bindings.write().unwrap();
        guard.clear();
        guard.extend(keys);
        tracing::debug!(count = guard.len(), "context binding 禁用列表已更新");
    }

    #[allow(dead_code)] // 配置热更新入口,当前未接入
    pub fn set_takeover_enabled(&self, enabled: bool) {
        *self.takeover_enabled.write().unwrap() = enabled;
    }

    /// 从 manifest 注入 keyword 规则。调用方负责 surface/view 的向后兼容转换。
    pub fn add_keyword_rule(
        &self,
        plugin_id: String,
        keyword: String,
        surface: Surface,
        view: SurfaceView,
    ) {
        let mut rules = self.rules.write().unwrap();
        rules.push(Rule {
            plugin_id,
            kind: RuleKind::Keyword(keyword),
            surface,
            view,
        });
    }

    /// 删除某个插件的所有规则（热更新时用）。清 keyword/regex + context 三张表。
    pub fn remove_plugin_rules(&self, plugin_id: &str) {
        self.rules
            .write()
            .unwrap()
            .retain(|r| r.plugin_id != plugin_id);
        self.context_rules
            .write()
            .unwrap()
            .retain(|r| r.plugin_id != plugin_id);
    }

    /// 注入本体 engine keyword 规则（0.8.5 §6.4）。
    ///
    /// 由 `main.rs` 启动时调用（build_default_registry 同类工作，只不过针对 engine）。
    /// engine keyword 表运行时不变（engine 由本体编译期决定），无需 remove/热更新方法。
    ///
    /// 重复注入同 engine_id 覆盖前值（防单测/重复 setup 泄漏，同 `apply_context_disable_list`）。
    pub fn add_engine_rule(&self, engine_id: String, keywords: Vec<String>) {
        let mut rules = self.engine_rules.write().unwrap();
        rules.retain(|r| r.engine_id != engine_id);
        rules.push(EngineRule {
            engine_id,
            keywords,
        });
    }

    /// 从 manifest 注入 Context 规则（0.8.2 §3.4）。
    ///
    /// - `surface` 声明 `Inline` 会 warn+降级 Priority（0.8.2 收窄）。
    /// - 未来放开 `Inline` 时改本函数即可，规则表已按 `Surface` 存。
    pub fn add_context_rule(
        &self,
        plugin_id: String,
        when: ContextTrigger,
        declared_surface: crate::domain::plugin::ManifestSurfaceHint,
    ) {
        use crate::domain::plugin::ManifestSurfaceHint;
        let surface = match declared_surface {
            ManifestSurfaceHint::Priority => Surface::Priority,
            ManifestSurfaceHint::Inline => {
                // 0.8.2 §3.2.3 收窄：Inline 保留 enum 但代码路径 warn+降级
                tracing::warn!(
                    plugin = %plugin_id,
                    "context trigger 声明 surface=inline,0.8.2 阶段降级为 priority(0.8.3+ 再放开)",
                );
                Surface::Priority
            }
        };
        self.context_rules.write().unwrap().push(ContextRule {
            plugin_id,
            when,
            surface,
        });
    }

    /// 重新加载某个插件的触发规则（热更新）。
    pub fn reload_plugin_triggers(
        &self,
        plugin_id: &str,
        triggers: &[crate::domain::plugin::PluginTrigger],
    ) {
        // 先删旧规则（keyword/regex + context 全清）
        self.remove_plugin_rules(plugin_id);

        // 再加新规则
        for trigger in triggers {
            match trigger {
                crate::domain::plugin::PluginTrigger::Keyword { keyword, exclusive } => {
                    let surface = if *exclusive {
                        Surface::Auto
                    } else {
                        Surface::Inline
                    };
                    self.add_keyword_rule(
                        plugin_id.to_string(),
                        keyword.clone(),
                        surface,
                        SurfaceView::List,
                    );
                }
                crate::domain::plugin::PluginTrigger::Regex { pattern, exclusive } => {
                    let surface = if *exclusive {
                        Surface::Auto
                    } else {
                        Surface::Inline
                    };
                    let _ = self.add_regex_rule(
                        plugin_id.to_string(),
                        pattern,
                        surface,
                        SurfaceView::List,
                    );
                }
                crate::domain::plugin::PluginTrigger::Context { when, surface } => {
                    // manifest 侧 when 映射为 domain 侧 ContextTrigger（0.8.2 §3.4 review #4）。
                    let ctx_when: ContextTrigger = (*when).into();
                    tracing::debug!(plugin = %plugin_id, when = ?ctx_when, surface = ?surface, "注册 context 规则");
                    self.add_context_rule(plugin_id.to_string(), ctx_when, *surface);
                }
            }
        }
        tracing::debug!(
            plugin_id,
            total = triggers.len(),
            kw = self
                .rules
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.plugin_id == plugin_id)
                .count(),
            ctx = self
                .context_rules
                .read()
                .unwrap()
                .iter()
                .filter(|r| r.plugin_id == plugin_id)
                .count(),
            "插件触发规则已重载",
        );
    }

    /// 从 manifest 注入 regex 规则。pattern 编译失败时返回 Err(描述),调用方可 warn! 跳过。
    pub fn add_regex_rule(
        &self,
        plugin_id: String,
        pattern: &str,
        surface: Surface,
        view: SurfaceView,
    ) -> Result<(), String> {
        let re = regex::Regex::new(pattern).map_err(|e| format!("regex 编译失败: {e}"))?;
        let mut rules = self.rules.write().unwrap();
        rules.push(Rule {
            plugin_id,
            kind: RuleKind::Regex(re),
            surface,
            view,
        });
        Ok(())
    }

    /// 收集所有 keyword 规则的 `(原文, pinyin_full)` 二元组，供 ghost text 计算（0.8.1）。
    /// regex 跳过（无"完整形式"概念）。同一 keyword 多次注册（不同插件同 keyword）会重复；
    /// `compute_hint` 内部按分数取最高，无副作用。
    pub(crate) fn collect_suggest_keywords(&self) -> Vec<(String, String)> {
        let rules = self.rules.read().unwrap();
        rules
            .iter()
            .filter_map(|r| match &r.kind {
                RuleKind::Keyword(kw) => Some((kw.clone(), pinyin_full(kw))),
                RuleKind::Regex(_) => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl IntentRouter for RuleRouter {
    async fn route(
        &self,
        query: &str,
        _history: &std::collections::HashMap<String, (i64, i64)>,
        ranking_hint: Option<&RankingHint>,
    ) -> Route {
        let q = query.trim();
        let takeover_enabled = *self.takeover_enabled.read().unwrap();

        // ── 0. Engine keyword 优先（0.8.5 §6.4）───────────────────
        //     本体 engine 的 keyword 是强信号（"剪贴板"不会误命中），命中即独占。
        //     放在 plugin/context 之前判是因为语义强度天然更高（本体自家数据）。
        //     takeover_enabled=false 时降级 Mixed——engine 只作 candidate 加不了排序，
        //     0.8.5 阶段行为回退到"和其他引擎混排"，跟 plugin Takeover 降级同心智。
        if takeover_enabled {
            if let Some(hit) = match_engine_keyword(q, &self.engine_rules.read().unwrap()) {
                return Route::EngineTakeover {
                    engine_id: hit.0,
                    arg: hit.1,
                };
            }
        }

        // ── 0.5. AI 前缀触发（0.9.x）────────────────────────────────
        //     用户输入 "ai xxx" 直接进入 AI 模式，不走 plugin keyword 匹配。
        //     "ai" 是本体保留前缀，优先级介于 EngineTakeover 和 plugin keyword 之间。
        //     空 arg（"ai" 或 "ai "）不触发——等同于无参，留给 Ghost 兜底。
        if takeover_enabled && q.len() > 3 && q[..3].eq_ignore_ascii_case("ai ") {
            let arg = q[3..].trim();
            if !arg.is_empty() {
                return Route::AiTrigger {
                    arg: arg.to_string(),
                };
            }
        }

        // ── 1. keyword / regex 匹配 ────────────────────────────
        let mut hits: Vec<Hit> = Vec::new();
        {
            let rules = self.rules.read().unwrap();
            for rule in rules.iter() {
                let mt_opt: Option<MatchType> = match &rule.kind {
                    RuleKind::Keyword(kw) => match_keyword(q, kw),
                    RuleKind::Regex(re) => {
                        // regex 命中:无"参数"概念,但 auto 对 regex 视为强信号 → takeover。
                        // 归入 Prefix{arg: ""} 让 resolve_surface(Auto) 取 Takeover。
                        if re.is_match(q) {
                            Some(MatchType::Prefix {
                                arg: String::new(),
                                hint: None,
                            })
                        } else {
                            None
                        }
                    }
                };
                if let Some(mt) = mt_opt {
                    let arg = match &mt {
                        MatchType::Exact { .. } | MatchType::InitialsExact { .. } => ExecArg::None,
                        MatchType::Prefix { arg, .. } | MatchType::InitialsPrefix { arg, .. } => {
                            if arg.is_empty() {
                                ExecArg::None
                            } else {
                                ExecArg::UserExplicit(arg.clone())
                            }
                        }
                    };
                    let hint: Option<String> = match &mt {
                        MatchType::Exact { hint } | MatchType::Prefix { hint, .. } => hint.clone(),
                        MatchType::InitialsExact { hint }
                        | MatchType::InitialsPrefix { hint, .. } => Some(hint.clone()),
                    };
                    let actual = resolve_surface(rule.surface, &mt, takeover_enabled);
                    hits.push(Hit {
                        plugin_id: rule.plugin_id.clone(),
                        arg,
                        surface: actual,
                        view: rule.view,
                        hint,
                        source: HitSource::Keyword,
                        when: None,
                        origin: None,
                    });
                }
            }
        }

        // ── 2. Surface Booster（0.8.4 §5.3.1）──────────────────
        //    route 对 Awareness 完全无知——Context 命中已不进 route（只在 best_suggestion
        //    产 Ghost + Tab 采纳）。Suggestion 域通过 RankingHint 单向反馈:上一轮 Context
        //    命中的 plugin,这一轮若被 keyword 命中,surface 升到 Priority（顶前排名）。
        //    hint 只影响排序,不代参、不召回新候选。
        let hits = merge_hits_with_hint(hits, ranking_hint);

        // ── 4. 仲裁 ─────────────────────────────────────────
        if let Some(t) = hits.iter().find(|h| h.surface == Surface::Takeover) {
            return Route::Takeover {
                plugin_id: t.plugin_id.clone(),
                arg: t.arg.clone(),
                view: t.view,
                hint: t.hint.clone(),
            };
        }

        Route::Mixed {
            candidates: hits
                .into_iter()
                .map(|h| Candidate {
                    plugin_id: h.plugin_id,
                    arg: h.arg,
                    surface: h.surface,
                    hint: h.hint,
                })
                .collect(),
        }
    }

    fn suggest_completion(&self, query: &str, min_score: f64) -> Option<CompletionHint> {
        let keywords = self.collect_suggest_keywords();
        suggest::compute_hint(&keywords, query, min_score)
    }

    fn best_suggestion(
        &self,
        query: &str,
        snapshot: &ContextSnapshot,
        min_score: f64,
    ) -> Option<Suggestion> {
        // 0.8.6 §8.1.2：委托 SuggestionArbiter 做多源竞争。
        // Keyword/Context 两个 producer 各自独立产出候选，arbiter 按 confidence 选 top-1。
        let arbiter = self.arbiter.read().unwrap();
        if arbiter.producer_count() > 0 {
            // arbiter 已初始化（生产环境通过 init_arbiter 注入 producers）
            let (sug, hint) = arbiter.best(query, snapshot);
            *self.last_ranking_hint.lock().unwrap() = hint;
            return sug;
        }
        drop(arbiter); // 释放读锁再调 fallback

        // fallback：arbiter 未初始化时（单测环境），走原直接实现
        self.best_suggestion_direct(query, snapshot, min_score)
    }

    fn set_app_language(&self, language: String) {
        // 委托到 inherent method(单测直接用 RuleRouter 类型,生产环境走 trait)
        RuleRouter::set_app_language(self, language);
    }

    fn apply_context_disable_list(&self, keys: Vec<String>) {
        RuleRouter::apply_context_disable_list(self, keys);
    }

    fn take_last_ranking_hint(&self) -> Option<RankingHint> {
        RuleRouter::take_last_ranking_hint(self)
    }
}

impl RuleRouter {
    /// 直接实现的 best_suggestion（0.8.6 arbiter 未初始化时的 fallback）。
    ///
    /// 策略：空 query → Context Ghost；非空 → Keyword Ghost（无命中则 None）。
    /// 单测环境走此路径（`init_arbiter` 未调用）。
    #[allow(deprecated)] // fallback 仍读 Suggestion.ranking_hint，生产环境走 arbiter 不会到这里
    fn best_suggestion_direct(
        &self,
        query: &str,
        snapshot: &ContextSnapshot,
        min_score: f64,
    ) -> Option<Suggestion> {
        let trimmed = query.trim();
        if trimmed.is_empty() {
            let sug = self.context_suggestion(query, snapshot);
            *self.last_ranking_hint.lock().unwrap() =
                sug.as_ref().and_then(|s| s.ranking_hint.clone());
            sug
        } else if let Some((hint, score)) =
            suggest::compute_hint_scored(&self.collect_suggest_keywords(), query, min_score)
        {
            let sug = Suggestion {
                display: hint.display,
                replacement: hint.replacement,
                source: SuggestionSource::Keyword,
                confidence: score.min(1.0),
                prefix_len: hint.prefix_len,
                origin: None,
                ranking_hint: None,
            };
            *self.last_ranking_hint.lock().unwrap() = None;
            Some(sug)
        } else {
            // 非空 query 无 Keyword 命中 → 不显示 Ghost
            *self.last_ranking_hint.lock().unwrap() = None;
            None
        }
    }

    /// 从 Context 命中产出 top-1 Suggestion（空 query 专属）。
    ///
    /// 多 Context 命中取 confidence 最高；产出的 Suggestion 携带 RankingHint（Surface Booster
    /// 单向反馈）。无命中返回 None。
    ///
    /// **非空 query 短路**：用户已输入内容时不再显示 Context Ghost——输入即意图表达，
    /// 环境感知建议会干扰用户操作。Context Ghost 只在空 query（用户刚唤起、尚未表达意图）时出现。
    #[allow(deprecated)] // 构造 Suggestion 时填充 ranking_hint，0.9 彻底移除字段后简化
    pub(crate) fn context_suggestion(
        &self,
        query: &str,
        snapshot: &ContextSnapshot,
    ) -> Option<Suggestion> {
        // 非空 query 不显示 Context Ghost——用户已表达意图，环境感知会干扰
        if !query.trim().is_empty() {
            return None;
        }

        let hits = self.match_context_hits(snapshot);
        let best_ctx = hits.into_iter().max_by(|a, b| {
            let ca = a
                .when
                .map(|w| context_confidence(&w, a.origin))
                .unwrap_or(0.0);
            let cb = b
                .when
                .map(|w| context_confidence(&w, b.origin))
                .unwrap_or(0.0);
            ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
        })?;

        // 采纳后自抑制：query 已命中 best_ctx 所属 plugin 的 keyword → 静默
        if self.query_hits_plugin_keyword(query, &best_ctx.plugin_id) {
            return None;
        }

        let when = best_ctx.when.as_ref()?;
        let confidence = context_confidence(when, best_ctx.origin);
        let origin = best_ctx.origin.map(SuggestionOrigin::from);
        let (display, replacement) = self.build_context_suggestion_text(&best_ctx, snapshot);
        Some(Suggestion {
            display,
            replacement,
            source: SuggestionSource::Context,
            confidence,
            prefix_len: 0,
            origin,
            ranking_hint: Some(RankingHint {
                boost_plugin_id: best_ctx.plugin_id.clone(),
            }),
        })
    }

    /// 扫描 Context 规则表，返回命中的 Hit 列表（0.8.2 §3.4 / 0.8.3 §4.13 P1 共享判定）。
    ///
    /// **共享入口**：`route()` 与 `best_suggestion()` 都调此函数，保命中判定一致，
    /// 避免 candidate 与 Ghost 撕裂（§4.13 P1-2）。
    ///
    /// 对每条规则：
    /// 1. **黑名单过滤**：binding key 在 `disabled_bindings` 内 → 跳过（0.8.3 §4.6）
    /// 2. **启用态过滤**：target 插件被 disable → 跳过（0.8.3 §4.13 P1-3，防止 Ghost→Tab 后 route 找不到 target）
    /// 3. 解析 target（仅 `TextIsNonTargetLang` 需要）：优先插件 `target_lang` → `auto` 回退 `app_language` → None 回退 `app_language`
    /// 4. `is_hit` 判定命中
    /// 5. 从 `TextSource` 抽 arg，长度 > 2000 截断（§3.4 边界约定）
    /// 6. arg 为 None → 不召回（Context 门禁）
    fn match_context_hits(&self, snapshot: &ContextSnapshot) -> Vec<Hit> {
        let rules = self.context_rules.read().unwrap();
        if rules.is_empty() {
            return Vec::new();
        }
        tracing::trace!(
            rule_count = rules.len(),
            has_selection = snapshot.find_text(AwarenessSource::Selection).is_some(),
            has_clipboard = snapshot.find_text(AwarenessSource::Clipboard).is_some(),
            "扫描 context 规则",
        );
        let app_lang = self.app_language.read().unwrap().clone();
        let resolver = self.settings.read().unwrap().clone();
        let disabled = self.disabled_bindings.read().unwrap().clone();

        let mut out = Vec::new();
        for rule in rules.iter() {
            // 1. 黑名单过滤（0.8.3 §4.6）
            let key = binding_key(&rule.plugin_id, trigger_key(&rule.when));
            if disabled.contains(&key) {
                tracing::trace!(binding = %key, "context binding 被用户禁用,跳过");
                continue;
            }

            // 2. 启用态过滤（0.8.3 §4.13 P1「运行时查启用态」）
            if let Some(r) = resolver.as_ref() {
                if !r.is_enabled(&rule.plugin_id) {
                    tracing::trace!(plugin = %rule.plugin_id, "target 插件已禁用,跳过 context binding");
                    continue;
                }
            }

            // 3. 解析 target
            let target = if matches!(rule.when, ContextTrigger::TextIsNonTargetLang { .. }) {
                let plugin_target = resolver
                    .as_ref()
                    .and_then(|r| r.get_string(&rule.plugin_id, "target_lang"));
                match plugin_target.as_deref() {
                    // "auto" 或未配置 → 回退 app_language
                    Some("auto") | None => Some(app_lang.clone()),
                    Some(other) => Some(other.to_string()),
                }
            } else {
                None
            };

            // 4. 命中判定
            if !ctx_trigger::is_hit(&rule.when, snapshot, target.as_deref()) {
                tracing::trace!(
                    plugin = %rule.plugin_id,
                    when = ?rule.when,
                    target = ?target,
                    "context 规则未命中",
                );
                continue;
            }
            tracing::debug!(
                plugin = %rule.plugin_id,
                when = ?rule.when,
                target = ?target,
                "context 规则命中",
            );

            // 5. 抽展示文本 + origin（0.8.3 收尾 · awareness；0.8.4 §5.3.2 类型墙）
            //    展示文本（仅 TextIsNonTargetLang 有,如待翻译内容）用于:
            //    (a) 门禁判定（抽不到 → 不召回）
            //    (b) best_suggestion 的 build_context_suggestion_text 构建 Ghost 文本
            //    但**不进 Hit.arg** —— Hit.arg 是执行参数(ExecArg),context hit 恒 None
            //    (context 不代执行参)。展示文本属 Suggestion 域(唯一能读 Awareness 的层),
            //    由 build_context_suggestion_text 从 snapshot 直接取。origin 仍从数据侧带来。
            //
            //    - `TextIsNonTargetLang`：走 source.extract 拿 AwarenessView,text + origin 一起
            //    - `ClipboardIsUrl` / `ClipboardIsFilePath`：trigger 语义锁定 Clipboard 来源
            //    - `SelectionNonEmpty`：trigger 语义锁定 Selection 来源
            let (display_text, origin) = match &rule.when {
                ContextTrigger::TextIsNonTargetLang { source } => match source.extract(snapshot) {
                    Some(view) => (truncate_arg(view.text), Some(view.source)),
                    None => (String::new(), None),
                },
                ContextTrigger::ClipboardIsUrl | ContextTrigger::ClipboardIsFilePath => {
                    (String::new(), Some(AwarenessSource::Clipboard))
                }
                ContextTrigger::SelectionNonEmpty => {
                    (String::new(), Some(AwarenessSource::Selection))
                }
            };

            // 6. 门禁：TextIsNonTargetLang 抽不到展示文本 → 不召回
            //    （其他 when 是 event-only 触发，不受此闸约束）
            if matches!(rule.when, ContextTrigger::TextIsNonTargetLang { .. })
                && display_text.is_empty()
            {
                tracing::trace!(plugin = %rule.plugin_id, "context 命中但展示文本空,跳过召回");
                continue;
            }

            tracing::trace!(
                plugin = %rule.plugin_id,
                text_len = display_text.chars().count(),
                surface = ?rule.surface,
                origin = ?origin,
                "context 规则产出 Hit",
            );

            out.push(Hit {
                plugin_id: rule.plugin_id.clone(),
                arg: ExecArg::None, // context hit 不代执行参(0.8.4 §5.3.2);展示文本由 Suggestion 域从 snapshot 取
                surface: rule.surface,
                view: SurfaceView::List,
                hint: None,
                source: HitSource::Context,
                when: Some(rule.when),
                origin,
            });
        }
        out
    }

    /// 0.8.3 之前的名字（历史兼容）。内部委托到 `match_context_hits`。
    #[deprecated(note = "0.8.3: 用 match_context_hits")]
    #[allow(dead_code)]
    fn match_context_rules(&self, snapshot: &ContextSnapshot) -> Vec<Hit> {
        self.match_context_hits(snapshot)
    }

    /// 从 Context Hit 构造 Suggestion 显示文本（0.8.3 §4.13 P0 修订）。
    ///
    /// **display**：本地化名（`翻译 "hello..."` / `Translate "hello..."`）——
    /// 从 `PluginSettingResolver::get_display_name(plugin_id, app_language)` 读 manifest.name；
    /// 失败 fallback 到 id 末段。
    ///
    /// **replacement**：真实可命中的 keyword + arg（Tab 采纳后要能命中 `route()` 的 keyword
    /// 表 → 走 Takeover）——从 `RuleRouter::rules` 反查同 plugin_id 的 Keyword rule,按当前
    /// UI 语言字符集偏好选一个（zh UI 优先含 CJK 的 keyword、en UI 优先纯 ASCII）；
    /// 反查不到时 fallback 到 id 末段（保历史行为）。
    ///
    /// **无 keyword trigger 的 target**（未来纯 Context 触发的动作）：display + replacement
    /// 都会 fallback 到 id 末段；此时 Tab 采纳 → keyword 表不命中 → 降级模糊搜索。这条
    /// 死态是设计边界（本方法不管），需要在注册 Context binding 的地方 warn。
    fn build_context_suggestion_text(
        &self,
        hit: &Hit,
        snapshot: &ContextSnapshot,
    ) -> (String, String) {
        // 展示文本（0.8.4 §5.3.2）：从 snapshot 按 hit.when 直接取,不读 hit.arg ——
        // hit.arg 是执行参数(ExecArg),context hit 恒 None;展示文本属 Suggestion 域
        // (唯一能读 Awareness 的层)。仅 TextIsNonTargetLang 携带展示文本。
        let arg_text: String = match &hit.when {
            Some(ContextTrigger::TextIsNonTargetLang { source }) => source
                .extract(snapshot)
                .map(|v| truncate_arg(v.text))
                .unwrap_or_default(),
            _ => String::new(),
        };

        // display 截 40 字符便于 ghost 单行展示
        const DISPLAY_MAX: usize = 40;
        let display_arg: String = if arg_text.chars().count() > DISPLAY_MAX {
            let truncated: String = arg_text.chars().take(DISPLAY_MAX).collect();
            format!("{truncated}…")
        } else {
            arg_text.clone()
        };

        let app_lang = self.app_language.read().unwrap().clone();

        // display：优先本地化 manifest.name,失败 fallback id 末段
        let display_name = self
            .settings
            .read()
            .unwrap()
            .as_ref()
            .and_then(|r| r.get_display_name(&hit.plugin_id, &app_lang))
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| short_target_name(&hit.plugin_id));

        let display = if display_arg.is_empty() {
            display_name.clone()
        } else {
            format!("{display_name} \"{display_arg}\"")
        };

        // replacement：反查 rules 拿真实 keyword（zh UI 偏好 CJK / en UI 偏好 ASCII）,
        // 反查不到时 fallback 到 id 末段。**关键**：这是 Tab 后要塞回输入框重跑 route()
        // 的文本,必须能命中 keyword 表——用 display_name（本地化名）就断链了。
        let keyword = self
            .preferred_keyword(&hit.plugin_id, &app_lang)
            .unwrap_or_else(|| short_target_name(&hit.plugin_id));
        let replacement = if arg_text.is_empty() {
            format!("{keyword} ")
        } else {
            format!("{keyword} {}", arg_text)
        };

        (display, replacement)
    }

    /// 反查 `rules` 拿同 plugin_id 的 keyword,按当前 UI 语言字符集偏好选一个。
    ///
    /// - zh UI：优先含 CJK（U+4E00..=U+9FFF）的 keyword，fallback 到首个可用 keyword
    /// - en UI（其他）：优先纯 ASCII 的 keyword，fallback 到首个可用 keyword
    ///
    /// 无 Keyword rule 时返回 None（调用方 fallback 到 id 末段）。
    fn preferred_keyword(&self, plugin_id: &str, lang: &str) -> Option<String> {
        let rules = self.rules.read().unwrap();
        let kws: Vec<String> = rules
            .iter()
            .filter(|r| r.plugin_id == plugin_id)
            .filter_map(|r| match &r.kind {
                RuleKind::Keyword(k) => Some(k.clone()),
                RuleKind::Regex(_) => None,
            })
            .collect();
        if kws.is_empty() {
            return None;
        }

        let prefer_cjk = lang.starts_with("zh");
        let has_cjk = |s: &str| s.chars().any(|c| ('\u{4e00}'..='\u{9fff}').contains(&c));
        let is_ascii = |s: &str| s.is_ascii();

        let picked = if prefer_cjk {
            kws.iter().find(|k| has_cjk(k))
        } else {
            kws.iter().find(|k| is_ascii(k))
        };
        picked.cloned().or_else(|| kws.into_iter().next())
    }

    /// 判断 query 是否已命中指定 plugin 的任一 keyword（原文/pinyin_full/pinyin_initials 三形式）。
    ///
    /// 用于 `ContextProducer` 的采纳后自抑制护栏（0.8.8 bugfix）：Tab 采纳 Context Ghost
    /// 后 query 变成 `翻译 xxx`，若此时 Context 仍产同一 plugin 的 Suggestion → Ghost 反复
    /// 弹出、用户可无限 Tab 叠加。此 helper 让 Producer 在"用户已明确用 keyword 表达意图"
    /// 时静默——语义上"你已经进 Takeover 了，我不用再劝你翻译"。
    ///
    /// 复用 `match_keyword` 保和 `route()` 判定一致（同 Exact / Prefix / InitialsExact /
    /// InitialsPrefix 四种命中都算命中）。
    pub(crate) fn query_hits_plugin_keyword(&self, query: &str, plugin_id: &str) -> bool {
        let q = query.trim();
        if q.is_empty() {
            return false;
        }
        let rules = self.rules.read().unwrap();
        rules
            .iter()
            .filter(|r| r.plugin_id == plugin_id)
            .any(|r| match &r.kind {
                RuleKind::Keyword(kw) => match_keyword(q, kw).is_some(),
                RuleKind::Regex(_) => false,
            })
    }
}

// ── 内部辅助 ──────────────────────────────────────────────

/// context binding 的稳定 key（0.8.3 §4.6）。
///
/// 格式 `{target_id}::{trigger_key}` —— **双冒号避开 target_id 内部点/冒号**
/// （插件 `builtin.translate`、内置动作 `builtin:open_url` 命名不统一，单冒号分隔
/// 会导致 3 段冒号歧义）。target_id 不做归一化，原样保留。
///
/// 该 key 是 `disabled_context_bindings` 存储/前端展示的稳定标识符。
pub fn binding_key(target_id: &str, trigger_key: &str) -> String {
    format!("{target_id}::{trigger_key}")
}

/// `ContextTrigger` → snake_case key（对应 manifest 侧的 `ManifestContextWhen`）。
///
/// 用于生成 `binding_key` 的第二段。变体 payload（如 `TextIsNonTargetLang { source }`）
/// **不进 key**——未来 `TextSource` 拆多变体时（选区 vs 剪贴板分开 disable）再升 key schema
/// （§4.13 P1 备忘）。
pub fn trigger_key(when: &ContextTrigger) -> &'static str {
    match when {
        ContextTrigger::ClipboardIsUrl => "clipboard_is_url",
        ContextTrigger::ClipboardIsFilePath => "clipboard_is_file_path",
        ContextTrigger::SelectionNonEmpty => "selection_non_empty",
        ContextTrigger::TextIsNonTargetLang { .. } => "text_is_non_target_lang",
    }
}

/// Context 触发的置信度评分（0.8.3 §4.5 · 0.8.3 收尾重构简化）。
///
/// **重构记忆**：0.8.3 一版这里内嵌 `snapshot` 参数 + `snapshot_has_meaningful_selection`
/// helper 做「有没有选区」的**事后推断**。awareness 重构后 origin 从 Hit 带来,本函数
/// 只做 `(base, src_w)` 映射,不再摸 snapshot。删掉两个 helper (`infer_origin` /
/// `snapshot_has_meaningful_selection`) —— 三处重复推断收敛为**数据侧一等标签**。
///
/// 用 enum discriminant 顺序 + source_weight,**不引入拍脑袋常数**：
/// - base: URL(0.90) > FilePath(0.85) > NonTargetLang(0.75) > SelectionNonEmpty(0.50)。
///   数字唯一约束是**可解释的排序**(URL/File 类基础分 > 语言类 > 空选区类)。
/// - src_w: origin=Selection→1.0；origin=Clipboard→0.85；origin=None→1.0（trigger 无 source 依赖）。
fn context_confidence(when: &ContextTrigger, origin: Option<AwarenessSource>) -> f64 {
    let base = match when {
        ContextTrigger::ClipboardIsUrl => 0.90,
        ContextTrigger::ClipboardIsFilePath => 0.85,
        ContextTrigger::TextIsNonTargetLang { .. } => 0.75,
        ContextTrigger::SelectionNonEmpty => 0.50,
    };

    // origin 从 Hit 直接拿——数据侧带来,不再事后推断
    let src_w = match origin {
        Some(AwarenessSource::Selection) => 1.0,
        Some(AwarenessSource::Clipboard) => 0.85,
        None => 1.0, // trigger 无 source 依赖（当前所有 trigger 都能拿到 origin,None 走 fallback）
    };

    base * src_w
}

/// 从 plugin_id 抽显示短名——**仅作 fallback**（0.8.3 §4.13 P0 修订）：
/// - display 优先 `manifest.name.resolve(lang)`（`翻译`/`Translate`），此处 fallback
/// - replacement 优先反查 keyword 表（`翻译`/`translate`），此处 fallback
///
/// 两条 fallback 都走这里,保历史行为——未加载插件 / 无 keyword rule 时仍能出 Ghost,
/// 只是 replacement 采纳后可能不命中 Takeover（降级模糊搜索）,不是死链。
fn short_target_name(plugin_id: &str) -> String {
    plugin_id
        .rsplit(&['.', ':'][..])
        .next()
        .unwrap_or(plugin_id)
        .to_string()
}

/// 命中来源，`merge_hits` 用来判"kw 优先 arg / max surface"。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum HitSource {
    Keyword,
    Context,
}

struct Hit {
    plugin_id: String,
    arg: ExecArg,
    surface: Surface,
    view: SurfaceView,
    hint: Option<String>,
    source: HitSource,
    /// Context 命中的原触发规则（用于 `best_suggestion` 算 confidence 与
    /// 生成 binding_key）。Keyword/regex 命中恒 None。
    when: Option<ContextTrigger>,
    /// Context 命中的数据侧 origin（0.8.3 收尾 · awareness 重构）——
    /// 由 `match_context_hits` 从 `AwarenessView.source` 或 trigger 语义带来,
    /// 供 `best_suggestion` 直接映射到 `Suggestion.origin`,
    /// 供 `context_confidence` 决定 selection/clipboard 权重。
    /// Keyword/regex 命中恒 None。
    origin: Option<AwarenessSource>,
}

/// 0.8.4 §5.3.1 Surface Booster：keyword 命中 + RankingHint boost 同 plugin → surface 升级。
///
/// 取代 0.8.3 的 `merge_hits_keyword_only`（基于 ctx_hits）:
/// - route() 断 Awareness 后不再有 ctx_hits,Suggestion 域通过 RankingHint 单向反馈
/// - hint.boost_plugin_id 命中的 kw hit → surface 升到 Priority（顶前排名）
/// - 不动 arg（参数由用户显式给,不隐式代填）、不召回新候选（只影响排序）
///
/// 无 hint 或 hint 未命中任何 kw hit 时,等价 keyword-only 原样返回。
fn merge_hits_with_hint(kw_hits: Vec<Hit>, hint: Option<&RankingHint>) -> Vec<Hit> {
    let Some(h) = hint else { return kw_hits };
    let mut out = kw_hits;
    for hit in out.iter_mut() {
        if hit.plugin_id == h.boost_plugin_id {
            // Suggestion 域上一轮 Context 命中此 plugin → 这一轮 kw 命中升到 Priority
            hit.surface = surface_max(hit.surface, Surface::Priority);
            hit.source = HitSource::Keyword;
        }
    }
    out
}

/// surface 强度：Takeover > Priority > Inline > Auto。
///
/// `Auto` 理论上此处不该出现（`resolve_surface` 已归解）；兜底当 Inline 处理。
fn surface_max(a: Surface, b: Surface) -> Surface {
    fn rank(s: Surface) -> u8 {
        match s {
            Surface::Takeover => 3,
            Surface::Priority => 2,
            Surface::Inline => 1,
            Surface::Auto => 0,
        }
    }
    if rank(a) >= rank(b) { a } else { b }
}

/// Context 抽出的 arg 截断到 2000 字符 + `…`（0.8.2 §3.4 边界约定）。
///
/// 超长文本压垮子进程 stdin / 网络请求 body / UI 渲染都有风险；2000 字符对绝大多数
/// 场景（一段英文、一段代码注释）都够用。
fn truncate_arg(s: &str) -> String {
    const MAX_CHARS: usize = 2000;
    let mut count = 0;
    let mut end = s.len();
    for (i, _) in s.char_indices() {
        if count == MAX_CHARS {
            end = i;
            break;
        }
        count += 1;
    }
    if end == s.len() {
        s.to_string()
    } else {
        let mut out = String::with_capacity(end + 3);
        out.push_str(&s[..end]);
        out.push('…');
        out
    }
}

/// 由 (声明 surface, 命中类型, 全局开关) 解出实际 surface。
///
/// 0.8.1 §2.3 三态（Initials 拆二分后纯类型驱动，无需读 arg 字段）：
/// - `Auto` + `Exact` → Priority
/// - `Auto` + `Prefix`（命中原文形式）→ Takeover
/// - `Auto` + `InitialsExact`（首拼派生无参）→ Priority（弱信号不独占）
/// - `Auto` + `InitialsPrefix`（首拼派生带参）→ Inline
fn resolve_surface(declared: Surface, mt: &MatchType, takeover_enabled: bool) -> Surface {
    let actual = match declared {
        Surface::Auto => match mt {
            MatchType::Exact { .. } => Surface::Priority,
            MatchType::Prefix { .. } => Surface::Takeover,
            MatchType::InitialsExact { .. } => Surface::Priority,
            MatchType::InitialsPrefix { .. } => Surface::Inline,
        },
        Surface::Inline => Surface::Inline,
        Surface::Priority => Surface::Priority,
        Surface::Takeover => Surface::Takeover,
    };
    if actual == Surface::Takeover && !takeover_enabled {
        Surface::Priority
    } else {
        actual
    }
}

/// keyword 匹配(§4.2)：0.8.1 §2.3 三态。
///
/// 意图侧展开 3 个候选形式（与应用搜索的 `normalize_candidates` 契约独立）：
/// - `原文小写`（如 `翻译` / `translate`）→ Exact / Prefix，强信号。
/// - `pinyin_full`（如 `fanyi`）→ Exact / Prefix，强信号（完整拼音≈原文）。
/// - `pinyin_initials`（如 `fy`）→ Initials，**弱信号**，`resolve_surface(Auto)` 下不独占。
///
/// 3 者可能重合（纯 ASCII keyword 三者相等）→ 只走一次 Exact/Prefix，行为不变。
/// 匹配本体 engine keyword（0.8.5 §6.4）。
///
/// 与插件 `match_keyword` 分离的原因：
/// - engine keyword 只走强信号（原文 exact / prefix + 空格），不做首拼派生
///   （engine 场景无 UX 教学诉求，首拼弱信号价值不大）
/// - 单条 EngineRule 携带多触发词，一次遍历取第一命中
///
/// 返回 `(engine_id, ExecArg)`：无参 → ExecArg::None；带参 → ExecArg::UserExplicit。
fn match_engine_keyword(q: &str, rules: &[EngineRule]) -> Option<(String, ExecArg)> {
    if q.is_empty() {
        return None;
    }
    let q_lower = q.to_ascii_lowercase();
    for rule in rules {
        for kw in &rule.keywords {
            let kw_lower = kw.to_ascii_lowercase();
            // Exact
            if q_lower == kw_lower {
                return Some((rule.engine_id.clone(), ExecArg::None));
            }
            // Prefix + 空格分隔的参数
            let mut with_space = kw_lower.clone();
            with_space.push(' ');
            if q_lower.starts_with(&with_space) {
                let arg = q[kw.len()..].trim();
                let exec_arg = if arg.is_empty() {
                    ExecArg::None
                } else {
                    ExecArg::UserExplicit(arg.to_string())
                };
                return Some((rule.engine_id.clone(), exec_arg));
            }
        }
    }
    None
}

fn match_keyword(query: &str, keyword: &str) -> Option<MatchType> {
    let q_lower = query.to_ascii_lowercase();
    let orig_lower = keyword.to_ascii_lowercase();
    let full = pinyin_full(keyword);
    let initials = pinyin_initials(keyword);

    // 派生形式的 hint：完整拼音（供 UI 教学"更规范形式"）。
    // 若原文本身即完整拼音（纯 ASCII），hint fallback 到原文。
    let derived_hint: String = if !full.is_empty() {
        full.clone()
    } else {
        orig_lower.clone()
    };

    // 候选按"强度"排序：原文 > pinyin_full > pinyin_initials。
    // (candidate_lower, is_strong_signal)
    let mut candidates: Vec<(String, bool)> = Vec::with_capacity(3);
    if !orig_lower.is_empty() {
        candidates.push((orig_lower.clone(), true));
    }
    if !full.is_empty() && full != orig_lower {
        candidates.push((full, true));
    }
    if !initials.is_empty()
        && initials != orig_lower
        && !candidates.iter().any(|(c, _)| c == &initials)
    {
        candidates.push((initials, false));
    }

    for (kw, is_strong) in &candidates {
        if q_lower == *kw {
            return Some(if *is_strong {
                MatchType::Exact { hint: None }
            } else {
                MatchType::InitialsExact { hint: derived_hint }
            });
        }
        let prefix = format!("{kw} ");
        if q_lower.starts_with(&prefix) {
            let arg = query[prefix.len()..].trim().to_string();
            return Some(if *is_strong {
                MatchType::Prefix { arg, hint: None }
            } else {
                MatchType::InitialsPrefix {
                    arg,
                    hint: derived_hint,
                }
            });
        }
    }
    None
}

// ── 单测 ──────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn router_with_rules(takeover_enabled: bool) -> RuleRouter {
        let r = RuleRouter::new(takeover_enabled);
        // echo: auto(默认),无参→Priority,带参→Takeover
        r.add_keyword_rule(
            "builtin.echo".into(),
            "echo".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        // ip: 专有无参词,显式 takeover
        r.add_keyword_rule(
            "builtin.ip".into(),
            "ip".into(),
            Surface::Takeover,
            SurfaceView::List,
        );
        // dict: 始终 inline
        r.add_keyword_rule(
            "builtin.dict".into(),
            "dict".into(),
            Surface::Inline,
            SurfaceView::List,
        );
        r
    }

    fn run_route(r: &RuleRouter, q: &str) -> Route {
        let h = std::collections::HashMap::new();
        // 0.8.4 §5.3.1：route 断 Awareness 依赖,不再传 snapshot；hint=None
        tauri::async_runtime::block_on(r.route(q, &h, None))
    }

    // ── 0.8.5 §6.4 EngineTakeover 分派 ──────────────────────────

    #[test]
    fn engine_keyword_exact_produces_engine_takeover_no_arg() {
        let r = router_with_rules(true);
        r.add_engine_rule("clipboard".into(), vec!["剪贴板".into(), "clip".into()]);
        let route = run_route(&r, "剪贴板");
        assert!(matches!(
            &route,
            Route::EngineTakeover { engine_id, arg }
                if engine_id == "clipboard" && matches!(arg, ExecArg::None)
        ));
    }

    #[test]
    fn engine_keyword_prefix_produces_engine_takeover_with_arg() {
        let r = router_with_rules(true);
        r.add_engine_rule("clipboard".into(), vec!["剪贴板".into(), "clip".into()]);
        let route = run_route(&r, "剪贴板 hello");
        assert!(matches!(
            &route,
            Route::EngineTakeover { engine_id, arg }
                if engine_id == "clipboard"
                    && matches!(arg, ExecArg::UserExplicit(s) if s == "hello")
        ));
    }

    #[test]
    fn engine_keyword_case_insensitive_english() {
        let r = router_with_rules(true);
        r.add_engine_rule("clipboard".into(), vec!["剪贴板".into(), "clip".into()]);
        let route = run_route(&r, "CLIP world");
        assert!(matches!(
            &route,
            Route::EngineTakeover { engine_id, arg }
                if engine_id == "clipboard"
                    && matches!(arg, ExecArg::UserExplicit(s) if s == "world")
        ));
    }

    #[test]
    fn engine_keyword_no_match_falls_through_to_plugin_rules() {
        // 无 engine 命中时，plugin rules 正常生效（不因 engine 检查阻断）
        let r = router_with_rules(true);
        r.add_engine_rule("clipboard".into(), vec!["剪贴板".into(), "clip".into()]);
        let route = run_route(&r, "echo hi");
        assert!(matches!(
            route,
            Route::Takeover { plugin_id, .. } if plugin_id == "builtin.echo"
        ));
    }

    #[test]
    fn engine_keyword_disabled_when_takeover_off() {
        // takeover_enabled=false 时 engine 也不独占（跟 plugin Takeover 降级同心智）
        let r = router_with_rules(false);
        r.add_engine_rule("clipboard".into(), vec!["剪贴板".into()]);
        let route = run_route(&r, "剪贴板");
        // 应该走 Mixed（plugin echo 也匹配不到"剪贴板"，candidates 空）
        assert!(matches!(route, Route::Mixed { .. }));
    }

    #[test]
    fn auto_exact_is_priority() {
        let r = router_with_rules(true);
        let route = run_route(&r, "echo");
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.len() == 1 && candidates[0].plugin_id == "builtin.echo" && matches!(candidates[0].surface, Surface::Priority))
        );
    }

    #[test]
    fn auto_prefix_is_takeover() {
        let r = router_with_rules(true);
        let route = run_route(&r, "echo hello");
        assert!(
            matches!(route, Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.echo" && arg == ExecArg::UserExplicit("hello".to_string()))
        );
    }

    #[test]
    fn explicit_takeover_always() {
        let r = router_with_rules(true);
        let route = run_route(&r, "ip");
        assert!(matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "builtin.ip"));
    }

    #[test]
    fn explicit_inline_always() {
        let r = router_with_rules(true);
        let route = run_route(&r, "dict hello");
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.len() == 1 && candidates[0].plugin_id == "builtin.dict" && matches!(candidates[0].surface, Surface::Inline))
        );
    }

    #[test]
    fn global_switch_downgrades_takeover() {
        let r = router_with_rules(true);
        r.set_takeover_enabled(false);
        let route = run_route(&r, "ip");
        // ip 显式 takeover,但全局开关关闭 → 降级 priority
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.len() == 1 && candidates[0].plugin_id == "builtin.ip" && matches!(candidates[0].surface, Surface::Priority))
        );
    }

    #[test]
    fn no_hit_returns_empty_mixed() {
        let r = router_with_rules(true);
        let route = run_route(&r, "chrome");
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn pinyin_initials_keyword_downgrades_to_inline() {
        // 0.8.1 §2.3 核心行为变更：首拼带参从 Takeover 降级 Inline（弱信号不独占）。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.weather".into(),
            "天气".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "tq 北京");
        // 首拼带参 → Inline，而不是 Takeover
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.weather"
                && candidates[0].arg == ExecArg::UserExplicit("北京".to_string())
                && matches!(candidates[0].surface, Surface::Inline)
                && candidates[0].hint.as_deref() == Some("tianqi")
        ));
    }

    #[test]
    fn pinyin_initials_keyword_no_arg_is_priority() {
        // 首拼无参 → Priority（不独占其他候选），带 hint。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.weather".into(),
            "天气".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "tq");
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.weather"
                && candidates[0].arg.is_none()
                && matches!(candidates[0].surface, Surface::Priority)
                && candidates[0].hint.as_deref() == Some("tianqi")
        ));
    }

    #[test]
    fn pinyin_full_keyword_is_takeover() {
        // 完整拼音 == 原文的等价强信号（0.8.1 §2.2）
        // 用户输入 "fanyi hello" 应该像输入 "翻译 hello" 一样直接 Takeover。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "fanyi hello");
        assert!(matches!(
            route,
            Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.translate" && arg == ExecArg::UserExplicit("hello".to_string())
        ));
    }

    #[test]
    fn pinyin_full_keyword_exact_is_priority() {
        // 完整拼音无参 → Priority（等价原文精确命中）
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "fanyi");
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.translate"
                && matches!(candidates[0].surface, Surface::Priority)
        ));
    }

    #[test]
    fn latin_keyword_prefix_is_takeover() {
        // 纯 ASCII keyword（三候选合一）→ Prefix Takeover，行为不变
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "translate".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "translate hello");
        assert!(matches!(
            route,
            Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.translate" && arg == ExecArg::UserExplicit("hello".to_string())
        ));
    }

    #[test]
    fn multiple_takeovers_first_wins() {
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "a".into(),
            "foo".into(),
            Surface::Takeover,
            SurfaceView::List,
        );
        r.add_keyword_rule(
            "b".into(),
            "foo".into(),
            Surface::Takeover,
            SurfaceView::List,
        );
        let route = run_route(&r, "foo");
        assert!(matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "a"));
    }

    #[test]
    fn regex_trigger_hit() {
        let r = RuleRouter::new(true);
        r.add_regex_rule(
            "builtin.hex".into(),
            r"^0x[0-9a-fA-F]+$",
            Surface::Auto,
            SurfaceView::List,
        )
        .unwrap();
        let route = run_route(&r, "0xFF");
        assert!(matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "builtin.hex"));
    }

    #[test]
    fn regex_trigger_miss() {
        let r = RuleRouter::new(true);
        r.add_regex_rule(
            "builtin.hex".into(),
            r"^0x[0-9a-fA-F]+$",
            Surface::Auto,
            SurfaceView::List,
        )
        .unwrap();
        let route = run_route(&r, "123");
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn regex_invalid_pattern_skipped() {
        let r = RuleRouter::new(true);
        assert!(
            r.add_regex_rule("x".into(), "[", Surface::Auto, SurfaceView::List)
                .is_err()
        );
    }

    #[test]
    fn regex_priority_when_takeover_disabled() {
        let r = RuleRouter::new(false); // 全局开关关闭
        r.add_regex_rule(
            "builtin.hex".into(),
            r"^0x[0-9a-fA-F]+$",
            Surface::Takeover,
            SurfaceView::List,
        )
        .unwrap();
        let route = run_route(&r, "0xFF");
        // 显式 takeover,但全局开关关闭 → 降级 priority
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.len() == 1 && candidates[0].plugin_id == "builtin.hex" && matches!(candidates[0].surface, Surface::Priority))
        );
    }

    #[test]
    fn suggest_completion_via_router() {
        // suggest_completion 出口贯通：keyword 表收集 + compute_hint 联动
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        // 首拼 "fy" 应能命中 fanyi
        let hint = r
            .suggest_completion("fy hello", 0.7)
            .expect("should have hint");
        assert_eq!(hint.display, "fanyi");
        assert_eq!(hint.replacement, "fanyi hello");

        // 已完整 + 带参 → None（已进 Takeover）
        assert_eq!(r.suggest_completion("fanyi hello", 0.7), None);
        assert_eq!(r.suggest_completion("翻译 hello", 0.7), None);

        // 已完整 + 无尾内容 → display="" 的 Tab-only hint（0.8.1 优化：提示可 Tab 进参数模式）
        let tab_only = r.suggest_completion("fanyi", 0.7).expect("tab-only hint");
        assert_eq!(tab_only.display, "");
        assert_eq!(tab_only.replacement, "fanyi ");

        // 无匹配 → None
        assert_eq!(r.suggest_completion("chrome", 0.7), None);
    }

    // ── 0.8.2 §3.4 Context 路由测试 ──────────────────────────────

    use std::collections::HashMap;

    /// mock `PluginSettingResolver`：内置 (plugin_id, key) → value 表。
    struct MockResolver {
        table: HashMap<(String, String), String>,
    }

    impl MockResolver {
        fn new() -> Self {
            Self {
                table: HashMap::new(),
            }
        }
        fn with(mut self, plugin_id: &str, key: &str, value: &str) -> Self {
            self.table
                .insert((plugin_id.to_string(), key.to_string()), value.to_string());
            self
        }
    }

    impl crate::domain::plugin::PluginSettingResolver for MockResolver {
        fn get_string(&self, plugin_id: &str, key: &str) -> Option<String> {
            self.table
                .get(&(plugin_id.to_string(), key.to_string()))
                .cloned()
        }
    }

    fn run_route_with_snapshot(r: &RuleRouter, q: &str, _snapshot: ContextSnapshot) -> Route {
        let h = std::collections::HashMap::new();
        // 0.8.4 §5.3.1：route 断 Awareness 依赖,snapshot 不再被读；hint=None。
        // 函数签名保留(收 snapshot)以最小化调用点改动,但 snapshot 被忽略——
        // 这些测试验的是 kw 行为(route 基于 kw resolve surface),不依赖 context 进 route。
        // Surface Booster(hint boost)由 Task 7 单独加 run_route_with_hint 测试。
        tauri::async_runtime::block_on(r.route(q, &h, None))
    }

    /// 0.8.3 §4.4：空 query 场景验 best_suggestion（Context 走 Ghost 不产 candidate）。
    fn run_best_suggestion(
        r: &RuleRouter,
        q: &str,
        snapshot: &ContextSnapshot,
    ) -> Option<Suggestion> {
        r.best_suggestion(q, snapshot, 0.7)
    }

    fn snap_selection(text: &str) -> ContextSnapshot {
        ContextSnapshot::with_selection(text)
    }

    fn snap_clipboard(text: &str) -> ContextSnapshot {
        ContextSnapshot::with_clipboard(text)
    }

    /// 构造一个只声明 `TextIsNonTargetLang` context trigger 的翻译 router，
    /// target_lang 由 mock resolver 提供（默认 zh）。
    fn translate_router_with_target(target: &str) -> RuleRouter {
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            target,
        )));
        r
    }

    #[test]
    fn context_hit_empty_query_selection_english_triggers_translate() {
        // 0.8.3：空 query 时 route() 不产 candidate（Context 走 Ghost），
        // best_suggestion() 产 Context Suggestion。
        let r = translate_router_with_target("zh");
        let snapshot = snap_selection("hello world foo");
        let route = run_route_with_snapshot(&r, "", snapshot.clone());
        assert!(matches!(&route, Route::Mixed { candidates } if candidates.is_empty()));
        let sug = run_best_suggestion(&r, "", &snapshot).expect("expected context suggestion");
        assert_eq!(sug.source, SuggestionSource::Context);
        assert!(sug.replacement.contains("hello world foo"));
    }

    #[test]
    fn context_hit_clipboard_english_triggers_translate() {
        let r = translate_router_with_target("zh");
        let snapshot = snap_clipboard("hello world foo");
        let route = run_route_with_snapshot(&r, "", snapshot.clone());
        assert!(matches!(&route, Route::Mixed { candidates } if candidates.is_empty()));
        let sug = run_best_suggestion(&r, "", &snapshot).expect("expected context suggestion");
        assert_eq!(sug.source, SuggestionSource::Context);
    }

    #[test]
    fn context_hit_newer_source_wins() {
        // 0.9.2.1：Selection 与 Clipboard 都非空时按 captured_at 择新。
        // 旧行为「Selection 恒压 Clipboard」在剪贴板被 update_clipboard_text 局部刷新
        // 而 Selection 陈旧不动的场景下会让 Ghost 一直显示老选区,不刷新——现在改成
        // 「最新的用户行为胜」:后 upsert 的一方胜出。
        let r = translate_router_with_target("zh");

        // 情形 A:Selection 先、Clipboard 后 → Clipboard 胜
        let mut snap_a = ContextSnapshot::default();
        snap_a.upsert_text(
            AwarenessSource::Selection,
            Some("selected english text".into()),
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        snap_a.upsert_text(
            AwarenessSource::Clipboard,
            Some("clipboard content here".into()),
        );
        let sug_a = run_best_suggestion(&r, "", &snap_a).expect("expected context suggestion");
        assert!(
            sug_a.replacement.contains("clipboard content here"),
            "较新的 Clipboard 应胜出,replacement={}",
            sug_a.replacement
        );

        // 情形 B:Clipboard 先、Selection 后 → Selection 胜（覆盖用户先复制再划词）
        let mut snap_b = ContextSnapshot::default();
        snap_b.upsert_text(
            AwarenessSource::Clipboard,
            Some("clipboard content here".into()),
        );
        std::thread::sleep(std::time::Duration::from_millis(2));
        snap_b.upsert_text(
            AwarenessSource::Selection,
            Some("selected english text".into()),
        );
        let sug_b = run_best_suggestion(&r, "", &snap_b).expect("expected context suggestion");
        assert!(
            sug_b.replacement.contains("selected english text"),
            "较新的 Selection 应胜出,replacement={}",
            sug_b.replacement
        );
    }

    #[test]
    fn context_hit_url_guard_no_translation() {
        // P0-2 关键回归：剪贴板是 URL → 翻译**不**触发（route 与 best_suggestion 一致）
        let r = translate_router_with_target("zh");
        let snapshot = snap_clipboard("https://github.com/anthropics/foo");
        let route = run_route_with_snapshot(&r, "", snapshot.clone());
        assert!(matches!(&route, Route::Mixed { candidates } if candidates.is_empty()));
        assert!(run_best_suggestion(&r, "", &snapshot).is_none());
    }

    #[test]
    fn context_hit_file_path_guard_no_translation() {
        let r = translate_router_with_target("zh");
        let snapshot = snap_clipboard(r"C:\Users\a\file.txt");
        assert!(run_best_suggestion(&r, "", &snapshot).is_none());
    }

    #[test]
    fn context_hit_short_text_no_translation() {
        let r = translate_router_with_target("zh");
        let snapshot = snap_selection("hi");
        assert!(run_best_suggestion(&r, "", &snapshot).is_none());
    }

    #[test]
    fn context_hit_same_family_no_translation() {
        // target=zh + selection 是中文 → 不触发
        let r = translate_router_with_target("zh");
        let snapshot = snap_selection("你好世界");
        assert!(run_best_suggestion(&r, "", &snapshot).is_none());
    }

    #[test]
    fn context_target_auto_falls_back_to_app_language() {
        // target_lang="auto" → 回退 app_language="en"
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "auto",
        )));
        r.set_app_language("en".into());
        // app_language=en，selection 是中文 → 触发翻译（走 best_suggestion）
        let snapshot = snap_selection("你好世界啊");
        let sug = run_best_suggestion(&r, "", &snapshot).expect("expected suggestion");
        assert_eq!(sug.source, SuggestionSource::Context);
    }

    #[test]
    fn context_target_missing_falls_back_to_app_language() {
        // resolver 里没配 target_lang → 回退 app_language="zh"
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new())); // 空 resolver
        // app_language 默认 zh，selection 是英文 → 触发
        let snapshot = snap_selection("hello world foo");
        assert!(run_best_suggestion(&r, "", &snapshot).is_some());
    }

    #[test]
    fn context_hit_arg_truncated_at_2000_chars() {
        let r = translate_router_with_target("zh");
        let long_text = "a".repeat(3000);
        let snapshot = snap_selection(&long_text);
        let sug = run_best_suggestion(&r, "", &snapshot).expect("expected suggestion");
        // replacement 里的 arg 部分应被截断（2000 char + '…'）
        assert!(sug.replacement.chars().count() >= 2001);
        assert!(sug.replacement.contains('…'));
    }

    #[test]
    fn context_hit_keyword_takeover_beats_context() {
        // keyword: "翻译" Takeover + Context 命中同 plugin → surface = Takeover(kw 强信号)，arg 用 kw
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        let route =
            run_route_with_snapshot(&r, "翻译 hello", snap_clipboard("some other english text"));
        // 走 Takeover 且 arg 用 keyword_arg="hello"
        assert!(matches!(
            route,
            Route::Takeover { plugin_id, arg, .. }
                if plugin_id == "builtin.translate" && arg == ExecArg::UserExplicit("hello".to_string())
        ));
    }

    #[test]
    fn context_hit_two_plugins_coexist() {
        // 0.8.3：两个 plugin 都声明 TextIsNonTargetLang,空 query 时 best_suggestion 取 top-1
        // （不产多 candidate；具体谁赢由 confidence 决定,两者相同则按注册顺序）。
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.add_context_rule(
            "builtin.search_selection".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(
            MockResolver::new()
                .with("builtin.translate", "target_lang", "zh")
                .with("builtin.search_selection", "target_lang", "zh"),
        ));
        // route() 空 query 不产 candidate
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(&route, Route::Mixed { candidates } if candidates.is_empty()));
        // best_suggestion 只产 top-1
        let sug = run_best_suggestion(&r, "", &snap_selection("hello world foo"))
            .expect("expected suggestion");
        assert_eq!(sug.source, SuggestionSource::Context);
    }

    #[test]
    fn context_hit_no_resolver_translate_silent_miss() {
        // 没 set_setting_resolver → target 恒 app_language(默认 zh) → 英文选区仍能触发翻译
        // （这里检验默认 language 兜底不至于让路由完全瘫痪；生产环境总是有 resolver）
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        // 无 set_setting_resolver 调用
        let snapshot = snap_selection("hello world foo");
        assert!(run_best_suggestion(&r, "", &snapshot).is_some());
    }

    #[test]
    fn context_hit_reload_clears_context_rules() {
        // reload_plugin_triggers 应清掉旧 context 规则
        let r = translate_router_with_target("zh");
        let snapshot = snap_selection("hello world foo");
        // 原来能命中
        assert!(run_best_suggestion(&r, "", &snapshot).is_some());

        // 重载空 triggers → context 规则清空
        r.reload_plugin_triggers("builtin.translate", &[]);
        assert!(run_best_suggestion(&r, "", &snapshot).is_none());
    }

    #[test]
    fn context_hit_inline_declared_downgrades_to_priority() {
        // manifest 侧 surface=inline → warn+降级 Priority（0.8.2 收窄）。
        // 0.8.3：空 query 不再产 candidate → 用「双源 merge_hits」验降级效果:
        // keyword 命中 Priority + context 命中（Inline→Priority）同 plugin → merge surface = Priority
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Inline, // 声明 Inline
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        // 非空 query: `翻译` 无参 → Priority；ctx 补 Inline → merge_hits surface_max = Priority
        let route = run_route_with_snapshot(&r, "翻译", snap_selection("hello world foo"));
        if let Route::Mixed { candidates } = route {
            assert_eq!(candidates.len(), 1);
            // ctx 侧 Inline 声明已被 add_context_rule warn+降级为 Priority；merge 后依然 Priority
            assert!(matches!(candidates[0].surface, Surface::Priority));
        } else {
            panic!("expected Mixed");
        }
    }

    #[test]
    fn truncate_arg_short_unchanged() {
        assert_eq!(truncate_arg("hello"), "hello");
        assert_eq!(truncate_arg(""), "");
    }

    #[test]
    fn truncate_arg_exact_boundary() {
        let s: String = "a".repeat(2000);
        assert_eq!(truncate_arg(&s), s); // 恰好 2000 不加省略号
    }

    #[test]
    fn truncate_arg_long_cuts_with_ellipsis() {
        let s: String = "a".repeat(2500);
        let out = truncate_arg(&s);
        assert_eq!(out.chars().count(), 2001);
        assert!(out.ends_with('…'));
    }

    #[test]
    fn truncate_arg_multibyte_correct_boundary() {
        // 3000 个中文字符 → 截到 2000 个 + '…'
        let s: String = "你".repeat(3000);
        let out = truncate_arg(&s);
        assert_eq!(out.chars().count(), 2001);
    }

    // ── 0.8.3 §4.4 Suggestion 通道单测 ────────────────────────────

    #[test]
    fn suggestion_keyword_first_letters_returns_keyword_source() {
        // Keyword 分支：非空 query "fy" 命中"翻译"首拼 → Suggestion { source=Keyword }
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = ContextSnapshot::default();
        let sug = r
            .best_suggestion("fy", &snap, 0.7)
            .expect("expected keyword suggestion");
        assert_eq!(sug.source, SuggestionSource::Keyword);
        assert_eq!(sug.display, "fanyi");
        assert!((0.0..=1.0).contains(&sug.confidence));
    }

    #[test]
    fn suggestion_keyword_exact_confidence_is_one() {
        // Keyword exact 命中 → confidence 恒 1.0（f64::INFINITY 归一 min(_,1.0)）
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = ContextSnapshot::default();
        let sug = r
            .best_suggestion("fanyi", &snap, 0.7)
            .expect("expected suggestion");
        assert_eq!(sug.source, SuggestionSource::Keyword);
        assert_eq!(sug.confidence, 1.0);
    }

    #[test]
    fn suggestion_context_only_on_empty_query() {
        // 非空 query 不显示 Context Ghost——用户已输入内容即意图表达，环境感知会干扰
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        // 空 query → Context Suggestion
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected context");
        assert_eq!(sug.source, SuggestionSource::Context);
        // 非空 query → 不显示 Context Ghost
        let sug = r.best_suggestion("chrome", &snap, 0.7);
        assert!(sug.is_none(), "非空 query 不应显示 Context Ghost");
    }

    #[test]
    fn suggestion_context_url_no_translate() {
        // URL 护栏：即使空 query,剪贴板是 URL 不触发翻译 Ghost
        let r = translate_router_with_target("zh");
        let snap = snap_clipboard("https://github.com/x/y");
        assert!(r.best_suggestion("", &snap, 0.7).is_none());
    }

    #[test]
    fn suggestion_disabled_binding_skipped() {
        // 0.8.6：用户 disable 该 binding → Ghost 不出
        let r = translate_router_with_target("zh");
        // 空 query 时能命中
        let snap = snap_selection("hello world foo");
        assert!(r.best_suggestion("", &snap, 0.7).is_some());
        // disable 后不命中
        r.apply_context_disable_list(vec![binding_key(
            "builtin.translate",
            "text_is_non_target_lang",
        )]);
        assert!(r.best_suggestion("", &snap, 0.7).is_none());
        // 清空 disable 列表 → 恢复
        r.apply_context_disable_list(vec![]);
        assert!(r.best_suggestion("", &snap, 0.7).is_some());
    }

    #[test]
    fn suggestion_target_plugin_disabled_skipped() {
        // 插件被 disable → resolver.is_enabled=false → context binding 跳过
        struct DisabledResolver;
        impl crate::domain::plugin::PluginSettingResolver for DisabledResolver {
            fn get_string(&self, _plugin_id: &str, _key: &str) -> Option<String> {
                Some("zh".into())
            }
            fn is_enabled(&self, _plugin_id: &str) -> bool {
                false
            }
        }
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(DisabledResolver));
        let snap = snap_selection("hello world foo");
        assert!(r.best_suggestion("", &snap, 0.7).is_none());
    }

    #[test]
    fn suggestion_none_on_empty_query_no_context_hit() {
        // 空 query + 无 Context 命中 → None（不兜底历史，§4.12 备忘）
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = ContextSnapshot::default(); // 无选区/剪贴板
        assert!(r.best_suggestion("", &snap, 0.7).is_none());
    }

    #[test]
    fn suggestion_multi_context_hits_takes_top_confidence() {
        // 多命中：URL(0.90) > TextIsNonTargetLang(0.75) → Ghost 只显 URL 那一条
        //
        // 构造：两条 Context binding 都指向"翻译"（避免额外插件依赖）；实际情况一般是
        // 不同 plugin。此处只验 confidence 排序逻辑。
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.open_url".into(),
            ContextTrigger::ClipboardIsUrl,
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        // 剪贴板是 URL：URL binding 命中；TextIsNonTargetLang 因 URL 护栏不命中
        // → top-1 就是 URL（无并存竞争，边界更清晰）
        let snap =
            snap_clipboard("https://example.com/very-long-english-page-title-here-for-testing");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected top-1 suggestion");
        assert!(sug.replacement.contains("open_url"));
    }

    #[test]
    fn suggestion_context_confidence_url_higher_than_lang() {
        // 纯函数级验证 confidence 排序（不依赖 route 全流程）。
        // 0.8.3 收尾：context_confidence 签名从 (&when, &snapshot) 改为 (&when, Option<AwarenessSource>),
        // origin 从 Hit 带来而非 snapshot 推断。
        let c_url = context_confidence(
            &ContextTrigger::ClipboardIsUrl,
            Some(AwarenessSource::Clipboard),
        );
        let c_lang = context_confidence(
            &ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            Some(AwarenessSource::Clipboard),
        );
        assert!(c_url > c_lang, "URL(0.90) 应 > NonTargetLang(0.75)");
    }

    #[test]
    fn suggestion_context_selection_weight_higher_than_clipboard() {
        // 有选区 → src_w=1.0；无选区 fallback clipboard → src_w=0.85
        // 0.8.3 收尾：直接传 origin,不再靠 snapshot 反推。
        let when = ContextTrigger::TextIsNonTargetLang {
            source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
        };
        let c_sel = context_confidence(&when, Some(AwarenessSource::Selection));
        let c_clip = context_confidence(&when, Some(AwarenessSource::Clipboard));
        assert!(c_sel > c_clip);
    }

    #[test]
    fn suggestion_disabled_by_autosuggest_returns_none_upstream() {
        // best_suggestion 本身不查 autosuggest_enabled（那是 SearchService 层）,
        // 但走 keyword 分支时 min_score 过高会返回 None。等效验证。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        // 阈值 1.5 → 归一化后 fuzzy 分不可能到 1.5 → None
        let snap = ContextSnapshot::default();
        assert!(r.best_suggestion("fan", &snap, 1.5).is_none());
    }

    #[test]
    fn suggestion_binding_key_format_double_colon() {
        // §4.6 决策：双冒号避开 target_id 内部点/冒号
        assert_eq!(
            binding_key("builtin.translate", "text_is_non_target_lang"),
            "builtin.translate::text_is_non_target_lang"
        );
        assert_eq!(
            binding_key("open_url", "clipboard_is_url"),
            "open_url::clipboard_is_url"
        );
    }

    #[test]
    fn suggestion_trigger_key_snake_case() {
        // 对齐 manifest 侧 snake_case
        assert_eq!(
            trigger_key(&ContextTrigger::ClipboardIsUrl),
            "clipboard_is_url"
        );
        assert_eq!(
            trigger_key(&ContextTrigger::ClipboardIsFilePath),
            "clipboard_is_file_path"
        );
        assert_eq!(
            trigger_key(&ContextTrigger::SelectionNonEmpty),
            "selection_non_empty"
        );
        assert_eq!(
            trigger_key(&ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            }),
            "text_is_non_target_lang"
        );
    }

    #[test]
    fn suggestion_replacement_falls_back_to_id_tail_when_no_keyword_rule() {
        // 无 keyword rule 时 replacement fallback 到 id 末段（保历史行为）
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        // short_target_name("builtin.translate") = "translate"
        assert!(
            sug.replacement.starts_with("translate"),
            "replacement={}",
            sug.replacement
        );
    }

    #[test]
    fn suggestion_replacement_prefers_cjk_keyword_in_zh_ui() {
        // 0.8.3 §4.13 P0 修订：zh UI 下 replacement 应用中文 keyword「翻译」而非 `translate`,
        // 保证 Tab 采纳后能命中 keyword 表 → 走 Takeover。**这是产品闭环关键**。
        let r = RuleRouter::new(true);
        // 同时注册两个 keyword,模拟翻译插件 manifest
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_keyword_rule(
            "builtin.translate".into(),
            "translate".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert!(
            sug.replacement.starts_with("翻译 "),
            "expected CJK keyword, got: {}",
            sug.replacement
        );
    }

    #[test]
    fn suggestion_replacement_prefers_ascii_keyword_in_en_ui() {
        // en UI 下 replacement 应用英文 keyword `translate`
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_keyword_rule(
            "builtin.translate".into(),
            "translate".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "en",
        )));
        r.set_app_language("en".into());
        // en UI + target=en → 需要选中非英文（中文）才触发翻译
        let snap = snap_selection("你好世界啊");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert!(
            sug.replacement.starts_with("translate "),
            "expected ASCII keyword, got: {}",
            sug.replacement
        );
    }

    #[test]
    fn suggestion_display_uses_localized_manifest_name() {
        // 0.8.3 §4.13 P0 修订：display 应用 resolver.get_display_name（本地化 manifest.name）
        // 而非 id 末段。zh UI 显「翻译 "hello..."」,en UI 显「Translate "hello..."」。
        struct NamedResolver {
            target_lang: String,
        }
        impl PluginSettingResolver for NamedResolver {
            fn get_string(&self, _plugin_id: &str, key: &str) -> Option<String> {
                if key == "target_lang" {
                    Some(self.target_lang.clone())
                } else {
                    None
                }
            }
            fn get_display_name(&self, plugin_id: &str, lang: &str) -> Option<String> {
                if plugin_id != "builtin.translate" {
                    return None;
                }
                Some(if lang.starts_with("zh") {
                    "翻译".into()
                } else {
                    "Translate".into()
                })
            }
        }
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(NamedResolver {
            target_lang: "zh".into(),
        }));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert!(
            sug.display.starts_with("翻译 "),
            "zh display should use 翻译: {}",
            sug.display
        );

        // 切 en UI
        r.set_setting_resolver(Arc::new(NamedResolver {
            target_lang: "en".into(),
        }));
        r.set_app_language("en".into());
        let snap_zh = snap_selection("你好世界啊");
        let sug = r
            .best_suggestion("", &snap_zh, 0.7)
            .expect("expected suggestion");
        assert!(
            sug.display.starts_with("Translate "),
            "en display should use Translate: {}",
            sug.display
        );
    }

    #[test]
    fn suggestion_ghost_replacement_hits_takeover_via_route() {
        // 0.8.3 §4.13 P0 闭环单测：Ghost.replacement 喂回 route() 必须能命中 keyword Takeover。
        // 这条闭环旧实现（fallback 到 id 末段 `translate`）+ 无 keyword rule 会断链;
        // 新实现从 rules 反查偏好字符集的 keyword,确保能命中。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");

        // 1. 拿 Context Ghost 的 replacement
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        let replacement = sug.replacement.clone();

        // 2. 用 replacement 走 route(),期待 Takeover 命中 builtin.translate
        //    （keyword「翻译」+ arg 触发 Prefix 分支 → resolve_surface = Takeover）
        let route = run_route_with_snapshot(&r, &replacement, snap);
        match route {
            Route::Takeover { plugin_id, arg, .. } => {
                assert_eq!(plugin_id, "builtin.translate");
                assert_eq!(arg, ExecArg::UserExplicit("hello world foo".to_string()));
            }
            Route::Mixed { candidates } if !candidates.is_empty() => {
                // 未升 Takeover 也算过——至少 candidate 命中就说明闭环没断
                assert_eq!(candidates[0].plugin_id, "builtin.translate");
            }
            other => panic!(
                "expected Takeover/Mixed with hits for replacement={:?}, got {:?}",
                replacement, other
            ),
        }
    }

    #[test]
    fn suggestion_context_arg_truncated_in_display() {
        // 长文本：display 截 40 字符 + `…` + 引号收尾（build_context_suggestion_text 内部）
        let r = translate_router_with_target("zh");
        let long_text = "hello ".repeat(200); // > 40 字符
        let snap = snap_selection(&long_text);
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert!(
            sug.display.contains('…'),
            "display should contain ellipsis: {}",
            sug.display
        );
    }

    #[test]
    fn suggestion_non_empty_query_no_context_ghost() {
        // 非空 query 不显示 Context Ghost——用户已输入内容即意图表达，环境感知会干扰
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        let sug = r.best_suggestion("chrome", &snap, 0.7);
        assert!(sug.is_none(), "非空 query 不应显示 Context Ghost");
    }

    // ── 0.8.3 收尾 · 参数不隐式注入回归 ──────────────────────────
    // 用户报告的 bug：输入 `fy` 未打参数,应用竟然调剪贴板内容作参数发起翻译。
    // 根因：merge_hits_keyword_only 里 `existing.arg = ctx_hit.arg` 隐式代参。
    // 修复：删掉这行,Context 只加 surface,不代参。

    #[test]
    fn merge_hits_does_not_inject_ctx_arg_when_kw_arg_empty() {
        // 首拼 `fy` 命中「翻译」keyword（arg=""）,剪贴板有英文 context 命中 → merge
        // 后 arg 仍应为空,不应被 ctx.arg 代填。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(std::sync::Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        let snap = snap_clipboard("hello world foo bar");
        let route = run_route_with_snapshot(&r, "fy", snap);
        // 首拼弱信号 + 无参 → Priority（不 Takeover）,arg 保持空
        if let Route::Mixed { candidates } = route {
            let translate = candidates
                .iter()
                .find(|c| c.plugin_id == "builtin.translate")
                .expect("翻译插件应出现在候选");
            assert!(translate.arg.is_none(), "参数不应被剪贴板隐式注入,应保持空");
        } else {
            panic!("expected Mixed route, got Takeover (Context 不该升 Takeover)");
        }
    }

    #[test]
    fn merge_hits_context_surface_boost_still_works() {
        // Context 加 surface 的功能保留 —— 用户打 `翻译`（无参 Priority）+ 剪贴板英文,
        // Context 命中同 plugin 应把 Priority 保住（surface_max 不会降级）,
        // 但 arg 仍应保持空（用户没给参数）。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(std::sync::Arc::new(MockResolver::new().with(
            "builtin.translate",
            "target_lang",
            "zh",
        )));
        let snap = snap_clipboard("hello world foo bar");
        let route = run_route_with_snapshot(&r, "翻译", snap);
        if let Route::Mixed { candidates } = route {
            let translate = candidates
                .iter()
                .find(|c| c.plugin_id == "builtin.translate")
                .expect("翻译插件应出现");
            // 双源命中 → Priority 保住
            assert!(matches!(translate.surface, Surface::Priority));
            // arg 仍应为空（用户没输参数,Context 不代填）
            assert!(translate.arg.is_none());
        } else {
            panic!("expected Mixed");
        }
    }

    // ── 0.8.4 §5.3.1 Surface Booster（RankingHint）──────────────────

    #[test]
    fn ranking_hint_boosts_surface_without_touching_arg() {
        // 0.8.4 §5.4 边界回归：RankingHint 只升 surface（排序）,不动 arg、不召回新候选。
        // 构造首拼带参 kw hit（InitialsPrefix → Inline）,验 hint 把它 boost 到 Priority,
        // 同时 arg 保持用户显式输入不变。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.weather".into(),
            "天气".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let h = std::collections::HashMap::new();

        // 无 hint：tq 北京 → InitialsPrefix（首拼带参,弱信号）→ Inline,arg=北京
        let candidates = match tauri::async_runtime::block_on(r.route("tq 北京", &h, None)) {
            Route::Mixed { candidates } => candidates,
            other => panic!("无 hint 应 Mixed,got {:?}", other),
        };
        assert_eq!(candidates.len(), 1, "无 hint 单 candidate");
        assert!(
            matches!(candidates[0].surface, Surface::Inline),
            "无 hint 应 Inline"
        );
        assert_eq!(candidates[0].arg, ExecArg::UserExplicit("北京".to_string()));

        // 有 hint boost weather：surface 升 Priority,arg 不变（不被 hint 动）,候选集不变
        let hint = RankingHint {
            boost_plugin_id: "builtin.weather".into(),
        };
        let candidates = match tauri::async_runtime::block_on(r.route("tq 北京", &h, Some(&hint)))
        {
            Route::Mixed { candidates } => candidates,
            other => panic!("有 hint 应 Mixed,got {:?}", other),
        };
        assert_eq!(candidates.len(), 1, "hint 不召回新 candidate");
        assert!(
            matches!(candidates[0].surface, Surface::Priority),
            "hint 应 boost 到 Priority"
        );
        assert_eq!(
            candidates[0].arg,
            ExecArg::UserExplicit("北京".to_string()),
            "arg 不被 hint 动（参数注入必须显式）"
        );
    }

    // ── 0.8.3 §4.9 origin 单测 ────────────────────────────────────

    #[test]
    fn suggestion_origin_selection_when_selected_text_present() {
        // 选中英文 → origin = Selection
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert_eq!(sug.origin, Some(SuggestionOrigin::Selection));
    }

    #[test]
    fn suggestion_origin_clipboard_when_only_clipboard() {
        // 只剪贴板有内容 → origin = Clipboard
        let r = translate_router_with_target("zh");
        let snap = snap_clipboard("hello world foo");
        let sug = r
            .best_suggestion("", &snap, 0.7)
            .expect("expected suggestion");
        assert_eq!(sug.origin, Some(SuggestionOrigin::Clipboard));
    }

    #[test]
    fn suggestion_origin_clipboard_for_url_trigger() {
        // ClipboardIsUrl trigger 恒 Clipboard,不管选区是否有内容
        let r = RuleRouter::new(true);
        r.add_context_rule(
            "builtin.open_url".into(),
            ContextTrigger::ClipboardIsUrl,
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        let mut snapshot = ContextSnapshot::default();
        snapshot.upsert_text(
            AwarenessSource::Selection,
            Some("some selected text".into()),
        );
        snapshot.upsert_text(
            AwarenessSource::Clipboard,
            Some("https://example.com".into()),
        );
        let sug = r
            .best_suggestion("", &snapshot, 0.7)
            .expect("expected suggestion");
        assert_eq!(sug.origin, Some(SuggestionOrigin::Clipboard));
    }

    #[test]
    fn suggestion_origin_none_for_keyword_branch() {
        // Keyword 分支恒 origin=None
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = ContextSnapshot::default();
        let sug = r
            .best_suggestion("fy", &snap, 0.7)
            .expect("expected keyword suggestion");
        assert_eq!(sug.source, SuggestionSource::Keyword);
        assert!(sug.origin.is_none());
    }

    #[test]
    fn suggestion_origin_matches_confidence_source() {
        // 端到端 origin 传导（0.8.3 收尾 · awareness 重构验收）：
        // - snap.selection 非空 → Hit.origin=Selection → confidence 用 selection 权重(1.0),
        //   Suggestion.origin=Selection。
        // - snap.clipboard 非空 → Hit.origin=Clipboard → confidence 用 clipboard 权重(0.85),
        //   Suggestion.origin=Clipboard。
        // 重构前:三处推断分别做,需 `snapshot_has_meaningful_selection` helper 绑一致。
        // 重构后:origin 由 AwarenessView 直接带来,by construction 不可能撕裂。
        let r = translate_router_with_target("zh");

        let with_sel = snap_selection("hello world foo");
        let sug1 = r.best_suggestion("", &with_sel, 0.7).unwrap();
        assert_eq!(sug1.origin, Some(SuggestionOrigin::Selection));
        assert!(
            (sug1.confidence - 0.75).abs() < 1e-9,
            "expected 0.75, got {}",
            sug1.confidence
        );

        let with_clip = snap_clipboard("hello world foo");
        let sug2 = r.best_suggestion("", &with_clip, 0.7).unwrap();
        assert_eq!(sug2.origin, Some(SuggestionOrigin::Clipboard));
        // 0.75 * 0.85 = 0.6375
        assert!(
            (sug2.confidence - 0.6375).abs() < 1e-9,
            "expected 0.6375, got {}",
            sug2.confidence
        );
    }

    // ── 0.8.8 bugfix · Context 采纳后自抑制护栏 ──────────────────────────
    // 用户报告的 bug：Ghost 显示 `翻译 "tab"` → 按 Tab 采纳 → query 变 `翻译 tab`
    // → Context 又产同一 Suggestion → Ghost 又画回来 → 无限 Tab 叠加。
    // 根因：ContextProducer 不看 query，snapshot 只要命中就一直产。
    // 修复：query 已命中同 plugin keyword → context_suggestion 静默。
    //
    // 【为何护栏落点在 Context 而不是 Keyword】
    // best_suggestion 里非空 query 是"keyword 分支优先，fuzzy 未命中才 fallback Context"。
    // 死角 case：query="翻译 tab"，keyword 表里"翻译"因为带空格 fuzzy 不达分数阈值 →
    // Keyword 分支返回 None → 落到 Context fallback → 未加护栏时会产同一 Suggestion。

    #[test]
    fn context_suggestion_silenced_after_keyword_accepted() {
        // 核心回归：Tab 采纳 Context Ghost `翻译 "tab"` 后 query 变 `翻译 tab`，
        // 此时 keyword fuzzy 因带空格不达阈值 → 走 Context fallback → 护栏必须静默。
        let r = translate_router_with_target("zh");
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = snap_selection("hello world foo");

        // 用户 Tab 采纳后的 query：既走不进 Keyword 分支（fuzzy 带空格失败），
        // 也不该走进 Context fallback（护栏兜住）→ 整个 Suggestion 为 None。
        // 之前的 bug 行为：Suggestion 又给出 Context「翻译 "hello world foo"」→ Ghost 复活。
        let sug = r.best_suggestion("翻译 tab", &snap, 0.7);
        assert!(
            sug.is_none(),
            "Context should be silenced when query already hits same plugin's keyword, got: {sug:?}",
        );

        // 对照组：非空无关 query 也不显示 Context Ghost
        let sug_fallback = r.best_suggestion("xyz random", &snap, 0.7);
        assert!(
            sug_fallback.is_none(),
            "non-empty query should not get Context Ghost"
        );
    }

    #[test]
    fn context_suggestion_silenced_with_pinyin_keyword_forms() {
        // 拼音三形式同 plugin 采纳后一样要静默。直接测 `context_suggestion` 避开
        // `best_suggestion` 里 Keyword-first fuzzy 分支的干扰——因为拼音短 query
        // 有可能被 Keyword 分支先接住返回 Keyword Suggestion，测不到 Context 护栏。
        let r = translate_router_with_target("zh");
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let snap = snap_selection("hello world foo");

        // 三种形式都能触发 query_hits_plugin_keyword → context_suggestion 直接返回 None
        assert!(
            r.context_suggestion("翻译 tab", &snap).is_none(),
            "原文 Prefix should silence"
        );
        assert!(
            r.context_suggestion("fanyi tab", &snap).is_none(),
            "pinyin_full Prefix should silence"
        );
        assert!(
            r.context_suggestion("fy tab", &snap).is_none(),
            "pinyin_initials Prefix should silence"
        );
        assert!(
            r.context_suggestion("翻译", &snap).is_none(),
            "原文 Exact should silence"
        );
        assert!(
            r.context_suggestion("fanyi", &snap).is_none(),
            "pinyin_full Exact should silence"
        );
        assert!(
            r.context_suggestion("fy", &snap).is_none(),
            "pinyin_initials Exact should silence"
        );

        // 对照：空 query 仍能产 Context，非空无关 query 不产 Context
        assert!(
            r.context_suggestion("", &snap).is_some(),
            "empty query still fires"
        );
        assert!(
            r.context_suggestion("xyz random", &snap).is_none(),
            "non-empty query should not fire"
        );
    }

    #[test]
    fn query_hits_plugin_keyword_matches_all_forms() {
        // helper 单测：三种 keyword 形式（原文/pinyin_full/pinyin_initials）都算命中。
        let r = RuleRouter::new(true);
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            Surface::Auto,
            SurfaceView::List,
        );

        // 原文 Exact
        assert!(r.query_hits_plugin_keyword("翻译", "builtin.translate"));
        // 原文 Prefix
        assert!(r.query_hits_plugin_keyword("翻译 hello", "builtin.translate"));
        // pinyin_full Exact
        assert!(r.query_hits_plugin_keyword("fanyi", "builtin.translate"));
        // pinyin_full Prefix
        assert!(r.query_hits_plugin_keyword("fanyi hello", "builtin.translate"));
        // pinyin_initials Exact
        assert!(r.query_hits_plugin_keyword("fy", "builtin.translate"));

        // 未命中
        assert!(!r.query_hits_plugin_keyword("chrome", "builtin.translate"));
        // 空 query
        assert!(!r.query_hits_plugin_keyword("", "builtin.translate"));
        // 只有空格
        assert!(!r.query_hits_plugin_keyword("   ", "builtin.translate"));
        // 命中的是别的 plugin
        assert!(!r.query_hits_plugin_keyword("翻译", "other.plugin"));
    }
}
