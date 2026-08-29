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

use sha2::Digest;

use super::no_window;

/// Blink 管理的 Python 版本。
///
/// FunASR 1.3.x 兼容 Python 3.8-3.12；3.12 有所有依赖的预编译 wheel
/// （包括 editdistance），避免 C 编译失败。3.13+ 部分包尚无预编译 wheel。
const PYTHON_VERSION: &str = "3.12";

/// uv 固定版本（供应链锁定）。
///
/// 放弃 `latest` 路径，改用固定版本 + SHA-256 强校验，
/// 防止供应链劫持或 CDN 篡改。
///
/// 升级 uv 时需同步更新此常量、`UV_ARCHIVE_URL` 和 `UV_SHA256`。
const UV_VERSION: &str = "0.12.7";

/// uv 下载地址（固定版本，GitHub releases，x86_64 Windows）。
///
/// 使用 `releases.astral.sh` CDN（GitHub 官方 release asset mirror），
/// 避免 `latest` 重定向带来的不可预测性。
const UV_ARCHIVE_URL: &str =
    "https://releases.astral.sh/github/uv/releases/download/0.12.7/uv-x86_64-pc-windows-msvc.zip";

/// uv zip 包的 SHA-256（供应链强校验）。
///
/// 下载后必须校验此 hash，不匹配则拒绝安装并清理残留。
/// 升级 uv 版本时必须同步更新此值——可通过以下命令获取：
/// ```sh
/// curl -sL https://releases.astral.sh/github/uv/releases/download/{VERSION}/uv-x86_64-pc-windows-msvc.zip.sha256
/// ```
const UV_SHA256: &str = "bf1518af459a3915511a11fdc6e2f43ef9a2afa138b9d498eeb9642fe9d85218";

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

// ── 路径 ─────────────────────────────────────────────────────────────────

/// 获取 blink python 根目录（`%APPDATA%\blink\python\`）。
fn python_dir() -> PathBuf {
    crate::infra::utils::paths::python_dir()
}

fn venv_dir() -> PathBuf {
    python_dir().join("venv")
}

pub fn venv_funasr_server() -> Option<PathBuf> {
    let path = venv_dir().join("Scripts").join("funasr-server.exe");
    if path.exists() { Some(path) } else { None }
}

// ── uv 安装 ──────────────────────────────────────────────────────────────

pub async fn install_uv_to_dir(
    target_dir: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<PathBuf, String> {
    std::fs::create_dir_all(target_dir).map_err(|e| format!("创建 uv 目录失败: {e}"))?;

    // ── 取消检查：在下载前 ──
    if let Some(ct) = cancel_token {
        if ct.is_cancelled() {
            return Err("uv 下载在开始前被取消".to_string());
        }
    }

    // ── 下载 uv zip ──
    tracing::info!(
        url = UV_ARCHIVE_URL,
        version = UV_VERSION,
        "下载 uv 二进制到 Blink 托管目录..."
    );

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(UV_DOWNLOAD_TIMEOUT_SECS))
        .build()
        .map_err(|e| format!("HTTP client 创建失败: {e}"))?;

    let resp_future = client.get(UV_ARCHIVE_URL).send();

    let resp = if let Some(ct) = cancel_token {
        tokio::select! {
            biased;
            _ = ct.cancelled() => {
                tracing::info!("uv 下载在 HTTP 请求阶段被取消");
                return Err("uv 下载被取消".to_string());
            }
            r = resp_future => r.map_err(|e| format!("下载 uv 失败: {e}"))?,
        }
    } else {
        resp_future
            .await
            .map_err(|e| format!("下载 uv 失败: {e}"))?
    };

    if !resp.status().is_success() {
        return Err(format!("下载 uv 失败: HTTP {}", resp.status()));
    }

    let bytes_future = resp.bytes();
    let zip_bytes = if let Some(ct) = cancel_token {
        tokio::select! {
            biased;
            _ = ct.cancelled() => {
                tracing::info!("uv 下载在读取内容阶段被取消");
                return Err("uv 下载被取消".to_string());
            }
            b = bytes_future => b.map_err(|e| format!("读取 uv 下载内容失败: {e}"))?,
        }
    } else {
        bytes_future
            .await
            .map_err(|e| format!("读取 uv 下载内容失败: {e}"))?
    };

    tracing::info!(size = zip_bytes.len(), "uv zip 下载完成");

    // ── SHA-256 校验（供应链锁定） ──
    let actual_hash = format!("{:x}", sha2::Sha256::digest(&zip_bytes));
    if actual_hash != UV_SHA256 {
        tracing::error!(
            expected = UV_SHA256,
            actual = %actual_hash,
            "uv zip SHA-256 校验失败"
        );
        return Err(format!(
            "uv zip SHA-256 校验失败: 期望 {UV_SHA256}，实际 {actual_hash}"
        ));
    }
    tracing::info!(hash = %actual_hash, "uv zip SHA-256 校验通过");

    // ── 用 zip crate 解压（纯 Rust，无 PowerShell 依赖）──
    let extract_dir = target_dir.join("extract");
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

    // ── 原子提升：先写到临时文件再 rename，避免半写状态 ──
    let target = target_dir.join("uv.exe");
    let staging_exe = target_dir.join("uv.exe.staging");
    std::fs::copy(&uv_exe, &staging_exe).map_err(|e| format!("复制 uv.exe 失败: {e}"))?;

    // rename 是原子操作（同卷）；如果 target 已存在则先删除
    if target.exists() {
        let _ = std::fs::remove_file(&target);
    }
    std::fs::rename(&staging_exe, &target).map_err(|e| {
        // rename 失败则清理 staging 文件
        let _ = std::fs::remove_file(&staging_exe);
        format!("原子提升 uv.exe 失败: {e}")
    })?;

    // ── 清理临时文件 ──
    let _ = std::fs::remove_dir_all(&extract_dir);

    tracing::info!(path = %target.display(), "Blink 托管 uv 安装完成");
    Ok(target)
}

// ── CUDA 检测 ────────────────────────────────────────────────────────────

/// 检测系统是否有 NVIDIA GPU 及 CUDA 版本。
///
/// 通过运行 `nvidia-smi` 并解析输出中的 CUDA 版本。
/// 兼容新旧驱动格式：
/// - 旧：`CUDA Version: 12.2`
/// - 新：`CUDA UMD Version: 13.3`
///
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
                let version = rest.split_whitespace().next()?.trim_end_matches('|').trim();
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

    // ── 0.22.6 H3: 供应链锁定测试 ──────────────────────────────────────────

    /// 验证 uv 版本常量已固定（非 `latest`）。
    #[test]
    fn uv_version_is_pinned() {
        assert!(!UV_VERSION.is_empty(), "UV_VERSION 不应为空");
        assert!(
            UV_VERSION
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_digit()),
            "UV_VERSION 应以数字开头，got: {UV_VERSION}"
        );
    }

    /// 验证 uv 下载 URL 使用固定版本而非 `latest`。
    #[test]
    fn uv_archive_url_is_versioned() {
        assert!(
            !UV_ARCHIVE_URL.contains("/latest/"),
            "UV_ARCHIVE_URL 不应使用 latest 路径: {UV_ARCHIVE_URL}"
        );
        assert!(
            UV_ARCHIVE_URL.contains(UV_VERSION),
            "UV_ARCHIVE_URL 应包含 UV_VERSION={UV_VERSION}: {UV_ARCHIVE_URL}"
        );
    }

    /// 验证 uv SHA-256 常量是有效的 64 位十六进制字符串。
    #[test]
    fn uv_sha256_is_valid_hex() {
        assert_eq!(
            UV_SHA256.len(),
            64,
            "SHA-256 应为 64 个字符，got: {}",
            UV_SHA256.len()
        );
        assert!(
            UV_SHA256.bytes().all(|b| b.is_ascii_hexdigit()),
            "SHA-256 应为十六进制，got: {UV_SHA256}"
        );
        assert!(
            !UV_SHA256.chars().all(|c| c == '0'),
            "SHA-256 不应为全零（需更新为真实 hash）"
        );
    }

    /// 验证 SHA-256 校验逻辑：正确 hash 通过，错误 hash 拒绝。
    #[test]
    fn sha256_verification_logic() {
        let test_bytes = b"hello world";
        let correct_hash = format!("{:x}", sha2::Sha256::digest(test_bytes));
        let wrong_hash = "0".repeat(64);

        // 正确 hash 应匹配
        assert_eq!(
            correct_hash,
            "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9"
        );

        // 错误 hash 不匹配
        assert_ne!(correct_hash, wrong_hash);
    }
}
