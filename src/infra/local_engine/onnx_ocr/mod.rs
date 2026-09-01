//! ONNX OCR in-process executor（0.22.8-C）。
//!
//! `OnnxOcrExecutor` 是 topology-neutral 的进程内 OCR 执行器：
//! - 不启动子进程、不走 HTTP——直接在 Blink 进程内的专用阻塞线程上
//!   持有 `oar_ocr::OAROCR` pipeline 并执行推理。
//! - 专用阻塞线程：ORT Session 的 `predict()` 是同步阻塞调用，
//!   绝不在 tokio runtime 线程上执行。
//! - 有界队列：最多 4 个 pending 请求排队，第 5 个立即返回
//!   `BackendUnavailable`（背压），不无限堆积。
//! - 生命周期：lazy load（首次请求触发 Session 构建）+ TTL drop
//!   （idle 超时后 drop Session 释放内存）。
//! - 诚实取消：每个请求携带 `CancellationToken`，取消后 receiver 立即
//!   停止等待，但已提交给工作线程的请求会完成（结果被丢弃）。
//!
//! ## 设计铁则
//!
//! - **topology-neutral**：executor 不理解 PaddleOCR/det/rec 的业务语义，
//!   只负责"把 PNG bytes 喂给某个 in-process OCR pipeline 并拿回文本"。
//! - **不持有 tokio runtime handle**：工作线程是 `std::thread::spawn`，
//!   与 tokio 异步世界通过 `tokio::sync::oneshot` 回传结果。
//! - **线程退出保证**：shutdown 时通过 drop sender 让工作线程自然退出，
//!   另加 join timeout 兜底。
//! - **不 use tauri**：infra 层，不依赖 app/domain/tauri。

#[allow(dead_code)]
pub mod executor;
#[allow(dead_code)]
pub mod pipeline;
#[allow(dead_code)]
pub mod state;

#[allow(unused_imports)]
pub use executor::{
    OcrExecutor, OcrExecutorConfig, OcrExecutorError, OnnxOcrExecutor, RecognizeRequest,
};
#[allow(unused_imports)]
pub use state::ExecutorState;

#[cfg(test)]
mod tests;
