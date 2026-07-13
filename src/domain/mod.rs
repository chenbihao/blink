//! 领域层：搜索、意图、插件核心逻辑

pub mod ai;
pub mod capability; // 0.9.7：能力协议层（原子能力 + 统一声明/返回）
pub mod chord;
pub mod context;
pub mod execution;
pub mod intent;
pub mod plugin;
pub mod search;
