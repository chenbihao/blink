//! Blink 配置域 crate（0.25.2 自 bin crate 拆出）。
//!
//! 首批迁入 STT 配置（`stt_config`）与配置分片存取协议（`store`——
//! ConfigKey trait + ConfigStore）——stt 域 crate（0.25.3）的依赖基座。
//! 其余配置文件（app_config/ai_config/...）仍在 bin，随 0.25.4 剩余
//! domain 整体成 crate；bin 侧 `src/domain/config/mod.rs` re-export
//! 保持旧路径。

pub mod stt_config;
pub mod store;
