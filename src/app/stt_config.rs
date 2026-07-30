//! STT 配置 re-export 垫片（0.14.6 §2.1 过渡）。
//!
//! 所有 STT 配置类型已迁入 `domain::config::stt_config`。
//! 此文件保留 re-export 以避免 app 层大量 import 改动。

pub use crate::domain::config::stt_config::*;
