//! Python 环境自管理（基于 uv）。
//!
//! Blink 通过 [uv](https://github.com/astral-sh/uv) 创建和管理独立的 Python 虚拟环境，
//! 用户无需手动安装 Python 或 pip 包。uv 会自动下载所需版本的 Python（standalone build）。
//!
//! ## 为什么用 uv
//!
//! 1. **零 Python 依赖**：uv 自身是单个 Rust 二进制，能自动下载和管理 Python 版本
//! 2. **版本锁定**：避免用户系统 Python 版本过高（如 3.14）导致 C 扩展编译失败
//! 3. **隔离环境**：Blink 的 Python 环境与用户系统完全隔离，互不污染
//! 4. **速度快**：uv 用 Rust 写的，比 pip 快 10-100 倍
//!
//! ## 目录结构
//!
//! ```text
//! %APPDATA%\blink\python\
//!   uv\
//!     uv.exe              — uv 二进制（本地安装，不污染 PATH）
//!   venv\                 — Python 虚拟环境（uv 创建）
//!     Scripts\
//!       python.exe        — venv 中的 Python
//!       funasr-server.exe — funasr 服务启动脚本（pip install 自动生成）
//! ```
//!
//! ## 工作流
//!
//! 1. [`ensure_uv`] — 确保 uv 可用（PATH 查找 → 本地下载安装）
//! 2. [`create_venv`] — 创建 venv（uv 自动下载 Python 3.12）
//! 3. [`install_packages`] — 在 venv 中安装包（`uv pip install funasr`）
//! 4. [`venv_python`] — 获取 venv Python 路径，用于启动 funasr-server
//! 5. [`setup`] — 一键完成上述所有步骤
//!
//! ## uv 下载策略
//!
//! 1. 优先检查 PATH 中是否有 uv（用户可能已安装）
//! 2. 检查本地 `%APPDATA%\blink\python\uv\uv.exe`
//! 3. 从 GitHub releases 下载 uv zip 包（~15MB），解压 uv.exe 到本地

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use super::{no_window, no_window_tokio};

/// Blink 管理的 Python 版本。
///
/// FunASR 1.3.x 兼容 Python 3.8-3.12；3.12 有所有依赖的预编译 wheel
/// （包括 editdistance），避免 C 编译失败。3.13+ 部分包尚无预编译 wheel。
const PYTHON_VERSION: &str = "3.12";

/// uv 下载地址（GitHub releases，x86_64 Windows）。
const UV_DOWNLOAD_URL: &str =
    "https://github.com/astral-sh/uv/releases/latest/download/uv-x86_64-pc-windows-msvc.zip";

/// uv 下载超时（秒）。
const UV_DOWNLOAD_TIMEOUT_SECS: u64 = 120;

/// Python 包安装超时（秒）。funasr + torch 及其依赖体积较大。
const PIP_INSTALL_TIMEOUT_SECS: u64 = 600;

/// PyTorch CPU 版本下载索引 URL。
///
/// FunASR 依赖 PyTorch，但不在 pip 依赖中声明（ML 包惯例，
/// 因为 torch 有 CPU/CUDA 多种变体）。我们安装 CPU 版本以避免
/// 下载巨大的 CUDA 包（CPU ~200MB vs CUDA ~2GB）。
const TORCH_CPU_INDEX_URL: &str = "https://download.pytorch.org/whl/cpu";

// ── 环境状态 ─────────────────────────────────────────────────────────────

/// Python 环境完整状态快照（供前端展示和诊断）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PythonEnvStatus {
    // ── uv ──
    /// uv 是否可用（PATH 或本地安装）
    pub uv_available: bool,
    /// uv 可执行文件路径
    pub uv_path: Option<String>,
    /// uv 版本号（如 "uv 0.6.10"）
    pub uv_version: Option<String>,

    // ── venv ──
    /// venv 是否已创建
    pub venv_exists: bool,
    /// venv 中的 Python 版本（如 "Python 3.12.8"）
    pub venv_python_version: Option<String>,

    // ── torch ──
    /// torch（PyTorch）是否已安装在 venv 中
    pub torch_installed: bool,
    /// torch 版本号
    pub torch_version: Option<String>,
    /// 已安装的 PyTorch 是否支持 CUDA（`torch.cuda.is_available()`）
    ///
    /// CPU-only build 返回 false。用于诊断 GPU 是否真正生效，
    /// 以及在切换到 CUDA 模式时判断是否需要重装 PyTorch。
    pub torch_cuda_available: bool,

    // ── funasr ──
    /// funasr 包是否已安装在 venv 中
    pub funasr_installed: bool,
    /// funasr 版本号
    pub funasr_version: Option<String>,

    // ── 综合 ──
    /// 环境是否完全就绪（uv + venv + torch + funasr 四者齐备）
    pub env_ready: bool,
}

// ── 路径 ─────────────────────────────────────────────────────────────────

/// 获取 blink python 根目录（`%APPDATA%\blink\python\`）。
fn python_dir() -> PathBuf {
    crate::infra::utils::paths::python_dir()
}

/// uv 本地安装目录（`%APPDATA%\blink\python\uv\`）。
fn uv_install_dir() -> PathBuf {
    python_dir().join("uv")
}

/// uv 本地安装的 `uv.exe` 路径。
fn local_uv_exe() -> PathBuf {
    uv_install_dir().join("uv.exe")
}

/// venv 目录（`%APPDATA%\blink\python\venv\`）。
fn venv_dir() -> PathBuf {
    python_dir().join("venv")
}

/// 获取 venv 中的 `python.exe` 路径。
///
/// 返回 `None` 表示 venv 尚未创建。
pub fn venv_python() -> Option<PathBuf> {
    let path = venv_dir().join("Scripts").join("python.exe");
    if path.exists() { Some(path) } else { None }
}

/// 获取 venv 中的 `funasr-server.exe` 路径（pip install funasr 自动生成）。
///
/// 返回 `None` 表示 venv 尚未创建或 funasr 未安装。
///
/// 0.10.3 起 Blink 改用 `python blink_stt_server.py` 启动服务，
/// 此函数不再被调用，但保留以备回退或诊断用途。
#[allow(dead_code)]
pub fn venv_funasr_server() -> Option<PathBuf> {
    let path = venv_dir().join("Scripts").join("funasr-server.exe");
    if path.exists() { Some(path) } else { None }
}

// ── uv 检测 ──────────────────────────────────────────────────────────────

/// 查找 uv 可执行文件。
///
/// 查找顺序：
/// 1. 系统 PATH（`where uv`）—— 用户可能已全局安装 uv
/// 2. 本地安装（`%APPDATA%\blink\python\uv\uv.exe`）
pub fn find_uv() -> Option<PathBuf> {
    // 1. Check PATH via `where uv`
    if let Ok(output) = no_window(Command::new("where")).args(["uv"]).output() {
        if output.status.success() {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if let Some(first_line) = stdout.lines().next() {
                let path = PathBuf::from(first_line.trim());
                if path.exists() {
                    tracing::debug!(path = %path.display(), "在 PATH 中找到 uv");
                    return Some(path);
                }
            }
        }
    }

    // 2. Check local install
    let local = local_uv_exe();
    if local.exists() {
        tracing::debug!(path = %local.display(), "找到本地安装的 uv");
        return Some(local);
    }

    None
}

/// 获取 uv 版本号。
fn get_uv_version(uv_path: &Path) -> Option<String> {
    no_window(Command::new(uv_path))
        .args(["--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

// ── uv 安装 ──────────────────────────────────────────────────────────────

/// 下载并安装 uv 到本地目录。
///
/// 从 GitHub releases 下载 uv zip 包，用纯 Rust `zip` crate 解压，
/// 提取 `uv.exe` 到 `%APPDATA%\blink\python\uv\uv.exe`。
pub async fn install_uv() -> Result<PathBuf, String> {
    let uv_dir = uv_install_dir();
    std::fs::create_dir_all(&uv_dir).map_err(|e| format!("创建 uv 目录失败: {e}"))?;

    // ── 下载 uv zip ──
    tracing::info!(url = UV_DOWNLOAD_URL, "下载 uv 二进制...");

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(UV_DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp = client
        .get(UV_DOWNLOAD_URL)
        .send()
        .await
        .map_err(|e| format!("下载 uv 失败: {e}"))?;

    if !resp.status().is_success() {
        return Err(format!("下载 uv 失败: HTTP {}", resp.status()));
    }

    let zip_bytes = resp
        .bytes()
        .await
        .map_err(|e| format!("读取 uv 下载内容失败: {e}"))?;

    tracing::info!(size = zip_bytes.len(), "uv zip 下载完成");

    // ── 用 zip crate 解压（纯 Rust，无 PowerShell 依赖）──
    let extract_dir = uv_dir.join("extract");
    if extract_dir.exists() {
        let _ = std::fs::remove_dir_all(&extract_dir);
    }
    std::fs::create_dir_all(&extract_dir).map_err(|e| format!("创建解压目录失败: {e}"))?;

    let cursor = std::io::Cursor::new(&zip_bytes[..]);
    let mut archive = zip::ZipArchive::new(cursor).map_err(|e| format!("打开 zip 失败: {e}"))?;

    for i in 0..archive.len() {
        let mut file = archive
            .by_index(i)
            .map_err(|e| format!("读取 zip 条目失败: {e}"))?;

        let outpath = match file.enclosed_name() {
            Some(path) => extract_dir.join(path),
            None => continue,
        };

        if file.is_dir() {
            std::fs::create_dir_all(&outpath).map_err(|e| format!("创建目录失败: {e}"))?;
        } else {
            if let Some(parent) = outpath.parent() {
                std::fs::create_dir_all(parent).map_err(|e| format!("创建父目录失败: {e}"))?;
            }
            let mut outfile =
                std::fs::File::create(&outpath).map_err(|e| format!("创建文件失败: {e}"))?;
            std::io::copy(&mut file, &mut outfile).map_err(|e| format!("写入文件失败: {e}"))?;
        }
    }

    tracing::info!("uv zip 解压完成（纯 Rust zip crate）");

    // ── 在解压目录中查找 uv.exe ──
    let uv_exe = find_file_recursive(&extract_dir, "uv.exe")
        .ok_or_else(|| "解压后未找到 uv.exe".to_string())?;

    // ── 复制 uv.exe 到目标位置 ──
    let target = local_uv_exe();
    std::fs::copy(&uv_exe, &target).map_err(|e| format!("复制 uv.exe 失败: {e}"))?;

    // ── 清理临时文件 ──
    let _ = std::fs::remove_dir_all(&extract_dir);

    tracing::info!(path = %target.display(), "uv 安装完成");
    Ok(target)
}

/// 确保 uv 可用（查找 → 本地安装）。
///
/// 如果 uv 已在 PATH 或本地安装，直接返回路径；否则下载安装。
pub async fn ensure_uv() -> Result<PathBuf, String> {
    if let Some(path) = find_uv() {
        return Ok(path);
    }
    install_uv().await
}

// ── venv 管理 ────────────────────────────────────────────────────────────

/// 创建 Python 虚拟环境。
///
/// 使用 `uv venv --python 3.12` 创建 venv。uv 会自动下载 Python 3.12
/// standalone build（如果系统没有该版本）。
///
/// 如果 venv 已存在，跳过创建（幂等）。
pub async fn create_venv(uv_path: &Path) -> Result<(), String> {
    let venv = venv_dir();

    // 幂等：已存在则跳过
    if venv_python().is_some() {
        tracing::info!("venv 已存在，跳过创建");
        return Ok(());
    }

    // 确保父目录存在
    std::fs::create_dir_all(python_dir()).map_err(|e| format!("创建 python 目录失败: {e}"))?;

    tracing::info!(python = PYTHON_VERSION, venv = %venv.display(), "创建 Python venv...");

    let output = no_window_tokio(tokio::process::Command::new(uv_path))
        .args(["venv", "--python", PYTHON_VERSION])
        .arg(&venv)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("执行 uv venv 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "创建 venv 失败 (exit={:?}):\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ));
    }

    tracing::info!(venv = %venv.display(), "venv 创建完成");
    Ok(())
}

// ── 包安装 ───────────────────────────────────────────────────────────────

/// 在 venv 中安装 Python 包。
///
/// 使用 `uv pip install --python <venv_python> <packages...>`。
/// uv 的安装速度比 pip 快 10-100 倍。
pub async fn install_packages(uv_path: &Path, packages: &[&str]) -> Result<(), String> {
    install_packages_inner(uv_path, packages, None).await
}

/// 在 venv 中安装 Python 包（使用指定 index URL）。
///
/// 用于安装 PyTorch CPU 版本等需要特殊索引的包。
pub async fn install_packages_with_index(
    uv_path: &Path,
    packages: &[&str],
    index_url: &str,
) -> Result<(), String> {
    install_packages_inner(uv_path, packages, Some(index_url)).await
}

/// install_packages 的内部实现，支持可选的 index URL。
async fn install_packages_inner(
    uv_path: &Path,
    packages: &[&str],
    index_url: Option<&str>,
) -> Result<(), String> {
    let python = venv_python().ok_or_else(|| "venv 未创建，无法安装包".to_string())?;

    tracing::info!(packages = ?packages, index_url = ?index_url, "安装 Python 包...");

    let install_future = async {
        let mut cmd = tokio::process::Command::new(uv_path);
        cmd.args(["pip", "install", "--python"]).arg(&python);

        if let Some(url) = index_url {
            cmd.args(["--index-url", url]);
        }

        cmd.args(packages)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        no_window_tokio(cmd).output().await
    };

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(PIP_INSTALL_TIMEOUT_SECS),
        install_future,
    )
    .await
    .map_err(|_| format!("安装包超时（{PIP_INSTALL_TIMEOUT_SECS}s），可能网络较慢，请重试"))?
    .map_err(|e| format!("执行 uv pip install 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "安装包失败 (exit={:?}):\nstdout: {stdout}\nstderr: {stderr}",
            output.status.code()
        ));
    }

    tracing::info!("Python 包安装完成");
    Ok(())
}

// ── 包卸载 ───────────────────────────────────────────────────────────────

/// 从 venv 中卸载 Python 包。
///
/// 使用 `uv pip uninstall --python <venv_python> <packages...>`。
/// 用于在重装 PyTorch（CPU→CUDA 变体替换）前彻底清除旧安装，
/// 避免 uv 检测到版本号匹配而跳过实际替换。
pub async fn uninstall_packages(uv_path: &Path, packages: &[&str]) -> Result<(), String> {
    let python = venv_python().ok_or_else(|| "venv 未创建，无法卸载包".to_string())?;

    tracing::info!(packages = ?packages, "卸载 Python 包...");

    let output = no_window_tokio(tokio::process::Command::new(uv_path))
        .args(["pip", "uninstall", "--python"])
        .arg(&python)
        .args(packages)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("执行 uv pip uninstall 失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        // 卸载不存在的包不算错误（uv 可能返回非零退出码）
        tracing::warn!(%stderr, "卸载包返回非零退出码（可能包不存在）");
    }

    tracing::info!("Python 包卸载完成");
    Ok(())
}

// ── CUDA 检测 ────────────────────────────────────────────────────────────

/// 检测系统是否有 NVIDIA GPU 及 CUDA 版本。
///
/// 通过运行 `nvidia-smi` 并解析输出中的 CUDA 版本。
/// 兼容新旧驱动格式：
/// - 旧：`CUDA Version: 12.2`
/// - 新：`CUDA UMD Version: 13.3`
/// 返回 CUDA 版本字符串（如 "12.2" / "13.3"），无 GPU 时返回 None。
pub fn detect_cuda() -> Option<String> {
    let output = no_window(Command::new("nvidia-smi")).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    for line in stdout.lines() {
        // 匹配 "CUDA Version: X.Y" 或 "CUDA UMD Version: X.Y"
        if line.contains("CUDA") && line.contains("Version:") {
            // 取 "Version:" 后面的版本号
            if let Some(idx) = line.find("Version:") {
                let rest = &line[idx + "Version:".len()..];
                // 跳过空格，取第一个数字串（如 "12.2" 或 "13.3"）
                let version = rest
                    .trim_start()
                    .split_whitespace()
                    .next()?
                    .trim_end_matches('|')
                    .trim();
                if !version.is_empty()
                    && version
                        .chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                {
                    return Some(version.to_string());
                }
            }
        }
    }
    None
}

/// 检查 uv 是否支持 `--torch-backend` 参数（uv ≥ 0.4.0）。
///
/// 通过 `uv pip install --help` 输出中是否包含 `--torch-backend` 判定。
fn uv_supports_torch_backend(uv_path: &Path) -> bool {
    let output = match no_window(Command::new(uv_path))
        .args(["pip", "install", "--help"])
        .output()
    {
        Ok(o) => o,
        Err(_) => return false,
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    stdout.contains("--torch-backend")
}

// ── 流式安装 ──────────────────────────────────────────────────────────────

/// 在 venv 中安装 Python 包，stdout/stderr 实时逐行转发到 `on_log`。
///
/// 比 [`install_packages`] 多了日志流式输出——安装 torch + funasr 可能 5-10 分钟，
/// 用户可以实时看到 uv 的安装进度行（如 `Downloading torch-2.x.x`、`Resolved 47 packages`）。
async fn install_packages_streaming(
    uv_path: &Path,
    packages: &[&str],
    extra_args: &[&str],
    on_log: &Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    let python = venv_python().ok_or_else(|| "venv 未创建，无法安装包".to_string())?;

    tracing::info!(packages = ?packages, extra_args = ?extra_args, "安装 Python 包（流式）...");

    let mut cmd = tokio::process::Command::new(uv_path);
    cmd.args(["pip", "install", "--python"]).arg(&python);
    cmd.args(extra_args);
    cmd.args(packages);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let mut cmd = no_window_tokio(cmd);

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("执行 uv pip install 失败: {e}"))?;

    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();

    // 并发读取 stdout + stderr，逐行转发到 on_log
    let on_log1 = Arc::clone(on_log);
    let stdout_task = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stdout);
        use tokio::io::AsyncBufReadExt;
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            on_log1(&line);
        }
    });

    let on_log2 = Arc::clone(on_log);
    let stderr_task = tokio::spawn(async move {
        let reader = tokio::io::BufReader::new(stderr);
        use tokio::io::AsyncBufReadExt;
        let mut lines = reader.lines();
        while let Ok(Some(line)) = lines.next_line().await {
            on_log2(&line);
        }
    });

    // 等待进程结束（带超时）
    let status = tokio::time::timeout(
        std::time::Duration::from_secs(PIP_INSTALL_TIMEOUT_SECS),
        child.wait(),
    )
    .await
    .map_err(|_| format!("安装包超时（{PIP_INSTALL_TIMEOUT_SECS}s），可能网络较慢，请重试"))?
    .map_err(|e| format!("等待 uv pip install 完成失败: {e}"))?;

    // 等待读取 task 排空剩余输出
    let _ = tokio::join!(stdout_task, stderr_task);

    if !status.success() {
        return Err(format!("安装包失败 (exit={:?})", status.code()));
    }

    tracing::info!("Python 包安装完成");
    Ok(())
}

// ── 状态检查 ─────────────────────────────────────────────────────────────

/// 检查 torch（PyTorch）是否已安装在 venv 中。
///
/// 使用 `importlib.metadata.version('torch')` 检测——轻量快速，
/// 不触发 torch 的重型 import（torch import 需 5-15 秒）。
///
/// 返回 (是否已安装, 版本号)。
pub fn check_torch() -> (bool, Option<String>) {
    let python = match venv_python() {
        Some(p) => p,
        None => return (false, None),
    };

    match no_window(Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('torch'))",
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (true, Some(version))
        }
        _ => (false, None),
    }
}

/// 检查已安装的 PyTorch 是否支持 CUDA。
///
/// 运行 `python -c "import torch; print(torch.cuda.is_available())"`。
/// CPU-only build 返回 false。用于诊断 GPU 是否真正生效。
///
/// 如果 torch 未安装或导入失败（如 `torch._C` 损坏），返回 false。
pub fn check_torch_cuda() -> bool {
    let python = match venv_python() {
        Some(p) => p,
        None => return false,
    };

    match no_window(Command::new(python))
        .args(["-c", "import torch; print(torch.cuda.is_available())"])
        .output()
    {
        Ok(output) if output.status.success() => {
            let stdout = String::from_utf8_lossy(&output.stdout).trim().to_string();
            stdout == "True"
        }
        Ok(output) => {
            // torch import 失败（如 torch._C 损坏）
            let stderr = String::from_utf8_lossy(&output.stderr);
            tracing::warn!(%stderr, "PyTorch import 失败，CUDA 不可用");
            false
        }
        _ => false,
    }
}

/// 检查 venv 中的 Python 版本。
fn check_venv_python_version() -> Option<String> {
    let python = venv_python()?;
    no_window(Command::new(python))
        .args(["--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

/// 检查 funasr 包是否已安装在 venv 中。
///
/// 使用 `importlib.metadata.version('funasr')` 检测——比 `import funasr` 轻量得多
/// （后者会加载 PyTorch/torchaudio 等重型依赖，耗时 10-30 秒）。
///
/// 返回 (是否已安装, 版本号)。
pub fn check_funasr() -> (bool, Option<String>) {
    let python = match venv_python() {
        Some(p) => p,
        None => return (false, None),
    };

    match no_window(Command::new(python))
        .args([
            "-c",
            "import importlib.metadata as m; print(m.version('funasr'))",
        ])
        .output()
    {
        Ok(output) if output.status.success() => {
            let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
            (true, Some(version))
        }
        _ => (false, None),
    }
}

/// 获取完整环境状态快照。
pub fn check_status() -> PythonEnvStatus {
    let uv_path = find_uv();
    let uv_version = uv_path.as_ref().and_then(|p| get_uv_version(p));
    let uv_available = uv_path.is_some();

    let venv_exists = venv_python().is_some();
    let venv_python_version = if venv_exists {
        check_venv_python_version()
    } else {
        None
    };

    let (torch_installed, torch_version) = if venv_exists {
        check_torch()
    } else {
        (false, None)
    };

    // 只在 torch 已安装时才检查 CUDA 支持（避免无意义的子进程调用）
    let torch_cuda_available = torch_installed && check_torch_cuda();
    if torch_installed {
        tracing::info!(torch_cuda_available, "PyTorch CUDA 支持检测");
    }

    let (funasr_installed, funasr_version) = if venv_exists {
        check_funasr()
    } else {
        (false, None)
    };

    let env_ready = uv_available && venv_exists && torch_installed && funasr_installed;

    PythonEnvStatus {
        uv_available,
        uv_path: uv_path.map(|p| p.display().to_string()),
        uv_version,
        venv_exists,
        venv_python_version,
        torch_installed,
        torch_version,
        torch_cuda_available,
        funasr_installed,
        funasr_version,
        env_ready,
    }
}

/// 异步获取完整环境状态快照。
///
/// 将同步的子进程调用放到 `spawn_blocking` 线程池执行，避免阻塞 async 运行时。
/// 适用于 Tauri async 命令中调用。
pub async fn check_status_async() -> PythonEnvStatus {
    tokio::task::spawn_blocking(check_status)
        .await
        .unwrap_or_else(|e| {
            tracing::error!(%e, "check_status spawn_blocking 失败");
            PythonEnvStatus {
                uv_available: false,
                uv_path: None,
                uv_version: None,
                venv_exists: false,
                venv_python_version: None,
                torch_installed: false,
                torch_version: None,
                torch_cuda_available: false,
                funasr_installed: false,
                funasr_version: None,
                env_ready: false,
            }
        })
}

// ── 一键设置 ─────────────────────────────────────────────────────────────

/// 一键设置完整 Python 环境（无进度回调）。
///
/// 内部调用 [`setup_with_progress`]，传入 no-op 回调。
/// 需要进度/日志通知的场景（如设置页）直接调用 [`setup_with_progress`]。
pub async fn setup(device: &str) -> Result<(), String> {
    let on_progress: Arc<dyn Fn(&str, &str) + Send + Sync> = Arc::new(|_, _| ());
    let on_log: Arc<dyn Fn(&str) + Send + Sync> = Arc::new(|_| ());
    setup_with_progress(device, on_progress, on_log).await
}

/// 一键设置完整 Python 环境，带进度 + 日志回调。
///
/// 步骤：
/// 1. 确保 uv 可用（查找 → 下载安装）
/// 2. 创建 venv（uv 自动下载 Python 3.12）
/// 3. 安装所有包（funasr + fastapi + uvicorn + python-multipart + torch）
///
/// **torch 安装策略**（B1 改进）：
/// - 若 uv 支持 `--torch-backend`（≥0.4.0）：一条命令安装所有包，
///   `--torch-backend=auto` 让 uv 自动检测 CUDA 版本并选择正确的 PyTorch 变体。
///   CPU 用户用 `--torch-backend=cpu`。
/// - 否则（旧版 uv）：回退到两步安装（先 torch 从 pytorch.org index，
///   再 funasr 从 PyPI）。
///
/// `on_progress(stage, status)`：阶段进度（如 `("uv", "done")`）。
/// `on_log(line)`：安装过程中的逐行日志输出（含 uv 的 `Downloading...` 行）。
pub async fn setup_with_progress(
    device: &str,
    on_progress: Arc<dyn Fn(&str, &str) + Send + Sync>,
    on_log: Arc<dyn Fn(&str) + Send + Sync>,
) -> Result<(), String> {
    // 快速检查：如果已就绪，跳过安装。
    // 但当 device == "cuda" 时，需额外验证已安装的 PyTorch 是否含 CUDA 支持——
    // 如果之前以 CPU 模式安装了 CPU-only PyTorch，切换到 CUDA 后需重装。
    let status = check_status();
    let need_torch_reinstall =
        device == "cuda" && status.torch_installed && !status.torch_cuda_available;

    if need_torch_reinstall {
        tracing::warn!("配置为 CUDA 模式但已安装的 PyTorch 不含 CUDA 支持，将重装 PyTorch");
        on_log("[Blink] ⚠️ 检测到当前 PyTorch 为 CPU 版，正在重装 CUDA 版 PyTorch...");
    }

    if status.env_ready && !need_torch_reinstall {
        tracing::info!("Python 环境已就绪，跳过安装");
        on_progress("complete", "ready");
        return Ok(());
    }

    // Step 1: uv
    if !status.uv_available {
        on_progress("uv", "starting");
        on_log("[Blink] 下载安装 uv 中...");
        match ensure_uv().await {
            Ok(path) => {
                tracing::info!(path = %path.display(), "uv 安装完成");
                on_log(&format!("[Blink] ✅ uv 安装完成: {}", path.display()));
            }
            Err(e) => {
                on_progress("error", &e);
                on_log(&format!("[Blink] ❌ uv 安装失败: {e}"));
                return Err(e);
            }
        }
    }
    on_progress("uv", "done");

    let uv_path = find_uv().ok_or("uv 不可用")?;

    // Step 2: venv
    if !status.venv_exists {
        on_progress("venv", "starting");
        on_log("[Blink] 创建 Python 3.12 虚拟环境...");
        if let Err(e) = create_venv(&uv_path).await {
            on_progress("error", &e);
            on_log(&format!("[Blink] ❌ venv 创建失败: {e}"));
            return Err(e);
        }
    }
    on_progress("venv", "done");
    on_log("[Blink] ✅ Python venv 就绪");

    // Step 3: 安装包（torch + funasr）
    // 当 need_torch_reinstall 时，即使 torch 已安装也需重装（CPU→CUDA）
    if !status.torch_installed || !status.funasr_installed || need_torch_reinstall {
        // CPU→CUDA 重装：先彻底卸载旧 PyTorch，再安装 CUDA 版。
        // --reinstall-package / --force-reinstall 都不够可靠——
        // uv 检测到版本号匹配会复用缓存的 CPU wheel，不会真正替换为 CUDA 变体。
        // 只有先 uninstall 清除残留文件，再 fresh install 才能确保 CUDA wheel 生效。
        if need_torch_reinstall {
            on_log("[Blink] 卸载旧版 PyTorch (CPU)...");
            uninstall_packages(&uv_path, &["torch", "torchaudio"]).await?;
        }

        let supports_tbb = uv_supports_torch_backend(&uv_path);

        if supports_tbb {
            // ── 新方案：一条命令 + --torch-backend ──
            let backend = if device == "cuda" {
                match detect_cuda() {
                    Some(v) => {
                        on_log(&format!("[Blink] 检测到 CUDA {v}，使用 GPU 加速"));
                        "auto" // uv 自动匹配 CUDA 版本
                    }
                    None => {
                        on_log("[Blink] ⚠️ 未检测到 CUDA，使用 CPU 版 PyTorch");
                        "cpu"
                    }
                }
            } else {
                "cpu"
            };

            on_progress("packages", "installing");
            let desc = if backend == "auto" {
                "PyTorch (CUDA auto) + funasr"
            } else {
                "PyTorch (CPU) + funasr"
            };
            on_log(&format!("[Blink] 安装 {desc}...（这可能需要几分钟）"));

            install_packages_streaming(
                &uv_path,
                // torch + torchaudio 必须显式列出——funasr 不声明 torch 依赖（ML 包惯例），
                // --torch-backend 只控制 torch 包的 index，不会自动把 torch 加入安装列表。
                // torch_complex 是 FunASR 的可选依赖，不声明在 funasr 的 install_requires 中。
                // numba>=0.59 强制使用支持 Python 3.12 的版本（含预编译 wheel），
                // 避免 funasr→umap-learn→numba 0.53→llvmlite 0.36 在 3.12 上编译失败。
                &[
                    "torch",
                    "torchaudio",
                    "torch_complex",
                    "numba>=0.59",
                    "funasr",
                    "fastapi",
                    "uvicorn[standard]",
                    "python-multipart",
                ],
                &["--torch-backend", backend],
                &on_log,
            )
            .await?;
        } else {
            // ── 回退：旧版 uv 两步安装 ──
            on_log("[Blink] ⚠️ uv 版本较旧，使用两步安装（torch + funasr）");

            // Step 3a: torch
            // need_torch_reinstall 时即使 torch 已安装也需重装（CPU→CUDA）
            // 先 uninstall 已在上面完成，此处直接安装
            if !status.torch_installed || need_torch_reinstall {
                let is_cuda = device == "cuda";
                let (index_url, desc) = if is_cuda {
                    (
                        "https://download.pytorch.org/whl/cu121",
                        "PyTorch CUDA 版（~2GB，请耐心等待）",
                    )
                } else {
                    (TORCH_CPU_INDEX_URL, "PyTorch CPU 版（~200MB）")
                };
                on_progress("torch", "installing");
                on_log(&format!("[Blink] 安装 {desc}..."));
                install_packages_with_index(&uv_path, &["torch", "torchaudio"], index_url).await?;
            }
            on_progress("torch", "done");
            on_log("[Blink] ✅ PyTorch 安装完成");

            // Step 3b: funasr + server 依赖 + torch_complex
            if !status.funasr_installed || need_torch_reinstall {
                on_progress("funasr", "installing");
                on_log("[Blink] 安装 funasr + fastapi + uvicorn[standard]...");
                // numba>=0.59 强制使用支持 Python 3.12 的版本，避免 llvmlite 编译失败
                install_packages(
                    &uv_path,
                    &[
                        "funasr",
                        "fastapi",
                        "uvicorn[standard]",
                        "python-multipart",
                        "torch_complex",
                        "numba>=0.59",
                    ],
                )
                .await?;
            }
        }
    }

    on_progress("packages", "done");
    on_progress("complete", "ready");
    on_log("[Blink] ✅ Python 环境安装完成，可以启动服务了");

    tracing::info!("Python 环境设置完成");
    Ok(())
}

// ── 工具函数 ─────────────────────────────────────────────────────────────

/// 递归查找目录中指定文件名的文件。
fn find_file_recursive(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
    {
        if entry.file_name().to_str() == Some(name) {
            return Some(entry.path().to_path_buf());
        }
    }
    None
}

// ── 测试 ─────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_dir_under_appdata() {
        let dir = python_dir();
        assert!(
            dir.ends_with("blink\\python") || dir.ends_with("blink/python"),
            "python_dir 应在 blink/python 下, got: {}",
            dir.display()
        );
    }

    #[test]
    fn venv_python_path_is_scripts_python_exe() {
        let python = venv_dir().join("Scripts").join("python.exe");
        assert!(
            python.ends_with("venv\\Scripts\\python.exe")
                || python.ends_with("venv/Scripts/python.exe")
        );
    }

    #[test]
    fn local_uv_exe_path_is_correct() {
        let uv = local_uv_exe();
        assert!(
            uv.ends_with("uv\\uv.exe") || uv.ends_with("uv/uv.exe"),
            "local_uv_exe 应在 uv/uv.exe, got: {}",
            uv.display()
        );
    }

    #[test]
    fn check_status_returns_struct_without_panic() {
        // 只验证不 panic，实际状态取决于运行环境
        let _status = check_status();
    }

    #[test]
    fn find_file_recursive_finds_existing_file() {
        let tmp = std::env::temp_dir().join("blink_find_file_test");
        std::fs::create_dir_all(&tmp).unwrap();
        let nested = tmp.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();
        let target = nested.join("target.exe");
        std::fs::write(&target, b"test").unwrap();

        let found = find_file_recursive(&tmp, "target.exe");
        assert_eq!(found, Some(target.clone()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn find_file_recursive_returns_none_for_missing() {
        let tmp = std::env::temp_dir().join("blink_find_file_empty");
        std::fs::create_dir_all(&tmp).unwrap();
        assert!(find_file_recursive(&tmp, "nonexistent.exe").is_none());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
