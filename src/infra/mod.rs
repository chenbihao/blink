//! 基础设施层 re-export shim（0.25.1 crate 化）。
//!
//! 实体已拆至 workspace crate `blink-infra`（`crates/blink-infra`）——编译记账
//! 单位独立后，infra 改动不再触发 bin 的整 crate 重编。本模块保持
//! `crate::infra::*` 旧路径可用，bin/domain 侧 330+ 处引用零改写。

pub use blink_infra::*;
