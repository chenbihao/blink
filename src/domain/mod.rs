//! 领域层：搜索、意图、插件核心逻辑

pub mod ai;
pub mod capability; // 0.9.7：能力协议层（原子能力 + 统一声明/返回）
pub mod chord;
pub mod clipboard; // 0.19.6：剪贴板读写共享语义（command / Capability 共用）
pub mod color; // 0.20.3：确定性颜色字面量解析（纯函数，Rust/JS 共享 fixture）
pub mod palette; // 0.20.7：配色核心（OKLab/OKLCH/聚类/角色/搭配/对比度，Rust 单一真源）
pub mod config; // 0.14.6 §2.1：配置域（从 app/ 下沉）
pub mod context;
pub mod event; // 0.14.6 §2.2：领域环境抽象（DomainEnv trait，domain 去 tauri）
pub mod event_names; // 0.14.6 §3.3：blink:// 事件名常量（domain 层，避免反向依赖）
pub mod execution;
pub mod feature_catalog; // 0.21.4：功能目录聚合层
pub mod intent;
pub mod mcp; // 0.13.0：MCP client（消费外部 tool，包装进 Tool 适配层）
pub mod plugin;
pub mod schema; // 0.14.6：ToolSchema 公共基（ActionSchema / CapabilitySchema 共享）
pub mod search;
pub mod sticky;
pub mod stt; // 0.10：语音转文字（STT engine trait + mock + 模型注册表） // 0.16.7：桌面便签域（模型、服务、恢复）
