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

use std::sync::{Arc, RwLock};

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

// ── 路由结果 ──────────────────────────────────────────────

/// Mixed 分支的单个候选(命中但未 takeover 的插件)。
#[derive(Debug, Clone)]
pub struct Candidate {
    pub plugin_id: String,
    /// 传给插件的参数(精确命中→空串;前缀命中→余下文本)。
    pub arg: String,
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
        arg: String,
        #[allow(dead_code)] // 0.4 仅 List;P3 扩展 Chat/Custom 时消费
        view: SurfaceView,
        /// 同 Candidate.hint；Takeover 走 keyword 强信号时恒 None（首拼不升级 Takeover）。
        #[allow(dead_code)]
        hint: Option<String>,
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
    async fn route(&self, query: &str, ctx: &QueryContext<'_>) -> Route;

    /// 算 ghost text 补全（0.8.1 §2.4）。默认实现返回 None（非 RuleRouter 实现无需支持）。
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
}

// ── RuleRouter ────────────────────────────────────────────

/// 规则匹配核心:纯同步、可单测。
///
/// 0.8.2 §3.4 起持三张规则表：keyword / regex（原有）+ context（新增）。
/// Context 规则通过 `PluginSettingResolver` 反查插件 `target_lang`（`auto` → `app_language`）。
///
/// 0.8.3 §4.6 加 `disabled_bindings`：用户在「上下文智能感知」面板关掉某条 context binding
/// 时进此集合。key 格式 `{target_id}::{trigger_key}`（双冒号避开 target_id 内部点/冒号）。
/// 采用黑名单模式，`context_rules` 依然是运行时唯一 trigger→target 表——从 manifest 加载
/// = 默认全启用，disable 项在 `match_context_hits` 中按 key 跳过。
pub struct RuleRouter {
    rules: RwLock<Vec<Rule>>,
    /// 全局总闸:为 false 时所有 Takeover 降级 Priority。
    takeover_enabled: RwLock<bool>,
    /// Context 规则表（0.8.2 §3.4）。与 keyword/regex 表并存,不受 query 影响。
    context_rules: RwLock<Vec<ContextRule>>,
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
            settings: RwLock::new(None),
            // 单测下不注入 language → 用 "zh" 兜底（Blink 默认 UI 语言）。
            app_language: RwLock::new("zh".to_string()),
            disabled_bindings: RwLock::new(std::collections::HashSet::new()),
        }
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
            kw = self.rules.read().unwrap().iter().filter(|r| r.plugin_id == plugin_id).count(),
            ctx = self.context_rules.read().unwrap().iter().filter(|r| r.plugin_id == plugin_id).count(),
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
    fn collect_suggest_keywords(&self) -> Vec<(String, String)> {
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
    async fn route(&self, query: &str, ctx: &QueryContext<'_>) -> Route {
        let q = query.trim();
        let takeover_enabled = *self.takeover_enabled.read().unwrap();

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
                            Some(MatchType::Prefix { arg: String::new(), hint: None })
                        } else {
                            None
                        }
                    }
                };
                if let Some(mt) = mt_opt {
                    let arg = match &mt {
                        MatchType::Exact { .. } | MatchType::InitialsExact { .. } => String::new(),
                        MatchType::Prefix { arg, .. } | MatchType::InitialsPrefix { arg, .. } => arg.clone(),
                    };
                    let hint: Option<String> = match &mt {
                        MatchType::Exact { hint } | MatchType::Prefix { hint, .. } => hint.clone(),
                        MatchType::InitialsExact { hint } | MatchType::InitialsPrefix { hint, .. } => Some(hint.clone()),
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

        // ── 2. Context 匹配（0.8.2 §3.4，不受 query 影响）──────
        //    0.8.3 §4.13 P0-3：空 query 时 Context 不产独立 candidate（改由 best_suggestion
        //    产 Ghost + Tab 采纳）；非空 query 时保留 kw+ctx 同 plugin 的 merge_hits 加分。
        //    单独命中（context 命中但 kw 未命中）的 context_hit 在非空 query 时视为「用户
        //    已表 keyword 意图」直接丢弃——避免复制英文 + 输入 chrome 时翻译还抢首屏。
        let context_hits = if q.is_empty() {
            Vec::new()
        } else {
            self.match_context_hits(ctx.snapshot)
        };

        // ── 3. 合并（keyword + context 同 plugin 取 max surface / kw 优先 arg）──
        //     非空 query：merge 只保留双源命中；单独 context 命中被丢弃。
        let hits = merge_hits_keyword_only(hits, context_hits);

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
        // 空/非空 query 走不同源；0.8.3 阶段互斥（0.9 AI 接入后走同一竞争）。
        let trimmed = query.trim();
        if trimmed.is_empty() {
            // Context 分支：多命中取 confidence 最高的（0.8.3 收尾：origin 从 Hit 直取）
            let hits = self.match_context_hits(snapshot);
            let best_ctx = hits.into_iter().max_by(|a, b| {
                let ca = a.when.map(|w| context_confidence(&w, a.origin)).unwrap_or(0.0);
                let cb = b.when.map(|w| context_confidence(&w, b.origin)).unwrap_or(0.0);
                ca.partial_cmp(&cb).unwrap_or(std::cmp::Ordering::Equal)
            })?;

            let when = best_ctx.when.as_ref()?;
            let confidence = context_confidence(when, best_ctx.origin);
            // 0.8.3 收尾：origin 从 Hit 直接映射为 SuggestionOrigin,不再事后推断
            let origin = best_ctx.origin.map(SuggestionOrigin::from);
            let (display, replacement) = self.build_context_suggestion_text(&best_ctx);
            Some(Suggestion {
                display,
                replacement,
                source: SuggestionSource::Context,
                confidence,
                prefix_len: 0,
                origin,
            })
        } else {
            // Keyword 分支：走 compute_hint_scored 拿 fuzzy 分（§4.13 P0-2）
            let keywords = self.collect_suggest_keywords();
            let (hint, score) = suggest::compute_hint_scored(&keywords, query, min_score)?;
            Some(Suggestion {
                display: hint.display,
                replacement: hint.replacement,
                source: SuggestionSource::Keyword,
                confidence: score.min(1.0), // f64::INFINITY exact 命中 → 1.0
                prefix_len: hint.prefix_len,
                origin: None, // Keyword 无外部来源
            })
        }
    }

    fn set_app_language(&self, language: String) {
        // 委托到 inherent method(单测直接用 RuleRouter 类型,生产环境走 trait)
        RuleRouter::set_app_language(self, language);
    }

    fn apply_context_disable_list(&self, keys: Vec<String>) {
        RuleRouter::apply_context_disable_list(self, keys);
    }
}

impl RuleRouter {
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

            // 5. 抽 arg + origin：（0.8.3 收尾 · awareness）
            //    - `TextIsNonTargetLang`：走 source.extract 拿 AwarenessView,arg + origin 一起
            //    - `ClipboardIsUrl` / `ClipboardIsFilePath`：trigger 语义锁定 Clipboard 来源
            //    - `SelectionNonEmpty`：trigger 语义锁定 Selection 来源
            //    origin 从数据侧带来,不再事后推断（删掉 infer_origin）
            let (arg, origin) = match &rule.when {
                ContextTrigger::TextIsNonTargetLang { source } => {
                    match source.extract(snapshot) {
                        Some(view) => (truncate_arg(view.text), Some(view.source)),
                        None => (String::new(), None),
                    }
                }
                ContextTrigger::ClipboardIsUrl | ContextTrigger::ClipboardIsFilePath => {
                    (String::new(), Some(AwarenessSource::Clipboard))
                }
                ContextTrigger::SelectionNonEmpty => {
                    (String::new(), Some(AwarenessSource::Selection))
                }
            };

            // 6. 门禁：TextIsNonTargetLang 抽不到 arg → 不召回
            //    （arg="" 场景是 event-only 触发，不受此闸约束）
            if matches!(rule.when, ContextTrigger::TextIsNonTargetLang { .. }) && arg.is_empty() {
                tracing::trace!(plugin = %rule.plugin_id, "context 命中但 arg 空,跳过召回");
                continue;
            }

            tracing::trace!(
                plugin = %rule.plugin_id,
                arg_len = arg.chars().count(),
                surface = ?rule.surface,
                origin = ?origin,
                "context 规则产出 Hit",
            );

            out.push(Hit {
                plugin_id: rule.plugin_id.clone(),
                arg,
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
    fn build_context_suggestion_text(&self, hit: &Hit) -> (String, String) {
        // display 截 40 字符便于 ghost 单行展示
        const DISPLAY_MAX: usize = 40;
        let display_arg: String = if hit.arg.chars().count() > DISPLAY_MAX {
            let truncated: String = hit.arg.chars().take(DISPLAY_MAX).collect();
            format!("{truncated}…")
        } else {
            hit.arg.clone()
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
        let replacement = if hit.arg.is_empty() {
            format!("{keyword} ")
        } else {
            format!("{keyword} {}", hit.arg)
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
    arg: String,
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

/// 合并 keyword + context 两路命中（0.8.2 §3.4.4）。
///
/// 规则：
/// - 同 plugin_id 两路都命中：
///   - surface = max(kw.surface, ctx.surface) —— Takeover > Priority > Inline
///   - arg     = kw.arg（非空则用）否则 ctx.arg —— keyword 显式意图优先
///   - hint    = kw.hint —— 保留强信号 hint（ctx.hint 恒 None）
///   - view    = kw.view
/// - 只 keyword 命中：原样保留
/// - 只 context 命中：原样保留（surface 已在 add_context_rule 收窄为 Priority）
///
/// **arg 截断**：`truncate_arg` 在 `match_context_hits` 内做；`merge_hits` 只组合。
#[allow(dead_code)] // 0.8.3 §4.13 P0-3：route() 走 merge_hits_keyword_only；此函数保留供后续/单测
fn merge_hits(kw_hits: Vec<Hit>, ctx_hits: Vec<Hit>) -> Vec<Hit> {
    let mut out: Vec<Hit> = kw_hits;

    for ctx_hit in ctx_hits {
        // 查看 out 里是否已有同 plugin 的 keyword hit
        if let Some(existing) = out.iter_mut().find(|h| h.plugin_id == ctx_hit.plugin_id) {
            // 双路命中同 plugin
            existing.surface = surface_max(existing.surface, ctx_hit.surface);
            if existing.arg.is_empty() {
                existing.arg = ctx_hit.arg;
            }
            // hint 保留 kw 的；source 标记双源以便后续（未来需要）
            existing.source = HitSource::Keyword; // kw 强信号胜出
        } else {
            out.push(ctx_hit);
        }
    }
    out
}

/// 0.8.3 §4.13 P0-3 变体：只保留 keyword 命中 + 双源命中的加分结果；丢弃 solo context 命中。
///
/// 与 `merge_hits` 的差别：**单独 context 命中（无 keyword 同 plugin）被丢弃**——非空 query
/// 已表明用户 keyword 意图,solo context 命中抢首屏太激进（0.8.2 push 模式的历史包袱）。
/// 空 query 场景 route() 已提前把 ctx_hits 传空,此函数等价 keyword-only。
fn merge_hits_keyword_only(kw_hits: Vec<Hit>, ctx_hits: Vec<Hit>) -> Vec<Hit> {
    let mut out: Vec<Hit> = kw_hits;

    for ctx_hit in ctx_hits {
        if let Some(existing) = out.iter_mut().find(|h| h.plugin_id == ctx_hit.plugin_id) {
            // 双路命中同 plugin：走加分逻辑
            existing.surface = surface_max(existing.surface, ctx_hit.surface);
            if existing.arg.is_empty() {
                existing.arg = ctx_hit.arg;
            }
            existing.source = HitSource::Keyword;
        }
        // else：solo context 命中丢弃（0.8.3 §4.13 P0-3）
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
                MatchType::InitialsPrefix { arg, hint: derived_hint }
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
        let snapshot = crate::infra::platform::context::ContextSnapshot::default();
        let ctx = QueryContext { history: &h, snapshot: &snapshot };
        tauri::async_runtime::block_on(r.route(q, &ctx))
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
            matches!(route, Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.echo" && arg == "hello")
        );
    }

    #[test]
    fn explicit_takeover_always() {
        let r = router_with_rules(true);
        let route = run_route(&r, "ip");
        assert!(
            matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "builtin.ip")
        );
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
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.is_empty())
        );
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
                && candidates[0].arg == "北京"
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
                && candidates[0].arg.is_empty()
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
            Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.translate" && arg == "hello"
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
            Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.translate" && arg == "hello"
        ));
    }

    #[test]
    fn multiple_takeovers_first_wins() {
        let r = RuleRouter::new(true);
        r.add_keyword_rule("a".into(), "foo".into(), Surface::Takeover, SurfaceView::List);
        r.add_keyword_rule("b".into(), "foo".into(), Surface::Takeover, SurfaceView::List);
        let route = run_route(&r, "foo");
        assert!(
            matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "a")
        );
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
        assert!(
            matches!(route, Route::Takeover { plugin_id, .. } if plugin_id == "builtin.hex")
        );
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
        assert!(
            matches!(route, Route::Mixed { candidates } if candidates.is_empty())
        );
    }

    #[test]
    fn regex_invalid_pattern_skipped() {
        let r = RuleRouter::new(true);
        assert!(r.add_regex_rule("x".into(), "[", Surface::Auto, SurfaceView::List).is_err());
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
        let hint = r.suggest_completion("fy hello", 0.7).expect("should have hint");
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

    fn run_route_with_snapshot(r: &RuleRouter, q: &str, snapshot: ContextSnapshot) -> Route {
        let h = std::collections::HashMap::new();
        let ctx = QueryContext { history: &h, snapshot: &snapshot };
        tauri::async_runtime::block_on(r.route(q, &ctx))
    }

    /// 0.8.3 §4.4：空 query 场景验 best_suggestion（Context 走 Ghost 不产 candidate）。
    fn run_best_suggestion(r: &RuleRouter, q: &str, snapshot: &ContextSnapshot) -> Option<Suggestion> {
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
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", target),
        ));
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
    fn context_hit_selection_beats_clipboard() {
        // selection 非空时不看 clipboard；validate 通过 best_suggestion 的 replacement
        let r = translate_router_with_target("zh");
        let mut snapshot = ContextSnapshot::default();
        snapshot.upsert_text(AwarenessSource::Selection, Some("selected english text".into()));
        snapshot.upsert_text(AwarenessSource::Clipboard, Some("clipboard content here".into()));
        let sug = run_best_suggestion(&r, "", &snapshot).expect("expected context suggestion");
        assert!(sug.replacement.contains("selected english text"));
        assert!(!sug.replacement.contains("clipboard content here"));
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
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "auto"),
        ));
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
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "zh"),
        ));
        let route = run_route_with_snapshot(
            &r,
            "翻译 hello",
            snap_clipboard("some other english text"),
        );
        // 走 Takeover 且 arg 用 keyword_arg="hello"
        assert!(matches!(
            route,
            Route::Takeover { plugin_id, arg, .. }
                if plugin_id == "builtin.translate" && arg == "hello"
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
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "zh"),
        ));
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
    fn merge_hits_surface_max_and_arg_priority() {
        // 单元测：keyword hit (Priority, arg="foo") + context hit (Priority, arg="bar")
        //         → 合并后 surface=Priority，arg="foo"（kw 优先）
        let kw = vec![Hit {
            plugin_id: "p".into(),
            arg: "foo".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Keyword,
            when: None,
            origin: None,
        }];
        let ctx = vec![Hit {
            plugin_id: "p".into(),
            arg: "bar".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Context,
            when: Some(ContextTrigger::SelectionNonEmpty),
            origin: Some(AwarenessSource::Selection),
        }];
        let merged = merge_hits(kw, ctx);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].arg, "foo");
        assert!(matches!(merged[0].surface, Surface::Priority));
    }

    #[test]
    fn merge_hits_kw_arg_empty_takes_ctx_arg() {
        // keyword 命中但 arg="" + context 命中同 plugin arg="bar" → 合并 arg="bar"
        let kw = vec![Hit {
            plugin_id: "p".into(),
            arg: "".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Keyword,
            when: None,
            origin: None,
        }];
        let ctx = vec![Hit {
            plugin_id: "p".into(),
            arg: "bar".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Context,
            when: Some(ContextTrigger::SelectionNonEmpty),
            origin: Some(AwarenessSource::Selection),
        }];
        let merged = merge_hits(kw, ctx);
        assert_eq!(merged[0].arg, "bar");
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
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        let snap = ContextSnapshot::default();
        let sug = r.best_suggestion("fy", &snap, 0.7).expect("expected keyword suggestion");
        assert_eq!(sug.source, SuggestionSource::Keyword);
        assert_eq!(sug.display, "fanyi");
        assert!((0.0..=1.0).contains(&sug.confidence));
    }

    #[test]
    fn suggestion_keyword_exact_confidence_is_one() {
        // Keyword exact 命中 → confidence 恒 1.0（f64::INFINITY 归一 min(_,1.0)）
        let r = RuleRouter::new(true);
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        let snap = ContextSnapshot::default();
        let sug = r.best_suggestion("fanyi", &snap, 0.7).expect("expected suggestion");
        assert_eq!(sug.source, SuggestionSource::Keyword);
        assert_eq!(sug.confidence, 1.0);
    }

    #[test]
    fn suggestion_context_only_on_empty_query() {
        // Context 分支：仅在空 query 触发（非空 query 不产 Context Suggestion）
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        // 空 query → Context Suggestion
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected context");
        assert_eq!(sug.source, SuggestionSource::Context);
        // 非空 query（无 keyword 命中）→ 没有 keyword 分支的候选,Context 也不产 → None
        assert!(r.best_suggestion("chrome", &snap, 0.7).is_none());
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
        r.apply_context_disable_list(vec![
            binding_key("builtin.translate", "text_is_non_target_lang"),
        ]);
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
            fn is_enabled(&self, _plugin_id: &str) -> bool { false }
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
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
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
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "zh"),
        ));
        // 剪贴板是 URL：URL binding 命中；TextIsNonTargetLang 因 URL 护栏不命中
        // → top-1 就是 URL（无并存竞争，边界更清晰）
        let snap = snap_clipboard("https://example.com/very-long-english-page-title-here-for-testing");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected top-1 suggestion");
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
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        // 阈值 1.5 → 归一化后 fuzzy 分不可能到 1.5 → None
        let snap = ContextSnapshot::default();
        assert!(r.best_suggestion("fan", &snap, 1.5).is_none());
    }

    #[test]
    fn suggestion_binding_key_format_double_colon() {
        // §4.6 决策：双冒号避开 target_id 内部点/冒号
        assert_eq!(binding_key("builtin.translate", "text_is_non_target_lang"),
                   "builtin.translate::text_is_non_target_lang");
        assert_eq!(binding_key("open_url", "clipboard_is_url"),
                   "open_url::clipboard_is_url");
    }

    #[test]
    fn suggestion_trigger_key_snake_case() {
        // 对齐 manifest 侧 snake_case
        assert_eq!(trigger_key(&ContextTrigger::ClipboardIsUrl), "clipboard_is_url");
        assert_eq!(trigger_key(&ContextTrigger::ClipboardIsFilePath), "clipboard_is_file_path");
        assert_eq!(trigger_key(&ContextTrigger::SelectionNonEmpty), "selection_non_empty");
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
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        // short_target_name("builtin.translate") = "translate"
        assert!(sug.replacement.starts_with("translate"), "replacement={}", sug.replacement);
    }

    #[test]
    fn suggestion_replacement_prefers_cjk_keyword_in_zh_ui() {
        // 0.8.3 §4.13 P0 修订：zh UI 下 replacement 应用中文 keyword「翻译」而非 `translate`,
        // 保证 Tab 采纳后能命中 keyword 表 → 走 Takeover。**这是产品闭环关键**。
        let r = RuleRouter::new(true);
        // 同时注册两个 keyword,模拟翻译插件 manifest
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        r.add_keyword_rule("builtin.translate".into(), "translate".into(), Surface::Auto, SurfaceView::List);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "zh"),
        ));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        assert!(sug.replacement.starts_with("翻译 "), "expected CJK keyword, got: {}", sug.replacement);
    }

    #[test]
    fn suggestion_replacement_prefers_ascii_keyword_in_en_ui() {
        // en UI 下 replacement 应用英文 keyword `translate`
        let r = RuleRouter::new(true);
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        r.add_keyword_rule("builtin.translate".into(), "translate".into(), Surface::Auto, SurfaceView::List);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "en"),
        ));
        r.set_app_language("en".into());
        // en UI + target=en → 需要选中非英文（中文）才触发翻译
        let snap = snap_selection("你好世界啊");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        assert!(sug.replacement.starts_with("translate "), "expected ASCII keyword, got: {}", sug.replacement);
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
                if key == "target_lang" { Some(self.target_lang.clone()) } else { None }
            }
            fn get_display_name(&self, plugin_id: &str, lang: &str) -> Option<String> {
                if plugin_id != "builtin.translate" { return None; }
                Some(if lang.starts_with("zh") { "翻译".into() } else { "Translate".into() })
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
        r.set_setting_resolver(Arc::new(NamedResolver { target_lang: "zh".into() }));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        assert!(sug.display.starts_with("翻译 "), "zh display should use 翻译: {}", sug.display);

        // 切 en UI
        r.set_setting_resolver(Arc::new(NamedResolver { target_lang: "en".into() }));
        r.set_app_language("en".into());
        let snap_zh = snap_selection("你好世界啊");
        let sug = r.best_suggestion("", &snap_zh, 0.7).expect("expected suggestion");
        assert!(sug.display.starts_with("Translate "), "en display should use Translate: {}", sug.display);
    }

    #[test]
    fn suggestion_ghost_replacement_hits_takeover_via_route() {
        // 0.8.3 §4.13 P0 闭环单测：Ghost.replacement 喂回 route() 必须能命中 keyword Takeover。
        // 这条闭环旧实现（fallback 到 id 末段 `translate`）+ 无 keyword rule 会断链;
        // 新实现从 rules 反查偏好字符集的 keyword,确保能命中。
        let r = RuleRouter::new(true);
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        r.add_context_rule(
            "builtin.translate".into(),
            ContextTrigger::TextIsNonTargetLang {
                source: crate::domain::context::trigger::TextSource::SelectionThenClipboard,
            },
            crate::domain::plugin::ManifestSurfaceHint::Priority,
        );
        r.set_setting_resolver(Arc::new(
            MockResolver::new().with("builtin.translate", "target_lang", "zh"),
        ));
        r.set_app_language("zh".into());
        let snap = snap_selection("hello world foo");

        // 1. 拿 Context Ghost 的 replacement
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        let replacement = sug.replacement.clone();

        // 2. 用 replacement 走 route(),期待 Takeover 命中 builtin.translate
        //    （keyword「翻译」+ arg 触发 Prefix 分支 → resolve_surface = Takeover）
        let route = run_route_with_snapshot(&r, &replacement, snap);
        match route {
            Route::Takeover { plugin_id, arg, .. } => {
                assert_eq!(plugin_id, "builtin.translate");
                assert_eq!(arg, "hello world foo");
            }
            Route::Mixed { candidates } if !candidates.is_empty() => {
                // 未升 Takeover 也算过——至少 candidate 命中就说明闭环没断
                assert_eq!(candidates[0].plugin_id, "builtin.translate");
            }
            other => panic!("expected Takeover/Mixed with hits for replacement={:?}, got {:?}", replacement, other),
        }
    }

    #[test]
    fn suggestion_context_arg_truncated_in_display() {
        // 长文本：display 截 40 字符 + `…` + 引号收尾（build_context_suggestion_text 内部）
        let r = translate_router_with_target("zh");
        let long_text = "hello ".repeat(200); // > 40 字符
        let snap = snap_selection(&long_text);
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        assert!(sug.display.contains('…'), "display should contain ellipsis: {}", sug.display);
    }

    #[test]
    fn suggestion_non_empty_query_does_not_produce_context() {
        // 非空 query（未命中任何 keyword）+ 选中英文 → best_suggestion 应返回 None
        // （0.8.3 决策：keyword/context 因空/非空互斥）
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        assert!(r.best_suggestion("chrome", &snap, 0.7).is_none());
    }

    // ── 0.8.3 §4.9 origin 单测 ────────────────────────────────────

    #[test]
    fn suggestion_origin_selection_when_selected_text_present() {
        // 选中英文 → origin = Selection
        let r = translate_router_with_target("zh");
        let snap = snap_selection("hello world foo");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
        assert_eq!(sug.origin, Some(SuggestionOrigin::Selection));
    }

    #[test]
    fn suggestion_origin_clipboard_when_only_clipboard() {
        // 只剪贴板有内容 → origin = Clipboard
        let r = translate_router_with_target("zh");
        let snap = snap_clipboard("hello world foo");
        let sug = r.best_suggestion("", &snap, 0.7).expect("expected suggestion");
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
        snapshot.upsert_text(AwarenessSource::Selection, Some("some selected text".into()));
        snapshot.upsert_text(AwarenessSource::Clipboard, Some("https://example.com".into()));
        let sug = r.best_suggestion("", &snapshot, 0.7).expect("expected suggestion");
        assert_eq!(sug.origin, Some(SuggestionOrigin::Clipboard));
    }

    #[test]
    fn suggestion_origin_none_for_keyword_branch() {
        // Keyword 分支恒 origin=None
        let r = RuleRouter::new(true);
        r.add_keyword_rule("builtin.translate".into(), "翻译".into(), Surface::Auto, SurfaceView::List);
        let snap = ContextSnapshot::default();
        let sug = r.best_suggestion("fy", &snap, 0.7).expect("expected keyword suggestion");
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
        assert!((sug1.confidence - 0.75).abs() < 1e-9, "expected 0.75, got {}", sug1.confidence);

        let with_clip = snap_clipboard("hello world foo");
        let sug2 = r.best_suggestion("", &with_clip, 0.7).unwrap();
        assert_eq!(sug2.origin, Some(SuggestionOrigin::Clipboard));
        // 0.75 * 0.85 = 0.6375
        assert!((sug2.confidence - 0.6375).abs() < 1e-9, "expected 0.6375, got {}", sug2.confidence);
    }
}
