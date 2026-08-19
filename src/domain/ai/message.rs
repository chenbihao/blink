//! AI 消息类型——**独立于 rig-core**,顶层抽象不泄漏 vendor 类型。
//!
//! **为什么不直接用 `rig::completion::Message`**：
//! - rig 每月 breaking——把它锁死在 `domain::ai` 内部适配层里,业务代码
//!   `use crate::domain::ai::ChatMessage` 就够,rig 破 API 我们改一层
//! - `ToolCall.arguments` 用 `serde_json::Value` 而不是 rig 的 wrapper——
//!   直接喂 Capability invoke 的 args，零适配
//!
//! **0.9.1 阶段规模**:主窗口打字模式,消息历史通常 1-3 条(system + user 或
//! user + tool-result)。没上多轮对话,不做 message trimming / summarization。

use serde::{Deserialize, Serialize};

use crate::domain::schema::ToolSchema;

/// 会话消息角色。
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
#[allow(dead_code)] // 0.9.1 Phase 4 定义,Phase 5 起 AIProvider dispatch 消费
pub enum Role {
    /// 系统提示——路由模型的意图分类 prompt 一般是这个
    System,
    /// 用户输入
    User,
    /// 助手回复(工具调用前的中间态)
    Assistant,
    /// 工具执行结果(0.10 Agent 窗口多轮才用到)
    Tool,
}

/// 一条会话消息——统一 provider 抽象的最小消息类型。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[allow(dead_code)]
pub struct ChatMessage {
    pub role: Role,
    pub content: String,
    /// Tool 消息专用——关联到哪个 `ToolCall.id`。其他角色为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Tool 消息专用——产生此结果的工具名（对应 `ToolCall.name`）。
    /// rig 0.42 `Message::tool_result` 需要 name 参数，不可用空字符串占位。
    /// 其他角色为 None。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_name: Option<String>,
}

impl ChatMessage {
    #[allow(dead_code)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    #[allow(dead_code)]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    #[allow(dead_code)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
            tool_name: None,
        }
    }

    /// 带 tool_call_id 的 assistant 消息（0.11.4 Turn 2 回流用）。
    ///
    /// `content` 是 `{"name":"...","arguments":{...}}` JSON 字符串——
    /// `RigProvider::build_rig_request` 会解析它构造 rig `AssistantContent::ToolCall`。
    /// `tool_call_id` 是 Turn 1 AI 返回的 tool call ID，与后续 `ChatMessage::tool` 的 id 对齐。
    #[allow(dead_code)] // 0.11.4 Turn 2 回流用，rig_provider 测试消费
    pub fn assistant_tool_call(
        tool_call_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: None,
        }
    }

    #[allow(dead_code)]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: None,
        }
    }

    /// 带 tool_call_id 和 tool_name 的 Tool 消息（0.42: rig 需要真实工具名）。
    ///
    /// `tool_call_id` 是 Turn 1 AI 返回的 tool call ID。
    /// `tool_name` 是对应的 `ToolCall.name`（工具的真实名称）。
    #[allow(dead_code)]
    pub fn tool_with_name(
        tool_call_id: impl Into<String>,
        tool_name: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
            tool_name: Some(tool_name.into()),
        }
    }
}

/// Provider 返回的工具调用——参数交给对应 Capability 解析。
///
/// `arguments` 是 `serde_json::Value` 而不是 struct——因为不同 tool 的参数结构
/// 不同，由 `Capability::invoke` 各自解析。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct ToolCall {
    /// Provider 生成的调用 id(用于 Tool 消息关联)
    pub id: String,
    /// 对应的 `Capability::id()`——只查 `CapabilityRegistry`
    pub name: String,
    /// JSON Object，直接作为 `Capability::invoke` 的 args。
    pub arguments: serde_json::Value,
}

/// Token usage（供应商返回时填充，骨架层不承诺准确性）。
///
/// **0.21.17: 全仓唯一生产 Usage 类型**——`token_budget::FullUsage` 已删除，
/// `ChatStreamChunk::Done`、`UsageCalibrator`、`CompletionResponse` 全部使用此类型。
///
/// 完整保留 Rig 0.42 的七个 usage 字段 + `reported` 标记。
/// 零值是"供应商没有报告 usage"的约定哨兵，不能直接解释为真实零消耗。
/// `reported` 字段区分"未报告"和"真实零"，使用 Rig `Usage::has_values()` 语义。
///
/// 旧字段 `input_tokens` / `output_tokens` 保持不变，新字段使用 `#[serde(default)]`
/// 确保旧前端消费端不受影响。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
    /// 总 token 数（input + output + cache 等）。
    #[serde(default)]
    pub total_tokens: u32,
    /// 缓存命中的输入 token 数（prompt caching）。
    #[serde(default)]
    pub cached_input_tokens: u32,
    /// 缓存创建写入的 token 数（prompt caching 首次写入成本）。
    #[serde(default)]
    pub cache_creation_input_tokens: u32,
    /// 工具定义 prompt token 数。
    #[serde(default)]
    pub tool_use_prompt_tokens: u32,
    /// 推理（thinking/reasoning）token 数。
    #[serde(default)]
    pub reasoning_tokens: u32,
    /// 供应商是否报告了 usage。`false` = 未报告（零值是哨兵，不是真实零消耗）。
    /// 使用 Rig `Usage::has_values()` 语义：Rig Usage 全零时 `reported = false`。
    #[serde(default)]
    pub reported: bool,
}

impl Usage {
    /// 从 Rig `Usage` 构造——**全仓唯一 from_rig_usage 映射**。
    ///
    /// 安全饱和转换 u64 → u32。
    /// `reported` 使用 Rig `Usage::has_values()` 语义：
    /// - Rig Usage 全零（= `new()`）→ `reported = false`（供应商未报告）
    /// - Rig Usage 任一字段非零 → `reported = true`
    ///
    /// 这覆盖了 cache-only / reasoning-only / tool-use-only 等部分报告场景。
    pub fn from_rig_usage(rig_usage: &rig_core::completion::Usage) -> Self {
        Self {
            input_tokens: saturate_u64_to_u32(rig_usage.input_tokens),
            output_tokens: saturate_u64_to_u32(rig_usage.output_tokens),
            total_tokens: saturate_u64_to_u32(rig_usage.total_tokens),
            cached_input_tokens: saturate_u64_to_u32(rig_usage.cached_input_tokens),
            cache_creation_input_tokens: saturate_u64_to_u32(rig_usage.cache_creation_input_tokens),
            tool_use_prompt_tokens: saturate_u64_to_u32(rig_usage.tool_use_prompt_tokens),
            reasoning_tokens: saturate_u64_to_u32(rig_usage.reasoning_tokens),
            reported: rig_usage.has_values(),
        }
    }

    /// 未报告的 usage（所有字段为零，`reported` = false）。
    pub fn unreported() -> Self {
        Self {
            reported: false,
            ..Default::default()
        }
    }

    /// 累加另一个 usage（多轮 Agent 累加不丢字段）。
    #[allow(dead_code)]
    pub fn add(&mut self, other: &Usage) {
        self.input_tokens = self.input_tokens.saturating_add(other.input_tokens);
        self.output_tokens = self.output_tokens.saturating_add(other.output_tokens);
        self.total_tokens = self.total_tokens.saturating_add(other.total_tokens);
        self.cached_input_tokens = self
            .cached_input_tokens
            .saturating_add(other.cached_input_tokens);
        self.cache_creation_input_tokens = self
            .cache_creation_input_tokens
            .saturating_add(other.cache_creation_input_tokens);
        self.tool_use_prompt_tokens = self
            .tool_use_prompt_tokens
            .saturating_add(other.tool_use_prompt_tokens);
        self.reasoning_tokens = self.reasoning_tokens.saturating_add(other.reasoning_tokens);
        // 只要有一方报告了，就标记为已报告
        self.reported = self.reported || other.reported;
    }

    /// 是否有实质性的 token 消耗（用于区分空响应）。
    ///
    /// `reported = false` 时一律返回 `false`（未报告不等于真实零消耗，但也不等于有消耗）。
    /// `reported = true` 时任一字段 > 0 返回 `true`。
    #[allow(dead_code)]
    pub fn has_real_usage(&self) -> bool {
        self.reported
            && (self.input_tokens > 0
                || self.output_tokens > 0
                || self.total_tokens > 0
                || self.cached_input_tokens > 0
                || self.cache_creation_input_tokens > 0
                || self.tool_use_prompt_tokens > 0
                || self.reasoning_tokens > 0)
    }
}

/// 安全将 u64 饱和转换为 u32。
fn saturate_u64_to_u32(v: u64) -> u32 {
    v.min(u32::MAX as u64) as u32
}

/// AI 调用请求。
///
/// **`tools` 用我们自己的 `ToolSchema`**——不是 `rig::ToolDefinition`。
/// Provider 内部通过 `ToolSchema::to_rig_tool()` 投影(唯一 rig 触点)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    /// tool 描述——空 vec 表示"无 tool 调用需求,只做文本 completion"
    pub tools: Vec<ToolSchema>,
    /// 生成上限——None 用 provider default
    pub max_tokens: Option<u32>,
    /// 采样温度——0.0=确定,1.0=创意。路由模型建议 0.0-0.2
    pub temperature: Option<f32>,
    /// 硬超时(毫秒)——None 用 AIConfig 的 `slo_hard_timeout_ms` 或 default 20000
    pub timeout_ms: Option<u32>,
}

/// AI 调用响应。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompletionResponse {
    /// 文本响应——若 provider 只返 tool_calls 则为 None
    pub text: Option<String>,
    /// 工具调用列表——SearchService 走 SuggestionArbiter 分发
    pub tool_calls: Vec<ToolCall>,
    pub usage: Usage,
    /// §3.3 骨架 SLO——首 token 到达时刻(SSE 模式);非流式则等于 `total_ms`
    pub first_token_ms: u32,
    /// §3.3 骨架 SLO——响应完整收齐时刻
    pub total_ms: u32,
}

// ── 测试 ────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_serializes_lowercase() {
        assert_eq!(serde_json::to_string(&Role::System).unwrap(), "\"system\"");
        assert_eq!(serde_json::to_string(&Role::User).unwrap(), "\"user\"");
        assert_eq!(
            serde_json::to_string(&Role::Assistant).unwrap(),
            "\"assistant\""
        );
        assert_eq!(serde_json::to_string(&Role::Tool).unwrap(), "\"tool\"");
    }

    #[test]
    fn chat_message_constructors_set_correct_role() {
        assert_eq!(ChatMessage::system("s").role, Role::System);
        assert_eq!(ChatMessage::user("u").role, Role::User);
        assert_eq!(ChatMessage::assistant("a").role, Role::Assistant);
        let tm = ChatMessage::tool("tc_1", "result");
        assert_eq!(tm.role, Role::Tool);
        assert_eq!(tm.tool_call_id, Some("tc_1".to_string()));
        assert_eq!(tm.tool_name, None);

        let tmn = ChatMessage::tool_with_name("tc_1", "search_apps", "result");
        assert_eq!(tmn.role, Role::Tool);
        assert_eq!(tmn.tool_call_id, Some("tc_1".to_string()));
        assert_eq!(tmn.tool_name.as_deref(), Some("search_apps"));
    }

    #[test]
    fn tool_call_arguments_directly_json_value() {
        // Capability invoke 消费 serde_json::Value——ToolCall 提供的
        // arguments 类型必须能直接注入
        let tc = ToolCall {
            id: "call_123".into(),
            name: "open_url".into(),
            arguments: serde_json::json!({ "url": "https://example.com" }),
        };
        assert!(tc.arguments.is_object());
        assert_eq!(
            tc.arguments.get("url").and_then(|v| v.as_str()),
            Some("https://example.com")
        );
    }

    #[test]
    fn chat_message_tool_call_id_omitted_when_none() {
        let m = ChatMessage::user("hi");
        let s = serde_json::to_string(&m).unwrap();
        // skip_serializing_if 生效:tool_call_id 字段完全不出现
        assert!(
            !s.contains("tool_call_id"),
            "非 Tool 消息不应含 tool_call_id 字段: {s}"
        );
        // tool_name 同样不应出现
        assert!(
            !s.contains("tool_name"),
            "非 Tool 消息不应含 tool_name 字段: {s}"
        );
    }

    #[test]
    fn chat_message_tool_with_name_serializes_tool_name() {
        let m = ChatMessage::tool_with_name("tc_1", "search_apps", "result");
        let s = serde_json::to_string(&m).unwrap();
        assert!(s.contains("tool_name"), "Tool 消息应含 tool_name 字段: {s}");
        assert!(
            s.contains("search_apps"),
            "tool_name 值应出现在序列化结果中: {s}"
        );
    }
}
