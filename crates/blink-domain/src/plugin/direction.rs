//! 翻译方向策略（0.28.3 §3.4）——建议与执行共用的目标语言解析规则。
//!
//! **优先级（phase 0.28 已定）**：人工单次目标 > 当前插件窗口会话目标 > 已保存默认。
//! 单次/会话目标都不回写 `plugin:{id}` 配置——默认设置不因一次反向操作被修改。
//! 单次覆盖的表达通路：
//! - 插件窗口页面：tool 参数 `target_lang` / `engine`（每次调用显式传入，天然单次）；
//! - 主窗快查：自由参数尾部方向 token（如 `翻译 hello -> en`），由翻译插件进程
//!   在解析 arg 时剥离并作为本次目标——宿主不经手改写 arg（语法见插件 crate
//!   `blink-plugin-translate` 的 direction 解析与同源测试）。
//!
//! **`auto` 语义**（中英互译模式）：执行侧插件按检测语言互换（zh→en / 非 zh→zh）；
//! 展示侧（Ghost 建议/AI producer）折叠为应用语言——建议只在"文本语言 ≠ 目标"
//! 时触发，此时互换结果与应用语言恒一致（见下方测试 `auto_folds_to_app_language`）。
//!
//! 本模块是宿主侧（intent 建议链路）的统一出口；插件进程持同语义的独立副本
//! （协议 struct 同模式，插件 crate 不依赖 domain）。

/// 支持的目标语言代码（与翻译插件 manifest `target_lang` enum 对齐）。
pub const SUPPORTED_TARGETS: [&str; 5] = ["auto", "zh", "en", "ja", "ko"];

/// 展示层目标解析：单次 > 已保存设置；`auto`/缺省折叠为应用语言。
///
/// - `one_shot`：人工本次目标（非空且受支持才生效，未知值忽略——不伪造）；
/// - `saved_setting`：插件设置 `target_lang` 原值（`"auto"` 或缺失 → 应用语言）；
/// - `app_language`：应用 UI 语言（折叠基准）。
pub fn resolve_display_target(
    one_shot: Option<&str>,
    saved_setting: Option<&str>,
    app_language: &str,
) -> String {
    if let Some(target) = one_shot.filter(|t| SUPPORTED_TARGETS.contains(t) && *t != "auto") {
        return target.to_string();
    }
    match saved_setting {
        Some(setting) if SUPPORTED_TARGETS.contains(&setting) && setting != "auto" => {
            setting.to_string()
        }
        _ => app_language.to_string(),
    }
}

/// 目标语言展示名（Ghost/页面方向选择器共用；未知代码原样返回，不猜测）。
pub fn lang_display_name(lang: &str) -> &str {
    match lang {
        "zh" => "中文",
        "en" => "英文",
        "ja" => "日文",
        "ko" => "韩文",
        "auto" => "自动",
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priority_one_shot_over_saved_and_default() {
        // 单次 > 已保存 > 应用语言
        assert_eq!(
            resolve_display_target(Some("ja"), Some("zh"), "en"),
            "ja"
        );
        assert_eq!(resolve_display_target(None, Some("ko"), "en"), "ko");
        assert_eq!(resolve_display_target(None, None, "en"), "en");
    }

    #[test]
    fn auto_folds_to_app_language() {
        assert_eq!(resolve_display_target(None, Some("auto"), "zh"), "zh");
        assert_eq!(resolve_display_target(None, None, "en"), "en");
        // 单次 "auto" 无意义（单次就是要确定方向）——按折叠处理
        assert_eq!(resolve_display_target(Some("auto"), Some("ja"), "zh"), "ja");
    }

    #[test]
    fn unknown_values_are_ignored_not_forged() {
        // 未知单次值（拼写错误/不支持语种）不生效、不伪造
        assert_eq!(resolve_display_target(Some("fr"), Some("zh"), "en"), "zh");
        // 未知保存值 → 应用语言兜底
        assert_eq!(resolve_display_target(None, Some("xx"), "zh"), "zh");
    }

    #[test]
    fn display_names_cover_supported_targets() {
        for target in SUPPORTED_TARGETS {
            assert!(!lang_display_name(target).is_empty());
        }
        assert_eq!(lang_display_name("zh"), "中文");
        assert_eq!(lang_display_name("fr"), "fr");
    }
}
