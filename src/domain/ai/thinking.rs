//! 供应商 thinking 开关/强度补丁（0.21.16 + 0.21.17）——chat 对话窗口
//! （`agent_provider`）与主窗口（`rig_provider`）共用。
//!
//! 不同供应商的"开启/关闭思考"字段结构完全不同，这里把「供应商 → 开/关各发什么」
//! 收敛成单一纯函数 `thinking_request_patch`，两个调用方共享同一份逻辑。

use crate::domain::config::ai_config::{ProviderKind, ThinkingStyle};

/// 思考控件形态（0.21.22）——`resolve_thinking_mode` 返回的三态。
///
/// - `DeepSeekSwitch`：控件开关，请求发 `thinking.type`（DeepSeek 底座格式）
/// - `EffortLevels`：控件下拉，请求发 `reasoning_effort`
/// - `PlainSwitch`：控件开关，请求发各 provider 自有格式（Anthropic/Ollama/Gemini）
///
/// `thinking_supports_effort` 判断 `EffortLevels` vs 其他；
/// `thinking_request_patch` 据此分支决定发送格式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ThinkingMode {
    /// DeepSeek 底座开关——发 `thinking.type`
    DeepSeekSwitch,
    /// OpenAI reasoning_effort 等级下拉——发 `reasoning_effort`
    EffortLevels,
    /// 非 OAICompatible 的简单开关——发各自格式
    PlainSwitch,
}

/// 判定思考控件形态的单一真源（0.21.22）。
///
/// 语义：
/// - `style = Auto`（None 或 `Some(Auto)`）：完全现状——启发式判定
/// - `style = Effort`：强制 `EffortLevels`——控件下拉，不走 DeepSeek 分支
/// - `style = Toggle`：强制 `DeepSeekSwitch`（OAICompatible）或 `PlainSwitch`（其余）——控件开关
///
/// 非 `OpenAICompatible` 时 style 不生效，维持各自格式。
pub(crate) fn resolve_thinking_mode(
    kind: ProviderKind,
    base_url: Option<&str>,
    model_id: &str,
    style: Option<ThinkingStyle>,
) -> ThinkingMode {
    match kind {
        ProviderKind::OpenAICompatible => match style {
            Some(ThinkingStyle::Effort) => ThinkingMode::EffortLevels,
            Some(ThinkingStyle::Toggle) => ThinkingMode::DeepSeekSwitch,
            None | Some(ThinkingStyle::Auto) => {
                if is_deepseek_base(base_url, model_id) {
                    ThinkingMode::DeepSeekSwitch
                } else {
                    ThinkingMode::EffortLevels
                }
            }
        },
        // 非 OAICompatible：style 不生效，维持各自格式
        ProviderKind::AnthropicMessages
        | ProviderKind::OllamaHttp
        | ProviderKind::GeminiGenerateContent => ThinkingMode::PlainSwitch,
    }
}

/// 供应商特定的 thinking 请求补丁（纯函数，请求时按 provider + 开关状态敲定）。
///
/// 不同供应商的"开启/关闭思考"字段结构完全不同，0.21.16 前统一发
/// `{"thinking":{"type":"enabled"}}` 只对 DeepSeek 底座生效：
/// - DeepSeek（OpenAI 兼容 + deepseek 底座）：`{"thinking":{"type":"enabled"/"disabled"}}`
/// - OpenAI 官方 / 其他 OpenAI 兼容（Chat Completions）：
///   `{"reasoning_effort": ...}`——`none` 真关闭思考，仅新一代 gpt-5.x 系
///   模型支持，老 o1/o3 等只收 low/medium/high，发 none 会 400。
///   `reasoning_effort` 参数（0.21.17）提供显式等级/关闭/省略：
///   - `Some("none")` → 显式关闭（`{"reasoning_effort":"none"}`）
///   - `Some("")` / `None` → 不发送该字段（omit，用模型默认档，绝不出 400）。
///     0.21.18 起 `None`（未配置）与 `""`（显式默认）语义统一——「默认档 =
///     不主动给模型打 patch」，避免 UI 显示「默认」而实际仍发 high/none 的矛盾
///   - `Some(level)` → 该档位原样发送（`minimal/low/medium/high/xhigh/max` 或自定义）
/// - Anthropic：`{"thinking":{"type":"enabled","budget_tokens":N}}` / `{"type":"disabled"}`，
///   开启时 `budget_tokens` 必填且须小于 `max_tokens`（默认 `ANTHROPIC_THINKING_BUDGET`）
/// - Ollama：`{"think":true}` / `{"think":false}`（本地模型，仅支持思考的模型生效）
/// - Gemini：尚未接入，返回 `None`（字段是 `generationConfig.thinkingConfig` 且
///   2.5/3 代字段互斥，需额外决策）
///
/// 返回 `None` 表示该供应商不追加任何 thinking 参数。
pub(crate) fn thinking_request_patch(
    kind: ProviderKind,
    base_url: Option<&str>,
    model_id: &str,
    thinking_enabled: bool,
    reasoning_effort: Option<&str>,
    style: Option<ThinkingStyle>,
) -> Option<serde_json::Value> {
    let mode = resolve_thinking_mode(kind, base_url, model_id, style);
    match mode {
        // DeepSeek 底座走 thinking.type（style=Toggle 强制 OAICompatible 也走这里）
        ThinkingMode::DeepSeekSwitch => Some(serde_json::json!({
            "thinking": { "type": if thinking_enabled { "enabled" } else { "disabled" } }
        })),
        // OpenAI 官方 / 其他 OpenAI 兼容：Chat Completions 用 reasoning_effort 控制推理
        ThinkingMode::EffortLevels => match reasoning_effort {
            // 默认档（未配置 None 或显式 ""）：omit——不发送该字段，用模型默认档
            None | Some("") => None,
            // 显式等级：none（关闭）或任何档位/自定义值原样发送
            Some(level) => Some(serde_json::json!({ "reasoning_effort": level })),
        },
        // 非 OAICompatible 的各自格式
        ThinkingMode::PlainSwitch => match kind {
            // Anthropic：开启必带 budget_tokens（须小于 max_tokens），关闭只发 disabled
            ProviderKind::AnthropicMessages if thinking_enabled => Some(serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": ANTHROPIC_THINKING_BUDGET }
            })),
            ProviderKind::AnthropicMessages => Some(serde_json::json!({
                "thinking": { "type": "disabled" }
            })),
            // Ollama：本地模型，think 开关
            ProviderKind::OllamaHttp => Some(serde_json::json!({ "think": thinking_enabled })),
            // Gemini：尚未接入
            ProviderKind::GeminiGenerateContent => None,
            // OpenAICompatible 不应到达此分支（被 DeepSeekSwitch/EffortLevels 覆盖）
            ProviderKind::OpenAICompatible => {
                unreachable!("OpenAICompatible resolved to PlainSwitch — resolve_thinking_mode bug")
            }
        },
    }
}

/// 该 provider 是否支持 `reasoning_effort` 等级（0.21.17 + 0.21.22）——前端据此决定
/// 思考控件是"强度下拉"还是"简单开关"。
///
/// 0.21.22 起走 `resolve_thinking_mode` 单一真源：`EffortLevels` → true，其余 → false。
pub(crate) fn thinking_supports_effort(
    kind: ProviderKind,
    base_url: Option<&str>,
    model_id: &str,
    style: Option<ThinkingStyle>,
) -> bool {
    resolve_thinking_mode(kind, base_url, model_id, style) == ThinkingMode::EffortLevels
}

/// Anthropic 开启思考的默认 token 预算——须在 1024..max_tokens 之间。
/// Anthropic 未配 max_tokens 时由上层兜底 4096，2048 居中安全；用户可按模型调大。
const ANTHROPIC_THINKING_BUDGET: u64 = 2048;

/// 是否 DeepSeek 底座——base_url 含 `deepseek.com`，或模型名以 `deepseek` 开头。
/// OpenAICompatible 同时覆盖 OpenAI 官方与 DeepSeek，而两者 thinking 开关格式不同，需区分。
fn is_deepseek_base(base_url: Option<&str>, model_id: &str) -> bool {
    base_url.is_some_and(|u| u.contains("deepseek.com"))
        || model_id.to_ascii_lowercase().starts_with("deepseek")
}

// ── T2: reasoning_effort 400 自愈（0.21.20）──────────────────────────────────

/// 从 API 400 错误消息中解析 `reasoning_effort` 的合法档位列表。
///
/// OpenAI 风格 400 错误体形如：
/// `Unsupported value: 'xhigh' is not a valid value for 'reasoning_effort'.
///  Supported values are: 'minimal', 'low', 'medium', 'high'.`
///
/// 大小写不敏感匹配 `reasoning_effort` 上下文 + `Supported values are:` 后的引号列表。
/// 解析不到 → None（非 OpenAI 风格错误，如 DeepSeek/Anthropic/Ollama 的 400 不匹配）。
pub(crate) fn parse_supported_reasoning_efforts(message: &str) -> Option<Vec<String>> {
    let lower = message.to_ascii_lowercase();
    // 必须同时出现 reasoning_effort 和 supported values
    if !lower.contains("reasoning_effort") || !lower.contains("supported values are") {
        return None;
    }
    // 截取 "Supported values are:" 之后的部分
    let after = lower.split("supported values are").nth(1)?;
    // 提取所有引号内的值——兼容 ' 和 " 引号
    let mut values = Vec::new();
    let mut in_quote = false;
    let mut current = String::new();
    for ch in after.chars() {
        if ch == '\'' || ch == '"' {
            if in_quote {
                if !current.is_empty() {
                    values.push(current.clone());
                    current.clear();
                }
                in_quote = false;
            } else {
                in_quote = true;
            }
        } else if in_quote {
            current.push(ch);
        }
    }
    if values.is_empty() {
        None
    } else {
        // 去重，保持顺序
        let mut seen = std::collections::HashSet::new();
        let deduped: Vec<String> = values
            .into_iter()
            .filter(|v| seen.insert(v.clone()))
            .collect();
        Some(deduped)
    }
}

/// 已知 reasoning_effort 档位排序——值越大思考越强。
///
/// `minimal` < `low` < `medium` < `high`。
/// `xhigh` / `max` 等视作高于 `high`（排序值为 4）。
/// 未知自定义值也视作高于 `high`（保守降级）。
fn effort_rank(effort: &str) -> u8 {
    match effort.to_ascii_lowercase().as_str() {
        "minimal" => 0,
        "low" => 1,
        "medium" => 2,
        "high" => 3,
        // xhigh / max / 其他自定义值 → 高于 high
        _ => 4,
    }
}

/// 从支持的档位列表中选择 fallback 档位。
///
/// 策略：选「不高于 attempted 的最高支持档」。
/// - 若 attempted 是已知档位（minimal/low/medium/high），选 ≤ 其排名的最高支持档；
/// - 若 attempted 是未知自定义值（rank=4），取支持列表中排名最高的档；
/// - 若支持列表为空 → None。
/// - 若找不到不高于 attempted 的档（attempted 太低）→ 取支持列表最低档。
pub(crate) fn pick_fallback_effort(attempted: &str, supported: &[String]) -> Option<String> {
    if supported.is_empty() {
        return None;
    }
    let attempted_rank = effort_rank(attempted);
    // 尝试选不高于 attempted 的最高支持档
    let mut best: Option<(u8, &str)> = None;
    for s in supported {
        let rank = effort_rank(s);
        if rank <= attempted_rank {
            match best {
                None => best = Some((rank, s)),
                Some((br, _)) if rank > br => best = Some((rank, s)),
                _ => {}
            }
        }
    }
    match best {
        Some((_, s)) => Some(s.to_string()),
        // 找不到不高于 attempted 的 → 取支持列表最低档
        None => supported
            .iter()
            .min_by_key(|s| effort_rank(s))
            .map(|s| s.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// DeepSeek 底座（官方 base_url）：开 → thinking.type=enabled，关 → disabled。
    /// 显式传 reasoning_effort 也不改变 DeepSeek 底座格式（等级对 DeepSeek 无效）。
    #[test]
    fn deepseek_base_toggles_thinking_type() {
        let on = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-flash",
            true,
            None,
            None,
        );
        assert_eq!(
            on,
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
        let off = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-flash",
            false,
            None,
            None,
        );
        assert_eq!(
            off,
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
        // 等级对 DeepSeek 底座无效——仍走 thinking.type
        let with_effort = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.deepseek.com/v1"),
            "deepseek-v4-flash",
            true,
            Some("medium"),
            None,
        );
        assert_eq!(
            with_effort,
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
    }

    /// DeepSeek 模型名经第三方代理（base_url 非 deepseek.com）也判为 DeepSeek 底座。
    #[test]
    fn deepseek_model_via_proxy_uses_thinking_type() {
        let on = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://one-api.example.com/v1"),
            "deepseek-v4-flash",
            true,
            None,
            None,
        );
        assert_eq!(
            on,
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
    }

    /// OpenAI 官方 / 其他 OpenAI 兼容（非 DeepSeek）默认档（未配置 None 或显式 ""）：
    /// 一律 omit——不发送 reasoning_effort，用模型默认档（0.21.18 起 None 与 "" 语义统一）。
    #[test]
    fn openai_compatible_default_omits_reasoning_effort() {
        let on = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            None,
            None,
        );
        assert_eq!(on, None);
        let off = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            false,
            None,
            None,
        );
        assert_eq!(off, None);
        let explicit_default = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            Some(""),
            None,
        );
        assert_eq!(explicit_default, None);
    }

    /// OpenAI 兼容显式等级：Some(档位) 原样发送；Some("none") = 显式关闭；Some("") = omit。
    #[test]
    fn openai_compatible_explicit_effort_levels() {
        let medium = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            Some("medium"),
            None,
        );
        assert_eq!(
            medium,
            Some(serde_json::json!({ "reasoning_effort": "medium" }))
        );
        let off = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            false,
            Some("none"),
            None,
        );
        assert_eq!(off, Some(serde_json::json!({ "reasoning_effort": "none" })));
        // 自定义-不发送（omit）：返回 None，绝不发可能 400 的字段值
        let omit = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            Some(""),
            None,
        );
        assert_eq!(omit, None);
    }

    /// OpenAI 兼容自定义档位：供应商私有值原样透传（如 xhigh / 20-50-80）。
    #[test]
    fn openai_compatible_custom_effort_passthrough() {
        let custom = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://one-api.example.com/v1"),
            "some-model",
            true,
            Some("xhigh"),
            None,
        );
        assert_eq!(
            custom,
            Some(serde_json::json!({ "reasoning_effort": "xhigh" }))
        );
        let proxy_custom = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://proxy.example.com/v1"),
            "kimi-k2.6",
            true,
            Some("20-50-80"),
            None,
        );
        assert_eq!(
            proxy_custom,
            Some(serde_json::json!({ "reasoning_effort": "20-50-80" }))
        );
    }

    /// Anthropic：开 → thinking.type=enabled + budget_tokens；关 → disabled。
    #[test]
    fn anthropic_toggles_thinking_type() {
        let on = thinking_request_patch(
            ProviderKind::AnthropicMessages,
            None,
            "claude-3-7-sonnet",
            true,
            None,
            None,
        );
        assert_eq!(
            on,
            Some(serde_json::json!({
                "thinking": { "type": "enabled", "budget_tokens": ANTHROPIC_THINKING_BUDGET }
            }))
        );
        let off = thinking_request_patch(
            ProviderKind::AnthropicMessages,
            None,
            "claude-3-7-sonnet",
            false,
            None,
            None,
        );
        assert_eq!(
            off,
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
    }

    /// Ollama：开 → think=true，关 → think=false。
    #[test]
    fn ollama_toggles_think_flag() {
        let on = thinking_request_patch(ProviderKind::OllamaHttp, None, "qwen3", true, None, None);
        assert_eq!(on, Some(serde_json::json!({ "think": true })));
        let off =
            thinking_request_patch(ProviderKind::OllamaHttp, None, "qwen3", false, None, None);
        assert_eq!(off, Some(serde_json::json!({ "think": false })));
    }

    /// 尚未接入的供应商（Gemini）→ None，开关不生效。
    #[test]
    fn gemini_returns_none() {
        for enabled in [true, false] {
            assert_eq!(
                thinking_request_patch(
                    ProviderKind::GeminiGenerateContent,
                    None,
                    "gemini-3-pro",
                    enabled,
                    None,
                    None,
                ),
                None
            );
        }
    }

    /// thinking_supports_effort：仅 OpenAI 兼容且非 DeepSeek 底座支持等级。
    #[test]
    fn supports_effort_scope() {
        // OpenAI 官方 → 支持
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            None,
        ));
        // 任意 OpenAI 兼容代理（非 deepseek）→ 支持
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://proxy.example.com/v1"),
            "qwen3-max",
            None,
        ));
        // DeepSeek 底座（模型名/官方 base_url）→ 不支持
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            None,
            "deepseek-v4-flash",
            None,
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://api.deepseek.com/v1"),
            "chat",
            None,
        ));
        // Anthropic / Ollama / Gemini → 不支持
        assert!(!thinking_supports_effort(
            ProviderKind::AnthropicMessages,
            None,
            "claude-3-7-sonnet",
            None,
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::OllamaHttp,
            None,
            "qwen3",
            None,
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::GeminiGenerateContent,
            None,
            "gemini-3-pro",
            None,
        ));
    }

    // ── T2: parse_supported_reasoning_efforts / pick_fallback_effort ──────────

    /// 标准 OpenAI 400 错误体——解析出 4 个合法档位。
    #[test]
    fn parse_standard_openai_400() {
        let msg = "Unsupported value: 'xhigh' is not a valid value for 'reasoning_effort'. Supported values are: 'minimal', 'low', 'medium', 'high'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 大小写不敏感——消息全大写也能解析。
    #[test]
    fn parse_case_insensitive() {
        let msg = "UNSUPPORTED VALUE: 'XHIGH' IS NOT A VALID VALUE FOR 'REASONING_EFFORT'. SUPPORTED VALUES ARE: 'MINIMAL', 'LOW', 'MEDIUM', 'HIGH'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 无 Supported values 列表 → None。
    #[test]
    fn parse_no_supported_values_returns_none() {
        let msg = "Invalid reasoning_effort value";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// 非 reasoning_effort 相关的 400 → None。
    #[test]
    fn parse_non_reasoning_effort_400_returns_none() {
        let msg = "Unsupported value: 'foo' is not a valid value for 'temperature'. Supported values are: '0', '1', '2'.";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// DeepSeek thinking.type 的 400 不含 supported values → None。
    #[test]
    fn parse_deepseek_400_returns_none() {
        let msg = "Invalid thinking type: enabled";
        assert_eq!(parse_supported_reasoning_efforts(msg), None);
    }

    /// 双引号也能解析。
    #[test]
    fn parse_double_quotes() {
        let msg = "Unsupported value: \"xhigh\" is not a valid value for \"reasoning_effort\". Supported values are: \"minimal\", \"low\", \"medium\", \"high\".";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(
            result,
            Some(vec![
                "minimal".to_string(),
                "low".to_string(),
                "medium".to_string(),
                "high".to_string(),
            ])
        );
    }

    /// 去重——重复档位只保留一份。
    #[test]
    fn parse_dedup_values() {
        let msg = "Unsupported value for 'reasoning_effort'. Supported values are: 'low', 'low', 'high', 'high'.";
        let result = parse_supported_reasoning_efforts(msg);
        assert_eq!(result, Some(vec!["low".to_string(), "high".to_string(),]));
    }

    /// pick_fallback_effort: attempted=xhigh（未知高值）→ 选支持列表中最高档 high。
    #[test]
    fn fallback_xhigh_picks_high() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("xhigh", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: attempted=high → 选 high（恰好匹配）。
    #[test]
    fn fallback_high_picks_high() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("high", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: attempted=medium → 选 medium（不选 high，因 high > medium）。
    #[test]
    fn fallback_medium_picks_medium() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("medium", &supported),
            Some("medium".to_string())
        );
    }

    /// pick_fallback_effort: attempted=low → 选 low。
    #[test]
    fn fallback_low_picks_low() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("low", &supported),
            Some("low".to_string())
        );
    }

    /// pick_fallback_effort: attempted=minimal → 选 minimal（恰好最低）。
    #[test]
    fn fallback_minimal_picks_minimal() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("minimal", &supported),
            Some("minimal".to_string())
        );
    }

    /// pick_fallback_effort: 支持列表不含 minimal（如老 o1 只收 low/medium/high），
    /// attempted=minimal → 取支持列表最低档 low。
    #[test]
    fn fallback_minimal_with_no_minimal_picks_low() {
        let supported = vec!["low".to_string(), "medium".to_string(), "high".to_string()];
        assert_eq!(
            pick_fallback_effort("minimal", &supported),
            Some("low".to_string())
        );
    }

    /// pick_fallback_effort: 自定义值 20-50-80 → 取支持列表最高档。
    #[test]
    fn fallback_custom_value_picks_highest() {
        let supported = vec![
            "minimal".to_string(),
            "low".to_string(),
            "medium".to_string(),
            "high".to_string(),
        ];
        assert_eq!(
            pick_fallback_effort("20-50-80", &supported),
            Some("high".to_string())
        );
    }

    /// pick_fallback_effort: 空支持列表 → None。
    #[test]
    fn fallback_empty_supported_returns_none() {
        assert_eq!(pick_fallback_effort("high", &[]), None);
    }

    /// pick_fallback_effort: 支持列表只有 high，attempted=low → 取 high（兜底取最低档=唯一档）。
    #[test]
    fn fallback_only_high_attempted_low() {
        let supported = vec!["high".to_string()];
        // low 的 rank=1, high 的 rank=3, 3 > 1 → 无不高于的档 → 兜底取最低=high
        assert_eq!(
            pick_fallback_effort("low", &supported),
            Some("high".to_string())
        );
    }

    // ── T3: resolve_thinking_mode 三态矩阵 + style 覆盖 ──────────────────────

    /// resolve_thinking_mode: Auto（None）完全维持现状启发式。
    #[test]
    fn resolve_auto_maintains_heuristic() {
        // OpenAI 官方 + Auto → EffortLevels
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://api.openai.com/v1"),
                "gpt-5.4-mini",
                None,
            ),
            ThinkingMode::EffortLevels
        );
        // DeepSeek 官方 base_url + Auto → DeepSeekSwitch
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://api.deepseek.com/v1"),
                "chat",
                None,
            ),
            ThinkingMode::DeepSeekSwitch
        );
        // DeepSeek 模型名 + 非 deepseek base_url + Auto → DeepSeekSwitch
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://one-api.example.com/v1"),
                "deepseek-v4-flash",
                None,
            ),
            ThinkingMode::DeepSeekSwitch
        );
        // Some(Auto) 等同 None
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://api.openai.com/v1"),
                "gpt-5.4-mini",
                Some(ThinkingStyle::Auto),
            ),
            ThinkingMode::EffortLevels
        );
    }

    /// resolve_thinking_mode: Effort 强制 EffortLevels——即使模型名是 deepseek。
    #[test]
    fn resolve_effort_forces_effort_levels() {
        // deepseek 模型名 + 非 deepseek base_url + Effort → EffortLevels（不再降级为开关）
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://one-api.example.com/v1"),
                "deepseek-v4-flash",
                Some(ThinkingStyle::Effort),
            ),
            ThinkingMode::EffortLevels
        );
        // deepseek 官方 base_url + Effort → 仍 EffortLevels
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://api.deepseek.com/v1"),
                "deepseek-v4-flash",
                Some(ThinkingStyle::Effort),
            ),
            ThinkingMode::EffortLevels
        );
    }

    /// resolve_thinking_mode: Toggle 强制 DeepSeekSwitch（OAICompatible）——即使非 deepseek。
    #[test]
    fn resolve_toggle_forces_switch_for_oai() {
        // OpenAI 官方 + Toggle → DeepSeekSwitch（控件开关，发 thinking.type）
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://api.openai.com/v1"),
                "gpt-5.4-mini",
                Some(ThinkingStyle::Toggle),
            ),
            ThinkingMode::DeepSeekSwitch
        );
        // 任意代理 + Toggle → DeepSeekSwitch
        assert_eq!(
            resolve_thinking_mode(
                ProviderKind::OpenAICompatible,
                Some("https://proxy.example.com/v1"),
                "qwen3-max",
                Some(ThinkingStyle::Toggle),
            ),
            ThinkingMode::DeepSeekSwitch
        );
    }

    /// resolve_thinking_mode: 非 OAICompatible 时 style 不生效，恒为 PlainSwitch。
    #[test]
    fn resolve_non_oai_ignores_style() {
        for style in [
            None,
            Some(ThinkingStyle::Auto),
            Some(ThinkingStyle::Effort),
            Some(ThinkingStyle::Toggle),
        ] {
            assert_eq!(
                resolve_thinking_mode(
                    ProviderKind::AnthropicMessages,
                    None,
                    "claude-3-7-sonnet",
                    style
                ),
                ThinkingMode::PlainSwitch
            );
            assert_eq!(
                resolve_thinking_mode(ProviderKind::OllamaHttp, None, "qwen3", style),
                ThinkingMode::PlainSwitch
            );
            assert_eq!(
                resolve_thinking_mode(
                    ProviderKind::GeminiGenerateContent,
                    None,
                    "gemini-3-pro",
                    style
                ),
                ThinkingMode::PlainSwitch
            );
        }
    }

    /// thinking_request_patch: style=Effort 下 deepseek 模型名走 reasoning_effort 而非 thinking.type。
    #[test]
    fn effort_style_deepseek_model_uses_reasoning_effort() {
        // deepseek 模型名 + 非 deepseek base_url + style=Effort → 发 reasoning_effort
        let patch = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://one-api.example.com/v1"),
            "deepseek-v4-flash",
            true,
            Some("medium"),
            Some(ThinkingStyle::Effort),
        );
        assert_eq!(
            patch,
            Some(serde_json::json!({ "reasoning_effort": "medium" }))
        );
        // 默认档 omit
        let omit = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://one-api.example.com/v1"),
            "deepseek-v4-flash",
            true,
            None,
            Some(ThinkingStyle::Effort),
        );
        assert_eq!(omit, None);
    }

    /// thinking_request_patch: style=Toggle 下 OpenAI 官方模型走 thinking.type。
    #[test]
    fn toggle_style_openai_uses_thinking_type() {
        let on = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            None,
            Some(ThinkingStyle::Toggle),
        );
        assert_eq!(
            on,
            Some(serde_json::json!({ "thinking": { "type": "enabled" } }))
        );
        let off = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            false,
            None,
            Some(ThinkingStyle::Toggle),
        );
        assert_eq!(
            off,
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
    }

    /// thinking_supports_effort: style=Effort 使 deepseek 模型也返回 true。
    #[test]
    fn effort_style_makes_deepseek_support_effort() {
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://one-api.example.com/v1"),
            "deepseek-v4-flash",
            Some(ThinkingStyle::Effort),
        ));
        // style=Toggle 使 OpenAI 官方也返回 false
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            Some(ThinkingStyle::Toggle),
        ));
    }

    /// ModelEntry serde: 老配置缺 thinking_style 字段 → None（零迁移）。
    #[test]
    fn model_entry_serde_legacy_without_thinking_style() {
        use crate::domain::config::ai_config::ModelEntry;
        // 最小老配置——无 thinking_style 字段
        let legacy = serde_json::json!({
            "id": "test-model",
            "display_name": "Test",
            "enabled": true,
            "capabilities": ["chat"]
        });
        let entry: ModelEntry = serde_json::from_value(legacy).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, None);
    }

    /// ModelEntry serde: thinking_style 各值正确解析。
    #[test]
    fn model_entry_serde_with_thinking_style() {
        use crate::domain::config::ai_config::{ModelEntry, ThinkingStyle};
        let with_effort = serde_json::json!({
            "id": "m1", "display_name": "M1", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "effort"
        });
        let entry: ModelEntry = serde_json::from_value(with_effort).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Effort));

        let with_toggle = serde_json::json!({
            "id": "m2", "display_name": "M2", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "toggle"
        });
        let entry: ModelEntry = serde_json::from_value(with_toggle).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Toggle));

        let with_auto = serde_json::json!({
            "id": "m3", "display_name": "M3", "enabled": true,
            "capabilities": ["chat"], "thinking_style": "auto"
        });
        let entry: ModelEntry = serde_json::from_value(with_auto).expect("反序列化应成功");
        assert_eq!(entry.thinking_style, Some(ThinkingStyle::Auto));
    }
}
