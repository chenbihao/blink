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
    // ── 中：内置工具 ──
    pub builtin_tools: Vec<BuiltinToolSummary>,
    // ── 下：MCP 服务 ──
    pub mcp_servers: Vec<McpServerSummary>,
    // ── 汇总 ──
    pub builtin_count: usize,
    pub mcp_count: usize,
    pub total_count: usize,
}
