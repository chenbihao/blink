//! AI 提示词统一管理（0.11.3 改进 4；0.27.2 删除意图路由死代码、标题 prompt 迁入）。
//!
//! **在册函数**：
//! - `chat_system_prompt*` / `pure_chat_system_prompt*` —— 对话窗口 Agent preamble 族
//! - `conversation_title_system_prompt()` —— 对话标题生成（0.27.2 从 app 层迁入，纯函数）
//! - `build_prompt_infos` / `ToolPromptInfo` —— 工具定义 → prompt 元数据投影
//!
//! **0.27.2 清理**：意图路由与 Turn 2 结果回流两个死 prompt（运行时调用方已随
//! 0.17.6 AI lane 移除，长期 `#[allow(dead_code)]` 仅测试消费）连同其专属工具链
//! （工具列表文字段 / 参数摘要）一并删除。
//!
//! **token 成本控制**（§3.8）：构建 system prompt 时估算 token 数，超 5000 token
//! `tracing::warn!` 告警。工具描述走"分层详略"——完整 JSON Schema 走 `tools` 协议字段。
//!
//! **插件贡献 prompt hint**：manifest 的 `tools[].hint` 字段自动拼入工具元数据。

use crate::ai::skill::{SkillEntry, SkillSummary};
use crate::schema::ToolSchema;
use std::collections::HashMap;

/// system prompt token 告警阈值（§3.8：超 5000 token warn）。
const TOKEN_WARN_THRESHOLD: usize = 5000;

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

/// 工具来源摘要（0.21.22）——喂给 chat preamble 的「工具来源」段。
///
/// 内置能力数量 + 各在线 MCP server 的工具名清单。
/// 由 `McpClientManager::tool_source_summary()` 产出，
/// `chat_system_prompt_with_skills` 消费。
#[derive(Debug, Clone, Default)]
pub struct ToolSourceSummary {
    /// 内置能力（Capability）工具数量
    pub builtin_count: usize,
    /// 各在线 MCP server 名 → 工具名清单（已过滤 disabled）
    pub mcp_servers: Vec<(String, Vec<String>)>,
}

impl ToolSourceSummary {
    /// 是否为空——内置和 MCP 都没有工具
    pub fn is_empty(&self) -> bool {
        self.builtin_count == 0 && self.mcp_servers.is_empty()
    }
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

// ── token 估算（0.21.17 收敛到 `token_budget`）──────────────────────────────────

// 0.21.17: `estimate_tokens` 和 `is_cjk` 已收敛到 `token_budget` 模块。
// 此处重导出保持 `crate::ai::prompt::estimate_tokens` 调用路径兼容。
pub use crate::ai::token_budget::estimate_text_tokens as estimate_tokens;

// ── system prompt 构建 ──────────────────────────────────────────────────────────

/// 对话标题生成的 system prompt（0.12.5 §5.3；0.27.2 从 app/commands/ai/conversations.rs
/// 迁入，保持纯函数与原输出语义——生成 6-10 字语义化标题，只输出标题文本）。
///
/// 消费方：`generate_conversation_title`。输入截断（500 字）与输出清理由调用方负责。
pub fn conversation_title_system_prompt() -> &'static str {
    "请用 6-10 个字概括以下用户消息，作为对话标题。只输出标题文本，不要加引号或标点符号。"
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
    tool_sources: &ToolSourceSummary,
) -> String {
    let mut prompt = chat_system_prompt_with_group(group_prompt);

    // 0.21.22: 「工具来源」段——仅 extensions 模式注入
    if !tool_sources.is_empty() {
        prompt.push_str("\n\n【工具来源】\n");
        if tool_sources.builtin_count > 0 {
            prompt.push_str(&format!("- 内置能力：{} 个\n", tool_sources.builtin_count));
        }
        for (server_name, tool_names) in &tool_sources.mcp_servers {
            prompt.push_str(&format!(
                "- MCP server \"{}\"：{}\n",
                server_name,
                tool_names.join(", ")
            ));
        }
    }

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

// ── 单测 ───────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ai::skill::SkillSource;
    use serde_json::json;

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

    // ── conversation_title_system_prompt（0.27.2 迁入） ──

    #[test]
    fn title_prompt_keeps_original_semantics() {
        // 迁移钉住：与 0.12.5 app 层内联原文逐字节一致，输出语义不变
        assert_eq!(
            conversation_title_system_prompt(),
            "请用 6-10 个字概括以下用户消息，作为对话标题。只输出标题文本，不要加引号或标点符号。"
        );
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

    // ── 字符级截断 helper 已随死 prompt 链删除（0.27.2）；token 预算截断真源在
    //    ai/token_budget.rs::utf8_safe_token_prefix ──

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
        let prompt =
            chat_system_prompt_with_skills(None, &summaries, &[], &ToolSourceSummary::default());
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
        let prompt = chat_system_prompt_with_skills(
            None,
            &summaries,
            &triggered,
            &ToolSourceSummary::default(),
        );
        assert!(prompt.contains("已激活技能详情"));
        assert!(prompt.contains("Rust Debug Workflow"));
        assert!(prompt.contains("Read error"));
    }

    #[test]
    fn chat_prompt_with_skills_empty_equals_group_prompt() {
        let prompt = chat_system_prompt_with_skills(None, &[], &[], &ToolSourceSummary::default());
        assert_eq!(prompt, chat_system_prompt());
    }

    #[test]
    fn chat_prompt_with_skills_preserves_group_prompt() {
        let prompt = chat_system_prompt_with_skills(
            Some("你是翻译助手"),
            &[],
            &[],
            &ToolSourceSummary::default(),
        );
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
        let prompt = chat_system_prompt_with_skills(
            Some("你是助手"),
            &summaries,
            &[],
            &ToolSourceSummary::default(),
        );
        assert!(prompt.contains("分组指令"), "分组指令应保留");
        assert!(prompt.contains("可用技能"), "技能摘要应追加");
    }

    // ── T2: tool_source preamble 格式测试（0.21.22）──

    #[test]
    fn chat_prompt_with_tool_sources_includes_mcp_section() {
        let tool_sources = ToolSourceSummary {
            builtin_count: 3,
            mcp_servers: vec![
                (
                    "github".to_string(),
                    vec!["create_issue".to_string(), "list_prs".to_string()],
                ),
                ("filesystem".to_string(), vec!["read_file".to_string()]),
            ],
        };
        let prompt = chat_system_prompt_with_skills(None, &[], &[], &tool_sources);
        assert!(prompt.contains("【工具来源】"), "应有工具来源段");
        assert!(prompt.contains("内置能力：3 个"), "应有内置能力数量");
        assert!(
            prompt.contains("MCP server \"github\""),
            "应有 github server 名"
        );
        assert!(
            prompt.contains("create_issue, list_prs"),
            "应有 github 工具名清单"
        );
        assert!(
            prompt.contains("MCP server \"filesystem\""),
            "应有 filesystem server 名"
        );
        assert!(prompt.contains("read_file"), "应有 filesystem 工具名");
    }

    #[test]
    fn chat_prompt_with_tool_sources_only_builtin() {
        let tool_sources = ToolSourceSummary {
            builtin_count: 5,
            mcp_servers: vec![],
        };
        let prompt = chat_system_prompt_with_skills(None, &[], &[], &tool_sources);
        assert!(prompt.contains("【工具来源】"), "应有工具来源段");
        assert!(prompt.contains("内置能力：5 个"), "应有内置能力数量");
        assert!(
            !prompt.contains("MCP server"),
            "无 MCP server 不应输出 MCP 行"
        );
    }

    #[test]
    fn chat_prompt_with_empty_tool_sources_omits_section() {
        let tool_sources = ToolSourceSummary::default();
        let prompt = chat_system_prompt_with_skills(None, &[], &[], &tool_sources);
        assert!(!prompt.contains("【工具来源】"), "空工具来源不应输出段落");
    }
}
