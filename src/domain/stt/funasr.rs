//! Blink STT Server Python 环境工具 + HTTP 健康检查（0.22.6）。
//!
//! ## 设计
//!
//! 本模块提供 FunASR STT 所需的纯工具函数和 HTTP 健康检查：
//! 1. 嵌入 `blink_stt_server.py` 并在启动时释放到 `%APPDATA%\blink\python\`
//! 2. 热词文件生成（逗号分隔 → 换行分隔）
//! 3. HTTP 健康检查（`/health` 端点模型加载状态查询）
//! 4. TCP 端口预检 + server base URL 构造
//! 5. FunASR 日志噪声过滤
//! 6. 诊断状态聚合（`FunasrEnv`，兼容旧前端事件）
//!
//! ## 0.22.6 变更
//!
//! ServerStartParams / build_launch_request / start_server / mark_server_stopped
//! 已删除。启动逻辑完全由 `app/local_engine/funasr.rs` 的 `FunasrAdapter::prepare_launch`
//! 通过 `LocalEngineService` 统一管理，使用 generation-based 隔离环境。
//!
//! ## 兼容性
//!
//! HTTP 端点路径和响应格式与官方 `funasr-server` 完全一致，
//! 现有 Rust 侧的 `LocalSttEngine` / `PseudoStreamingSttEngine` 和 `check_model_loaded()` 无需修改。
//!
//! ## 旧全局 venv
//!
//! `get_env_status_async` 中的 `check_status_async` / `check_torch` / `check_funasr`
//! 仍读取旧全局 venv（`%APPDATA%\blink\python\venv`），仅供兼容诊断展示。
//! 新安装路径为 `runtimes/engines/funasr/generations/{install_id}/venv`。

use std::path::PathBuf;

/// 嵌入的 blink_stt_server.py 脚本（随 Rust 二进制发布）。
const BLINK_STT_SERVER_PY: &str = include_str!("../../../resources/stt/funasr/blink_stt_server.py");

/// server 启动超时（秒）。
/// 首次启动需要从 ModelScope 下载模型（~234MB），加上 PyTorch 加载，
/// 可能需要 3-5 分钟。后续启动仅模型加载，通常 30-60 秒。
pub const SERVER_STARTUP_TIMEOUT_SECS: u64 = 300;

// ── Python 脚本释放 ────────────────────────────────────────────────────────

/// 获取 `%APPDATA%\blink\python\` 目录路径。
fn python_dir() -> PathBuf {
    #[cfg(test)]
    {
        return crate::infra::local_engine::runtime::python_shared_root();
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::python_dir()
}

/// 获取 blink_stt_server.py 的目标路径。
#[allow(dead_code)] // STT 脚本路径工具，待 release 流程消费
pub fn server_script_path() -> PathBuf {
    python_dir().join("blink_stt_server.py")
}

/// 确保 blink_stt_server.py 已释放到 `%APPDATA%\blink\python\`。
///
/// 每次调用都覆写（保证脚本随 Blink 版本更新），失败不阻断——
/// 如果文件已存在且内容相同则跳过写入。
///
/// 返回脚本路径，失败时返回 None（调用方应提示用户）。
pub fn ensure_server_script() -> Result<PathBuf, String> {
    ensure_server_script_in(&python_dir())
}

/// `ensure_server_script` 的内部实现，接受显式目标目录（测试用）。
///
/// 生产入口 [`ensure_server_script`] 使用正式 `python_dir()`，
/// 测试传入 `tempfile::TempDir` 路径以隔离真实 `%APPDATA%`。
pub(crate) fn ensure_server_script_in(dir: &std::path::Path) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 python 目录失败: {e}"))?;

    let script_path = dir.join("blink_stt_server.py");

    // 检查是否已存在且内容一致（避免无谓写入）
    let need_write = match std::fs::read_to_string(&script_path) {
        Ok(existing) => existing != BLINK_STT_SERVER_PY,
        Err(_) => true, // 不存在或读取失败
    };

    if need_write {
        tracing::info!(
            path = %script_path.display(),
            "释放 blink_stt_server.py（{}字节）",
            BLINK_STT_SERVER_PY.len()
        );
        std::fs::write(&script_path, BLINK_STT_SERVER_PY)
            .map_err(|e| format!("写入 blink_stt_server.py 失败: {e}"))?;
    }

    Ok(script_path)
}

/// 将热词配置写入 `%APPDATA%\blink\python\hotwords.txt`。
///
/// 前端用英文逗号分隔热词（省空间），FunASR 要求每行一个——
/// 此函数自动将逗号 / 换行混合分隔转为换行格式。
///
/// 返回文件路径（如果 hotwords 为空则返回 None，不写文件）。
pub fn write_hotwords_file(hotwords: &Option<String>) -> Option<PathBuf> {
    write_hotwords_file_in(&python_dir(), hotwords)
}

/// `write_hotwords_file` 的内部实现，接受显式目标目录（测试用）。
///
/// 生产入口 [`write_hotwords_file`] 使用正式 `python_dir()`，
/// 测试传入 `tempfile::TempDir` 路径以隔离真实 `%APPDATA%`。
pub(crate) fn write_hotwords_file_in(
    dir: &std::path::Path,
    hotwords: &Option<String>,
) -> Option<PathBuf> {
    let normalized = normalize_hotwords(hotwords.as_deref()?);
    if normalized.is_empty() {
        return None;
    }

    if std::fs::create_dir_all(dir).is_err() {
        return None;
    }

    let path = dir.join("hotwords.txt");
    match std::fs::write(&path, &normalized) {
        Ok(()) => {
            tracing::info!(path = %path.display(), "热词文件已写入");
            Some(path)
        }
        Err(e) => {
            tracing::warn!(%e, "热词文件写入失败");
            None
        }
    }
}

/// 纯函数：将热词配置文本归一化为换行分隔格式。
///
/// 前端用英文逗号分隔热词（省空间），FunASR 要求每行一个——
/// 此函数自动将逗号 / 换行混合分隔转为换行格式。
///
/// 空白输入返回空字符串（调用方据此跳过文件写入）。
pub(crate) fn normalize_hotwords(raw: &str) -> String {
    if raw.trim().is_empty() {
        return String::new();
    }

    raw.split([',', '\n', '\r'])
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

// ── 状态结构 ──────────────────────────────────────────────────────────────

/// FunASR 环境 + 服务完整状态。
///
/// 聚合了 Python 环境状态（uv/venv/funasr）和 server 运行状态，
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
    /// 已安装的 PyTorch 是否支持 CUDA
    pub torch_cuda_available: bool,
    /// funasr 包是否已安装
    pub funasr_installed: bool,
    /// funasr 版本
    pub funasr_version: Option<String>,
    /// Python 环境是否完全就绪
    pub env_ready: bool,

    // ── server 状态 ──
    /// server 是否正在运行（0.22.3：由 app 层从 ManagedProcess 填充）
    pub server_running: bool,
    /// 当前配置的监听端口
    pub server_port: u16,
    /// 当前配置的非流式模型
    pub server_model: String,
}

/// 获取环境 + 服务的完整状态（异步版，不阻塞 async 运行时）。
///
/// 将 Python 子进程检测放到 `spawn_blocking` 线程池执行。
/// 适用于 Tauri async 命令中调用，避免阻塞 UI 线程。
/// 获取环境 + 服务的完整状态（异步版，不阻塞 async 运行时）。
///
/// 将 Python 子进程检测放到 `spawn_blocking` 线程池执行。
///
/// 0.22.3：`server_running` 参数由调用方（app 层）从 ManagedProcess
/// 查询后传入。domain 层不再持有全局可变状态。
pub async fn get_env_status_async(
    server_port: u16,
    server_model: String,
    server_running: bool,
) -> FunasrEnv {
    let py_status = crate::infra::platform::python::check_status_async().await;
    let (torch_installed, torch_version, torch_cuda_available, funasr_installed, funasr_version) =
        tokio::task::spawn_blocking(|| {
            let (torch_installed, torch_version) = crate::infra::platform::python::check_torch();
            let torch_cuda_available =
                torch_installed && crate::infra::platform::python::check_torch_cuda();
            let (funasr_installed, funasr_version) = crate::infra::platform::python::check_funasr();
            (
                torch_installed,
                torch_version,
                torch_cuda_available,
                funasr_installed,
                funasr_version,
            )
        })
        .await
        .unwrap_or((false, None, false, false, None));
    let env_ready = py_status.env_ready && torch_installed && funasr_installed;

    FunasrEnv {
        uv_available: py_status.uv_available,
        uv_version: py_status.uv_version,
        venv_exists: py_status.venv_exists,
        venv_python_version: py_status.venv_python_version,
        torch_installed,
        torch_version,
        torch_cuda_available,
        funasr_installed,
        funasr_version,
        env_ready,
        server_running,
        server_port,
        server_model,
    }
}

// ── 健康检查 ──────────────────────────────────────────────────────────────

/// 判断一行 funasr 日志是否为噪声（应过滤掉）。
///
/// FunASR 的 stderr 会输出大量 tqdm 进度条和推理指标，对调试无帮助且刷屏：
/// - `{'load_data': '0.000', ...}` — 推理指标
/// - `rtf_avg: 0.227: 100%|██████████|...` — RTF 平均值 + 进度条
/// - `100%|██████████| 1/1 [00:00<00:00, 8.24it/s]` — tqdm 进度条
/// - 纯 ANSI 转义序列（`\x1b[34m` 等）
pub(crate) fn is_funasr_noise(line: &str) -> bool {
    // 委托给公共入口
    is_funasr_noise_pub(line)
}

/// 公共入口：判断一行 funasr 日志是否为噪声（应过滤掉）。
///
/// 0.22.1：app 层从 ManagedProcess 日志流过滤时调用此函数。
/// FunASR 的 tqdm/ANSI 噪声过滤属于 FunASR adapter/app 投影逻辑，
/// 不应硬编码进通用 ManagedProcess。
pub fn is_funasr_noise_pub(line: &str) -> bool {
    // tqdm 进度条行
    if line.contains("it/s]") {
        return true;
    }
    // 推理指标行：`{'load_data': ...}` 或 `rtf_avg:`
    if line.starts_with("{'load_data'") || line.starts_with("rtf_avg:") {
        return true;
    }
    // 含进度条百分比的行：`100%|` 开头
    if line.contains("|") && line.contains("%|") {
        return true;
    }
    // FunASR 内部加载噪声
    if line.contains("trust_remote_code:") {
        return true;
    }
    if line.starts_with("scope_map:") || line.starts_with("excludes:") {
        return true;
    }
    if line.starts_with("funasr version:") {
        return true;
    }
    if line.contains("Check update of funasr") || line.contains("You are using the latest version")
    {
        return true;
    }
    // 非流式转录请求参数行（每次请求都重复，参数在启动时已打印）
    if line.contains("非流式转录: samples=") {
        return true;
    }
    // 热词解析日志（每次请求都重复打印，只需在启动时看一次）
    if line.contains("Attempting to parse hotwords") || line.contains("Initialized hotword list") {
        return true;
    }
    // HTTP 访问日志（每次请求都有，噪声大）
    if line.contains("POST /v1/audio/transcriptions HTTP/1.1")
        || line.contains("GET /health HTTP/1.1")
        || line.contains("GET /v1/models HTTP/1.1")
    {
        return true;
    }
    // FunASR 内部解码日志（每次请求都有，无诊断价值）
    if line.contains("decoding, utt:") || line.contains("empty speech") {
        return true;
    }
    // 纯 ANSI 转义 + 空白
    let stripped = strip_ansi(line);
    stripped.is_empty()
}

/// 去除 ANSI 转义序列，返回纯文本内容。
fn strip_ansi(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // ESC sequence: ESC [ ... m
            if chars.peek() == Some(&'[') {
                chars.next();
                while let Some(&c2) = chars.peek() {
                    chars.next();
                    if c2.is_ascii_alphabetic() {
                        break;
                    }
                }
            }
        } else {
            result.push(c);
        }
    }
    result.trim().to_string()
}

/// 检查 server 是否在指定端口上监听（TCP 级别）。
///
/// **注意**：此函数只检查 TCP 端口是否可连接，**不验证 HTTP API 是否就绪**，
/// 也**不区分端口占用者是否为 Blink 管理的子进程**。
///
/// 以下情况都会返回 `true`：
/// - Blink 通过 LocalEngineService 启动的子进程正在监听
/// - Blink 崩溃后遗留的孤儿进程仍在监听（child handle 已丢失）
/// - 其他程序恰好占用了同一端口
///
/// server 启动后 uvicorn 先绑定 TCP 端口，但模型可能还在加载（30-60s），
/// 此时 TCP 连接成功但 HTTP 请求会失败。
///
/// 用于快速预检（如 `LocalSttEngine::new` 中的快速失败判断）。
/// 在需要确保模型真正就绪的场景，使用 [`check_model_loaded`]。
/// 在需要清理孤儿进程的场景，使用 `infra::platform::process::kill_process_by_port`。
pub fn is_server_ready(port: u16) -> bool {
    use std::net::TcpStream;
    use std::time::Duration;

    // 0.22.3：直接使用 127.0.0.1，与 Endpoint 协议一致。
    let addr = std::net::SocketAddr::from(([127, 0, 0, 1], port));
    TcpStream::connect_timeout(
        &addr,
        // 500ms 超时：服务正常时 loopback TCP 连接毫秒级返回；
        // 服务未启动时 Windows 上未监听端口会等满超时（非 RST），2s 太长会阻塞调用方。
        Duration::from_millis(500),
    )
    .is_ok()
}

/// `is_server_ready` 的异步版本，用 tokio async TCP + 短超时。
///
/// **为什么不用 `spawn_blocking(is_server_ready)`**：Windows 上 127.0.0.1 未监听端口
/// 的 `connect_timeout` 返回 "connection timed out"（非 "refused"），等满整个超时时间。
/// 旧实现用 2s 超时 + spawn_blocking，虽不阻塞 worker 线程，但 effect 串行循环仍需
/// 等 2s 才能处理 HoldReleased -> 窗口出现/消失慢一拍。
///
/// 改用 `tokio::net::TcpStream::connect` + 500ms 超时：端口有服务时毫秒级返回，
/// 无服务时最多等 500ms（而非 2s）。不影响其他 tokio task。
pub async fn is_server_ready_async(port: u16) -> bool {
    let addr = format!("127.0.0.1:{port}");
    match tokio::net::lookup_host(addr).await {
        Ok(mut addrs) => {
            if let Some(sock_addr) = addrs.next() {
                matches!(
                    tokio::time::timeout(
                        std::time::Duration::from_millis(500),
                        tokio::net::TcpStream::connect(sock_addr),
                    )
                    .await,
                    Ok(Ok(_))
                )
            } else {
                false
            }
        }
        Err(_) => false,
    }
}

/// 模型加载状态（从 Python server `/health` 端点获取）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelLoadStatus {
    /// Python server 尚未响应（HTTP 不可达或响应异常）
    Unreachable,
    /// 模型尚未开始加载（idle）
    Idle,
    /// 模型正在下载/加载中（首次需从 ModelScope 下载 ~234MB）
    Loading,
    /// 模型已就绪，可接受转录请求
    Ready,
    /// 模型加载失败
    Error,
}

/// 检查模型是否已加载完毕。
///
/// 通过 `GET /health` 端点的 `model_status` 字段判断模型加载状态。
/// 仅当返回 [`ModelLoadStatus::Ready`] 时，转录请求才能立即响应。
///
/// 用于：
/// - `commands.rs` 启动流程的轮询（区分 "服务已启动但模型还在下载" 与 "模型就绪"）
/// - `local.rs` 的 `finalize()` 检查（提供更精准的错误提示）
pub async fn check_model_loaded(port: u16) -> ModelLoadStatus {
    // 0.22.3：使用 127.0.0.1 而非 localhost，与 Endpoint 协议一致。
    let url = format!("http://127.0.0.1:{port}/health");
    let client = match reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(2))
        .build()
    {
        Ok(c) => c,
        Err(_) => return ModelLoadStatus::Unreachable,
    };

    match client.get(&url).send().await {
        Ok(resp) if resp.status().is_success() => {
            match resp.json::<serde_json::Value>().await {
                Ok(v) => {
                    // 优先读 model_status（新字段），回退到 model_loaded（旧字段兼容）
                    match v.get("model_status").and_then(|s| s.as_str()) {
                        Some("ready") => ModelLoadStatus::Ready,
                        Some("loading") => ModelLoadStatus::Loading,
                        Some("error") => ModelLoadStatus::Error,
                        Some("idle") => ModelLoadStatus::Idle,
                        _ => {
                            // 旧版 server 没有 model_status 字段，回退到 model_loaded
                            if v.get("model_loaded").and_then(|b| b.as_bool()) == Some(true) {
                                ModelLoadStatus::Ready
                            } else {
                                ModelLoadStatus::Loading
                            }
                        }
                    }
                }
                Err(_) => ModelLoadStatus::Unreachable,
            }
        }
        _ => ModelLoadStatus::Unreachable,
    }
}

/// 检查模型是否就绪，不就绪则返回对应的错误消息。
///
/// 供 `LocalSttEngine` 和 `PseudoStreamingSttEngine` 共用，
/// 消除重复的 match 分支。
pub async fn check_model_ready_or_error(port: u16) -> Result<(), String> {
    match check_model_loaded(port).await {
        ModelLoadStatus::Ready => Ok(()),
        ModelLoadStatus::Loading | ModelLoadStatus::Idle => Err(format!(
            "模型正在加载中（端口 {port}），首次使用需下载 ~234MB 模型文件，请稍后在设置页等待加载完成后重试。"
        )),
        ModelLoadStatus::Error => Err(format!(
            "模型加载失败（端口 {port}），请在设置页查看日志或检查网络连接后重启服务。"
        )),
        ModelLoadStatus::Unreachable => Err(format!(
            "FunASR 服务不可达（端口 {port}）。请确认服务已在设置页启动。"
        )),
    }
}

// ── server 启动参数（0.22.6 已移除 legacy 启动路径）──────────────────────
//
// 0.22.6：ServerStartParams / build_launch_request / start_server / mark_server_stopped
// 已删除。启动逻辑完全由 app/local_engine/funasr.rs 的 FunasrAdapter::prepare_launch
// 通过 LocalEngineService 统一管理。此模块仅保留工具函数和 HTTP 健康检查。

/// 生成 server 的 base_url（供 HTTP 转录使用）。
///
/// 0.22.3：使用 `127.0.0.1` 而非 `localhost`，与 LocalEngineService 的
/// Endpoint 协议一致——Endpoint 只允许 loopback。
pub fn server_base_url(port: u16) -> String {
    format!("http://127.0.0.1:{port}/v1")
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_script_is_not_empty() {
        assert!(!BLINK_STT_SERVER_PY.is_empty());
        // 验证脚本是有效 Python（含关键标识）
        assert!(BLINK_STT_SERVER_PY.contains("blink_stt_server"));
        assert!(BLINK_STT_SERVER_PY.contains("/v1/audio/transcriptions"));
        assert!(BLINK_STT_SERVER_PY.contains("/health"));
    }

    #[test]
    fn server_script_path_is_in_python_dir() {
        let path = server_script_path();
        assert!(
            path.ends_with("python\\blink_stt_server.py")
                || path.ends_with("python/blink_stt_server.py"),
            "script path should be in python dir, got: {}",
            path.display()
        );
    }

    #[test]
    fn ensure_server_script_creates_file() {
        // 使用临时目录，不写真实 %APPDATA%
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let path = ensure_server_script_in(tmp.path()).expect("ensure_server_script_in 失败");
        assert!(path.exists(), "script file should exist after ensure");

        // 验证文件内容与嵌入内容一致
        let content = std::fs::read_to_string(&path).expect("读取脚本失败");
        assert_eq!(content, BLINK_STT_SERVER_PY);
    }

    // ── normalize_hotwords 纯函数测试 ──

    #[test]
    fn normalize_hotwords_empty_returns_empty() {
        assert_eq!(normalize_hotwords(""), "");
        assert_eq!(normalize_hotwords("   "), "");
        assert_eq!(normalize_hotwords("  \n  \r  "), "");
    }

    #[test]
    fn normalize_hotwords_comma_separated() {
        assert_eq!(
            normalize_hotwords("美团 100, 快手 80, Blink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_newline_separated() {
        assert_eq!(
            normalize_hotwords("美团 100\n快手 80\nBlink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_mixed_separators() {
        assert_eq!(
            normalize_hotwords("美团 100, 快手 80\nBlink 100"),
            "美团 100\n快手 80\nBlink 100"
        );
    }

    #[test]
    fn normalize_hotwords_trims_whitespace() {
        assert_eq!(
            normalize_hotwords("  美团 100 ,  快手 80  "),
            "美团 100\n快手 80"
        );
    }

    #[test]
    fn normalize_hotwords_filters_empty_entries() {
        assert_eq!(
            normalize_hotwords("美团 100,, ,快手 80"),
            "美团 100\n快手 80"
        );
    }

    // ── write_hotwords_file 落盘测试（使用临时目录，只验证一次实际写入）──

    #[test]
    fn write_hotwords_none_for_empty() {
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let result = write_hotwords_file_in(tmp.path(), &None);
        assert!(result.is_none());

        let result = write_hotwords_file_in(tmp.path(), &Some("   \n  ".to_string()));
        assert!(result.is_none());
    }

    #[test]
    fn write_hotwords_creates_file() {
        // 只验证一次实际落盘和路径
        let tmp = tempfile::tempdir().expect("创建临时目录失败");
        let hotwords = "美团 100, 快手 80, Blink 100".to_string();
        let path = write_hotwords_file_in(tmp.path(), &Some(hotwords));
        assert!(path.is_some(), "热词文件应被创建");

        let path = path.unwrap();
        assert!(path.exists(), "热词文件应存在");
        assert!(path.ends_with("hotwords.txt"));

        let content = std::fs::read_to_string(&path).expect("读取热词文件失败");
        assert_eq!(content, "美团 100\n快手 80\nBlink 100");
    }

    // ── 日志噪声过滤测试 ──

    #[test]
    fn noise_filter_detects_tqdm_progress() {
        assert!(is_funasr_noise(
            "100%|\x1b[34m██████████\x1b[0m| 1/1 [00:00<00:00, 8.24it/s]"
        ));
        assert!(is_funasr_noise(
            "  0%|\x1b[34m          \x1b[0m| 0/1 [00:00<?, ?it/s]"
        ));
    }

    #[test]
    fn noise_filter_detects_rtf_metrics() {
        assert!(is_funasr_noise(
            "{'load_data': '0.000', 'extract_feat': 0.0, 'forward': '0.000', 'batch_size': '1', 'rtf': '-0.000'}, : 100%|\x1b[34m██████████\x1b[0m| 1/1 [00:00<?, ?it/s]"
        ));
        assert!(is_funasr_noise(
            "rtf_avg: 0.227: 100%|\x1b[34m██████████\x1b[0m| 1/1 [00:00<00:00,  8.24it/s]"
        ));
    }

    #[test]
    fn noise_filter_preserves_useful_logs() {
        assert!(!is_funasr_noise(
            "INFO:     Started server process [293704]"
        ));
        assert!(!is_funasr_noise(
            "INFO:     Uvicorn running on http://0.0.0.0:8000 (Press CTRL+C to quit)"
        ));
        assert!(!is_funasr_noise(
            "Downloading 11 files from iic/speech_paraformer-large_asr_nat-zh-cn-16k-common-vocab8404-online@master"
        ));
        assert!(!is_funasr_noise(
            "19:16:18 [root] INFO: Loading pretrained params from C:\\Users\\...\\model.pt"
        ));
        assert!(!is_funasr_noise(
            "Loading ckpt: ..., status: <All keys matched successfully>"
        ));
    }

    #[test]
    fn noise_filter_detects_funasr_internal_noise() {
        assert!(is_funasr_noise(
            "19:51:15 [root] WARNING: trust_remote_code: False"
        ));
        assert!(is_funasr_noise("scope_map: ['module.', 'None']"));
        assert!(is_funasr_noise("excludes: None"));
        assert!(is_funasr_noise("funasr version: 1.3.14."));
        assert!(is_funasr_noise(
            "Check update of funasr, and it would cost few times."
        ));
        assert!(is_funasr_noise(
            "You are using the latest version of funasr-1.3.14"
        ));
    }

    #[test]
    fn noise_filter_detects_pure_ansi() {
        assert!(is_funasr_noise("\x1b[34m\x1b[0m"));
        assert!(is_funasr_noise(""));
    }

    #[test]
    fn noise_filter_detects_hotword_parsing() {
        // 热词解析每次请求都重复打印，过滤掉
        assert!(is_funasr_noise(
            "19:11:49 [root] INFO: Attempting to parse hotwords from local txt..."
        ));
        assert!(is_funasr_noise(
            "19:11:49 [root] INFO: Initialized hotword list from file: \
             C:\\Users\\99452\\AppData\\Roaming\\blink\\python\\hotwords.txt, \
             hotword list: ['伪流式', '<s>']."
        ));
    }

    #[test]
    fn noise_filter_detects_http_access_log() {
        // HTTP 访问日志每次请求都有，过滤掉
        assert!(is_funasr_noise(
            "INFO:     127.0.0.1:5527 - \"POST /v1/audio/transcriptions HTTP/1.1\" 200 OK"
        ));
        assert!(is_funasr_noise(
            "INFO:     127.0.0.1:4444 - \"GET /health HTTP/1.1\" 200 OK"
        ));
        assert!(is_funasr_noise(
            "INFO:     127.0.0.1:4444 - \"GET /v1/models HTTP/1.1\" 200 OK"
        ));
    }

    #[test]
    fn noise_filter_detects_decoding_logs() {
        // FunASR 内部解码日志，每次请求都有
        assert!(is_funasr_noise(
            "19:24:05 [root] INFO: decoding, utt: tmpgfzhfbo3, empty speech"
        ));
    }

    #[test]
    fn noise_filter_detects_transcription_request_params() {
        // 非流式转录参数行每次请求都重复，参数在启动时已打印
        assert!(is_funasr_noise(
            "19:32:08 [blink_stt_server] INFO: 非流式转录: samples=6720, model=paraformer-zh, hotword=yes, itn=True"
        ));
    }

    /// 验证嵌入的 Python 脚本包含模型名解析函数（修复 FunASR 1.3.14 短名 404 问题）。
    #[test]
    fn embedded_script_contains_model_alias_resolution() {
        assert!(
            BLINK_STT_SERVER_PY.contains("_MODEL_ALIASES"),
            "blink_stt_server.py 应包含 _MODEL_ALIASES 模型别名映射"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("_resolve_model_id"),
            "blink_stt_server.py 应包含 _resolve_model_id 函数"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("iic/SenseVoiceSmall"),
            "blink_stt_server.py 应包含完整 ModelScope ID 'iic/SenseVoiceSmall'"
        );
    }

    /// 验证嵌入的 Python 脚本包含 SenseVoice 输出标签后处理。
    ///
    /// SenseVoice 模型输出形如 `<|zh|><|NEUTRAL|><|Speech|><|withitn|>文本`，
    /// 需用 `rich_transcription_postprocess` 去除这些元数据标签。
    #[test]
    fn embedded_script_contains_postprocess_for_sensevoice_tags() {
        assert!(
            BLINK_STT_SERVER_PY.contains("_postprocess_text"),
            "blink_stt_server.py 应包含 _postprocess_text 后处理函数"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("rich_transcription_postprocess"),
            "blink_stt_server.py 应导入 rich_transcription_postprocess"
        );
        assert!(
            BLINK_STT_SERVER_PY.contains("_postprocess_text(raw_text)"),
            "transcribe 端点应调用 _postprocess_text"
        );
    }
}
