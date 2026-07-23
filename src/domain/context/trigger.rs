//! Context 触发条件与参数来源枚举 —— 内置动作与插件共用。
//!
//! 0.8.0 §1.3 时这两个 enum 只在 `domain/search/builtin_engine.rs` 里私有定义；
//! 0.8.2 §3.2.1 上移到这里，让 `intent::RuleRouter` 也能声明「插件按 Context 触发」
//! 的规则，避免两处漂移（内置动作与插件的判定语义必须一致）。
//!
//! **纯枚举 + 判定辅助**，无平台调用；判定所需的文本函数（`is_url` / `is_file_path`
//! / `needs_translation`）仍在 `probe.rs`，本模块只做「触发条件 → snapshot 命中判定」
//! 的组织工作。

use crate::infra::platform::context::{AwarenessSnapshot, AwarenessSource, AwarenessView};

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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
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
    /// 按当前策略从 snapshot 抽取带 source 标签的文本视图（0.8.3 收尾 · awareness）。
    ///
    /// **关键契约**：返回 `AwarenessView` 而不是 `&str` —— 调用方拿到 `(source, text)`
    /// 一起,避免 intent 层事后推断 origin。
    ///
    /// 空/None（trim 后无内容）返回 None。
    ///
    /// **`SelectionThenClipboard` 策略**：Selection 恒压 Clipboard。
    /// 划词是显式有意识的用户行为（选中文本），优先级高于剪贴板变化
    /// （可能是被动的：其他 app 复制、系统操作等）。两者只有一条时按原语义。
    pub fn extract<'a>(&self, snapshot: &'a AwarenessSnapshot) -> Option<AwarenessView<'a>> {
        match self {
            TextSource::SelectionThenClipboard => {
                let sel = snapshot.find_text(AwarenessSource::Selection);
                let clip = snapshot.find_text(AwarenessSource::Clipboard);
                match (sel, clip) {
                    (Some(s), _) => Some(s),    // Selection 恒胜
                    (None, Some(c)) => Some(c), // 无 Selection 时回退 Clipboard
                    (None, None) => None,
                }
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
    ///
    /// 0.8.3 收尾：改走 `AwarenessSnapshot::find_text` 统一 trim 判定。
    pub fn extract(&self, snapshot: &AwarenessSnapshot) -> Option<serde_json::Value> {
        let source = match self {
            ParamSource::None => return None,
            ParamSource::Clipboard => AwarenessSource::Clipboard,
            ParamSource::Selection => AwarenessSource::Selection,
        };
        snapshot
            .find_text(source)
            .map(|v| serde_json::Value::String(v.text.to_string()))
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
///
/// 0.8.3 收尾：字段访问全部改走 `AwarenessSnapshot::find_text`,与 origin 传导同源。
pub fn is_hit(
    trigger: &ContextTrigger,
    snapshot: &AwarenessSnapshot,
    target: Option<&str>,
) -> bool {
    match trigger {
        ContextTrigger::ClipboardIsUrl => snapshot
            .find_text(AwarenessSource::Clipboard)
            .map(|v| probe::is_url(v.text))
            .unwrap_or(false),
        ContextTrigger::ClipboardIsFilePath => snapshot
            .find_text(AwarenessSource::Clipboard)
            .map(|v| probe::is_file_path(v.text))
            .unwrap_or(false),
        ContextTrigger::SelectionNonEmpty => {
            snapshot.find_text(AwarenessSource::Selection).is_some()
        }
        ContextTrigger::TextIsNonTargetLang { source } => {
            let Some(view) = source.extract(snapshot) else {
                return false;
            };
            let Some(target) = target else { return false };
            probe::needs_translation(view.text, target)
        }
    }
}

/// 任一 trigger 命中即通过（OR 语义）。空 slice 恒 false（= 不参与 Context 路由）。
///
/// **参数**：`target` 仅供 `TextIsNonTargetLang` 使用；其他 trigger 忽略。
///
/// 0.11.8：`BuiltinEngine` 改为逐 trigger 判定（需查 binding 黑名单），不再调用本函数；
/// 当前唯一消费者是本模块的测试。保留作公共 API——未来出现「多 trigger + 无需逐个查黑名单」
/// 的场景（如纯 OR 语义的命中预检）可直接复用。
#[allow(dead_code)]
pub fn any_hit(
    triggers: &[ContextTrigger],
    snapshot: &AwarenessSnapshot,
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

    // 0.8.3 收尾：snap 构造走 AwarenessSnapshot::with_selection / with_clipboard helper。
    // 老的 `snap_with_clipboard` / `snap_with_selection` 函数删除,同名 helper 语义等价但
    // 内部走 upsert_text —— 保证测试也在验共享判定入口。

    fn snap_with_both(sel: &str, clip: &str) -> AwarenessSnapshot {
        let mut s = AwarenessSnapshot::default();
        s.upsert_text(AwarenessSource::Selection, Some(sel.to_string()));
        s.upsert_text(AwarenessSource::Clipboard, Some(clip.to_string()));
        s
    }

    #[test]
    fn any_hit_empty_slice_is_false() {
        let s = AwarenessSnapshot::default();
        assert!(!any_hit(&[], &s, None));
    }

    #[test]
    fn clipboard_is_url_hit() {
        let s = AwarenessSnapshot::with_clipboard("https://example.com");
        assert!(is_hit(&ContextTrigger::ClipboardIsUrl, &s, None));
        assert!(!is_hit(&ContextTrigger::ClipboardIsFilePath, &s, None));
    }

    #[test]
    fn clipboard_is_file_path_hit() {
        let s = AwarenessSnapshot::with_clipboard(r"C:\Users\a\file.txt");
        assert!(is_hit(&ContextTrigger::ClipboardIsFilePath, &s, None));
        assert!(!is_hit(&ContextTrigger::ClipboardIsUrl, &s, None));
    }

    #[test]
    fn selection_non_empty_hit_after_trim() {
        let s = AwarenessSnapshot::with_selection("   hello   ");
        assert!(is_hit(&ContextTrigger::SelectionNonEmpty, &s, None));
        let s2 = AwarenessSnapshot::with_selection("    ");
        assert!(!is_hit(&ContextTrigger::SelectionNonEmpty, &s2, None));
    }

    #[test]
    fn any_hit_or_semantics() {
        let s = AwarenessSnapshot::with_clipboard("https://example.com");
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
    fn text_source_selection_always_wins_when_both_present() {
        // Selection 恒压 Clipboard——划词是显式有意识的用户行为，优先级最高。
        // 无论 Clipboard 何时更新，只要有 Selection 就选 Selection。
        let s = snap_with_both("SEL", "CLIP");
        let view = TextSource::SelectionThenClipboard.extract(&s).unwrap();
        assert_eq!(view.text, "SEL");
        assert_eq!(view.source, AwarenessSource::Selection);
    }

    #[test]
    fn text_source_selection_wins_even_when_clipboard_newer() {
        // Clipboard 先 upsert、Selection 后 upsert → 仍取 Selection（不看时间戳）。
        let mut s = AwarenessSnapshot::default();
        s.upsert_text(AwarenessSource::Clipboard, Some("CLIP".into()));
        s.upsert_text(AwarenessSource::Selection, Some("SEL".into()));
        let view = TextSource::SelectionThenClipboard.extract(&s).unwrap();
        assert_eq!(view.text, "SEL");
        assert_eq!(view.source, AwarenessSource::Selection);
    }

    #[test]
    fn text_source_fallback_to_clipboard() {
        let s = snap_with_both("   ", "CLIP");
        let view = TextSource::SelectionThenClipboard.extract(&s).unwrap();
        assert_eq!(view.text, "CLIP");
        // 关键回归：origin 从数据侧带来,不用推断
        assert_eq!(view.source, AwarenessSource::Clipboard);
    }

    #[test]
    fn text_source_none_when_both_empty() {
        let s = AwarenessSnapshot::default();
        assert!(TextSource::SelectionThenClipboard.extract(&s).is_none());
    }

    #[test]
    fn text_is_non_target_lang_hit() {
        let s = AwarenessSnapshot::with_selection("this is a longer english sentence");
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
        let s = AwarenessSnapshot::with_clipboard("hello world foo bar");
        let trigger = ContextTrigger::TextIsNonTargetLang {
            source: TextSource::SelectionThenClipboard,
        };
        assert!(is_hit(&trigger, &s, Some("zh")));
    }

    #[test]
    fn text_is_non_target_lang_url_guard_via_probe() {
        let s = AwarenessSnapshot::with_clipboard("https://github.com/anthropics/foo");
        let trigger = ContextTrigger::TextIsNonTargetLang {
            source: TextSource::SelectionThenClipboard,
        };
        assert!(!is_hit(&trigger, &s, Some("zh")));
    }

    #[test]
    fn param_source_extract_none() {
        let s = AwarenessSnapshot::with_clipboard("hello");
        assert_eq!(ParamSource::None.extract(&s), None);
    }

    #[test]
    fn param_source_extract_clipboard() {
        let s = AwarenessSnapshot::with_clipboard("  hello  ");
        assert_eq!(
            ParamSource::Clipboard.extract(&s),
            Some(serde_json::Value::String("hello".to_string())),
        );
    }

    #[test]
    fn param_source_extract_selection() {
        let s = AwarenessSnapshot::with_selection("  world  ");
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
