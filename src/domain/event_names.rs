//! 事件名常量清单（0.21.14 已下沉到 `infra/event_names.rs`）。
//!
//! 0.21.14：为消除 `infra` 对 `domain` 的反向依赖，`EventNames` 定义已移至
//! `infra/event_names.rs`。本文件保留重导出，使 domain 子模块现有的
//! `use crate::domain::event_names::EventNames` 路径不需修改。
//!
//! domain 引用 infra 是向下依赖，符合 spec-architecture §A1。

pub use crate::infra::event_names::EventNames;
