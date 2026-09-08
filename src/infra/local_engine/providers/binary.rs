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
//! - **self-test 真实执行**：`<exe> --blink-backend-probe --backend ...`
//!   执行 worker 权威 backend probe，核对 requested/actual/device 后才算
//!   self-test 通过——不用"文件存在"或协议版本回显冒充。
//! - 不创建 venv、不执行 pip、不读取用户代码解释器。

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};

use sha2::{Digest, Sha256};

use super::{
    BinaryArtifactPlan, BinaryInstallPlan, CompatibilityCheck, InstallPlan, InstallSink,
    ManifestExtension, PrepareResult, ResolvedProfile, RuntimeError, RuntimeProvider,
};
use crate::infra::local_engine::runtime;
use crate::infra::local_engine::runtime::ComputeBackend;

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

#[derive(Debug, Clone)]
struct ManifestFile {
    sha256: String,
    size_bytes: Option<u64>,
}

/// 根据冻结的 profile 选择单个 artifact 的来源与布局。
fn artifact_plan_for(
    plan: &BinaryInstallPlan,
    artifact_id: &runtime::ArtifactId,
) -> Result<BinaryArtifactPlan, RuntimeError> {
    if let Some(spec) = plan
        .artifact_plans
        .iter()
        .find(|spec| spec.artifact_id == *artifact_id)
    {
        return Ok(spec.clone());
    }
    // 兼容旧的单 artifact CPU manifest；GPU artifact 不得复用它。
    if plan.archive_artifact_id == *artifact_id {
        return Ok(BinaryArtifactPlan {
            artifact_id: artifact_id.clone(),
            archive_url: plan.archive_url.clone(),
            archive_sha256: plan.archive_sha256.clone(),
            executable: plan.executable.clone(),
            backend_dir: None,
            dependencies: Vec::new(),
            file_allowlist: Vec::new(),
            bundled_dir: plan.bundled_dir.clone(),
        });
    }
    Err(RuntimeError::InstallFailed {
        message: format!(
            "resolved profile artifact '{}' 未在 ManagedBinary 安装计划中声明",
            artifact_id
        ),
    })
}

/// 返回 artifact 的显式依赖闭包，依赖项排在被依赖项之前。
fn artifact_plan_closure(
    plan: &BinaryInstallPlan,
    root: &runtime::ArtifactId,
) -> Result<Vec<BinaryArtifactPlan>, RuntimeError> {
    fn visit(
        plan: &BinaryInstallPlan,
        artifact_id: &runtime::ArtifactId,
        visiting: &mut BTreeSet<String>,
        visited: &mut BTreeSet<String>,
        output: &mut Vec<BinaryArtifactPlan>,
    ) -> Result<(), RuntimeError> {
        if visited.contains(artifact_id.as_str()) {
            return Ok(());
        }
        if !visiting.insert(artifact_id.to_string()) {
            return Err(RuntimeError::InstallFailed {
                message: format!("artifact 依赖闭包存在循环: {artifact_id}"),
            });
        }
        let current = artifact_plan_for(plan, artifact_id)?;
        for dependency in &current.dependencies {
            visit(plan, dependency, visiting, visited, output)?;
        }
        visiting.remove(artifact_id.as_str());
        visited.insert(artifact_id.to_string());
        output.push(current);
        Ok(())
    }

    let mut visiting = BTreeSet::new();
    let mut visited = BTreeSet::new();
    let mut output = Vec::new();
    visit(plan, root, &mut visiting, &mut visited, &mut output)?;
    Ok(output)
}

/// 定位捆绑 worker 目录。
///
/// infra 不依赖 app，这里按固定候选布局解析（exe 同级 / 上溯仓库根 /
/// resources 子目录）。候选构造与命中判定拆开，便于对安装布局做单元测试。
fn resolve_bundled_dir(relative: &str) -> Option<std::path::PathBuf> {
    if let Some(root) = super::debug_local_artifact_root()
        && let Some(dir) = find_local_artifact_dir(&root, relative)
    {
        return Some(dir);
    }
    let exe_dir = current_exe_dir()?;
    find_bundled_dir(&exe_dir, relative)
}

/// 在显式开发产物根目录中解析单个 artifact。路径仍按受信相对路径规则
/// 收口，并要求 manifest 存在；后续安装流程会继续执行完整内容校验。
fn find_local_artifact_dir(root: &Path, relative: &str) -> Option<std::path::PathBuf> {
    let relative = safe_relative_file(relative).ok()?;
    let candidate = root.join(relative);
    candidate
        .join("manifest.json")
        .is_file()
        .then_some(candidate)
}

/// 当前进程 exe 所在目录。
fn current_exe_dir() -> Option<std::path::PathBuf> {
    let exe = std::env::current_exe().ok()?;
    exe.parent().map(|p| p.to_path_buf())
}

/// 在候选布局中命中第一个含 manifest.json 的捆绑目录。
fn find_bundled_dir(exe_dir: &Path, relative: &str) -> Option<std::path::PathBuf> {
    bundled_dir_candidates(exe_dir, relative)
        .into_iter()
        .find(|d| d.join("manifest.json").is_file())
}

/// 构造捆绑目录候选列表（顺序即优先级）。
///
/// 每个候选独立判断：某级父目录不存在（如安装目录浅至盘根下一级，
/// 上溯链提前耗尽）只跳过该候选，绝不影响后续候选——安装版布局
/// （exe 同级 `resources/`）必须永远参与命中判定。
fn bundled_dir_candidates(exe_dir: &Path, relative: &str) -> Vec<std::path::PathBuf> {
    let rel = std::path::Path::new(relative);
    let mut candidates = vec![exe_dir.join(rel)];
    // dev 布局上溯：target/debug → 仓库根（上溯 2 级）
    if let Some(root) = exe_dir.parent().and_then(|p| p.parent()) {
        candidates.push(root.join("resources").join(rel));
    }
    // 测试二进制位于 target/debug/deps/ —— 需再上溯一级
    if let Some(root) = exe_dir
        .parent()
        .and_then(|p| p.parent())
        .and_then(|p| p.parent())
    {
        candidates.push(root.join("resources").join(rel));
    }
    // release 安装布局：exe 同级 resources/
    candidates.push(exe_dir.join("resources").join(rel));
    candidates
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

fn safe_relative_file(path: &str) -> Result<PathBuf, RuntimeError> {
    let normalized = path.replace('\\', "/");
    let candidate = Path::new(&normalized);
    if normalized.is_empty() || candidate.is_absolute() {
        return Err(RuntimeError::PathTraversal {
            path: path.to_string(),
        });
    }
    for component in candidate.components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(RuntimeError::PathTraversal {
                    path: path.to_string(),
                });
            }
        }
    }
    Ok(candidate.to_path_buf())
}

/// 解析 deployment 内的受信相对路径，并确认 canonical path 仍在根目录内。
///
/// manifest/descriptor 中的路径即使来自编译期声明，也必须经过同一条边界
/// 校验；启动和 self-test 不能因为路径“看起来像内部值”而绕过目录约束。
fn deployment_path(root: &Path, relative: &str) -> Result<PathBuf, RuntimeError> {
    let safe = safe_relative_file(relative)?;
    let canonical_root = root.canonicalize().map_err(RuntimeError::Io)?;
    let canonical_path = root.join(safe).canonicalize().map_err(RuntimeError::Io)?;
    if !canonical_path.starts_with(&canonical_root) {
        return Err(RuntimeError::PathTraversal {
            path: relative.to_string(),
        });
    }
    Ok(canonical_path)
}

fn manifest_files(
    value: &serde_json::Value,
) -> Result<BTreeMap<String, ManifestFile>, RuntimeError> {
    let mut files = BTreeMap::new();
    if let Some(map) = value.as_object() {
        for (name, meta) in map {
            let safe = safe_relative_file(name)?;
            let key = safe.to_string_lossy().replace('\\', "/");
            files.insert(key.clone(), parse_manifest_file(meta, &key)?);
        }
    } else if let Some(entries) = value.as_array() {
        for entry in entries {
            let name = entry
                .get("path")
                .and_then(|value| value.as_str())
                .ok_or_else(|| RuntimeError::InstallFailed {
                    message: "worker manifest file 缺少 path".to_string(),
                })?;
            let safe = safe_relative_file(name)?;
            let key = safe.to_string_lossy().replace('\\', "/");
            if files.contains_key(&key) {
                return Err(RuntimeError::InstallFailed {
                    message: format!("worker manifest 重复声明文件: {key}"),
                });
            }
            files.insert(key.clone(), parse_manifest_file(entry, &key)?);
        }
    } else {
        return Err(RuntimeError::InstallFailed {
            message: "worker manifest 的 files 必须是对象或数组".to_string(),
        });
    }
    if files.is_empty() {
        return Err(RuntimeError::InstallFailed {
            message: "worker manifest 未包含文件条目".to_string(),
        });
    }
    Ok(files)
}

fn parse_manifest_file(meta: &serde_json::Value, name: &str) -> Result<ManifestFile, RuntimeError> {
    let Some(sha) = meta.get("sha256").and_then(|s| s.as_str()) else {
        return Err(RuntimeError::InstallFailed {
            message: format!("worker manifest 缺少 {name} 的 SHA-256"),
        });
    };
    if sha.len() != 64 || !sha.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(RuntimeError::InstallFailed {
            message: format!("worker manifest 的 {name} SHA-256 非法"),
        });
    }
    Ok(ManifestFile {
        sha256: sha.to_ascii_lowercase(),
        size_bytes: meta.get("size_bytes").and_then(|value| value.as_u64()),
    })
}

fn select_manifest_files(
    manifest: &serde_json::Value,
    expected_artifact: &runtime::ArtifactId,
) -> Result<BTreeMap<String, ManifestFile>, RuntimeError> {
    if let Some(artifacts) = manifest.get("artifacts") {
        if let Some(map) = artifacts.as_object() {
            let Some(entry) = map.get(expected_artifact.as_str()) else {
                return Err(RuntimeError::InstallFailed {
                    message: format!(
                        "worker manifest 未声明 resolved artifact '{}'",
                        expected_artifact
                    ),
                });
            };
            return manifest_files(entry.get("files").ok_or_else(|| {
                RuntimeError::InstallFailed {
                    message: format!("artifact '{}' 缺少 files 闭包", expected_artifact),
                }
            })?);
        }
        if let Some(entries) = artifacts.as_array() {
            let Some(entry) = entries.iter().find(|entry| {
                entry.get("artifact_id").and_then(|id| id.as_str())
                    == Some(expected_artifact.as_str())
            }) else {
                return Err(RuntimeError::InstallFailed {
                    message: format!(
                        "worker manifest 未声明 resolved artifact '{}'",
                        expected_artifact
                    ),
                });
            };
            return manifest_files(entry.get("files").ok_or_else(|| {
                RuntimeError::InstallFailed {
                    message: format!("artifact '{}' 缺少 files 闭包", expected_artifact),
                }
            })?);
        }
        return Err(RuntimeError::InstallFailed {
            message: "worker manifest 的 artifacts 必须是对象或数组".to_string(),
        });
    }

    let manifest_artifact_id = manifest
        .get("artifact_id")
        .and_then(|id| id.as_str())
        .or_else(|| manifest.pointer("/artifact/id").and_then(|id| id.as_str()));
    if let Some(id) = manifest_artifact_id
        && id != expected_artifact.as_str()
    {
        return Err(RuntimeError::InstallFailed {
            message: format!(
                "worker manifest artifact_id='{id}' 与 resolved artifact='{expected_artifact}' 不一致"
            ),
        });
    }
    manifest_files(
        manifest
            .get("files")
            .ok_or_else(|| RuntimeError::InstallFailed {
                message: "worker manifest 缺少 files/artifacts".to_string(),
            })?,
    )
}

fn collect_regular_files(
    root: &Path,
    current: &Path,
    out: &mut BTreeSet<String>,
) -> Result<(), RuntimeError> {
    for entry in std::fs::read_dir(current)? {
        let entry = entry?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|_| RuntimeError::PathTraversal {
                path: path.display().to_string(),
            })?;
        if relative == Path::new("manifest.json") {
            continue;
        }
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink() {
            return Err(RuntimeError::PathTraversal {
                path: relative.display().to_string(),
            });
        }
        if metadata.is_dir() {
            collect_regular_files(root, &path, out)?;
        } else if metadata.is_file() {
            out.insert(relative.to_string_lossy().replace('\\', "/"));
        } else {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "worker artifact 包含不支持的文件类型: {}",
                    relative.display()
                ),
            });
        }
    }
    Ok(())
}

/// 读取随发布 manifest.json 并校验选定 artifact 的完整文件闭包。
fn verify_bundled_source(
    dir: &Path,
    expected_artifact: &runtime::ArtifactId,
    allowlist: &[String],
) -> Result<BundledSource, RuntimeError> {
    let manifest_path = dir.join("manifest.json");
    let text =
        std::fs::read_to_string(&manifest_path).map_err(|e| RuntimeError::InstallFailed {
            message: format!("读取 {} 失败: {e}", manifest_path.display()),
        })?;
    let v: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| RuntimeError::InstallFailed {
            message: format!("manifest.json 解析失败: {e}"),
        })?;
    let schema = v.get("schema").and_then(|s| s.as_u64()).unwrap_or(0);
    if schema != 1 && schema != 2 {
        return Err(RuntimeError::InstallFailed {
            message: format!("worker manifest schema 不支持: {schema}"),
        });
    }

    let selected = select_manifest_files(&v, expected_artifact)?;
    let allowlist: BTreeSet<String> = allowlist
        .iter()
        .map(|name| safe_relative_file(name).map(|p| p.to_string_lossy().replace('\\', "/")))
        .collect::<Result<_, _>>()?;
    if !allowlist.is_empty()
        && (allowlist.len() != selected.len()
            || allowlist.iter().any(|name| !selected.contains_key(name)))
    {
        return Err(RuntimeError::InstallFailed {
            message: format!("artifact '{expected_artifact}' 文件白名单与 manifest 不一致"),
        });
    }

    let canonical_root = dir.canonicalize().map_err(RuntimeError::Io)?;
    let mut actual_files = BTreeSet::new();

    // 逐文件 hash 校验
    for (name, expected) in &selected {
        let relative = safe_relative_file(name)?;
        let path = dir.join(&relative);
        let metadata = std::fs::symlink_metadata(&path)?;
        if metadata.file_type().is_symlink()
            || !metadata.is_file()
            || !path.canonicalize()?.starts_with(&canonical_root)
        {
            return Err(RuntimeError::InstallFailed {
                message: format!("捆绑 worker 文件缺失或路径越界: {name}"),
            });
        }
        let actual_size = metadata.len();
        if expected.size_bytes.is_some_and(|size| size != actual_size) {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "worker {name} 文件大小不匹配（manifest={:?} actual={actual_size}）",
                    expected.size_bytes
                ),
            });
        }
        let actual = sha256_file(&path)?;
        if actual != expected.sha256 {
            return Err(RuntimeError::InstallFailed {
                message: format!(
                    "worker {name} SHA-256 不匹配（manifest={} actual={actual}）",
                    expected.sha256
                ),
            });
        }
        actual_files.insert(name.clone());
    }
    collect_regular_files(dir, dir, &mut actual_files)?;
    let selected_names: BTreeSet<String> = selected.keys().cloned().collect();
    if actual_files != selected_names {
        let extras: Vec<_> = actual_files.difference(&selected_names).cloned().collect();
        return Err(RuntimeError::InstallFailed {
            message: format!("worker artifact 含未声明文件: {}", extras.join(", ")),
        });
    }
    Ok(BundledSource {
        dir: dir.to_path_buf(),
        files: selected
            .into_iter()
            .map(|(name, file)| (name, file.sha256))
            .collect(),
    })
}

async fn download_archive(
    url: &str,
    expected_sha256: &str,
    dest: &Path,
    cancel_token: Option<&tokio_util::sync::CancellationToken>,
    sink: Option<&dyn InstallSink>,
) -> Result<(), RuntimeError> {
    use tokio::io::AsyncWriteExt;

    if let Some(ct) = cancel_token
        && ct.is_cancelled()
    {
        return Err(RuntimeError::OperationCancelled {
            message: "worker artifact 下载开始前被取消".to_string(),
        });
    }
    if let Some(s) = sink {
        s.on_log("info", "正在下载锁定的 worker artifact");
    }
    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .map_err(|e| RuntimeError::InstallFailed {
            message: format!("HTTP client 构造失败: {e}"),
        })?;
    let response = tokio::select! {
        result = client.get(url).send() => result.map_err(|e| RuntimeError::InstallFailed {
            message: format!("worker artifact 下载失败: {e}"),
        })?,
        _ = async {
            match cancel_token {
                Some(ct) => ct.cancelled().await,
                None => std::future::pending().await,
            }
        } => return Err(RuntimeError::OperationCancelled {
            message: "worker artifact 下载被取消".to_string(),
        }),
    };
    if !response.status().is_success() {
        return Err(RuntimeError::InstallFailed {
            message: format!("worker artifact 下载失败: HTTP {}", response.status()),
        });
    }
    let total = response.content_length();
    let temp = dest.with_extension("download.tmp");
    let mut file = tokio::fs::File::create(&temp)
        .await
        .map_err(RuntimeError::Io)?;
    let mut stream = response.bytes_stream();
    let mut hasher = Sha256::new();
    let mut downloaded = 0u64;
    use futures::StreamExt;
    loop {
        tokio::select! {
            chunk = stream.next() => match chunk {
                Some(Ok(bytes)) => {
                    hasher.update(&bytes);
                    file.write_all(&bytes).await.map_err(RuntimeError::Io)?;
                    downloaded += bytes.len() as u64;
                    if let Some(s) = sink { s.on_progress(downloaded, total); }
                }
                Some(Err(e)) => {
                    let _ = tokio::fs::remove_file(&temp).await;
                    return Err(RuntimeError::InstallFailed { message: format!("worker artifact 下载流失败: {e}") });
                }
                None => break,
            },
            _ = async {
                match cancel_token {
                    Some(ct) => ct.cancelled().await,
                    None => std::future::pending().await,
                }
            } => {
                let _ = tokio::fs::remove_file(&temp).await;
                return Err(RuntimeError::OperationCancelled { message: "worker artifact 下载被取消".to_string() });
            }
        }
    }
    file.flush().await.map_err(RuntimeError::Io)?;
    drop(file);
    let actual = format!("{:x}", hasher.finalize());
    if actual != expected_sha256.to_ascii_lowercase() {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(RuntimeError::InstallFailed {
            message: format!("worker artifact SHA-256 不匹配: expected={expected_sha256}"),
        });
    }
    tokio::fs::rename(&temp, dest)
        .await
        .map_err(RuntimeError::Io)
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<(), RuntimeError> {
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).map_err(|e| RuntimeError::InstallFailed {
        message: format!("worker artifact ZIP 解析失败: {e}"),
    })?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| RuntimeError::InstallFailed {
                message: format!("worker artifact ZIP 条目读取失败: {e}"),
            })?;
        let enclosed = entry
            .enclosed_name()
            .ok_or_else(|| RuntimeError::PathTraversal {
                path: entry.name().to_string(),
            })?;
        let relative = safe_relative_file(&enclosed.to_string_lossy())?;
        let output = destination.join(relative);
        if entry.is_dir() {
            std::fs::create_dir_all(output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = std::fs::File::create(output)?;
        std::io::copy(&mut entry, &mut out)?;
    }
    Ok(())
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
            CompatibilityCheck::RequiresCuda { min_version } => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                // 预筛只用于减少无效安装；权威结论仍由 worker probe 给出。
                Ok(crate::infra::platform::gpu::cuda_meets_min_version(
                    min_version.as_deref(),
                ))
            }
            CompatibilityCheck::RequiresVulkan => {
                if !self.allow_gpu {
                    return Ok(false);
                }
                Ok(crate::infra::platform::gpu::detect_vulkan().is_some())
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
        resolved_profile: &ResolvedProfile,
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

        let artifact_plans = artifact_plan_closure(binary_plan, &resolved_profile.artifact_id)?;

        // 1. 解析并校验依赖闭包。bundled 与网络来源都保持在本次事务的
        // staging/temp 范围内；旧 active deployment 在整个过程中保持不变。
        let mut sources = Vec::with_capacity(artifact_plans.len());
        let mut download_temps = Vec::new();
        for artifact_plan in &artifact_plans {
            let source = if let Some(bundled_relative) = &artifact_plan.bundled_dir {
                let source_dir = resolve_bundled_dir(bundled_relative).ok_or_else(|| {
                    RuntimeError::InstallFailed {
                        message: format!(
                            "未找到 artifact '{}' 的捆绑目录（{bundled_relative}）",
                            artifact_plan.artifact_id
                        ),
                    }
                })?;
                let source_dir_for_verify = source_dir.clone();
                let expected_artifact = artifact_plan.artifact_id.clone();
                let allowlist = artifact_plan.file_allowlist.clone();
                tokio::task::spawn_blocking(move || {
                    verify_bundled_source(&source_dir_for_verify, &expected_artifact, &allowlist)
                })
                .await
                .map_err(|e| RuntimeError::InstallFailed {
                    message: format!("spawn_blocking verify_bundled_source 失败: {e}"),
                })??
            } else {
                if artifact_plan.archive_url.is_empty()
                    || artifact_plan.archive_sha256.len() != 64
                    || !artifact_plan
                        .archive_sha256
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit())
                    || !artifact_plan.archive_url.starts_with("https://")
                    || artifact_plan.archive_url.contains("latest")
                {
                    return Err(RuntimeError::InstallFailed {
                        message: format!(
                            "artifact '{}' 的网络来源未锁定 URL/version/hash",
                            artifact_plan.artifact_id
                        ),
                    });
                }
                let download_temp =
                    tempfile::tempdir_in(staging_dir.parent().unwrap_or(staging_dir))
                        .map_err(RuntimeError::Io)?;
                let archive_path = download_temp.path().join("artifact.zip");
                download_archive(
                    &artifact_plan.archive_url,
                    &artifact_plan.archive_sha256,
                    &archive_path,
                    cancel_token,
                    sink,
                )
                .await?;
                let extracted_dir = download_temp.path().join("extracted");
                std::fs::create_dir_all(&extracted_dir)?;
                let archive_for_extract = archive_path.clone();
                let extract_destination = extracted_dir.clone();
                tokio::task::spawn_blocking(move || {
                    extract_archive(&archive_for_extract, &extract_destination)
                })
                .await
                .map_err(|e| RuntimeError::InstallFailed {
                    message: format!("spawn_blocking 解包 worker artifact 失败: {e}"),
                })??;
                let expected_artifact = artifact_plan.artifact_id.clone();
                let allowlist = artifact_plan.file_allowlist.clone();
                let source = verify_bundled_source(&extracted_dir, &expected_artifact, &allowlist)?;
                download_temps.push(download_temp);
                source
            };
            if let Some(s) = sink {
                s.on_log(
                    "info",
                    &format!(
                        "worker artifact hash 校验通过（artifact={}，{} 个文件）",
                        artifact_plan.artifact_id,
                        source.files.len()
                    ),
                );
            }
            sources.push(source);
        }

        if let Some(ct) = cancel_token
            && ct.is_cancelled()
        {
            return Err(RuntimeError::OperationCancelled {
                message: "ManagedBinary 安装在复制前被取消".to_string(),
            });
        }

        // 2. 只复制已校验依赖闭包的文件集合；不复制额外 DLL，也不把
        // artifact source manifest 混入 deployment manifest。
        if let Some(s) = sink {
            s.on_stage("installing");
        }
        let staging_owned = staging_dir.to_path_buf();
        let mut files_for_aggregate = BTreeMap::new();
        for source in &sources {
            for (name, sha) in &source.files {
                if let Some(existing) = files_for_aggregate.get(name)
                    && existing != sha
                {
                    return Err(RuntimeError::InstallFailed {
                        message: format!("artifact 依赖文件 hash 冲突: {name}"),
                    });
                }
                files_for_aggregate.insert(name.clone(), sha.clone());
            }
        }
        let files_for_aggregate: Vec<(String, String)> = files_for_aggregate.into_iter().collect();
        let sources_owned = sources;
        tokio::task::spawn_blocking(move || -> Result<(), RuntimeError> {
            for source in &sources_owned {
                for (name, _) in &source.files {
                    let source_path = source.dir.join(name);
                    let dest_path = staging_owned.join(name);
                    if dest_path.is_file() {
                        continue;
                    }
                    if let Some(parent) = dest_path.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::copy(source_path, dest_path)?;
                }
            }
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
                artifact_id: resolved_profile.artifact_id.clone(),
                sha256: aggregate_hex,
            },
        })
    }

    async fn self_test(
        &self,
        deployment_dir: &Path,
        plan: &InstallPlan,
        resolved_profile: &ResolvedProfile,
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

        let artifact_plan = artifact_plan_for(binary_plan, &resolved_profile.artifact_id)?;

        if binary_plan.self_test_command.is_empty() {
            return Err(RuntimeError::SelfTestFailed {
                message: "ManagedBinary 未声明 backend probe executable".to_string(),
            });
        }

        if let Some(s) = sink {
            s.on_stage("self_test");
            s.on_log(
                "info",
                "执行 worker backend probe（--blink-backend-probe）...",
            );
        }

        let model_executable = binary_plan
            .model_executables
            .iter()
            .find(|(model_id, _)| model_id == &resolved_profile.model_id)
            .map(|(_, executable)| executable.as_str());
        let exe_rel = model_executable
            .or_else(|| {
                (!artifact_plan.executable.is_empty()).then_some(artifact_plan.executable.as_str())
            })
            .unwrap_or(&binary_plan.self_test_command[0]);
        let exe = deployment_path(deployment_dir, exe_rel)?;
        if !exe.is_file() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!("self-test 可执行文件缺失: {}", exe.display()),
            });
        }

        let mut cmd = crate::infra::platform::no_window_tokio(tokio::process::Command::new(&exe));
        let backend_dir = artifact_plan
            .backend_dir
            .as_deref()
            .map(|relative| deployment_path(deployment_dir, relative))
            .transpose()?
            .unwrap_or(deployment_dir.canonicalize().map_err(RuntimeError::Io)?);
        if !backend_dir.is_dir() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!("backend 目录缺失: {}", backend_dir.display()),
            });
        }
        cmd.args([
            "--blink-backend-probe",
            "--backend",
            &resolved_profile.backend.to_string(),
            "--backend-dir",
            &backend_dir.display().to_string(),
        ])
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

        // stdout/stderr 必须并发 drain，否则管道写满后进程阻塞写另一端导致死锁。
        // 串行 read_to_end 会死锁：先读 stdout 时 stderr 管道写满 → 进程阻塞 →
        // stdout 永远不会 EOF。改为并发 drain + 有界 capture。
        let mut stdout_pipe = child.stdout.take();
        let mut stderr_pipe = child.stderr.take();

        // 超过 64KB capture 时截断，防止无界内存增长，但继续 drain 管道避免死锁。
        const CAPTURE_LIMIT: usize = 64 * 1024;

        #[allow(unused_assignments)]
        let mut stdout_buf = Vec::new();
        #[allow(unused_assignments)]
        let mut stderr_buf = Vec::new();

        let stdout_fut = async {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            if let Some(p) = stdout_pipe.as_mut() {
                let mut tmp = [0u8; 4096];
                loop {
                    match p.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if buf.len() + n <= CAPTURE_LIMIT {
                                buf.extend_from_slice(&tmp[..n]);
                            } else if buf.len() < CAPTURE_LIMIT {
                                let remaining = CAPTURE_LIMIT - buf.len();
                                buf.extend_from_slice(&tmp[..remaining]);
                            }
                            // 超过限制后继续 drain 但不保存——避免管道满
                        }
                        Err(_) => break,
                    }
                }
            }
            buf
        };

        let stderr_fut = async {
            use tokio::io::AsyncReadExt;
            let mut buf = Vec::new();
            if let Some(p) = stderr_pipe.as_mut() {
                let mut tmp = [0u8; 4096];
                loop {
                    match p.read(&mut tmp).await {
                        Ok(0) => break,
                        Ok(n) => {
                            if buf.len() + n <= CAPTURE_LIMIT {
                                buf.extend_from_slice(&tmp[..n]);
                            } else if buf.len() < CAPTURE_LIMIT {
                                let remaining = CAPTURE_LIMIT - buf.len();
                                buf.extend_from_slice(&tmp[..remaining]);
                            }
                        }
                        Err(_) => break,
                    }
                }
            }
            buf
        };

        tokio::pin!(stdout_fut);
        tokio::pin!(stderr_fut);

        // 并发 drain + wait + 超时/取消
        let output = tokio::select! {
            res = child.wait() => {
                // 进程退出后 drain 管道剩余数据
                let (s, e) = tokio::join!(stdout_fut, stderr_fut);
                stdout_buf = s;
                stderr_buf = e;
                res
            }
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                // 超时：终止进程树并 reap
                let _ = child.start_kill();
                let _ = child.wait().await;
                // drain 管道以避免资源泄漏
                let _ = tokio::join!(stdout_fut, stderr_fut);
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
                // 取消：终止进程树并 reap
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = tokio::join!(stdout_fut, stderr_fut);
                return Err(RuntimeError::OperationCancelled {
                    message: "worker self-test 被取消".to_string(),
                });
            }
        }?;

        #[cfg(windows)]
        drop(job_handle);

        if !output.success() {
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "worker self-test 退出码 {:?}: {}",
                    output.code(),
                    String::from_utf8_lossy(&stderr_buf)
                ),
            });
        }

        // 解析 worker probe JSON；只接受 actual backend 与 device 均有明确回报。
        let stdout = String::from_utf8_lossy(&stdout_buf);
        let probe_line = stdout
            .lines()
            .rev()
            .find(|line| !line.trim().is_empty())
            .unwrap_or_default();
        let v: serde_json::Value =
            serde_json::from_str(probe_line).map_err(|e| RuntimeError::SelfTestFailed {
                message: format!("backend probe 输出解析失败: {e}"),
            })?;
        let probe_type = v.get("type").and_then(|t| t.as_str());
        let ok = v
            .get("ok")
            .and_then(|value| value.as_bool())
            .unwrap_or(true);
        let actual = v
            .get("actual_backend")
            .or_else(|| v.get("backend"))
            .and_then(|value| value.as_str())
            .and_then(ComputeBackend::parse);
        let requested = v
            .get("requested_backend")
            .and_then(|value| value.as_str())
            .and_then(ComputeBackend::parse);
        let device_name = v
            .get("device_name")
            .or_else(|| v.get("device"))
            .and_then(|value| value.as_str())
            .filter(|value| !value.trim().is_empty());
        if !matches!(probe_type, Some("backend_probe") | Some("probe"))
            || !ok
            || actual != Some(resolved_profile.backend)
            || requested != Some(resolved_profile.backend)
            || device_name.is_none()
        {
            return Err(RuntimeError::SelfTestFailed {
                message: format!(
                    "backend probe 不匹配: requested={:?}, actual={:?}, expected={}, device={:?}",
                    requested, actual, resolved_profile.backend, device_name
                ),
            });
        }

        if let Some(s) = sink {
            s.on_log(
                "info",
                &format!(
                    "worker backend probe 通过（backend={}，device={}）",
                    resolved_profile.backend,
                    device_name.unwrap_or_default()
                ),
            );
        }
        Ok(())
    }

    fn build_manifest_extension(
        &self,
        deployment_dir: &Path,
        plan: &InstallPlan,
        resolved_profile: &ResolvedProfile,
    ) -> Result<ManifestExtension, RuntimeError> {
        let binary_plan = match plan {
            InstallPlan::ManagedBinary(p) => p,
            _ => {
                return Err(RuntimeError::ManifestSerializeFailed {
                    message: "ManagedBinaryProvider 收到非 ManagedBinary 安装计划".to_string(),
                });
            }
        };

        let artifact_plan = artifact_plan_for(binary_plan, &resolved_profile.artifact_id)?;
        // manifest extension 只记录 staging 中实际复制的文件，不信任 source
        // manifest 里其他 artifact 的条目。
        let mut names = BTreeSet::new();
        collect_regular_files(deployment_dir, deployment_dir, &mut names).map_err(|e| {
            RuntimeError::ManifestSerializeFailed {
                message: format!("扫描部署文件闭包失败: {e}"),
            }
        })?;
        let mut files = Vec::new();
        for name in names {
            let path = deployment_dir.join(&name);
            let sha = sha256_file(&path).map_err(|e| RuntimeError::ManifestSerializeFailed {
                message: format!("计算部署文件 hash 失败: {name}: {e}"),
            })?;
            let size = std::fs::metadata(&path)?.len();
            files.push(runtime::FileEntry {
                is_dll: path
                    .extension()
                    .and_then(|ext| ext.to_str())
                    .is_some_and(|ext| ext.eq_ignore_ascii_case("dll")),
                path: name,
                sha256: sha,
                size,
            });
        }

        let executable = binary_plan
            .model_executables
            .iter()
            .find(|(model_id, _)| model_id == &resolved_profile.model_id)
            .map(|(_, executable)| executable.clone())
            .unwrap_or_else(|| artifact_plan.executable.clone());
        let executable_path = deployment_path(deployment_dir, &executable)?;
        if !executable_path.is_file() {
            return Err(RuntimeError::ManifestSerializeFailed {
                message: format!("manifest executable 缺失: {executable}"),
            });
        }
        let backend_dir = artifact_plan
            .backend_dir
            .as_deref()
            .map(|relative| {
                let path = deployment_path(deployment_dir, relative)?;
                if !path.is_dir() {
                    return Err(RuntimeError::ManifestSerializeFailed {
                        message: format!("manifest backend 目录缺失: {relative}"),
                    });
                }
                let normalized = safe_relative_file(relative)?;
                Ok(normalized.to_string_lossy().replace('\\', "/"))
            })
            .transpose()?;

        Ok(ManifestExtension::ManagedBinary(
            runtime::BinaryManifestExt {
                archive_artifact_id: resolved_profile.artifact_id.clone(),
                archive_sha256: artifact_plan.archive_sha256,
                executable,
                backend_dir,
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

    /// 浅安装布局（距盘根仅两级，如 `D:\DevTools\Blink`）：上溯链在盘根
    /// 提前耗尽，但安装版候选（exe 同级 resources/）必须仍在列表中。
    /// 回归：0.22.7-0.22.10 候选数组内 `?` 短路导致安装版永远解析失败。
    #[test]
    fn bundled_dir_candidates_shallow_install_layout() {
        let exe_dir = Path::new("D:\\DevTools\\Blink");
        assert_eq!(
            exe_dir.parent().and_then(|p| p.parent()),
            Some(Path::new("D:\\"))
        );
        assert_eq!(
            exe_dir
                .parent()
                .and_then(|p| p.parent())
                .and_then(|p| p.parent()),
            None
        );

        let candidates = bundled_dir_candidates(exe_dir, "bin/funasr-worker");
        let last = candidates.last().expect("候选列表不得为空");
        assert_eq!(
            last.as_path(),
            exe_dir
                .join("resources")
                .join("bin/funasr-worker")
                .as_path(),
            "安装版布局候选必须始终参与命中判定"
        );
    }

    /// 命中判定：浅安装布局下 exe 同级 resources/ 可被解析到。
    #[test]
    fn find_bundled_dir_resolves_shallow_install_layout() {
        let root = test_temp_root("shallow-install");
        let exe_dir = root.join("Blink");
        let bundled = exe_dir.join("resources").join("bin").join("funasr-worker");
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::write(bundled.join("manifest.json"), "{\"schema\":1}").unwrap();

        let found = find_bundled_dir(&exe_dir, "bin/funasr-worker");
        assert_eq!(found.as_deref(), Some(bundled.as_path()));

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 命中判定：dev 布局（target/debug → 仓库根 resources/）仍然有效。
    #[test]
    fn find_bundled_dir_resolves_dev_layout() {
        let root = test_temp_root("dev-layout");
        let exe_dir = root.join("target").join("debug");
        let bundled = root.join("resources").join("bin").join("funasr-worker");
        std::fs::create_dir_all(&exe_dir).unwrap();
        std::fs::create_dir_all(&bundled).unwrap();
        std::fs::write(bundled.join("manifest.json"), "{\"schema\":1}").unwrap();

        let found = find_bundled_dir(&exe_dir, "bin/funasr-worker");
        assert_eq!(found.as_deref(), Some(bundled.as_path()));

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn find_local_artifact_dir_resolves_artifact_id_under_explicit_root() {
        let root = test_temp_root("local-artifact-root");
        let artifact = root.join("funasr-runtime-windows-x64-vulkan");
        std::fs::create_dir_all(&artifact).unwrap();
        std::fs::write(artifact.join("manifest.json"), "{\"schema\":2}").unwrap();

        let found = find_local_artifact_dir(&root, "funasr-runtime-windows-x64-vulkan");
        assert_eq!(found.as_deref(), Some(artifact.as_path()));
        assert!(find_local_artifact_dir(&root, "../outside").is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    fn write_manifest_v2_fixture(root: &Path, artifact_id: &str) {
        std::fs::create_dir_all(root.join("backend")).unwrap();
        std::fs::write(root.join("worker.exe"), b"worker").unwrap();
        std::fs::write(root.join("backend/ggml.dll"), b"backend").unwrap();
        let files = ["worker.exe", "backend/ggml.dll"]
            .into_iter()
            .map(|relative| {
                let path = root.join(relative);
                serde_json::json!({
                    "path": relative,
                    "size_bytes": std::fs::metadata(&path).unwrap().len(),
                    "sha256": sha256_file(&path).unwrap(),
                })
            })
            .collect::<Vec<_>>();
        let manifest = serde_json::json!({
            "schema": 2,
            "artifact": { "id": artifact_id },
            "files": files,
        });
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn bundled_manifest_v2_array_is_verified_and_closed() {
        let root = test_temp_root("manifest-v2");
        write_manifest_v2_fixture(&root, "fixture-v2");
        let artifact = runtime::ArtifactId::new("fixture-v2").unwrap();
        let verified = verify_bundled_source(&root, &artifact, &[]).unwrap();
        assert_eq!(verified.files.len(), 2);
        assert!(verified.files.iter().any(|(name, _)| name == "worker.exe"));
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundled_manifest_rejects_hash_tampering_and_extra_files() {
        let root = test_temp_root("manifest-fail-closed");
        write_manifest_v2_fixture(&root, "fixture-v2");
        std::fs::write(root.join("worker.exe"), b"tampered").unwrap();
        let artifact = runtime::ArtifactId::new("fixture-v2").unwrap();
        assert!(verify_bundled_source(&root, &artifact, &[]).is_err());

        write_manifest_v2_fixture(&root, "fixture-v2");
        std::fs::write(root.join("extra.dll"), b"not declared").unwrap();
        assert!(verify_bundled_source(&root, &artifact, &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn bundled_manifest_rejects_wrong_artifact_and_path_traversal() {
        let root = test_temp_root("manifest-identity");
        write_manifest_v2_fixture(&root, "other-artifact");
        let artifact = runtime::ArtifactId::new("fixture-v2").unwrap();
        assert!(verify_bundled_source(&root, &artifact, &[]).is_err());

        let manifest = serde_json::json!({
            "schema": 2,
            "artifact": { "id": "fixture-v2" },
            "files": [{
                "path": "../outside.dll",
                "size_bytes": 1,
                "sha256": "0".repeat(64),
            }],
        });
        std::fs::write(
            root.join("manifest.json"),
            serde_json::to_vec_pretty(&manifest).unwrap(),
        )
        .unwrap();
        assert!(verify_bundled_source(&root, &artifact, &[]).is_err());
        let _ = std::fs::remove_dir_all(&root);
    }

    fn test_temp_root(tag: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blink-binary-test-{}-{}", tag, std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
}
