//! Context 触发条件与参数来源枚举 —— 内置动作与插件共用。
//!
//! 0.8.0 §1.3 时这两个 enum 只在 `domain/search/builtin_engine.rs` 里私有定义；
//! 0.8.2 §3.2.1 上移到这里，让 `intent::RuleRouter` 也能声明「插件按 Context 触发」
//! 的规则，避免两处漂移（内置动作与插件的判定语义必须一致）。
//!
//! **纯枚举 + 判定辅助**，无平台调用；判定所需的文本函数（`is_url` / `is_file_path`
//! / `needs_translation`）仍在 `probe.rs`，本模块只做「触发条件 → snapshot 命中判定」
//! 的组织工作。

use crate::infra::platform::context::ContextSnapshot;

use super::probe;

/// Context 触发条件。
///
/// 变体只声明**已有消费者**的形态：
/// - `ClipboardIsUrl` / `ClipboardIsFilePath`：0.8.0 内置动作使用（打开链接/打开路径/资源管理器定位）
/// - `SelectionNonEmpty`：0.8.0 预留（内置动作未消费，等 0.8.3 Chord 复用）
/// - `TextIsNonTargetLang`：0.8.2 §3.4 翻译插件消费
///
/// 未加 `ClipboardIsCode` / `ForegroundIs` 等——按 [[configurable-by-default]] 精神，
/// 有真实消费者再加，避免「永远返回 false 的诱饵」。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContextTrigger {
    /// 剪贴板文本是 URL（http/https/file/ftp）
    ClipboardIsUrl,
    /// 剪贴板文本是 Windows 文件路径（绝对路径 / UNC）
    ClipboardIsFilePath,
    /// 选区文本非空
    #[allow(dead_code)] // 0.8.3 Chord/选区插件消费
    SelectionNonEmpty,
    /// 文本（按 `source` 抽取）值得翻译——即非目标语言且非 URL/路径/短文本。
    ///
    /// **target 由调用方在判定前解析好**（`RuleRouter` 通过 `PluginSettingResolver`
    /// 读插件 `target_lang`，`auto` 回退 `AppConfig.language`），本枚举**不含 target 字段**。
    /// 0.8.2 §3.4
    TextIsNonTargetLang { source: TextSource },
}

/// 文本抽取来源。0.8.2 只支持 `SelectionThenClipboard`；未来 Chord 抓特定 source 时
/// 再加 `SelectionOnly` / `ClipboardOnly` 等变体。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TextSource {
    /// 先看 selection；空则回退 clipboard。
    SelectionThenClipboard,
}

impl TextSource {
    /// 按当前策略从 snapshot 抽取文本。空/None 返回 None。
    pub fn extract<'a>(&self, snapshot: &'a ContextSnapshot) -> Option<&'a str> {
        match self {
            TextSource::SelectionThenClipboard => {
                let sel = snapshot
                    .selected_text
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                sel.or_else(|| {
                    snapshot
                        .clipboard_text
                        .as_deref()
                        .map(str::trim)
                        .filter(|s| !s.is_empty())
                })
            }
        }
    }
}

/// 参数来源。0.8.0 §1.3 内置动作与 0.8.2 §3.4 插件共用。
///
/// `None` = 无参数动作；`Clipboard` / `Selection` = 从 snapshot 抽字符串。
/// `QueryRest`（`echo hello` 里的 hello）无消费者延后，长句参数走 0.9 AI function calling。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ParamSource {
    None,
    Clipboard,
    #[allow(dead_code)] // 0.8.3 Chord 消费选区参数化动作
    Selection,
}

impl ParamSource {
    /// 按 source 从 snapshot 抽取字符串参数（trim 后空视为 None）。
    /// 内置动作的 OpenUrl/OpenPath/RevealInExplorer 用此。
    pub fn extract(&self, snapshot: &ContextSnapshot) -> Option<serde_json::Value> {
        let raw = match self {
            ParamSource::None => return None,
            ParamSource::Clipboard => snapshot.clipboard_text.as_deref(),
            ParamSource::Selection => snapshot.selected_text.as_deref(),
        };
        raw.map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| serde_json::Value::String(s.to_string()))
    }
}

/// 从 manifest 侧 `ManifestContextWhen` 映射到 domain 侧 `ContextTrigger`（0.8.2 §3.4 review #4）。
///
/// 集中放在这里避免映射决策散落到 `RuleRouter`：变体加减时改一处即可。
impl From<crate::domain::plugin::ManifestContextWhen> for ContextTrigger {
    fn from(w: crate::domain::plugin::ManifestContextWhen) -> Self {
        use crate::domain::plugin::ManifestContextWhen as M;
        match w {
            M::ClipboardIsUrl => ContextTrigger::ClipboardIsUrl,
            M::ClipboardIsFilePath => ContextTrigger::ClipboardIsFilePath,
            M::SelectionNonEmpty => ContextTrigger::SelectionNonEmpty,
            M::TextIsNonTargetLang => ContextTrigger::TextIsNonTargetLang {
                source: TextSource::SelectionThenClipboard,
            },
        }
    }
}

/// 判定单个 Context 触发条件是否命中 snapshot（review #7：`is_hit` 替代 `matches`，
/// 避免与 `matches!` 宏和 `str::matches` 视觉混淆）。
///
/// `TextIsNonTargetLang` 需要 target 语言参数，由调用方（`RuleRouter` /
/// `BuiltinEngine`）在调用前解析好；本函数只做「已解析 target → 命中吗」。
/// target 为 `None` 时不命中；`"auto"` 由 `probe::needs_translation` 内部兜底 false。
pub fn is_hit(
    trigger: &ContextTrigger,
    snapshot: &ContextSnapshot,
    target: Option<&str>,
) -> bool {
    match trigger {
        ContextTrigger::ClipboardIsUrl => snapshot
            .clipboard_text
            .as_deref()
            .map(probe::is_url)
            .unwrap_or(false),
        ContextTrigger::ClipboardIsFilePath => snapshot
            .clipboard_text
            .as_deref()
            .map(probe::is_file_path)
            .unwrap_or(false),
        ContextTrigger::SelectionNonEmpty => snapshot
            .selected_text
            .as_deref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false),
        ContextTrigger::TextIsNonTargetLang { source } => {
            let Some(text) = source.extract(snapshot) else { return false };
            let Some(target) = target else { return false };
            probe::needs_translation(text, target)
        }
    }
}

/// 任一 trigger 命中即通过（OR 语义）。空 slice 恒 false（= 不参与 Context 路由）。
///
/// **参数**：`target` 仅供 `TextIsNonTargetLang` 使用；其他 trigger 忽略。
pub fn any_hit(
    triggers: &[ContextTrigger],
    snapshot: &ContextSnapshot,
    target: Option<&str>,
) -> bool {
    if triggers.is_empty() {
        return false;
    }
    triggers.iter().any(|t| is_hit(t, snapshot, target))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn snap_with_clipboard(text: &str) -> ContextSnapshot {
        ContextSnapshot {
            clipboard_text: Some(text.to_string()),
            ..ContextSnapshot::default()
        }
    }

    fn snap_with_selection(text: &str) -> ContextSnapshot {
        ContextSnapshot {
            selected_text: Some(text.to_string()),
            ..ContextSnapshot::default()
        }
    }

    #[test]
    fn any_hit_empty_slice_is_false() {
        let s = ContextSnapshot::default();
        assert!(!any_hit(&[], &s, None));
    }

    #[test]
    fn clipboard_is_url_hit() {
        let s = snap_with_clipboard("https://example.com");
        assert!(is_hit(&ContextTrigger::ClipboardIsUrl, &s, None));
        assert!(!is_hit(&ContextTrigger::ClipboardIsFilePath, &s, None));
    }

    #[test]
    fn clipboard_is_file_path_hit() {
        let s = snap_with_clipboard(r"C:\Users\a\file.txt");
        assert!(is_hit(&ContextTrigger::ClipboardIsFilePath, &s, None));
        assert!(!is_hit(&ContextTrigger::ClipboardIsUrl, &s, None));
    }

    #[test]
    fn selection_non_empty_hit_after_trim() {
        let s = snap_with_selection("   hello   ");
        assert!(is_hit(&ContextTrigger::SelectionNonEmpty, &s, None));
        let s2 = snap_with_selection("    ");
        assert!(!is_hit(&ContextTrigger::SelectionNonEmpty, &s2, None));
    }

    #[test]
    fn any_hit_or_semantics() {
        let s = snap_with_clipboard("https://example.com");
        assert!(any_hit(
            &[
                ContextTrigger::SelectionNonEmpty,
                ContextTrigger::ClipboardIsFilePath,
                ContextTrigger::ClipboardIsUrl,
            ],
            &s,
            None,
        ));
    }

    #[test]
    fn text_source_extract_prefers_selection() {
        let s = ContextSnapshot {
            selected_text: Some("SEL".to_string()),
            clipboard_text: Some("CLIP".to_string()),
            ..ContextSnapshot::default()
        };
        assert_eq!(TextSource::SelectionThenClipboard.extract(&s), Some("SEL"));
    }

    #[test]
    fn text_source_fallback_to_clipboard() {
        let s = ContextSnapshot {
            selected_text: Some("   ".to_string()),
            clipboard_text: Some("CLIP".to_string()),
            ..ContextSnapshot::default()
        };
        assert_eq!(TextSource::SelectionThenClipboard.extract(&s), Some("CLIP"));
    }

    #[test]
    fn text_source_none_when_both_empty() {
        let s = ContextSnapshot::default();
        assert_eq!(TextSource::SelectionThenClipboard.extract(&s), None);
    }

    #[test]
    fn text_is_non_target_lang_hit() {
        let s = snap_with_selection("this is a longer english sentence");
        let trigger = ContextTrigger::TextIsNonTargetLang {
            source: TextSource::SelectionThenClipboard,
        };
        assert!(!is_hit(&trigger, &s, None));
        assert!(!is_hit(&trigger, &s, Some("auto")));
        assert!(is_hit(&trigger, &s, Some("zh")));
        assert!(!is_hit(&trigger, &s, Some("en")));
    }

    #[test]
    fn text_is_non_target_lang_from_clipboard_fallback() {
        let s = ContextSnapshot {
            selected_text: None,
            clipboard_text: Some("hello world foo bar".to_string()),
            ..ContextSnapshot::default()
        };
        let trigger = ContextTrigger::TextIsNonTargetLang {
            source: TextSource::SelectionThenClipboard,
        };
        assert!(is_hit(&trigger, &s, Some("zh")));
    }

    #[test]
    fn text_is_non_target_lang_url_guard_via_probe() {
        let s = snap_with_clipboard("https://github.com/anthropics/foo");
        let trigger = ContextTrigger::TextIsNonTargetLang {
            source: TextSource::SelectionThenClipboard,
        };
        assert!(!is_hit(&trigger, &s, Some("zh")));
    }

    #[test]
    fn param_source_extract_none() {
        let s = snap_with_clipboard("hello");
        assert_eq!(ParamSource::None.extract(&s), None);
    }

    #[test]
    fn param_source_extract_clipboard() {
        let s = snap_with_clipboard("  hello  ");
        assert_eq!(
            ParamSource::Clipboard.extract(&s),
            Some(serde_json::Value::String("hello".to_string())),
        );
    }

    #[test]
    fn param_source_extract_selection() {
        let s = snap_with_selection("  world  ");
        assert_eq!(
            ParamSource::Selection.extract(&s),
            Some(serde_json::Value::String("world".to_string())),
        );
    }

    #[test]
    fn from_manifest_when_maps_all_variants() {
        use crate::domain::plugin::ManifestContextWhen as M;
        assert_eq!(
            ContextTrigger::from(M::ClipboardIsUrl),
            ContextTrigger::ClipboardIsUrl,
        );
        assert_eq!(
            ContextTrigger::from(M::ClipboardIsFilePath),
            ContextTrigger::ClipboardIsFilePath,
        );
        assert_eq!(
            ContextTrigger::from(M::SelectionNonEmpty),
            ContextTrigger::SelectionNonEmpty,
        );
        assert_eq!(
            ContextTrigger::from(M::TextIsNonTargetLang),
            ContextTrigger::TextIsNonTargetLang {
                source: TextSource::SelectionThenClipboard,
            },
        );
    }
}
