//! 意图路由层 —— 从 query 到呈现调度的转换(0.4)。
//!
//! 核心模型:触发(match)与呈现(surface)正交(见 `product-platform.md` §4.3)。
//! RuleRouter 持 keyword/regex 规则表,`route()` 判定命中后解出 surface(takeover/priority/inline),
//! 返回 `Route` 供 SearchService 调度。

use std::sync::RwLock;

use crate::context::ContextSnapshot;
use crate::text::normalize_candidates;

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

/// 单次命中类型。
enum MatchType {
    Exact,           // 精确命中(无参)
    Prefix(String),  // 前缀带参(余下文本)
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
}

#[async_trait::async_trait]
impl IntentRouter for RuleRouter {
    async fn route(&self, query: &str, _ctx: &QueryContext<'_>) -> Route {
        let q = query.trim();
        let rules = self.rules.read().unwrap();
        let takeover_enabled = *self.takeover_enabled.read().unwrap();

        let mut hits: Vec<Hit> = Vec::new();
        for rule in rules.iter() {
            let (matched, arg) = match &rule.kind {
                RuleKind::Keyword(kw) => match match_keyword(q, kw) {
                    Some(MatchType::Exact) => (true, String::new()),
                    Some(MatchType::Prefix(a)) => (true, a),
                    None => (false, String::new()),
                },
                RuleKind::Regex(re) => {
                    // regex 命中:无"参数"概念,但 auto 对 regex 视为强信号 → takeover。
                    // 传空 arg + Prefix 让 resolve_surface(Auto) 取 Takeover。
                    if re.is_match(q) {
                        (true, String::new())
                    } else {
                        (false, String::new())
                    }
                }
            };
            if matched {
                let mt = match &rule.kind {
                    // regex 无"参数"概念,但 auto 对 regex 视为强信号 → takeover。
                    // 故 regex 命中统一按 Prefix 处理(空参也 takeover)。
                    RuleKind::Regex(_) => MatchType::Prefix(arg.clone()),
                    RuleKind::Keyword(_) => {
                        if arg.is_empty() {
                            MatchType::Exact
                        } else {
                            MatchType::Prefix(arg.clone())
                        }
                    }
                };
                let actual = resolve_surface(rule.surface, &mt, takeover_enabled);
                hits.push(Hit {
                    plugin_id: rule.plugin_id.clone(),
                    arg,
                    surface: actual,
                    view: rule.view,
                });
            }
        }

        // 仲裁:有 Takeover 命中 → 取首个 takeover(规则表顺序 = manifest 加载顺序)
        if let Some(t) = hits.iter().find(|h| h.surface == Surface::Takeover) {
            return Route::Takeover {
                plugin_id: t.plugin_id.clone(),
                arg: t.arg.clone(),
                view: t.view,
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
                })
                .collect(),
        }
    }
}

// ── 内部辅助 ──────────────────────────────────────────────

struct Hit {
    plugin_id: String,
    arg: String,
    surface: Surface,
    view: SurfaceView,
}

/// 由 (声明 surface, 命中类型, 全局开关) 解出实际 surface。
fn resolve_surface(declared: Surface, mt: &MatchType, takeover_enabled: bool) -> Surface {
    let actual = match declared {
        Surface::Auto => match mt {
            MatchType::Exact => Surface::Priority,
            MatchType::Prefix(_) => Surface::Takeover,
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

/// keyword 匹配(§4.2):精确或前缀带参。
/// query 与 keyword 都过 `normalize_candidates`(小写 + 拼音首字母),使中文 keyword 支持首拼输入。
fn match_keyword(query: &str, keyword: &str) -> Option<MatchType> {
    let q_lower = query.to_ascii_lowercase();
    for kw in normalize_candidates(keyword) {
        if q_lower == kw {
            return Some(MatchType::Exact);
        }
        let prefix = format!("{kw} ");
        if q_lower.starts_with(&prefix) {
            return Some(MatchType::Prefix(query[prefix.len()..].trim().to_string()));
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
        let snapshot = crate::context::ContextSnapshot::default();
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
    fn pinyin_initials_keyword() {
        let r = RuleRouter::new(true);
        // 中文 keyword "天气" 支持首拼 "tq"
        r.add_keyword_rule(
            "builtin.weather".into(),
            "天气".into(),
            Surface::Auto,
            SurfaceView::List,
        );
        let route = run_route(&r, "tq 北京");
        assert!(
            matches!(route, Route::Takeover { plugin_id, arg, .. } if plugin_id == "builtin.weather" && arg == "北京")
        );
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
}
