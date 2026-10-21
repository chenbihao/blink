//! WebEngine：用户自管超链/搜索引擎（0.26.12，sync lane）。
//!
//! 两路工作：
//! - **EngineTakeover**（keyword 命中独占）：`gh` 直达出单条「GitHub」；
//!   `g rust` 出单条「Google 搜索 rust」。命中 keyword 经
//!   `QueryContext::takeover_keyword` 下传（0.26.12 `Route::EngineTakeover`
//!   扩展字段），engine 据此定位条目——多 keyword 引擎无法从剥离后的 arg 反推。
//! - **Mixed**（无 keyword 命中的常规查询）：仅**直达型**（模板不含 `{query}`）
//!   按 name/keyword/拼音首字母三字段 fuzzy 召回；搜索型不混排——否则任意输入
//!   都会弹出各家搜索建议，污染常规结果（2026-10-18 定案）。
//!
//! 执行统一走 `SearchAction::Open { path: url }` → 前端 `launchApp` → ShellExecute
//! 打开默认浏览器，与文件/应用共用链路，不新增执行入口。

use std::sync::{Arc, RwLock};

use crate::config::shards::{WebLinkEntry, WebLinksConfig};
use crate::intent::KeywordOwner;

use super::engine::{Lane, QueryContext, SearchAction, SearchEngine, SearchItem};

pub const ENGINE_ID: &str = "weblinks";

/// Mixed 混排召回上限。超链表通常 <50 条，上限只是防御异常大表拖慢热路径。
const MIXED_LIMIT: usize = 8;

pub struct WebEngine {
    config: Arc<RwLock<WebLinksConfig>>,
}

impl WebEngine {
    pub fn with_config(config: WebLinksConfig) -> Self {
        Self {
            config: Arc::new(RwLock::new(config)),
        }
    }

    /// 更新配置（保存 weblinks 分片时由 `SearchService::update_engine_config` 转发）。
    pub fn update_config(&self, config: WebLinksConfig) {
        *self.config.write().unwrap() = config;
    }

    /// 当前启用条目的 keyword 列表（注册 `add_engine_rule` 用；调用方负责冲突过滤）。
    pub fn effective_keywords(&self) -> Vec<String> {
        self.config
            .read()
            .unwrap()
            .entries
            .iter()
            .filter(|e| e.enabled)
            .map(|e| e.keyword.trim().to_string())
            .filter(|k| !k.is_empty())
            .collect()
    }
}

// ── 纯函数：URL 渲染 ──────────────────────────────────────────────────────────

/// 模板含 `{query}` 即搜索型。
pub fn is_search_template(template: &str) -> bool {
    template.contains("{query}")
}

/// `{query}` 参数的 UTF-8 percent-encoding。
///
/// RFC 3986 非保留字符（`A-Za-z0-9-._~`）原样保留，其余按 UTF-8 字节逐个
/// `%XX`。空格编为 `%20`（查询串通用；`+` 仅 form 编码语义，不采用）。
pub fn percent_encode_query(query: &str) -> String {
    let mut out = String::with_capacity(query.len());
    for &b in query.as_bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 渲染 URL 模板：每个 `{query}` 替换为 percent-encoding 后的参数；无占位符原样返回。
pub fn render_url(template: &str, query: &str) -> String {
    if !is_search_template(template) {
        return template.to_string();
    }
    template.replace("{query}", &percent_encode_query(query))
}

/// 只接受可由默认浏览器打开的 HTTP(S) 地址；模板占位符仅允许出现在路径/参数中。
fn valid_web_url(raw: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(raw.trim()).ok()?;
    (matches!(url.scheme(), "http" | "https")
        && url.host_str().is_some_and(|host| !host.contains(['{', '}']))
        && url.username().is_empty() && url.password().is_none()).then_some(url)
}

/// 主页手动配置优先；旧直达地址保留完整路径；搜索地址仅推导 origin，不猜注册域名。
pub fn homepage_url(entry: &WebLinkEntry) -> Option<String> {
    if !entry.home_url.trim().is_empty() {
        if is_search_template(&entry.home_url) { return None; }
        valid_web_url(&entry.home_url)?;
        return Some(entry.home_url.trim().to_string());
    }
    let url = valid_web_url(&entry.url_template)?;
    if !is_search_template(&entry.url_template) {
        return Some(entry.url_template.trim().to_string());
    }
    Some(format!("{}/", url.origin().ascii_serialization()))
}

/// 保存时校验主页与搜索地址。缺省主页可以推导；显式搜索地址必须保留 {query}。
pub fn validate_urls(entry: &WebLinkEntry) -> bool {
    homepage_url(entry).is_some()
        && (entry.url_template.trim().is_empty() || valid_web_url(&entry.url_template).is_some())
        && (entry.home_url.trim().is_empty() || entry.url_template.trim().is_empty()
            || is_search_template(&entry.url_template))
}

// ── 纯函数：keyword 冲突校验 ──────────────────────────────────────────────────

// 占用者类型在 intent 域（`RuleRouter::keyword_owner` 的返回）；search 依赖 intent 为既有方向。

/// 冲突类型（前端按 kind 做 i18n 文案，不解析 reason 字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum KeywordConflictKind {
    /// 空、含空白或全空白——前缀匹配依赖 `<keyword> ` 分隔，含空格永远无法命中。
    Invalid,
    /// 与本体保留前缀 `ai` 冲突（Route::AiTrigger 语义）。
    Reserved,
    /// 表内 keyword 重复（含禁用条目——开关切换后即暴露）。
    Duplicate,
    /// 已被其他 engine 触发词占用（`owner` 携带 engine id）。
    TakenByEngine,
    /// 已被插件触发词占用（`owner` 携带 plugin id）。
    TakenByPlugin,
}

/// 单条冲突（保存时整体拒绝并回传全量冲突，前端逐条标红）。
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct KeywordConflict {
    pub entry_id: String,
    pub keyword: String,
    pub kind: KeywordConflictKind,
    /// 占用者 id（仅 TakenBy* 变体）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
}

/// 校验表内全部条目的 keyword（保存与加载共用；外部占用经 `owner_of` 查询，
/// 传入 `RuleRouter::keyword_owner` 的闭包绑定——校验本身不依赖 router）。
///
/// 重复判定基于**全表**（含禁用条目）：禁用只是不注册，keyword 仍算占用，
/// 否则开关切换后静默冲突。
pub fn validate_keywords(
    entries: &[WebLinkEntry],
    owner_of: &dyn Fn(&str) -> Option<KeywordOwner>,
) -> Result<(), Vec<KeywordConflict>> {
    let mut conflicts = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for entry in entries {
        let kw = entry.keyword.trim();
        let invalid = kw.is_empty() || kw.chars().any(char::is_whitespace);
        let dup = seen.iter().any(|s| s.eq_ignore_ascii_case(kw));
        if invalid {
            conflicts.push(KeywordConflict {
                entry_id: entry.id.clone(),
                keyword: entry.keyword.clone(),
                kind: KeywordConflictKind::Invalid,
                owner: None,
            });
        } else if kw.eq_ignore_ascii_case("ai") {
            conflicts.push(KeywordConflict {
                entry_id: entry.id.clone(),
                keyword: kw.to_string(),
                kind: KeywordConflictKind::Reserved,
                owner: None,
            });
        } else if dup {
            conflicts.push(KeywordConflict {
                entry_id: entry.id.clone(),
                keyword: kw.to_string(),
                kind: KeywordConflictKind::Duplicate,
                owner: None,
            });
        } else if let Some(owner) = owner_of(kw) {
            let (kind, owner_id) = match &owner {
                KeywordOwner::Engine(id) => (KeywordConflictKind::TakenByEngine, id.clone()),
                KeywordOwner::Plugin(id) => (KeywordConflictKind::TakenByPlugin, id.clone()),
            };
            conflicts.push(KeywordConflict {
                entry_id: entry.id.clone(),
                keyword: kw.to_string(),
                kind,
                owner: Some(owner_id),
            });
            continue;
        }
        if !invalid {
            seen.push(kw.to_string());
        }
    }
    if conflicts.is_empty() {
        Ok(())
    } else {
        Err(conflicts)
    }
}

// ── SearchEngine 实现 ─────────────────────────────────────────────────────────

fn open_item(entry: &WebLinkEntry, title: String, url: String, score: f32) -> SearchItem {
    SearchItem {
        id: format!("weblink:{}", entry.id),
        title,
        subtitle: Some(url.clone()),
        score,
        action: SearchAction::Open { path: url },
        source: ENGINE_ID.to_string(),
        score_detail: None,
        context_aware: false,
        color_list_hex: None,
        pinned_until: 0,
    }
}

/// Takeover 条目标题（zh/en 双语；语言来自 `QueryContext.language`）。
fn takeover_title(entry: &WebLinkEntry, arg: &str, language: &str) -> String {
    let en = language.starts_with("en");
    if arg.is_empty() || !is_search_template(&entry.url_template) {
        return entry.name.clone();
    }
    if en {
        format!("Search {arg} with {}", entry.name)
    } else {
        format!("{} 搜索 {}", entry.name, arg)
    }
}

/// Mixed 三字段 fuzzy（name / keyword / name 拼音首字母，取最高分）。
/// 复用 search 域的 nucleo 原语（与 `fuzzy_entry_scores` 同构，多一个 keyword 字段）。
fn fuzzy_score_entry(
    pattern: &nucleo::pattern::Pattern,
    matcher: &mut nucleo::Matcher,
    entry: &WebLinkEntry,
    pinyin_initials: &str,
) -> u32 {
    let mut buf = Vec::new();
    [
        entry.name.as_str(),
        entry.keyword.as_str(),
        pinyin_initials,
    ]
    .into_iter()
    .map(|field| {
        let haystack = nucleo::Utf32Str::new(field, &mut buf);
        pattern.score(haystack, matcher).unwrap_or(0)
    })
    .max()
    .unwrap_or(0)
}

#[async_trait::async_trait]
impl SearchEngine for WebEngine {
    fn id(&self) -> &'static str {
        ENGINE_ID
    }

    fn lane(&self) -> Lane {
        Lane::Sync
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    /// 参与常规混排（直达型召回）；搜索型只经 Takeover 出现。
    fn takeover_only(&self) -> bool {
        false
    }

    async fn search(&self, query: &str, ctx: &QueryContext<'_>) -> Vec<SearchItem> {
        let cfg = self.config.read().unwrap().clone();

        // ── Takeover 路径：keyword 命中独占，query 是剥离后的参数 ──────────
        if let Some(kw) = ctx.takeover_keyword {
            let Some(entry) = cfg
                .entries
                .iter()
                .find(|e| e.enabled && e.keyword.trim().eq_ignore_ascii_case(kw.trim()))
            else {
                // keyword 未注册/条目被禁用/配置已变——静默空返回（注册侧已过滤冲突，
                // 正常配置不会走到这里；防御路由规则与配置短暂不一致的窗口）。
                tracing::debug!(keyword = kw, "weblinks keyword 未命中启用条目");
                return Vec::new();
            };
            let arg = query.trim();
            let url = if arg.is_empty() || !is_search_template(&entry.url_template) {
                homepage_url(entry)
            } else {
                valid_web_url(&entry.url_template).map(|_| render_url(&entry.url_template, arg))
            };
            let Some(url) = url else { return Vec::new(); };
            let title = takeover_title(entry, arg, ctx.language);
            return vec![open_item(entry, title, url, 1.0)];
        }

        // ── Mixed 路径：仅直达型 fuzzy 召回 ────────────────────────────────
        let q = query.trim();
        if q.is_empty() {
            return Vec::new();
        }
        let candidates: Vec<&WebLinkEntry> = cfg
            .entries
            .iter()
            .filter(|e| e.enabled && !is_search_template(&e.url_template) && homepage_url(e).is_some())
            .collect();
        if candidates.is_empty() {
            return Vec::new();
        }

        let query_lower = q.to_ascii_lowercase();
        let mut matcher = nucleo::Matcher::new(nucleo::Config::DEFAULT);
        let pattern = nucleo::pattern::Pattern::new(
            &query_lower,
            nucleo::pattern::CaseMatching::Smart,
            nucleo::pattern::Normalization::Smart,
            nucleo::pattern::AtomKind::Fuzzy,
        );
        let mut scored: Vec<(u32, &WebLinkEntry)> = candidates
            .into_iter()
            .filter_map(|e| {
                let initials = super::to_pinyin_initials(&e.name);
                let score = fuzzy_score_entry(&pattern, &mut matcher, e, &initials);
                (score > 0).then_some((score, e))
            })
            .collect();
        if scored.is_empty() {
            return Vec::new();
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0));
        let best = scored[0].0;
        scored
            .into_iter()
            .take(MIXED_LIMIT)
            .filter_map(|(raw, e)| homepage_url(e).map(|url| open_item(e, e.name.clone(), url, raw as f32 / best as f32)))
            .collect()
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use blink_infra::platform::context::ContextSnapshot;

    fn entry(id: &str, name: &str, keyword: &str, url: &str, enabled: bool) -> WebLinkEntry {
        WebLinkEntry {
            id: id.into(),
            name: name.into(),
            keyword: keyword.into(),
            home_url: String::new(),
            url_template: url.into(),
            enabled,
        }
    }

    fn ctx<'a>(
        history: &'a std::collections::HashMap<String, (i64, i64)>,
        snapshot: &'a ContextSnapshot,
        takeover_keyword: Option<&'a str>,
    ) -> QueryContext<'a> {
        QueryContext {
            history,
            snapshot,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
            takeover_keyword,
        }
    }

    async fn run(engine: &WebEngine, query: &str, kw: Option<&str>) -> Vec<SearchItem> {
        let h = std::collections::HashMap::new();
        let snap = ContextSnapshot::default();
        engine.search(query, &ctx(&h, &snap, kw)).await
    }

    fn engine_with(entries: Vec<WebLinkEntry>) -> WebEngine {
        WebEngine::with_config(WebLinksConfig {
            entries,
            seeded: true,
        })
    }

    #[tokio::test]
    async fn homepage_override_and_search_use_separate_destinations() {
        let mut site = entry("bili", "Bilibili", "bili", "https://search.bilibili.com/all?keyword={query}", true);
        site.home_url = "https://www.bilibili.com/".into();
        let e = engine_with(vec![site]);
        for query in ["", "  "] {
            let items = run(&e, query, Some("bili")).await;
            assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "https://www.bilibili.com/"));
        }
        let items = run(&e, "测试 & Rust", Some("bili")).await;
        assert!(matches!(&items[0].action, SearchAction::Open { path }
            if path == "https://search.bilibili.com/all?keyword=%E6%B5%8B%E8%AF%95%20%26%20Rust"));
    }

    #[test]
    fn homepage_inference_preserves_origin_and_legacy_direct_path() {
        let mut site = entry("s", "S", "s", "https://sub.example.co.uk:8443/find?q={query}#results", true);
        assert_eq!(homepage_url(&site).as_deref(), Some("https://sub.example.co.uk:8443/"));
        site.url_template = "https://example.com/projects?sort=new#top".into();
        assert_eq!(homepage_url(&site).as_deref(), Some(site.url_template.as_str()));
        site.home_url = "https://example.com/portal".into();
        site.url_template.clear();
        assert!(validate_urls(&site));
        assert_eq!(homepage_url(&site).as_deref(), Some("https://example.com/portal"));
        for invalid in ["javascript:alert(1)", "https://", "https://user:pass@example.com", "https://example.com/{query}"] {
            site.home_url = invalid.into();
            assert!(!validate_urls(&site), "{invalid}");
        }
    }

    #[test]
    fn old_config_upgrades_only_unchanged_seed_homepage() {
        let mut cfg = WebLinksConfig::seed();
        cfg.entries.iter_mut().for_each(|e| e.home_url.clear());
        let old_json = serde_json::to_string(&cfg).unwrap();
        let mut loaded: WebLinksConfig = serde_json::from_str(&old_json).unwrap();
        assert!(loaded.upgrade_home_urls());
        let bili = loaded.entries.iter().find(|e| e.id == "bilibili").unwrap();
        assert_eq!(homepage_url(bili).as_deref(), Some("https://www.bilibili.com/"));
        assert!(!loaded.upgrade_home_urls(), "升级幂等");
        let bili = cfg.entries.iter_mut().find(|e| e.id == "bilibili").unwrap();
        bili.url_template = "https://example.com/custom?q={query}".into();
        assert!(!cfg.upgrade_home_urls(), "不覆盖自定义地址");
        cfg.entries.clear();
        assert!(!cfg.upgrade_home_urls(), "不恢复用户清空的表");
    }

    #[tokio::test]
    async fn github_seed_upgrade_preserves_choices_and_supports_search() {
        let mut cfg = WebLinksConfig::seed();
        let github = cfg.entries.iter_mut().find(|e| e.id == "github").unwrap();
        github.name = "My GitHub".into();
        github.keyword = "git".into();
        github.enabled = false;
        github.url_template = "https://github.com".into();
        assert!(cfg.upgrade_home_urls());
        let github = cfg.entries.iter().find(|e| e.id == "github").unwrap();
        assert_eq!(github.name, "My GitHub");
        assert_eq!(github.keyword, "git");
        assert!(!github.enabled);
        assert_eq!(homepage_url(github).as_deref(), Some("https://github.com/"));
        assert_eq!(github.url_template, "https://github.com/search?q={query}&ref=opensearch");
        assert!(!cfg.upgrade_home_urls());
        let github = cfg.entries.iter_mut().find(|e| e.id == "github").unwrap();
        github.url_template = "https://github.com/orgs/custom".into();
        github.home_url.clear();
        assert!(!cfg.upgrade_home_urls());

        let e = engine_with(WebLinksConfig::seed().entries);
        let items = run(&e, "rust & 中文", Some("gh")).await;
        assert!(matches!(&items[0].action, SearchAction::Open { path }
            if path == "https://github.com/search?q=rust%20%26%20%E4%B8%AD%E6%96%87&ref=opensearch"));
        let items = run(&e, "", Some("gh")).await;
        assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "https://github.com/"));
    }

    // ── percent-encoding / URL 渲染 ──────────────────────────────────────

    #[test]
    fn percent_encode_keeps_unreserved_and_encodes_rest() {
        assert_eq!(percent_encode_query("rust-lang_1.0~"), "rust-lang_1.0~");
        assert_eq!(percent_encode_query("a b"), "a%20b");
        assert_eq!(percent_encode_query("你好"), "%E4%BD%A0%E5%A5%BD");
        assert_eq!(
            percent_encode_query("a&b=c+d?e#f"),
            "a%26b%3Dc%2Bd%3Fe%23f"
        );
        assert_eq!(percent_encode_query("%"), "%25");
        assert_eq!(percent_encode_query(""), "");
    }

    #[test]
    fn render_url_replaces_all_placeholders_or_none() {
        assert_eq!(
            render_url("https://s.test/q?x={query}&y={query}", "中文 查"),
            "https://s.test/q?x=%E4%B8%AD%E6%96%87%20%E6%9F%A5&y=%E4%B8%AD%E6%96%87%20%E6%9F%A5"
        );
        assert_eq!(render_url("https://gh.test/user", "任何"), "https://gh.test/user");
        assert_eq!(render_url("https://s.test/q={query}", ""), "https://s.test/q=");
    }

    #[test]
    fn search_template_detection() {
        assert!(is_search_template("https://a.test/s?q={query}"));
        assert!(!is_search_template("https://a.test"));
    }

    // ── Takeover 路径 ────────────────────────────────────────────────────

    #[tokio::test]
    async fn takeover_search_empty_arg_opens_homepage() {
        let e = engine_with(vec![entry(
            "g",
            "Google",
            "g",
            "https://google.com/search?q={query}",
            true,
        )]);
        let items = run(&e, "", Some("g")).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Google");
        assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "https://google.com/"));
    }

    #[tokio::test]
    async fn takeover_search_with_arg_renders_encoded_url() {
        let e = engine_with(vec![entry(
            "g",
            "Google",
            "g",
            "https://google.com/search?q={query}",
            true,
        )]);
        let items = run(&e, "rust 泛型", Some("g")).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "Google 搜索 rust 泛型");
        assert!(matches!(&items[0].action, SearchAction::Open { path }
            if path == "https://google.com/search?q=rust%20%E6%B3%9B%E5%9E%8B"));
    }

    #[tokio::test]
    async fn takeover_direct_entry_opens_template_and_ignores_arg() {
        let e = engine_with(vec![entry("gh", "GitHub", "gh", "https://github.com", true)]);
        let items = run(&e, "任意参数", Some("GH")).await;
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].title, "GitHub");
        assert!(matches!(&items[0].action, SearchAction::Open { path } if path == "https://github.com"));
    }

    #[tokio::test]
    async fn takeover_title_follows_language() {
        let e = engine_with(vec![entry(
            "g",
            "Google",
            "g",
            "https://google.com/search?q={query}",
            true,
        )]);
        let h = std::collections::HashMap::new();
        let snap = ContextSnapshot::default();
        let zh = QueryContext {
            history: &h,
            snapshot: &snap,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "zh",
            takeover_keyword: Some("g"),
        };
        assert_eq!(e.search("rust", &zh).await[0].title, "Google 搜索 rust");
        let en = QueryContext {
            history: &h,
            snapshot: &snap,
            disabled_builtin_actions: &[],
            disabled_context_bindings: &[],
            language: "en-US",
            takeover_keyword: Some("g"),
        };
        assert_eq!(e.search("rust", &en).await[0].title, "Search rust with Google");
        assert_eq!(
            e.search("", &en).await[0].title,
            "Google"
        );
    }

    #[tokio::test]
    async fn takeover_keyword_miss_or_disabled_entry_returns_empty() {
        let e = engine_with(vec![
            entry("gh", "GitHub", "gh", "https://github.com", false),
            entry("zh", "知乎", "zh", "https://zhihu.com", true),
        ]);
        assert!(run(&e, "", Some("gh")).await.is_empty(), "禁用条目不响应");
        assert!(run(&e, "", Some("none")).await.is_empty(), "未注册 keyword 空");
    }

    #[tokio::test]
    async fn update_config_hot_swaps_entries() {
        let e = engine_with(vec![entry("gh", "GitHub", "gh", "https://github.com", true)]);
        assert_eq!(run(&e, "", Some("gh")).await.len(), 1);
        e.update_config(WebLinksConfig {
            entries: vec![],
            seeded: true,
        });
        assert!(run(&e, "", Some("gh")).await.is_empty(), "清空后不再命中");
        assert!(e.effective_keywords().is_empty());
    }

    // ── Mixed 路径 ───────────────────────────────────────────────────────

    #[tokio::test]
    async fn mixed_recalls_direct_entries_by_name_keyword_and_pinyin() {
        let e = engine_with(vec![
            entry("gh", "GitHub", "gh", "https://github.com", true),
            entry("zh", "知乎", "zh", "https://zhihu.com", true),
            entry("bili", "哔哩哔哩", "bili", "https://bilibili.com", true),
        ]);
        // name 前缀
        assert!(run(&e, "gith", None).await.iter().any(|i| i.title == "GitHub"));
        // keyword 精确/前缀
        assert!(run(&e, "zh", None).await.iter().any(|i| i.title == "知乎"));
        // 拼音首字母（哔哩哔哩 → blbl；"bili" 同时是 keyword，双路命中）
        assert!(run(&e, "bl", None).await.iter().any(|i| i.title == "哔哩哔哩"));
    }

    #[tokio::test]
    async fn mixed_excludes_search_templates_and_empty_query() {
        let e = engine_with(vec![
            entry("g", "Google", "g", "https://google.com/search?q={query}", true),
            entry("gh", "GitHub", "gh", "https://github.com", true),
        ]);
        // "g" 若搜索型混排会命中 Google——定案：搜索型只在 keyword 触发时出现
        let items = run(&e, "g", None).await;
        assert!(items.iter().all(|i| i.title != "Google"), "搜索型不混排: {items:?}");
        assert!(items.iter().any(|i| i.title == "GitHub"));
        assert!(run(&e, "", None).await.is_empty(), "空 query 无召回");
    }

    #[tokio::test]
    async fn mixed_skips_disabled_entries_and_normalizes_score() {
        let e = engine_with(vec![
            entry("gh", "GitHub", "gh", "https://github.com", false),
            entry("gt", "Gitee", "gt", "https://gitee.com", true),
        ]);
        let items = run(&e, "g", None).await;
        assert!(items.iter().all(|i| i.title != "GitHub"), "禁用不召回");
        assert!(!items.is_empty());
        assert!((items[0].score - 1.0).abs() < f32::EPSILON, "最高分归一 1.0");
    }

    #[tokio::test]
    async fn mixed_scores_are_top_relative() {
        let e = engine_with(vec![
            entry("gh", "GitHub", "gh", "https://github.com", true),
            entry("gt", "Gitee", "gt", "https://gitee.com", true),
        ]);
        let items = run(&e, "git", None).await;
        assert!(items.len() >= 2);
        assert!(items[0].score >= items[1].score);
        assert!(items.iter().all(|i| i.score > 0.0 && i.score <= 1.0));
    }

    // ── keyword 冲突校验 ─────────────────────────────────────────────────

    #[test]
    fn validate_rejects_invalid_reserved_and_duplicate() {
        let entries = vec![
            entry("a", "A", "go od", "https://a.test", true),   // 含空白
            entry("b", "B", "  ", "https://b.test", true),        // 全空白
            entry("c", "C", "ai", "https://c.test", true),        // 保留前缀
            entry("d", "D", "dup", "https://d.test", true),
            entry("e", "E", "DUP", "https://e.test", false),      // 表内重复（跨大小写、跨禁用）
        ];
        let err = validate_keywords(&entries, &|_| None).unwrap_err();
        assert_eq!(err.len(), 4);
        assert_eq!(err[0].kind, KeywordConflictKind::Invalid);
        assert_eq!(err[1].kind, KeywordConflictKind::Invalid);
        assert_eq!(err[2].kind, KeywordConflictKind::Reserved);
        assert_eq!(err[3].kind, KeywordConflictKind::Duplicate);
    }

    #[test]
    fn validate_reports_external_owners_and_allows_clean_table() {
        let entries = vec![
            entry("a", "A", "clip", "https://a.test", true),     // engine 占用
            entry("b", "B", "翻译", "https://b.test", true),      // 插件占用
            entry("c", "C", "ok", "https://c.test", true),
        ];
        let err = validate_keywords(&entries, &|kw| match kw {
            "clip" => Some(KeywordOwner::Engine("clipboard".into())),
            "翻译" => Some(KeywordOwner::Plugin("builtin.translate".into())),
            _ => None,
        })
        .unwrap_err();
        assert_eq!(err.len(), 2);
        assert_eq!(err[0].kind, KeywordConflictKind::TakenByEngine);
        assert_eq!(err[0].owner.as_deref(), Some("clipboard"));
        assert_eq!(err[1].kind, KeywordConflictKind::TakenByPlugin);
        assert_eq!(err[1].owner.as_deref(), Some("builtin.translate"));

        let clean = vec![entry("c", "C", "ok", "https://c.test", true)];
        assert!(validate_keywords(&clean, &|_| None).is_ok());
    }

    #[test]
    fn seed_table_passes_internal_validation() {
        // 种子表自身必须通过表内校验（无重复/保留/非法），否则首启即陷入拒绝循环。
        let seed = WebLinksConfig::seed();
        assert!(seed.seeded);
        assert_eq!(seed.entries.len(), 7);
        assert!(validate_keywords(&seed.entries, &|_| None).is_ok());
        // Google 收录但默认禁用
        let google = seed.entries.iter().find(|e| e.id == "google").unwrap();
        assert!(!google.enabled);
    }

    // ── 路由与注册联动 ───────────────────────────────────────────────────

    use crate::intent::{ExecArg, IntentRouter, KeywordOwner, Route};
    use crate::search::register_weblinks_rules;

    async fn route_of(r: &crate::intent::RuleRouter, q: &str) -> Route {
        let h = std::collections::HashMap::new();
        r.route(q, &h, None).await
    }

    #[tokio::test]
    async fn engine_takeover_carries_hit_keyword() {
        let r = crate::intent::RuleRouter::new(true);
        register_weblinks_rules(&r, &WebLinksConfig::seed());
        // 带参：arg 剥离 + keyword 回传注册词原文（种子表 gh=GitHub 默认启用）
        assert!(matches!(
            &route_of(&r, "gh").await,
            Route::EngineTakeover { engine_id, keyword, arg }
                if engine_id == ENGINE_ID
                    && keyword.as_deref() == Some("gh")
                    && matches!(arg, ExecArg::None)
        ));
        assert!(matches!(
            &route_of(&r, "bing rust 泛型").await,
            Route::EngineTakeover { keyword, arg, .. }
                if keyword.as_deref() == Some("bing")
                    && matches!(arg, ExecArg::UserExplicit(s) if s == "rust 泛型")
        ));
        // 大小写归一：输入 GH，回传注册词原文 gh
        assert!(matches!(
            &route_of(&r, "GH").await,
            Route::EngineTakeover { keyword, .. } if keyword.as_deref() == Some("gh")
        ));
    }

    #[tokio::test]
    async fn route_extension_keeps_clipboard_engine_behavior() {
        // Route::EngineTakeover 增加 keyword 字段不影响剪贴板既有行为（验收项）
        let r = crate::intent::RuleRouter::new(true);
        crate::search::register_engine_rules(&r);
        assert!(matches!(
            &route_of(&r, "clip hello").await,
            Route::EngineTakeover { engine_id, keyword, arg }
                if engine_id == "clipboard"
                    && matches!(arg, ExecArg::UserExplicit(s) if s == "hello")
                    && keyword.as_deref() == Some("clip")
        ));
        assert!(matches!(
            route_of(&r, "剪贴板").await,
            Route::EngineTakeover { engine_id, .. } if engine_id == "clipboard"
        ));
    }

    #[test]
    fn register_skips_conflicting_keywords() {
        let r = crate::intent::RuleRouter::new(true);
        crate::search::register_engine_rules(&r); // clipboard: 剪贴板/clip/...
        r.add_keyword_rule(
            "builtin.translate".into(),
            "翻译".into(),
            crate::intent::Surface::Auto,
            crate::intent::SurfaceView::List,
        );
        let cfg = WebLinksConfig {
            entries: vec![
                entry("a", "A", "clip", "https://a.test", true), // engine 冲突 → 跳过
                entry("b", "B", "翻译", "https://b.test", true),  // 插件冲突 → 跳过
                entry("c", "C", "ok", "https://c.test", true),
                entry("d", "D", "OK", "https://d.test", true), // 表内重复（跨大小写）→ 只注册首条
            ],
            seeded: true,
        };
        let skipped = register_weblinks_rules(&r, &cfg);
        assert_eq!(skipped, vec!["clip".to_string(), "翻译".to_string()]);
        assert_eq!(r.keyword_owner("ok", Some(ENGINE_ID)), None);
        assert_eq!(
            r.keyword_owner("clip", Some(ENGINE_ID)),
            Some(KeywordOwner::Engine("clipboard".into()))
        );
        assert_eq!(
            r.keyword_owner("翻译", Some(ENGINE_ID)),
            Some(KeywordOwner::Plugin("builtin.translate".into()))
        );
    }

    #[tokio::test]
    async fn empty_or_cleared_weblinks_does_not_take_over() {
        // 用户清空/全部禁用 → 注入空 keywords 表，路由不再命中 EngineTakeover
        let r = crate::intent::RuleRouter::new(true);
        register_weblinks_rules(&r, &WebLinksConfig::seed());
        assert!(matches!(
            route_of(&r, "gh").await,
            Route::EngineTakeover { .. }
        ));
        register_weblinks_rules(&r, &WebLinksConfig { entries: vec![], seeded: true });
        for q in ["gh", "gh rust", "bing", "wiki"] {
            assert!(
                matches!(route_of(&r, q).await, Route::Mixed { .. }),
                "q={q}"
            );
        }
    }

    #[tokio::test]
    async fn disabled_entries_do_not_register() {
        let r = crate::intent::RuleRouter::new(true);
        let cfg = WebLinksConfig {
            entries: vec![entry("g", "Google", "g", "https://g.test", false)],
            seeded: true,
        };
        register_weblinks_rules(&r, &cfg);
        assert!(matches!(
            route_of(&r, "g rust").await,
            Route::Mixed { .. }
        ));
    }
}
