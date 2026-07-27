//! MCP 模块——Blink 与 MCP 协议的双向交互。
//!
//! ## 架构
//!
//! - `config` — MCP client 配置（name / command / args / env / enabled / tool 可见性）
//! - `client` — MCP client 编排：拉起外部 server 子进程 + 握手 + 拉 tool 列表
//! - `server_config` — MCP server 配置（总开关 + 暴露能力清单）
//! - `server` — MCP server 编排：暴露 Blink 能力给外部 client（正向投影 + 授权 + 审计）
//! - `projection` — 正向投影（CapabilitySchema → rmcp::model::Tool）
//!
//! ## 双向对称
//!
//! - MCP **client**（0.13.0）：Blink 消费外部 tool（让 Blink 的 AI 更强）
//! - MCP **server**（0.13.4）：Blink 暴露自身能力（让外部 AI 能用 Blink）
//!
//! 两者共用 rmcp 投影基础设施，是一对对称的开放能力。

pub mod client;
pub mod config;
pub mod import;
pub mod projection;
pub mod server;
pub mod server_config;

pub use client::{McpClientManager, McpServerStatus, McpToolInfo};
pub use config::{McpServerConfig, McpServerConfigStore};
#[allow(unused_imports)]
pub use config::McpTransport;
pub use import::{McpImportSource, ImportResult};
#[allow(unused_imports)]
pub use server::{BlinkMcpServer, run_stdio_server};
pub use server_config::{McpServerModeConfig, McpServerModeConfigStore};
