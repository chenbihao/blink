//! 应用层：命令编排、配置管理、服务生命周期

pub mod ai_config;
pub mod audio_transcription_service; // 0.22.16 Handoff 04：一次性 STT 编排
pub mod capture_orchestrator; // 0.22.14：AI 净化截图原子事务 guard
pub mod command_error; // 0.14.7 W3：IPC 边界的结构化错误协议
pub mod commands;
pub mod config;
pub mod domain_env; // 0.14.6 §2.2：TauriDomainEnv——DomainEnv trait 的 Tauri 实现
pub mod editor; // 0.23.1：单 EditorSession 服务（会话身份/窗口绑定/保存分派）
pub mod editor_draft; // 修复：编辑器恢复草稿存储（原子持久化 + 单调 revision 水位）
pub mod editor_transform; // 0.23.4：编辑器 AI 整理编排（全局单活跃/只产候选）
pub mod local_engine; // 0.22.3：本地引擎生命周期编排服务（EngineManager + EngineRegistry）
pub mod mcp_server_runtime; // 0.19.13：主进程 Streamable HTTP MCP Server 生命周期管理
pub mod service;
pub mod setting_service;
pub mod stt_config;
pub mod tray; // 系统托盘菜单构建 + 文案 i18n（运行时热切换）
pub mod voice; // 0.10：语音管线编排(hold→录音→STT→注入)
pub mod window_orchestrator; // 0.21.14：窗口业务编排（从 infra 上移）

// 0.21.14：窗口事件回调类型——infra 层通过 Tauri state 消费，不反向依赖 domain。
pub use window_orchestrator::{ChatCloseCallback, StickySpareCloseCallback, WelcomeCloseCallback};
