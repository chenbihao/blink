//! 领域层：搜索、意图、插件核心逻辑

pub mod ai;
pub mod capability; // 0.9.7：能力协议层（原子能力 + 统一声明/返回）
pub mod chord;
pub mod context;
pub mod execution;
pub mod intent;
pub mod mcp; // 0.13.0：MCP client（消费外部 tool，包装进 Tool 适配层）
pub mod plugin;
pub mod search;
pub mod stt; // 0.10：语音转文字（STT engine trait + mock + 模型注册表）
