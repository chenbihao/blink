//! Capability 能力协议层——错误类型（0.25.1 自 bin crate 拆出）。
//!
//! `CapabilityError` 是全部能力/域层的共享错误词汇（51+ 文件引用），下沉为
//! 独立 crate 后，stt 等域 crate 可依赖它而不牵动协议主体（协议主体
//! `registry/policy/result/...` 与域服务互相缠绕，随 0.25.4 剩余 domain
//! 整体成 crate）。bin 侧 `src/domain/capability/mod.rs` re-export 保持旧路径。
//!
//! 本 crate 铁则：只依赖 serde，不依赖 tauri/windows/infra——错误词汇是
//! 最底层协议。

pub mod error;

pub use error::CapabilityError;
