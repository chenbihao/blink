//! ManagedBinary Provider（0.22.2 协议位 → 0.22.7 首个真实实现）。
//!
//! 首个真实 binary 引擎是 FunASR GGUF worker：三个 exe 由
//! `cargo xtask funasr-worker` 从锁定源码（FunASR commit 55b662c ==
//! runtime-llamacpp-v0.2.6 + llama.cpp 803b7fc，MIT）构建，随 Blink 发布
//! 捆绑在 `resources/bin/funasr-worker/`（tauri bundle resource），并携带
//! 同目录 `manifest.json`（文件 SHA-256，构建期生成、安装期校验）。
//!
//! ## 设计铁则
//!
//! - **bundled 安装无网络**：文件来自发布资源目录，manifest hash 是唯一
//!   完整性真源（exe 逐机器构建，hash 随发布而非随仓库走）。
//! - **可复现来源锁定**：仓库内 `resources/stt/funasr-gguf/worker-lock.json`
//!   记录源码 pin（commit/zip sha256/许可），由 release-check 校验。
//! - **self-test 真实执行**：`<exe> --blink-selftest` 输出协议版本 JSON，
//!   解析核对后才算 self-test 通过——不用"文件存在"冒充。
//! - 不创建 venv、不执行 pip、不读取用户代码解释器。

use std::path::Path;

use sha2::{Digest, Sha256};

use super::{
    CompatibilityCheck, InstallPlan, InstallSink, ManifestExtension, PrepareResult,
    ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;

/// ManagedBinary Provider。
pub struct ManagedBinaryProvider {
    /// 是否允许 GPU backend（测试时可关闭）。
    allow_gpu: bool,
}

impl ManagedBinaryProvider {
    /// 创建 ManagedBinaryProvider。
    pub fn new() -> Self {
        Self { allow_gpu: true }
    }

    /// 创建只允许 CPU 的 ManagedBinaryProvider（测试用）。
    #[allow(dead_code)]
    pub fn cpu_only() -> Self {
        Self { allow_gpu: false }
    }
}

impl Default for ManagedBinaryProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 捆绑资源目录的解析结果。
struct BundledSource {
    dir: std::path::PathBuf,
    /// 文件名 → sha256（来自随发布 manifest.json）。
    files: Vec<(String, String)>,
}

/// 定位捆绑 worker 目录。
///
/// 委托 app 层注入的解析器？——不：infra 不依赖 app。这里按固定候选布局
/// 解析（exe 同级 / 上溯仓库根 / resources 子目录），与 app 层
/// `resolve_bundled_worker_dir` 保持一致布局常量。
fn resolve_bundled_dir(relative: &str) -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let exe_dir = exe.parent()?;
    let rel = std::path::Path::new(relative);
    // dev 布局上溯：target/debug[/deps] → 仓库根。release 布局：安装目录同级。
    let candidates = [
        exe_dir.join(rel),
        exe_dir
            .parent()
            .and_then(|p| p.parent())
            .map(|root| root.join("resources").join(rel))?,
        // 测试二进制位于 target/debug/deps/ —— 需再上溯一级
        exe_dir
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .map(|root| root.join("resources").join(rel))?,
        exe_dir.join("resources").join(rel),
    ];
    candidates
        .into_iter()
        .find(|d| d.join("manifest.json").is_file())
}

fn sha256_file(path: &Path) -> Result<String, RuntimeError> {
    let mut hasher = Sha256::new();
    let data = std::fs::read(path)?;
    hasher.update(&data);
    Ok(hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect())
}

/// 读取随发布 manifest.json 并校验所有 exe hash。
fn verify_bundled_source(dir: &Path) -> Result<BundledSource, RuntimeError> {
    let manifest_path = dir.join("manifest.json");
    let text =
        std::fs::read_to_string(&manifest_path).map_err(|e| RuntimeError::InstallFailed {
            message: format!("读取 {} 失败: {e}", manifest_path.display()),
        })?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| RuntimeError::InstallFailed {
            message: format!("manifest.json 解析失败: {e}"),
        })?;
    if v.get("schema").and_then(|s| s.as_u64()) != Some(1) {
        return Err(RuntimeError::InstallFailed {
            message: "worker manifest schema 不支持（期望 1）".to_string(),
        });
    }

    let mut files = Vec::new();
    if let Some(map) = v.get("files").and_then(|f| f.as_object()) {
        for (name, meta) in map {
            let Some(sha) = meta.get("sha256").and_then(|s| s.as_str()) else {
                continue;
            };
            files.push((name.clone(), sha.to_string()));
        }
    }
    if files.is_empty() {
        return Err(RuntimeError::InstallFailed {
            message: "worker manifest 未包含文件条目".to_string(),
        });
    }

    // 逐文件 hash 校验
    for (name, expected) in &files {
        let path = dir.join(name);
        if !path.is_file() {
            return Err(RuntimeError::InstallFailed {
                message: format!("捆绑 worker 文件缺失: {name}"),
            });
        }
        let actual = sha256_file(&path)?;
        if &actual != expected {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "worker {name} SHA-256 不匹配（manifest={expected} actual={actual}）"
                ),
            });
        }
    }
    Ok(BundledSource {
        dir: dir.to_path_buf(),
        files,
    })
}

#[async_trait::async_trait]
impl RuntimeProvider for ManagedBinaryProvider {
    fn kind(&self) -> runtime::RuntimePlan {
        runtime::RuntimePlan::ManagedBinary
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
                // 检查 nvidia-smi
                Ok(crate::infra::platform::python::detect_cuda().is_some())
            }
            CompatibilityCheck::RequiresVulkan => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // Vulkan 驱动检查（未来实现）
                // 目前保守返回 false
                Ok(false)
            }
            CompatibilityCheck::RequiresCpuFeature { feature } => {
                // CPU feature 检查（如 AVX2）
                // Windows 上可通过 IsProcessorFeaturePresent 检查
                match feature.as_str() {
                    "avx2" => Ok(check_avx2()),
                    "avx" => Ok(check_avx()),
                    "sse2" => Ok(true), // x64 默认支持 SSE2
                    _ => Ok(false),
                }
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
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::InstallFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        std::fs::create_dir_all(staging_dir)?;

        if let Some(s) = sink {
            s.on_stage("verifying");
        }

        // 1. 解析来源（bundled 或网络——bundled 是首个真实实现）
        let Some(bundled_relative) = &binary_plan.bundled_dir else {
            return Err(RuntimeError::InstallFailed {
                message: "网络下载型 ManagedBinary 安装尚未实现（当前仅支持 bundled_dir）"
                    .to_string(),
            });
        };

        let source_dir =
            resolve_bundled_dir(bundled_relative).ok_or_else(|| RuntimeError::InstallFailed {
                message: format!(
                    "未找到随发布捆绑的 worker 目录（{bundled_relative}）。\
                     开发环境请先运行 `cargo xtask funasr-worker` 构建 worker。"
                ),
            })?;

        // 2. 校验随发布 manifest 的全部文件 hash
        let source = tokio::task::block_in_place(|| verify_bundled_source(&source_dir))?;
        if let Some(s) = sink {
            s.on_log(
                "info",
                &format!(
                    "捆绑 worker hash 校验通过（{} 个文件，来源 {}）",
                    source.files.len(),
                    source_dir.display()
                ),
            );
        }

        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            return Err(RuntimeError::OperationCancelled {
                message: "ManagedBinary 安装在复制前被取消".to_string(),
            });
        }

        // 3. 复制到 staging（exe + manifest.json；hash 已校验）
        if let Some(s) = sink {
            s.on_stage("installing");
        }
        let staging_owned = staging_dir.to_path_buf();
        let files_for_aggregate = source.files.clone();
        let source_owned = source;
        tokio::task::spawn_blocking(move || -> Result<(), RuntimeError> {
            for (name, _) in &source_owned.files {
                std::fs::copy(source_owned.dir.join(name), staging_owned.join(name))?;
            }
            std::fs::copy(
                source_owned.dir.join("manifest.json"),
                staging_owned.join("manifest.json"),
            )?;
            Ok(())
        })
        .await
        .map_err(|e| RuntimeError::InstallFailed {
            message: format!("复制 worker 文件失败: {e}"),
        })??;

        // 4. artifact identity：聚合 hash（排序后的 name:sha 行再 sha256）
        let mut lines: Vec<String> = files_for_aggregate
            .iter()
            .map(|(n, s)| format!("{n}:{s}"))
            .collect();
        lines.sort();
        let aggregate = {
            let mut h = Sha256::new();
            h.update(lines.join("\n").as_bytes());
            h.finalize()
        };
        let aggregate_hex: String = aggregate.iter().map(|b| format!("{b:02x}")).collect();

        if let Some(s) = sink {
            s.on_stage("staged");
            s.on_log(
                "info",
                &format!("worker 已复制到 staging（artifact hash {aggregate_hex:.16}…）"),
            );
        }

        Ok(PrepareResult {
            artifact: runtime::ArtifactIdentity {
                runtime_kind: runtime::RuntimePlan::ManagedBinary,
                artifact_id: binary_plan.archive_artifact_id.clone(),
                sha256: aggregate_hex,
            },
        })
    }

    async fn self_test(
        &self,
        deployment_dir: &Path,
        plan: &InstallPlan,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::SelfTestFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        if binary_plan.self_test_command.is_empty() {
            return Ok(());
        }

        if let Some(s) = sink {
            s.on_stage("self_test");
            s.on_log("info", "执行 worker self-test（--blink-selftest）...");
        }

        // self_test_command = [exe, "--blink-selftest", ...]
        let exe_rel = &binary_plan.self_test_command[0];
        let exe = deployment_dir.join(exe_rel);
        if !exe.is_file() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!("self-test 可执行文件缺失: {}", exe.display()),
            });
        }

        let mut cmd = crate::infra::platform::no_window_tokio(tokio::process::Command::new(&exe));
        cmd.args(&binary_plan.self_test_command[1..])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        // Windows 上进程树回收保障
        let mut child = cmd.spawn().map_err(|e| RuntimeError::SelfTestFailed {
            message: format!("启动 self-test 失败: {e}"),
        })?;
        let pid = child.id().unwrap_or(0);
        #[cfg(windows)]
        let job_handle = crate::infra::platform::process::assign_job_object(pid).ok();

        // wait_with_output 消耗 child——超时/取消分支需要独立 wait 句柄，
        // 因此用 `child.wait()` + 手动取管道的形态：先取 stdout/stderr 再等。
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        let collect = async {
            use tokio::io::AsyncReadExt;
            let mut stdout_buf = Vec::new();
            let mut stderr_buf = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let _ = p.read_to_end(&mut stdout_buf).await;
            }
            if let Some(p) = stderr_pipe.as_mut() {
                let _ = p.read_to_end(&mut stderr_buf).await;
            }
            (stdout_buf, stderr_buf)
        };
        tokio::pin!(collect);

        let output = tokio::select! {
            res = child.wait() => res,
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(RuntimeError::SelfTestFailed {
                    message: "worker self-test 超时（30s）".to_string(),
                });
            }
            _ = async {
                match cancel_token {
                    Some(ct) => ct.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                return Err(RuntimeError::OperationCancelled {
                    message: "worker self-test 被取消".to_string(),
                });
            }
        }?;
        let (stdout_buf, stderr_buf) = collect.await;

        #[cfg(windows)]
        drop(job_handle);

        let output = output;
        if !output.success() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "worker self-test 退出码 {:?}: {}",
                    output.code(),
                    String::from_utf8_lossy(&stderr_buf)
                ),
            });
        }

        // 解析 selftest JSON：{"type":"selftest","worker":..,"protocol_version":1}
        let stdout = String::from_utf8_lossy(&stdout_buf);
        let v: serde_json::Value =
            serde_json::from_str(stdout.trim()).map_err(|e| RuntimeError::SelfTestFailed {
                message: format!("self-test 输出解析失败（{e}）: {stdout}"),
            })?;
        if v.get("type").and_then(|t| t.as_str()) != Some("selftest")
            || v.get("protocol_version").and_then(|p| p.as_u64()) != Some(1)
        {
            return Err(RuntimeError::SelfTestFailed {
                message: format!("self-test 协议不匹配: {stdout}"),
            });
        }

        if let Some(s) = sink {
            s.on_log("info", "worker self-test 通过（protocol_version=1）");
        }
        Ok(())
    }

    fn build_manifest_extension(
        &self,
        deployment_dir: &Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::ManifestSerializeFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        // 从部署目录的 manifest.json 读文件 hash（构建 files 清单）
        let mut files = Vec::new();
        let manifest_path = deployment_dir.join("manifest.json");
        let text = std::fs::read_to_string(&manifest_path).map_err(|e| {
            RuntimeError::ManifestSerializeFailed {
                message: format!("读取部署 manifest.json 失败: {e}"),
            }
        })?;
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
            && let Some(map) = v.get("files").and_then(|f| f.as_object())
        {
            for (name, meta) in map {
                if let Some(sha) = meta.get("sha256").and_then(|s| s.as_str()) {
                    let size = std::fs::metadata(deployment_dir.join(name))
                        .map(|m| m.len())
                        .unwrap_or(0);
                    files.push(runtime::FileEntry {
                        path: name.clone(),
                        sha256: sha.to_string(),
                        size,
                        is_dll: false,
                    });
                }
            }
        }

        Ok(ManifestExtension::ManagedBinary(
            runtime::BinaryManifestExt {
                archive_artifact_id: binary_plan.archive_artifact_id.clone(),
                archive_sha256: binary_plan.archive_sha256.clone(),
                executable: binary_plan.executable.clone(),
                files,
                stdlib_artifact: binary_plan.stdlib_artifact.clone(),
                required_cpu_features: binary_plan.required_cpu_features.clone(),
                required_drivers: binary_plan.required_drivers.clone(),
                self_test_passed: true,
            },
        ))
    }
}

// ── CPU feature 检测（Windows）─────────────────────────────────────────────

/// 检查 CPU 是否支持 AVX2。
#[cfg(target_arch = "x86_64")]
fn check_avx2() -> bool {
    is_x86_feature_detected!("avx2")
}

/// 检查 CPU 是否支持 AVX。
#[cfg(target_arch = "x86_64")]
fn check_avx() -> bool {
    is_x86_feature_detected!("avx")
}

/// 非 x86_64 架构的占位实现。
#[cfg(not(target_arch = "x86_64"))]
fn check_avx2() -> bool {
    false
}

#[cfg(not(target_arch = "x86_64"))]
fn check_avx() -> bool {
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn managed_binary_provider_kind() {
        let provider = ManagedBinaryProvider::new();
        assert_eq!(provider.kind(), runtime::RuntimePlan::ManagedBinary);
    }

    #[test]
    fn managed_binary_always_compatible() {
        let provider = ManagedBinaryProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::Always)
                .unwrap()
        );
    }

    #[test]
    fn managed_binary_cpu_only_rejects_gpu() {
        let provider = ManagedBinaryProvider::cpu_only();
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
    fn managed_binary_requires_cpu_feature_sse2() {
        let provider = ManagedBinaryProvider::new();
        // SSE2 在 x64 上总是支持
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::RequiresCpuFeature {
                    feature: "sse2".to_string()
                })
                .unwrap()
        );
    }
}
