//! AI 提示词统一管理（0.11.3 改进 4）。
//!
//! 从 `service.rs::build_routing_prompt` 迁出，新增 `tool_result_feedback_prompt`
//! （Turn 2 用，0.11.4 改进 2 消费），工具列表增强（含参数摘要 + 插件 hint）。
//!
//! **三函数设计**（文档 §2.4）：
//! - `routing_system_prompt(tools, lang)` —— 主窗口路由 system prompt
//! - `tool_result_feedback_prompt(tools, lang)` —— Turn 2 结果回流 system prompt
//! - `tool_list_section(tools)` —— 工具列表文字段（含 name + description + 参数名 + hint）
//!
//! **token 成本控制**（§3.8）：每次构建 system prompt 估算 token 数，超 1500 token
//! `tracing::warn!` 告警。工具描述走"分层详略"——system prompt 文字段只含 name +
//! 一句话 description + 参数名（不含 schema），完整 JSON Schema 走 `tools` 协议字段。
//!
//! **插件贡献 prompt hint**：manifest 的 `tools[].hint` 字段自动拼入 system prompt
//! 工具描述段——插件作者一句话告诉 AI 这个工具的用法窍门。

use crate::domain::ai::skill::{SkillEntry, SkillSummary};
use crate::domain::schema::ToolSchema;
use std::collections::HashMap;

/// system prompt token 告警阈值（§3.8：超 1500 token warn）。
const TOKEN_WARN_THRESHOLD: usize = 1500;

/// 工具提示词信息——`ToolSchema` 的超集，多了 `hint` 字段。
///
/// `hint` 来自 manifest `tools[].hint`（0.11.1 改进 3a），`ToolSchema` 不携带
/// （它是协议层描述，hint 是 prompt 层元数据）。调用方从 `ToolSchema` + hints map
/// 构造 `ToolPromptInfo` 列表传入。
#[derive(Debug, Clone)]
pub struct ToolPromptInfo {
    /// 工具名（与 ToolSchema.name 一致）。
    pub name: String,
    /// 人类可读描述。
    pub description: String,
    /// JSON Schema Object。
    pub parameters: serde_json::Value,
    /// 给 AI 的额外提示（manifest `tools[].hint`），自动拼入 system prompt。
    pub hint: Option<String>,
}

impl ToolPromptInfo {
    /// 从 `ToolSchema` 构造（无 hint）。
    #[allow(dead_code)] // 便利 API，build_prompt_infos 批量构造时用 from_schema_with_hint
    pub fn from_schema(schema: ToolSchema) -> Self {
        ToolPromptInfo {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            hint: None,
        }
    }

    /// 从 `ToolSchema` + hint 构造。
    pub fn from_schema_with_hint(schema: ToolSchema, hint: Option<String>) -> Self {
        ToolPromptInfo {
            name: schema.name,
            description: schema.description,
            parameters: schema.parameters,
            hint,
        }
    }
}

/// 从 `Vec<ToolSchema>` + hints map 批量构造 `ToolPromptInfo` 列表。
///
/// `hints` 的 key 是 tool name（如 `"builtin.weather:get_weather"`），
/// value 是 manifest `tools[].hint` 的值。
pub fn build_prompt_infos(
    tools: Vec<ToolSchema>,
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

/// Turn 2 结果回流 system prompt（0.11.4 改进 2 消费）。
///
/// AI 拿到工具返回结果后，用此 prompt 做第二轮 completion——总结结果 /
/// 链式调 safe tool 完成闭环 / 解释错误。
///
/// **tool chain 闭环**：用户说"打开微信" → Turn 1 `search_apps` 返回路径 →
/// Turn 2 AI 调 `file_action(action=open_path, path=...)` 直接打开。
/// `file_action` 是分组 tool，`open_path` 是 Safe action，
/// `handle_turn2_tool_call` 会自动执行（不需用户确认）。
///
/// `_lang` 参数预留 i18n。
#[allow(dead_code)] // 0.11.4 Turn 2 回流用，测试消费
pub fn tool_result_feedback_prompt(tools: &[ToolPromptInfo], _lang: &str) -> String {
    let mut prompt = String::from(
        "你刚才调用了工具并拿到了结果。现在请基于结果回应用户。\n\n\
         【行为准则】\n\
         1. 如果用户意图是执行操作（如打开应用/文件/网址），且结果中包含目标路径 → \
         调用 file_action 工具（action=open_path, path=结果中的路径）直接打开，不要只返回路径文字\n\
         2. 如果结果是数据/列表 → 用自然语言总结最可能关心的，提示\"按 ↓ 查看完整列表\"\n\
         3. 如果工具返回错误 → 用自然语言解释原因 + 给出可操作的修复建议\n\
         （如\"API key 未配置，请到设置页 AI tab 配置\"），不要只说\"失败了\"\n\
         4. 不要复述原始 JSON 数据，不要说\"我没有权限\"或\"没有可用的工具\"——\
         下方列出的安全工具你都可以调用\n\n",
    );

    if !tools.is_empty() {
        prompt.push_str("【可调用的安全工具】\n");
        prompt.push_str(&tool_list_section(tools));
        prompt.push('\n');
    }

    prompt.push_str("【语言】跟随用户输入语言回答。");

    // token 监控（§3.8）
    let tokens = estimate_tokens(&prompt);
    if tokens > TOKEN_WARN_THRESHOLD {
        tracing::warn!(
            tokens = tokens,
            threshold = TOKEN_WARN_THRESHOLD,
            tools_count = tools.len(),
            "Turn 2 feedback prompt 超过 {} token",
            TOKEN_WARN_THRESHOLD,
        );
    } else {
        tracing::debug!(
            tokens = tokens,
            tools_count = tools.len(),
            "Turn 2 feedback prompt 构建",
        );
    }

    prompt
}

/// 独立 chat 窗口的 Agent system prompt（0.12.1 Phase 3B）。
///
/// Tool schema 由 rig AgentBuilder 独立挂载，此处只约束对话角色、工具使用和安全行为。
pub fn chat_system_prompt() -> String {
    String::from(
        "你是 Blink 的 AI 助手。请直接、准确地帮助用户完成任务。\n\n\
         【工具使用】\n\
         1. 需要读取环境或执行操作时，优先调用已提供的工具，不要声称拥有不存在的能力。\n\
         2. 不确定工具参数时先向用户澄清，不要猜测路径、应用名或不可逆参数。\n\
         3. 危险操作必须等待 Blink 的用户确认；未确认、被拒绝或超时都视为未执行。\n\
         4. 工具失败时如实说明原因，并给出可操作的修复建议。\n\n\
         【安全】不要主动退出 Blink、规避确认机制或假称操作成功。\n\n\
         【语言】跟随用户输入语言回答。",
    )
}

/// 纯对话模式的 system prompt。明确声明当前对话无外部工具，避免模型依据普通
/// Agent prompt 猜测可用能力；分组提示仍由调用方通过统一 helper 追加。
pub fn pure_chat_system_prompt() -> String {
    String::from(
        "你是 Blink 的 AI 助手。请直接、准确地帮助用户完成任务。\n\n\
         【当前模式】这是纯对话，当前对话没有可调用的外部工具。请仅基于对话上下文回答；\
         不要声称读取了环境、执行了操作或调用了工具。\n\n\
         【安全】不要假称操作成功。\n\n\
         【语言】跟随用户输入语言回答。",
    )
}

fn append_group_prompt(base: String, group_prompt: Option<&str>) -> String {
    match group_prompt {
        Some(p) if !p.is_empty() => format!("{base}\n\n【分组指令】\n{p}"),
        _ => base,
    }
}

/// 带分组系统提示词的 chat system prompt（0.12.6）。
///
/// 在基础 system prompt 之后追加分组级系统提示词（如果有）。
/// 分组提示词用于给特定场景下的对话设定角色或行为约束，如"你是翻译助手"。
///
/// `group_prompt` 为 None 或空字符串时，退化为 `chat_system_prompt()`。
pub fn chat_system_prompt_with_group(group_prompt: Option<&str>) -> String {
    append_group_prompt(chat_system_prompt(), group_prompt)
}

/// 带可选分组指令的纯对话 system prompt。
pub fn pure_chat_system_prompt_with_group(group_prompt: Option<&str>) -> String {
    append_group_prompt(pure_chat_system_prompt(), group_prompt)
}

/// 带分组系统提示词 + Skill 注入的完整 chat system prompt（0.13.3）。
///
/// 在 `chat_system_prompt_with_group` 基础上追加 Skill 内容（渐进式披露）：
/// - 阶段 1（常驻）：所有 Skill 的 `name + description` 摘要，标注来源
/// - 阶段 2（按需）：命中触发条件的 Skill 的完整 SKILL.md 全文
///
/// preamble 字符串变化 → `hash_preamble()` 变 → AgentProvider cache miss → 重建。
/// 无需手工失效缓存（0.12.6 hash 机制已覆盖）。
///
/// `group_prompt` 为 None 或空时不追加分组指令。
/// `skill_summaries` 为空时不追加【可用技能】段。
/// `triggered_skills` 为空时不追加【已激活技能详情】段。
pub fn chat_system_prompt_with_skills(
    group_prompt: Option<&str>,
    skill_summaries: &[SkillSummary],
    triggered_skills: &[SkillEntry],
) -> String {
    let mut prompt = chat_system_prompt_with_group(group_prompt);

    // 阶段 1：所有 Skill 摘要（常驻，~50 token/skill）
    if !skill_summaries.is_empty() {
        prompt.push_str("\n\n【可用技能】\n");
        for s in skill_summaries {
            let trigger_hint = if s.has_triggers {
                " (自动触发)"
            } else {
                ""
            };
            prompt.push_str(&format!(
                "- [{}] {}{}: {}\n",
                s.source.display_name(),
                s.name,
                trigger_hint,
                s.description
            ));
        }
        prompt.push_str(
            "提示：输入 /skill <名称> 可手动激活技能。可用 /skill <名称>@<来源> 消歧同名技能。",
        );
    }

    // 阶段 2：触发的 Skill 全文（按需注入）
    if !triggered_skills.is_empty() {
        prompt.push_str("\n\n【已激活技能详情】\n");
        for skill in triggered_skills {
            prompt.push_str(&format!(
                "--- {} ({}) ---\n{}\n",
                skill.name,
                skill.source.display_name(),
                skill.full_content
            ));
        }
    }

    // token 监控
    let tokens = estimate_tokens(&prompt);
    if tokens > TOKEN_WARN_THRESHOLD {
        tracing::warn!(
            tokens = tokens,
            threshold = TOKEN_WARN_THRESHOLD,
            skill_count = skill_summaries.len(),
            triggered_count = triggered_skills.len(),
            "chat system prompt (with skills) 超过 {} token",
            TOKEN_WARN_THRESHOLD,
        );
    }

    prompt
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
        if let Some(hint) = &t.hint
            && !hint.is_empty()
        {
            buf.push_str(&format!("。提示: {hint}"));
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
    use crate::domain::ai::skill::SkillSource;
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
        let p = tool_result_feedback_prompt(&[], "zh");
        assert!(p.contains("错误"));
        assert!(p.contains("修复建议"));
        assert!(p.contains("API key"));
    }

    #[test]
    fn feedback_prompt_includes_tool_list_when_nonempty() {
        let tools = vec![make_tool(
            "open_path",
            "Open a file or directory",
            json!({"type":"object","properties":{"path":{"type":"string","description":"file path"}}}),
        )];
        let p = tool_result_feedback_prompt(&tools, "zh");
        assert!(p.contains("open_path"));
        assert!(p.contains("安全工具"));
    }

    #[test]
    fn feedback_prompt_guides_tool_chain_execution() {
        // Turn 2 回流引导 AI 在用户意图是执行操作时调 file_action(action=open_path)
        let p = tool_result_feedback_prompt(&[], "zh");
        assert!(p.contains("file_action"));
        assert!(p.contains("open_path"));
        assert!(p.contains("不要说\"我没有权限\""));
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
        let mut tool = make_tool(
            "get_ip",
            "获取 IP",
            json!({"type":"object","properties":{}}),
        );
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
            ToolSchema {
                name: "get_weather".into(),
                description: "天气".into(),
                parameters: json!({"type":"object","properties":{}}),
            },
            ToolSchema {
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
        assert_eq!(infos[1].hint.as_deref(), Some("公网 IP 最有价值"));
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

    #[test]
    fn chat_prompt_contains_tool_and_safety_contracts() {
        let prompt = chat_system_prompt();
        assert!(prompt.contains("工具"));
        assert!(prompt.contains("用户确认"));
        assert!(prompt.contains("不要主动退出 Blink"));
        assert!(prompt.contains("跟随用户输入语言"));
    }

    #[test]
    fn chat_prompt_with_group_appends_group_prompt() {
        let prompt = chat_system_prompt_with_group(Some("你是翻译助手"));
        assert!(prompt.contains("工具"), "基础 prompt 应保留");
        assert!(prompt.contains("分组指令"));
        assert!(prompt.contains("你是翻译助手"));
    }

    #[test]
    fn chat_prompt_with_group_none_equals_base() {
        let prompt = chat_system_prompt_with_group(None);
        assert_eq!(prompt, chat_system_prompt());
    }

    #[test]
    fn chat_prompt_with_group_empty_equals_base() {
        let prompt = chat_system_prompt_with_group(Some(""));
        assert_eq!(prompt, chat_system_prompt());
    }

    #[test]
    fn pure_chat_prompt_explicitly_has_no_tools() {
        let prompt = pure_chat_system_prompt();
        assert!(prompt.contains("纯对话"));
        assert!(prompt.contains("没有可调用的外部工具"));
        assert!(!prompt.contains("危险操作必须等待"));
    }

    #[test]
    fn pure_chat_prompt_preserves_group_instruction() {
        let prompt = pure_chat_system_prompt_with_group(Some("你是翻译助手"));
        assert!(prompt.contains("没有可调用的外部工具"));
        assert!(prompt.contains("分组指令"));
        assert!(prompt.contains("你是翻译助手"));
    }

    // ── chat_system_prompt_with_skills ──

    fn make_summary(
        name: &str,
        desc: &str,
        source: SkillSource,
        has_triggers: bool,
    ) -> SkillSummary {
        SkillSummary {
            name: name.to_string(),
            description: desc.to_string(),
            source,
            has_triggers,
        }
    }

    #[test]
    fn chat_prompt_with_skills_includes_summaries() {
        let summaries = vec![
            make_summary("rust-debug", "Debug Rust errors", SkillSource::Blink, true),
            make_summary("translator", "Translate text", SkillSource::Claude, false),
        ];
        let prompt = chat_system_prompt_with_skills(None, &summaries, &[]);
        assert!(prompt.contains("可用技能"));
        assert!(prompt.contains("[blink] rust-debug"));
        assert!(prompt.contains("Debug Rust errors"));
        assert!(prompt.contains("[claude] translator"));
        assert!(prompt.contains("(自动触发)"), "有 triggers 的 skill 应标注");
        assert!(prompt.contains("/skill"), "应提示 /skill 指令");
    }

    #[test]
    fn chat_prompt_with_skills_includes_triggered_full_content() {
        let summaries = vec![make_summary(
            "rust-debug",
            "Debug",
            SkillSource::Blink,
            true,
        )];
        let triggered = vec![SkillEntry {
            name: "rust-debug".to_string(),
            description: "Debug".to_string(),
            triggers: None,
            compiled_patterns: Vec::new(),
            full_content: "# Rust Debug Workflow\n\n1. Read error\n2. Fix".to_string(),
            source: SkillSource::Blink,
            dir_path: std::path::PathBuf::from("/tmp"),
            source_cli_path: None,
        }];
        let prompt = chat_system_prompt_with_skills(None, &summaries, &triggered);
        assert!(prompt.contains("已激活技能详情"));
        assert!(prompt.contains("Rust Debug Workflow"));
        assert!(prompt.contains("Read error"));
    }

    #[test]
    fn chat_prompt_with_skills_empty_equals_group_prompt() {
        let prompt = chat_system_prompt_with_skills(None, &[], &[]);
        assert_eq!(prompt, chat_system_prompt());
    }

    #[test]
    fn chat_prompt_with_skills_preserves_group_prompt() {
        let prompt = chat_system_prompt_with_skills(Some("你是翻译助手"), &[], &[]);
        assert!(prompt.contains("分组指令"));
        assert!(prompt.contains("你是翻译助手"));
    }

    #[test]
    fn chat_prompt_with_skills_combines_group_and_skills() {
        let summaries = vec![make_summary(
            "test",
            "Test skill",
            SkillSource::Blink,
            false,
        )];
        let prompt = chat_system_prompt_with_skills(Some("你是助手"), &summaries, &[]);
        assert!(prompt.contains("分组指令"), "分组指令应保留");
        assert!(prompt.contains("可用技能"), "技能摘要应追加");
    }

    /// 辅助：构造单元素 Vec
    fn tools_vec(tool: &[ToolPromptInfo]) -> Vec<ToolPromptInfo> {
        tool.to_vec()
    }
}
