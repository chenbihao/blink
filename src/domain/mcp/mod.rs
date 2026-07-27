//! MCP client 模块（0.13.0）——消费外部 MCP server 提供的 tool。
//!
//! Blink 作为 MCP client，拉起外部 server 子进程（stdio JSON-RPC），握手后拉 tool 列表，
//! 包装成 `rig_core::tool::rmcp::McpTool`（已 impl `ToolDyn`）进对话窗口 tool 池。
//!
//! ## 架构
//!
//! - `config` — server 配置（name / command / args / env / enabled / tool 可见性），存配置库
//! - `client` — client 编排：拉起子进程 + 握手 + 拉 tool + 故障降级 + 手动重连
//!
//! ## 与 rig 的关系
//!
//! rig-core 已内置 `McpTool`（impl `ToolDyn`）和 `From<rmcp::model::Tool> for ToolDefinition`
//! 投影，本模块**不重复造轮子**——直接用 rig 的 McpTool，只负责编排和配置管理。
//!
//! ## tool 可见性控制
//!
//! 用户可在设置页勾选/取消具体 tool，控制喂给 AI 的 tool 子集。
//! `disabled_tools` 记录被用户取消的 tool 名称，`collect_tools()` 时过滤。

pub mod client;
pub mod config;

pub use client::{McpClientManager, McpServerStatus, McpToolInfo};
pub use config::{McpServerConfig, McpServerConfigStore};
