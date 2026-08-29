//! 模型安装执行器与目录（从原 ModelService 拆分）。
//!
//! ModelService 已删除——模型资产业务编排（状态、事务、冲突检查、
//! selected/active 投影）统一由 `EngineManager` 承载（单一业务真相）。
//! 本模块只保留：
//! - `ModelRegistry`：编译期模型目录（allowlist）；
//! - `ModelInstallWorker`：下载执行器 trait + FunASR 实现（受管 venv python 驱动）；
//! - 安装 sink：有界缓冲 + 事件广播（operation_id 隔离）；
//! - 模型 DTO 与投影（commands 层使用）。
//!
//! 持久真源是磁盘 manifest（infra model_storage）；本模块无状态。

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;

use crate::domain::local_engine::{
    EngineModelDescriptor, EngineModelStatus, LocalEngineErrorCode, ModelOperationResult,
};
use crate::infra::local_engine::runtime::EngineId;

// ── ModelRegistry ──────────────────────────────────────────────────────────

/// 编译期模型注册表（allowlist）。
///
/// 每个引擎在编译期声明自己支持的模型候选列表。
/// 不暴露动态注册 API——所有注册项在构造时确定。
pub struct ModelRegistry {
    /// engine_id → 模型 descriptor 列表
    models: HashMap<EngineId, Vec<EngineModelDescriptor>>,
}

impl Clone for ModelRegistry {
    fn clone(&self) -> Self {
        Self {
            models: self.models.clone(),
        }
    }
}

impl ModelRegistry {
    /// 创建带指定模型列表的注册表。
    pub fn new_with_models(models: Vec<EngineModelDescriptor>) -> Self {
        let mut map: HashMap<EngineId, Vec<EngineModelDescriptor>> = HashMap::new();
        for m in models {
            map.entry(m.engine_id.clone()).or_default().push(m);
        }
        Self { models: map }
    }

    /// 创建空注册表（测试用）。
    pub fn empty() -> Self {
        Self {
            models: HashMap::new(),
        }
    }

    /// 查找引擎的所有模型候选。
    pub fn list(&self, engine_id: &EngineId) -> &[EngineModelDescriptor] {
        self.models
            .get(engine_id)
            .map(|v| v.as_slice())
            .unwrap_or(&[])
    }

    /// 查找特定模型。
    pub fn find(&self, engine_id: &EngineId, model_id: &str) -> Option<&EngineModelDescriptor> {
        self.models
            .get(engine_id)?
            .iter()
            .find(|m| m.model_id == model_id)
    }
}

// ── InstallSink（有界日志/阶段 sink）──────────────────────────────────────

/// 模型安装阶段的日志/进度 sink。
///
/// **铁则**：
/// - 有界：实现必须维护有界缓冲，禁止无限制累积日志。
/// - 阶段性：`emit_stage` 报告安装阶段（如 downloading/verifying），
///   但**不伪造下载百分比**——无法取得字节级进度时只报阶段。
/// - 不接收 URL、executable、argv、环境变量或脚本路径。
pub trait InstallSink: Send + Sync {
    /// 发射一条日志行。
    fn emit_log(&self, line: &str);

    /// 发射阶段变更。
    fn emit_stage(&self, stage: &str);
}

/// 有界内存日志 sink（用于测试和轻量诊断）。
///
/// 缓冲上限为 `max_lines`，超出后丢弃旧行。
pub struct BoundedInstallSink {
    lines: std::sync::Mutex<std::collections::VecDeque<String>>,
    max_lines: usize,
}

impl BoundedInstallSink {
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: std::sync::Mutex::new(std::collections::VecDeque::with_capacity(max_lines)),
            max_lines,
        }
    }
}

impl InstallSink for BoundedInstallSink {
    fn emit_log(&self, line: &str) {
        let mut buf = self.lines.lock().unwrap();
        if buf.len() >= self.max_lines {
            buf.pop_front();
        }
        buf.push_back(line.to_string());
    }

    fn emit_stage(&self, stage: &str) {
        self.emit_log(&format!("[stage] {stage}"));
    }
}

impl BoundedInstallSink {
    /// 取缓冲尾部 n 行（用于失败时把 installer 真实输出附进错误详情）。
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        let buf = self.lines.lock().unwrap();
        buf.iter().rev().take(n).rev().cloned().collect()
    }
}

/// 模型安装日志广播 sink——把 installer 输出桥接到 `EventPort`（前端实时事件）
/// 并缓冲到内部 `BoundedInstallSink`（失败诊断用）。
///
/// 与 service.rs 的 `InstallSinkAdapter`（引擎环境安装）语义一致：
/// - `emit_install_log` 以 `operation_id` 隔离，`instance_id` 为空，
///   前端按 `operation_id != null` 识别为操作日志（不做 instance 过滤）；
/// - installer 原始输出默认 debug 级 tracing，`[ERROR]`/`[WARN]` 前缀升级；
/// - 洪泛保护由内部缓冲上限与 installer 侧 `disable_progress_bar` 共同保证。
pub struct BroadcastingInstallSink {
    inner: BoundedInstallSink,
    event_port: Arc<dyn super::EventPort>,
    engine_id: EngineId,
    operation_id: String,
    log_seq: std::sync::atomic::AtomicU64,
}

impl BroadcastingInstallSink {
    pub fn new(
        inner: BoundedInstallSink,
        event_port: Arc<dyn super::EventPort>,
        engine_id: EngineId,
        operation_id: String,
    ) -> Self {
        Self {
            inner,
            event_port,
            engine_id,
            operation_id,
            log_seq: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// 取缓冲尾部 n 行（透传内部缓冲）。
    pub fn tail_lines(&self, n: usize) -> Vec<String> {
        self.inner.tail_lines(n)
    }
}

/// 从 installer 输出行推断日志级别。
///
/// stdout/stderr 只是传输通道：受信任 installer 的显式前缀优先，
/// 未分类输出归 info（前端展示）+ debug（tracing）。
fn classify_installer_line(line: &str) -> crate::app::local_engine::dto::EngineLogLevel {
    use crate::app::local_engine::dto::EngineLogLevel;
    if line.starts_with("[ERROR]") {
        EngineLogLevel::Error
    } else if line.starts_with("[WARN") || line.starts_with("WARNING") {
        EngineLogLevel::Warn
    } else {
        EngineLogLevel::Info
    }
}

impl InstallSink for BroadcastingInstallSink {
    fn emit_log(&self, line: &str) {
        self.inner.emit_log(line);

        let level = classify_installer_line(line);
        match level {
            crate::app::local_engine::dto::EngineLogLevel::Error => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            crate::app::local_engine::dto::EngineLogLevel::Warn => tracing::warn!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
            _ => tracing::debug!(
                engine_id = %self.engine_id,
                op = %self.operation_id,
                output = line,
                "模型 installer 输出"
            ),
        }

        let seq = self
            .log_seq
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            + 1;
        self.event_port
            .emit_install_log(&self.engine_id, &self.operation_id, seq, level, line);
    }

    fn emit_stage(&self, stage: &str) {
        self.inner.emit_stage(stage);
        tracing::debug!(
            engine_id = %self.engine_id,
            op = %self.operation_id,
            stage,
            "模型安装阶段变更"
        );
        self.event_port
            .emit_install_stage(&self.engine_id, &self.operation_id, stage);
    }
}

// ── ModelInstallWorker trait ────────────────────────────────────────────────

/// 模型安装 worker trait（installer port）。
///
/// 每个引擎 adapter 提供编译期固定的专用安装 worker。
/// worker 负责实际的模型下载（如 ModelScope/FunASR 官方库），
/// 下载结果写入指定的 staging payload 目录。
///
/// **铁则**：
/// - worker 只负责下载到 staging，不负责校验/提升/manifest
/// - worker 不接收前端提交的 URL、脚本路径、Python 路径
/// - worker 参数必须是 allowlist 中的 model id/revision
/// - worker 设置 MODELSCOPE_CACHE 为本次 staging 目录，禁止回落到用户默认缓存
/// - worker 必须作为受管进程运行，接入 CancellationToken 和超时
/// - worker 通过 `InstallSink` 报告有界日志和阶段，不伪造百分比
#[async_trait::async_trait]
pub trait ModelInstallWorker: Send + Sync {
    /// 下载模型到 staging payload 目录。
    ///
    /// 成功时返回下载来源描述（用于 manifest 的 source/provenance）。
    /// 失败时返回错误（Rust 会清理 staging）。
    ///
    /// `sink` 可选——worker 通过 sink 报告有界日志和安装阶段。
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError>;
}

/// 模型下载结果（worker 返回）。
#[derive(Debug, Clone)]
pub struct ModelDownloadOutcome {
    /// 下载来源描述（如 "modelscope:iic/SenseVoiceSmall"）。
    pub source: String,
    /// 下载来源的 checksum 信息。
    pub checksum_source: ModelDownloadChecksumSource,
}

/// 下载来源 checksum 信息。
#[derive(Debug, Clone)]
pub enum ModelDownloadChecksumSource {
    /// 上游不提供稳定 checksum。
    Unverified,
    /// 上游提供稳定 SHA-256（worker 契约保留：上游开始提供时无需改 trait）。
    #[allow(dead_code)]
    Sha256(String),
}

/// 模型下载错误。
#[derive(Debug, thiserror::Error)]
pub enum ModelDownloadError {
    #[error("下载失败: {message}")]
    Failed { message: String },

    #[error("下载被取消")]
    Cancelled,

    #[error("下载超时")]
    TimedOut,

    /// worker 契约保留：installer 可区分磁盘/网络失败时无需改 trait。
    #[error("磁盘空间不足: {message}")]
    #[allow(dead_code)]
    DiskFull { message: String },

    #[error("网络不可达: {message}")]
    #[allow(dead_code)]
    Network { message: String },

    #[error("worker 内部错误: {message}")]
    Internal { message: String },
}

impl ModelDownloadError {
    /// 映射到 LocalEngineErrorCode。
    pub fn to_code(&self) -> LocalEngineErrorCode {
        match self {
            Self::Cancelled => LocalEngineErrorCode::Cancelled,
            Self::TimedOut => LocalEngineErrorCode::Timeout,
            Self::DiskFull { .. } => LocalEngineErrorCode::DiskFull,
            Self::Network { .. } => LocalEngineErrorCode::NetworkError,
            Self::Failed { .. } | Self::Internal { .. } => LocalEngineErrorCode::InstallFailed,
        }
    }
}

pub struct NoopModelWorker;

#[async_trait::async_trait]
impl ModelInstallWorker for NoopModelWorker {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        _model_id: &str,
        _revision: &str,
        _staging_payload_dir: &std::path::Path,
        _cancel_token: CancellationToken,
        _sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        Err(ModelDownloadError::Internal {
            message: "NoopModelWorker: 模型下载未实现（等待 B2 FunASR worker）".to_string(),
        })
    }
}

// ── FunasrModelInstallWorker ───────────────────────────────────────────────

/// 嵌入的 blink_model_installer.py 脚本（随 Rust 二进制发布）。
const BLINK_MODEL_INSTALLER_PY: &str =
    include_str!("../../../resources/stt/funasr/blink_model_installer.py");

/// FunASR 专用模型安装 worker（B2）。
///
/// 使用 active deployment venv 中的 Python 运行 `blink_model_installer.py`，
/// 通过 ModelScope 官方库下载模型到 staging payload 目录。
///
/// **铁则**：
/// - 只使用 active deployment venv 中的 Python
/// - 只接受编译期 allowlist 中的 model id/revision
/// - Rust adapter 将 canonical model id 映射为固定 worker 参数
/// - 前端和通用 command 不得提供 URL、Python 路径、脚本路径或环境变量
/// - MODELSCOPE_CACHE 指向本次 staging payload 目录
/// - staging 目录创建失败必须 fail closed
/// - 禁止回落到用户 ~/.cache/modelscope
/// - stdout/stderr 实时进入 operation 日志
/// - worker 由受管进程运行，接入 Job Object、CancellationToken 和超时
/// - 取消/超时后 worker 及其子进程全部退出
/// - worker 成功只代表下载完成；最终 fingerprint、manifest 与 promote 由 Rust 执行
pub struct FunasrModelInstallWorker {
    /// 下载超时（秒），0 = 无超时。
    timeout_secs: u64,
}

impl FunasrModelInstallWorker {
    /// 创建默认 worker（超时 600s = 10min，模型下载可能较慢）。
    pub fn new() -> Self {
        Self { timeout_secs: 600 }
    }

    /// 创建带自定义超时的 worker。
    #[allow(dead_code)]
    pub fn with_timeout(secs: u64) -> Self {
        Self { timeout_secs: secs }
    }

    /// 释放 installer 脚本到 python_dir。
    fn ensure_installer_script() -> Result<std::path::PathBuf, String> {
        let dir = crate::infra::utils::paths::python_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("创建 python 目录失败: {e}"))?;
        let script_path = dir.join("blink_model_installer.py");
        let need_write = match std::fs::read_to_string(&script_path) {
            Ok(existing) => existing != BLINK_MODEL_INSTALLER_PY,
            Err(_) => true,
        };
        if need_write {
            std::fs::write(&script_path, BLINK_MODEL_INSTALLER_PY)
                .map_err(|e| format!("写入 blink_model_installer.py 失败: {e}"))?;
        }
        Ok(script_path)
    }

    /// 查找 active deployment venv 中的 python.exe。
    fn find_active_deployment_python() -> Option<std::path::PathBuf> {
        let engine_id = EngineId::new("funasr").ok()?;
        // active 部署（slot + pointer）中的 venv python
        let (_pointer, dir) =
            crate::infra::local_engine::deployment::DeploymentStore::active_dir(&engine_id)
                .ok()
                .flatten()?;
        let python_exe = dir.join("venv").join("Scripts").join("python.exe");
        if python_exe.exists() {
            Some(python_exe)
        } else {
            None
        }
    }
}

impl Default for FunasrModelInstallWorker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl ModelInstallWorker for FunasrModelInstallWorker {
    async fn download_to_staging(
        &self,
        engine_id: &EngineId,
        model_id: &str,
        revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        // 1. 查找 active deployment venv 中的 Python
        let python =
            Self::find_active_deployment_python().ok_or_else(|| ModelDownloadError::Internal {
                message: "FunASR active deployment venv 未安装——请先安装环境".to_string(),
            })?;

        // 2. 释放 installer 脚本
        let script_path =
            Self::ensure_installer_script().map_err(|e| ModelDownloadError::Internal {
                message: format!("释放 installer 脚本失败: {e}"),
            })?;

        // 3. 确保 staging payload 目录存在（fail closed）
        std::fs::create_dir_all(staging_payload_dir).map_err(|e| ModelDownloadError::Internal {
            message: format!("staging 目录创建失败: {e}"),
        })?;

        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloading");
            s.emit_log(&format!(
                "开始下载模型 {model_id} (revision={revision}) 到 {staging_payload_dir:?}"
            ));
        }

        // 4. 构建启动命令
        let mut cmd = tokio::process::Command::new(&python);
        cmd.arg(&script_path)
            .arg("--model")
            .arg(model_id)
            .arg("--revision")
            .arg(revision)
            .arg("--staging-dir")
            .arg(staging_payload_dir)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .stdin(std::process::Stdio::null());

        // 设置环境变量：禁止回落到用户默认缓存
        cmd.env("MODELSCOPE_CACHE", staging_payload_dir.as_os_str());
        // Python 无缓冲 + UTF-8
        cmd.env("PYTHONUNBUFFERED", "1");
        cmd.env("PYTHONUTF8", "1");
        cmd.env("PYTHONIOENCODING", "utf-8");

        // CREATE_NO_WINDOW
        cmd = crate::infra::platform::no_window_tokio(cmd);

        // 5. 启动子进程
        let mut child = cmd.spawn().map_err(|e| ModelDownloadError::Internal {
            message: format!("启动 installer 进程失败: {e}"),
        })?;

        let pid = child.id().unwrap_or(0);

        // 5a. 分配 Job Object（Windows 进程树回收）
        //
        // **铁则**：installer 进程必须进入 Job Object，确保取消/超时/Blink 退出时
        // 整个进程树（包括 pip 子进程）全部被回收。
        // Job handle 在 wait 完成后 drop，触发 KILL_ON_JOB_CLOSE。
        #[cfg(windows)]
        let job_handle = match crate::infra::platform::process::assign_job_object(pid) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(%e, pid, "installer Job Object 分配失败，终止子进程");
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(ModelDownloadError::Internal {
                    message: format!("Job Object 分配失败: {e}"),
                });
            }
        };

        tracing::info!(
            engine_id = %engine_id,
            model_id = %model_id,
            pid,
            "FunASR model installer 进程已启动"
        );

        if let Some(s) = sink.as_deref() {
            s.emit_log(&format!("installer 进程已启动 (pid={pid})"));
        }

        // 6. 并发排空 stdout/stderr 管道（防止背压死锁）
        //
        // **铁则**：必须在 wait 之前启动管道排空 task。
        // 如果 wait 先完成再读管道，子进程 stdout/stderr 缓冲区满后会阻塞，
        // 导致 child.wait() 永不返回（死锁）。
        //
        // 排空 task 逐行将输出实时送入 sink，不等待进程退出。
        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let stdout_task = if let Some(stdout) = stdout {
            let sink_ref = sink.clone();
            Some(tokio::spawn(async move {
                pump_pipe_to_sink(stdout, sink_ref).await;
            }))
        } else {
            None
        };

        let stderr_task = if let Some(stderr) = stderr {
            let sink_ref = sink.clone();
            Some(tokio::spawn(async move {
                pump_pipe_to_sink(stderr, sink_ref).await;
            }))
        } else {
            None
        };

        // 7. 等待进程完成，带超时和取消
        let timeout = std::time::Duration::from_secs(self.timeout_secs);

        let wait_result = tokio::select! {
            result = child.wait() => {
                result.map_err(|e| ModelDownloadError::Internal {
                    message: format!("等待 installer 进程失败: {e}"),
                })?
            }
            _ = tokio::time::sleep(timeout) => {
                // 超时——kill 进程并等待退出
                tracing::warn!(pid, "FunASR model installer 超时，终止进程");
                let _ = child.start_kill();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                ).await;

                // drop Job handle 触发 KILL_ON_JOB_CLOSE（进程树回收）
                #[cfg(windows)]
                drop(job_handle);

                // 等待管道排空 task 完成
                if let Some(t) = stdout_task { let _ = t.await; }
                if let Some(t) = stderr_task { let _ = t.await; }

                return Err(ModelDownloadError::TimedOut);
            }
            _ = cancel_token.cancelled() => {
                // 取消——kill 进程并等待退出
                tracing::info!(pid, "FunASR model installer 被取消，终止进程");
                let _ = child.start_kill();
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                ).await;

                #[cfg(windows)]
                drop(job_handle);

                if let Some(t) = stdout_task { let _ = t.await; }
                if let Some(t) = stderr_task { let _ = t.await; }

                return Err(ModelDownloadError::Cancelled);
            }
        };

        // 8. 等待管道排空 task 完成（进程已退出，管道即将 EOF）
        if let Some(t) = stdout_task {
            let _ = t.await;
        }
        if let Some(t) = stderr_task {
            let _ = t.await;
        }

        // 9. drop Job handle（进程树最终回收保障）
        #[cfg(windows)]
        drop(job_handle);

        // 10. 检查退出码
        let output = wait_result;
        let code = output.code().unwrap_or(-1);
        if !output.success() {
            if cancel_token.is_cancelled() {
                return Err(ModelDownloadError::Cancelled);
            }
            return Err(ModelDownloadError::Failed {
                message: format!("installer 进程退出码 {code}"),
            });
        }

        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloaded");
            s.emit_log(&format!("模型 {model_id} 下载完成 (exit_code={code})"));
        }

        // 11. 验证 staging 目录非空
        if !staging_payload_dir.exists()
            || std::fs::read_dir(staging_payload_dir)
                .map(|mut d| d.next().is_none())
                .unwrap_or(true)
        {
            return Err(ModelDownloadError::Failed {
                message: "下载完成但 staging 目录为空".to_string(),
            });
        }

        Ok(ModelDownloadOutcome {
            source: format!("modelscope:{model_id}"),
            checksum_source: ModelDownloadChecksumSource::Unverified,
        })
    }
}

/// 并发排空子进程管道，逐行送入有界 sink。
///
/// **铁则**：
/// - 必须在 child.wait() 之前启动，防止 stdout/stderr 缓冲区满后死锁。
/// - 逐行读取（LineAccumulator），不使用 read_until（无界增长）。
/// - 单行最大字节数 8KB，超出截断。
/// - 实时送入 sink，不等待进程退出。
async fn pump_pipe_to_sink<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    sink: Option<Arc<dyn InstallSink>>,
) {
    use crate::infra::local_engine::log_pipe::LineAccumulator;
    use tokio::io::AsyncReadExt;

    let mut acc = LineAccumulator::new(8192);
    let mut read_buf = vec![0u8; 8192];

    loop {
        match reader.read(&mut read_buf).await {
            Ok(0) => {
                // EOF——flush 残留
                if let Some((text, _truncated)) = acc.finish() {
                    if !text.is_empty() {
                        if let Some(s) = sink.as_deref() {
                            s.emit_log(&text);
                        }
                    }
                }
                break;
            }
            Ok(n) => {
                let lines = acc.push_data(&read_buf[..n]);
                for (text, _truncated) in lines {
                    if let Some(s) = sink.as_deref() {
                        s.emit_log(&text);
                    }
                }
            }
            Err(e) => {
                tracing::debug!(%e, "pump_pipe_to_sink: pipe read error");
                if let Some((text, _truncated)) = acc.finish() {
                    if !text.is_empty() {
                        if let Some(s) = sink.as_deref() {
                            s.emit_log(&text);
                        }
                    }
                }
                break;
            }
        }
    }
}

// ── FakeInstaller ───────────────────────────────────────────────────────────

/// 可注入的假模型安装 worker（测试用）。
///
/// **能力**：
/// - 成功写入固定 payload（可自定义内容）
/// - 可阻塞并响应取消
/// - 可注入下载失败
/// - 可注入校验失败（写入空文件或损坏内容模拟 fingerprint 不匹配）
/// - 可生成不同 revision/content（用于 repair 测试）
/// - 通过 sink 报告阶段日志
#[cfg(test)]
pub struct FakeInstaller {
    /// 是否成功下载。
    pub success: bool,
    /// 下载延迟（毫秒），>0 时模拟可取消的下载。
    pub delay_ms: u64,
    /// 写入 staging 的文件内容。
    pub file_content: Vec<u8>,
    /// 写入的文件名（默认 `model.bin`）。
    pub file_name: String,
    /// 下载来源描述。
    pub source: Option<String>,
    /// 下载 checksum 来源。
    pub checksum_source: Option<ModelDownloadChecksumSource>,
}

#[cfg(test)]
impl FakeInstaller {
    /// 创建成功写入固定 payload 的 installer。
    pub fn success() -> Self {
        Self {
            success: true,
            delay_ms: 0,
            file_content: b"fake model data".to_vec(),
            file_name: "model.bin".to_string(),
            source: None,
            checksum_source: None,
        }
    }
}

#[cfg(test)]
#[async_trait::async_trait]
impl ModelInstallWorker for FakeInstaller {
    async fn download_to_staging(
        &self,
        _engine_id: &EngineId,
        model_id: &str,
        _revision: &str,
        staging_payload_dir: &std::path::Path,
        cancel_token: CancellationToken,
        sink: Option<Arc<dyn InstallSink>>,
    ) -> Result<ModelDownloadOutcome, ModelDownloadError> {
        if let Some(s) = sink.as_deref() {
            s.emit_stage("downloading");
            s.emit_log(&format!("开始下载模型 {model_id}"));
        }

        if self.delay_ms > 0 {
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_millis(self.delay_ms)) => {}
                _ = cancel_token.cancelled() => {
                    return Err(ModelDownloadError::Cancelled);
                }
            }
        }

        if cancel_token.is_cancelled() {
            return Err(ModelDownloadError::Cancelled);
        }

        if !self.success {
            return Err(ModelDownloadError::Failed {
                message: "fake download failure".to_string(),
            });
        }

        if let Some(s) = sink.as_deref() {
            s.emit_stage("writing");
            s.emit_log("写入 payload 文件");
        }

        let file_name = if self.file_name.is_empty() {
            "model.bin"
        } else {
            self.file_name.as_str()
        };
        std::fs::write(staging_payload_dir.join(file_name), &self.file_content).map_err(|e| {
            ModelDownloadError::Internal {
                message: e.to_string(),
            }
        })?;

        Ok(ModelDownloadOutcome {
            source: self
                .source
                .clone()
                .unwrap_or_else(|| format!("fake:{model_id}")),
            checksum_source: self
                .checksum_source
                .clone()
                .unwrap_or_else(|| ModelDownloadChecksumSource::Unverified),
        })
    }
}

// ── DTO ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCatalogItemDto {
    pub engine_id: String,
    pub model_id: String,
    pub display_name: String,
    pub description: String,
    pub revision: String,
    pub estimated_size_mb: Option<u64>,
    pub install_state: String,
    pub verification_state: String,
    pub cache_size_bytes: Option<u64>,
    pub is_selected: bool,
    pub is_active: bool,
    pub compatibility: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelOperationRequestDto {
    pub engine_id: String,
    pub model_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelOperationResultDto {
    pub engine_id: String,
    pub model_id: String,
    pub operation_id: String,
    pub operation_kind: String,
    pub final_stage: String,
    pub success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<serde_json::Value>,
}

pub fn project_model_status(
    descriptor: &EngineModelDescriptor,
    status: &EngineModelStatus,
) -> ModelCatalogItemDto {
    ModelCatalogItemDto {
        engine_id: descriptor.engine_id.to_string(),
        model_id: descriptor.model_id.clone(),
        display_name: descriptor.display_name.clone(),
        description: descriptor.description.clone(),
        revision: descriptor.revision.clone(),
        estimated_size_mb: descriptor.estimated_size_mb,
        install_state: status.install_state.to_string(),
        verification_state: status.verification_state.to_string(),
        cache_size_bytes: status.cache_size_bytes,
        is_selected: status.is_selected,
        is_active: status.is_active,
        compatibility: status.compatibility.to_string(),
    }
}

pub fn project_model_operation_result(result: &ModelOperationResult) -> ModelOperationResultDto {
    ModelOperationResultDto {
        engine_id: result.engine_id.clone(),
        model_id: result.model_id.clone(),
        operation_id: result.operation_id.clone(),
        operation_kind: result.operation_kind.to_string(),
        final_stage: result.final_stage.to_string(),
        success: result.success,
        error: result
            .error
            .as_ref()
            .map(|e| serde_json::to_value(e).unwrap_or(serde_json::Value::Null)),
    }
}

// ── FunASR 注册 ────────────────────────────────────────────────────────────

pub fn make_funasr_model_registry() -> ModelRegistry {
    ModelRegistry::new_with_models(vec![
        EngineModelDescriptor::sensevoice_small(),
        EngineModelDescriptor::paraformer_zh(),
    ])
}
