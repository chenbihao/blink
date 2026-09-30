//! FeatureCatalog —— 面向用户的功能目录聚合层（0.21.4）。
//!
//! 把 builtin descriptor、Chord binding、builtin/plugin Capability 聚合为
//! 统一的目录项，供设置页"能力管理"子页展示三出口状态（本地/AI/MCP）。
//!
//! **设计原则**（§3.6 / §5.5）：
//! - Feature / Capability / Binding 三类 id 分栏，不假设同名。
//! - capability schema、danger、sensitive、policy 只投影，不复制存储。
//! - 本地状态从各 binding store 聚合；批量操作通过 adapter 写回原真源。
//! - 未知/已移除 id 保留为可诊断残留，不阻断目录初始化。
//! - IPC 契约：只读 `list_feature_catalog()`；批量写 `apply_binding_batch(ops)`。
//!   目录刷新由前端订阅 `blink://config-changed`，不新增专用事件。

mod aggregator;
mod binding_adapter;
mod types;

pub use aggregator::FeatureCatalogAggregator;
pub use binding_adapter::apply_binding_batch;
pub use types::*;
