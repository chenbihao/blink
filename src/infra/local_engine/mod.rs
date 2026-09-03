//! 本地引擎受管子进程基础设施（0.22.1）。
//!
//! 提供通用、安全、可测试的受管子进程生命周期与有界日志管道。
//! 不理解 FunASR、OCR、模型或 Provider——只提供通用的进程管理原语。
//!
//! ## 分层归属
//!
//! - `infra/local_engine`：启动/停止子进程、排空管道、PID 身份验证、端口探测、
//!   endpoint 分配与身份验证原语；不依赖 app/domain/tauri，不发送 Tauri 事件。
//! - `domain/local_engine`（0.22.3）：领域类型、状态、描述符与 adapter 契约。
//! - `app/local_engine`（0.22.3）：读取配置、串行化状态、调用 adapter + infra、
//!   持有运行实例、广播事件，退出时回收所有受管进程。
//!
//! ## 0.22.3 新增
//!
//! - `port` 模块：provider-neutral 的 endpoint 分配、冲突重试和身份验证原语。
//!   仅允许 loopback；未知端口占用绝不触发 kill。
//!
//! ## 并发模型
//!
//! 单个内部状态锁 + 每次启动唯一 InstanceToken 保证提交条件。
//! 耗时 spawn/wait 不长期持有全局锁。
//! 旧 generation 的退出事件不能覆盖新 generation 状态。

pub mod asset_lock;
pub mod deployment;
pub mod lease;
pub mod lease_recovery;
pub mod log_pipe;
pub mod model_storage;
pub mod onnx_ocr;
pub mod paraformer_runner;
pub mod paraformer_worker;
pub mod port;
pub mod process;
pub mod providers;
pub mod runtime;
pub mod state;
pub mod stream_worker_lifecycle;
pub mod stream_worker_proto;
pub mod streaming_stt_adapter;
pub mod stt_asset_lock;
#[cfg(test)]
mod tests;
pub mod worker_proto;
