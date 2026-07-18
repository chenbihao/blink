//! AI 提示词统一管理（0.11.3 改进 4）。
//!
//! 从 `service.rs::build_routing_prompt` 迁出，新增 `tool_result_feedback_prompt`
//! （Turn 2 用，0.11.4 改进 2 消费），工具列表增强（含参数摘要 + 插件 hint）。
//!
//! **三函数设计**（文档 §2.4）：
//! - `routing_system_prompt(tools, lang)` —— 主窗口路由 system prompt
//! - `tool_result_feedback_prompt(lang)` —— Turn 2 结果回流 system prompt
//! - `tool_list_section(tools)` —— 工具列表文字段（含 name + description + 参数名 + hint）
//!
//! **token 成本控制**（§3.8）：每次构建 system prompt 估算 token 数，超 1500 token
//! `tracing::warn!` 告警。工具描述走"分层详略"——system prompt 文字段只含 name +
//! 一句话 description + 参数名（不含 schema），完整 JSON Schema 走 `tools` 协议字段。
//!
//! **插件贡献 prompt hint**：manifest 的 `tools[].hint` 字段自动拼入 system prompt
//! 工具描述段——插件作者一句话告诉 AI 这个工具的用法窍门。

use crate::domain::execution::ActionSchema;
use std::collections::HashMap;

/// system prompt token 告警阈值（§3.8：超 1500 token warn）。
const TOKEN_WARN_THRESHOLD: usize = 1500;

/// 工具提示词信息——`ActionSchema` 的超集，多了 `hint` 字段。
///
/// `hint` 来自 manifest `tools[].hint`（0.11.1 改进 3a），`ActionSchema` 不携带
/// （它是协议层描述，hint 是 prompt 层元数据）。调用方从 `ActionSchema` + hints map
/// 构造 `ToolPromptInfo` 列表传入。
#[derive(Debug, Clone)]
pub struct ToolPromptInfo {
    /// 工具名（与 ActionSchema.name 一致）。
    pub name: String,
    /// 人类可读描述。
    pub description: String,
    /// JSON Schema Object。
    pub parameters: serde_json::Value,
    /// 给 AI 的额外提示（manifest `tools[].hint`），自动拼入 system prompt。
    pub hint: Option<String>,
}

impl ToolPromptInfo {
    /// 从 `ActionSchema` 构造（无 hint）。
    #[allow(dead_code)] // 便利 API，build_prompt_infos 批量构造时用 from_schema_with_hint
    pub fn from_schema(schema: ActionSchema) -> Self {
        ToolPromptInfo {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            hint: None,
        }
    }

    /// 从 `ActionSchema` + hint 构造。
    pub fn from_schema_with_hint(schema: ActionSchema, hint: Option<String>) -> Self {
        ToolPromptInfo {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            hint,
        }
    }
}

/// 从 `Vec<ActionSchema>` + hints map 批量构造 `ToolPromptInfo` 列表。
///
/// `hints` 的 key 是 tool name（如 `"builtin.weather:get_weather"`），
/// value 是 manifest `tools[].hint` 的值。
pub fn build_prompt_infos(
    tools: Vec<ActionSchema>,
    hints: &HashMap<String, String>,
) -> Vec<ToolPromptInfo> {
    tools
        .into_iter()
        .map(|schema| {
            let hint = hints.get(&schema.name).cloned();
            ToolPromptInfo::from_schema_with_hint(schema, hint)
        })
        .collect()
}

// ── token 估算 ─────────────────────────────────────────────────────────────────

/// 估算文本的 token 数（启发式，非精确 BPE）。
///
/// **为什么不用 `tiktoken-rs`**：文档 §3.8 提到 `tiktoken-rs`，但该 crate 捆绑
/// ~1.8MB BPE 词表，对"仅监控告警"的场景过重。启发式估算在 ±20% 内足够判断
/// 是否超 1500 阈值。若 0.12 本地模型需要精确 token 计数（context window 截断），
/// 再引入 `tiktoken-rs` 替换此函数。
///
/// **估算规则**：
/// - CJK 字符（中日韩统一表意文字 + 韩文 + 全角符号）：1 token/char
/// - 其他字符（ASCII 字母/数字/标点/空格）：4 char/token（向上取整）
///
/// 这个规则基于 GPT tokenizer 的经验观察：CJK 文本约 1 token/字，英文约
/// 4 char/token。混合文本按字符类型分别计数后求和。
pub fn estimate_tokens(text: &str) -> usize {
    let mut cjk = 0usize;
    let mut other = 0usize;
    for ch in text.chars() {
        if is_cjk(ch) {
            cjk += 1;
        } else {
            other += 1;
        }
    }
    // CJK: ~1 token/char; 非 CJK: ~4 char/token（向上取整）
    cjk + other.div_ceil(4)
}

/// 判断字符是否为 CJK（中日韩统一表意文字 + 韩文 + 全角符号）。
fn is_cjk(ch: char) -> bool {
    let code = ch as u32;
    matches!(
        code,
        0x3000..=0x9FFF   // CJK 符号和标点 + 统一表意文字 + 假名
        | 0xAC00..=0xD7AF // 韩文音节
        | 0xF900..=0xFAFF // CJK 兼容表意文字
        | 0xFF00..=0xFFEF // 半角/全角形式
    )
}

// ── system prompt 构建 ──────────────────────────────────────────────────────────

/// 主窗口路由 system prompt（从 `service.rs::build_routing_prompt` 迁出 + 增强）。
///
/// **增强点**（D4 修复 + 改进 4）：
/// 1. 工具列表从 `"- {name}: {desc}"` 升级为含参数摘要 + hint
/// 2. 拼入插件 manifest `tools[].hint` 字段
/// 3. 构建 token 估算 + 超阈值 warn
///
/// `_lang` 参数预留 i18n（0.x 阶段 prompt 用中文，AI 按用户输入语言回答）。
pub fn routing_system_prompt(tools: &[ToolPromptInfo], _lang: &str) -> String {
    let mut prompt =
        String::from("你是 Blink 的意图路由器。用户输入未命中确定性规则,由你判断意图。\n\n");

    if !tools.is_empty() {
        prompt.push_str("【可用工具】\n");
        prompt.push_str(&tool_list_section(tools));
        prompt.push('\n');
    }

    prompt.push_str(
        "【判断规则】\n\
         1. 如果用户意图明确匹配某个工具 → 返回该工具的 tool_call（参数从用户输入提取）\n\
         2. 如果是问题/对话/翻译请求/不确定 → 直接文本回答\n\
         3. 不确定时宁可文本回答,不要猜测工具参数\n\n\
         【语言】跟随用户输入语言回答。",
    );

    // token 监控（§3.8）
    let tokens = estimate_tokens(&prompt);
    if tokens > TOKEN_WARN_THRESHOLD {
        tracing::warn!(
            tokens = tokens,
            threshold = TOKEN_WARN_THRESHOLD,
            tools_count = tools.len(),
            "system prompt 超过 {} token，可能影响 AI 响应质量与延迟",
            TOKEN_WARN_THRESHOLD,
        );
    } else {
        tracing::debug!(
            target: crate::infra::utils::perf::ai_slo::TARGET,
            tokens = tokens,
            tools_count = tools.len(),
            "system prompt 构建",
        );
    }

    prompt
}

/// Turn 2 结果回流 system prompt（0.11.4 改进 2 消费，此版本预置）。
///
/// 文档 §2.2.4 的 prompt 原文。AI 拿到工具返回结果后，用此 prompt 做第二轮
/// completion——总结结果 / 链式调 safe tool / 解释错误。
///
/// `_lang` 参数预留 i18n。
#[allow(dead_code)] // 0.11.4 改进 2 消费
pub fn tool_result_feedback_prompt(_lang: &str) -> String {
    "你刚才调用了工具,以下是工具返回的结果。\n\
     请基于结果用自然语言回答用户,不要简单复述原始数据。\n\
     如果结果包含多项,挑用户最可能关心的总结;用户想看全部时可提示\"按 ↓ 查看完整列表\"。\n\
     若需要执行后续操作(如打开搜到的应用),可调用安全工具;危险操作需用户确认。\n\
     若工具返回错误,请用自然语言解释错误原因,并给出用户可操作的修复建议\n\
     (如\"API key 未配置,请到设置页 AI tab 配置天气插件密钥\")。不要只说\"失败了\",要让用户知道下一步怎么办。"
        .to_string()
}

/// 工具列表文字段（含 name + description + 参数摘要 + hint）。
///
/// **分层详略**（§3.8）：system prompt 文字段只含 name + 一句话 description + 参数名
/// （不含完整 schema，~30 token/工具）；完整 JSON Schema 走 `tools` 协议字段。
///
/// 格式示例：
/// ```text
/// - get_weather: 查询指定城市天气。参数: city(城市名称), unit(温度单位)。提示: 返回结构化数据
/// - get_ip: 获取本机 IP 地址信息。参数: include_ipv6(是否包含 IPv6 地址)。提示: 返回多个 IP,公网 IP 通常最有价值
/// - system_action: 系统操作。参数: action(要执行的操作)
/// ```
fn tool_list_section(tools: &[ToolPromptInfo]) -> String {
    let mut buf = String::with_capacity(tools.len() * 80);
    for t in tools {
        buf.push_str(&format!("- {}: {}", t.name, t.description));

        // 参数摘要：从 JSON Schema 提取参数名 + description 截断
        if let Some(params_summary) = parameters_summary(&t.parameters) {
            buf.push_str(&format!("。参数: {params_summary}"));
        }

        // 插件 hint（manifest `tools[].hint`）
        if let Some(hint) = &t.hint {
            if !hint.is_empty() {
                buf.push_str(&format!("。提示: {hint}"));
            }
        }

        buf.push('\n');
    }
    buf
}

/// 从 JSON Schema 提取参数摘要（`name(desc), name2(desc2)` 格式）。
///
/// - 跳过 `action` 字段（分组 tool 的内部字段，不给 AI 看）
/// - description 截断到 20 字（省 token）
/// - 无参数返回 None
fn parameters_summary(parameters: &serde_json::Value) -> Option<String> {
    let props = parameters.get("properties")?.as_object()?;
    if props.is_empty() {
        return None;
    }

    let summaries: Vec<String> = props
        .iter()
        .filter(|(name, _)| name.as_str() != "action") // 跳过分组 tool 的 action 字段
        .map(|(name, schema)| {
            let desc = schema
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("");
            // 截断 description 到 20 字（省 token），空 description 只显示参数名
            if desc.is_empty() {
                name.clone()
            } else {
                let truncated = truncate_chars(desc, 20);
                format!("{name}({truncated})")
            }
        })
        .collect();

    if summaries.is_empty() {
        None
    } else {
        Some(summaries.join(", "))
    }
}

/// 按字符数截断字符串（不按字节，避免中文 panic），超长追加 `…`。
fn truncate_chars(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        return s.to_string();
    }
    let truncated: String = chars.iter().take(max).collect();
    format!("{truncated}…")
}

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn make_tool(name: &str, desc: &str, params: serde_json::Value) -> ToolPromptInfo {
        ToolPromptInfo {
            name: name.to_string(),
            description: desc.to_string(),
            parameters: params,
            hint: None,
        }
    }

    // ── estimate_tokens ──

    #[test]
    fn estimate_tokens_pure_ascii() {
        // 纯 ASCII：4 char/token（向上取整）。22 字符 → div_ceil(22,4) = 6 token
        assert_eq!(estimate_tokens("Hello world, test 123!"), 6);
    }

    #[test]
    fn estimate_tokens_pure_cjk() {
        // 纯中文：1 token/char。5 字 → 5 token
        assert_eq!(estimate_tokens("你好世界啊"), 5);
    }

    #[test]
    fn estimate_tokens_mixed() {
        // 混合：3 中文(3 token) + 8 ASCII(2 token) = 5 token
        assert_eq!(estimate_tokens("你好啊 hello!"), 5);
    }

    #[test]
    fn estimate_tokens_empty() {
        assert_eq!(estimate_tokens(""), 0);
    }

    // ── routing_system_prompt ──

    #[test]
    fn routing_prompt_contains_judgment_rules() {
        let prompt = routing_system_prompt(&[], "zh");
        assert!(prompt.contains("判断规则"));
        assert!(prompt.contains("跟随用户输入语言"));
    }

    #[test]
    fn routing_prompt_includes_tool_list_when_nonempty() {
        let tools = vec![make_tool(
            "get_weather",
            "查询天气",
            json!({"type":"object","properties":{"city":{"type":"string","description":"城市名"}}}),
        )];
        let prompt = routing_system_prompt(&tools, "zh");
        assert!(prompt.contains("get_weather"));
        assert!(prompt.contains("查询天气"));
        assert!(prompt.contains("city"));
    }

    #[test]
    fn routing_prompt_omits_tool_list_when_empty() {
        let prompt = routing_system_prompt(&[], "zh");
        assert!(!prompt.contains("可用工具"));
    }

    // ── tool_result_feedback_prompt ──

    #[test]
    fn feedback_prompt_contains_error_guidance() {
        let p = tool_result_feedback_prompt("zh");
        assert!(p.contains("错误"));
        assert!(p.contains("修复建议"));
        assert!(p.contains("API key"));
    }

    // ── tool_list_section ──

    #[test]
    fn tool_list_includes_parameter_names() {
        let tools = vec![make_tool(
            "get_weather",
            "查询天气",
            json!({
                "type": "object",
                "properties": {
                    "city": {"type":"string","description":"城市名"},
                    "unit": {"type":"string","description":"温度单位"}
                }
            }),
        )];
        let section = tool_list_section(&tools);
        assert!(section.contains("city(城市名)"));
        assert!(section.contains("unit(温度单位)"));
    }

    #[test]
    fn tool_list_skips_action_field() {
        // 分组 tool 的 action 字段不展示给 AI
        let tools = vec![make_tool(
            "system_action",
            "系统操作",
            json!({
                "type": "object",
                "properties": {
                    "action": {"type":"string","enum":["lock","shutdown"],"description":"要执行的操作"},
                    "url": {"type":"string","description":"URL"}
                }
            }),
        )];
        let section = tool_list_section(&tools);
        assert!(!section.contains("action("));
        assert!(section.contains("url(URL)"));
    }

    #[test]
    fn tool_list_includes_hint_when_present() {
        let mut tool = make_tool(
            "get_ip",
            "获取 IP",
            json!({"type":"object","properties":{}}),
        );
        tool.hint = Some("返回多个 IP,公网 IP 通常最有价值".to_string());
        let section = tool_list_section(&tools_vec(&[tool]));
        assert!(section.contains("提示: 返回多个 IP"));
    }

    #[test]
    fn tool_list_omits_hint_when_empty() {
        let mut tool = make_tool("get_ip", "获取 IP", json!({"type":"object","properties":{}}));
        tool.hint = Some(String::new());
        let section = tool_list_section(&tools_vec(&[tool]));
        assert!(!section.contains("提示:"));
    }

    #[test]
    fn tool_list_truncates_long_description() {
        let long_desc = "这是一个非常非常非常非常非常非常非常非常非常非常非常长的参数描述";
        let tools = vec![make_tool(
            "test",
            "测试",
            json!({"type":"object","properties":{"param":{"type":"string","description":long_desc}}}),
        )];
        let section = tool_list_section(&tools);
        // 截断后 20 字 + …
        assert!(section.contains("…"));
        // 原文不应完整出现（超过 20 字被截断）
        assert!(!section.contains(long_desc));
    }

    #[test]
    fn tool_list_param_without_description_shows_name_only() {
        let tools = vec![make_tool(
            "test",
            "测试",
            json!({"type":"object","properties":{"flag":{"type":"boolean"}}}),
        )];
        let section = tool_list_section(&tools);
        assert!(section.contains("flag"));
        assert!(!section.contains("flag()")); // 无 description 不加括号
    }

    #[test]
    fn tool_list_no_params_omits_param_section() {
        let tools = vec![make_tool(
            "lock",
            "锁屏",
            json!({"type":"object","properties":{}}),
        )];
        let section = tool_list_section(&tools);
        assert!(!section.contains("参数:"));
    }

    // ── parameters_summary ──

    #[test]
    fn parameters_summary_returns_none_for_empty_properties() {
        let params = json!({"type":"object","properties":{}});
        assert!(parameters_summary(&params).is_none());
    }

    #[test]
    fn parameters_summary_returns_none_for_no_properties_key() {
        let params = json!({"type":"object"});
        assert!(parameters_summary(&params).is_none());
    }

    // ── build_prompt_infos ──

    #[test]
    fn build_prompt_infos_attaches_hints() {
        let tools = vec![
            ActionSchema {
                name: "get_weather".into(),
                description: "天气".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
            ActionSchema {
                name: "get_ip".into(),
                description: "IP".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
        ];
        let mut hints = HashMap::new();
        hints.insert("get_ip".to_string(), "公网 IP 最有价值".to_string());

        let infos = build_prompt_infos(tools, &hints);
        assert_eq!(infos.len(), 2);
        assert!(infos[0].hint.is_none()); // get_weather 无 hint
        assert_eq!(
            infos[1].hint.as_deref(),
            Some("公网 IP 最有价值")
        );
    }

    // ── truncate_chars ──

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate_chars("短文本", 10), "短文本");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        let result = truncate_chars("一二三四五六七八九十", 5);
        assert_eq!(result, "一二三四五…");
    }

    #[test]
    fn truncate_exact_length_no_ellipsis() {
        assert_eq!(truncate_chars("一二三", 3), "一二三");
    }

    /// 辅助：构造单元素 Vec
    fn tools_vec(tool: &[ToolPromptInfo]) -> Vec<ToolPromptInfo> {
        tool.to_vec()
    }
}
