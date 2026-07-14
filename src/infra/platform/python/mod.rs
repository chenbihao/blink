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
    dirs_next::data_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("blink")
        .join("python")
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
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

/// 获取 venv 中的 `funasr-server.exe` 路径（pip install funasr 自动生成）。
///
/// 返回 `None` 表示 venv 尚未创建或 funasr 未安装。
pub fn venv_funasr_server() -> Option<PathBuf> {
    let path = venv_dir().join("Scripts").join("funasr-server.exe");
    if path.exists() {
        Some(path)
    } else {
        None
    }
}

// ── uv 检测 ──────────────────────────────────────────────────────────────

/// 查找 uv 可执行文件。
///
/// 查找顺序：
/// 1. 系统 PATH（`where uv`）—— 用户可能已全局安装 uv
/// 2. 本地安装（`%APPDATA%\blink\python\uv\uv.exe`）
pub fn find_uv() -> Option<PathBuf> {
    // 1. Check PATH via `where uv`
    if let Ok(output) = Command::new("where").args(["uv"]).output() {
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
    Command::new(uv_path)
        .args(["--version"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
}

// ── uv 安装 ──────────────────────────────────────────────────────────────

/// 下载并安装 uv 到本地目录。
///
/// 从 GitHub releases 下载 uv zip 包，用 PowerShell `Expand-Archive` 解压，
/// 提取 `uv.exe` 到 `%APPDATA%\blink\python\uv\uv.exe`。
pub async fn install_uv() -> Result<PathBuf, String> {
    let uv_dir = uv_install_dir();
    std::fs::create_dir_all(&uv_dir)
        .map_err(|e| format!("创建 uv 目录失败: {e}"))?;

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

    // ── 保存 zip 到临时文件 ──
    let zip_path = uv_dir.join("uv_download.zip");
    std::fs::write(&zip_path, &zip_bytes)
        .map_err(|e| format!("保存 uv zip 失败: {e}"))?;

    // ── 用 PowerShell Expand-Archive 解压 ──
    let extract_dir = uv_dir.join("extract");
    if extract_dir.exists() {
        let _ = std::fs::remove_dir_all(&extract_dir);
    }
    std::fs::create_dir_all(&extract_dir)
        .map_err(|e| format!("创建解压目录失败: {e}"))?;

    let ps_cmd = format!(
        "Expand-Archive -Path '{}' -DestinationPath '{}' -Force",
        zip_path.display(),
        extract_dir.display()
    );

    let output = Command::new("powershell")
        .args(["-NoProfile", "-NonInteractive", "-Command", &ps_cmd])
        .output()
        .map_err(|e| format!("启动 PowerShell 解压失败: {e}"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        return Err(format!(
            "解压 uv zip 失败: exit={:?}, stderr={stderr}, stdout={stdout}",
            output.status.code()
        ));
    }

    // ── 在解压目录中查找 uv.exe ──
    let uv_exe = find_file_recursive(&extract_dir, "uv.exe")
        .ok_or_else(|| "解压后未找到 uv.exe".to_string())?;

    // ── 复制 uv.exe 到目标位置 ──
    let target = local_uv_exe();
    std::fs::copy(&uv_exe, &target)
        .map_err(|e| format!("复制 uv.exe 失败: {e}"))?;

    // ── 清理临时文件 ──
    let _ = std::fs::remove_file(&zip_path);
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
    std::fs::create_dir_all(python_dir())
        .map_err(|e| format!("创建 python 目录失败: {e}"))?;

    tracing::info!(python = PYTHON_VERSION, venv = %venv.display(), "创建 Python venv...");

    let output = tokio::process::Command::new(uv_path)
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
    let python = venv_python()
        .ok_or_else(|| "venv 未创建，无法安装包".to_string())?;

    tracing::info!(packages = ?packages, index_url = ?index_url, "安装 Python 包...");

    let install_future = async {
        let mut cmd = tokio::process::Command::new(uv_path);
        cmd.args(["pip", "install", "--python"])
            .arg(&python);

        if let Some(url) = index_url {
            cmd.args(["--index-url", url]);
        }

        cmd.args(packages)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .output()
            .await
    };

    let output = tokio::time::timeout(
        std::time::Duration::from_secs(PIP_INSTALL_TIMEOUT_SECS),
        install_future,
    )
    .await
    .map_err(|_| {
        format!("安装包超时（{PIP_INSTALL_TIMEOUT_SECS}s），可能网络较慢，请重试")
    })?
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

    match Command::new(python)
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

/// 检查 venv 中的 Python 版本。
fn check_venv_python_version() -> Option<String> {
    let python = venv_python()?;
    Command::new(python)
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

    match Command::new(python)
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
                funasr_installed: false,
                funasr_version: None,
                env_ready: false,
            }
        })
}

// ── 一键设置 ─────────────────────────────────────────────────────────────

/// 一键设置完整 Python 环境。
///
/// 步骤：
/// 1. 确保 uv 可用（查找 → 下载安装）
/// 2. 创建 venv（uv 自动下载 Python 3.12）
/// 3. 安装 PyTorch（CPU 或 CUDA 版，取决于 `device` 参数）
/// 4. 安装 funasr + fastapi + uvicorn + python-multipart（funasr-server 依赖）
///
/// 此函数是幂等的：如果环境已就绪，直接返回 Ok。
///
/// `device` 参数控制 PyTorch 版本："cpu" 安装 CPU 版（~200MB），"cuda" 安装 CUDA 版（~2GB）。
pub async fn setup(device: &str) -> Result<(), String> {
    // 快速检查：如果已就绪，跳过
    let status = check_status();
    if status.env_ready {
        tracing::info!("Python 环境已就绪，跳过安装");
        return Ok(());
    }

    // Step 1: uv
    let uv_path = ensure_uv().await?;
    tracing::info!(uv = %uv_path.display(), "uv 就绪");

    // Step 2: venv
    create_venv(&uv_path).await?;
    tracing::info!("venv 就绪");

    // Step 3: torch（CPU 或 CUDA 版）
    // funasr 依赖 torch 但不通过 pip 依赖声明（ML 包惯例），需手动安装。
    if !check_torch().0 {
        let is_cuda = device == "cuda";
        let (index_url, desc) = if is_cuda {
            ("https://download.pytorch.org/whl/cu121", "CUDA")
        } else {
            (TORCH_CPU_INDEX_URL, "CPU")
        };
        tracing::info!(device = %desc, "安装 PyTorch...");
        install_packages_with_index(
            &uv_path,
            &["torch", "torchaudio"],
            index_url,
        )
        .await?;
    }
    tracing::info!("PyTorch 安装完成");

    // Step 4: funasr + server 依赖
    // funasr-server 需要 fastapi/uvicorn/python-multipart，但 funasr 包不声明这些依赖。
    if !check_funasr().0 {
        install_packages(&uv_path, &["funasr", "fastapi", "uvicorn", "python-multipart"]).await?;
    }
    tracing::info!("funasr 安装完成");

    tracing::info!("Python 环境设置完成");
    Ok(())
}

// ── 工具函数 ─────────────────────────────────────────────────────────────

/// 递归查找目录中指定文件名的文件。
fn find_file_recursive(dir: &Path, name: &str) -> Option<PathBuf> {
    for entry in walkdir::WalkDir::new(dir).into_iter().filter_map(|e| e.ok()) {
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
        assert!(dir.ends_with("blink\\python") || dir.ends_with("blink/python"),
            "python_dir 应在 blink/python 下, got: {}", dir.display());
    }

    #[test]
    fn venv_python_path_is_scripts_python_exe() {
        let python = venv_dir().join("Scripts").join("python.exe");
        assert!(python.ends_with("venv\\Scripts\\python.exe") || python.ends_with("venv/Scripts/python.exe"));
    }

    #[test]
    fn local_uv_exe_path_is_correct() {
        let uv = local_uv_exe();
        assert!(uv.ends_with("uv\\uv.exe") || uv.ends_with("uv/uv.exe"),
            "local_uv_exe 应在 uv/uv.exe, got: {}", uv.display());
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
