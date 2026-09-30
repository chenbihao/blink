//! 本地引擎领域协议 re-export shim（0.25.2 crate 化）。
//!
//! 实体已拆至 workspace crate `blink-domain-local-engine`（零出边汇点：
//! 仅依赖 infra；身份/错误类型定义在 blink-infra）。本模块保持
//! `crate::domain::local_engine::*` 旧路径可用。

pub use blink_domain_local_engine::*;
pub use blink_domain_local_engine::{adapter, descriptor, implementation, model, operation, status};
