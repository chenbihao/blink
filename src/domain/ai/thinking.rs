//! 供应商 thinking 开关/强度补丁（0.21.16 + 0.21.17）——chat 对话窗口
//! （`agent_provider`）与主窗口（`rig_provider`）共用。
//!
//! 不同供应商的"开启/关闭思考"字段结构完全不同，这里把「供应商 → 开/关各发什么」
//! 收敛成单一纯函数 `thinking_request_patch`，两个调用方共享同一份逻辑。

use crate::domain::config::ai_config::ProviderKind;

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
) -> Option<serde_json::Value> {
    match kind {
        // DeepSeek 底座（官方 base_url 或 deepseek 前缀模型名）走 thinking.type
        ProviderKind::OpenAICompatible if is_deepseek_base(base_url, model_id) => Some(
            serde_json::json!({ "thinking": { "type": if thinking_enabled { "enabled" } else { "disabled" } } }),
        ),
        // OpenAI 官方 / 其他 OpenAI 兼容：Chat Completions 用 reasoning_effort 控制推理
        ProviderKind::OpenAICompatible => match reasoning_effort {
            // 默认档（未配置 None 或显式 ""）：omit——不发送该字段，用模型默认档，
            // 绝不用会触发 400 的字段值（0.21.18 起 None 与 "" 语义统一）
            None | Some("") => None,
            // 显式等级：none（关闭）或任何档位/自定义值原样发送
            Some(level) => Some(serde_json::json!({ "reasoning_effort": level })),
        },
        // Anthropic：开启必带 budget_tokens（须小于 max_tokens），关闭只发 disabled
        ProviderKind::AnthropicMessages if thinking_enabled => Some(serde_json::json!({
            "thinking": { "type": "enabled", "budget_tokens": ANTHROPIC_THINKING_BUDGET }
        })),
        ProviderKind::AnthropicMessages => Some(serde_json::json!({
            "thinking": { "type": "disabled" }
        })),
        // Ollama：本地模型，think 开关（仅支持思考的模型生效）
        ProviderKind::OllamaHttp => Some(serde_json::json!({ "think": thinking_enabled })),
        // Gemini：尚未接入
        ProviderKind::GeminiGenerateContent => None,
    }
}

/// 该 provider 是否支持 `reasoning_effort` 等级（0.21.17）——前端据此决定
/// 思考控件是"强度下拉"还是"简单开关"。仅 OpenAI 兼容且非 DeepSeek 底座支持；
/// 其余（DeepSeek/Anthropic/Ollama/Gemini）走简单开关。
pub(crate) fn thinking_supports_effort(
    kind: ProviderKind,
    base_url: Option<&str>,
    model_id: &str,
) -> bool {
    matches!(kind, ProviderKind::OpenAICompatible) && !is_deepseek_base(base_url, model_id)
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
        );
        assert_eq!(on, None);
        let off = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            false,
            None,
        );
        assert_eq!(off, None);
        let explicit_default = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            Some(""),
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
        );
        assert_eq!(off, Some(serde_json::json!({ "reasoning_effort": "none" })));
        // 自定义-不发送（omit）：返回 None，绝不发可能 400 的字段值
        let omit = thinking_request_patch(
            ProviderKind::OpenAICompatible,
            Some("https://api.openai.com/v1"),
            "gpt-5.4-mini",
            true,
            Some(""),
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
        );
        assert_eq!(
            off,
            Some(serde_json::json!({ "thinking": { "type": "disabled" } }))
        );
    }

    /// Ollama：开 → think=true，关 → think=false。
    #[test]
    fn ollama_toggles_think_flag() {
        let on = thinking_request_patch(ProviderKind::OllamaHttp, None, "qwen3", true, None);
        assert_eq!(on, Some(serde_json::json!({ "think": true })));
        let off = thinking_request_patch(ProviderKind::OllamaHttp, None, "qwen3", false, None);
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
        ));
        // 任意 OpenAI 兼容代理（非 deepseek）→ 支持
        assert!(thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://proxy.example.com/v1"),
            "qwen3-max",
        ));
        // DeepSeek 底座（模型名/官方 base_url）→ 不支持
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            None,
            "deepseek-v4-flash",
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::OpenAICompatible,
            Some("https://api.deepseek.com/v1"),
            "chat",
        ));
        // Anthropic / Ollama / Gemini → 不支持
        assert!(!thinking_supports_effort(
            ProviderKind::AnthropicMessages,
            None,
            "claude-3-7-sonnet",
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::OllamaHttp,
            None,
            "qwen3",
        ));
        assert!(!thinking_supports_effort(
            ProviderKind::GeminiGenerateContent,
            None,
            "gemini-3-pro",
        ));
    }
}
