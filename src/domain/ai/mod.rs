//! AI 领域层（0.9.1 起）——Provider 抽象 + 意图路由骨架。
//!
//! **0.9.1 阶段边界**：
//! - `spike/` —— AI 抗延迟骨架验证测试（`#[cfg(test)]` 门槛，release 零残留）
//! - `provider.rs` —— `AIProvider` trait（Phase 4 落地，§2.6 类型收窄）
//! - `message.rs`  —— `ChatMessage` / `Role` / `ToolCall` / `CompletionRequest / Response`
//! - `registry.rs` —— `AIProviderRegistry` + `ProviderFactory`（Phase 5a 落地，
//!   Provider 池 + 三档 dispatch + 切换零重启）
//!
//! **rig 隔离墙**：`domain::ai` 是唯一 import `rig_core` 的领域模块（除
//! `domain::execution::schema.rs::to_rig_tool`）；上层只 `use domain::ai::AIProvider`,
//! 拿不到 rig 类型。
//!
//! **§2.6 类型收窄**：主窗口 `use crate::domain::ai::AIProvider` 编译期就没有
//! `AgentBuilder / prompt / memory`——这些留 0.10 落 `agent_window/` 独立模块。

pub mod agent_provider;
pub mod chat_service;
pub mod cli_recognizer;
pub mod factory;
pub mod gating;
pub mod memory;
pub mod message;
pub mod prompt;
pub mod provider;
pub mod registry;
pub mod rig_provider;
pub mod skill;
pub mod spike;
pub mod tool_adapter;

#[allow(unused_imports)] // 0.9.1 Phase 5 起被 AppContext 消费
pub use factory::default_factory;
#[allow(unused_imports)]
pub use message::{ChatMessage, CompletionRequest, CompletionResponse, Role, ToolCall, Usage};
#[allow(unused_imports)]
pub use provider::{AIError, AIProvider, StreamChunk};
#[allow(unused_imports)]
pub use registry::{AIProviderRegistry, ProviderFactory, ResolvedProviderEntries};
