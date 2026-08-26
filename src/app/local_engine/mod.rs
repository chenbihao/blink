//! 本地引擎应用层服务（0.22.3）。
//!
//! `app/local_engine` 负责读取各引擎配置、串行化状态变更、调用 adapter + infra、
//! 持有运行实例、广播事件，并在退出时回收所有受管进程。
//!
//! ## 分层归属（§3.1）
//!
//! - `app/local_engine`：读取配置、串行化状态、调用 adapter + infra、
//!   持有运行实例、广播事件，退出时回收所有受管进程。
//! - `domain/local_engine`：稳定 id、声明、状态类型、错误分类、生命周期策略
//!   和引擎特有的启动/健康适配接口；不发送 Tauri 事件。
//! - `infra/local_engine`：启动/停止子进程、排空管道、PID 身份验证、端口探测；
//!   不依赖 app/domain。
//!
//! ## 事件投影
//!
//! `TauriEventPort` 实现 `EventPort` trait，把通用 status/log 事件 emit 为
//! `blink://local-engine-status` / `blink://local-engine-log`，
//! 并做旧 FunASR 兼容投影。
//!
//! ## H4 实现者须知
//!
//! H4（FunASR adapter 实现）必须：
//! 1. 构造 `EngineDescriptor`（含 engine_id、display、capability_kind、
//!    runtime_kind、install_plan、model_contract、lifecycle、timeouts、
//!    resource_budget、cleanup）。
//! 2. 实现 `LocalEngineAdapter` trait 的全部方法：
//!    - `descriptor()` — 返回引擎描述符
//!    - `prepare_launch()` — 从已校验配置构造受限 LaunchDescriptor
//!    - `map_health()` — 把 FunASR health 响应映射为 HealthMapping
//!    - `self_test()` — 安装后/启动前验证
//!    - `diagnostics()` — 引擎专属诊断投影
//! 3. 在 `EngineRegistry::new()` 中编译期注册 adapter。
//! 4. `prepare_launch` 必须从 descriptor 锁定的 artifact 自行解析 executable/args/env，
//!    **不接收前端提供的 executable、argv、脚本路径、环境变量或任意 URL**。
//! 5. `map_health` 必须把 FunASR 的 HTTP /health 响应映射为领域统一的
//!    ServiceHealth / ModelHealth / BackendObservation。

pub mod dto;
pub mod event_port;
pub mod funasr;
pub mod ocr_coordinator; // 0.22.4：OCR Coordinator（路由 + 生命周期 + 并发）
pub mod paddleocr; // 0.22.4：PaddleOCR adapter
pub mod registry;
pub mod service;

#[allow(unused_imports)]
pub use event_port::{TauriEventPort, make_event_port};
#[allow(unused_imports)]
pub use registry::{EngineRegistry, RegistryEntry, RegistryLookup};
#[allow(unused_imports)]
pub use service::{EventPort, LocalEngineService, NoopEventPort, StructuredLogEntry};
