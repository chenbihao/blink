//! 配置 re-export 垫片（0.14.6 §2.1 过渡）。
//!
//! 所有配置类型 + 操作函数已迁入 `domain::config`。
//! 此文件保留 re-export 以避免 app 层大量 import 改动。
//! 后续可逐步将 app 层 import 直接指向 `domain::config`，最终删除此垫片。

pub use crate::domain::config::app_config::*;
pub use crate::domain::config::plugin_config::*;
pub use crate::domain::config::shards::*;
pub use crate::domain::config::store::*;
