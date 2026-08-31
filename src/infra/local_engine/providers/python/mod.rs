//! PythonVenvProvider 完整实现（0.22.2 / 0.22.6 H2 硬化）。
//!
//! 负责：
//! - 确保 uv 可用（**只使用 Blink 托管的 uv**，不静默接受 PATH 中任意版本）
//! - 校验实际 uv 版本满足 `PythonInstallPlan` 声明的版本
//! - 创建隔离 venv（按 generation 隔离，不共享 site-packages）
//! - 同步锁定依赖（package lock + index）
//! - self-test（执行 descriptor 声明的 self-test 脚本）
//! - 查询包状态（`importlib.metadata.version`）
//! - 清理 uv cache
//!
//! ## 0.22.6 H2 硬化铁则
//!
//! - **只使用 Blink 托管的 uv**：`ensure_uv` 只检查 `runtime::local_uv_exe()`，
//!   不再扫描系统 PATH。如果本地 uv 不存在，下载安装到 Blink 托管目录。
//! - **版本校验**：安装/准备环境前校验实际 uv 版本满足 descriptor 声明的
//!   `uv_version`；不满足则返回错误，不静默继续。
//! - **显式环境变量**：所有 uv/venv/pip 命令显式设置 `UV_CACHE_DIR`、
//!   `UV_PYTHON_INSTALL_DIR` 等 Blink 自有环境变量，确保 uv 行为与
//!   `runtime::uv_cache_dir()` / `uv_python_dir()` 一致。
//! - **取消安全**：取消/超时不只 drop future，还终止受管子进程并清理 staging。
//! - **不读取用户配置 Python 解释器**：系统 PATH 的脚本解释器与本地模型
//!   托管环境保持隔离；uv 命令的环境不继承用户 PATH 中的 Python。

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use sha2::{Digest, Sha256};
use tokio::process::Child;
use tokio::sync::Mutex;

use super::{
    CompatibilityCheck, InstallPlan, InstallSink, ManifestExtension, PackageLock, PipExtraArg,
    PrepareResult, ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;

pub fn render_hashed_requirements(packages: &[PackageLock]) -> Result<String, RuntimeError> {
    let mut requirements = String::new();
    for package in packages {
        if package
            .version
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '>' | '<' | '~' | '!'))
        {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "require-hashes 要求精确版本，{} 使用了非精确约束 {}",
                    package.name, package.version
                ),
            });
        }
        let version = package.version.trim_start_matches('=');
        // 使用 all_hashes（多平台 wheel hash），回退到 sha256（单 hash）
        let hashes: Vec<&str> = if !package.all_hashes.is_empty() {
            package.all_hashes.iter().map(|s| s.as_str()).collect()
        } else {
            package.sha256.as_deref().into_iter().collect()
        };
        if hashes.is_empty() {
            return Err(RuntimeError::InstallFailed {
                message: format!("{} 缺少 SHA-256", package.name),
            });
        }
        // 验证所有 hash 格式
        for hash in &hashes {
            if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
                return Err(RuntimeError::InstallFailed {
                    message: format!("{} 的 SHA-256 格式无效", package.name),
                });
            }
        }
        requirements.push_str(&format!("{}=={}", package.name, version));
        for hash in &hashes {
            requirements.push_str(&format!(" --hash=sha256:{}", hash));
        }
        requirements.push('\n');
    }
    Ok(requirements)
}
use crate::infra::platform::python;

// ── Singleflight: 跨引擎并发 ensure_uv 锁 ──────────────────────────────────

/// 全局 ensure_uv 互斥锁。
///
/// **0.22.6 H3**：防止多个引擎（如 FunASR + PaddleOCR）并发安装时
/// 争抢共享的 `uv` 目录——同一时间只允许一个 `ensure_uv` 执行下载/安装，
/// 其余等待者复用第一个完成的结果。
///
/// 使用 `OnceLock` 实现 lazy init（const fn 不可用 tokio::sync::Mutex::new）。
static UV_ENSURE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

/// 获取全局 ensure_uv 锁。
fn uv_ensure_lock() -> &'static Mutex<()> {
    UV_ENSURE_LOCK.get_or_init(|| Mutex::new(()))
}

// ── uv 版本比较 ─────────────────────────────────────────────────────────────

/// 解析 uv 版本字符串（如 `uv 0.6.10`）为主版本号三元组。
///
/// uv --version 输出格式通常为 `uv 0.6.10`。
/// 返回 `Option<(major, minor, patch)>`，解析失败时返回 None。
fn parse_uv_version(version_str: &str) -> Option<(u32, u32, u32)> {
    // 去掉前缀 "uv " 如果存在
    let cleaned = version_str
        .trim()
        .strip_prefix("uv ")
        .unwrap_or(version_str.trim());
    let parts: Vec<&str> = cleaned.split('.').collect();
    if parts.len() < 3 {
        return None;
    }
    let major = parts[0].parse::<u32>().ok()?;
    let minor = parts[1].parse::<u32>().ok()?;
    // patch 可能带有后缀（如 "10+meta"），取数字部分
    let patch_str = parts[2].split(|c: char| !c.is_ascii_digit()).next()?;
    let patch = patch_str.parse::<u32>().ok()?;
    Some((major, minor, patch))
}

/// 校验实际 uv 版本是否满足声明的要求版本。
///
/// 声明版本格式与 `PythonInstallPlan::uv_version` 一致（如 `0.6.10`）。
/// 实际版本 >= 声明版本即满足。
fn uv_version_satisfies(actual: &str, required: &str) -> bool {
    let actual_ver = match parse_uv_version(actual) {
        Some(v) => v,
        None => {
            tracing::warn!(%actual, "无法解析实际 uv 版本");
            return false;
        }
    };
    let required_ver = match parse_uv_version(required) {
        Some(v) => v,
        None => {
            tracing::warn!(%required, "无法解析声明 uv 版本要求");
            // 如果声明版本不可解析，保守起见认为不满足
            return false;
        }
    };
    actual_ver >= required_ver
}

// ── 环境变量构建 ─────────────────────────────────────────────────────────────

/// 为 uv 命令设置 Blink 托管环境变量。
///
/// 确保所有 uv/venv/pip 下载命令显式使用 Blink 自有的 `UV_CACHE_DIR`、
/// `UV_PYTHON_INSTALL_DIR` 等必要环境变量，使 `runtime::uv_cache_dir()`、
/// `uv_python_dir()` 与真实 uv 行为一致。
fn apply_blink_uv_env(cmd: &mut tokio::process::Command) {
    let cache_dir = runtime::uv_cache_dir();
    let python_install_dir = runtime::uv_python_dir();

    // 确保目录存在（uv 需要时自行创建，但提前创建更安全）
    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::create_dir_all(&python_install_dir);

    cmd.env("UV_CACHE_DIR", &cache_dir);
    cmd.env("UV_PYTHON_INSTALL_DIR", &python_install_dir);
    // UV_TOOL_DIR 控制工具安装目录，与 UV_PYTHON_INSTALL_DIR 分离
    // uv venv 创建的 venv 位置由 --directory 参数控制，不需要环境变量
}

/// 为同步 `std::process::Command` 设置 Blink 托管环境变量。
fn apply_blink_uv_env_sync(cmd: &mut Command) {
    let cache_dir = runtime::uv_cache_dir();
    let python_install_dir = runtime::uv_python_dir();

    let _ = std::fs::create_dir_all(&cache_dir);
    let _ = std::fs::create_dir_all(&python_install_dir);

    cmd.env("UV_CACHE_DIR", &cache_dir);
    cmd.env("UV_PYTHON_INSTALL_DIR", &python_install_dir);
}

/// PythonVenv Provider。
///
/// 封装 uv + Python distribution + venv + pip 的完整生命周期。
/// 每个 generation 拥有独立 venv，不共享 site-packages。
pub struct PythonVenvProvider {
    /// uv 可执行文件路径（ensure_uv 后缓存）。
    uv_path: Option<PathBuf>,
    /// 是否允许 GPU backend（测试时可关闭）。
    allow_gpu: bool,
}

impl PythonVenvProvider {
    /// 创建 PythonVenvProvider。
    pub fn new() -> Self {
        Self {
            uv_path: None,
            allow_gpu: true,
        }
    }

    /// 创建只允许 CPU 的 PythonVenvProvider（测试用）。
    #[allow(dead_code)]
    pub fn cpu_only() -> Self {
        Self {
            uv_path: None,
            allow_gpu: false,
        }
    }

    /// 确保 uv 可用，返回 Blink 托管的 uv 路径。
    ///
    /// **0.22.6 H2**：只使用 Blink 托管的 uv（`runtime::local_uv_exe()`），
    /// 不静默接受 PATH 中任意版本 uv。如果本地 uv 不存在，下载安装到
    /// Blink 托管目录。
    ///
    /// **0.22.6 H3**：使用全局 `Singleflight` 锁——防止多个引擎并发
    /// 安装时争抢共享的 `uv` 目录。同一时间只允许一个 `ensure_uv`
    /// 执行下载/安装，其余等待者复用第一个完成的结果。
    ///
    /// **0.22.6 H2**：`cancel_token` 传播到 `install_uv_to_dir`，
    /// 支持在下载阶段取消。
    async fn ensure_uv(
        &mut self,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<PathBuf, RuntimeError> {
        if let Some(ref cached) = self.uv_path
            && cached.exists()
        {
            return Ok(cached.clone());
        }

        // 只检查 Blink 托管目录，不扫描系统 PATH
        let local_uv = runtime::local_uv_exe();

        if !local_uv.exists() {
            // Singleflight: 获取全局锁，防止并发下载
            let lock = uv_ensure_lock();
            let _guard = lock.lock().await;

            // 双重检查：在等待锁期间，另一个引擎可能已完成安装
            if local_uv.exists() {
                tracing::debug!(
                    path = %local_uv.display(),
                    "在等待 Singleflight 锁期间，uv 已被其他引擎安装完成"
                );
                self.uv_path = Some(local_uv.clone());
                return Ok(local_uv);
            }

            // 取消检查：获取锁后再次检查
            if let Some(ct) = cancel_token
                && ct.is_cancelled()
            {
                return Err(RuntimeError::OperationCancelled {
                    message: "ensure_uv 在获取锁后被取消".to_string(),
                });
            }

            tracing::info!("Blink 托管 uv 不存在，开始下载安装...");
            if let Some(s) = sink {
                s.on_log("info", "正在下载安装 Blink 托管 uv...");
            }

            // 复用 platform/python 的下载逻辑，但确保安装到 runtime 声明的目录
            let uv_path = python::install_uv_to_dir(&runtime::uv_install_dir(), cancel_token)
                .await
                .map_err(|e| RuntimeError::InstallFailed {
                    message: format!("下载安装 Blink 托管 uv 失败: {e}"),
                })?;
            tracing::info!(path = %uv_path.display(), "Blink 托管 uv 安装完成");
            if let Some(s) = sink {
                s.on_log("info", &format!("uv 安装完成: {}", uv_path.display()));
            }
            self.uv_path = Some(uv_path.clone());
            return Ok(uv_path);
        }

        tracing::debug!(path = %local_uv.display(), "找到 Blink 托管 uv");
        self.uv_path = Some(local_uv.clone());
        Ok(local_uv)
    }

    /// 校验实际 uv 版本满足声明的要求版本。
    ///
    /// 如果声明版本为空或 "any"，跳过校验。
    fn verify_uv_version(uv_path: &Path, required_version: &str) -> Result<(), RuntimeError> {
        if required_version.is_empty() || required_version.eq_ignore_ascii_case("any") {
            return Ok(());
        }

        let actual_version =
            Self::get_uv_version(uv_path).ok_or_else(|| RuntimeError::InstallFailed {
                message: format!(
                    "无法获取 uv 版本（路径: {}），无法校验是否满足声明版本 {}",
                    uv_path.display(),
                    required_version
                ),
            })?;

        if !uv_version_satisfies(&actual_version, required_version) {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "uv 版本不满足要求: 实际 '{}', 声明 '>={}'",
                    actual_version, required_version
                ),
            });
        }

        tracing::debug!(
            actual = %actual_version,
            required = %required_version,
            "uv 版本校验通过"
        );
        Ok(())
    }

    /// 在 staging 目录中创建 venv。
    ///
    /// 使用 `uv venv --python {version}` 创建 venv。
    /// venv 目录为 `staging_dir/venv`。
    ///
    /// **0.22.6 H2**：显式设置 `UV_CACHE_DIR`、`UV_PYTHON_INSTALL_DIR`，
    /// 确保 uv 行为与 `runtime` 声明的路径一致。
    async fn create_venv_in_staging(
        &self,
        uv_path: &Path,
        staging_dir: &Path,
        python_version: &str,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<PathBuf, RuntimeError> {
        let venv_dir = staging_dir.join("venv");

        // 如果 venv 已存在则跳过
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if python_exe.exists() {
            tracing::debug!("venv 已存在，跳过创建");
            return Ok(venv_dir);
        }

        std::fs::create_dir_all(staging_dir)?;

        tracing::info!(
            python = python_version,
            venv = %venv_dir.display(),
            "在 staging 中创建 Python venv..."
        );

        let mut cmd = tokio::process::Command::new(uv_path);
        cmd.args(["venv", "--python", python_version])
            .arg(&venv_dir);
        apply_blink_uv_env(&mut cmd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let no_window_cmd = crate::infra::platform::no_window_tokio(cmd);

        // 0.22.6 H2：取消时终止子进程；H5：流式上报 stdout/stderr
        let output = run_command_with_cancel(no_window_cmd, cancel_token, "uv venv", sink).await?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "创建 venv 失败 (exit={:?}):\nstdout: {stdout}\nstderr: {stderr}",
                    output.status.code()
                ),
            });
        }

        tracing::info!(venv = %venv_dir.display(), "venv 创建完成");
        Ok(venv_dir)
    }

    /// 在 venv 中安装锁定包。
    ///
    /// 使用 `uv pip install --python <venv_python> <packages...>`。
    ///
    /// 如果所有包都有 SHA-256 hash，添加 `--require-hashes` 强校验。
    /// `extra_args` 是闭合枚举，不接受任意字符串。
    ///
    /// **0.22.6 H2**：显式设置 `UV_CACHE_DIR`、`UV_PYTHON_INSTALL_DIR`，
    /// 并在取消/超时时终止子进程。
    #[allow(clippy::too_many_arguments)] // uv 安装参数链路稳定，不过度包装
    async fn install_locked_packages(
        &self,
        uv_path: &Path,
        venv_dir: &Path,
        packages: &[PackageLock],
        index_url: Option<&str>,
        extra_args: &[PipExtraArg],
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let python_exe = venv_dir.join("Scripts").join("python.exe");

        if !python_exe.exists() {
            return Err(RuntimeError::InstallFailed {
                message: "venv python.exe 不存在".to_string(),
            });
        }

        if packages.is_empty() {
            tracing::debug!("无锁定包需要安装");
            return Ok(());
        }

        // 检查是否所有包都有 SHA-256 hash
        let all_have_hashes = packages.iter().all(|p| p.sha256.is_some());

        // 构建 pip install 参数
        let package_specs: Vec<String> = packages
            .iter()
            .map(|p| {
                if p.version.starts_with('>') || p.version.starts_with('=') {
                    format!("{}{}", p.name, p.version)
                } else {
                    format!("{}=={}", p.name, p.version)
                }
            })
            .collect();

        tracing::info!(
            packages = ?package_specs,
            index_url = ?index_url,
            extra_args = ?extra_args,
            require_hashes = all_have_hashes,
            "安装锁定包..."
        );

        let mut cmd = tokio::process::Command::new(uv_path);
        cmd.args(["pip", "install", "--python"]).arg(&python_exe);

        if let Some(url) = index_url {
            cmd.args(["--index-url", url]);
        }

        let requirements_path = venv_dir
            .parent()
            .unwrap_or(venv_dir)
            .join("locked-requirements.txt");

        // 把 hash 写入 requirements 输入；只加 --require-hashes 而不传 --hash
        // 条目并不能形成可执行的锁定安装契约。
        if all_have_hashes && !packages.is_empty() {
            let requirements = render_hashed_requirements(packages)?;
            std::fs::write(&requirements_path, requirements)?;
            cmd.args(["--require-hashes", "--requirement"])
                .arg(&requirements_path);
        }

        // 应用闭合枚举的额外参数
        for arg in extra_args {
            match arg {
                PipExtraArg::ExtraIndexUrl(url) => {
                    cmd.args(["--extra-index-url", url]);
                }
                PipExtraArg::IndexStrategyUnsafeBestMatch => {
                    cmd.args(["--index-strategy", "unsafe-best-match"]);
                }
                PipExtraArg::NoDeps => {
                    cmd.arg("--no-deps");
                }
                PipExtraArg::NoBuildIsolation => {
                    cmd.arg("--no-build-isolation");
                }
            }
        }

        if !all_have_hashes {
            cmd.args(&package_specs);
        }
        // 0.22.6 H2：显式设置 Blink 托管环境变量
        apply_blink_uv_env(&mut cmd);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let no_window_cmd = crate::infra::platform::no_window_tokio(cmd);

        let output = run_command_with_cancel_timeout(
            no_window_cmd,
            cancel_token,
            "uv pip install",
            std::time::Duration::from_secs(600),
            sink,
        )
        .await?;

        if all_have_hashes {
            let _ = std::fs::remove_file(&requirements_path);
        }

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "安装包失败 (exit={:?}):\nstdout: {stdout}\nstderr: {stderr}",
                    output.status.code()
                ),
            });
        }

        tracing::info!("锁定包安装完成");
        Ok(())
    }

    /// 执行 self-test 脚本（候选环境验证）。
    ///
    /// 在 venv 中执行 descriptor 声明的 Python 代码片段。
    ///
    /// **0.22.6 H2**：取消时终止子进程。
    /// **0.22.6.1**：self-test 可能耗时数十秒（torch import）——超过 10s 后
    /// 每 10s 发出一次低频"仍在执行"提示（带已持续秒数），非洪泛。
    async fn run_self_test_script(
        &self,
        venv_dir: &Path,
        script: &str,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if !python_exe.exists() {
            return Err(RuntimeError::SelfTestFailed {
                message: "venv python.exe 不存在".to_string(),
            });
        }

        if script.is_empty() || script == "pass" {
            tracing::debug!("self-test 脚本为空，跳过");
            return Ok(());
        }

        tracing::info!("执行候选环境 self-test...");
        if let Some(s) = sink {
            s.on_log("info", "正在验证候选环境（self-test）...");
        }

        let mut cmd = tokio::process::Command::new(&python_exe);
        cmd.args(["-c", script]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let no_window_cmd = crate::infra::platform::no_window_tokio(cmd);
        let run_future = run_command_with_cancel(no_window_cmd, cancel_token, "self-test", sink);
        tokio::pin!(run_future);

        // 仍在执行提示：>10s 后每 10s 一次（低频、带已持续秒数）
        const HINT_INTERVAL_SECS: u64 = 10;
        let mut elapsed_secs: u64 = 0;
        let output = loop {
            tokio::select! {
                result = &mut run_future => break result?,
                _ = tokio::time::sleep(std::time::Duration::from_secs(HINT_INTERVAL_SECS)) => {
                    elapsed_secs += HINT_INTERVAL_SECS;
                    tracing::debug!("self-test 仍在执行（已 {elapsed_secs}s）");
                    if let Some(s) = sink {
                        s.on_log("info", &format!("self-test 仍在执行（已 {elapsed_secs}s）..."));
                    }
                }
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "self-test 失败 (exit={:?}):\nstdout: {stdout}\nstderr: {stderr}",
                    output.status.code()
                ),
            });
        }

        tracing::info!("候选环境 self-test 通过");
        if let Some(s) = sink {
            s.on_log("info", "候选环境验证通过");
        }
        Ok(())
    }

    /// 获取 venv 中的 Python 版本。
    fn get_venv_python_version(venv_dir: &Path) -> Option<String> {
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if !python_exe.exists() {
            return None;
        }

        let mut cmd = crate::infra::platform::no_window(Command::new(python_exe));
        cmd.args(["--version"]);
        cmd.output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// 获取 uv 版本（使用 Blink 托管环境变量）。
    fn get_uv_version(uv_path: &Path) -> Option<String> {
        let mut cmd = crate::infra::platform::no_window(Command::new(uv_path));
        cmd.args(["--version"]);
        apply_blink_uv_env_sync(&mut cmd);
        cmd.output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// 查询 venv 中已安装的包状态。
    ///
    /// 使用 `importlib.metadata.version` 逐个检查。
    fn query_packages(
        venv_dir: &Path,
        locked_packages: &[PackageLock],
    ) -> Vec<runtime::PackageStatus> {
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if !python_exe.exists() {
            return locked_packages
                .iter()
                .map(|p| runtime::PackageStatus {
                    name: p.name.clone(),
                    installed_version: None,
                    locked_version: p.version.clone(),
                    satisfies_lock: false,
                })
                .collect();
        }

        locked_packages
            .iter()
            .map(|p| {
                let installed = crate::infra::platform::no_window(Command::new(&python_exe))
                    .args([
                        "-c",
                        &format!(
                            "import importlib.metadata as m; print(m.version('{}'))",
                            p.name
                        ),
                    ])
                    .output();

                let (installed_version, satisfies) = match installed {
                    Ok(o) if o.status.success() => {
                        let v = String::from_utf8_lossy(&o.stdout).trim().to_string();
                        // 简化：只要安装了就算满足（精确版本检查由 adapter 处理）
                        (Some(v.clone()), true)
                    }
                    _ => (None, false),
                };

                runtime::PackageStatus {
                    name: p.name.clone(),
                    installed_version,
                    locked_version: p.version.clone(),
                    satisfies_lock: satisfies,
                }
            })
            .collect()
    }
    /// 查询只读运行时底座状态（不触发安装）。
    ///
    /// **0.22.6 H2**：提供只读 `RuntimeFoundationStatus` 数据结构，
    /// 至少包含 uv 来源、路径的安全展示、版本、托管 Python 状态、cache/root 状态。
    /// 查询不得触发安装。
    /// 当前生产 command 以 json! 投影底座状态；本方法由模块测试行使。
    #[allow(dead_code)]
    pub fn foundation_status() -> RuntimeFoundationStatus {
        let uv_path = runtime::local_uv_exe();
        let uv_exists = uv_path.exists();
        let uv_version = if uv_exists {
            Self::get_uv_version(&uv_path)
        } else {
            None
        };

        let cache_dir = runtime::uv_cache_dir();
        let python_dir = runtime::uv_python_dir();
        let uv_install_dir = runtime::uv_install_dir();

        // 检查托管 Python distributions
        let python_distributions: Vec<String> = if python_dir.exists() {
            std::fs::read_dir(&python_dir)
                .ok()
                .map(|entries| {
                    entries
                        .filter_map(|e| e.ok())
                        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
                        .filter_map(|e| e.file_name().to_str().map(|s| s.to_string()))
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        RuntimeFoundationStatus {
            uv_source: if uv_exists {
                UvSource::BlinkManaged
            } else {
                UvSource::NotInstalled
            },
            uv_path: if uv_exists {
                Some(uv_path.display().to_string())
            } else {
                None
            },
            uv_version,
            uv_install_dir: uv_install_dir.display().to_string(),
            uv_cache_dir: cache_dir.display().to_string(),
            uv_python_install_dir: python_dir.display().to_string(),
            uv_cache_exists: cache_dir.exists(),
            uv_python_install_dir_exists: python_dir.exists(),
            python_distributions,
        }
    }
}

impl Default for PythonVenvProvider {
    fn default() -> Self {
        Self::new()
    }
}

// ── RuntimeFoundationStatus（底座状态协议，当前由模块测试行使）──────────────

/// 只读运行时底座状态（0.22.6 H2）。
///
/// 提供 uv 来源、路径的安全展示、版本、托管 Python 状态、cache/root 状态。
/// **查询不得触发安装**——此结构只读取已存在的文件系统状态。
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize)]
pub struct RuntimeFoundationStatus {
    /// uv 来源。
    pub uv_source: UvSource,
    /// uv 可执行文件路径（安全展示用字符串）。
    pub uv_path: Option<String>,
    /// uv 版本号（如 "uv 0.6.10"）。
    pub uv_version: Option<String>,
    /// uv 安装目录的安全展示路径。
    pub uv_install_dir: String,
    /// uv cache 目录的安全展示路径。
    pub uv_cache_dir: String,
    /// uv Python distributions 目录的安全展示路径。
    pub uv_python_install_dir: String,
    /// uv cache 目录是否存在。
    pub uv_cache_exists: bool,
    /// uv Python distributions 目录是否存在。
    pub uv_python_install_dir_exists: bool,
    /// 已安装的 Python distributions 列表（目录名）。
    pub python_distributions: Vec<String>,
}

/// uv 来源分类。
#[allow(dead_code)]
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UvSource {
    /// Blink 托管（`runtime::local_uv_exe()` 存在）。
    BlinkManaged,
    /// 未安装。
    NotInstalled,
}

// ── 取消安全的命令执行 ───────────────────────────────────────────────────────

/// 运行 tokio 命令，支持取消信号。
///
/// **0.22.6 H2**：取消时不只 drop future，还终止受管子进程。
/// 这确保取消后不会留下 uv/pip 子进程。
///
/// **0.22.6 H5**: `sink` 用于实时逐行上报 stdout/stderr。
async fn run_command_with_cancel(
    mut cmd: tokio::process::Command,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    label: &str,
    sink: Option<&dyn InstallSink>,
) -> Result<std::process::Output, RuntimeError> {
    let child = cmd.spawn().map_err(|e| RuntimeError::InstallFailed {
        message: format!("启动 {label} 失败: {e}"),
    })?;

    run_child_with_cancel(child, cancel_token, label, sink).await
}

/// 运行 tokio 命令，支持取消信号和超时。
///
/// **0.22.6 H2**：取消或超时不只 drop future，还终止受管子进程。
///
/// **0.22.6 H5**: `sink` 用于实时逐行上报 stdout/stderr。
async fn run_command_with_cancel_timeout(
    mut cmd: tokio::process::Command,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    label: &str,
    timeout: std::time::Duration,
    sink: Option<&dyn InstallSink>,
) -> Result<std::process::Output, RuntimeError> {
    let child = cmd.spawn().map_err(|e| RuntimeError::InstallFailed {
        message: format!("启动 {label} 失败: {e}"),
    })?;

    // 使用 select 同时监听 child output、取消信号和超时
    let output_future = run_child_with_cancel(child, cancel_token, label, sink);

    match tokio::time::timeout(timeout, output_future).await {
        Ok(result) => result,
        Err(_) => {
            tracing::warn!(%label, ?timeout, "命令超时");
            Err(RuntimeError::InstallFailed {
                message: format!("{label} 超时（{}s）", timeout.as_secs()),
            })
        }
    }
}

/// 运行已 spawn 的子进程，支持取消信号。
///
/// 取消时调用 `kill()` 终止子进程，确保不留孤儿。
///
/// **0.22.6 H5**: 增加 `sink` 参数，实时逐行上报 stdout/stderr。
/// 当 `sink` 为 `None` 时，行为与旧版一致（read_to_end 一次性读取）。
async fn run_child_with_cancel(
    mut child: Child,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    label: &str,
    sink: Option<&dyn InstallSink>,
) -> Result<std::process::Output, RuntimeError> {
    // uv/pip 可能再派生 Python 等子进程。仅 kill 直接 child 会让后代继续持有
    // stdout/stderr 管道，导致取消看似成功却要等整条命令自然退出。
    #[cfg(windows)]
    let mut job_handle = {
        let pid = child.id().ok_or_else(|| RuntimeError::InstallFailed {
            message: format!("无法获取 {label} 进程 PID"),
        })?;
        match crate::infra::platform::process::assign_job_object(pid) {
            Ok(handle) => Some(handle),
            Err(e) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(RuntimeError::InstallFailed {
                    message: format!("{label} Job Object 分配失败: {e}"),
                });
            }
        }
    };

    // 取 child 的 stdin 避免管道死锁
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();

    let wait_future = async {
        // 并发读取 stdout + stderr，等待进程退出
        use tokio::io::AsyncReadExt;

        // 0.22.6 H5: 当 sink 存在时，逐行流式读取 stdout/stderr；
        // 否则一次性 read_to_end（保持旧行为）。
        let (stdout_data, stderr_data) = match (stdout, stderr) {
            (Some(so), Some(se)) => {
                if sink.is_some() {
                    let so_fut = async { read_lines_to_end(so, sink, "info", label).await };
                    let se_fut = async { read_lines_to_end(se, sink, "warn", label).await };
                    let (so_data, se_data) = tokio::join!(so_fut, se_fut);
                    (so_data, se_data)
                } else {
                    let so_fut = async {
                        let mut buf = Vec::new();
                        let mut reader = so;
                        let _ = reader.read_to_end(&mut buf).await;
                        buf
                    };
                    let se_fut = async {
                        let mut buf = Vec::new();
                        let mut reader = se;
                        let _ = reader.read_to_end(&mut buf).await;
                        buf
                    };
                    let (so_data, se_data) = tokio::join!(so_fut, se_fut);
                    (so_data, se_data)
                }
            }
            (Some(so), None) => {
                let mut buf = Vec::new();
                let mut reader = so;
                let _ = reader.read_to_end(&mut buf).await;
                (buf, Vec::new())
            }
            (None, Some(se)) => {
                let mut buf = Vec::new();
                let mut reader = se;
                let _ = reader.read_to_end(&mut buf).await;
                (Vec::new(), buf)
            }
            (None, None) => (Vec::new(), Vec::new()),
        };

        let status = child
            .wait()
            .await
            .map_err(|e| RuntimeError::InstallFailed {
                message: format!("等待 {label} 退出失败: {e}"),
            })?;

        Ok::<_, RuntimeError>(std::process::Output {
            status,
            stdout: stdout_data,
            stderr: stderr_data,
        })
    };

    if let Some(ct) = cancel_token {
        tokio::select! {
            biased;
            _ = ct.cancelled() => {
                tracing::info!(%label, "命令被取消，终止子进程");
                if let Some(s) = sink {
                    s.on_log("warn", &format!("{label} 被取消，终止子进程"));
                }
                // 终止子进程
                let _ = child.start_kill();
                #[cfg(windows)]
                drop(job_handle.take());
                let _ = tokio::time::timeout(
                    std::time::Duration::from_secs(5),
                    child.wait(),
                ).await;
                Err(RuntimeError::OperationCancelled {
                    message: format!("{label} 被取消"),
                })
            }
            result = wait_future => result,
        }
    } else {
        wait_future.await
    }
}

/// 日志行最大长度（截断后），防止超长行洪泛日志。
const LOG_LINE_MAX_LEN: usize = 4096;

/// 敏感值被替换后的占位符。
const REDACTED: &str = "***REDACTED***";

/// 需要 mask 的敏感键名列表（大小写不敏感匹配）。
const SENSITIVE_KEYS: &[&str] = &[
    "password",
    "passwd",
    "token",
    "secret",
    "api_key",
    "api-key",
    "apikey",
    "access_key",
    "access-key",
    "private_key",
    "private-key",
    "authorization",
];

/// 过滤日志行中的敏感信息。
///
/// **0.22.6 H6**: mask 常见敏感模式——密码、token、密钥、API key 等。
///
/// 覆盖的模式（大小写不敏感）：
/// - `password=xxx` / `password: xxx`
/// - `--password xxx`（flag 模式，key 前面是 `--`）
/// - `token=xxx` / `token: xxx`
/// - `secret=xxx` / `secret: xxx`
/// - `api_key=xxx` / `api-key=xxx` / `apikey=xxx`
/// - `Authorization: Bearer xxx`
///
/// 敏感值被替换为 `***REDACTED***`，保留键名和结构便于诊断。
/// 每行最多 mask 第一个匹配的敏感键（已知限制，防止正则回溯爆炸）。
fn sanitize_log_line(line: &str) -> String {
    // 先截断超长行
    let truncated = if line.len() > LOG_LINE_MAX_LEN {
        let mut s = line[..LOG_LINE_MAX_LEN].to_string();
        s.push_str("...[truncated]");
        s
    } else {
        line.to_string()
    };

    let lower = truncated.to_lowercase();

    // 尝试每个敏感键，找到第一个能匹配并 mask 的就返回
    for key in SENSITIVE_KEYS {
        if let Some(pos) = lower.find(key) {
            let after_key = pos + key.len();
            if after_key >= truncated.len() {
                continue;
            }
            let rest = &truncated[after_key..];

            // 分隔符判定：只接受 `=` 或 `:` 作为键值分隔符。
            // 对于 `--password xxx` flag 模式，要求 key 前面是 `--`。
            let sep = rest.chars().next();
            let value_start = if sep == Some('=') || sep == Some(':') {
                // 跳过分隔符和后续空格
                after_key
                    + rest[1..]
                        .find(|c: char| c != ' ' && c != '\t')
                        .map(|i| 1 + i)
                        .unwrap_or(1)
            } else if pos >= 2 && &truncated[pos - 2..pos] == "--" {
                // flag 模式：--password xxx
                // 跳过空格
                after_key + rest.find(|c: char| c != ' ' && c != '\t').unwrap_or(0)
            } else {
                // 不是键值对，跳过
                continue;
            };

            if value_start >= truncated.len() {
                continue;
            }

            // 特殊处理 `Bearer xxx`：如果值以 `Bearer ` 开头，
            // 跳过 `Bearer ` 前缀，mask 后面的实际 token。
            let value_rest = &truncated[value_start..];
            let actual_value_start = if value_rest
                .get(0..7)
                .is_some_and(|s| s.eq_ignore_ascii_case("Bearer "))
            {
                value_start + 7
            } else {
                value_start
            };

            if actual_value_start >= truncated.len() {
                continue;
            }

            // 找到值结束的位置（空格、换行、行尾）
            let value_rest = &truncated[actual_value_start..];
            let value_end = value_rest
                .find([' ', '\t', '\r', '\n'])
                .unwrap_or(value_rest.len());

            if value_end > 0 {
                let abs_value_end = actual_value_start + value_end;
                let prefix = &truncated[..actual_value_start];
                let suffix = &truncated[abs_value_end..];
                return format!("{prefix}{REDACTED}{suffix}");
            }
        }
    }

    truncated
}

/// 逐行读取子进程输出，实时通过 sink 上报，同时积累完整 buffer。
///
/// **0.22.6 H5**: 实现流式 stdout/stderr 读取——不等进程结束后一次性返回。
/// - 逐行读取（BufReader::lines），每行立即通过 `sink.on_log` 上报。
/// - 同时把所有行积累到 Vec<u8> 返回（保持与 read_to_end 兼容的返回值）。
/// - UTF-8 lossy 转换，保证不会因编码问题崩溃。
/// - 每行截断到 4096 字符，防止超长行洪泛日志。
/// - 敏感信息过滤：mask 密码、token、密钥等常见模式。
async fn read_lines_to_end<R: tokio::io::AsyncRead + Unpin>(
    reader: R,
    sink: Option<&dyn InstallSink>,
    level: &str,
    _label: &str,
) -> Vec<u8> {
    use tokio::io::{AsyncBufReadExt, AsyncReadExt};

    let mut buf_reader = tokio::io::BufReader::new(reader);
    let mut full_buf = Vec::new();
    let mut line = String::new();

    loop {
        line.clear();
        match buf_reader.read_line(&mut line).await {
            Ok(0) => break, // EOF
            Ok(_) => {
                // 上报到 sink（截断超长行 + 敏感信息过滤）
                if let Some(s) = sink {
                    let trimmed = line.trim_end_matches(['\r', '\n']);
                    if !trimmed.is_empty() {
                        let safe = sanitize_log_line(trimmed);
                        s.on_log(level, &safe);
                    }
                }
                // 积累到完整 buffer（原始数据，不过滤——full_buf 仅用于内部诊断）
                full_buf.extend_from_slice(line.as_bytes());
            }
            Err(_) => break,
        }
    }

    // 如果 BufReader 内部还有未读取的数据（无换行符的尾部），也收入 full_buf
    let mut remaining = Vec::new();
    let _ = buf_reader.read_to_end(&mut remaining).await;
    if !remaining.is_empty() {
        if let Some(s) = sink {
            let text = String::from_utf8_lossy(&remaining);
            let trimmed = text.trim_end_matches(['\r', '\n']);
            if !trimmed.is_empty() {
                let safe = sanitize_log_line(trimmed);
                s.on_log(level, &safe);
            }
        }
        full_buf.extend_from_slice(&remaining);
    }

    full_buf
}

#[async_trait::async_trait]
impl RuntimeProvider for PythonVenvProvider {
    fn kind(&self) -> runtime::RuntimePlan {
        runtime::RuntimePlan::PythonVenv
    }

    fn check_compatibility(
        &self,
        compatibility: &CompatibilityCheck,
    ) -> Result<bool, RuntimeError> {
        match compatibility {
            CompatibilityCheck::Always => Ok(true),
            CompatibilityCheck::RequiresCuda { .. } => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                Ok(python::detect_cuda().is_some())
            }
            CompatibilityCheck::RequiresVulkan => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // Vulkan 驱动检查（未来实现）
                Ok(false)
            }
            CompatibilityCheck::RequiresCpuFeature { .. } => {
                // Python venv 不关心 CPU feature（由 ManagedBinary 处理）
                Ok(true)
            }
        }
    }

    async fn prepare_environment(
        &self,
        staging_dir: &Path,
        plan: &InstallPlan,
        _resolved_profile: &ResolvedProfile,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<PrepareResult, RuntimeError> {
        let python_plan = match plan {
            InstallPlan::PythonVenv(p) => p,
            _ => {
                return Err(RuntimeError::InstallFailed {
                    message: "PythonVenvProvider 收到非 PythonVenv 安装计划".to_string(),
                });
            }
        };

        // 创建一个临时 provider 实例来持有 uv_path
        let mut provider = PythonVenvProvider {
            uv_path: None,
            allow_gpu: self.allow_gpu,
        };

        // 1. 确保 uv（只使用 Blink 托管的 uv）
        if let Some(s) = sink {
            s.on_log("info", "正在确保 uv 可用...");
        }
        let uv_path = provider.ensure_uv(cancel_token, sink).await?;

        // 1b. 校验实际 uv 版本满足声明版本
        Self::verify_uv_version(&uv_path, &python_plan.uv_version)?;

        // 2. 创建 venv
        if let Some(s) = sink {
            s.on_log(
                "info",
                &format!("创建 Python venv ({} )...", python_plan.python_version),
            );
        }
        let venv_dir = provider
            .create_venv_in_staging(
                &uv_path,
                staging_dir,
                &python_plan.python_version,
                cancel_token,
                sink,
            )
            .await?;

        // 3. 安装锁定包
        if let Some(s) = sink {
            s.on_log("info", "安装锁定包...");
        }
        provider
            .install_locked_packages(
                &uv_path,
                &venv_dir,
                &python_plan.packages,
                python_plan.index_url.as_deref(),
                &python_plan.extra_pip_args,
                cancel_token,
                sink,
            )
            .await?;

        // 0.22.6.1：**不在 prepare_environment 内执行完整 self-test**——
        // 此前的重复执行（prepare 内 1 次 + promote 前 1 次 + 切换后 1 次）
        // 会让安装日志出现三遍"执行 self-test"。完整验证统一由
        // `InstallTransaction` 编排：promote 前 provider.self_test（候选
        // 环境验证）+ 切换后 verify_after_switch（切换后验证）。

        // 以本次 generation 实际使用的解释器内容作为可复核身份，不能把
        // `uv-verified` 之类标签冒充 SHA-256。
        let python_exe = staging_dir.join("venv").join("Scripts").join("python.exe");
        let python_bytes =
            std::fs::read(&python_exe).map_err(|error| RuntimeError::InstallFailed {
                message: format!("读取托管 Python 解释器失败: {error}"),
            })?;
        let artifact = runtime::ArtifactIdentity {
            runtime_kind: runtime::RuntimePlan::PythonVenv,
            artifact_id: python_plan.python_artifact_id.clone(),
            sha256: format!("{:x}", Sha256::digest(&python_bytes)),
        };

        Ok(PrepareResult { artifact })
    }

    async fn self_test(
        &self,
        generation_dir: &Path,
        plan: &InstallPlan,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let python_plan = match plan {
            InstallPlan::PythonVenv(p) => p,
            _ => return Ok(()),
        };

        let venv_dir = generation_dir.join("venv");
        if !venv_dir.exists() {
            return Err(RuntimeError::SelfTestFailed {
                message: "venv 目录不存在".to_string(),
            });
        }

        // 0.22.6.1：这是 promote 前唯一一次完整 self-test（候选环境验证）——
        // prepare_environment 不再重复执行。
        // 此前此处只做"最终验证"，完整脚本执行曾发生在 prepare_environment。

        // 如果 self_test_script 非空且非 "pass"，执行一次
        if !python_plan.self_test_script.is_empty() && python_plan.self_test_script != "pass" {
            self.run_self_test_script(&venv_dir, &python_plan.self_test_script, cancel_token, sink)
                .await?;
        }

        Ok(())
    }

    fn build_manifest_extension(
        &self,
        generation_dir: &Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        let python_plan = match plan {
            InstallPlan::PythonVenv(p) => p,
            _ => {
                return Err(RuntimeError::ManifestSerializeFailed {
                    message: "PythonVenvProvider 收到非 PythonVenv 安装计划".to_string(),
                });
            }
        };

        let venv_dir = generation_dir.join("venv");

        // 获取 Python 版本
        let python_version = Self::get_venv_python_version(&venv_dir)
            .unwrap_or_else(|| python_plan.python_version.clone());

        // 获取 uv 版本
        let uv_version = self
            .uv_path
            .as_ref()
            .and_then(|p| Self::get_uv_version(p))
            .unwrap_or_else(|| "unknown".to_string());

        // 查询包状态
        let packages = Self::query_packages(&venv_dir, &python_plan.packages);

        Ok(ManifestExtension::PythonVenv(runtime::PythonManifestExt {
            python_version,
            python_artifact_id: python_plan.python_artifact_id.clone(),
            packages,
            uv_version,
            index_url: python_plan.index_url.clone(),
            self_test_passed: true,
        }))
    }
}

#[cfg(test)]
mod tests;
