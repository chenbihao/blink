//! 意图路由层 —— 从 query 到呈现调度的转换(0.4)。
//!
//! 核心模型:触发(match)与呈现(surface)正交(见 `product-platform.md` §4.3)。
//! RuleRouter 持 keyword/regex 规则表,`route()` 判定命中后解出 surface(takeover/priority/inline),
//! 返回 `Route` 供 SearchService 调度。

use std::sync::RwLock;

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
}

// ── RuleRouter ────────────────────────────────────────────

/// 规则匹配核心:纯同步、可单测。
pub struct RuleRouter {
    rules: RwLock<Vec<Rule>>,
    /// 全局总闸:为 false 时所有 Takeover 降级 Priority。
    takeover_enabled: RwLock<bool>,
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
        }
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

    /// 删除某个插件的所有规则（热更新时用）。
    pub fn remove_plugin_rules(&self, plugin_id: &str) {
        let mut rules = self.rules.write().unwrap();
        rules.retain(|r| r.plugin_id != plugin_id);
    }

    /// 重新加载某个插件的触发规则（热更新）。
    pub fn reload_plugin_triggers(
        &self,
        plugin_id: &str,
        triggers: &[crate::domain::plugin::PluginTrigger],
    ) {
        // 先删旧规则
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
            }
        }
        tracing::debug!(plugin_id, count = triggers.len(), "插件触发规则已重载");
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
    async fn route(&self, query: &str, _ctx: &QueryContext<'_>) -> Route {
        let q = query.trim();
        let rules = self.rules.read().unwrap();
        let takeover_enabled = *self.takeover_enabled.read().unwrap();

        let mut hits: Vec<Hit> = Vec::new();
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
                });
            }
        }

        // 仲裁:有 Takeover 命中 → 取首个 takeover(规则表顺序 = manifest 加载顺序)
        if let Some(t) = hits.iter().find(|h| h.surface == Surface::Takeover) {
            return Route::Takeover {
                plugin_id: t.plugin_id.clone(),
                arg: t.arg.clone(),
                view: t.view,
                hint: t.hint.clone(),
            };
        }

        // 否则全部进 Mixed(Inline / Priority)
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
}

// ── 内部辅助 ──────────────────────────────────────────────

struct Hit {
    plugin_id: String,
    arg: String,
    surface: Surface,
    view: SurfaceView,
    hint: Option<String>,
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
}
