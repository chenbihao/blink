//! 应用层：命令编排、配置管理、服务生命周期

pub mod ai_config;
pub mod commands;
pub mod config;
pub mod service;
pub mod stt_config;
pub mod tray; // 系统托盘菜单构建 + 文案 i18n（运行时热切换）
pub mod voice; // 0.10：语音管线编排(hold→录音→STT→注入)
