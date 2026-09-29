//! ContextProducer：环境感知 Suggestion 生产者（0.8.6 §8.1.2；0.24.2 扩为 Translate
//! 域唯一生产者；0.24.8 增打开类候选）。
//!
//! 产出候选（§3.4 rank 表）：
//! - Translate 域：`translate-query` 0.92 / `translate-selection` 0.92 /
//!   `translate-clipboard` 0.82
//! - 打开类（0.24.8）：`open-url-*` 0.95 / `open-path-*` 0.93 / `reveal-*` 0.88
//!   ——URL 与文件路径互斥判定（`is_url` / `is_file_path`），query 派生（输入框里
//!   粘贴的 URL/路径，origin=None）与 awareness 派生（剪贴板，origin=Clipboard）
//!   各自独立产出；同 Kind 由 Coordinator 去重，query 派生经分层优先。
//!
//! 命中判定委托 `RuleRouter`（context 规则表 + `PluginSettingResolver`）——
//! 翻译插件启用态/binding 黑名单/target_lang 解析都在 router 侧，与
//! 0.8.2 起的 manifest context 路径同源。**非空 query 不再短路**：awareness 派生
//! 候选在非空 query 下参选，最多占 secondary（分层在 Coordinator，§3.4）。
//!
//! **打开类禁用一致性**（0.24.8）：与 BuiltinEngine result 侧同源检查——
//! `disabled_builtin_actions`（动作粒度，query/awareness 派生都拦）与
//! `disabled_context_bindings`（binding 粒度，只拦 awareness 派生；query 派生
//! 是用户输入意图，不受环境 binding 约束）。两份状态经共享 cell 注入
//! （KeywordProducer 读 `min_score` 同款模式），设置页保存即热生效，
//! 不会出现"result 侧禁用了、建议侧还在提示"。

use std::sync::{Arc, RwLock};

use crate::domain::context::probe;
use crate::domain::intent::RuleRouter;
use crate::infra::platform::context::{AwarenessSnapshot, AwarenessSource};

use super::producer::SuggestionProducer;
use super::{
    Suggestion, SuggestionAction, SuggestionKind, SuggestionOrigin, SuggestionSource,
    text_fingerprint,
};

/// 打开类候选 rank 值（0.24 §3.4 rank 表 0.24.8 激活行）。
const RANK_OPEN_URL: f64 = 0.95;
const RANK_OPEN_PATH: f64 = 0.93;
const RANK_REVEAL: f64 = 0.88;

/// Translate 域 + 打开类的环境感知 Suggestion 生产者。
pub struct ContextProducer {
    router: Arc<RuleRouter>,
    /// 与 SearchService 共享的禁用动作列表（`disabled_builtin_actions`）。
    disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
    /// 与 SearchService 共享的禁用 binding key 列表（`disabled_context_bindings`）。
    disabled_context_bindings: Arc<RwLock<Vec<String>>>,
}

impl ContextProducer {
    pub fn new(
        router: Arc<RuleRouter>,
        disabled_builtin_actions: Arc<RwLock<Vec<String>>>,
        disabled_context_bindings: Arc<RwLock<Vec<String>>>,
    ) -> Self {
        Self {
            router,
            disabled_builtin_actions,
            disabled_context_bindings,
        }
    }

    /// 动作粒度禁用（query/awareness 派生都拦，与 result 侧 `disabled_builtin_actions` 同源）。
    fn action_disabled(&self, action_id: &str) -> bool {
        self.disabled_builtin_actions
            .read()
            .map(|list| list.iter().any(|id| id == action_id))
            .unwrap_or(false)
    }

    /// binding 粒度禁用（只拦 awareness 派生；key 形如 `builtin:open_url::clipboard_is_url`）。
    fn binding_disabled(&self, binding: &str) -> bool {
        self.disabled_context_bindings
            .read()
            .map(|list| list.iter().any(|k| k == binding))
            .unwrap_or(false)
    }

    /// 打开类候选生产（0.24.8）。
    ///
    /// 产出（互斥按文本形态）：
    /// - URL → OpenUrl；文件路径 → OpenPath + RevealInExplorer
    /// - query 派生（origin=None）：输入框文本命中即产，只查动作粒度禁用
    /// - awareness 派生（origin=Clipboard）：剪贴板命中即产，另查 binding 粒度禁用
    fn produce_open_targets(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        let mut out = Vec::new();

        // query 派生：输入框里粘贴/输入的 URL 或路径（0.24.8 纯建议化后，
        // 输入 URL 在 result lane 无 keyword 可匹配，这里是唯一打开入口）。
        // 是输入意图不是环境，binding 粒度禁用不约束（恒 false）。
        let q = query.trim();
        if !q.is_empty() {
            out.extend(open_target_candidates(
                q,
                None,
                &|action_id: &str| self.action_disabled(action_id),
                &|_: &str| false,
            ));
        }

        // awareness 派生：剪贴板（0.8.0「打开链接」context 召回的建议化承接）
        if let Some(clip) = snapshot.find_text(AwarenessSource::Clipboard) {
            out.extend(open_target_candidates(
                clip.text,
                Some(SuggestionOrigin::Clipboard),
                &|action_id: &str| self.action_disabled(action_id),
                &|binding: &str| self.binding_disabled(binding),
            ));
        }

        out
    }
}

impl SuggestionProducer for ContextProducer {
    fn source(&self) -> SuggestionSource {
        SuggestionSource::Context
    }

    /// 产出 Translate + 打开类 Suggestion（query 派生 + awareness 派生）。
    ///
    /// 采纳后自抑制护栏（0.8.8 bugfix）内聚在 router 侧两个构造方法：
    /// query 已命中目标 plugin keyword（如 `翻译 xxx`）→ 不产同义建议，
    /// 避免 Ghost 反复弹出 / 无限 Tab 叠加（§3.3"明确 keyword 走确定路由不叠加"）。
    fn produce(&self, query: &str, snapshot: &AwarenessSnapshot) -> Vec<Suggestion> {
        let mut out = Vec::new();
        if let Some(sug) = self.router.translate_query_suggestion(query) {
            out.push(sug);
        }
        if let Some(sug) = self.router.context_suggestion(query, snapshot) {
            out.push(sug);
        }
        out.extend(self.produce_open_targets(query, snapshot));
        out
    }
}

/// 打开类候选构造表（0.24.8）。
///
/// 按 `text` 形态互斥产出：URL → OpenUrl；文件路径 → OpenPath + Reveal。
/// `origin=None` 为 query 派生（id 后缀 `-query`），`Some` 为 awareness 派生
/// （`-clipboard`）——id 语义 slug 供采纳遥测，rank 按 §3.4 表。
#[allow(deprecated)]
fn open_target_candidates(
    text: &str,
    origin: Option<SuggestionOrigin>,
    action_disabled: &dyn Fn(&str) -> bool,
    binding_disabled: &dyn Fn(&str) -> bool,
) -> Vec<Suggestion> {
    /// (kind, capability_id, 参数字段, binding 触发 key, rank)。
    /// URL 组只在 URL 形态下取；path 组两条在路径形态下都取（双槽齐备）。
    const URL_SET: (SuggestionKind, &str, &str, &str, f64) = (
        SuggestionKind::OpenUrl,
        "open_url",
        "url",
        "clipboard_is_url",
        RANK_OPEN_URL,
    );
    const PATH_SETS: [(SuggestionKind, &str, &str, &str, f64); 2] = [
        (
            SuggestionKind::OpenPath,
            "open_path",
            "path",
            "clipboard_is_file_path",
            RANK_OPEN_PATH,
        ),
        (
            SuggestionKind::RevealInExplorer,
            "reveal_in_explorer",
            "path",
            "clipboard_is_file_path",
            RANK_REVEAL,
        ),
    ];

    let sets: &[(SuggestionKind, &str, &str, &str, f64)] = if probe::is_url(text) {
        std::slice::from_ref(&URL_SET)
    } else if probe::is_file_path(text) {
        &PATH_SETS
    } else {
        return Vec::new();
    };

    let suffix = if origin.is_some() { "clipboard" } else { "query" };
    sets.iter()
        .filter(|(_, action_id, _, trigger_key, _)| {
            let binding =
                crate::domain::intent::binding_key(&format!("builtin:{action_id}"), trigger_key);
            !action_disabled(action_id) && !binding_disabled(&binding)
        })
        .map(|(kind, capability_id, field, _, rank)| {
            open_suggestion(*kind, capability_id, field, text, *rank, origin, suffix)
        })
        .collect()
}

/// 单个打开类候选构造（display = 目标文本，前端按 kind 组 i18n 文案并截断预览）。
#[allow(deprecated)]
fn open_suggestion(
    kind: SuggestionKind,
    capability_id: &str,
    param_field: &str,
    target: &str,
    rank: f64,
    origin: Option<SuggestionOrigin>,
    suffix: &str,
) -> Suggestion {
    let id = format!(
        "{}-{suffix}",
        match kind {
            SuggestionKind::OpenUrl => "open-url",
            SuggestionKind::OpenPath => "open-path",
            SuggestionKind::RevealInExplorer => "reveal",
            _ => "open",
        }
    );
    Suggestion {
        id,
        kind,
        action: SuggestionAction::InvokeCapability {
            capability_id: capability_id.to_string(),
            args: serde_json::json!({ param_field: target }),
        },
        rank_score: rank,
        display: target.to_string(),
        prefix_len: 0,
        origin,
        fingerprint: text_fingerprint(target),
        source: SuggestionSource::Context,
        ranking_hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::intent::RuleRouter;
    use crate::infra::platform::context::ContextSnapshot;

    fn producer(
        disabled_actions: Vec<String>,
        disabled_bindings: Vec<String>,
    ) -> ContextProducer {
        ContextProducer::new(
            Arc::new(RuleRouter::new(false)),
            Arc::new(RwLock::new(disabled_actions)),
            Arc::new(RwLock::new(disabled_bindings)),
        )
    }

    fn snap_with_clipboard(text: &str) -> ContextSnapshot {
        ContextSnapshot::with_clipboard(text)
    }

    /// 剪贴板 URL → 只产 OpenUrl（0.95，origin=Clipboard），无 path/reveal。
    #[test]
    fn clipboard_url_yields_open_url_only() {
        let p = producer(vec![], vec![]);
        let snap = snap_with_clipboard("https://example.com");
        let out = p.produce_open_targets("", &snap);
        assert_eq!(out.len(), 1, "URL 与文件路径互斥，只产 OpenUrl");
        let s = &out[0];
        assert_eq!(s.id, "open-url-clipboard");
        assert_eq!(s.kind, SuggestionKind::OpenUrl);
        assert_eq!(s.rank_score, 0.95);
        assert_eq!(s.origin, Some(SuggestionOrigin::Clipboard));
        assert_eq!(
            s.action,
            SuggestionAction::InvokeCapability {
                capability_id: "open_url".into(),
                args: serde_json::json!({"url": "https://example.com"}),
            }
        );
    }

    /// 剪贴板文件路径 → OpenPath（0.93）+ Reveal（0.88）双候选。
    #[test]
    fn clipboard_path_yields_open_path_and_reveal() {
        let p = producer(vec![], vec![]);
        let snap = snap_with_clipboard("C:\\Users\\test.txt");
        let out = p.produce_open_targets("", &snap);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].kind, SuggestionKind::OpenPath);
        assert_eq!(out[0].rank_score, 0.93);
        assert_eq!(out[0].id, "open-path-clipboard");
        assert_eq!(out[1].kind, SuggestionKind::RevealInExplorer);
        assert_eq!(out[1].rank_score, 0.88);
        assert_eq!(out[1].id, "reveal-clipboard");
        assert!(out.iter().all(|s| matches!(
            &s.action,
            SuggestionAction::InvokeCapability { args, .. }
                if args.get("path").and_then(|v| v.as_str()) == Some("C:\\Users\\test.txt")
        )));
    }

    /// query 是 URL → query 派生 OpenUrl（origin=None，id 后缀 -query）。
    #[test]
    fn query_url_yields_query_derived_open_url() {
        let p = producer(vec![], vec![]);
        let snap = ContextSnapshot::default();
        let out = p.produce_open_targets("https://example.com/path?q=1", &snap);
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, "open-url-query");
        assert_eq!(out[0].origin, None, "query 派生 origin=None（分层优先）");
    }

    /// query 与剪贴板同为 URL 时两候选同 Kind —— 由 Coordinator 去重（此处只验产出）。
    #[test]
    fn query_and_clipboard_url_both_produced() {
        let p = producer(vec![], vec![]);
        let snap = snap_with_clipboard("https://clip.example.com");
        let out = p.produce_open_targets("https://typed.example.com", &snap);
        assert_eq!(out.len(), 2, "query 派生 + awareness 派生各一条");
        assert_eq!(out[0].id, "open-url-query");
        assert_eq!(out[1].id, "open-url-clipboard");
    }

    /// 普通文本（非 URL 非路径）→ 打开类零候选。
    #[test]
    fn plain_text_yields_no_open_candidates() {
        let p = producer(vec![], vec![]);
        let snap = snap_with_clipboard("just some text 缺");
        assert!(p.produce_open_targets("", &snap).is_empty());
        assert!(p.produce_open_targets("chrome", &snap).is_empty());
    }

    /// 动作粒度禁用（disabled_builtin_actions）→ query/awareness 派生都拦。
    #[test]
    fn action_disabled_blocks_both_derivations() {
        let p = producer(vec!["open_url".to_string()], vec![]);
        let snap = snap_with_clipboard("https://example.com");
        assert!(p.produce_open_targets("", &snap).is_empty());
        assert!(
            p.produce_open_targets("https://example.com", &snap).is_empty(),
            "动作粒度禁用对 query 派生同样生效"
        );
    }

    /// binding 粒度禁用只拦 awareness 派生；query 派生不受环境 binding 约束。
    #[test]
    fn binding_disabled_blocks_awareness_only() {
        let p = producer(
            vec![],
            vec!["builtin:open_url::clipboard_is_url".to_string()],
        );
        let snap = snap_with_clipboard("https://example.com");
        assert!(
            p.produce_open_targets("", &snap).is_empty(),
            "binding 禁用 → 剪贴板派生被拦"
        );
        assert_eq!(
            p.produce_open_targets("https://example.com", &snap).len(),
            1,
            "query 派生是输入意图，不受 binding 约束"
        );
    }

    /// path 场景的 binding 禁用：只拦被禁的那条，另一条照常。
    #[test]
    fn path_binding_disabled_blocks_only_that_action() {
        let p = producer(
            vec![],
            vec!["builtin:reveal_in_explorer::clipboard_is_file_path".to_string()],
        );
        let snap = snap_with_clipboard("C:\\Users\\test.txt");
        let out = p.produce_open_targets("", &snap);
        assert_eq!(out.len(), 1, "reveal 被 binding 禁用，open_path 照常");
        assert_eq!(out[0].kind, SuggestionKind::OpenPath);
    }

    /// produce() 全链路：translate（router 无规则时不产）+ 打开类合并输出。
    #[test]
    fn produce_merges_translate_and_open_targets() {
        let p = producer(vec![], vec![]);
        let snap = snap_with_clipboard("https://example.com");
        let out = p.produce("", &snap);
        assert!(out.iter().any(|s| s.kind == SuggestionKind::OpenUrl));
    }
}
