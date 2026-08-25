//! 配置域（0.14.6 §2.1）——所有配置分片 struct + ConfigKey trait + ConfigStore 统一归此域。
//!
//! **架构对齐**：原 `app/config.rs` / `app/ai_config.rs` / `app/stt_config.rs` 的类型定义
//! 和操作函数迁入此域。app / infra / domain 都依赖此域，依赖方向正确（不再反向 use app）。
//!
//! 模块结构：
//! - `store` — ConfigKey trait + ConfigStore 泛型存取 + 外部类型的 ConfigKey impl
//! - `shards` — AppConfig 6 分片 + 引擎配置 + ContextConfig + ScreenshotConfig
//! - `app_config` — AppConfig 门面 + init/get/save/update 操作函数
//! - `plugin_config` — PluginConfig + 插件配置 CRUD
//! - `ai_config` — AIConfig 第 7 分片 + Provider/Model 类型 + 缓存
//! - `ai_capability_access` — AiCapabilityAccessConfig AI 出口授权分片（0.21.5）
//! - `stt_config` — SttConfig 第 8 分片 + STT 引擎类型 + 缓存

pub mod ai_capability_access;
pub mod ai_config;
pub mod app_config;
pub mod managed_settings;
pub mod ocr_config; // 0.22.4：OCR 配置分片（第 9 KV）
pub mod plugin_config;
pub mod shards;
pub mod store;
pub mod stt_config;

// ── 扁平 re-exports（方便 `crate::domain::config::*` 直接引用）─────────────────
#[allow(unused_imports)]
pub use ai_capability_access::*;
#[allow(unused_imports)]
pub use ai_config::*;
#[allow(unused_imports)]
pub use app_config::*;
pub use managed_settings::*;
#[allow(unused_imports)]
pub use ocr_config::*;
pub use plugin_config::*;
pub use shards::*;
#[allow(unused_imports)]
pub use store::*;
