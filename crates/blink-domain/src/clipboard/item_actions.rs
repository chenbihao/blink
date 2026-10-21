//! 0.26.10：剪贴板历史条目的右键「智能操作」只读查询。
//!
//! 输入是类型化条目引用 `{kind, id}`——**查库取完整原文/类型化路径**做打开类
//! 派生（列表摘要、预览选区、当前系统剪贴板、当前 query 都不能替代条目内容）。
//! 全文只在后端派生，不把长正文额外放入 DOM。
//!
//! 展示前按 `LocalSurface` 的 origin/runtime/policy 预检能力可调用性——
//! 与 `run_builtin_action` 执行时构造的 InvokeContext 同构；点击时 Registry
//! 门禁仍会重检（禁用动作不能借右键复活，两道检查各司其职）。
//! Producer 侧纯派生不查 DB/fs/网络；本服务负责读库与动作可用性过滤。
//!
//! 数据库故障返回 `Err`（结构化错误，调用方不能用当前剪贴板兜底）；
//! 条目不存在/无适用操作返回空组。

use blink_infra::data::DbPools;

use crate::capability::{CapabilityRegistry, InvocationOrigin, RuntimeCapabilities};
use crate::clipboard::history::{ClipboardEntryKind, ClipboardEntryRef};
use crate::intent::suggestion::{self, SuggestionAction, SuggestionKind};

/// 单个派生动作的有限描述（菜单展示与点击执行所需的全部信息）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ClipboardItemAction {
    /// 打开类 Kind（`openUrl` / `openPath` / `revealInExplorer`，wire camelCase）。
    pub kind: SuggestionKind,
    /// Capability id（执行走 `run_builtin_action`）。
    pub capability_id: String,
    /// 绑定参数（Capability schema 最终 JSON，如 `{"url": ...}`）。
    pub args: serde_json::Value,
    /// 文案所需目标摘要（有界；不含完整长正文）。
    pub target: String,
    /// 稳定动作 key（菜单内容代次协议：跨更新复用动作身份，不用数组下标）。
    pub action_key: String,
}

/// 目标摘要长度上限（字符）——菜单文案展示用，完整目标已在 args 内。
const TARGET_SUMMARY_CHARS: usize = 120;

/// 按条目引用派生打开类动作（只读查询）。
///
/// `disabled_builtin_actions` 与建议侧/result 侧共享同源列表（设置页保存热生效）。
/// 多文件条目、图片条目、普通文本不产打开类动作（返回空组，不取首个目标）。
pub async fn clipboard_item_actions(
    pools: &DbPools,
    registry: &CapabilityRegistry,
    entry: &ClipboardEntryRef,
    disabled_builtin_actions: &[String],
) -> Result<Vec<ClipboardItemAction>, String> {
    let content = match entry.kind {
        ClipboardEntryKind::Text => {
            match blink_infra::data::clipboard::get_text_by_id(&pools.history, &entry.id).await {
                Some(text) => text,
                None => return Ok(Vec::new()), // 条目不存在/已清理 → 空组
            }
        }
        ClipboardEntryKind::File => {
            match blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &entry.id)
                .await
            {
                // 恰好单路径才派生；使用 DB 原始路径（目录适用），不从展示标题反推
                Some(item) if item.paths.len() == 1 => item.paths.into_iter().next().unwrap(),
                _ => return Ok(Vec::new()),
            }
        }
        ClipboardEntryKind::Image => return Ok(Vec::new()),
    };

    let action_disabled = |action_id: &str| {
        disabled_builtin_actions
            .iter()
            .any(|disabled| disabled == action_id)
    };
    let candidates = suggestion::context::item_open_candidates(content.trim(), &action_disabled);

    // 展示前过滤：能力不存在/LocalSurface 不可调用/桌面运行时不满足 → 不展示。
    // 与执行时 Registry 门禁同构（surface=Some(env) 由调用方注入，见命令层）。
    let mut out = Vec::with_capacity(candidates.len());
    for candidate in candidates {
        let (capability_id, args) = match &candidate.action {
            SuggestionAction::InvokeCapability {
                capability_id,
                args,
            } => (capability_id.clone(), args.clone()),
            #[allow(unreachable_patterns)]
            _ => continue,
        };
        if !callable_from_local_surface(registry, &capability_id) {
            continue;
        }
        out.push(ClipboardItemAction {
            kind: candidate.kind,
            capability_id,
            args,
            target: truncate_chars(&candidate.display, TARGET_SUMMARY_CHARS),
            action_key: format!("{}:{}", candidate.id, entry.id),
        });
    }
    Ok(out)
}

/// LocalSurface 可调用性预检（与 `run_builtin_action` 的 InvokeContext 同构：
/// main_process + desktop_session + surface 可用）。策略缺失的 id 返回 false。
fn callable_from_local_surface(registry: &CapabilityRegistry, capability_id: &str) -> bool {
    let Some(capability) = registry.get(capability_id) else {
        return false;
    };
    let policy = capability.policy();
    if !policy
        .allowed_origins
        .contains(InvocationOrigin::LocalSurface)
    {
        return false;
    }
    // 与执行时相同的运行时要求判定；surface 预检在主进程桌面会话恒可用
    let runtime = RuntimeCapabilities {
        surface: None,
        main_process: true,
        desktop_session: true,
    };
    policy.runtime_satisfied(runtime.as_requirement())
}

/// 按字符数安全截断（UTF-8 边界安全，`string_slice` 由 clippy deny）。
fn truncate_chars(text: &str, max_chars: usize) -> String {
    if text.chars().count() <= max_chars {
        return text.to_string();
    }
    let truncated: String = text.chars().take(max_chars).collect();
    format!("{truncated}…")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_is_char_boundary_safe() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        let long = "中".repeat(200);
        let out = truncate_chars(&long, 120);
        assert!(out.chars().count() == 121 && out.ends_with('…'));
    }

    #[test]
    fn callable_check_rejects_missing_capability() {
        let registry = CapabilityRegistry::new();
        assert!(!callable_from_local_surface(
            &registry,
            "no_such_capability"
        ));
        // open_url 允许 LocalSurface（ALL），主进程桌面会话运行时满足
        assert!(callable_from_local_surface(&registry, "open_url"));
        assert!(callable_from_local_surface(&registry, "open_path"));
        assert!(callable_from_local_surface(&registry, "reveal_in_explorer"));
    }

    #[tokio::test]
    async fn text_entry_url_yields_open_url_and_disabled_filters() {
        let history = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard::init_db(&history)
            .await
            .unwrap();
        blink_infra::data::clipboard::save_item(
            &history,
            &blink_infra::data::clipboard::ClipboardItem {
                id: "t1".into(),
                text: "  https://example.com/page  ".into(),
                preview: String::new(),
                created_at: 1,
                source_app: None,
                hit_count: 0,
                pinned_until: 0,
            },
        )
        .await
        .unwrap();
        let pools = DbPools {
            config: history.clone(),
            ai: history.clone(),
            history: history.clone(),
            cache: history,
        };
        let registry = CapabilityRegistry::new();
        let entry = ClipboardEntryRef {
            kind: ClipboardEntryKind::Text,
            id: "t1".into(),
        };
        let actions = clipboard_item_actions(&pools, &registry, &entry, &[])
            .await
            .unwrap();
        assert_eq!(actions.len(), 1, "完整 trim 后是单个 URL → OpenUrl");
        assert_eq!(actions[0].capability_id, "open_url");
        assert_eq!(actions[0].args["url"], "https://example.com/page");
        assert!(actions[0].action_key.ends_with(":t1"));
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../../../frontend/js/main-window/fixtures/clipboard-item-actions.json"
        ))
        .unwrap();
        assert_eq!(
            serde_json::to_value(&actions).unwrap(),
            fixture,
            "菜单测试与真实 Rust 序列化共用 wire fixture"
        );

        // disabled_builtin_actions 拦截
        let blocked = clipboard_item_actions(&pools, &registry, &entry, &["open_url".to_string()])
            .await
            .unwrap();
        assert!(blocked.is_empty());

        // 条目不存在 → 空组
        let missing = ClipboardEntryRef {
            kind: ClipboardEntryKind::Text,
            id: "missing".into(),
        };
        assert!(
            clipboard_item_actions(&pools, &registry, &missing, &[])
                .await
                .unwrap()
                .is_empty()
        );
    }

    #[tokio::test]
    async fn single_file_entry_yields_open_and_reveal_multi_file_empty() {
        let history = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect("sqlite::memory:")
            .await
            .unwrap();
        blink_infra::data::clipboard_files::init_db(&history)
            .await
            .unwrap();
        blink_infra::data::clipboard_files::save_file(
            &history,
            &blink_infra::data::clipboard_files::ClipboardFileItem {
                id: "f1".into(),
                paths: vec![r"C:\Users\docs".into()],
                created_at: 1,
                source_app: None,
            },
        )
        .await
        .unwrap();
        blink_infra::data::clipboard_files::save_file(
            &history,
            &blink_infra::data::clipboard_files::ClipboardFileItem {
                id: "f2".into(),
                paths: vec![r"C:\a.txt".into(), r"C:\b.txt".into()],
                created_at: 2,
                source_app: None,
            },
        )
        .await
        .unwrap();
        let pools = DbPools {
            config: history.clone(),
            ai: history.clone(),
            history: history.clone(),
            cache: history,
        };
        let registry = CapabilityRegistry::new();
        let single = clipboard_item_actions(
            &pools,
            &registry,
            &ClipboardEntryRef {
                kind: ClipboardEntryKind::File,
                id: "f1".into(),
            },
            &[],
        )
        .await
        .unwrap();
        assert_eq!(single.len(), 2, "单路径（目录适用）→ OpenPath + Reveal");
        assert_eq!(single[0].capability_id, "open_path");
        assert_eq!(single[1].capability_id, "reveal_in_explorer");
        assert_eq!(single[0].args["path"], r"C:\Users\docs");

        let multi = clipboard_item_actions(
            &pools,
            &registry,
            &ClipboardEntryRef {
                kind: ClipboardEntryKind::File,
                id: "f2".into(),
            },
            &[],
        )
        .await
        .unwrap();
        assert!(multi.is_empty(), "多文件不取首个目标");

        let image = clipboard_item_actions(
            &pools,
            &registry,
            &ClipboardEntryRef {
                kind: ClipboardEntryKind::Image,
                id: "whatever".into(),
            },
            &[],
        )
        .await
        .unwrap();
        assert!(image.is_empty());
    }

    #[test]
    fn long_text_excluded_by_probe_limits_without_truncation_misjudgment() {
        // 超过 is_url 2048 上限的长文本（前缀是 URL 形态）不得误判
        let long_url_prefix = format!("https://example.com/{}", "a".repeat(3000));
        let out =
            crate::intent::suggestion::context::item_open_candidates(&long_url_prefix, &|_| false);
        assert!(out.is_empty(), "超长文本不截断误判");
        // 多行/带引号文本不新增特殊解析（沿用判定器）
        let multiline = "C:\\a.txt\nC:\\b.txt";
        assert!(
            crate::intent::suggestion::context::item_open_candidates(multiline, &|_| false)
                .is_empty(),
            "多行路径不按路径派生"
        );
        let quoted = "\"C:\\a.txt\"";
        assert!(
            crate::intent::suggestion::context::item_open_candidates(quoted, &|_| false).is_empty(),
            "带引号文本不按路径派生"
        );
    }
}
