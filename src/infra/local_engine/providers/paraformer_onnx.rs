//! ParaformerOnline ONNX Provider（0.22.9，Handoff 04）。
//!
//! 实现 `RuntimeProvider` trait 的完整安装事务：
//! - ORT DLL：从 `stt_asset_lock` 读取锁定信息，下载 → hash 校验 → staging
//! - 模型文件：encoder / decoder / CMVN / tokenizer 各自下载 + hash + size 校验
//! - 联合 self-test：通过 `blink.exe paraformer-selftest` 隔离验证进程执行
//!   真实 ORT Session 加载 + 最小推理 + 二进制协议 v2 帧编解码验证
//!
//! ## 设计铁则
//!
//! - **不注册为用户可见模型**：provider 只负责环境准备和 self-test，
//!   不将 ParaformerOnline 注册到用户模型目录。
//! - **隔离 self-test**：self-test 在 `blink.exe paraformer-selftest` 子进程中
//!   执行，禁止 Blink 主进程从 staging 加载 ORT DLL。
//! - **placeholder hash 拒绝**：asset lock 中的 placeholder hash 不可用于
//!   真实部署，`prepare_environment` 在开始前检查并拒绝。
//! - **仅 CPU DLL**：只下载和加载 CPU-only ORT DLL。
//! - **与 OCR provider 独立**：STT 和 OCR 可使用不同 ORT 版本，
//!   各自锁定，互不干扰。
//! - **复用 download_and_verify**：与 OCR provider 共享下载+校验逻辑。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    CompatibilityCheck, InstallPlan, InstallSink, ManifestExtension, OnnxInstallPlan,
    PrepareResult, ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;
use crate::infra::local_engine::stt_asset_lock;

/// ParaformerOnline ONNX Provider。
///
/// 负责 ORT DLL + encoder/decoder/CMVN/tokenizer 的下载、hash 校验和
/// 隔离 self-test 调度。不启动常驻 worker——worker 的生命周期由
/// `ManagedProcess` + `StreamWorkerClient` 在引擎 start 阶段接管。
pub struct ParaformerOnnxProvider {
    /// 是否允许 GPU backend（当前只支持 CPU，此字段为 false）。
    #[allow(dead_code)]
    allow_gpu: bool,
}

impl ParaformerOnnxProvider {
    /// 创建 ParaformerOnnxProvider（CPU-only）。
    pub fn new() -> Self {
        Self { allow_gpu: false }
    }

    /// 创建只允许 CPU 的 ParaformerOnnxProvider（测试用）。
    #[allow(dead_code)]
    pub fn cpu_only() -> Self {
        Self { allow_gpu: false }
    }
}

impl Default for ParaformerOnnxProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 隔离验证进程超时（秒）。
/// self-test 必须在此时间内完成 DLL 加载 + Session 创建 + 最小推理 + 协议验证。
const VALIDATOR_TIMEOUT_SECS: u64 = 120;

/// 定位 blink.exe 隔离验证进程。
fn validator_exe_path() -> Result<PathBuf, RuntimeError> {
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(Path::new("."));
        let candidate = dir.join("blink.exe");
        if candidate.exists() {
            return Ok(candidate);
        }
    }
    Err(RuntimeError::SelfTestFailed {
        message: "无法定位 blink.exe 隔离验证进程".to_string(),
    })
}

/// 下载文件并校验 SHA-256。
///
/// 与 `providers::onnx::download_and_verify` 共享相同逻辑，但独立定义
/// 以避免跨模块私有函数依赖。
async fn download_and_verify(
    url: &str,
    expected_sha256: Option<&str>,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<u64, RuntimeError> {
    use tokio::io::AsyncWriteExt;

    tracing::info!(url = url, dest = %dest.display(), "开始下载");

    if let Some(s) = sink {
        s.on_log("info", &format!("正在下载: {url}"));
    }

    if let Some(ct) = cancel_token
        && ct.is_cancelled()
    {
        return Err(RuntimeError::OperationCancelled {
            message: "下载开始前被取消".to_string(),
        });
    }

    // User-Agent 必须显式标识——ModelScope 等源的 LFS 重定向会拒绝
    // 空默认 UA（HTTP 403）；产品 UA 已实测可过（Handoff 08 E2E）。
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .user_agent("Blink/0.22 (stt-asset-download)")
        .build()
        .map_err(|e| RuntimeError::InstallFailed {
            message: format!("HTTP client 构造失败: {e}"),
        })?;

    let response = tokio::select! {
        r = client.get(url).send() => {
            r.map_err(|e| RuntimeError::InstallFailed {
                message: format!("下载失败: {e}"),
            })?
        }
        _ = async {
            if let Some(ct) = cancel_token {
                ct.cancelled().await;
            }
        } => {
            return Err(RuntimeError::OperationCancelled {
                message: format!("下载被取消: {url}"),
            });
        }
    };

    if !response.status().is_success() {
        return Err(RuntimeError::InstallFailed {
            message: format!("下载失败: HTTP {}", response.status()),
        });
    }

    let tmp_name = format!(
        ".tmp_download_{}",
        dest.file_name().and_then(|n| n.to_str()).unwrap_or("file")
    );
    let tmp_path = dest.parent().unwrap_or(Path::new(".")).join(&tmp_name);
    if let Some(p) = tmp_path.parent() {
        std::fs::create_dir_all(p)?;
    }

    let mut file = tokio::fs::File::create(&tmp_path)
        .await
        .map_err(RuntimeError::Io)?;

    let mut hasher = Sha256::new();
    let mut stream = response.bytes_stream();
    use futures::StreamExt;
    let mut total_written: u64 = 0;

    loop {
        tokio::select! {
            chunk = stream.next() => {
                match chunk {
                    Some(Ok(bytes)) => {
                        hasher.update(&bytes);
                        file.write_all(&bytes).await.map_err(RuntimeError::Io)?;
                        total_written += bytes.len() as u64;
                    }
                    Some(Err(e)) => {
                        let _ = tokio::fs::remove_file(&tmp_path).await;
                        return Err(RuntimeError::InstallFailed {
                            message: format!("下载流读取失败: {e}"),
                        });
                    }
                    None => break,
                }
            }
            _ = async {
                if let Some(ct) = cancel_token {
                    ct.cancelled().await;
                }
            } => {
                let _ = tokio::fs::remove_file(&tmp_path).await;
                return Err(RuntimeError::OperationCancelled {
                    message: format!("下载被取消: {url}"),
                });
            }
        }
    }

    file.flush().await.map_err(RuntimeError::Io)?;
    drop(file);

    if let Some(expected) = expected_sha256 {
        let actual_hash = format!("{:x}", hasher.finalize());
        if actual_hash != expected {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(RuntimeError::InstallFailed {
                message: format!("SHA-256 校验失败: expected={expected}, actual={actual_hash}"),
            });
        }
    }

    tokio::fs::rename(&tmp_path, dest)
        .await
        .map_err(RuntimeError::Io)?;

    tracing::info!(
        url = url,
        dest = %dest.display(),
        size = total_written,
        "下载完成，hash 校验通过"
    );

    if let Some(s) = sink {
        s.on_log(
            "info",
            &format!("下载完成: {total_written} bytes, hash 校验通过"),
        );
    }

    Ok(total_written)
}

/// 解压 ORT archive 到 staging 目录。
///
/// 与 OCR provider 的 `extract_ort_archive` 相同逻辑：ORT archive 是 zip
/// 格式，只提取 lib/ 下的 DLL 文件。
fn extract_ort_archive(
    archive_path: &Path,
    staging_dir: &Path,
    sink: Option<&dyn InstallSink>,
) -> Result<Vec<runtime::FileEntry>, RuntimeError> {
    use std::io::Read;

    let file = std::fs::File::open(archive_path)?;
    let mut zip = zip::ZipArchive::new(file).map_err(|e| RuntimeError::InstallFailed {
        message: format!("ZIP 解压失败: {e}"),
    })?;

    let mut entries = Vec::new();

    for i in 0..zip.len() {
        let entry = zip.by_index(i).map_err(|e| RuntimeError::InstallFailed {
            message: format!("ZIP entry 读取失败: {e}"),
        })?;

        let name = entry.name().replace('\\', "/");

        if !name.contains("/lib/") || !name.ends_with(".dll") {
            continue;
        }

        let filename = name.rsplit('/').next().unwrap_or(&name);
        let dest = staging_dir.join(filename);

        let mut entry = entry;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(RuntimeError::Io)?;

        let mut hasher = Sha256::new();
        hasher.update(&buf);
        let sha256 = format!("{:x}", hasher.finalize());
        let size = buf.len() as u64;

        std::fs::write(&dest, &buf)?;

        entries.push(runtime::FileEntry {
            path: filename.to_string(),
            sha256,
            size,
            is_dll: true,
        });

        if let Some(s) = sink {
            s.on_log("info", &format!("提取: {filename} ({size} bytes)"));
        }
    }

    let _ = std::fs::remove_file(archive_path);

    Ok(entries)
}

/// 下载单个 STT 模型文件到 staging 目录。
async fn download_stt_model(
    model: &stt_asset_lock::SttModelLock,
    staging_dir: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<(), RuntimeError> {
    let dest = staging_dir.join(&model.filename);
    download_and_verify(&model.url, Some(&model.sha256), &dest, cancel_token, sink).await?;

    let metadata = std::fs::metadata(&dest)?;
    if metadata.len() != model.size_bytes {
        return Err(RuntimeError::InstallFailed {
            message: format!(
                "模型 {} 大小不匹配: expected={}, actual={}",
                model.filename,
                model.size_bytes,
                metadata.len()
            ),
        });
    }

    Ok(())
}

/// 在隔离验证进程中执行 self-test。
///
/// 启动 `blink.exe paraformer-selftest` 子进程，传入 staging 目录路径。
/// 子进程加载 staging DLL，创建 encoder/decoder Session，执行最小推理，
/// 并验证二进制协议 v2 帧编解码。
fn run_isolated_self_test(
    staging_dir: &Path,
    install_plan: &OnnxInstallPlan,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<(), RuntimeError> {
    let exe = validator_exe_path()?;

    let dll_path = staging_dir.join("onnxruntime.dll");
    let encoder_path = staging_dir.join("encoder.onnx");
    let decoder_path = staging_dir.join("decoder.onnx");
    let cmvn_path = staging_dir.join("am.mvn");
    let tokenizer_path = staging_dir.join("tokenizer.json");

    for (name, path) in [
        ("DLL", &dll_path),
        ("encoder", &encoder_path),
        ("decoder", &decoder_path),
        ("CMVN", &cmvn_path),
        ("tokenizer", &tokenizer_path),
    ] {
        if !path.exists() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!("{name} 文件不存在: {}", path.display()),
            });
        }
    }

    let mut cmd = std::process::Command::new(&exe);
    cmd.arg("paraformer-selftest")
        .arg("--dll")
        .arg(&dll_path)
        .arg("--encoder")
        .arg(&encoder_path)
        .arg("--decoder")
        .arg(&decoder_path)
        .arg("--cmvn")
        .arg(&cmvn_path)
        .arg("--tokenizer")
        .arg(&tokenizer_path)
        .arg("--intra-op")
        .arg(install_plan.intra_op.to_string())
        .arg("--inter-op")
        .arg(install_plan.inter_op.to_string())
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    if let Some(s) = sink {
        s.on_log("info", "启动隔离验证进程...");
    }

    tracing::info!(
        exe = %exe.display(),
        staging = %staging_dir.display(),
        "启动 ParaformerOnline 隔离验证进程"
    );

    let mut child = cmd.spawn().map_err(|e| RuntimeError::SelfTestFailed {
        message: format!("启动隔离验证进程失败: {e}"),
    })?;

    let timeout = std::time::Duration::from_secs(VALIDATOR_TIMEOUT_SECS);
    let start = std::time::Instant::now();

    loop {
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            let _ = child.kill();
            return Err(RuntimeError::OperationCancelled {
                message: "隔离验证进程被取消".to_string(),
            });
        }

        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    if let Some(s) = sink {
                        s.on_log("info", "隔离验证进程通过");
                    }
                    tracing::info!("ParaformerOnline 隔离验证进程通过");
                    return Ok(());
                } else {
                    let stderr = child
                        .stderr
                        .take()
                        .map(|mut s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            s.read_to_string(&mut buf).ok();
                            buf
                        })
                        .unwrap_or_default();

                    let stdout = child
                        .stdout
                        .take()
                        .map(|mut s| {
                            use std::io::Read;
                            let mut buf = String::new();
                            s.read_to_string(&mut buf).ok();
                            buf
                        })
                        .unwrap_or_default();

                    return Err(RuntimeError::SelfTestFailed {
                        message: format!(
                            "隔离验证进程退出码非零: {status}\nstdout: {stdout}\nstderr: {stderr}"
                        ),
                    });
                }
            }
            Ok(None) => {
                if start.elapsed() > timeout {
                    let _ = child.kill();
                    return Err(RuntimeError::SelfTestFailed {
                        message: format!("隔离验证进程超时（{VALIDATOR_TIMEOUT_SECS}s）"),
                    });
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => {
                let _ = child.kill();
                return Err(RuntimeError::SelfTestFailed {
                    message: format!("等待隔离验证进程失败: {e}"),
                });
            }
        }
    }
}

#[async_trait::async_trait]
impl RuntimeProvider for ParaformerOnnxProvider {
    fn kind(&self) -> runtime::RuntimePlan {
        runtime::RuntimePlan::OnnxRuntime
    }

    fn check_compatibility(
        &self,
        compatibility: &CompatibilityCheck,
    ) -> Result<bool, RuntimeError> {
        match compatibility {
            CompatibilityCheck::Always => Ok(true),
            _ => Ok(false),
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
        let onnx_plan = match plan {
            InstallPlan::OnnxRuntime(p) => p,
            _ => {
                return Err(RuntimeError::InstallFailed {
                    message: "ParaformerOnnxProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        std::fs::create_dir_all(staging_dir)?;

        // ── 0. 检查 placeholder hash ──────────────────────────────────
        if stt_asset_lock::has_placeholder_hashes()? {
            return Err(RuntimeError::InstallFailed {
                message: "STT asset-lock.json 包含 placeholder hash，不可用于真实部署。\
                          请先通过供应链流水线填入实际 SHA-256 和 size_bytes。"
                    .to_string(),
            });
        }

        // ── 1. 下载 ORT DLL archive ────────────────────────────────────
        let lock = stt_asset_lock::parse_asset_lock()?;

        if let Some(s) = sink {
            s.on_stage("downloading");
            s.on_log("info", &format!("正在下载 ORT v{}...", lock.ort.version));
        }

        let archive_path = staging_dir.join("ort-archive.zip");
        download_and_verify(
            &lock.ort.url,
            None, // zip-level hash 跳过；解压后逐文件校验
            &archive_path,
            cancel_token,
            sink,
        )
        .await?;

        // ── 2. 解压 ORT archive ────────────────────────────────────────
        if let Some(s) = sink {
            s.on_stage("verifying");
            s.on_log("info", "正在解压 ORT archive...");
        }

        let dll_entries = extract_ort_archive(&archive_path, staging_dir, sink)?;

        // 校验解压出的 DLL hash
        for entry in &dll_entries {
            let expected = lock
                .ort
                .files
                .iter()
                .find(|f| f.path.ends_with(&entry.path))
                .ok_or_else(|| RuntimeError::InstallFailed {
                    message: format!("DLL {} 不在 STT asset lock 中", entry.path),
                })?;

            if entry.sha256 != expected.sha256 {
                return Err(RuntimeError::InstallFailed {
                    message: format!(
                        "DLL {} SHA-256 不匹配: expected={}, actual={}",
                        entry.path, expected.sha256, entry.sha256
                    ),
                });
            }

            if entry.size != expected.size_bytes {
                return Err(RuntimeError::InstallFailed {
                    message: format!(
                        "DLL {} 大小不匹配: expected={}, actual={}",
                        entry.path, expected.size_bytes, entry.size
                    ),
                });
            }
        }

        // ── 3. 下载模型文件（encoder / decoder / CMVN / tokenizer）────
        if let Some(s) = sink {
            s.on_stage("downloading");
            s.on_log("info", "正在下载 ParaformerOnline 模型...");
        }

        for model in &lock.models {
            download_stt_model(model, staging_dir, cancel_token, sink).await?;
        }

        // ── 4. 构建 artifact identity ──────────────────────────────────
        let dll_artifact_id = stt_asset_lock::ort_dll_artifact_id()?;
        let dll_sha256 = dll_entries
            .iter()
            .find(|e| e.path == "onnxruntime.dll")
            .map(|e| e.sha256.clone())
            .ok_or_else(|| RuntimeError::InstallFailed {
                message: "onnxruntime.dll 不在解压结果中".to_string(),
            })?;

        let artifact = runtime::ArtifactIdentity {
            runtime_kind: runtime::RuntimePlan::OnnxRuntime,
            artifact_id: dll_artifact_id,
            sha256: dll_sha256,
        };

        // 存储 dll_entries 供 build_manifest_extension 使用
        let entries_path = staging_dir.join(".stt_ort_dll_entries.json");
        runtime::atomic_write_json(&entries_path, &dll_entries)?;

        let _ = onnx_plan; // install plan 已消费

        Ok(PrepareResult { artifact })
    }

    async fn self_test(
        &self,
        staging_dir: &Path,
        plan: &InstallPlan,
        cancel_token: Option<&tokio_util::sync::CancellationToken>,
        sink: Option<&dyn InstallSink>,
    ) -> Result<(), RuntimeError> {
        let onnx_plan = match plan {
            InstallPlan::OnnxRuntime(p) => p,
            _ => {
                return Err(RuntimeError::SelfTestFailed {
                    message: "ParaformerOnnxProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        run_isolated_self_test(staging_dir, onnx_plan, cancel_token, sink)
    }

    fn build_manifest_extension(
        &self,
        staging_dir: &Path,
        plan: &InstallPlan,
    ) -> Result<ManifestExtension, RuntimeError> {
        let onnx_plan = match plan {
            InstallPlan::OnnxRuntime(p) => p,
            _ => {
                return Err(RuntimeError::ManifestSerializeFailed {
                    message: "ParaformerOnnxProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        // 读取 prepare_environment 存储的 DLL entries
        let entries_path = staging_dir.join(".stt_ort_dll_entries.json");
        let dll_entries: Vec<runtime::FileEntry> = if entries_path.exists() {
            let content = std::fs::read_to_string(&entries_path)?;
            serde_json::from_str(&content).map_err(|e| RuntimeError::ManifestParseFailed {
                message: format!("DLL entries 解析失败: {e}"),
            })?
        } else {
            scan_dll_files(staging_dir)?
        };

        let dll_sha256 = dll_entries
            .iter()
            .find(|e| e.path == "onnxruntime.dll")
            .map(|e| e.sha256.clone())
            .ok_or_else(|| RuntimeError::ManifestSerializeFailed {
                message: "onnxruntime.dll 不在 staging 中".to_string(),
            })?;

        let dll_artifact_id = stt_asset_lock::ort_dll_artifact_id()?;

        // 模型 generation id（使用 asset lock 版本确定性生成）
        let lock = stt_asset_lock::parse_asset_lock()?;
        let model_generation_id = format!(
            "paraformer-online-{}",
            lock.models
                .iter()
                .map(|m| m.sha256[..12].to_string())
                .collect::<Vec<_>>()
                .join("-")
        );

        Ok(ManifestExtension::OnnxRuntime(
            runtime::OnnxRuntimeManifestExt {
                dll_artifact_id,
                dll_sha256,
                ort_version: lock.ort.version,
                dll_files: dll_entries,
                model_generation_id,
                execution_provider: onnx_plan.execution_provider.clone(),
                inter_op: onnx_plan.inter_op,
                intra_op: onnx_plan.intra_op,
                self_test_passed: true,
            },
        ))
    }
}

/// 从 staging 目录扫描 DLL 文件（fallback 路径）。
fn scan_dll_files(dir: &Path) -> Result<Vec<runtime::FileEntry>, RuntimeError> {
    let mut entries = Vec::new();

    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if let Some(ext) = path.extension().and_then(|e| e.to_str())
            && ext.eq_ignore_ascii_case("dll")
        {
            let filename = entry.file_name().to_string_lossy().to_string();
            let content = std::fs::read(&path)?;
            let mut hasher = Sha256::new();
            hasher.update(&content);
            let sha256 = format!("{:x}", hasher.finalize());
            let size = content.len() as u64;
            entries.push(runtime::FileEntry {
                path: filename,
                sha256,
                size,
                is_dll: true,
            });
        }
    }

    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::local_engine::providers::PythonInstallPlan;

    #[test]
    fn paraformer_onnx_provider_kind() {
        let provider = ParaformerOnnxProvider::new();
        assert_eq!(provider.kind(), runtime::RuntimePlan::OnnxRuntime);
    }

    #[test]
    fn paraformer_onnx_always_compatible() {
        let provider = ParaformerOnnxProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::Always)
                .unwrap()
        );
    }

    #[test]
    fn paraformer_onnx_rejects_non_onnx_plan() {
        let provider = ParaformerOnnxProvider::new();
        let python_plan = InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
            packages: Vec::new(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            extra_pip_args: Vec::new(),
            self_test_script: "pass".to_string(),
        });
        assert!(
            provider
                .build_manifest_extension(std::path::Path::new("."), &python_plan)
                .is_err()
        );
    }

    #[test]
    fn prepare_environment_rejects_non_onnx_plan() {
        let provider = ParaformerOnnxProvider::new();
        let python_plan = InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
            packages: Vec::new(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            extra_pip_args: Vec::new(),
            self_test_script: "pass".to_string(),
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(provider.prepare_environment(
            std::path::Path::new("/tmp/test-staging"),
            &python_plan,
            &ResolvedProfile {
                profile_id: "test".to_string(),
                backend: runtime::ComputeBackend::Cpu,
                artifact_id: runtime::ArtifactId::new("test").unwrap(),
                priority: 0,
            },
            None,
            None,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn self_test_rejects_non_onnx_plan() {
        let provider = ParaformerOnnxProvider::new();
        let python_plan = InstallPlan::PythonVenv(PythonInstallPlan {
            python_version: "3.12.8".to_string(),
            python_artifact_id: runtime::ArtifactId::new("python-3.12.8").unwrap(),
            packages: Vec::new(),
            uv_version: "0.6.10".to_string(),
            index_url: None,
            extra_pip_args: Vec::new(),
            self_test_script: "pass".to_string(),
        });

        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(provider.self_test(
            std::path::Path::new("/tmp/test-staging"),
            &python_plan,
            None,
            None,
        ));
        assert!(result.is_err());
    }

    #[test]
    fn stt_asset_lock_embedded_and_parseable() {
        let lock = stt_asset_lock::parse_asset_lock().expect("STT asset lock 可解析");
        assert_eq!(lock.ort.version, "1.19.2");
        assert_eq!(lock.models.len(), 4);
    }

    #[test]
    fn stt_asset_lock_detects_placeholder() {
        // asset-lock.json 已填入真实 SHA-256 和 size_bytes
        assert!(!stt_asset_lock::has_placeholder_hashes().unwrap());
    }
}
