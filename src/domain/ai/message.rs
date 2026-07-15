//! AI 消息类型——**独立于 rig-core**,顶层抽象不泄漏 vendor 类型。
//!
//! **为什么不直接用 `rig::completion::Message`**：
//! - rig 每月 breaking——把它锁死在 `domain::ai` 内部适配层里,业务代码
//!   `use crate::domain::ai::ChatMessage` 就够,rig 破 API 我们改一层
//! - `ToolCall.arguments` 用 `serde_json::Value` 而不是 rig 的 wrapper——
//!   直接喂 `ActionContext::from_arguments()`,零适配
//!
//! **0.9.1 阶段规模**:主窗口打字模式,消息历史通常 1-3 条(system + user 或
//! user + tool-result)。没上多轮对话,不做 message trimming / summarization。

use serde::{Deserialize, Serialize};

use crate::domain::execution::ActionSchema;

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
}

impl ChatMessage {
    #[allow(dead_code)]
    pub fn system(content: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: content.into(),
            tool_call_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn user(content: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: content.into(),
            tool_call_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn assistant(content: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: content.into(),
            tool_call_id: None,
        }
    }

    #[allow(dead_code)]
    pub fn tool(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: content.into(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Provider 返回的工具调用——直接喂给 `ActionContext::from_arguments()`。
///
/// `arguments` 是 `serde_json::Value` 而不是 struct——因为不同 tool 的参数结构
/// 不同,由 `Action::from_arguments` 各自解析。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct ToolCall {
    /// Provider 生成的调用 id(用于 Tool 消息关联)
    pub id: String,
    /// 对应的 `Action::id()`——直接查 `ActionRegistry`
    pub name: String,
    /// JSON Object,直接 `ActionContext { arguments: this.into() }`
    pub arguments: serde_json::Value,
}

/// Token usage(供应商返回时填充,骨架层不承诺准确性)。
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq)]
#[allow(dead_code)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

/// AI 调用请求。
///
/// **`tools` 用我们自己的 `ActionSchema`**——不是 `rig::ToolDefinition`。
/// Provider 内部通过 `ActionSchema::to_rig_tool()` 投影(唯一 rig 触点)。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct CompletionRequest {
    pub messages: Vec<ChatMessage>,
    /// tool 描述——空 vec 表示"无 tool 调用需求,只做文本 completion"
    pub tools: Vec<ActionSchema>,
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
    }

    #[test]
    fn tool_call_arguments_directly_json_value() {
        // ActionContext::from_arguments 消费 serde_json::Value——TypeCall 提供的
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
    }
}
