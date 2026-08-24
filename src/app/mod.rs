//! 应用层：命令编排、配置管理、服务生命周期

pub mod ai_config;
pub mod command_error; // 0.14.7 W3：IPC 边界的结构化错误协议
pub mod commands;
pub mod config;
pub mod domain_env; // 0.14.6 §2.2：TauriDomainEnv——DomainEnv trait 的 Tauri 实现
pub mod local_engine; // 0.22.3：本地引擎生命周期编排服务（LocalEngineService + EngineRegistry）
pub mod mcp_server_runtime; // 0.19.13：主进程 Streamable HTTP MCP Server 生命周期管理
pub mod service;
pub mod setting_service;
pub mod stt_config;
pub mod tray; // 系统托盘菜单构建 + 文案 i18n（运行时热切换）
pub mod voice; // 0.10：语音管线编排(hold→录音→STT→注入)
pub mod window_orchestrator; // 0.21.14：窗口业务编排（从 infra 上移）

// 0.21.14：窗口事件回调类型——infra 层通过 Tauri state 消费，不反向依赖 domain。
pub use window_orchestrator::{ChatCloseCallback, StickySpareCloseCallback, WelcomeCloseCallback};
