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

/// /tools 工具快照单条（0.27.7 斜杠命令，phase §5.7.4）。
///
/// 复用 Agent 实际工具组装规则（模式 / allowlist / 运行时可用性 / MCP 开关），
/// 只读、无副作用——不因查看主动连接。
#[derive(Clone, Debug, Serialize)]
pub struct SlashToolEntry {
    pub name: String,
    pub description: String,
    /// `builtin` 或 `mcp:<server_name>`。
    pub source: String,
}

/// /mcp 单 server 连接状态（0.27.7，phase §5.7.4）。
///
/// `status`: `online` / `connecting` / `offline` / `disabled`（配置关闭）。
#[derive(Clone, Debug, Serialize)]
pub struct SlashMcpServer {
    pub name: String,
    pub transport: String,
    pub status: String,
    pub tool_count: usize,
    /// 非 disabled 的 tool 名（离线 server 为空）。
    pub tool_names: Vec<String>,
}

/// 斜杠命令动态内容快照（0.27.7，/tools /mcp /skill 数据源，一次 IPC 聚合）。
#[derive(Clone, Debug, Serialize)]
pub struct SlashCommandSnapshot {
    /// `full` / `pure_chat`。
    pub mode: String,
    pub tools: Vec<SlashToolEntry>,
    /// 工具池为空的原因：`pure_chat`（纯对话模式未启用工具）或 None（确实无工具）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools_empty_reason: Option<String>,
    /// 工具池口径：`live`（缓存 Agent 真实池）/ `estimate`（allowlist 估算兜底）。
    pub tools_source: String,
    pub mcp_servers: Vec<SlashMcpServer>,
    /// Skill 功能开关 + 已扫描数量（可用性判定由前端组合）。
    pub skills_enabled: bool,
    pub skill_count: usize,
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
