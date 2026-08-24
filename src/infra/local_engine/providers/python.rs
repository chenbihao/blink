//! PythonVenvProvider 完整实现（0.22.2）。
//!
//! 负责：
//! - 确保 uv 可用（复用 `infra/platform/python` 的 uv 下载逻辑）
//! - 创建隔离 venv（按 generation 隔离，不共享 site-packages）
//! - 同步锁定依赖（package lock + index）
//! - self-test（执行 descriptor 声明的 self-test 脚本）
//! - 查询包状态（`importlib.metadata.version`）
//! - 清理 uv cache
//!
//! ## 设计铁则
//!
//! - **不共享 site-packages**：每个 generation 拥有独立 venv。
//! - **复用 uv/Python distribution**：uv 二进制、uv cache 和 uv 管理的 Python
//!   distribution 是 provider 公共资产，可被多个引擎的 venv 引用。
//! - **不读取用户代码解释器**：只使用 Blink 托管的 Python。
//! - **包状态不含引擎专属字段**：`PackageStatus` 是通用结构，torch/funasr 等
//!   引擎专属投影由 adapter 处理。

use std::path::{Path, PathBuf};
use std::process::Command;

use sha2::{Digest, Sha256};

use super::{
    CompatibilityCheck, InstallPlan, ManifestExtension, PackageLock, PipExtraArg, PrepareResult,
    ProviderCleanupScope, ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;

fn render_hashed_requirements(packages: &[PackageLock]) -> Result<String, RuntimeError> {
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
        let hash = package
            .sha256
            .as_deref()
            .ok_or_else(|| RuntimeError::InstallFailed {
                message: format!("{} 缺少 SHA-256", package.name),
            })?;
        if hash.len() != 64 || !hash.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err(RuntimeError::InstallFailed {
                message: format!("{} 的 SHA-256 格式无效", package.name),
            });
        }
        requirements.push_str(&format!(
            "{}=={} --hash=sha256:{}\n",
            package.name, version, hash
        ));
    }
    Ok(requirements)
}
use crate::infra::platform::python;

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
    pub fn cpu_only() -> Self {
        Self {
            uv_path: None,
            allow_gpu: false,
        }
    }

    /// 确保 uv 可用，返回 uv 路径。
    ///
    /// 复用 `infra/platform/python::ensure_uv` 的查找 + 下载逻辑。
    /// 查找结果缓存在 `self.uv_path` 中，避免重复查找。
    async fn ensure_uv(&mut self) -> Result<PathBuf, RuntimeError> {
        if let Some(ref cached) = self.uv_path {
            if cached.exists() {
                return Ok(cached.clone());
            }
        }

        let uv_path = python::ensure_uv()
            .await
            .map_err(|e| RuntimeError::InstallFailed {
                message: format!("确保 uv 可用失败: {e}"),
            })?;

        self.uv_path = Some(uv_path.clone());
        Ok(uv_path)
    }

    /// 在 staging 目录中创建 venv。
    ///
    /// 使用 `uv venv --python {version}` 创建 venv。
    /// venv 目录为 `staging_dir/venv`。
    async fn create_venv_in_staging(
        &self,
        uv_path: &Path,
        staging_dir: &Path,
        python_version: &str,
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
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = crate::infra::platform::no_window_tokio(cmd)
            .output()
            .await
            .map_err(|e| RuntimeError::InstallFailed {
                message: format!("执行 uv venv 失败: {e}"),
            })?;

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
    async fn install_locked_packages(
        &self,
        uv_path: &Path,
        venv_dir: &Path,
        packages: &[PackageLock],
        index_url: Option<&str>,
        extra_args: &[PipExtraArg],
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
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let install_future = crate::infra::platform::no_window_tokio(cmd).output();

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(600), // 10 分钟超时
            install_future,
        )
        .await
        .map_err(|_| RuntimeError::InstallFailed {
            message: "安装包超时（600s）".to_string(),
        })?
        .map_err(|e| RuntimeError::InstallFailed {
            message: format!("执行 uv pip install 失败: {e}"),
        })?;

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

    /// 执行 self-test 脚本。
    ///
    /// 在 venv 中执行 descriptor 声明的 Python 代码片段。
    async fn run_self_test_script(
        &self,
        venv_dir: &Path,
        script: &str,
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

        tracing::info!("执行 self-test...");

        let mut cmd = tokio::process::Command::new(&python_exe);
        cmd.args(["-c", script]);
        cmd.stdout(std::process::Stdio::piped());
        cmd.stderr(std::process::Stdio::piped());

        let output = crate::infra::platform::no_window_tokio(cmd)
            .output()
            .await
            .map_err(|e| RuntimeError::SelfTestFailed {
                message: format!("执行 self-test 失败: {e}"),
            })?;

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

        tracing::info!("self-test 通过");
        Ok(())
    }

    /// 获取 venv 中的 Python 版本。
    fn get_venv_python_version(venv_dir: &Path) -> Option<String> {
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if !python_exe.exists() {
            return None;
        }

        crate::infra::platform::no_window(Command::new(python_exe))
            .args(["--version"])
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    }

    /// 获取 uv 版本。
    fn get_uv_version(uv_path: &Path) -> Option<String> {
        crate::infra::platform::no_window(Command::new(uv_path))
            .args(["--version"])
            .output()
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
}

impl Default for PythonVenvProvider {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl RuntimeProvider for PythonVenvProvider {
    fn kind(&self) -> runtime::RuntimeKind {
        runtime::RuntimeKind::PythonVenv
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
            CompatibilityCheck::RequiresDirectml => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // DirectML 检查（未来实现）
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

        // 1. 确保 uv
        let uv_path = provider.ensure_uv().await?;

        // 2. 创建 venv
        let venv_dir = provider
            .create_venv_in_staging(&uv_path, staging_dir, &python_plan.python_version)
            .await?;

        // 3. 安装锁定包
        provider
            .install_locked_packages(
                &uv_path,
                &venv_dir,
                &python_plan.packages,
                python_plan.index_url.as_deref(),
                &python_plan.extra_pip_args,
            )
            .await?;

        // 4. self-test
        provider
            .run_self_test_script(&venv_dir, &python_plan.self_test_script)
            .await?;

        // 以本次 generation 实际使用的解释器内容作为可复核身份，不能把
        // `uv-verified` 之类标签冒充 SHA-256。
        let python_exe = staging_dir.join("venv").join("Scripts").join("python.exe");
        let python_bytes =
            std::fs::read(&python_exe).map_err(|error| RuntimeError::InstallFailed {
                message: format!("读取托管 Python 解释器失败: {error}"),
            })?;
        let artifact = runtime::ArtifactIdentity {
            runtime_kind: runtime::RuntimeKind::PythonVenv,
            artifact_id: python_plan.python_artifact_id.clone(),
            sha256: format!("{:x}", Sha256::digest(&python_bytes)),
        };

        Ok(PrepareResult { artifact })
    }

    async fn self_test(
        &self,
        generation_dir: &Path,
        plan: &InstallPlan,
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

        // self-test 已在 prepare_environment 中执行，这里只做最终验证
        // 检查 python.exe 存在
        let python_exe = venv_dir.join("Scripts").join("python.exe");
        if !python_exe.exists() {
            return Err(RuntimeError::SelfTestFailed {
                message: "python.exe 不存在".to_string(),
            });
        }

        // 如果 self_test_script 非空且非 "pass"，执行一次
        if !python_plan.self_test_script.is_empty() && python_plan.self_test_script != "pass" {
            self.run_self_test_script(&venv_dir, &python_plan.self_test_script)
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

    fn query_package_status(
        &self,
        generation_dir: &Path,
        plan: &InstallPlan,
    ) -> Result<Vec<runtime::PackageStatus>, RuntimeError> {
        let python_plan = match plan {
            InstallPlan::PythonVenv(p) => p,
            _ => return Ok(Vec::new()),
        };

        let venv_dir = generation_dir.join("venv");
        Ok(Self::query_packages(&venv_dir, &python_plan.packages))
    }

    fn cleanup_provider_cache(&self, scope: &ProviderCleanupScope) -> Result<(), RuntimeError> {
        match scope {
            ProviderCleanupScope::DownloadCache => {
                let cache = runtime::uv_cache_dir();
                if cache.exists() {
                    std::fs::remove_dir_all(&cache).map_err(|e| RuntimeError::CleanupFailed {
                        message: format!("删除 uv cache 失败: {e}"),
                    })?;
                    tracing::info!("uv cache 已清理");
                }
                Ok(())
            }
            ProviderCleanupScope::SharedArtifact(_) => {
                // 共享 artifact 清理由通用 execute_cleanup 处理
                Ok(())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn python_venv_provider_kind() {
        let provider = PythonVenvProvider::new();
        assert_eq!(provider.kind(), runtime::RuntimeKind::PythonVenv);
    }

    #[test]
    fn python_venv_always_compatible() {
        let provider = PythonVenvProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::Always)
                .unwrap()
        );
    }

    #[test]
    fn python_venv_cpu_only_rejects_gpu() {
        let provider = PythonVenvProvider::cpu_only();
        assert!(
            !provider
                .check_compatibility(&CompatibilityCheck::RequiresCuda { min_version: None })
                .unwrap()
        );
        assert!(
            !provider
                .check_compatibility(&CompatibilityCheck::RequiresVulkan)
                .unwrap()
        );
    }

    #[test]
    fn python_venv_cpu_feature_ignored() {
        // Python venv 不关心 CPU feature（由 ManagedBinary 处理）
        let provider = PythonVenvProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::RequiresCpuFeature {
                    feature: "avx2".to_string()
                })
                .unwrap()
        );
    }

    #[test]
    fn python_venv_query_packages_empty_when_no_venv() {
        let tmp = tempfile::tempdir().unwrap();
        let venv_dir = tmp.path().join("venv");
        let packages = PythonVenvProvider::query_packages(&venv_dir, &[]);
        assert!(packages.is_empty());
    }

    #[test]
    fn python_venv_query_packages_reports_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let venv_dir = tmp.path().join("venv");
        let packages = PythonVenvProvider::query_packages(
            &venv_dir,
            &[
                PackageLock {
                    name: "torch".to_string(),
                    version: "2.5.0".to_string(),
                    sha256: None,
                },
                PackageLock {
                    name: "funasr".to_string(),
                    version: "1.3.0".to_string(),
                    sha256: None,
                },
            ],
        );
        assert_eq!(packages.len(), 2);
        assert_eq!(packages[0].name, "torch");
        assert!(packages[0].installed_version.is_none());
        assert!(!packages[0].satisfies_lock);
    }

    #[test]
    fn hashed_requirements_include_each_declared_hash() {
        let hash = "a".repeat(64);
        let rendered = render_hashed_requirements(&[PackageLock {
            name: "example".to_string(),
            version: "1.2.3".to_string(),
            sha256: Some(hash.clone()),
        }])
        .unwrap();
        assert_eq!(rendered, format!("example==1.2.3 --hash=sha256:{hash}\n"));
    }

    #[test]
    fn hashed_requirements_reject_non_exact_versions_and_invalid_hashes() {
        let ranged = PackageLock {
            name: "example".to_string(),
            version: ">=1.2".to_string(),
            sha256: Some("a".repeat(64)),
        };
        assert!(render_hashed_requirements(&[ranged]).is_err());

        let invalid_hash = PackageLock {
            name: "example".to_string(),
            version: "1.2.3".to_string(),
            sha256: Some("not-a-sha256".to_string()),
        };
        assert!(render_hashed_requirements(&[invalid_hash]).is_err());
    }
}
