//! FunASR Python 环境管理 + server 生命周期。
//!
//! ## 设计
//!
//! 不再使用 sherpa-onnx 二进制 + ONNX 模型，改为直接使用 FunASR Python 工具箱。
//! FunASR 提供 `funasr-server` 命令，启动 OpenAI 兼容的 API 服务。
//!
//! 本模块负责：
//! 1. 通过 [`infra::platform::python`] 模块管理 uv + venv + funasr 安装
//! 2. 启动 / 停止 funasr-server 子进程（使用 venv 中的 Python）
//! 3. 健康检查（确认服务就绪）
//!
//! ## uv 自管理环境
//!
//! Blink 通过 uv 创建独立的 Python 虚拟环境（Python 3.12），用户无需手动安装
//! Python 或 pip 包。环境位于 `%APPDATA%\blink\python\venv\`。
//!
//! 详见 [`crate::infra::platform::python`] 模块文档。
//!
//! ## FunASR 与 sherpa-onnx 的关系（历史）
//!
//! 旧方案：sherpa-onnx（C++ ONNX 引擎）+ 第三方 ONNX 模型转换（csukuangfj on HuggingFace）
//! 新方案：FunASR（Python 工具箱）原生推理 + OpenAI 兼容 API
//!
//! 新方案优势：
//! - FunASR 自动从 ModelScope 下载模型（国内 CDN，稳定）
//! - 内置 VAD + 标点恢复 + 说话人分离 pipeline
//! - OpenAI 兼容 API = 复用现有 CloudSttEngine HTTP 代码
//! - 无需管理 ONNX 模型文件、二进制版本
//! - **uv 自管理环境**：用户零手动安装，Blink 全自动管理 Python 依赖

use std::sync::atomic::{AtomicBool, Ordering};

/// FunASR 模型 ID（对应 funasr-server --model 参数）。
pub const DEFAULT_MODEL: &str = "sensevoice";

/// 默认监听端口。
pub const DEFAULT_PORT: u16 = 8000;

/// funasr-server 启动超时（秒）。
/// 首次启动需要从 ModelScope 下载模型（~234MB），加上 PyTorch 加载，
/// 可能需要 3-5 分钟。后续启动仅模型加载，通常 30-60 秒。
pub const SERVER_STARTUP_TIMEOUT_SECS: u64 = 300;

/// 全局 server 进程句柄（由 LocalSttEngine 管理）。
static SERVER_RUNNING: AtomicBool = AtomicBool::new(false);

/// FunASR 环境 + 服务完整状态。
///
/// 聚合了 Python 环境状态（uv/venv/funasr）和 funasr-server 运行状态，
/// 供前端展示和诊断使用。
#[derive(Debug, Clone, serde::Serialize)]
pub struct FunasrEnv {
    // ── Python 环境（来自 python 模块） ──
    /// uv 是否可用
    pub uv_available: bool,
    /// uv 版本
    pub uv_version: Option<String>,
    /// venv 是否已创建
    pub venv_exists: bool,
    /// venv Python 版本
    pub venv_python_version: Option<String>,
    /// torch（PyTorch）是否已安装
    pub torch_installed: bool,
    /// torch 版本
    pub torch_version: Option<String>,
    /// funasr 包是否已安装
    pub funasr_installed: bool,
    /// funasr 版本
    pub funasr_version: Option<String>,
    /// Python 环境是否完全就绪
    pub env_ready: bool,

    // ── funasr-server 状态 ──
    /// funasr-server 是否正在运行
    pub server_running: bool,
    /// 当前配置的监听端口
    pub server_port: u16,
    /// 当前配置的模型
    pub server_model: String,
}

/// 获取 FunASR 环境 + 服务的完整状态（同步版，会阻塞调用线程）。
///
/// 仅用于测试和诊断。生产代码应使用 [`get_env_status_async`]。
#[allow(dead_code)]
pub fn get_env_status(server_port: u16, server_model: &str) -> FunasrEnv {
    let py_status = crate::infra::platform::python::check_status();

    FunasrEnv {
        uv_available: py_status.uv_available,
        uv_version: py_status.uv_version,
        venv_exists: py_status.venv_exists,
        venv_python_version: py_status.venv_python_version,
        torch_installed: py_status.torch_installed,
        torch_version: py_status.torch_version,
        funasr_installed: py_status.funasr_installed,
        funasr_version: py_status.funasr_version,
        env_ready: py_status.env_ready,
        server_running: SERVER_RUNNING.load(Ordering::SeqCst),
        server_port,
        server_model: server_model.to_string(),
    }
}

/// 获取 FunASR 环境 + 服务的完整状态（异步版，不阻塞 async 运行时）。
///
/// 将 Python 子进程检测放到 `spawn_blocking` 线程池执行。
/// 适用于 Tauri async 命令中调用，避免阻塞 UI 线程。
pub async fn get_env_status_async(server_port: u16, server_model: String) -> FunasrEnv {
    let py_status = crate::infra::platform::python::check_status_async().await;

    FunasrEnv {
        uv_available: py_status.uv_available,
        uv_version: py_status.uv_version,
        venv_exists: py_status.venv_exists,
        venv_python_version: py_status.venv_python_version,
        torch_installed: py_status.torch_installed,
        torch_version: py_status.torch_version,
        funasr_installed: py_status.funasr_installed,
        funasr_version: py_status.funasr_version,
        env_ready: py_status.env_ready,
        server_running: SERVER_RUNNING.load(Ordering::SeqCst),
        server_port,
        server_model,
    }
}

/// 检查 funasr-server 是否在指定端口上响应。
///
/// 通过 TCP 连接检测端口是否在监听（比 HTTP 健康检查更轻量，且不需要 blocking feature）。
pub fn is_server_ready(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;

    let addr = format!("localhost:{port}");
    match TcpStream::connect_timeout(
        &addr.parse().unwrap_or_else(|_| std::net::SocketAddr::from(([127, 0, 0, 1], port))),
        Duration::from_secs(2),
    ) {
        Ok(_) => true,
        Err(_) => false,
    }
}

/// 启动 funasr-server 子进程（异步）。
///
/// 使用 Blink 自管理的 venv 中的 Python 启动 `funasr-server`。
/// 如果环境未就绪（venv 不存在或 funasr 未安装），返回错误提示用户安装。
///
/// 参数：
/// - `model`: FunASR 模型名（如 "sensevoice" / "paraformer"）
/// - `port`: 监听端口
/// - `device`: "cpu" 或 "cuda"
///
/// 返回子进程句柄 + 日志通道接收端。调用方负责管理子进程生命周期，
/// 并应 spawn 一个 task 持续读取日志通道转发到前端（避免管道阻塞）。
///
/// 如果端口已被占用（服务已在运行），直接返回 Ok(None)。
///
/// # ⚠️ 管道死锁防范
///
/// 子进程的 stdout/stderr 设为 `piped()`，但**必须持续读取**，否则
/// OS 管道缓冲区（Windows ~4KB）写满后子进程会永久阻塞在 write 上。
/// 本函数内部已 spawn 两个 tokio task 分别读取 stdout/stderr，转发到
/// tracing 日志和返回的 channel。
pub async fn start_server(
    model: &str,
    port: u16,
    device: &str,
) -> Result<Option<(tokio::process::Child, tokio::sync::mpsc::UnboundedReceiver<String>)>, String> {
    // 如果服务已就绪，无需启动
    if is_server_ready(port) {
        tracing::info!(port, "funasr-server 已在运行");
        SERVER_RUNNING.store(true, Ordering::SeqCst);
        return Ok(None);
    }

    // 检查 Python 环境是否就绪
    let python_path = crate::infra::platform::python::venv_python();
    if python_path.is_none() {
        return Err(
            "Python 环境未就绪。请在设置页「语音输入」→「本地模式」中点击「安装环境」按钮。\
             （Blink 会自动下载 uv + Python 3.12 + torch + funasr）"
                .to_string(),
        );
    }
    let python = python_path.unwrap();

    // 检查 funasr 是否已安装
    let (funasr_ok, funasr_ver) = crate::infra::platform::python::check_funasr();
    if !funasr_ok {
        return Err(
            "funasr 包未安装。请在设置页点击「安装环境」按钮，Blink 会自动完成安装。".to_string(),
        );
    }

    // 获取 funasr-server 可执行文件路径（pip install funasr 自动生成）
    let server_exe = crate::infra::platform::python::venv_funasr_server()
        .unwrap_or_else(|| {
            // 回退：如果 funasr-server.exe 不存在，用 python -m funasr.bin.server
            // 但正常情况下 pip install funasr 会生成 funasr-server.exe
            tracing::warn!("funasr-server.exe 未找到，尝试用 python -m 方式启动");
            python.clone()
        });
    let use_exe = server_exe != python;

    tracing::info!(
        server_exe = %server_exe.display(),
        ?funasr_ver,
        model,
        port,
        device,
        "启动 funasr-server 子进程",
    );

    let mut cmd = if use_exe {
        // 直接用 funasr-server.exe
        let mut c = tokio::process::Command::new(&server_exe);
        c.args(["--model", model])
            .args(["--port", &port.to_string()])
            .args(["--device", device]);
        c
    } else {
        // 回退：python -m funasr.bin.server
        let mut c = tokio::process::Command::new(&python);
        c.args(["-m", "funasr.bin.server"])
            .args(["--model", model])
            .args(["--port", &port.to_string()])
            .args(["--device", device]);
        c
    };

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("启动 funasr-server 失败: {e}"))?;

    // ── 提取管道句柄，spawn 异步读取 task ──
    // 不读取会导致管道缓冲区写满后子进程永久阻塞（Windows ~4KB）。
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let (log_tx, log_rx) = tokio::sync::mpsc::unbounded_channel::<String>();

    if let Some(stdout) = stdout {
        let tx = log_tx.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "funasr::stdout", "{}", line);
                let _ = tx.send(line);
            }
        });
    }

    if let Some(stderr) = stderr {
        let tx = log_tx.clone();
        tokio::spawn(async move {
            use tokio::io::{AsyncBufReadExt, BufReader};
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "funasr::stderr", "{}", line);
                let _ = tx.send(line);
            }
        });
    }

    // 丢弃原始 sender，这样当所有转发 task 结束后 receiver 会收到 None
    drop(log_tx);

    SERVER_RUNNING.store(true, Ordering::SeqCst);

    tracing::info!("funasr-server 子进程已启动，等待模型加载...");

    Ok(Some((child, log_rx)))
}

/// 异步等待 funasr-server 就绪（HTTP 健康检查轮询）。
///
/// 在 `start_server` 之后调用，轮询 `/v1/models` 端点直到服务响应或超时。
///
/// **注意**：本函数仅做 HTTP 轮询，不检测子进程是否已退出。
/// 如果子进程启动后立即崩溃，本函数会空等至超时。
/// 建议调用方自行通过 `child.try_wait()` 检测子进程退出。
pub async fn wait_for_server_ready(port: u16) -> Result<(), String> {
    let url = format!("http://localhost:{port}/v1/models");
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(SERVER_STARTUP_TIMEOUT_SECS);

    loop {
        if std::time::Instant::now() > deadline {
            return Err(format!(
                "funasr-server 在 {SERVER_STARTUP_TIMEOUT_SECS}s 内未就绪（端口 {port}）"
            ));
        }

        match client.get(&url).send().await {
            Ok(resp) if resp.status().is_success() => {
                tracing::info!(port, "funasr-server 就绪");
                return Ok(());
            }
            _ => {
                tokio::time::sleep(std::time::Duration::from_millis(1000)).await;
            }
        }
    }
}

/// 标记 server 已停止（子进程退出时调用）。
pub fn mark_server_stopped() {
    SERVER_RUNNING.store(false, Ordering::SeqCst);
}

/// 生成 funasr-server 的 base_url（供 CloudSttEngine 使用）。
pub fn server_base_url(port: u16) -> String {
    format!("http://localhost:{port}/v1")
}

/// 获取用于日志诊断的服务器状态摘要。
pub fn server_status_summary(port: u16, model: &str) -> String {
    let ready = is_server_ready(port);
    let running = SERVER_RUNNING.load(Ordering::SeqCst);
    format!(
        "funasr-server: model={model}, port={port}, running={running}, ready={ready}"
    )
}
