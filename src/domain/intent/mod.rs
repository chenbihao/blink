//! 意图路由层 —— 从 query 到呈现调度的转换(0.4)。
//!
//! 核心模型:触发(match)与呈现(surface)正交(见 `product-platform.md` §4.3)。
//! RuleRouter 持 keyword/regex/context 规则表,`route()` 判定命中后解出 surface(takeover/priority/inline),
//! 返回 `Route` 供 SearchService 调度。
//!
//! 0.8.2 §3.4 加 Context 规则表：与 keyword/regex 表并存,专用于「非 query 依赖」的
//! 触发信号（选区/剪贴板/前台）。`TextIsNonTargetLang` 需 target 语言,通过
//! `PluginSettingResolver` trait 反转读插件 settings(`target_lang`)。

use std::sync::{Arc, RwLock};

use crate::domain::context::trigger::{self as ctx_trigger, ContextTrigger};
use crate::domain::plugin::PluginSettingResolver;
use crate::infra::platform::context::ContextSnapshot;
use crate::infra::utils::text::{pinyin_full, pinyin_initials};

pub mod suggest;
pub use suggest::CompletionHint;

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

    /// 更新界面语言快照（0.8.2 §3.4）。默认 no-op；`RuleRouter` 覆写以支持
    /// `TextIsNonTargetLang` 中 `target_lang="auto"` 的回退。命令层 `update_language`
    /// 通过 `SearchService::update_language` 转发至此。
    fn set_app_language(&self, _language: String) {}
}

// ── RuleRouter ────────────────────────────────────────────

/// 规则匹配核心:纯同步、可单测。
///
/// 0.8.2 §3.4 起持三张规则表：keyword / regex（原有）+ context（新增）。
/// Context 规则通过 `PluginSettingResolver` 反查插件 `target_lang`（`auto` → `app_language`）。
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
                    });
                }
            }
        }

        // ── 2. Context 匹配（0.8.2 §3.4，不受 query 影响）──────
        let context_hits = self.match_context_rules(ctx.snapshot);

        // ── 3. 合并（keyword + context 同 plugin 取 max surface / kw 优先 arg）──
        let hits = merge_hits(hits, context_hits);

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

    fn set_app_language(&self, language: String) {
        // 委托到 inherent method(单测直接用 RuleRouter 类型,生产环境走 trait)
        RuleRouter::set_app_language(self, language);
    }
}

impl RuleRouter {
    /// 扫描 Context 规则表，返回命中的 Hit 列表（0.8.2 §3.4）。
    ///
    /// 对每条规则：
    /// 1. 解析 target（仅 `TextIsNonTargetLang` 需要）：优先插件 `target_lang` → `auto` 回退 `app_language` → None 回退 `app_language`
    /// 2. `is_hit` 判定命中
    /// 3. 从 `TextSource` 抽 arg，长度 > 2000 截断（§3.4 边界约定）
    /// 4. arg 为 None → 不召回（Context 门禁）
    /// 5. 空 query 场景：`base_score = 0.9`（略低于内置参数化 Action 的 1.0）——本 Hit 里
    ///    不直接携带 score，交给下游 `SearchService` 的 `placeholder_score` 决定；此处只标 surface。
    fn match_context_rules(&self, snapshot: &ContextSnapshot) -> Vec<Hit> {
        let rules = self.context_rules.read().unwrap();
        if rules.is_empty() {
            return Vec::new();
        }
        tracing::trace!(
            rule_count = rules.len(),
            has_selection = snapshot.selected_text.is_some(),
            has_clipboard = snapshot.clipboard_text.is_some(),
            "扫描 context 规则",
        );
        let app_lang = self.app_language.read().unwrap().clone();
        let resolver = self.settings.read().unwrap().clone();

        let mut out = Vec::new();
        for rule in rules.iter() {
            // 1. 解析 target
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

            // 2. 命中判定
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

            // 3. 抽 arg：仅 `TextIsNonTargetLang` 需要抽（source 指定）；其他 Context trigger 是"事件性"
            //    命中，本身无参数——arg="" 即可，交由插件根据 keyword 参数或 snapshot 自决。
            let arg = match &rule.when {
                ContextTrigger::TextIsNonTargetLang { source } => {
                    let raw = source.extract(snapshot).unwrap_or("");
                    truncate_arg(raw)
                }
                _ => String::new(),
            };

            // 4. 门禁：TextIsNonTargetLang 抽不到 arg → 不召回
            //    （arg="" 场景是 event-only 触发，不受此闸约束）
            if matches!(rule.when, ContextTrigger::TextIsNonTargetLang { .. }) && arg.is_empty() {
                tracing::trace!(plugin = %rule.plugin_id, "context 命中但 arg 空,跳过召回");
                continue;
            }

            tracing::trace!(
                plugin = %rule.plugin_id,
                arg_len = arg.chars().count(),
                surface = ?rule.surface,
                "context 规则产出 Hit",
            );

            out.push(Hit {
                plugin_id: rule.plugin_id.clone(),
                arg,
                surface: rule.surface,
                view: SurfaceView::List,
                hint: None,
                source: HitSource::Context,
            });
        }
        out
    }
}

// ── 内部辅助 ──────────────────────────────────────────────

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
/// **arg 截断**：`truncate_arg` 在 `match_context_rules` 内做；`merge_hits` 只组合。
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

    fn snap_selection(text: &str) -> ContextSnapshot {
        ContextSnapshot {
            selected_text: Some(text.to_string()),
            ..ContextSnapshot::default()
        }
    }

    fn snap_clipboard(text: &str) -> ContextSnapshot {
        ContextSnapshot {
            clipboard_text: Some(text.to_string()),
            ..ContextSnapshot::default()
        }
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
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.translate"
                && candidates[0].arg == "hello world foo"
                && matches!(candidates[0].surface, Surface::Priority)
        ));
    }

    #[test]
    fn context_hit_clipboard_english_triggers_translate() {
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_clipboard("hello world foo"));
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.translate"
                && matches!(candidates[0].surface, Surface::Priority)
        ));
    }

    #[test]
    fn context_hit_selection_beats_clipboard() {
        // selection 非空时不看 clipboard
        let r = translate_router_with_target("zh");
        let snapshot = ContextSnapshot {
            selected_text: Some("selected english text".to_string()),
            clipboard_text: Some("clipboard content here".to_string()),
            ..ContextSnapshot::default()
        };
        let route = run_route_with_snapshot(&r, "", snapshot);
        if let Route::Mixed { candidates } = route {
            assert_eq!(candidates[0].arg, "selected english text");
        } else {
            panic!("expected Mixed");
        }
    }

    #[test]
    fn context_hit_url_guard_no_translation() {
        // P0-2 关键回归：剪贴板是 URL → 翻译**不**触发
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_clipboard("https://github.com/anthropics/foo"));
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn context_hit_file_path_guard_no_translation() {
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_clipboard(r"C:\Users\a\file.txt"));
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn context_hit_short_text_no_translation() {
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_selection("hi"));
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn context_hit_same_family_no_translation() {
        // target=zh + selection 是中文 → 不触发
        let r = translate_router_with_target("zh");
        let route = run_route_with_snapshot(&r, "", snap_selection("你好世界"));
        assert!(matches!(route, Route::Mixed { candidates } if candidates.is_empty()));
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
        // app_language=en，selection 是中文 → 触发翻译
        let route = run_route_with_snapshot(&r, "", snap_selection("你好世界啊"));
        assert!(matches!(
            route,
            Route::Mixed { candidates } if candidates.len() == 1
                && candidates[0].plugin_id == "builtin.translate"
        ));
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
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(route, Route::Mixed { candidates } if !candidates.is_empty()));
    }

    #[test]
    fn context_hit_arg_truncated_at_2000_chars() {
        let r = translate_router_with_target("zh");
        let long_text = "a".repeat(3000);
        let route = run_route_with_snapshot(&r, "", snap_selection(&long_text));
        if let Route::Mixed { candidates } = route {
            assert_eq!(candidates.len(), 1);
            // 2000 char + '…'
            assert_eq!(candidates[0].arg.chars().count(), 2001);
            assert!(candidates[0].arg.ends_with('…'));
        } else {
            panic!("expected Mixed");
        }
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
        // 两个 plugin 都声明 TextIsNonTargetLang → 两个 candidate 并存
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
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        if let Route::Mixed { candidates } = route {
            assert_eq!(candidates.len(), 2);
        } else {
            panic!("expected Mixed");
        }
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
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(route, Route::Mixed { candidates } if !candidates.is_empty()));
    }

    #[test]
    fn context_hit_reload_clears_context_rules() {
        // reload_plugin_triggers 应清掉旧 context 规则
        let r = translate_router_with_target("zh");
        // 原来能命中
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(&route, Route::Mixed { candidates } if !candidates.is_empty()));

        // 重载空 triggers → context 规则清空
        r.reload_plugin_triggers("builtin.translate", &[]);
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        assert!(matches!(&route, Route::Mixed { candidates } if candidates.is_empty()));
    }

    #[test]
    fn context_hit_inline_declared_downgrades_to_priority() {
        // manifest 侧 surface=inline → warn+降级 Priority（0.8.2 收窄）
        let r = RuleRouter::new(true);
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
        let route = run_route_with_snapshot(&r, "", snap_selection("hello world foo"));
        if let Route::Mixed { candidates } = route {
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
        }];
        let ctx = vec![Hit {
            plugin_id: "p".into(),
            arg: "bar".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Context,
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
        }];
        let ctx = vec![Hit {
            plugin_id: "p".into(),
            arg: "bar".into(),
            surface: Surface::Priority,
            view: SurfaceView::List,
            hint: None,
            source: HitSource::Context,
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
}
