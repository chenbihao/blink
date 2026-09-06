//! Provider-neutral 部署格式与 IO 契约。
//!
//! 定义所有 provider 共享的闭合类型：deployment manifest、package 状态、
//! cleanup scope 等；以及路径解析、原子文件操作与身份 id 生成。
//!
//! 身份与计算类型（`EngineId`、`ModelId`、`ArtifactId`、`RuntimePlan`、
//! `ComputePreference` 系列、`ModelContract` 等）的**唯一定义在
//! `domain/local_engine/identity`**——本模块只 re-export，供既有 import
//! 路径继续工作，不复制第二套同义类型。
//!
//! ## 设计铁则
//!
//! - **闭合枚举**：`RuntimePlan` 是编译期闭合变体，禁止 String runtime plan
//!   或任意 JSON map 绕过。
//! - **通用格式不含引擎字段**：本模块的类型不出现 torch、funasr、paddleocr
//!   等引擎专属字段。引擎专属状态由 adapter 从 manifest/packages 投影。
//! - **provider 专属字段隔离**：Python 扩展和 Binary 扩展各自有独立的
//!   manifest 扩展类型，不泄漏进通用状态转换代码。
//! - **infra 不依赖 app**：本模块只使用标准库、serde、thiserror、domain
//!   身份类型和 infra 内部类型。
//!
//! ## 目录拓扑
//!
//! deployment key = engine id + implementation（见 `deployment.rs` 的
//! `DeploymentSpace`）；0.22.7/0.22.8 的兼容真源位于 engine 级空间：
//!
//! ```text
//! %APPDATA%\blink\runtimes\
//! ├─ shared\{provider}\{artifact-id}\        # 只读、内容寻址共享资产
//! └─ engines\{engine-id}\
//!    ├─ slot-a\  slot-b\                     # engine 级不可变部署 slot
//!    ├─ staging\{operation-id}\              # engine 级事务临时构建目录
//!    ├─ deployment.json                      # engine 级 active 指针
//!    ├─ transaction.json / residue.json
//!    └─ impl-{implementation}\               # implementation 级空间（0.22.9）
//!       └─ （同构的 slot/staging/指针/journal/residue）
//! ```
//!
//! 部署事务（slot 切换、journal、fail-closed 恢复）与全部空间内路径
//! 派生由 `deployment.rs` 承载；本模块只保留引擎级根目录、模型资产、
//! 共享 artifact 与原子 IO 原语。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[cfg(windows)]
use std::os::windows::ffi::OsStrExt;

// ── domain 身份类型 re-export（唯一定义在 domain/local_engine/identity）──

pub use crate::domain::local_engine::identity::{
    ArtifactId, ArtifactIdentity, BackendObservation, BackendState, ChecksumSource, ComputeBackend,
    ComputePreference, EngineId, FallbackReason, FallbackReasonKind, ModelContract,
    ResolvedProfile, RuntimePlan, verify_backend_consistency,
};

// ── PackageStatus ─────────────────────────────────────────────────────────

/// 包状态（通用，不含引擎专属字段）。
///
/// Python 的 torch/funasr 等包状态由 adapter 从 `packages` 列表投影，
/// infra 层只理解通用的 PackageStatus。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PackageStatus {
    /// 包名（如 `torch`、`funasr`、`paddleocr`）。
    pub name: String,
    /// 已安装版本（None 表示未安装）。
    pub installed_version: Option<String>,
    /// descriptor 锁定的版本要求。
    pub locked_version: String,
    /// 是否满足 descriptor 的版本要求。
    pub satisfies_lock: bool,
}

// ── DeploymentManifest ────────────────────────────────────────────────────

/// Manifest schema 版本。
pub const MANIFEST_SCHEMA_VERSION: u32 = 2;

/// 不可变部署（slot）的完整 manifest。
///
/// 通用部分表达所有 provider 共享的元数据；
/// provider 专属字段通过 `extension` 隔离。
///
/// manifest 的 `install_id` 是部署内容的身份标识（用于日志/事件/lease），
/// 物理位置由 slot 决定——稳定状态下一个引擎只有 active slot 一个部署。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeploymentManifest {
    /// Manifest schema 版本。
    pub schema_version: u32,
    /// 引擎 id。
    pub engine_id: EngineId,
    /// 运行时计划。
    pub runtime_kind: RuntimePlan,
    /// 安装 id（部署内容身份标识）。
    pub install_id: String,
    /// 用户请求的 compute preference。
    pub requested_preference: ComputePreference,
    /// 解析后的 profile。
    pub resolved_profile: ResolvedProfile,
    /// 安装时间（Unix 毫秒）。
    pub installed_at_ms: u64,
    /// artifact 身份标识。
    pub artifact: ArtifactIdentity,
    /// 模型契约（引擎锁定的模型身份）。
    pub model_contract: ModelContract,
    /// fallback 原因（如果 requested != resolved）。
    pub fallback_reasons: Vec<FallbackReason>,
    /// provider 专属扩展。
    pub extension: ManifestExtension,
}

/// provider 专属 manifest 扩展（闭合枚举，不允许任意 JSON）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ManifestExtension {
    /// Python venv 扩展（解释器与锁定包）。
    PythonVenv(PythonManifestExt),
    /// Managed binary 扩展（archive、executable、DLL 及 hash）。
    ManagedBinary(BinaryManifestExt),
    /// ONNX Runtime 扩展（版本化 DLL + 模型 generation，0.22.8）。
    OnnxRuntime(OnnxRuntimeManifestExt),
}

/// Python venv manifest 扩展。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PythonManifestExt {
    /// Python 版本（如 `3.12.8`）。
    pub python_version: String,
    /// Python distribution artifact id（引用共享 artifact）。
    pub python_artifact_id: ArtifactId,
    /// venv 内已安装的包列表。
    pub packages: Vec<PackageStatus>,
    /// uv 版本。
    pub uv_version: String,
    /// package index URL（如果使用非默认 index）。
    pub index_url: Option<String>,
    /// self-test 结果。
    pub self_test_passed: bool,
}

/// Managed binary manifest 扩展。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryManifestExt {
    /// archive artifact id。
    pub archive_artifact_id: ArtifactId,
    /// archive SHA-256。
    pub archive_sha256: String,
    /// 可执行文件路径（相对于部署根）。
    pub executable: String,
    /// 文件清单与 hash。
    pub files: Vec<FileEntry>,
    /// 引用的共享 stdlib artifact（如 Blink 托管 Python distribution）。
    /// 只读依赖，不创建 venv、不执行 pip。
    pub stdlib_artifact: Option<ArtifactIdentity>,
    /// CPU feature 前置条件（如 `avx2`）。
    pub required_cpu_features: Vec<String>,
    /// driver 前置条件（如 `cuda >= 12.0`）。
    pub required_drivers: Vec<String>,
    /// self-test 结果。
    pub self_test_passed: bool,
}

/// ONNX Runtime manifest 扩展（0.22.8）。
///
/// 与 `BinaryManifestExt` 的关键区别：
/// - OnnxRuntime 是**共享动态运行时**，DLL 不启动子进程，由 in-process
///   lazy Session 持有；
/// - manifest 同时冻结 ORT DLL identity 和模型 generation identity；
/// - `dll_identity` 用于 `pending_restart` 判定（已加载不同 DLL identity 时
///   保持 loaded 不变并投影 `pending_restart`）。
#[allow(dead_code)] // 0.22.8 A 包协议位——B/C/D/E 包消费
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OnnxRuntimeManifestExt {
    /// ORT DLL artifact id（版本化、不可变）。
    pub dll_artifact_id: ArtifactId,
    /// ORT DLL SHA-256（hex）。
    pub dll_sha256: String,
    /// ORT 版本（如 `1.20.0`）。
    pub ort_version: String,
    /// DLL 文件清单与 hash。
    pub dll_files: Vec<FileEntry>,
    /// 引用的模型 generation（由 `model_storage` 管理）。
    /// 联合提交时与 DLL identity 一起冻结。
    pub model_generation_id: String,
    /// ORT 执行 provider 模式（如 `cpu` / `dml`），闭合枚举的字符串投影。
    pub execution_provider: String,
    /// ORT `inter_op` 线程数。
    pub inter_op: u32,
    /// ORT `intra_op` 线程数上限。
    pub intra_op: u32,
    /// self-test 结果（隔离验证进程执行，非主进程 staging）。
    pub self_test_passed: bool,
}

/// 文件清单条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileEntry {
    /// 相对于部署根的路径。
    pub path: String,
    /// SHA-256 hash（hex）。
    pub sha256: String,
    /// 文件大小（字节）。
    pub size: u64,
    /// 是否为 DLL（用于 DLL 搜索路径设置）。
    pub is_dll: bool,
}

// ── CleanupScope ──────────────────────────────────────────────────────────

/// 清理范围（明确区分不同 scope，防止误删）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "scope", rename_all = "snake_case")]
pub enum CleanupScope {
    /// 删除一个部署空间（engine 级或 implementation 级）内非 active slot
    /// （residue 感知：被占用时登记残留）。active 判定按空间独立生效。
    EngineDeploymentSlot {
        /// 目标部署空间（engine id + implementation）。
        space: super::deployment::DeploymentSpace,
        /// slot 目录名（`slot-a` / `slot-b`）。
        slot: String,
    },
    /// 清扫引擎全部空间的孤儿 staging。
    EngineStaging { engine_id: EngineId },
    /// 单引擎的模型缓存。
    EngineModelCache { engine_id: EngineId },
    /// provider 共享 artifact（需要 active manifest 引用检查）。
    ProviderSharedArtifact {
        runtime_kind: RuntimePlan,
        artifact_id: ArtifactId,
    },
    /// provider 下载缓存（uv cache 等）。
    ProviderDownloadCache { runtime_kind: RuntimePlan },
}

// ── RuntimeError ──────────────────────────────────────────────────────────

/// Runtime 层错误类型。
#[derive(Debug, thiserror::Error)]
pub enum RuntimeError {
    #[error("路径逃逸: {path}")]
    PathTraversal { path: String },

    #[error("deployment 不存在: {install_id}")]
    GenerationNotFound { install_id: String },

    #[error("deployment.json 解析失败: {message}")]
    CurrentPointerParseFailed { message: String },

    #[error("事务 journal 解析失败: {message}")]
    TransactionJournalInvalid { message: String },

    #[error("manifest 解析失败: {message}")]
    ManifestParseFailed { message: String },

    #[error("manifest 序列化失败: {message}")]
    ManifestSerializeFailed { message: String },

    #[error("manifest schema 版本不兼容: expected={expected}, actual={actual}")]
    ManifestSchemaIncompatible { expected: u32, actual: u32 },

    #[error("staging 目录创建失败: {message}")]
    StagingCreateFailed { message: String },

    #[error("部署提升失败: {message}")]
    GenerationPromoteFailed { message: String },

    #[error("deployment.json 原子替换失败: {message}")]
    CurrentPointerSwitchFailed { message: String },

    #[error("安装失败: {message}")]
    InstallFailed { message: String },

    #[error("self-test 失败: {message}")]
    SelfTestFailed { message: String },

    /// 磁盘空间不足。
    #[error("磁盘空间不足: {message}")]
    InsufficientDiskSpace { message: String },

    /// 操作被取消。
    #[error("操作被取消: {message}")]
    OperationCancelled { message: String },

    #[error("compute profile 解析失败: {message}")]
    ProfileResolutionFailed { message: String },

    #[error("显式 backend 失败（不回退）: {message}")]
    ExplicitBackendFailed { message: String },

    #[error("清理失败: {message}")]
    CleanupFailed { message: String },

    #[error("共享 artifact 仍被引用，拒绝删除: {artifact_id}, refs={ref_count}")]
    ArtifactStillReferenced {
        artifact_id: String,
        ref_count: usize,
    },

    #[error("IO 错误: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON 错误: {0}")]
    Json(#[from] serde_json::Error),
}

impl RuntimeError {
    /// 从 IO 错误推断是否为磁盘空间不足。
    ///
    /// Windows: ERROR_DISK_FULL (112) / ERROR_HANDLE_DISK_FULL (39)
    /// Unix: ENOSPC (28)
    #[allow(dead_code)]
    pub fn from_io_disk_space(e: std::io::Error) -> Self {
        #[cfg(windows)]
        let is_disk_full = matches!(
            e.raw_os_error(),
            Some(112) | Some(39) // ERROR_DISK_FULL, ERROR_HANDLE_DISK_FULL
        );
        #[cfg(not(windows))]
        let is_disk_full = matches!(e.raw_os_error(), Some(28)); // ENOSPC

        if is_disk_full {
            Self::InsufficientDiskSpace {
                message: format!("磁盘空间不足: {e}"),
            }
        } else {
            Self::Io(e)
        }
    }
}

// ── 路径安全 API ───────────────────────────────────────────────────────────

/// 运行时根目录：`%APPDATA%\blink\runtimes`
pub fn runtimes_root() -> PathBuf {
    #[cfg(test)]
    {
        // 单测不得触碰真实 `%APPDATA%\blink`。进程级唯一根目录同时避免不同
        // cargo test 进程互相清理指针/引用测试数据。
        std::env::temp_dir().join(format!("blink-runtime-tests-{}", std::process::id()))
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::app_data_dir().join("runtimes")
}

/// 共享 artifact 目录：`runtimes/shared/{provider}/{artifact_id}`
pub fn shared_artifact_dir(runtime_kind: RuntimePlan, artifact_id: &ArtifactId) -> PathBuf {
    runtimes_root()
        .join("shared")
        .join(runtime_kind.provider_id())
        .join(artifact_id.as_str())
}

/// 引擎根目录：`runtimes/engines/{engine_id}`
///
/// engine 根是 deployment 空间的父目录（engine 级空间本身 + 各
/// `impl-{implementation}` 子空间）；deployment 内部路径一律经
/// `DeploymentSpace` 派生，不经本函数拼接。
pub fn engine_root(engine_id: &EngineId) -> PathBuf {
    runtimes_root().join("engines").join(engine_id.as_str())
}

/// 模型缓存根目录：`%APPDATA%\blink\models`
pub fn models_root() -> PathBuf {
    #[cfg(test)]
    {
        runtimes_root().join("models")
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::app_data_dir().join("models")
}

/// 引擎模型缓存目录：`models/{engine_id}`
pub fn engine_model_cache_dir(engine_id: &EngineId) -> PathBuf {
    models_root().join(engine_id.as_str())
}

/// Python 公共资产根目录：`%APPDATA%\blink\python`
///
/// 0.22.10 起 PythonVenv provider 已退役；此目录只承载 legacy 用户资产
/// （旧 uv 安装、uv cache、uv 下载的 Python distributions），
/// 供设置页存储管理扫描与清理。
pub fn python_shared_root() -> PathBuf {
    #[cfg(test)]
    {
        runtimes_root().join("python")
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::python_dir()
}

/// uv cache 目录：`python\cache\uv`（legacy 清理目标）。
pub fn uv_cache_dir() -> PathBuf {
    python_shared_root().join("cache").join("uv")
}

// ── 路径安全校验 ───────────────────────────────────────────────────────────

/// 校验 install_id（只允许 `[a-z0-9-]`，防止路径逃逸）。
pub fn validate_install_id(id: &str) -> Result<(), RuntimeError> {
    if id.is_empty() || id.len() > 128 {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    if !id
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    if id.starts_with('-') || id.ends_with('-') || id.contains("--") {
        return Err(RuntimeError::PathTraversal {
            path: id.to_string(),
        });
    }
    Ok(())
}

/// 校验 operation_id（只允许 `[a-z0-9-]`，防止路径逃逸）。
pub fn validate_operation_id(id: &str) -> Result<(), RuntimeError> {
    validate_install_id(id)
}

/// 安全校验路径不逃逸出指定根目录。
///
/// 检查规范化后的路径是否以 `root` 为前缀。
/// 拒绝 `..`、绝对路径和符号链接逃逸。
pub fn ensure_path_within(root: &Path, path: &Path) -> Result<PathBuf, RuntimeError> {
    let canonical_root = root.canonicalize().map_err(RuntimeError::Io)?;
    let canonical_path = if path.is_absolute() {
        path.canonicalize().map_err(RuntimeError::Io)?
    } else {
        root.join(path).canonicalize().map_err(RuntimeError::Io)?
    };
    if !canonical_path.starts_with(&canonical_root) {
        return Err(RuntimeError::PathTraversal {
            path: path.display().to_string(),
        });
    }
    Ok(canonical_path)
}

// ── 原子文件操作 ───────────────────────────────────────────────────────────

/// 原子写入小文件（同目录临时文件 + ReplaceFileW/rename）。
///
/// Windows 上使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)` 原子替换，
/// 保证目标文件在任何时刻都存在，即使进程崩溃也不会丢失新旧文件。
pub fn atomic_write_file(path: &Path, content: &[u8]) -> Result<(), RuntimeError> {
    let parent = path
        .parent()
        .ok_or_else(|| RuntimeError::CurrentPointerSwitchFailed {
            message: "路径无父目录".to_string(),
        })?;
    std::fs::create_dir_all(parent)?;

    // 创建同目录临时文件
    let tmp_name = format!(
        ".tmp_{}_{}",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos()
    );
    let tmp_path = parent.join(&tmp_name);

    // 写入临时文件
    std::fs::write(&tmp_path, content).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        RuntimeError::Io(e)
    })?;

    // 原子替换
    atomic_replace(&tmp_path, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp_path);
        RuntimeError::CurrentPointerSwitchFailed {
            message: format!("原子替换失败: {e}"),
        }
    })?;

    Ok(())
}

/// 原子替换文件（跨平台）。
///
/// - Windows: 使用 `MoveFileExW(REPLACE_EXISTING | WRITE_THROUGH)`。
/// - Unix: `std::fs::rename`（POSIX rename 是原子的）。
///
fn atomic_replace(from: &Path, to: &Path) -> Result<(), std::io::Error> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::{
            MOVE_FILE_FLAGS, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        };
        use windows::core::PCWSTR;

        let target_wide: Vec<u16> = to
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();
        let source_wide: Vec<u16> = from
            .as_os_str()
            .encode_wide()
            .chain(std::iter::once(0))
            .collect();

        let flags = MOVE_FILE_FLAGS(MOVEFILE_REPLACE_EXISTING.0 | MOVEFILE_WRITE_THROUGH.0);

        // MoveFileExW 偶发"拒绝访问 (0x80070005)"——杀软实时扫描或并发
        // 测试竞争短暂占用文件句柄。这是暂态错误，有限次重试即可恢复。
        // 生产路径（单引擎安装）几乎不会遇到；重试只为消除测试 flaky。
        const MAX_RETRIES: u32 = 5;
        let mut last_err: Option<windows::core::Error> = None;
        for attempt in 0..MAX_RETRIES {
            let result = unsafe {
                MoveFileExW(
                    PCWSTR(source_wide.as_ptr()),
                    PCWSTR(target_wide.as_ptr()),
                    flags,
                )
            };
            match result {
                Ok(()) => return Ok(()),
                Err(e) => {
                    // 0x80070005 = ERROR_ACCESS_DENIED（杀软/文件锁竞争）
                    // 不用 E_ACCESS_DENIED 常量（某些 windows crate 版本不含它）
                    if e.code() == windows::core::HRESULT::from_win32(0x80070005)
                        && attempt + 1 < MAX_RETRIES
                    {
                        tracing::debug!(
                            attempt = attempt + 1,
                            max = MAX_RETRIES,
                            "MoveFileExW 暂态拒绝访问，重试"
                        );
                        std::thread::sleep(std::time::Duration::from_millis(10 * (1 << attempt)));
                        last_err = Some(e);
                        continue;
                    }
                    return Err(std::io::Error::other(e));
                }
            }
        }
        // 所有重试用尽
        Err(std::io::Error::other(last_err.unwrap_or_else(|| {
            windows::core::Error::from(windows::Win32::Foundation::E_FAIL)
        })))
    }

    #[cfg(not(windows))]
    {
        std::fs::rename(from, to)
    }
}

/// 原子写入 JSON 文件。
pub fn atomic_write_json<T: Serialize>(path: &Path, value: &T) -> Result<(), RuntimeError> {
    let json = serde_json::to_vec_pretty(value)?;
    atomic_write_file(path, &json)
}

// ── slot manifest 校验 ─────────────────────────────────────────────────────

/// 校验 slot 名（固定闭合集合：slot-a / slot-b）。
pub fn validate_slot_name(slot: &str) -> Result<(), RuntimeError> {
    if slot == "slot-a" || slot == "slot-b" {
        Ok(())
    } else {
        Err(RuntimeError::PathTraversal {
            path: slot.to_string(),
        })
    }
}

// ── 共享 artifact 引用扫描（真源：各空间 active 部署 manifest）────────────

/// 共享 artifact 引用记录（来自某引擎某空间的 active 部署 manifest）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArtifactReference {
    pub engine_id: String,
    pub install_id: String,
}

/// 扫描所有引擎的 **active 部署 manifest**，查找引用了指定 artifact 的部署。
///
/// 这是共享 artifact 删除前引用检查的唯一真源——不维护独立的
/// refcount.json（可漂移）；只统计当前有效 deployment manifest 的引用，
/// 事务残留与已删除部署不构成引用。
///
/// 扫描覆盖 engine 级空间与全部 implementation 级空间（`impl-*`）；无法
/// 映射到闭合枚举的 `impl-*` 目录（高版本降级残留）也参与引用统计——
/// 保守方向 fail-closed：未知部署的引用不让共享资产被删除。
pub fn scan_artifact_references(
    runtime_kind: RuntimePlan,
    artifact_id: &ArtifactId,
) -> Result<Vec<ArtifactReference>, RuntimeError> {
    let engines_root = runtimes_root().join("engines");
    if !engines_root.exists() {
        return Ok(Vec::new());
    }

    let mut refs = Vec::new();

    for engine_entry in std::fs::read_dir(&engines_root)? {
        let engine_entry = engine_entry?;
        if !engine_entry.file_type()?.is_dir() {
            continue;
        }
        let engine_name = match engine_entry.file_name().to_str() {
            Some(n) => n.to_string(),
            None => continue,
        };
        let engine_dir = engines_root.join(&engine_name);

        // 候选指针位置：engine 级 + 各 implementation 级空间
        let mut pointer_locations = vec![engine_dir.join("deployment.json")];
        if let Ok(entries) = std::fs::read_dir(&engine_dir) {
            for entry in entries.flatten() {
                if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    continue;
                }
                let name = entry.file_name();
                if name.to_str().is_some_and(|n| n.starts_with("impl-")) {
                    pointer_locations.push(entry.path().join("deployment.json"));
                }
            }
        }

        for pointer_path in pointer_locations {
            if !pointer_path.exists() {
                continue;
            }
            let content = match std::fs::read_to_string(&pointer_path) {
                Ok(c) => c,
                Err(_) => continue,
            };
            let pointer: serde_json::Value = match serde_json::from_str(&content) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let slot = match pointer.get("slot").and_then(|s| s.as_str()) {
                Some(s) => s.to_string(),
                None => continue,
            };
            let install_id = pointer
                .get("install_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .to_string();

            let manifest_file = pointer_path
                .parent()
                .unwrap_or(&engine_dir)
                .join(&slot)
                .join("manifest.json");
            let manifest: DeploymentManifest = match std::fs::read_to_string(&manifest_file)
                .ok()
                .and_then(|c| serde_json::from_str(&c).ok())
            {
                Some(m) => m,
                None => continue,
            };

            // 主 artifact 引用
            if manifest.artifact.runtime_kind == runtime_kind
                && manifest.artifact.artifact_id == *artifact_id
            {
                refs.push(ArtifactReference {
                    engine_id: engine_name.clone(),
                    install_id: install_id.clone(),
                });
            }

            // Binary manifest 中的 stdlib artifact 引用
            if let ManifestExtension::ManagedBinary(ref ext) = manifest.extension
                && let Some(ref stdlib) = ext.stdlib_artifact
                && stdlib.runtime_kind == runtime_kind
                && stdlib.artifact_id == *artifact_id
            {
                refs.push(ArtifactReference {
                    engine_id: engine_name.clone(),
                    install_id: format!("{install_id}#stdlib"),
                });
            }

            // OnnxRuntime manifest 中的 DLL artifact 引用（0.22.8）
            if let ManifestExtension::OnnxRuntime(ref ext) = manifest.extension
                && manifest.artifact.runtime_kind == runtime_kind
                && ext.dll_artifact_id == *artifact_id
            {
                refs.push(ArtifactReference {
                    engine_id: engine_name.clone(),
                    install_id: format!("{install_id}#ort-dll"),
                });
            }
        }
    }

    Ok(refs)
}

// ── 时间戳辅助 ─────────────────────────────────────────────────────────────

/// 当前 Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

/// 生成 install_id（时间戳 + 随机后缀）。
pub fn generate_install_id() -> String {
    let now = now_ms();
    let rand = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() as u64;
        ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
    };
    format!("dep-{now:016x}-{rand:04x}")
}

/// 生成 operation_id（时间戳 + 随机后缀）。
pub fn generate_operation_id() -> String {
    let now = now_ms();
    let rand = {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let c = COUNTER.fetch_add(1, Ordering::Relaxed);
        let pid = std::process::id() as u64;
        ((pid.rotate_left(8) ^ c.rotate_left(16) ^ now) & 0xFFFF) as u16
    };
    format!("op-{now:016x}-{rand:04x}")
}

// ── 测试 ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn install_id_valid() {
        assert!(validate_install_id("dep-1234567890abcdef-abcd").is_ok());
        assert!(validate_install_id("install001").is_ok());
    }

    #[test]
    fn install_id_rejects_path_traversal() {
        assert!(validate_install_id("../escape").is_err());
        assert!(validate_install_id("a/../b").is_err());
        assert!(validate_install_id("a/b").is_err());
        assert!(validate_install_id("a\\b").is_err());
    }

    #[test]
    fn install_id_rejects_uppercase() {
        assert!(validate_install_id("Gen-ABC").is_err());
    }

    #[test]
    fn operation_id_valid() {
        assert!(validate_operation_id("op-1234567890abcdef-abcd").is_ok());
    }

    #[test]
    fn operation_id_rejects_path_traversal() {
        assert!(validate_operation_id("../escape").is_err());
    }

    #[test]
    fn slot_name_only_accepts_closed_set() {
        assert!(validate_slot_name("slot-a").is_ok());
        assert!(validate_slot_name("slot-b").is_ok());
        assert!(validate_slot_name("slot-c").is_err());
        assert!(validate_slot_name("../escape").is_err());
        assert!(validate_slot_name("").is_err());
    }

    #[test]
    fn generate_install_id_is_valid() {
        let id = generate_install_id();
        assert!(validate_install_id(&id).is_ok(), "install_id 不合法: {id}");
    }

    #[test]
    fn generate_operation_id_is_valid() {
        let id = generate_operation_id();
        assert!(
            validate_operation_id(&id).is_ok(),
            "operation_id 不合法: {id}"
        );
    }

    #[test]
    fn generate_install_id_unique() {
        let mut ids = Vec::new();
        for _ in 0..10 {
            let id = generate_install_id();
            assert!(!ids.contains(&id), "install_id 重复: {id}");
            ids.push(id);
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
    }

    #[test]
    fn ensure_path_within_rejects_traversal() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        let valid = root.join("subdir").join("file.txt");
        std::fs::create_dir_all(root.join("subdir")).unwrap();
        std::fs::write(&valid, b"test").unwrap();
        assert!(ensure_path_within(&root, &valid).is_ok());

        let escape = root.join("..").join("..").join("etc").join("passwd");
        assert!(ensure_path_within(&root, &escape).is_err());
    }

    // ── path-component-aware 边界测试 ──────────────────────────────────

    #[test]
    fn ensure_path_within_rejects_sibling_prefix() {
        // 同名前缀兄弟目录：`engine` vs `engine-evil` 不应匹配
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let good_dir = root.join("engine");
        let evil_dir = root.join("engine-evil");
        std::fs::create_dir_all(&good_dir).unwrap();
        std::fs::create_dir_all(&evil_dir).unwrap();
        let good_file = good_dir.join("model.gguf");
        let evil_file = evil_dir.join("malware.gguf");
        std::fs::write(&good_file, b"x").unwrap();
        std::fs::write(&evil_file, b"x").unwrap();

        // good_dir 内的文件在 root 下正常
        assert!(ensure_path_within(&root, &good_file).is_ok());
        // evil_dir 的文件也在 root 下（root 包含两者）
        assert!(ensure_path_within(&root, &evil_file).is_ok());

        // 但以 good_dir 为根时，evil_dir 的文件应被拒绝
        assert!(ensure_path_within(&good_dir, &evil_file).is_err());
        // good_dir 内的文件应通过
        assert!(ensure_path_within(&good_dir, &good_file).is_ok());
    }

    #[test]
    fn ensure_path_within_rejects_double_dot_in_middle() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let subdir = root.join("models").join("funasr");
        std::fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("model.gguf");
        std::fs::write(&file, b"x").unwrap();

        // 从子目录用 .. 逃逸到 root 上级
        let escape = subdir.join("..").join("..").join("..").join("etc");
        assert!(ensure_path_within(&root, &escape).is_err());

        // 合法 .. 回到 root 内
        let valid = subdir.join("..").join("funasr").join("model.gguf");
        assert!(ensure_path_within(&root, &valid).is_ok());
    }

    #[test]
    fn ensure_path_within_rejects_absolute_path_outside_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();

        // 绝对路径在 root 外——如果文件存在则应被拒绝
        let outside = std::env::temp_dir().join("blink-path-test-outside.txt");
        std::fs::write(&outside, b"x").unwrap();
        assert!(ensure_path_within(&root, &outside).is_err());
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn ensure_path_within_accepts_nested_subdirs() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let deep = root.join("a").join("b").join("c").join("d");
        std::fs::create_dir_all(&deep).unwrap();
        let file = deep.join("file.bin");
        std::fs::write(&file, b"x").unwrap();
        assert!(ensure_path_within(&root, &file).is_ok());
    }

    #[test]
    fn ensure_path_within_handles_trailing_separator() {
        // 带尾分隔符的根目录——Path::starts_with 按组件匹配应正确处理
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let subdir = root.join("slots");
        std::fs::create_dir_all(&subdir).unwrap();
        let file = subdir.join("manifest.json");
        std::fs::write(&file, b"{}").unwrap();

        // 根目录带尾分隔符（Windows: 反斜杠）
        let root_with_sep = {
            let mut s = root.to_string_lossy().to_string();
            if !s.ends_with(std::path::MAIN_SEPARATOR) {
                s.push(std::path::MAIN_SEPARATOR);
            }
            std::path::PathBuf::from(s)
        };
        // canonicalize 会去掉尾分隔符，所以带尾分隔符的路径在 canonicalize 后
        // 与不带尾分隔符的路径相同
        assert!(ensure_path_within(&root, &file).is_ok());
        // 直接用 root_with_sep 也能通过（canonicalize 处理了尾分隔符）
        if root_with_sep.exists() {
            let canon_sep = root_with_sep.canonicalize().unwrap();
            assert!(file.canonicalize().unwrap().starts_with(&canon_sep));
        }
    }

    #[test]
    fn ensure_path_within_rejects_symlink_escape() {
        // 符号链接逃逸测试：在 root 内创建指向 root 外的 symlink
        // canonicalize 会解析 symlink，指向 root 外的路径应被拒绝
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        let outside = std::env::temp_dir().join("blink-symlink-test-outside.txt");
        std::fs::write(&outside, b"x").unwrap();

        let link_path = root.join("escape_link.txt");
        #[cfg(windows)]
        {
            // Windows 上 symlink 需要管理员权限，用 junction 测试如果可用
            // 如果 symlink 创建失败（非管理员），跳过此测试
            if std::os::windows::fs::symlink_file(&outside, &link_path).is_err() {
                eprintln!("跳过 symlink 测试（Windows 需要管理员权限创建 symlink）");
                let _ = std::fs::remove_file(&outside);
                return;
            }
        }
        #[cfg(not(windows))]
        {
            std::os::unix::fs::symlink(&outside, &link_path).unwrap();
        }

        // canonicalize 会解析 symlink 到外部路径，应被拒绝
        assert!(ensure_path_within(&root, &link_path).is_err());
        let _ = std::fs::remove_file(&link_path);
        let _ = std::fs::remove_file(&outside);
    }

    #[test]
    fn python_manifest_roundtrip() {
        let manifest = DeploymentManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            engine_id: EngineId::new("funasr").unwrap(),
            runtime_kind: RuntimePlan::PythonVenv,
            install_id: "dep-test0001".to_string(),
            requested_preference: ComputePreference::Cpu,
            resolved_profile: ResolvedProfile {
                profile_id: "cpu-x64".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                priority: 0,
            },
            installed_at_ms: 1700000000000,
            artifact: ArtifactIdentity {
                runtime_kind: RuntimePlan::PythonVenv,
                artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                sha256: "abc123".to_string(),
            },
            model_contract: ModelContract {
                model_id: "funasr-model".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: ChecksumSource::Unverified,
            },
            fallback_reasons: Vec::new(),
            extension: ManifestExtension::PythonVenv(PythonManifestExt {
                python_version: "3.12.8".to_string(),
                python_artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                packages: vec![PackageStatus {
                    name: "torch".to_string(),
                    installed_version: Some("2.5.0".to_string()),
                    locked_version: "2.5.0".to_string(),
                    satisfies_lock: true,
                }],
                uv_version: "0.6.10".to_string(),
                index_url: None,
                self_test_passed: true,
            }),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let back: DeploymentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.schema_version, MANIFEST_SCHEMA_VERSION);
        assert_eq!(back.engine_id, manifest.engine_id);
        assert_eq!(back.runtime_kind, manifest.runtime_kind);
        assert_eq!(back.install_id, manifest.install_id);

        match back.extension {
            ManifestExtension::PythonVenv(ext) => {
                assert_eq!(ext.python_version, "3.12.8");
                assert_eq!(ext.packages.len(), 1);
                assert!(ext.self_test_passed);
            }
            ManifestExtension::ManagedBinary(_) => panic!("应为 PythonVenv"),
            ManifestExtension::OnnxRuntime(_) => panic!("应为 PythonVenv"),
        }
    }

    #[test]
    fn binary_manifest_roundtrip() {
        let manifest = DeploymentManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            engine_id: EngineId::new("funasr-gguf").unwrap(),
            runtime_kind: RuntimePlan::ManagedBinary,
            install_id: "dep-bin0001".to_string(),
            requested_preference: ComputePreference::Cpu,
            resolved_profile: ResolvedProfile {
                profile_id: "cpu-avx2".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                priority: 0,
            },
            installed_at_ms: 1700000000000,
            artifact: ArtifactIdentity {
                runtime_kind: RuntimePlan::ManagedBinary,
                artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                sha256: "def456".to_string(),
            },
            model_contract: ModelContract {
                model_id: "sensevoice-q8".to_string(),
                revision: "v1.0".to_string(),
                checksum_source: ChecksumSource::Sha256("abc789".to_string()),
            },
            fallback_reasons: Vec::new(),
            extension: ManifestExtension::ManagedBinary(BinaryManifestExt {
                archive_artifact_id: ArtifactId::new("llama-funasr-v0.2.0").unwrap(),
                archive_sha256: "def456".to_string(),
                executable: "llama-funasr-server.exe".to_string(),
                files: vec![FileEntry {
                    path: "llama-funasr-server.exe".to_string(),
                    sha256: "aaa111".to_string(),
                    size: 50000000,
                    is_dll: false,
                }],
                stdlib_artifact: Some(ArtifactIdentity {
                    runtime_kind: RuntimePlan::PythonVenv,
                    artifact_id: ArtifactId::new("python-3.12.8").unwrap(),
                    sha256: "abc123".to_string(),
                }),
                required_cpu_features: vec!["avx2".to_string()],
                required_drivers: Vec::new(),
                self_test_passed: true,
            }),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let back: DeploymentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime_kind, RuntimePlan::ManagedBinary);

        match back.extension {
            ManifestExtension::ManagedBinary(ext) => {
                assert_eq!(ext.executable, "llama-funasr-server.exe");
                assert!(ext.self_test_passed);
                assert!(ext.stdlib_artifact.is_some());
                assert_eq!(ext.required_cpu_features, vec!["avx2"]);
            }
            ManifestExtension::PythonVenv(_) => panic!("应为 ManagedBinary"),
            ManifestExtension::OnnxRuntime(_) => panic!("应为 ManagedBinary"),
        }
    }

    #[test]
    fn onnx_runtime_manifest_roundtrip() {
        let manifest = DeploymentManifest {
            schema_version: MANIFEST_SCHEMA_VERSION,
            engine_id: EngineId::new("paddleocr").unwrap(),
            runtime_kind: RuntimePlan::OnnxRuntime,
            install_id: "dep-onnx0001".to_string(),
            requested_preference: ComputePreference::Cpu,
            resolved_profile: ResolvedProfile {
                profile_id: "cpu-x64".to_string(),
                backend: ComputeBackend::Cpu,
                artifact_id: ArtifactId::new("ort-1.20.0").unwrap(),
                priority: 0,
            },
            installed_at_ms: 1700000000000,
            artifact: ArtifactIdentity {
                runtime_kind: RuntimePlan::OnnxRuntime,
                artifact_id: ArtifactId::new("ort-1.20.0").unwrap(),
                sha256: "abc123".to_string(),
            },
            model_contract: ModelContract {
                model_id: "PP-OCRv6".to_string(),
                revision: "ppocrv6-tiny".to_string(),
                checksum_source: ChecksumSource::Unverified,
            },
            fallback_reasons: Vec::new(),
            extension: ManifestExtension::OnnxRuntime(OnnxRuntimeManifestExt {
                dll_artifact_id: ArtifactId::new("ort-1.20.0").unwrap(),
                dll_sha256: "abc123".to_string(),
                ort_version: "1.20.0".to_string(),
                dll_files: vec![FileEntry {
                    path: "onnxruntime.dll".to_string(),
                    sha256: "def456".to_string(),
                    size: 20000000,
                    is_dll: true,
                }],
                model_generation_id: "gen-ppocrv6-v1".to_string(),
                execution_provider: "cpu".to_string(),
                inter_op: 1,
                intra_op: 4,
                self_test_passed: true,
            }),
        };

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let back: DeploymentManifest = serde_json::from_str(&json).unwrap();
        assert_eq!(back.runtime_kind, RuntimePlan::OnnxRuntime);

        match back.extension {
            ManifestExtension::OnnxRuntime(ext) => {
                assert_eq!(ext.ort_version, "1.20.0");
                assert_eq!(ext.execution_provider, "cpu");
                assert_eq!(ext.inter_op, 1);
                assert_eq!(ext.intra_op, 4);
                assert!(ext.self_test_passed);
                assert_eq!(ext.model_generation_id, "gen-ppocrv6-v1");
                assert_eq!(ext.dll_files.len(), 1);
                assert!(ext.dll_files[0].is_dll);
            }
            ManifestExtension::PythonVenv(_) => panic!("应为 OnnxRuntime"),
            ManifestExtension::ManagedBinary(_) => panic!("应为 OnnxRuntime"),
        }
    }

    #[test]
    fn legacy_python_venv_manifest_still_deserializes() {
        // 旧 PythonVenv manifest 必须能继续反序列化——升级用户的 deployment 不丢失。
        // JSON 是手写的，模拟 0.22.7 及之前版本写入的 manifest 结构。
        let legacy_json = r#"{
            "schema_version": 2,
            "engine_id": "paddleocr",
            "runtime_kind": "python_venv",
            "install_id": "dep-legacy001",
            "requested_preference": "cpu",
            "resolved_profile": {
                "profile_id": "cpu-x64",
                "backend": "cpu",
                "artifact_id": "python-3.12.8",
                "priority": 0
            },
            "installed_at_ms": 1700000000000,
            "artifact": {
                "runtime_kind": "python_venv",
                "artifact_id": "python-3.12.8",
                "sha256": "abc123"
            },
            "model_contract": {
                "model_id": "PP-OCRv6",
                "revision": "ppocrv6-tiny",
                "checksum_source": "Unverified"
            },
            "fallback_reasons": [],
            "extension": {
                "type": "python_venv",
                "python_version": "3.12.8",
                "python_artifact_id": "python-3.12.8",
                "packages": [],
                "uv_version": "0.6.10",
                "index_url": null,
                "self_test_passed": true
            }
        }"#;
        let manifest: DeploymentManifest = serde_json::from_str(legacy_json).unwrap();
        assert_eq!(manifest.runtime_kind, RuntimePlan::PythonVenv);
        assert_eq!(manifest.engine_id.as_str(), "paddleocr");
        match manifest.extension {
            ManifestExtension::PythonVenv(ext) => {
                assert_eq!(ext.python_version, "3.12.8");
                assert!(ext.self_test_passed);
            }
            _ => panic!("应为 PythonVenv（legacy）"),
        }
    }

    #[test]
    fn atomic_write_replaces_existing() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("deployment.json");

        let p1 = serde_json::json!({"install_id": "dep-v1", "slot": "slot-a"});
        atomic_write_json(&path, &p1).unwrap();

        let p2 = serde_json::json!({"install_id": "dep-v2", "slot": "slot-b"});
        atomic_write_json(&path, &p2).unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        let back: serde_json::Value = serde_json::from_str(&content).unwrap();
        assert_eq!(back["install_id"], "dep-v2");
    }

    #[test]
    fn atomic_replace_failure_preserves_existing_target() {
        let tmp = tempfile::tempdir().unwrap();
        let missing_source = tmp.path().join("missing.tmp");
        let target = tmp.path().join("deployment.json");
        std::fs::write(&target, b"old-pointer").unwrap();

        assert!(atomic_replace(&missing_source, &target).is_err());
        assert_eq!(std::fs::read(&target).unwrap(), b"old-pointer");
    }
}
