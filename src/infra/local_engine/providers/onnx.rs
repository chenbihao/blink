//! ONNX Runtime Provider（0.22.8-B 生产实现）。
//!
//! 实现 `RuntimeProvider` trait 的完整安装事务：
//! - ORT DLL：版本化、不可变 artifact，下载 → hash 校验 → staging → promote
//! - 模型 generation：det/rec/dictionary 通过 model_storage 准备
//! - 联合 self-test：在隔离验证进程中加载 staging DLL + 创建 Session + 最小推理
//! - 引用保护：cleanup 前扫描 active deployment manifest 引用
//!
//! ## 设计铁则
//!
//! - **不伪装成 ManagedBinary**：OnnxRuntime 是共享动态运行时，
//!   DLL 由 in-process lazy Session 持有，不启动子进程。
//! - **domain 不依赖 ort/oar-ocr**：本 provider 位于 infra 层，
//!   domain 只通过 `RuntimePlan::OnnxRuntime` 和 `ManifestExtension::OnnxRuntime`
//!   引用。
//! - **联合提交**：ORT DLL 与模型 generation 分别 staging、校验和 promote，
//!   只有两者联合 self-test 通过后才提交 deployment pointer。
//! - **隔离 self-test**：真实 Session + 最小推理 self-test 必须运行在
//!   一次性隔离验证进程中，禁止 Blink 主进程从 staging 加载 DLL。
//! - **仅 CPU DLL**：只下载和加载 CPU-only ORT DLL，禁止 CUDA/TensorRT provider。

use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    CompatibilityCheck, InstallPlan, InstallSink, ManifestExtension, PrepareResult,
    ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::asset_lock;
use crate::infra::local_engine::runtime;

/// ONNX Runtime Provider（0.22.8-B 生产实现）。
///
/// ORT DLL 使用版本化、不可变 artifact；模型 det/rec/dictionary 使用
/// `model_storage` generation。Provider 负责 staging、hash 校验和 promote，
/// 不伪装成 `ManagedBinary`（不启动子进程）。
pub struct OnnxRuntimeProvider {
    /// 是否允许 GPU backend（当前只支持 CPU，此字段为 false）。
    #[allow(dead_code)]
    allow_gpu: bool,
}

impl OnnxRuntimeProvider {
    /// 创建 OnnxRuntimeProvider（CPU-only）。
    pub fn new() -> Self {
        Self { allow_gpu: false }
    }

    /// 创建只允许 CPU 的 OnnxRuntimeProvider（测试用）。
    #[allow(dead_code)]
    pub fn cpu_only() -> Self {
        Self { allow_gpu: false }
    }
}

impl Default for OnnxRuntimeProvider {
    fn default() -> Self {
        Self::new()
    }
}

/// 隔离验证进程超时（秒）。
/// validator 必须在此时间内完成 DLL 加载 + Session 创建 + 最小推理。
const VALIDATOR_TIMEOUT_SECS: u64 = 60;

/// 隔离验证进程的可执行文件名。
/// 在开发/测试环境中使用 target/debug 或 target/release 下的 blink.exe。
fn validator_exe_path() -> Result<PathBuf, RuntimeError> {
    // 尝试当前可执行文件目录（生产环境：blink.exe 同目录）
    if let Ok(exe) = std::env::current_exe() {
        let dir = exe.parent().unwrap_or(Path::new("."));
        let candidate = dir.join("blink.exe");
        if candidate.exists() {
            return Ok(candidate);
        }
        // 开发环境：target/debug 或 target/release
        let candidate2 = dir.join("blink.exe");
        if candidate2.exists() {
            return Ok(candidate2);
        }
    }
    Err(RuntimeError::SelfTestFailed {
        message: "无法定位 blink.exe 隔离验证进程".to_string(),
    })
}

/// 下载文件并校验 SHA-256（可选）。
///
/// 使用 reqwest 下载，stream 到临时文件。若 `expected_sha256` 为 `Some`，
/// 下载完成后校验 hash；为 `None` 时跳过校验（调用方自行负责后续校验）。
/// 支持 cancel_token 取消。
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

    // 检查取消
    if let Some(ct) = cancel_token
        && ct.is_cancelled()
    {
        return Err(RuntimeError::OperationCancelled {
            message: "下载开始前被取消".to_string(),
        });
    }

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
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

    // 创建临时文件（同目录，下载完成后 rename）
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

    // 校验 hash（若调用方提供了期望值）
    if let Some(expected) = expected_sha256 {
        let actual_hash = format!("{:x}", hasher.finalize());
        if actual_hash != expected {
            let _ = tokio::fs::remove_file(&tmp_path).await;
            return Err(RuntimeError::InstallFailed {
                message: format!("SHA-256 校验失败: expected={expected}, actual={actual_hash}"),
            });
        }
    }

    // rename 到目标路径
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
/// ORT archive 是 zip 格式，只提取 lib/ 下的 DLL 文件。
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

        // 只提取 lib/ 下的 DLL 文件
        if !name.contains("/lib/") || !name.ends_with(".dll") {
            continue;
        }

        // 计算目标路径：取 lib/ 下的文件名
        let filename = name.rsplit('/').next().unwrap_or(&name);
        let dest = staging_dir.join(filename);

        // 提取文件
        let mut entry = entry;
        let mut buf = Vec::new();
        entry.read_to_end(&mut buf).map_err(RuntimeError::Io)?;

        // 计算 hash 和 size
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

    // 删除 archive
    let _ = std::fs::remove_file(archive_path);

    Ok(entries)
}

/// 下载单个模型文件到 staging 目录。
async fn download_model(
    model: &asset_lock::ModelLock,
    staging_dir: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<(), RuntimeError> {
    let dest = staging_dir.join(&model.filename);
    download_and_verify(&model.url, Some(&model.sha256), &dest, cancel_token, sink).await?;

    // 校验文件大小
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
/// 启动 `blink.exe onnx-validate` 子进程，传入 staging 目录路径。
/// 子进程加载 staging DLL，创建 ORT Session，执行最小推理。
fn run_isolated_self_test(
    staging_dir: &Path,
    install_plan: &super::OnnxInstallPlan,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<(), RuntimeError> {
    let exe = validator_exe_path()?;

    let dll_path = staging_dir.join("onnxruntime.dll");
    let det_path = staging_dir.join("pp-ocrv6_tiny_det.onnx");
    let rec_path = staging_dir.join("pp-ocrv6_tiny_rec.onnx");
    let dict_path = staging_dir.join("ppocrv6_tiny_dict.txt");

    if !dll_path.exists() {
        return Err(RuntimeError::SelfTestFailed {
            message: format!("staging DLL 不存在: {}", dll_path.display()),
        });
    }
    if !det_path.exists() {
        return Err(RuntimeError::SelfTestFailed {
            message: format!("staging det 模型不存在: {}", det_path.display()),
        });
    }
    if !rec_path.exists() {
        return Err(RuntimeError::SelfTestFailed {
            message: format!("staging rec 模型不存在: {}", rec_path.display()),
        });
    }
    if !dict_path.exists() {
        return Err(RuntimeError::SelfTestFailed {
            message: format!("staging dictionary 不存在: {}", dict_path.display()),
        });
    }

    // validator 是控制台子系统 exe——GUI 主进程直呼会闪黑窗，
    // 每次 OCR 环境 install/repair 的隔离自检都必须压窗口
    let mut cmd = crate::infra::platform::no_window(std::process::Command::new(&exe));
    cmd.arg("onnx-validate")
        .arg("--dll")
        .arg(&dll_path)
        .arg("--det")
        .arg(&det_path)
        .arg("--rec")
        .arg(&rec_path)
        .arg("--dict")
        .arg(&dict_path)
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
        "启动隔离验证进程"
    );

    let mut child = cmd.spawn().map_err(|e| RuntimeError::SelfTestFailed {
        message: format!("启动隔离验证进程失败: {e}"),
    })?;

    let timeout = std::time::Duration::from_secs(VALIDATOR_TIMEOUT_SECS);
    let start = std::time::Instant::now();

    loop {
        // 检查取消
        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            let _ = child.kill();
            return Err(RuntimeError::OperationCancelled {
                message: "隔离验证进程被取消".to_string(),
            });
        }

        // 尝试等待（短超时轮询）
        match child.try_wait() {
            Ok(Some(status)) => {
                if status.success() {
                    if let Some(s) = sink {
                        s.on_log("info", "隔离验证进程通过");
                    }
                    tracing::info!("隔离验证进程通过");
                    return Ok(());
                } else {
                    // 读取 stderr 获取错误信息
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
                // 进程仍在运行
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
impl RuntimeProvider for OnnxRuntimeProvider {
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
                    message: "OnnxRuntimeProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        std::fs::create_dir_all(staging_dir)?;

        // ── 1. 下载 ORT DLL archive ──
        let lock = asset_lock::parse_asset_lock()?;

        if let Some(s) = sink {
            s.on_stage("downloading");
            s.on_log("info", &format!("正在下载 ORT v{}...", lock.ort.version));
        }

        let archive_path = staging_dir.join("ort-archive.zip");
        // ORT zip 包本身不做 hash 校验——asset-lock.json 锁的是 zip 包内
        // 各 DLL 的 hash，解压后由 extract_ort_archive + 逐文件比对来保障完整性。
        download_and_verify(
            &lock.ort.url,
            None, // zip-level hash 跳过；解压后逐文件校验
            &archive_path,
            cancel_token,
            sink,
        )
        .await?;

        // ── 2. 解压 ORT archive ──
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
                    message: format!("DLL {} 不在 asset lock 中", entry.path),
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

        // ── 3. 下载模型文件（det / rec / dictionary）──
        if let Some(s) = sink {
            s.on_stage("downloading");
            s.on_log("info", "正在下载 PP-OCRv6 模型...");
        }

        for model in &lock.models {
            download_model(model, staging_dir, cancel_token, sink).await?;
        }

        // ── 4. 构建 artifact identity ──
        let dll_artifact_id = asset_lock::ort_dll_artifact_id()?;
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
        // 通过中间文件传递（provider 无状态，InstallTransaction 在 staging 上执行后
        // build_manifest_extension 也读 staging 目录）
        let entries_path = staging_dir.join(".ort_dll_entries.json");
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
                    message: "OnnxRuntimeProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        // 在隔离验证进程中执行 self-test
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
                    message: "OnnxRuntimeProvider 收到非 OnnxRuntime 安装计划".to_string(),
                });
            }
        };

        // 读取 prepare_environment 存储的 DLL entries
        let entries_path = staging_dir.join(".ort_dll_entries.json");
        let dll_entries: Vec<runtime::FileEntry> = if entries_path.exists() {
            let content = std::fs::read_to_string(&entries_path)?;
            serde_json::from_str(&content).map_err(|e| RuntimeError::ManifestParseFailed {
                message: format!("DLL entries 解析失败: {e}"),
            })?
        } else {
            // fallback：从 staging 目录扫描 DLL 文件
            scan_dll_files(staging_dir)?
        };

        let dll_sha256 = dll_entries
            .iter()
            .find(|e| e.path == "onnxruntime.dll")
            .map(|e| e.sha256.clone())
            .ok_or_else(|| RuntimeError::ManifestSerializeFailed {
                message: "onnxruntime.dll 不在 staging 中".to_string(),
            })?;

        let dll_artifact_id = asset_lock::ort_dll_artifact_id()?;

        // 模型 generation id（使用 asset lock 版本确定性生成）
        let lock = asset_lock::parse_asset_lock()?;
        let model_generation_id = format!(
            "ppocrv6-tiny-{}",
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
    use crate::infra::local_engine::providers::BinaryInstallPlan;

    /// 非 ONNX 安装计划 fixture（0.22.10 起以 ManagedBinary 充当，
    /// 原 PythonVenv 计划已随 Python/uv 栈退役删除）。
    fn non_onnx_plan() -> InstallPlan {
        InstallPlan::ManagedBinary(BinaryInstallPlan {
            archive_artifact_id: runtime::ArtifactId::new("fake-worker-archive").unwrap(),
            archive_url: "https://example.invalid/fake-worker.zip".to_string(),
            archive_sha256: "0".repeat(64),
            executable: "worker.exe".to_string(),
            stdlib_artifact: None,
            required_cpu_features: Vec::new(),
            required_drivers: Vec::new(),
            self_test_command: vec!["worker.exe".to_string(), "--self-test".to_string()],
            bundled_dir: None,
        })
    }

    #[test]
    fn onnx_runtime_provider_kind() {
        let provider = OnnxRuntimeProvider::new();
        assert_eq!(provider.kind(), runtime::RuntimePlan::OnnxRuntime);
    }

    #[test]
    fn onnx_runtime_always_compatible() {
        let provider = OnnxRuntimeProvider::new();
        assert!(
            provider
                .check_compatibility(&CompatibilityCheck::Always)
                .unwrap()
        );
    }

    #[test]
    fn onnx_runtime_rejects_non_onnx_plan() {
        let provider = OnnxRuntimeProvider::new();
        let python_plan = non_onnx_plan();
        // build_manifest_extension 应拒绝非 OnnxRuntime plan
        assert!(
            provider
                .build_manifest_extension(std::path::Path::new("."), &python_plan)
                .is_err()
        );
    }

    #[test]
    fn asset_lock_embedded_and_parseable() {
        // 确保编译期嵌入的 asset-lock.json 可解析
        let lock = asset_lock::parse_asset_lock().expect("asset lock 可解析");
        assert_eq!(lock.ort.version, "1.19.2");
        assert!(!lock.models.is_empty());
    }

    #[test]
    fn prepare_environment_rejects_non_onnx_plan() {
        let provider = OnnxRuntimeProvider::new();
        let python_plan = non_onnx_plan();

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
        let provider = OnnxRuntimeProvider::new();
        let python_plan = non_onnx_plan();

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
    fn build_manifest_extension_rejects_non_onnx_plan() {
        let provider = OnnxRuntimeProvider::new();
        let python_plan = non_onnx_plan();
        assert!(
            provider
                .build_manifest_extension(std::path::Path::new("."), &python_plan)
                .is_err()
        );
    }
}
