//! 共享类型：跨域使用的 struct 定义在此，各子模块通过 super::shared:: 引用。

use serde::Serialize;

/// Composer bar 悬浮预览中单个内置工具的摘要。
#[derive(Clone, Debug, Serialize)]
pub struct BuiltinToolSummary {
    pub name: String,
    pub description: String,
}

/// Composer bar 悬浮预览中单个 MCP server 的摘要。
#[derive(Clone, Debug, Serialize)]
pub struct McpServerSummary {
    pub name: String,
    pub transport: String,
    pub online: bool,
    pub tool_count: usize,
    /// 该 server 提供的 tool 名称列表（非 disabled 的）。
    pub tool_names: Vec<String>,
}

/// Composer bar 悬浮预览快照——一次 IPC 聚合所有 popup 数据。
#[derive(Clone, Debug, Serialize)]
pub struct ComposerBarSnapshot {
    // ── 上：上下文容量 ──
    pub estimated_tokens: usize,
    pub context_limit: usize,
    pub usage_percent: u8,
    pub last_compressed: bool,
    pub last_compressed_count: usize,
    pub last_recall_count: usize,
    /// 系统提示词（preamble）估算 token 数。
    pub preamble_tokens: usize,
    /// 当前待发消息估算 token 数。
    pub pending_message_tokens: usize,
    // ── 0.21.17 扩展：token 预算分项 ──
    /// 历史消息估算 token 数。
    #[serde(default)]
    pub history_tokens: usize,
    /// 工具定义估算 token 数。
    #[serde(default)]
    pub tools_tokens: usize,
    /// 协议开销 token 数。
    #[serde(default)]
    pub protocol_overhead_tokens: usize,
    /// 多模态内容保守估算 token 数。
    #[serde(default)]
    pub multimodal_tokens: usize,
    /// 输出预留 token 数。
    #[serde(default)]
    pub reserved_output_tokens: usize,
    /// 安全余量 token 数。
    #[serde(default)]
    pub safety_margin_tokens: usize,
    /// 有效输入上限（context_limit - reserved_output - safety_margin）。
    #[serde(default)]
    pub effective_input_limit: usize,
    /// 安全剩余 token 数。
    #[serde(default)]
    pub remaining_tokens: usize,
    /// context limit 来源（"configured" / "fallback"）。
    #[serde(default)]
    pub context_limit_source: String,
    /// 估算置信度（"high" / "medium" / "low"）。
    #[serde(default)]
    pub confidence: String,
    // ── 中：内置工具 ──
    pub builtin_tools: Vec<BuiltinToolSummary>,
    // ── 下：MCP 服务 ──
    pub mcp_servers: Vec<McpServerSummary>,
    // ── 0.21.23: 记忆健康度一览（压缩策略 / 摘要段 / 最近一次摘要）──
    #[serde(default)]
    pub memory: crate::domain::ai::chat_service::MemoryHealthSummary,
    // ── 汇总 ──
    pub builtin_count: usize,
    pub mcp_count: usize,
    pub total_count: usize,
}
