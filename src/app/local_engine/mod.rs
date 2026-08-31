//! 本地引擎应用层服务。
//!
//! `app/local_engine` 的**唯一业务真相是 `EngineManager`**：
//! 读取各引擎配置、串行化状态变更（进程级 `EngineOperationCoordinator`）、
//! 调用 adapter + infra、持有运行实例与 launch snapshot、广播事件、
//! 管理模型资产生命周期，并在退出时回收所有受管进程。
//!
//! ## 分层归属
//!
//! - `app/local_engine`：`EngineManager`（生命周期/状态/业务校验）+
//!   各引擎 adapter 实现 + 事件出口 + DTO 投影。
//! - `domain/local_engine`：稳定 id、声明、状态类型、操作结果、错误分类、
//!   生命周期策略和引擎特有的启动/健康适配接口；不依赖 infra、不发送 Tauri 事件。
//! - `infra/local_engine`：部署事务（slot/journal）、模型资产事务、
//!   受管子进程、端口探测；不依赖 app/domain。
//!
//! ## 事件投影
//!
//! `TauriEventPort` 实现 `EventPort` trait，把通用 status/log 事件 emit 为
//! `blink://local-engine-status` / `blink://local-engine-log`，
//! 并做旧 FunASR 兼容投影。

pub mod config_source; // AdapterConfig 配置真源（唯一入口，去重 commands/maintenance/service）
pub mod dto;
pub mod error_bridge;
pub mod event_port;
pub mod funasr;
pub mod manager; // EngineManager（生命周期/状态/模型/存储用例拆分为子模块）
pub mod model_installer; // 模型安装执行器 + 目录 + DTO（无状态；编排归 EngineManager）
pub mod ocr_coordinator; // OCR Coordinator（路由 + 生命周期 + 并发）
pub mod operation_coordinator; // 应用运行时 mutation claim + cancellation
pub mod operation_log_store; // 会话内 operation 日志回放
pub mod paddleocr; // PaddleOCR adapter
pub mod registry;

#[allow(unused_imports)]
pub use event_port::{TauriEventPort, make_event_port};
#[cfg(test)]
pub use manager::NoopEventPort;
#[allow(unused_imports)]
pub use manager::{EngineManager, EventPort, StructuredLogEntry};
#[allow(unused_imports)]
pub use operation_log_store::OperationLogStore;
#[allow(unused_imports)]
pub use registry::{EngineRegistry, RegistryEntry, RegistryLookup};
