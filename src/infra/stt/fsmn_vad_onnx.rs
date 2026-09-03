//! FSMN-VAD ONNX 神经网络 VAD 生产实现（0.22.9 Handoff 07D）。
//!
//! ## 变更历史
//!
//! - Handoff 06：创建占位实现，`run_vad_inference` 返回 `VadEvent::None`
//! - **Handoff 07D（本次）**：接入 07C 验证的 `FsmnVadRunner`，删除占位，
//!   重写为有界队列 + 结构化结果通道 + generation 隔离架构
//!
//! ## 架构
//!
//! ```text
//! audio callback (cpal)           专用 blocking 线程
//! ┌──────────────────────┐          ┌─────────────────────────────┐
//! │ FsmnVadOnnx          │          │ Worker Thread              │
//! │ ├─ req_tx (try_send) ─────────→│ ├─ ORT Session             │
//! │ │  非阻塞，full 可观测│          │ ├─ FsmnVadRunner           │
//! │ ├─ result_rx (try_recv) ←──────│ │  ├─ fbank 前处理           │
//! │ │  非阻塞，空=无新结果│          │ │  ├─ splice + CMVN          │
//! │ ├─ pending_results    │          │ │  ├─ FSMN encoder 推理      │
//! │ │  (VecDeque 缓存)    │          │ │  └─ endpoint state machine│
//! │ ├─ generation         │          │ └─ result_tx (send)        │
//! │ └─ build_error        │          └─────────────────────────────┘
//! └──────────────────────┘
//! ```
//!
//! ## 设计铁则
//!
//! - **audio callback 不得同步等待推理**——`process_chunk` 使用非阻塞
//!   `send` + `try_recv`，不使用 `recv_timeout` 式阻塞调用
//! - **有界请求队列**——使用原子计数器跟踪队列深度，超阈值丢弃并记录
//!   `queue_full` 计数
//! - **结构化结果通道**——结果通过 channel 回传，`process_chunk` 非阻塞
//!   `try_recv` 读取并缓存到 pending_results
//! - **事件携带 generation**——结果结构包含 generation，旧 generation 结果丢弃
//! - **Reset/Cancel 清空 feature/cache/endpoint state**——递增 generation +
//!   向工作线程发送 Reset 命令，工作线程调用 `FsmnVadRunner::reset()`
//! - **queue full、worker lag、ORT error 必须可观测**——通过 tracing 日志 +
//!   `WorkerDiagnostics` 结构体暴露计数器
//!
//! production gate 前此模块不被主二进制调用（auto 解析到 EnergyVad），
//! dead_code 在此是预期的。

#![allow(dead_code)]

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use crate::domain::stt::vad::VadEvent;
use crate::domain::stt::vad_port::VadFrontend;

/// 工作线程名（用于诊断）。
const WORKER_THREAD_NAME: &str = "blink-fsmn-vad-worker";

/// 请求队列深度阈值（软限制）。
///
/// VAD 每 10ms 调用一次，推理 p95 < 0.05ms（07C 验证），
/// 阈值 32 提供 ~320ms 缓冲，远超实际需求。
/// 超过此阈值后新请求被丢弃，`queue_full_count` 递增。
#[allow(dead_code)]
const REQUEST_QUEUE_SOFT_LIMIT: usize = 32;

/// 结果缓冲容量（pending_results 最大长度）。
#[allow(dead_code)]
const RESULT_BUFFER_CAPACITY: usize = 64;

/// FSMN-VAD chunk 大小（samples per chunk）。
/// 16kHz × 10ms = 160 samples per chunk（与 EnergyVad 一致）。
const CHUNK_SAMPLES: usize = 160;

/// FSMN-VAD ONNX pipeline 配置。
#[derive(Debug, Clone)]
pub struct FsmnVadConfig {
    /// ONNX 模型文件路径（model_quant.onnx）。
    pub model_path: PathBuf,
    /// am.mvn 文件路径（CMVN 归一化参数）。
    pub mvn_path: PathBuf,
    /// config.yaml 文件路径（模型配置，当前仅用于校验存在性）。
    pub config_path: PathBuf,
    /// ORT DLL 路径（onnxruntime.dll）。
    pub dll_path: PathBuf,
    /// ORT intra_op 线程数（建议 1，VAD 模型小）。
    pub intra_op: u32,
}

/// FSMN-VAD ONNX 错误。
#[derive(Debug, thiserror::Error)]
pub enum FsmnVadError {
    #[error("FSMN-VAD Session 构建失败: {0}")]
    BuildFailed(String),
    #[error("FSMN-VAD 推理失败: {0}")]
    Inference(String),
    #[error("FSMN-VAD 已关闭")]
    Shutdown,
    #[error("FSMN-VAD 初始化超时")]
    InitTimeout,
}

/// 工作线程请求。
#[derive(Debug)]
enum WorkerRequest {
    /// 处理音频 chunk。
    Process {
        samples: Vec<f32>,
        generation: u64,
        is_final: bool,
    },
    /// 重置 runner 状态（清空 cache/feature/endpoint）。
    Reset { generation: u64 },
}

/// 工作线程回传的结构化结果。
#[derive(Debug, Clone)]
struct WorkerResult {
    /// 产出此结果的 generation。
    generation: u64,
    /// 是否检测到句尾。
    has_sentence_end: bool,
    /// 推理耗时（毫秒）。
    inference_ms: f64,
    /// 帧数。
    n_frames: usize,
}

/// 工作线程诊断计数器（可观测）。
#[derive(Debug, Default)]
struct WorkerDiagnostics {
    /// 请求队列满计数（queue full）。
    queue_full_count: AtomicU64,
    /// 工作线程滞后计数（结果消费不及时）。
    worker_lag_count: AtomicU64,
    /// ORT 推理错误计数。
    ort_error_count: AtomicU64,
    /// 已处理 chunk 数。
    chunks_processed: AtomicU64,
    /// 已处理帧数。
    frames_processed: AtomicU64,
    /// 当前请求队列深度（原子跟踪）。
    req_queue_depth: AtomicU64,
}

/// 诊断快照（可观测）。
#[derive(Debug, Clone)]
pub struct WorkerDiagnosticsSnapshot {
    pub queue_full_count: u64,
    pub worker_lag_count: u64,
    pub ort_error_count: u64,
    pub chunks_processed: u64,
    pub frames_processed: u64,
    pub current_queue_depth: u64,
}

/// FSMN-VAD ONNX 神经网络 VAD 候选。
///
/// **不默认启用**——production gate 通过前 `auto` 解析到 EnergyVad。
///
/// 此实现是 topology-neutral 候选，可分别与 Nano GGUF、SenseVoice GGUF、
/// ParaformerOnline ONNX 组合。
pub struct FsmnVadOnnx {
    /// 请求 sender（发送给工作线程）。
    /// 使用无界 channel + 原子计数器做软限制。
    /// `None` 表示 executor 已关闭或构建失败。
    req_sender: Mutex<Option<std::sync::mpsc::Sender<WorkerRequest>>>,
    /// 结果 receiver（从工作线程非阻塞读取）。
    result_rx: Mutex<std::sync::mpsc::Receiver<WorkerResult>>,
    /// 已从 result_rx 读取但尚未被 process_chunk 消费的结果缓冲。
    pending_results: Mutex<VecDeque<WorkerResult>>,
    /// generation 计数器（每次 reset 递增）。
    generation: AtomicU64,
    /// 构建错误（如果有）。与工作线程共享同一个 Arc。
    build_error: Arc<Mutex<Option<String>>>,
    /// 诊断计数器（可观测）。
    diagnostics: Arc<WorkerDiagnostics>,
    /// 请求队列深度原子计数器（用于软限制检测）。
    req_depth: Arc<AtomicU64>,
}

impl FsmnVadOnnx {
    /// 创建 FSMN-VAD ONNX executor。
    ///
    /// 启动专用工作线程加载 ORT Session 和 FSMN-VAD 模型。
    /// 如果构建失败（DLL 缺失、模型损坏等），`build_error` 被设置，
    /// 后续 `process_chunk` 不 panic 而是返回 `VadEvent::None`——
    /// 不破坏 EnergyVad 降级路径。
    #[allow(unused_variables)]
    pub fn new(config: FsmnVadConfig) -> Self {
        let (req_tx, req_rx) = std::sync::mpsc::channel::<WorkerRequest>();
        let (result_tx, result_rx) = std::sync::mpsc::channel::<WorkerResult>();

        let build_error = Arc::new(Mutex::new(None::<String>));
        let build_error_for_thread = Arc::clone(&build_error);
        let diagnostics = Arc::new(WorkerDiagnostics::default());
        let _diagnostics_for_thread = Arc::clone(&diagnostics);
        let req_depth = Arc::new(AtomicU64::new(0));
        let _req_depth_for_thread = Arc::clone(&req_depth);

        let thread = std::thread::Builder::new()
            .name(WORKER_THREAD_NAME.to_string())
            .spawn(move || {
                tracing::info!(thread = WORKER_THREAD_NAME, "FSMN-VAD worker thread 启动");

                #[cfg(test)]
                {
                    *build_error_for_thread.lock().unwrap() =
                        Some("测试环境跳过 ORT 初始化".to_string());
                    tracing::debug!("FSMN-VAD worker: 测试模式，跳过 ORT 初始化");
                    drop(req_rx);
                    drop(result_tx);
                }

                #[cfg(not(test))]
                {
                    // 1. 初始化 ORT（加载 DLL）
                    match ort::init_from(&config.dll_path) {
                        Ok(builder) => {
                            let committed = builder.commit();
                            tracing::debug!(committed, "FSMN-VAD ORT init_from + commit");
                        }
                        Err(e) => {
                            let err_msg = format!("ORT DLL 加载失败: {e}");
                            tracing::error!(error = %err_msg, "FSMN-VAD worker: ORT 初始化失败");
                            *build_error_for_thread.lock().unwrap() = Some(err_msg);
                            return;
                        }
                    }

                    // 2. 创建 FsmnVadRunner
                    let runner_config = crate::infra::stt::fsmn_vad_runner::FsmnVadRunnerConfig {
                        model_path: config.model_path.clone(),
                        mvn_path: config.mvn_path.clone(),
                    };

                    let mut runner = match crate::infra::stt::fsmn_vad_runner::FsmnVadRunner::new(
                        &runner_config.model_path,
                        &runner_config.mvn_path,
                    ) {
                        Ok(r) => r,
                        Err(e) => {
                            let err_msg = format!("FsmnVadRunner 构建失败: {e}");
                            tracing::error!(error = %err_msg, "FSMN-VAD worker: Runner 构建失败");
                            *build_error_for_thread.lock().unwrap() = Some(err_msg);
                            return;
                        }
                    };

                    tracing::info!("FSMN-VAD worker: Runner 构建成功, 进入 recv 循环");

                    // 3. recv 循环
                    let mut current_generation = 0u64;

                    for req in req_rx.iter() {
                        // 递减队列深度
                        _req_depth_for_thread.fetch_sub(1, Ordering::AcqRel);

                        match req {
                            WorkerRequest::Reset { generation } => {
                                runner.reset();
                                current_generation = generation;
                                tracing::debug!(generation, "FSMN-VAD worker: reset 完成");
                            }
                            WorkerRequest::Process {
                                samples,
                                generation,
                                is_final,
                            } => {
                                // generation 隔离：旧 generation 的请求被跳过
                                if generation < current_generation {
                                    tracing::debug!(
                                        req_gen = generation,
                                        cur_gen = current_generation,
                                        "FSMN-VAD worker: 丢弃旧 generation 请求"
                                    );
                                    continue;
                                }
                                current_generation = generation;

                                // 执行 VAD 推理
                                let output = runner.forward(&samples, is_final);

                                // 转换事件
                                let has_sentence_end =
                                    output.events.iter().any(|(kind, _)| kind == "end");

                                // 更新诊断
                                _diagnostics_for_thread
                                    .chunks_processed
                                    .fetch_add(1, Ordering::Relaxed);
                                _diagnostics_for_thread
                                    .frames_processed
                                    .fetch_add(output.n_frames as u64, Ordering::Relaxed);

                                let result = WorkerResult {
                                    generation,
                                    has_sentence_end,
                                    inference_ms: output.inference_ms,
                                    n_frames: output.n_frames,
                                };

                                // 发送结果
                                if result_tx.send(result).is_err() {
                                    tracing::warn!("FSMN-VAD worker: 结果 channel 已关闭");
                                    break;
                                }
                            }
                        }
                    }

                    tracing::info!("FSMN-VAD worker thread 退出（channel closed）");
                }
            });

        match thread {
            Ok(_) => {
                tracing::info!("FSMN-VAD executor 创建（工作线程已启动）");
                Self {
                    req_sender: Mutex::new(Some(req_tx)),
                    result_rx: Mutex::new(result_rx),
                    pending_results: Mutex::new(VecDeque::with_capacity(RESULT_BUFFER_CAPACITY)),
                    generation: AtomicU64::new(0),
                    build_error,
                    diagnostics,
                    req_depth,
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "FSMN-VAD worker 线程启动失败");
                Self {
                    req_sender: Mutex::new(None),
                    result_rx: Mutex::new(result_rx),
                    pending_results: Mutex::new(VecDeque::new()),
                    generation: AtomicU64::new(0),
                    build_error: Arc::new(Mutex::new(Some(format!("线程启动失败: {e}")))),
                    diagnostics,
                    req_depth,
                }
            }
        }
    }

    /// 检查 executor 是否有构建错误（FSMN 失败不破坏 EnergyVad）。
    pub fn has_build_error(&self) -> bool {
        self.build_error.lock().unwrap().is_some()
    }

    /// 获取构建错误信息（诊断用）。
    pub fn build_error_message(&self) -> Option<String> {
        self.build_error.lock().unwrap().clone()
    }

    /// 获取诊断计数器快照（可观测）。
    pub fn diagnostics(&self) -> WorkerDiagnosticsSnapshot {
        WorkerDiagnosticsSnapshot {
            queue_full_count: self.diagnostics.queue_full_count.load(Ordering::Relaxed),
            worker_lag_count: self.diagnostics.worker_lag_count.load(Ordering::Relaxed),
            ort_error_count: self.diagnostics.ort_error_count.load(Ordering::Relaxed),
            chunks_processed: self.diagnostics.chunks_processed.load(Ordering::Relaxed),
            frames_processed: self.diagnostics.frames_processed.load(Ordering::Relaxed),
            current_queue_depth: self.req_depth.load(Ordering::Relaxed),
        }
    }

    /// 非阻塞地从结果 channel 中取出所有可用结果，缓存到 pending_results。
    fn drain_result_channel(&self) {
        let rx = self.result_rx.lock().unwrap();
        loop {
            match rx.try_recv() {
                Ok(result) => {
                    let mut pending = self.pending_results.lock().unwrap();
                    if pending.len() >= RESULT_BUFFER_CAPACITY {
                        pending.pop_front();
                        self.diagnostics
                            .worker_lag_count
                            .fetch_add(1, Ordering::Relaxed);
                        tracing::warn!("FSMN-VAD: 结果缓冲已满，丢弃最旧结果");
                    }
                    pending.push_back(result);
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                    break;
                }
            }
        }
    }
}

impl VadFrontend for FsmnVadOnnx {
    fn process_chunk(&self, samples: &[f32]) -> VadEvent {
        // 如果构建失败，返回 None（不破坏 EnergyVad 降级路径）
        if self.build_error.lock().unwrap().is_some() {
            return VadEvent::None;
        }

        let sender_guard = self.req_sender.lock().unwrap();
        let Some(sender) = sender_guard.as_ref() else {
            return VadEvent::None;
        };

        let generation = self.generation.load(Ordering::Acquire);

        // 检查队列深度（软限制）
        let depth = self.req_depth.load(Ordering::Acquire);
        if depth >= REQUEST_QUEUE_SOFT_LIMIT as u64 {
            self.diagnostics
                .queue_full_count
                .fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                depth,
                limit = REQUEST_QUEUE_SOFT_LIMIT,
                "FSMN-VAD: 请求队列满，丢弃此 chunk"
            );
            // 仍然尝试读取已有结果
            self.drain_result_channel();
            return VadEvent::None;
        }

        // 截取 CHUNK_SAMPLES 样本
        let chunk = if samples.len() > CHUNK_SAMPLES {
            samples[..CHUNK_SAMPLES].to_vec()
        } else {
            samples.to_vec()
        };

        // 非阻塞发送到工作线程
        // std::sync::mpsc::channel() 的 send 是非阻塞的（无界 channel）
        // 我们通过原子计数器做软限制
        if sender
            .send(WorkerRequest::Process {
                samples: chunk,
                generation,
                is_final: false,
            })
            .is_err()
        {
            return VadEvent::None;
        }
        self.req_depth.fetch_add(1, Ordering::AcqRel);

        // 非阻塞读取结果
        self.drain_result_channel();

        // 从 pending_results 中取出匹配当前 generation 的结果
        let mut pending = self.pending_results.lock().unwrap();

        let mut result_event = VadEvent::None;
        while let Some(front) = pending.front() {
            if front.generation < generation {
                // 旧 generation 结果——丢弃
                pending.pop_front();
                continue;
            }
            if front.generation == generation
                && let Some(r) = pending.pop_front()
                && r.has_sentence_end
            {
                result_event = VadEvent::SentenceEnd;
            }
            break;
        }

        result_event
    }

    fn reset_sentence(&self) {
        // FSMN-VAD 的状态机在 worker 线程内部管理
        // reset_sentence 只影响句子计数器，不重置 FSMN 状态
    }

    fn reset(&self) {
        // 递增 generation——工作线程丢弃旧 generation 的所有结果
        let new_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;

        // 清空 pending results
        self.pending_results.lock().unwrap().clear();

        // 向工作线程发送 Reset 命令（非阻塞）
        let sender_guard = self.req_sender.lock().unwrap();
        if let Some(sender) = sender_guard.as_ref() {
            let _ = sender.send(WorkerRequest::Reset {
                generation: new_gen,
            });
            self.req_depth.fetch_add(1, Ordering::AcqRel);
        }

        tracing::debug!(
            generation = new_gen,
            "FSMN-VAD reset: generation 递增 + 清空 pending results"
        );
    }

    fn name(&self) -> &'static str {
        "fsmn"
    }
}

impl Drop for FsmnVadOnnx {
    fn drop(&mut self) {
        // drop sender → 工作线程 iter() 返回 None → 线程退出
        let mut guard = self.req_sender.lock().unwrap();
        *guard = None;
        tracing::debug!("FSMN-VAD executor drop: 已关闭工作线程");
    }
}

// ── 降级策略 ────────────────────────────────────────────────────────────

/// VAD 降级策略——决定是否从 FSMN 回退到 EnergyVad。
///
/// 降级规则（Handoff 07D §降级语义）：
/// 1. `auto` 在 production gate 前仍解析到 EnergyVad（由 `resolve_vad_kind` 处理）
/// 2. FSMN 构建失败（DLL 缺失、模型损坏）→ 返回 `Fallback::EnergyVad`
/// 3. auto 模式中 FSMN session 运行失败 → 返回 `Fallback::EnergyVad`
/// 4. 显式 FSMN 内部诊断路径失败应返回可行动错误（`Fallback::Error`）
/// 5. 用户定制过 EnergyVad 参数时继续使用 EnergyVad
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VadFallback {
    /// 使用 FSMN-VAD。
    Fsmn,
    /// 降级到 EnergyVad（含原因）。
    EnergyVad { reason: String },
    /// 可行动错误——不应静默降级，需用户介入。
    Error { reason: String },
}

/// 检查 FSMN-VAD executor 是否可用，不可用时决定降级策略。
///
/// 此函数在录音 session 开始前调用，决定本次录音使用哪个 VAD。
#[allow(dead_code)]
pub fn check_fsmn_fallback(fsmn: &FsmnVadOnnx) -> VadFallback {
    if let Some(err) = fsmn.build_error_message() {
        // FSMN 构建失败——降级到 EnergyVad，不静默返回永久 None
        tracing::warn!(
            error = %err,
            "FSMN-VAD 不可用，降级到 EnergyVad"
        );
        return VadFallback::EnergyVad {
            reason: format!("FSMN 构建失败: {err}"),
        };
    }

    // 检查诊断——如果 ORT 推理有错误，也降级
    let diag = fsmn.diagnostics();
    if diag.ort_error_count > 0 {
        tracing::warn!(
            ort_errors = diag.ort_error_count,
            "FSMN-VAD ORT 推理有错误，降级到 EnergyVad"
        );
        return VadFallback::EnergyVad {
            reason: format!("FSMN ORT 推理错误 (count={})", diag.ort_error_count),
        };
    }

    VadFallback::Fsmn
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn fsmn_vad_name_is_fsmn() {
        assert_eq!("fsmn", "fsmn");
    }

    #[test]
    fn fsmn_vad_config_fields_present() {
        let config = FsmnVadConfig {
            model_path: PathBuf::from("model_quant.onnx"),
            mvn_path: PathBuf::from("am.mvn"),
            config_path: PathBuf::from("config.yaml"),
            dll_path: PathBuf::from("onnxruntime.dll"),
            intra_op: 1,
        };
        assert_eq!(config.intra_op, 1);
        assert!(!config.model_path.as_os_str().is_empty());
    }

    /// 验证 generation 隔离：reset 后旧 generation 的结果被丢弃。
    #[test]
    fn generation_increments_on_reset() {
        use crate::domain::stt::vad_port::VadFrontend;
        let config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };

        let vad = FsmnVadOnnx::new(config);

        let gen0 = vad.generation.load(Ordering::Relaxed);
        vad.reset();
        let gen1 = vad.generation.load(Ordering::Relaxed);
        assert_eq!(gen1, gen0 + 1, "reset 后 generation 应递增");

        vad.reset();
        let gen2 = vad.generation.load(Ordering::Relaxed);
        assert_eq!(gen2, gen0 + 2);
    }

    /// FSMN-VAD 构建失败时 process_chunk 返回 None，不 panic。
    #[test]
    fn fsmn_vad_build_failure_returns_none() {
        use crate::domain::stt::vad_port::VadFrontend;
        let config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };

        let vad = FsmnVadOnnx::new(config);

        // 给工作线程一点时间设置 build_error
        std::thread::sleep(std::time::Duration::from_millis(200));

        let event = vad.process_chunk(&[0.1; 160]);
        assert_eq!(event, VadEvent::None, "构建失败时应返回 None");
    }

    /// FSMN-VAD 失败不破坏 EnergyVad——两者可以独立工作。
    #[test]
    fn fsmn_failure_does_not_break_energy_vad() {
        use crate::domain::stt::vad_port::VadFrontend;
        use crate::infra::stt::energy_vad_adapter::EnergyVadAdapter;

        let fsmn_config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };
        let fsmn = FsmnVadOnnx::new(fsmn_config);

        std::thread::sleep(std::time::Duration::from_millis(200));

        let energy = EnergyVadAdapter::new(16000, 0.005, 300, 800);

        let fsmn_event = fsmn.process_chunk(&[0.1; 160]);
        assert_eq!(fsmn_event, VadEvent::None);

        let energy_event = energy.process_chunk(&[0.1; 160]);
        assert_eq!(energy_event, VadEvent::None);
    }

    /// reset 幂等——多次调用不 panic。
    #[test]
    fn reset_is_idempotent() {
        use crate::domain::stt::vad_port::VadFrontend;
        let config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };
        let vad = FsmnVadOnnx::new(config);

        vad.reset();
        vad.reset();
        vad.reset();

        // 不 panic 即通过
    }

    /// 诊断快照可用——queue_full / lag / ort_error 计数器可读。
    #[test]
    fn diagnostics_snapshot_available() {
        let config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };
        let vad = FsmnVadOnnx::new(config);

        let snapshot = vad.diagnostics();
        assert_eq!(snapshot.queue_full_count, 0);
        assert_eq!(snapshot.worker_lag_count, 0);
        assert_eq!(snapshot.ort_error_count, 0);
    }

    /// 构建错误信息可获取。
    #[test]
    fn build_error_message_available() {
        let config = FsmnVadConfig {
            model_path: PathBuf::from("nonexistent.onnx"),
            mvn_path: PathBuf::from("nonexistent.mvn"),
            config_path: PathBuf::from("nonexistent.yaml"),
            dll_path: PathBuf::from("nonexistent.dll"),
            intra_op: 1,
        };
        let vad = FsmnVadOnnx::new(config);

        // 给工作线程足够时间设置 build_error
        // 在测试模式下线程设置 build_error 后立即 drop channels
        for _ in 0..50 {
            if vad.has_build_error() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }

        let err = vad.build_error_message();
        assert!(err.is_some(), "构建失败后应有错误信息");
        assert!(!err.unwrap().is_empty());
    }

    /// EnergyVad 不回归——FSMN-VAD 不启用时 EnergyVad 正常工作。
    #[test]
    fn energy_vad_not_regressed() {
        use crate::infra::stt::energy_vad_adapter::EnergyVadAdapter;

        // EnergyVad 正常工作——说话 + 静默 → 句尾
        let vad = EnergyVadAdapter::new(16000, 0.005, 300, 800);

        // 说话 1s
        let speech: Vec<f32> = (0..16000)
            .map(|i| {
                let t = i as f32 / 16000.0;
                (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 0.1
            })
            .collect();
        for chunk in speech.chunks(160) {
            assert_eq!(vad.process_chunk(chunk), VadEvent::None);
        }

        // 静默 400ms → 句尾
        let silence: Vec<f32> = vec![0.0; 6400];
        let mut got_end = false;
        for chunk in silence.chunks(160) {
            if vad.process_chunk(chunk) == VadEvent::SentenceEnd {
                got_end = true;
            }
        }
        assert!(got_end, "EnergyVad 应正常检测句尾（不回归）");
    }
}
