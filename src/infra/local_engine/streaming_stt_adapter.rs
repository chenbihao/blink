//! ParaformerOnline ONNX worker 的 `StreamingSttPort` 适配器（0.22.9 Handoff 05）。
//!
//! 将 `StreamWorkerClient`（二进制协议 v2）包装为 domain 层的
//! `StreamingSttPort`，使 VoiceService 只消费统一 `SttEvent`。
//!
//! ## 工作模式
//!
//! ParaformerOnline 是真流式引擎——消费连续 PCM 音频，在 `push_audio` 期间
//! 产生 native partial 结果。
//!
//! ## 生命周期
//!
//! - `begin_session` → `send_hello` + `wait_ready` + `begin_stream`，返回 generation
//! - `push_audio` → `send_audio`（f32 → AudioFrame），非阻塞
//! - `finish_session` → `end_stream`，产出 `SttEvent::Final`
//! - `cancel_session` → `cancel_stream` + `reset`，丢弃在途结果
//! - `reset` → `reset`（幂等）
//!
//! ## 事件流
//!
//! ParaformerOnline worker 通过二进制协议 v2 产生 `Partial` 和 `Final` 消息。
//! 此适配器在独立 reader task 中消费 worker 事件，转换为 `SttEvent` 发送给
//! VoiceService 的 receiver。
//!
//! **Busy 处理**：worker 队列满时返回 `Busy`，适配器产出 `SttEvent::Busy`。
//!
//! ## domain 不依赖 ORT
//!
//! 此适配器位于 infra 层，依赖 `StreamWorkerClient`（也是 infra）。
//! domain 层的 `StreamingSttPort` trait 不引用 ORT、worker framing 或 concrete runtime。

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::{Mutex, Mutex as TokioMutex, mpsc};

use crate::domain::stt::{StreamingSttPort, SttError, SttEvent};
use crate::infra::local_engine::process::{
    LaunchRequest, ManagedProcess, ShutdownConfig, StdioConfig,
};
use crate::infra::local_engine::stream_worker_proto::{AudioFrame, ProtoError, StreamWorkerClient};

/// ParaformerOnline ONNX worker 的 `StreamingSttPort` 适配器。
///
/// 包装 `StreamWorkerClient`，将二进制协议 v2 的事件转换为 `SttEvent`。
///
/// ## 生产接线（Handoff 08）
///
/// EngineManager start 负责进程 spawn 与 hello/ready 握手，随后经
/// [`Self::with_process`] 构造适配器并存入 EngineEntry；`get_connection`
/// 把它投影为 VoiceService 消费的 `StreamingSttPort`。
///
/// ## Host Launcher
///
/// `launch()` 是独立入口——从冻结的 deployment snapshot 解析 worker 所需
/// 资产，创建 `ManagedProcess` 和 `StreamWorkerClient`，等待真实 Ready 后
/// 返回适配器（gate harness / 诊断工具使用）。
///
/// - **Ready 必须在 ORT 和模型真实加载成功后发送**——worker 端在创建
///   `ParaformerRunner` 成功后才发 Ready，host 收到后才视为实现就绪。
/// - **不修改已有用户 selected model**——适配器独立于用户模型选择。
pub struct ParaformerOnlineAdapter {
    /// worker client（由 app 层注入或 `launch` 创建）
    client: Arc<StreamWorkerClient>,
    /// 事件 sender
    event_tx: std::sync::Mutex<mpsc::UnboundedSender<SttEvent>>,
    /// 事件 receiver（events() 取出后置 None）
    event_rx: TokioMutex<Option<mpsc::UnboundedReceiver<SttEvent>>>,
    /// generation 计数器（与 worker 的 generation 同步）
    generation: AtomicU64,
    /// 当前 active generation
    active_gen: Mutex<Option<u64>>,
    /// 本 session 已收到的 Partial fragment 累积（0.22.9）。
    ///
    /// worker 的 Partial 是当前 chunk 的增量 fragment（CIF 在线解码逐 chunk
    /// 出新 token，不重复）；适配器在此累积——之前所有 fragment 固化为
    /// confirmed、最新 fragment 作为 preview，与伪流式的"固化 + 候选"
    /// 分层显示对齐。begin/cancel/reset/finish 时清空。
    partial_accumulated: std::sync::Mutex<String>,
    /// reader task JoinHandle
    reader_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// ManagedProcess 句柄——保持进程存活，drop 时触发回收
    #[allow(dead_code)]
    process: Option<Arc<ManagedProcess>>,
}

impl ParaformerOnlineAdapter {
    /// 创建适配器。
    ///
    /// `client` 必须已完成 `send_hello` + `wait_ready` 握手。
    #[allow(dead_code)]
    pub fn new(client: Arc<StreamWorkerClient>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        Self {
            client,
            event_tx: std::sync::Mutex::new(event_tx),
            event_rx: TokioMutex::new(Some(event_rx)),
            generation: AtomicU64::new(0),
            active_gen: Mutex::new(None),
            partial_accumulated: std::sync::Mutex::new(String::new()),
            reader_task: Mutex::new(None),
            process: None,
        }
    }

    /// 创建适配器并绑定受管进程句柄（EngineManager start 生产路径）。
    ///
    /// `client` 必须已完成 `send_hello` + `wait_ready` 握手；
    /// `process` 由调用方（EngineManager）持有并纳入生命周期管理，
    /// 适配器侧持有引用以保证 drop 语义完整（最后释放者触发 Job 回收）。
    pub fn with_process(client: Arc<StreamWorkerClient>, process: Arc<ManagedProcess>) -> Self {
        let mut adapter = Self::new(client);
        adapter.process = Some(process);
        adapter
    }

    /// worker 是否仍然健康（管道未断、未 poison）。
    ///
    /// VoiceService 在开始录音前以此做轻量就绪检查；
    /// ready 语义由 start 时的 hello/ready 握手保证——此处只检测断连。
    pub fn is_ready(&self) -> bool {
        !self.client.is_poisoned()
    }

    /// Host Launcher——启动真实 ParaformerOnline worker 子进程。
    ///
    /// 从冻结的 deployment snapshot 解析 worker 所需资产路径，创建
    /// `ManagedProcess` 和 `StreamWorkerClient`，等待真实 Ready。
    ///
    /// ## 参数
    ///
    /// - `deployment_dir`：包含 onnxruntime.dll / encoder.onnx / decoder.onnx /
    ///   am.mvn / tokenizer.json 的 active deployment slot 目录。
    /// - `ready_timeout`：等待 worker Ready 的超时（模型加载 + ORT 初始化）。
    ///
    /// ## 铁则
    ///
    /// - Ready 必须在 ORT 和模型真实加载成功后发送
    /// - worker stdout 零文本污染（binary protocol only）
    /// - crash/EOF 可被 host 和 EngineManager 正确感知
    /// - 主动 stop 不被投影成意外崩溃
    ///
    /// ## 退出语义
    ///
    /// - 适配器 drop → `ManagedProcess` drop → Job Object 回收进程树
    /// - worker EOF/poison → `is_poisoned()` 返回 true，后续操作失败
    /// - 可通过 `stop()` 主动优雅退出（Quit + 等待）
    #[allow(dead_code)]
    pub async fn launch(
        deployment_dir: PathBuf,
        ready_timeout: std::time::Duration,
    ) -> Result<Self, ParaformerLaunchError> {
        // ── 0. 规范化 deployment_dir 为绝对路径 ─────────────────────
        // ManagedProcess 设置 current_dir = deployment_dir，子进程 CWD 变为
        // deployment 目录。如果 deployment_dir 是相对路径，worker 端用此相对
        // 路径做 validate_deployment 会相对新 CWD 查找，导致路径不存在。
        // 必须在 launch 入口处转为绝对路径。
        let deployment_dir = if deployment_dir.is_absolute() {
            deployment_dir
        } else {
            std::env::current_dir()
                .map_err(|e| ParaformerLaunchError::DeploymentInvalid(e.to_string()))?
                .join(&deployment_dir)
        };

        // ── 1. 定位 blink.exe ────────────────────────────────────────
        let exe = std::env::current_exe()
            .map_err(|e| ParaformerLaunchError::LocatorFailed(e.to_string()))?;
        let exe_dir = exe.parent().ok_or_else(|| {
            ParaformerLaunchError::LocatorFailed("无法定位 exe 父目录".to_string())
        })?;
        let blink_exe = exe_dir.join("blink.exe");
        if !blink_exe.exists() {
            return Err(ParaformerLaunchError::LocatorFailed(format!(
                "blink.exe 不存在: {}",
                blink_exe.display()
            )));
        }

        // ── 2. 验证 deployment 目录 ──────────────────────────────────
        let assets =
            crate::infra::local_engine::paraformer_worker::validate_deployment(&deployment_dir)
                .map_err(ParaformerLaunchError::DeploymentInvalid)?;

        tracing::info!(
            exe = %blink_exe.display(),
            deployment = %deployment_dir.display(),
            dll = %assets.dll.display(),
            encoder = %assets.encoder.display(),
            "ParaformerOnline host launcher: 启动真实 worker"
        );

        // ── 3. 创建 ManagedProcess ───────────────────────────────────
        let managed = ManagedProcess::with_defaults();

        let req = LaunchRequest {
            executable: blink_exe,
            args: vec![
                "paraformer-worker".into(),
                "--deployment".into(),
                deployment_dir.as_os_str().into(),
            ],
            current_dir: Some(deployment_dir.clone()),
            env: std::collections::HashMap::new(),
            instance_id: crate::infra::local_engine::process::generate_instance_id_pub(),
            label: "paraformer-worker".to_string(),
            shutdown: ShutdownConfig::default(),
            stdio: StdioConfig::worker_protocol(),
        };

        managed
            .start(&req)
            .await
            .map_err(|e| ParaformerLaunchError::SpawnFailed(e.to_string()))?;

        // ── 3.5 订阅 worker stderr 日志并转发到 host tracing ──────────
        // worker 的 tracing 输出到 stderr，由 LogPipe 泵入；
        // host 订阅 LogPipe 并转发到 host tracing，使 worker 日志可见。
        {
            let log_sub = managed.subscribe_logs();
            tokio::spawn(async move {
                let mut rx = log_sub;
                while let Ok(entry) = rx.recv().await {
                    match entry.source {
                        crate::infra::local_engine::log_pipe::LogSource::Stderr => {
                            tracing::info!(
                                target: "paraformer-worker",
                                line = %entry.text.trim(),
                                "worker stderr"
                            );
                        }
                        crate::infra::local_engine::log_pipe::LogSource::Stdout => {
                            tracing::warn!(
                                target: "paraformer-worker",
                                line = %entry.text.trim(),
                                "worker stdout（不应出现）"
                            );
                        }
                    }
                }
            });
        }

        // ── 4. 取走 worker stdio ─────────────────────────────────────
        let worker_stdio = managed
            .take_worker_stdio()
            .await
            .ok_or(ParaformerLaunchError::StdioUnavailable)?;

        // ── 5. 创建 StreamWorkerClient ───────────────────────────────
        let client =
            StreamWorkerClient::new(Box::new(worker_stdio.stdin), Box::new(worker_stdio.stdout));

        // ── 6. Hello + 等待 Ready ────────────────────────────────────
        client
            .send_hello()
            .await
            .map_err(|e| ParaformerLaunchError::ProtocolError(e.to_string()))?;

        client.wait_ready(ready_timeout).await.map_err(|e| {
            if client.is_poisoned() {
                ParaformerLaunchError::WorkerPoisoned(e.to_string())
            } else {
                ParaformerLaunchError::ReadyTimeout(e.to_string())
            }
        })?;

        tracing::info!("ParaformerOnline host launcher: worker Ready 确认，创建适配器");

        // ── 7. 构建适配器，持有 ManagedProcess ───────────────────────
        let adapter = Self::new(client);
        // 覆写 process 字段——adapter 持有 managed，保证进程存活
        // 直到 adapter drop
        let mut adapter_with_process = adapter;
        adapter_with_process.process = Some(managed);
        Ok(adapter_with_process)
    }

    /// 主动优雅退出——发送 Quit，等待进程退出。
    ///
    /// 超时后 ManagedProcess 的 Job Object 强制回收。
    pub async fn stop(&self) -> Result<(), SttError> {
        // 先发 Quit（优雅退出信号）
        let _ = self.client.send_quit().await;
        // ManagedProcess 的 wait/stop 由进程句柄负责
        // 这里只清理 adapter 状态
        *self.active_gen.lock().await = None;
        Ok(())
    }

    /// 发送事件。
    #[allow(dead_code)]
    fn emit(&self, event: SttEvent) {
        let tx = self.event_tx.lock().unwrap();
        let _ = tx.send(event);
    }

    /// 启动 reader task，消费 worker 事件并转换为 `SttEvent`。
    ///
    /// reader task 独立运行，直到 channel 关闭或 worker EOF。
    #[allow(dead_code)]
    async fn start_reader(&self, _generation: u64) {
        // 取消旧的 reader task
        if let Some(handle) = self.reader_task.lock().await.take() {
            handle.abort();
        }

        // 0.22.9: reader task 重建 event channel——新 session 新 channel
        let (tx, rx) = mpsc::unbounded_channel();
        *self.event_tx.lock().unwrap() = tx;
        *self.event_rx.lock().await = Some(rx);

        let handle = tokio::spawn(async move {
            // reader task 不再直接消费 worker stdout——StreamWorkerClient
            // 的 events() 内部已有 reader task。
            // 此 task 仅用于在 finish/cancel 时等待结果。
            // 实际事件产出由 begin/push/finish 方法内联完成。
        });

        *self.reader_task.lock().await = Some(handle);
    }
}

#[async_trait::async_trait]
impl StreamingSttPort for ParaformerOnlineAdapter {
    async fn begin_session(&self) -> Result<u64, SttError> {
        let active = self.active_gen.lock().await;
        if active.is_some() {
            return Err(SttError::Engine("已有活跃 session".to_string()));
        }

        // 检查 worker 是否已 poison
        if self.client.is_poisoned() {
            return Err(SttError::Engine(
                "ParaformerOnline worker 已断开（poisoned）".to_string(),
            ));
        }

        // 启动 reader task（重建 event channel）
        drop(active); // 释放 active_gen 锁，避免死锁
        self.start_reader(0).await;
        let mut active = self.active_gen.lock().await;

        // begin_stream（内部会等待 Ack）
        let (stream_gen, _req_id) = self
            .client
            .begin_stream()
            .await
            .map_err(|e| SttError::Engine(format!("begin_stream 失败: {e}")))?;

        // 将 worker 的 u32 generation 映射到 domain 的 u64
        let session_gen = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        debug_assert_eq!(stream_gen as u64, session_gen);

        // 新 session 清空 Partial fragment 累积
        self.partial_accumulated.lock().unwrap().clear();
        *active = Some(session_gen);

        tracing::info!(generation = session_gen, "ParaformerOnline session begin");
        Ok(session_gen)
    }

    async fn push_audio(&self, generation: u64, samples: &[f32]) -> Result<(), SttError> {
        let active = self.active_gen.lock().await;
        if *active != Some(generation) {
            return Err(SttError::Engine(format!(
                "generation 不匹配: 期望 {generation}，当前 {active:?}"
            )));
        }
        drop(active);

        // f32 samples → AudioFrame（binary payload）
        let frame = AudioFrame::from_samples(samples);

        match self.client.send_audio(generation as u32, &frame).await {
            Ok(()) => {
                // 非阻塞消费 worker 产生的 Partial/Busy 事件，转发到 adapter event channel
                let gen_u32 = generation as u32;
                let events = self.client.try_recv_partial(gen_u32).await;
                for event in events {
                    if let crate::infra::local_engine::stream_worker_proto::WorkerEvent::Frame(
                        header,
                        payload,
                    ) = event
                    {
                        if header.generation != gen_u32 {
                            continue;
                        }
                        match header.msg_type {
                            crate::infra::local_engine::stream_worker_proto::MessageType::Partial => {
                                let text = String::from_utf8_lossy(&payload).to_string();
                                if !text.is_empty() {
                                    // 之前所有 fragment 固化为 confirmed，最新
                                    // fragment 作为 preview（分层显示），本条
                                    // fragment 并入累积、下一条 Partial 时固化
                                    let confirmed = {
                                        let mut acc =
                                            self.partial_accumulated.lock().unwrap();
                                        let confirmed = acc.clone();
                                        acc.push_str(&text);
                                        confirmed
                                    };
                                    self.emit(SttEvent::Partial {
                                        generation,
                                        confirmed,
                                        preview: text,
                                    });
                                }
                            }
                            crate::infra::local_engine::stream_worker_proto::MessageType::Busy => {
                                let reason = String::from_utf8_lossy(&payload).to_string();
                                self.emit(SttEvent::Busy { generation, reason });
                            }
                            crate::infra::local_engine::stream_worker_proto::MessageType::Error => {
                                let message = String::from_utf8_lossy(&payload).to_string();
                                self.emit(SttEvent::Error { generation, message });
                            }
                            _ => {} // Final/Ack/Ready 等不在此处理
                        }
                    }
                }
                Ok(())
            }
            Err(ProtoError::Busy(reason)) => {
                self.emit(SttEvent::Busy { generation, reason });
                Ok(()) // Busy 不是致命错误
            }
            Err(e) => {
                self.emit(SttEvent::Error {
                    generation,
                    message: e.to_string(),
                });
                Err(SttError::Engine(format!("send_audio 失败: {e}")))
            }
        }
    }

    async fn finish_session(&self, generation: u64) -> Result<(), SttError> {
        let mut active = self.active_gen.lock().await;
        if *active != Some(generation) {
            return Err(SttError::Engine(format!(
                "generation 不匹配: 期望 {generation}，当前 {active:?}"
            )));
        }

        // end_stream（内部等待 Final，消费中间的 Partial）
        match self
            .client
            .end_stream(generation as u32, std::time::Duration::from_secs(10))
            .await
        {
            Ok(result) => {
                self.emit(SttEvent::Final {
                    generation,
                    text: result.text,
                });
            }
            Err(ProtoError::Busy(reason)) => {
                self.emit(SttEvent::Busy { generation, reason });
                // Busy 后重试一次（短超时）
                match self
                    .client
                    .end_stream(generation as u32, std::time::Duration::from_secs(5))
                    .await
                {
                    Ok(result) => {
                        self.emit(SttEvent::Final {
                            generation,
                            text: result.text,
                        });
                    }
                    Err(e) => {
                        self.emit(SttEvent::Error {
                            generation,
                            message: format!("end_stream 重试失败: {e}"),
                        });
                    }
                }
            }
            Err(e) => {
                self.emit(SttEvent::Error {
                    generation,
                    message: format!("end_stream 失败: {e}"),
                });
            }
        }

        // session 结束——Final 已整体替换显示，清空 Partial 累积
        *active = None;
        self.partial_accumulated.lock().unwrap().clear();
        Ok(())
    }

    async fn cancel_session(&self, generation: u64) -> Result<(), SttError> {
        let mut active = self.active_gen.lock().await;
        if *active == Some(generation) {
            // cancel_stream（幂等）
            let _ = self.client.cancel_stream(generation as u32).await;
            // reset worker
            let _ = self.client.reset().await;
            *active = None;
            self.partial_accumulated.lock().unwrap().clear();
            tracing::info!(generation, "ParaformerOnline session cancelled");
        }
        Ok(())
    }

    async fn reset(&self) -> Result<(), SttError> {
        let _ = self.client.reset().await;
        *self.active_gen.lock().await = None;
        self.partial_accumulated.lock().unwrap().clear();
        // 不递增 generation——generation 只在 begin_session 中由 begin_stream
        // 配对递增。reset 只清空状态，不分配新 generation。
        Ok(())
    }

    fn supports_native_partial(&self) -> bool {
        true
    }

    fn events(&self) -> mpsc::UnboundedReceiver<SttEvent> {
        // 取出缓存的 receiver。如果已取出，创建新 channel。
        // try_lock 避免 async 上下文中 panic（blocking_lock 不可用）。
        // 在 begin_session 的 start_reader 中已提前放入 receiver。
        match self.event_rx.try_lock() {
            Ok(mut guard) => match guard.take() {
                Some(rx) => rx,
                None => {
                    // receiver 已被取走——重建 channel
                    let (tx, rx) = mpsc::unbounded_channel();
                    *self.event_tx.lock().unwrap() = tx;
                    rx
                }
            },
            Err(_) => {
                // 锁被占用（极少情况）——重建 channel
                let (tx, rx) = mpsc::unbounded_channel();
                *self.event_tx.lock().unwrap() = tx;
                rx
            }
        }
    }
}

/// ParaformerOnline worker 启动错误。
#[allow(dead_code)] // Handoff 07A: production wiring pending gate
#[derive(Debug, thiserror::Error)]
pub enum ParaformerLaunchError {
    #[error("无法定位 blink.exe: {0}")]
    LocatorFailed(String),
    #[error("deployment 目录无效: {0}")]
    DeploymentInvalid(String),
    #[error("子进程启动失败: {0}")]
    SpawnFailed(String),
    #[error("无法取走 worker stdio 管道")]
    StdioUnavailable,
    #[error("协议错误: {0}")]
    ProtocolError(String),
    #[error("等待 Ready 超时: {0}")]
    ReadyTimeout(String),
    #[error("worker 已中毒: {0}")]
    WorkerPoisoned(String),
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::local_engine::stream_worker_proto::{FakeWorker, FakeWorkerConfig};
    use tokio::io::duplex;

    /// 创建测试 harness。
    #[allow(dead_code)]
    fn para_harness(config: FakeWorkerConfig) -> (Arc<StreamWorkerClient>, Arc<FakeWorker>) {
        let (host_write, _worker_read) = duplex(256 * 1024);
        let (_worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(config));

        (client, worker)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_begin_push_finish_lifecycle() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(FakeWorkerConfig::default()));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        // hello + wait_ready
        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        assert_eq!(session_gen, 1);

        let mut rx = adapter.events();

        // push audio
        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();

        // finish
        adapter.finish_session(session_gen).await.unwrap();

        // 应收到 Final
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { generation, text } => {
                assert_eq!(generation, session_gen);
                assert!(text.contains("final("));
            }
            other => panic!("期望 Final，收到 {other:?}"),
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_cancel_discards_results() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(FakeWorkerConfig::default()));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        let _rx = adapter.events();

        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();

        // cancel
        adapter.cancel_session(session_gen).await.unwrap();

        // begin 新 session
        let gen2 = adapter.begin_session().await.unwrap();
        assert_ne!(session_gen, gen2);

        // start_reader 在 begin_session 中重建了 channel，需要重新获取 rx
        let mut rx = adapter.events();

        adapter.finish_session(gen2).await.unwrap();

        // 只应收到 gen2 的 Final
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { generation, .. } => {
                assert_eq!(generation, gen2);
            }
            other => panic!("期望 Final gen={gen2}，收到 {other:?}"),
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_supports_native_partial() {
        let (host_write, _worker_read) = duplex(256 * 1024);
        let (_worker_write, host_read) = duplex(256 * 1024);
        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let adapter = ParaformerOnlineAdapter::new(client);
        assert!(adapter.supports_native_partial());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_eof_produces_error() {
        // worker 立即断开 → client poison
        let (host_write, worker_read) = duplex(64 * 1024);
        let (worker_write, host_read) = duplex(64 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 立即关闭 worker 端
        drop(worker_read);
        drop(worker_write);

        // 等待 poison
        let _ = client.wait_ready(std::time::Duration::from_secs(2)).await;
        assert!(client.is_poisoned());

        let adapter = ParaformerOnlineAdapter::new(client);

        // begin 应失败（poisoned）
        let result = adapter.begin_session().await;
        assert!(result.is_err());
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_reset_idempotent() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(FakeWorkerConfig::default()));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        // reset 多次
        adapter.reset().await.unwrap();
        adapter.reset().await.unwrap();
        adapter.reset().await.unwrap();

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_busy_event() {
        // 配置 worker 队列容量为 1，不发 Ack
        let config = FakeWorkerConfig {
            queue_capacity: 1,
            ack_audio: false,
            process_delay_ms: 0,
            ..Default::default()
        };
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(config));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        // 快速发送大量音频帧触发 Busy
        for _ in 0..20 {
            let _ = adapter.push_audio(session_gen, &[0.1; 320]).await;
        }

        // finish —— 应能最终完成（或收到 Busy）
        let _ = adapter.finish_session(session_gen).await;

        // 至少应收到一个事件
        let event = rx.recv().await;
        assert!(event.is_some(), "应至少收到一个事件");

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_continuous_pcm() {
        // 验证连续推送 PCM 不死锁
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(FakeWorkerConfig {
            queue_capacity: 64,
            ..Default::default()
        }));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        // 连续推送 50 个音频帧
        for _ in 0..50 {
            adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        }

        adapter.finish_session(session_gen).await.unwrap();

        // 应收到 Final
        let event = rx.recv().await.unwrap();
        match event {
            SttEvent::Final { generation, text } => {
                assert_eq!(generation, session_gen);
                assert!(text.contains("final("));
            }
            other => panic!("期望 Final，收到 {other:?}"),
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn para_multiple_sessions() {
        // 验证连续多条流无死锁
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));
        let worker = Arc::new(FakeWorker::new(FakeWorkerConfig {
            queue_capacity: 64,
            ..Default::default()
        }));

        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;
            worker.run(&mut reader, &mut writer).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());

        // 连续 5 条流
        for i in 0..5u64 {
            let session_gen = adapter.begin_session().await.unwrap();
            let mut rx = adapter.events();

            for _ in 0..5 {
                adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
            }

            adapter.finish_session(session_gen).await.unwrap();

            let event = rx.recv().await.unwrap();
            assert!(
                matches!(event, SttEvent::Final { .. }),
                "流 {} 应收到 Final",
                i
            );
        }

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(10), worker_task).await;
    }

    // ── §A2: 文本契约定向测试 ──────────────────────────────────────────

    /// §A2: Partial fragment 累积语义（0.22.9）——worker 的 Partial 是当前
    /// chunk 的增量 fragment；适配器把之前所有 fragment 固化为 confirmed、
    /// 最新 fragment 作为 preview，与伪流式的"固化 + 候选"分层显示对齐。
    ///
    /// 验证：worker 连发两条 Partial，事件序列应为
    /// `Partial{confirmed:"", preview:"frag one"}` →
    /// `Partial{confirmed:"frag one", preview:"frag two"}`，
    /// 且 confirmed == 之前所有 preview 的拼接。Final 用全会话文本整体替换。
    ///
    /// **时序说明**：两条 Partial 在 worker 收到 Audio#1 后背靠背写入并一次
    /// flush；host 第二次 push_audio（200ms 后）的 `try_recv_partial` 批量
    /// 取出——无论第一条 Partial 被哪次 push 捕获，事件顺序与累积不变量一致。
    #[tokio::test(flavor = "multi_thread")]
    async fn para_partial_fragments_accumulate_into_confirmed_preview() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 自定义 worker：收到 Audio#1 后连发两条 Partial，End 时发 Final
        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;

            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut header_buf = [0u8; 20];
            // 读 Hello → 回 Ready (msg_type = 16)
            let _ = reader.read_exact(&mut header_buf).await;
            let ready = [
                b'B', b'L', b'N', b'K', 2, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ready).await;
            let _ = writer.flush().await;

            // 读 Begin → 回 Ack (msg_type = 19, generation = 1)
            let _ = reader.read_exact(&mut header_buf).await;
            let ack = [
                b'B', b'L', b'N', b'K', 2, 19, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ack).await;
            let _ = writer.flush().await;

            // 读 Audio#1（header + payload）
            let _ = reader.read_exact(&mut header_buf).await;
            let payload_len = u32::from_le_bytes([
                header_buf[16],
                header_buf[17],
                header_buf[18],
                header_buf[19],
            ]) as usize;
            if payload_len > 0 {
                let mut payload = vec![0u8; payload_len];
                let _ = reader.read_exact(&mut payload).await;
            }

            // 背靠背发两条 Partial（msg_type = 17）后一次 flush——
            // 保证两条同时到达 host，避免被 end_stream 消费
            for frag in [b"frag one".as_slice(), b"frag two".as_slice()] {
                let partial_frame = [
                    b'B',
                    b'L',
                    b'N',
                    b'K',
                    2,
                    17,
                    0,
                    0,
                    0,
                    0,
                    0,
                    0,
                    1,
                    0,
                    0,
                    0,
                    frag.len() as u8,
                    0,
                    0,
                    0,
                ];
                let _ = writer.write_all(&partial_frame).await;
                let _ = writer.write_all(frag).await;
            }
            let _ = writer.flush().await;

            // 读 Audio#2、Audio#3（header + payload）
            for _ in 0..2 {
                let _ = reader.read_exact(&mut header_buf).await;
                let payload_len = u32::from_le_bytes([
                    header_buf[16],
                    header_buf[17],
                    header_buf[18],
                    header_buf[19],
                ]) as usize;
                if payload_len > 0 {
                    let mut payload = vec![0u8; payload_len];
                    let _ = reader.read_exact(&mut payload).await;
                }
            }

            // 读 End → 回 Final（msg_type = 18）
            let _ = reader.read_exact(&mut header_buf).await;
            let final_text = b"final text";
            let final_frame = [
                b'B',
                b'L',
                b'N',
                b'K',
                2,
                18,
                0,
                0,
                0,
                0,
                0,
                0,
                1,
                0,
                0,
                0,
                final_text.len() as u8,
                0,
                0,
                0,
            ];
            let _ = writer.write_all(&final_frame).await;
            let _ = writer.write_all(final_text).await;
            let _ = writer.flush().await;

            // 等待 Quit
            let _ = reader.read_exact(&mut header_buf).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        // push#1 触发 worker 发两条 Partial；间隔后的 push#2/#3 批量取出
        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        adapter.push_audio(session_gen, &[0.0; 320]).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        adapter.push_audio(session_gen, &[0.0; 320]).await.unwrap();

        adapter.finish_session(session_gen).await.unwrap();

        let mut partials = Vec::new();
        let mut final_text = String::new();
        while let Ok(Some(event)) =
            tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await
        {
            match event {
                SttEvent::Partial {
                    confirmed, preview, ..
                } => partials.push((confirmed, preview)),
                SttEvent::Final { text, .. } => {
                    final_text = text;
                    break;
                }
                _ => {}
            }
        }

        assert_eq!(
            partials.len(),
            2,
            "应收到两条 Partial 事件: {partials:?}"
        );
        assert_eq!(
            partials[0],
            (String::new(), "frag one".to_string()),
            "首条 Partial：confirmed 为空，preview 为第一个 fragment"
        );
        assert_eq!(
            partials[1],
            ("frag one".to_string(), "frag two".to_string()),
            "次条 Partial：上一 fragment 固化为 confirmed"
        );
        assert!(final_text.contains("final"), "Final 应为全会话文本: {final_text}");

        client.send_quit().await.unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    /// §A2: 推理错误不产生 Final——worker 在 End 时发 Error 而非 Final。
    ///
    /// 用自定义 worker 在 End 时直接发 Error 帧，验证 adapter
    /// 产出 SttEvent::Error 而非 SttEvent::Final。
    #[tokio::test(flavor = "multi_thread")]
    async fn para_inference_error_does_not_produce_final() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 自定义 worker：End 时发 Error 而非 Final
        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;

            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut header_buf = [0u8; 20];
            // 读 Hello
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Ready (msg_type = 16 = Ready)
            let ready = [
                b'B', b'L', b'N', b'K', 2, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ready).await;
            let _ = writer.flush().await;

            // 读 Begin
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Ack (msg_type = 19 = Ack)
            let ack = [
                b'B', b'L', b'N', b'K', 2, 19, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ack).await;
            let _ = writer.flush().await;

            // 读 Audio（header + payload）
            let _ = reader.read_exact(&mut header_buf).await;
            let payload_len = u32::from_le_bytes([
                header_buf[16],
                header_buf[17],
                header_buf[18],
                header_buf[19],
            ]) as usize;
            // 读并丢弃 payload
            if payload_len > 0 {
                let mut payload = vec![0u8; payload_len];
                let _ = reader.read_exact(&mut payload).await;
            }

            // 读 End
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Error (msg_type = 21 = Error, 模拟推理错误)
            let error_payload = b"forward panic";
            let error_frame = [
                b'B',
                b'L',
                b'N',
                b'K',
                2,  // version
                21, // msg_type = Error
                0,
                0,
                0,
                0,
                0,
                0,
                1,
                0,
                0,
                0, // generation = 1
                error_payload.len() as u8,
                0,
                0,
                0,
            ];
            let _ = writer.write_all(&error_frame).await;
            let _ = writer.write_all(error_payload).await;
            let _ = writer.flush().await;

            // 等待 Quit
            let _ = reader.read_exact(&mut header_buf).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();

        // finish_session 内部会等待 Final，但 worker 发了 Error
        // adapter 应产出 Error 事件而非 Final
        let _ = adapter.finish_session(session_gen).await;

        // 应收到 Error 事件，不应收到 Final
        let event = tokio::time::timeout(std::time::Duration::from_secs(5), rx.recv()).await;
        assert!(event.is_ok(), "应收到事件");
        if let Ok(Some(ev)) = event {
            match ev {
                SttEvent::Error { message, .. } => {
                    assert!(!message.is_empty(), "Error 消息不应为空");
                }
                SttEvent::Final { .. } => {
                    panic!("推理错误不应产生 Final");
                }
                _ => {}
            }
        }

        client.send_quit().await.ok();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }

    /// §A2: 空 Final 被正确传播——worker 发空 Final 文本时，
    /// adapter 应产出 Final { text: "" }，而非省略事件。
    ///
    /// **根因**：自定义 worker 只读 20 字节 header 但不读 Audio payload，
    /// 导致后续读取错位（End 帧读到的是 Audio payload 的字节）。
    /// 修复：读取 header 后根据 payload_len 读取并丢弃 payload。
    #[tokio::test(flavor = "multi_thread")]
    async fn para_empty_final_is_propagated() {
        let (host_write, worker_read) = duplex(256 * 1024);
        let (worker_write, host_read) = duplex(256 * 1024);

        let client = StreamWorkerClient::new(Box::new(host_write), Box::new(host_read));

        // 自定义 worker：End 时发空 Final
        let worker_task = tokio::spawn(async move {
            let mut reader = worker_read;
            let mut writer = worker_write;

            use tokio::io::{AsyncReadExt, AsyncWriteExt};

            let mut header_buf = [0u8; 20];
            // 读 Hello
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Ready (msg_type = 16 = Ready)
            let ready = [
                b'B', b'L', b'N', b'K', 2, 16, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ready).await;
            let _ = writer.flush().await;

            // 读 Begin
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Ack (msg_type = 19 = Ack)
            let ack = [
                b'B', b'L', b'N', b'K', 2, 19, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&ack).await;
            let _ = writer.flush().await;

            // 读 Audio（header + payload）
            let _ = reader.read_exact(&mut header_buf).await;
            let payload_len = u32::from_le_bytes([
                header_buf[16],
                header_buf[17],
                header_buf[18],
                header_buf[19],
            ]) as usize;
            // 读并丢弃 payload
            if payload_len > 0 {
                let mut payload = vec![0u8; payload_len];
                let _ = reader.read_exact(&mut payload).await;
            }

            // 读 End
            let _ = reader.read_exact(&mut header_buf).await;
            // 回 Final（空文本, msg_type = 18 = Final, payload_len = 0）
            let final_frame = [
                b'B', b'L', b'N', b'K', 2, 18, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0,
            ];
            let _ = writer.write_all(&final_frame).await;
            let _ = writer.flush().await;

            // 等待 Quit
            let _ = reader.read_exact(&mut header_buf).await;
        });

        client.send_hello().await.unwrap();
        client
            .wait_ready(std::time::Duration::from_secs(5))
            .await
            .unwrap();

        let adapter = ParaformerOnlineAdapter::new(client.clone());
        let session_gen = adapter.begin_session().await.unwrap();
        let mut rx = adapter.events();

        adapter.push_audio(session_gen, &[0.1; 320]).await.unwrap();
        adapter.finish_session(session_gen).await.unwrap();

        // 应收到 Final { text: "" }
        let event = rx.recv().await;
        assert!(event.is_some(), "应收到事件");
        match event.unwrap() {
            SttEvent::Final { text, .. } => {
                assert_eq!(
                    text, "",
                    "空 Final 应传播为空字符串，而非被替换为 '(empty)'"
                );
            }
            other => panic!("期望 Final，收到 {other:?}"),
        }

        client.send_quit().await.ok();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), worker_task).await;
    }
}
