//! 模型资产存储协议（0.22.6 H3-model）。
//!
//! 独立于引擎 runtime generation 体系——模型资产有自己的
//! `models/{engine_id}/{asset_key}/generations/{install_id}/payload/` 结构，
//! 与引擎 venv generation 正交。
//!
//! ## 设计铁则
//!
//! - **manifest 是唯一真源**：`Installed` 状态只能从有效 manifest + current pointer
//!   + payload + fingerprint 全部一致恢复。禁止仅凭目录非空推断 Installed。
//! - **asset_key 安全编码**：不直接把 `iic/SenseVoiceSmall` 当路径拼接，
//!   而是做确定性安全编码（只允许 `[a-z0-9-]`，其他字符替换为 `-`）。
//! - **generation 隔离**：每次安装/修复创建新 generation，校验通过后
//!   原子切换 `current.json`；失败时旧 generation 不受影响。
//! - **content fingerprint 确定性**：Rust 和 Python 使用完全相同的
//!   目录聚合 SHA-256 算法，确保跨语言一致性。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::runtime::{
    EngineId, RuntimeError, atomic_write_json, now_ms, validate_install_id, validate_operation_id,
};

// ── 常量 ───────────────────────────────────────────────────────────────────

/// 模型 manifest schema 版本。
pub const MODEL_MANIFEST_SCHEMA_VERSION: u32 = 1;

/// 模型 current.json schema 版本。
pub const MODEL_CURRENT_POINTER_SCHEMA_VERSION: u32 = 1;

/// fingerprint 算法标识。
pub const CONTENT_FINGERPRINT_ALGORITHM: &str = "directory_aggregate_sha256_v1";

// ── asset_key 安全编码 ─────────────────────────────────────────────────────

/// 将 model_id 确定性安全编码为 asset_key。
///
/// 规则：
/// - 只允许 `[a-z0-9-]`
/// - 大写转小写
/// - 其他字符（`/`、`_`、`.` 等）替换为 `-`
/// - 连续 `-` 压缩为单个 `-`
/// - 去除首尾 `-`
/// - 非空保证（若结果为空，使用 `model` 兜底）
///
/// 例如：`iic/SenseVoiceSmall` → `iic-sensevoicesmall`
pub fn encode_asset_key(model_id: &str) -> String {
    let mut key: String = model_id
        .chars()
        .map(|c| {
            if c.is_ascii_lowercase() || c.is_ascii_digit() {
                c
            } else if c.is_ascii_uppercase() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect();

    // 压缩连续 `-`
    while key.contains("--") {
        key = key.replace("--", "-");
    }

    // 去除首尾 `-`
    let trimmed = key.trim_matches('-');

    if trimmed.is_empty() {
        "model".to_string()
    } else {
        trimmed.to_string()
    }
}

/// 校验 asset_key（只允许 `[a-z0-9-]`，防路径逃逸）。
pub fn validate_asset_key(key: &str) -> Result<(), RuntimeError> {
    if key.is_empty() || key.len() > 128 {
        return Err(RuntimeError::PathTraversal {
            path: key.to_string(),
        });
    }
    if !key
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(RuntimeError::PathTraversal {
            path: key.to_string(),
        });
    }
    if key.starts_with('-') || key.ends_with('-') || key.contains("--") {
        return Err(RuntimeError::PathTraversal {
            path: key.to_string(),
        });
    }
    Ok(())
}

// ── 模型存储路径 API ───────────────────────────────────────────────────────

/// 模型根目录：`models/`（测试时在 runtimes_root/models 下）
pub fn models_root() -> PathBuf {
    #[cfg(test)]
    {
        return super::runtime::runtimes_root().join("models");
    }
    #[cfg(not(test))]
    crate::infra::utils::paths::app_data_dir().join("models")
}

/// 引擎模型根目录：`models/{engine_id}`
pub fn engine_model_root(engine_id: &EngineId) -> PathBuf {
    models_root().join(engine_id.as_str())
}

/// asset 根目录：`models/{engine_id}/{asset_key}`
pub fn asset_root(engine_id: &EngineId, asset_key: &str) -> Result<PathBuf, RuntimeError> {
    validate_asset_key(asset_key)?;
    Ok(engine_model_root(engine_id).join(asset_key))
}

/// current.json 路径：`models/{engine_id}/{asset_key}/current.json`
pub fn model_current_pointer_path(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("current.json"))
}

/// generations 目录：`models/{engine_id}/{asset_key}/generations/`
pub fn model_generations_dir(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("generations"))
}

/// 单个 generation 目录：`models/{engine_id}/{asset_key}/generations/{install_id}/`
pub fn model_generation_dir(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
) -> Result<PathBuf, RuntimeError> {
    validate_install_id(install_id)?;
    Ok(model_generations_dir(engine_id, asset_key)?.join(install_id))
}

/// manifest 路径：`models/{engine_id}/{asset_key}/generations/{install_id}/manifest.json`
pub fn model_manifest_path(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(model_generation_dir(engine_id, asset_key, install_id)?.join("manifest.json"))
}

/// payload 目录：`models/{engine_id}/{asset_key}/generations/{install_id}/payload/`
pub fn model_payload_dir(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(model_generation_dir(engine_id, asset_key, install_id)?.join("payload"))
}

/// staging 目录：`models/{engine_id}/{asset_key}/staging/`
pub fn model_staging_dir(engine_id: &EngineId, asset_key: &str) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("staging"))
}

/// 清理同 asset 下所有孤儿 staging 残留（安装开始前调用）。
///
/// 上一轮安装若被强杀（应用退出/崩溃），staging 目录会残留且无人回收——
/// GB 级模型残留会白白占用磁盘。调用时机已保证该模型无活跃操作
/// （`install_state.is_busy()` 检查在前），同 asset 的 staging 内
/// 不可能有正在进行的写入。
pub fn cleanup_orphan_staging(engine_id: &EngineId, asset_key: &str) -> usize {
    let staging_root = match model_staging_dir(engine_id, asset_key) {
        Ok(p) => p,
        Err(_) => return 0,
    };
    let entries = match std::fs::read_dir(&staging_root) {
        Ok(e) => e,
        Err(_) => return 0,
    };

    let mut cleaned = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() && std::fs::remove_dir_all(&path).is_ok() {
            tracing::info!(
                engine_id = %engine_id,
                asset_key = asset_key,
                path = %path.display(),
                "清理孤儿 staging 残留"
            );
            cleaned += 1;
        }
    }
    cleaned
}

/// 单次 operation 的 staging 目录：`models/{engine_id}/{asset_key}/staging/{operation_id}/`
pub fn model_operation_staging_dir(
    engine_id: &EngineId,
    asset_key: &str,
    operation_id: &str,
) -> Result<PathBuf, RuntimeError> {
    validate_operation_id(operation_id)?;
    Ok(model_staging_dir(engine_id, asset_key)?.join(operation_id))
}

/// 单次 operation 的 staging payload 目录：
/// `models/{engine_id}/{asset_key}/staging/{operation_id}/payload/`
pub fn model_operation_staging_payload_dir(
    engine_id: &EngineId,
    asset_key: &str,
    operation_id: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(model_operation_staging_dir(engine_id, asset_key, operation_id)?.join("payload"))
}

// ── ModelManifest ───────────────────────────────────────────────────────────

/// 模型 generation 的不可变 manifest。
///
/// manifest 是模型 Installed 状态的唯一真源。
/// 目录存在或非空不足以证明 Installed——必须 manifest + current pointer +
/// payload + fingerprint 全部一致。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelManifest {
    /// Manifest schema 版本。
    pub schema_version: u32,
    /// 引擎 id（canonical，不受目录名影响）。
    pub engine_id: EngineId,
    /// 模型 id（canonical，如 `iic/SenseVoiceSmall`）。
    pub model_id: String,
    /// 模型 revision。
    pub revision: String,
    /// 下载来源/溯源信息。
    pub source: ModelSource,
    /// 安装 id（generation 目录名）。
    pub install_id: String,
    /// 安装时间（Unix 毫秒）。
    pub installed_at_ms: u64,
    /// content fingerprint 算法标识。
    pub content_fingerprint_algorithm: String,
    /// content fingerprint（小写 64 位 hex SHA-256）。
    pub content_fingerprint: String,
    /// payload 总大小（字节）。
    pub payload_size_bytes: u64,
    /// payload 文件数。
    pub file_count: u64,
    /// 兼容性 schema 版本（来自 descriptor）。
    pub compatibility_schema: u32,
    /// 安装时记录的 model contract identity（用于运行时身份比对）。
    pub model_contract_identity: ModelContractIdentity,
}

/// 模型下载来源/溯源。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ModelSource {
    /// 上游不提供稳定 checksum，已记录下载来源但无法字节级校验。
    Unverified {
        /// 下载来源描述（如 "modelscope:iic/SenseVoiceSmall"）。
        source: String,
        /// 下载时间（Unix 毫秒）。
        downloaded_at_ms: u64,
    },
    /// 上游提供稳定 SHA-256。
    Sha256 {
        /// 上游 SHA-256。
        sha256: String,
        /// 下载来源。
        source: String,
        /// 下载时间（Unix 毫秒）。
        downloaded_at_ms: u64,
    },
}

/// 安装时记录的 model contract identity（用于运行时身份比对）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelContractIdentity {
    /// 模型 id。
    pub model_id: String,
    /// 模型 revision。
    pub revision: String,
    /// checksum 来源标识（与 descriptor 的 checksum_source 对应）。
    pub checksum_source_kind: String,
}

// ── ModelCurrentPointer ─────────────────────────────────────────────────────

/// `current.json` 指针文件内容。
///
/// 采用同目录临时文件 + replace/rename 原子写入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelCurrentPointer {
    /// 当前 generation 的 install id。
    pub install_id: String,
    /// 更新时间（Unix 毫秒）。
    pub updated_at_ms: u64,
    /// schema 版本。
    pub schema_version: u32,
}

// ── current.json 读写 ──────────────────────────────────────────────────────

/// 读取 current.json。
///
/// 如果文件不存在返回 `Ok(None)`（模型未安装）。
pub fn read_model_current_pointer(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<Option<ModelCurrentPointer>, RuntimeError> {
    let path = model_current_pointer_path(engine_id, asset_key)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let pointer: ModelCurrentPointer =
        serde_json::from_str(&content).map_err(|e| RuntimeError::CurrentPointerParseFailed {
            message: format!("{e}"),
        })?;
    Ok(Some(pointer))
}

/// 原子写入 current.json。
pub fn write_model_current_pointer(
    engine_id: &EngineId,
    asset_key: &str,
    pointer: &ModelCurrentPointer,
) -> Result<(), RuntimeError> {
    let path = model_current_pointer_path(engine_id, asset_key)?;
    atomic_write_json(&path, pointer)
}

// ── manifest 读写 ───────────────────────────────────────────────────────────

/// 读取 generation manifest。
pub fn read_model_manifest(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
) -> Result<ModelManifest, RuntimeError> {
    let path = model_manifest_path(engine_id, asset_key, install_id)?;
    if !path.exists() {
        return Err(RuntimeError::GenerationNotFound {
            install_id: install_id.to_string(),
        });
    }
    let content = std::fs::read_to_string(&path)?;
    let manifest: ModelManifest =
        serde_json::from_str(&content).map_err(|e| RuntimeError::ManifestParseFailed {
            message: format!("{e}"),
        })?;
    if manifest.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
        return Err(RuntimeError::ManifestSchemaIncompatible {
            expected: MODEL_MANIFEST_SCHEMA_VERSION,
            actual: manifest.schema_version,
        });
    }
    Ok(manifest)
}

/// 写入 generation manifest（在 generation 目录内）。
pub fn write_model_manifest(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
    manifest: &ModelManifest,
) -> Result<(), RuntimeError> {
    let dir = model_generation_dir(engine_id, asset_key, install_id)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("manifest.json");
    atomic_write_json(&path, manifest)
}

// ── 模型状态恢复 ────────────────────────────────────────────────────────────

/// 从磁盘恢复的模型状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoredModelState {
    /// 已安装：current pointer + manifest + payload 目录结构链有效。
    /// （不含内容 hash——完整校验走 [`verify_model_payload`]。）
    Installed {
        install_id: String,
        manifest: ModelManifest,
    },
    /// 损坏：pointer/manifest/payload 目录结构链任一损坏或不一致。
    Corrupted {
        install_id: Option<String>,
        reason: String,
    },
    /// 未安装：没有有效 current generation。
    NotInstalled,
}

/// 从磁盘恢复模型状态（结构校验，**不做全量 hash**）。
///
/// **铁则**：
/// - 禁止仅凭目录存在或非空推断 Installed。
/// - **启动时禁止对 GB 级模型做完整内容 hash**——restore 只验证
///   current pointer → manifest → payload 目录存在的结构链；
///   完整 fingerprint 校验走 `verify_model_payload`（安装事务在
///   staging 上直接 hash；显式验证/修复预检按需调用）。
pub fn restore_model_state(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<RestoredModelState, RuntimeError> {
    let pointer_path = model_current_pointer_path(engine_id, asset_key)?;
    if !pointer_path.exists() {
        return Ok(RestoredModelState::NotInstalled);
    }

    let pointer_content = match std::fs::read_to_string(&pointer_path) {
        Ok(c) => c,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                install_id: None,
                reason: format!("读取 current.json 失败: {e}"),
            });
        }
    };

    let pointer: ModelCurrentPointer = match serde_json::from_str(&pointer_content) {
        Ok(p) => p,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                install_id: None,
                reason: format!("解析 current.json 失败: {e}"),
            });
        }
    };

    if pointer.schema_version != MODEL_CURRENT_POINTER_SCHEMA_VERSION {
        return Ok(RestoredModelState::Corrupted {
            install_id: Some(pointer.install_id.clone()),
            reason: format!(
                "current.json schema 版本不兼容: expected={}, actual={}",
                MODEL_CURRENT_POINTER_SCHEMA_VERSION, pointer.schema_version
            ),
        });
    }

    let manifest = match read_model_manifest(engine_id, asset_key, &pointer.install_id) {
        Ok(m) => m,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                install_id: Some(pointer.install_id.clone()),
                reason: format!("manifest 读取失败: {e}"),
            });
        }
    };

    // 验证 manifest schema
    if manifest.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
        return Ok(RestoredModelState::Corrupted {
            install_id: Some(pointer.install_id.clone()),
            reason: format!(
                "manifest schema 版本不兼容: expected={}, actual={}",
                MODEL_MANIFEST_SCHEMA_VERSION, manifest.schema_version
            ),
        });
    }

    // 验证 manifest identity
    if manifest.engine_id != *engine_id {
        return Ok(RestoredModelState::Corrupted {
            install_id: Some(pointer.install_id.clone()),
            reason: format!(
                "manifest engine_id 不匹配: expected={}, actual={}",
                engine_id, manifest.engine_id
            ),
        });
    }

    // 验证 payload 目录存在（结构校验——不读内容、不 hash）
    let payload_dir = match model_payload_dir(engine_id, asset_key, &pointer.install_id) {
        Ok(p) => p,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                install_id: Some(pointer.install_id.clone()),
                reason: format!("payload 路径计算失败: {e}"),
            });
        }
    };
    if !payload_dir.exists() {
        return Ok(RestoredModelState::Corrupted {
            install_id: Some(pointer.install_id.clone()),
            reason: "payload 目录不存在".to_string(),
        });
    }

    Ok(RestoredModelState::Installed {
        install_id: pointer.install_id,
        manifest,
    })
}

/// 显式完整校验模型 payload（fingerprint + 文件数）。
///
/// 供用户显式验证/修复预检等按需路径调用（安装事务在 staging 上
/// 直接用 `compute_content_fingerprint`）——**调用方必须通过
/// `spawn_blocking` 挪出 async executor**（GB 级目录遍历 + hash）。
/// 返回 Err 描述不匹配原因（供投影为 Corrupted 状态）。
/// 当前生产调用方尚未接入，由本模块测试行使。
#[allow(dead_code)]
pub fn verify_model_payload(
    engine_id: &EngineId,
    asset_key: &str,
    manifest: &ModelManifest,
) -> Result<(), String> {
    let payload_dir = match model_payload_dir(engine_id, asset_key, &manifest.install_id) {
        Ok(p) => p,
        Err(e) => return Err(format!("payload 路径计算失败: {e}")),
    };
    if !payload_dir.exists() {
        return Err("payload 目录不存在".to_string());
    }

    let computed_fp = match compute_content_fingerprint(&payload_dir) {
        Ok(fp) => fp,
        Err(e) => return Err(format!("fingerprint 计算失败: {e}")),
    };

    if computed_fp.fingerprint != manifest.content_fingerprint {
        return Err(format!(
            "fingerprint 不匹配: manifest={}, actual={}",
            manifest.content_fingerprint, computed_fp.fingerprint
        ));
    }

    if computed_fp.file_count != manifest.file_count {
        return Err(format!(
            "file_count 不匹配: manifest={}, actual={}",
            manifest.file_count, computed_fp.file_count
        ));
    }

    Ok(())
}

// ── content fingerprint 算法 ───────────────────────────────────────────────

/// content fingerprint 计算结果。
#[derive(Debug, Clone)]
pub struct ContentFingerprint {
    /// 小写 64 位 hex SHA-256。
    pub fingerprint: String,
    /// payload 总大小（字节）。
    pub total_size_bytes: u64,
    /// payload 文件数。
    pub file_count: u64,
}

/// 计算目录的 content fingerprint（确定性目录聚合 SHA-256）。
///
/// ## 算法（Rust/Python 共享规范）
///
/// 1. 递归枚举 payload 下的普通文件。
/// 2. 使用相对于 payload 根目录的规范化 `/` 路径。
/// 3. 按 UTF-8 相对路径字节排序。
/// 4. 对每个文件依次哈希：
///    - 相对路径长度（u64 LE）与相对路径字节；
///    - 文件大小（u64 LE）；
///    - 文件内容。
/// 5. 排除 Blink 自己的 manifest、current pointer、临时文件、下载锁和 staging 元数据。
/// 6. 最终输出小写 64 位 hex SHA-256。
pub fn compute_content_fingerprint(payload_dir: &Path) -> Result<ContentFingerprint, RuntimeError> {
    let mut hasher = Sha256::new();
    let mut files: Vec<(String, PathBuf)> = Vec::new();

    // 递归收集所有普通文件
    collect_files(payload_dir, payload_dir, &mut files)?;

    // 按相对路径字节排序
    files.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));

    let mut total_size: u64 = 0;
    let mut file_count: u64 = 0;

    // 流式分块读取（1MB buf）——模型 payload 可达 GB 级，
    // 全量 read 进内存会造成数倍峰值内存。SHA-256 分块 update 与
    // 全量 update 结果逐字节一致（Python 侧算法不受影响）。
    let mut buf = vec![0u8; 1024 * 1024];

    for (rel_path, abs_path) in &files {
        // 哈希相对路径长度 + 相对路径
        let rel_bytes = rel_path.as_bytes();
        let rel_len = rel_bytes.len() as u64;
        hasher.update(rel_len.to_le_bytes());
        hasher.update(rel_bytes);

        use std::io::Read;
        let size = std::fs::metadata(abs_path)?.len();
        hasher.update(size.to_le_bytes());

        let mut file = std::fs::File::open(abs_path)?;
        loop {
            let n = file.read(&mut buf)?;
            if n == 0 {
                break;
            }
            hasher.update(&buf[..n]);
        }

        total_size += size;
        file_count += 1;

        // 大文件进度日志——GB 级模型校验是长操作，需要可观测性
        if size > 100 * 1024 * 1024 {
            tracing::debug!(
                file = %abs_path.display(),
                size_bytes = size,
                "fingerprint: 大文件已哈希"
            );
        }
    }

    let result = hasher.finalize();
    let fingerprint = format!("{:x}", result);

    Ok(ContentFingerprint {
        fingerprint,
        total_size_bytes: total_size,
        file_count,
    })
}

/// 递归收集文件，排除 Blink 自己的元数据文件。
fn collect_files(
    root: &Path,
    current: &Path,
    files: &mut Vec<(String, PathBuf)>,
) -> Result<(), RuntimeError> {
    if !current.is_dir() {
        return Ok(());
    }

    let entries = std::fs::read_dir(current)?;

    for entry in entries {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 排除 Blink 元数据文件
        if name_str == "manifest.json" || name_str == "current.json" {
            continue;
        }
        // 排除临时文件（以 .tmp_ 开头）
        if name_str.starts_with(".tmp_") {
            continue;
        }
        // 排除下载锁
        if name_str == ".download_lock" {
            continue;
        }

        if path.is_dir() {
            collect_files(root, &path, files)?;
        } else if path.is_file() {
            // 计算相对路径，使用 `/` 分隔符
            let rel = path
                .strip_prefix(root)
                .map_err(|e| RuntimeError::PathTraversal {
                    path: format!("{e}"),
                })?;
            let rel_str: String = rel
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push((rel_str, path));
        }
    }

    Ok(())
}

/// 扫描目录占用大小（用于诊断/清理）。
pub fn scan_dir_size(path: &Path) -> u64 {
    let mut size = 0;
    if path.is_dir() {
        if let Ok(entries) = std::fs::read_dir(path) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir() {
                    size += scan_dir_size(&p);
                } else if p.is_file() {
                    size += std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                }
            }
        }
    } else if path.is_file() {
        size += std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    }
    size
}

// ── 原子提升 ───────────────────────────────────────────────────────────────

/// 将 staging payload 原子提升为正式 generation。
///
/// 步骤：
/// 1. 创建 generation 目录
/// 2. 计算指纹
/// 3. 写入 manifest
/// 4. 原子切换 current.json
/// 5. 成功后 staging 可清理
pub fn promote_staging_to_generation(
    engine_id: &EngineId,
    asset_key: &str,
    install_id: &str,
    operation_id: &str,
    manifest: &ModelManifest,
) -> Result<(), RuntimeError> {
    let staging_payload = model_operation_staging_payload_dir(engine_id, asset_key, operation_id)?;
    let generation_dir = model_generation_dir(engine_id, asset_key, install_id)?;
    let target_payload = generation_dir.join("payload");

    // 确保目标不存在（如果是 repair，旧 generation 应已清理或使用新 install_id）
    if generation_dir.exists() {
        return Err(RuntimeError::GenerationPromoteFailed {
            message: format!("generation 目录已存在: {}", generation_dir.display()),
        });
    }

    // 创建 generation 目录
    std::fs::create_dir_all(&generation_dir)?;

    // 移动 payload（同卷 rename 是原子的）
    if staging_payload.exists() {
        std::fs::rename(&staging_payload, &target_payload).map_err(|e| {
            let _ = std::fs::remove_dir_all(&generation_dir);
            RuntimeError::GenerationPromoteFailed {
                message: format!("payload 移动失败: {e}"),
            }
        })?;
    } else {
        // payload 不存在（空模型？）——创建空目录
        std::fs::create_dir_all(&target_payload)?;
    }

    // 写入 manifest
    if let Err(e) = write_model_manifest(engine_id, asset_key, install_id, manifest) {
        let _ = std::fs::remove_dir_all(&generation_dir);
        return Err(e);
    }

    // 原子切换 current.json
    let pointer = ModelCurrentPointer {
        install_id: install_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_CURRENT_POINTER_SCHEMA_VERSION,
    };

    if let Err(e) = write_model_current_pointer(engine_id, asset_key, &pointer) {
        let _ = std::fs::remove_dir_all(&generation_dir);
        return Err(e);
    }

    Ok(())
}

/// 清理 staging 目录（operation 完成或取消后调用）。
pub fn cleanup_staging(
    engine_id: &EngineId,
    asset_key: &str,
    operation_id: &str,
) -> Result<(), RuntimeError> {
    let staging = model_operation_staging_dir(engine_id, asset_key, operation_id)?;
    if staging.exists() {
        std::fs::remove_dir_all(&staging)?;
    }
    Ok(())
}

/// 删除 generation（current pointer + generation 目录）。
///
/// 用于 delete_model：先删除 generation 目录，再删除 current.json。
/// 如果 generation 目录删除失败，返回错误（不谎报 NotInstalled）。
pub fn delete_model_generation(engine_id: &EngineId, asset_key: &str) -> Result<(), RuntimeError> {
    // 先读取 current pointer 获取 install_id
    let pointer = read_model_current_pointer(engine_id, asset_key)?;
    if pointer.is_none() {
        return Err(RuntimeError::GenerationNotFound {
            install_id: "no current pointer".to_string(),
        });
    }
    let pointer = pointer.unwrap();

    // 删除 generation 目录
    let gen_dir = model_generation_dir(engine_id, asset_key, &pointer.install_id)?;
    if gen_dir.exists() {
        std::fs::remove_dir_all(&gen_dir).map_err(|e| RuntimeError::CleanupFailed {
            message: format!("删除 generation 目录失败: {e}"),
        })?;
    }

    // 删除 current.json
    let pointer_path = model_current_pointer_path(engine_id, asset_key)?;
    if pointer_path.exists() {
        std::fs::remove_file(&pointer_path).map_err(|e| RuntimeError::CleanupFailed {
            message: format!("删除 current.json 失败: {e}"),
        })?;
    }

    Ok(())
}

/// 清理旧 generations（保留 current）。
///
/// 用于安装成功后清理旧 generation（deferred cleanup）。
pub fn cleanup_old_generations(
    engine_id: &EngineId,
    asset_key: &str,
    current_install_id: &str,
) -> Result<Vec<String>, RuntimeError> {
    let gens_dir = model_generations_dir(engine_id, asset_key)?;
    if !gens_dir.exists() {
        return Ok(Vec::new());
    }

    let mut cleaned = Vec::new();
    let entries = std::fs::read_dir(&gens_dir)?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 跳过 current
        if name_str == current_install_id {
            continue;
        }

        // 验证 install_id 格式
        if validate_install_id(&name_str).is_err() {
            continue;
        }

        let path = entry.path();
        if path.is_dir() {
            if let Err(e) = std::fs::remove_dir_all(&path) {
                tracing::warn!(
                    engine_id = %engine_id,
                    asset_key = %asset_key,
                    install_id = %name_str,
                    error = %e,
                    "清理旧 generation 失败（跳过）"
                );
            } else {
                cleaned.push(name_str.to_string());
            }
        }
    }

    Ok(cleaned)
}

// ── 测试 ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::infra::local_engine::runtime::EngineId;

    // ── asset_key 编码 ──────────────────────────────────────────────────

    #[test]
    fn encode_iic_sensevoice_small() {
        assert_eq!(
            encode_asset_key("iic/SenseVoiceSmall"),
            "iic-sensevoicesmall"
        );
    }

    #[test]
    fn encode_paraformer_zh() {
        assert_eq!(encode_asset_key("paraformer-zh"), "paraformer-zh");
    }

    #[test]
    fn encode_with_underscores() {
        assert_eq!(encode_asset_key("my_model_v2"), "my-model-v2");
    }

    #[test]
    fn encode_with_dots() {
        assert_eq!(encode_asset_key("model.v2.0"), "model-v2-0");
    }

    #[test]
    fn encode_empty_falls_back_to_model() {
        assert_eq!(encode_asset_key("///"), "model");
    }

    #[test]
    fn encode_uppercase_to_lowercase() {
        assert_eq!(encode_asset_key("HelloWorld"), "helloworld");
    }

    #[test]
    fn encode_compresses_double_hyphens() {
        assert_eq!(encode_asset_key("a//b"), "a-b");
    }

    #[test]
    fn encode_trims_leading_trailing_hyphens() {
        assert_eq!(encode_asset_key("/a/b/"), "a-b");
    }

    #[test]
    fn validate_asset_key_rejects_empty() {
        assert!(validate_asset_key("").is_err());
    }

    #[test]
    fn validate_asset_key_rejects_uppercase() {
        assert!(validate_asset_key("HelloWorld").is_err());
    }

    #[test]
    fn validate_asset_key_rejects_slash() {
        assert!(validate_asset_key("a/b").is_err());
    }

    #[test]
    fn validate_asset_key_rejects_double_hyphen() {
        assert!(validate_asset_key("a--b").is_err());
    }

    #[test]
    fn validate_asset_key_accepts_valid() {
        assert!(validate_asset_key("iic-sensevoicesmall").is_ok());
        assert!(validate_asset_key("paraformer-zh").is_ok());
    }

    // ── fingerprint 算法 ─────────────────────────────────────────────────

    /// 创建测试 fixture 目录
    fn make_fixture(name: &str) -> PathBuf {
        let base = std::env::temp_dir()
            .join("blink-model-storage-tests")
            .join(format!("{}-{}", std::process::id(), name));
        let _ = std::fs::remove_dir_all(&base);
        std::fs::create_dir_all(&base).unwrap();
        base
    }

    fn write_file(dir: &Path, rel: &str, content: &[u8]) {
        let path = dir.join(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(&path, content).unwrap();
    }

    #[test]
    fn fingerprint_empty_dir() {
        let dir = make_fixture("fingerprint_empty");
        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 0);
        assert_eq!(fp.total_size_bytes, 0);
        // 空 SHA-256
        assert_eq!(
            fp.fingerprint,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_single_file() {
        let dir = make_fixture("fingerprint_single");
        write_file(&dir, "model.bin", b"hello world");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 1);
        assert_eq!(fp.total_size_bytes, 11);
        assert_eq!(fp.fingerprint.len(), 64);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_deterministic_same_content() {
        // 两个相同内容的目录应有相同 fingerprint
        let dir1 = make_fixture("fp_det_1");
        let dir2 = make_fixture("fp_det_2");

        write_file(&dir1, "a.bin", b"content_a");
        write_file(&dir1, "b.bin", b"content_b");
        write_file(&dir2, "a.bin", b"content_a");
        write_file(&dir2, "b.bin", b"content_b");

        let fp1 = compute_content_fingerprint(&dir1).unwrap();
        let fp2 = compute_content_fingerprint(&dir2).unwrap();

        assert_eq!(fp1.fingerprint, fp2.fingerprint);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn fingerprint_content_change_detected() {
        let dir1 = make_fixture("fp_change_1");
        let dir2 = make_fixture("fp_change_2");

        write_file(&dir1, "a.bin", b"content_a");
        write_file(&dir2, "a.bin", b"content_b"); // 不同内容

        let fp1 = compute_content_fingerprint(&dir1).unwrap();
        let fp2 = compute_content_fingerprint(&dir2).unwrap();

        assert_ne!(fp1.fingerprint, fp2.fingerprint);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn fingerprint_path_change_detected() {
        let dir1 = make_fixture("fp_path_1");
        let dir2 = make_fixture("fp_path_2");

        write_file(&dir1, "a.bin", b"same");
        write_file(&dir2, "b.bin", b"same"); // 不同路径

        let fp1 = compute_content_fingerprint(&dir1).unwrap();
        let fp2 = compute_content_fingerprint(&dir2).unwrap();

        assert_ne!(fp1.fingerprint, fp2.fingerprint);

        let _ = std::fs::remove_dir_all(&dir1);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    #[test]
    fn fingerprint_empty_file() {
        let dir = make_fixture("fp_empty_file");
        write_file(&dir, "empty.bin", b"");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 1);
        assert_eq!(fp.total_size_bytes, 0);
        assert_eq!(fp.fingerprint.len(), 64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_unicode_filename() {
        let dir = make_fixture("fp_unicode");
        write_file(&dir, "模型.bin", b"data");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 1);
        assert_eq!(fp.total_size_bytes, 4);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_excludes_manifest() {
        let dir = make_fixture("fp_exclude_manifest");

        write_file(&dir, "model.bin", b"model_data");
        // manifest.json 应被排除
        write_file(&dir, "manifest.json", b"should_be_excluded");
        // current.json 应被排除
        write_file(&dir, "current.json", b"should_be_excluded");
        // 临时文件应被排除
        write_file(&dir, ".tmp_file", b"should_be_excluded");
        // 下载锁应被排除
        write_file(&dir, ".download_lock", b"should_be_excluded");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 1); // 只有 model.bin
        assert_eq!(fp.total_size_bytes, 10); // "model_data"

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn fingerprint_nested_directories() {
        let dir = make_fixture("fp_nested");

        write_file(&dir, "a.bin", b"aaa");
        write_file(&dir, "sub/b.bin", b"bbb");
        write_file(&dir, "sub/deep/c.bin", b"ccc");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 3);
        assert_eq!(fp.total_size_bytes, 9);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 流式哈希与全量读取算法逐字节等价（大文件跨 1MB 缓冲边界）。
    ///
    /// 参考实现 = Python 侧 `blink_model_installer.py::compute_content_fingerprint`
    /// 的直译（全量 read 后 update），两者必须产出相同 hex。
    #[test]
    fn fingerprint_streaming_equivalent_to_full_read() {
        use sha2::{Digest, Sha256};

        let dir = make_fixture("fp_streaming");
        // 2.5MB 大文件（跨块边界）+ 小文件，确定性伪随机内容
        let mut big = Vec::with_capacity(2 * 1024 * 1024 + 512 * 1024);
        let mut x: u32 = 0x1234_5678;
        for _ in 0..(2 * 1024 * 1024 + 512 * 1024) / 4 {
            x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            big.extend_from_slice(&x.to_le_bytes());
        }
        write_file(&dir, "big.bin", &big);
        write_file(&dir, "sub/small.bin", b"small");

        let fp = compute_content_fingerprint(&dir).unwrap();

        // 全量读取参考实现（Python 算法直译）
        let mut hasher = Sha256::new();
        for (rel, content) in [
            ("big.bin", big.as_slice()),
            ("sub/small.bin", b"small".as_slice()),
        ] {
            let rel_bytes = rel.as_bytes();
            hasher.update((rel_bytes.len() as u64).to_le_bytes());
            hasher.update(rel_bytes);
            hasher.update((content.len() as u64).to_le_bytes());
            hasher.update(content);
        }
        let expected = format!("{:x}", hasher.finalize());

        assert_eq!(fp.fingerprint, expected, "流式与全量算法必须产出相同指纹");
        assert_eq!(fp.file_count, 2);
        assert_eq!(fp.total_size_bytes, (big.len() + 5) as u64);

        let _ = std::fs::remove_dir_all(&dir);
    }

    // ── Golden fingerprint 测试（Rust/Python 共享规范）──────────────────────
    //
    // 以下测试使用固定内容的文件，产生确定的指纹值。
    // Python 侧 `test_fingerprint_golden.py` 使用完全相同的 fixture 内容，
    // 验证两边产生完全一致的 hex SHA-256。
    //
    // 如果此处的 expected 值需要变更，必须同时更新 Python 侧 golden test。

    /// Golden fixture 1：单个文件，内容 "hello world"
    /// 使用确定性验证：两次计算同一 fixture 必须得到相同值。
    /// 跨语言一致性由 Python golden test 验证（使用完全相同的 fixture 内容）。
    #[test]
    fn golden_fingerprint_single_file() {
        let dir = make_fixture("golden_single");
        write_file(&dir, "model.bin", b"hello world");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 1);
        assert_eq!(fp.total_size_bytes, 11);

        // 确定性：同一 fixture 两次计算必相同
        let dir2 = make_fixture("golden_single_2");
        write_file(&dir2, "model.bin", b"hello world");
        let fp2 = compute_content_fingerprint(&dir2).unwrap();
        assert_eq!(fp.fingerprint, fp2.fingerprint);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Golden fixture 2：两个嵌套文件 + 排序验证
    #[test]
    fn golden_fingerprint_nested_sorted() {
        let dir = make_fixture("golden_nested");

        write_file(&dir, "b/model.pt", b"model_b_data");
        write_file(&dir, "a/model.pt", b"model_a_data");
        write_file(&dir, "config.json", b"{\"version\":1}");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 3);
        assert_eq!(fp.total_size_bytes, 37); // 12 + 12 + 13

        // 确定性：同一 fixture 两次计算必相同
        let dir2 = make_fixture("golden_nested_2");
        write_file(&dir2, "b/model.pt", b"model_b_data");
        write_file(&dir2, "a/model.pt", b"model_a_data");
        write_file(&dir2, "config.json", b"{\"version\":1}");
        let fp2 = compute_content_fingerprint(&dir2).unwrap();
        assert_eq!(fp.fingerprint, fp2.fingerprint);

        eprintln!("golden_fingerprint_nested_sorted = {}", fp.fingerprint);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    /// Golden fixture 3：空目录 + manifest 排除
    /// 空 SHA-256 是 e3b0c442...（已在上面的 fingerprint_empty_dir 测试中验证）
    #[test]
    fn golden_fingerprint_empty_with_manifest_excluded() {
        let dir = make_fixture("golden_empty_meta");

        // 只写 manifest.json + current.json（都应被排除）
        write_file(&dir, "manifest.json", b"{\"test\":true}");
        write_file(&dir, "current.json", b"{\"install_id\":\"test\"}");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 0);
        assert_eq!(fp.total_size_bytes, 0);

        // 空目录的 SHA-256 = e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855
        assert_eq!(
            fp.fingerprint,
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Golden fixture 4：混合内容（模拟真实模型目录结构）
    #[test]
    fn golden_fingerprint_model_like() {
        let dir = make_fixture("golden_model_like");

        // 模拟 FunASR 模型目录结构
        write_file(
            &dir,
            "model.pt",
            b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
        );
        write_file(
            &dir,
            "configuration.json",
            b"{\"model\":\"SenseVoice\",\"language\":\"zh\"}",
        );
        write_file(&dir, "examples/sample.wav", b"WAVE\x12\x34\x56\x78");
        write_file(&dir, "subdir/weights.bin", b"\xff\xfe\xfd\xfc");

        let fp = compute_content_fingerprint(&dir).unwrap();
        assert_eq!(fp.file_count, 4);
        assert_eq!(fp.total_size_bytes, 66); // 16 + 38 + 8 + 4

        // 确定性验证
        let dir2 = make_fixture("golden_model_like_2");
        write_file(
            &dir2,
            "model.pt",
            b"\x00\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f",
        );
        write_file(
            &dir2,
            "configuration.json",
            b"{\"model\":\"SenseVoice\",\"language\":\"zh\"}",
        );
        write_file(&dir2, "examples/sample.wav", b"WAVE\x12\x34\x56\x78");
        write_file(&dir2, "subdir/weights.bin", b"\xff\xfe\xfd\xfc");
        let fp2 = compute_content_fingerprint(&dir2).unwrap();
        assert_eq!(fp.fingerprint, fp2.fingerprint);

        eprintln!("golden_fingerprint_model_like = {}", fp.fingerprint);

        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&dir2);
    }

    // ── restore_model_state ──────────────────────────────────────────────

    #[test]
    fn restore_not_installed_when_no_pointer() {
        let engine = EngineId::new("funasr").unwrap();
        let dir = make_fixture("restore_not_installed");
        // 保存 runtimes_root 测试根，确保指向临时目录
        // models_root() 在 test 模式下返回 runtimes_root()/models
        // runtimes_root() 在 test 模式下返回 temp_dir()/blink-runtime-tests-{pid}
        // 所以 model_storage 的路径自动隔离

        let asset_key = "test-restore-not-installed";

        // 没有任何文件 → NotInstalled
        let state = restore_model_state(&engine, asset_key).unwrap();
        assert_eq!(state, RestoredModelState::NotInstalled);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn restore_installed_when_valid() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-restore-installed";

        // 创建 generation + payload + manifest + current.json
        let install_id = "gen-test-0001";
        let payload_dir = model_payload_dir(&engine, asset_key, install_id).unwrap();
        std::fs::create_dir_all(&payload_dir).unwrap();
        write_file(&payload_dir, "model.bin", b"model_data");

        let fp = compute_content_fingerprint(&payload_dir).unwrap();

        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine.clone(),
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            source: ModelSource::Unverified {
                source: "test".to_string(),
                downloaded_at_ms: now_ms(),
            },
            install_id: install_id.to_string(),
            installed_at_ms: now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fp.fingerprint.clone(),
            payload_size_bytes: fp.total_size_bytes,
            file_count: fp.file_count,
            compatibility_schema: 1,
            model_contract_identity: ModelContractIdentity {
                model_id: "test-model".to_string(),
                revision: "v1".to_string(),
                checksum_source_kind: "unverified".to_string(),
            },
        };

        write_model_manifest(&engine, asset_key, install_id, &manifest).unwrap();

        let pointer = ModelCurrentPointer {
            install_id: install_id.to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_CURRENT_POINTER_SCHEMA_VERSION,
        };
        write_model_current_pointer(&engine, asset_key, &pointer).unwrap();

        // 恢复 → Installed
        let state = restore_model_state(&engine, asset_key).unwrap();
        match state {
            RestoredModelState::Installed {
                install_id: iid,
                manifest: m,
            } => {
                assert_eq!(iid, install_id);
                assert_eq!(m.model_id, "test-model");
                assert_eq!(m.content_fingerprint, fp.fingerprint);
            }
            other => panic!("expected Installed, got {:?}", other),
        }

        // 清理
        let root = asset_root(&engine, asset_key).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_structural_ok_but_explicit_verify_catches_fingerprint_mismatch() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-restore-corrupted-fp";

        let install_id = "gen-test-corrupt-0001";
        let payload_dir = model_payload_dir(&engine, asset_key, install_id).unwrap();
        std::fs::create_dir_all(&payload_dir).unwrap();
        write_file(&payload_dir, "model.bin", b"model_data");

        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine.clone(),
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            source: ModelSource::Unverified {
                source: "test".to_string(),
                downloaded_at_ms: now_ms(),
            },
            install_id: install_id.to_string(),
            installed_at_ms: now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: "wrong_fingerprint".to_string(), // 故意错误
            payload_size_bytes: 10,
            file_count: 1,
            compatibility_schema: 1,
            model_contract_identity: ModelContractIdentity {
                model_id: "test-model".to_string(),
                revision: "v1".to_string(),
                checksum_source_kind: "unverified".to_string(),
            },
        };

        write_model_manifest(&engine, asset_key, install_id, &manifest).unwrap();

        let pointer = ModelCurrentPointer {
            install_id: install_id.to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_CURRENT_POINTER_SCHEMA_VERSION,
        };
        write_model_current_pointer(&engine, asset_key, &pointer).unwrap();

        // restore 只做结构校验 → Installed（不做 GB hash）
        match restore_model_state(&engine, asset_key).unwrap() {
            RestoredModelState::Installed { .. } => {}
            other => panic!("expected Installed, got {:?}", other),
        }

        // 显式完整校验 → fingerprint 不匹配被抓到
        let err = verify_model_payload(&engine, asset_key, &manifest).unwrap_err();
        assert!(err.contains("fingerprint 不匹配"), "unexpected: {err}");

        // 清理
        let root = asset_root(&engine, asset_key).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn restore_corrupted_when_payload_missing() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-restore-corrupted-payload";

        let install_id = "gen-test-corrupt-0002";
        // 不创建 payload 目录

        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine.clone(),
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            source: ModelSource::Unverified {
                source: "test".to_string(),
                downloaded_at_ms: now_ms(),
            },
            install_id: install_id.to_string(),
            installed_at_ms: now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: "any".to_string(),
            payload_size_bytes: 0,
            file_count: 0,
            compatibility_schema: 1,
            model_contract_identity: ModelContractIdentity {
                model_id: "test-model".to_string(),
                revision: "v1".to_string(),
                checksum_source_kind: "unverified".to_string(),
            },
        };

        write_model_manifest(&engine, asset_key, install_id, &manifest).unwrap();

        let pointer = ModelCurrentPointer {
            install_id: install_id.to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_CURRENT_POINTER_SCHEMA_VERSION,
        };
        write_model_current_pointer(&engine, asset_key, &pointer).unwrap();

        // 恢复 → Corrupted（payload 不存在）
        let state = restore_model_state(&engine, asset_key).unwrap();
        match state {
            RestoredModelState::Corrupted { reason, .. } => {
                assert!(reason.contains("payload"));
            }
            other => panic!("expected Corrupted, got {:?}", other),
        }

        // 清理
        let root = asset_root(&engine, asset_key).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    // ── promote + delete ─────────────────────────────────────────────────

    #[test]
    fn promote_staging_creates_generation_and_pointer() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-promote";
        let operation_id = "op-test-promote-0001";
        let install_id = "gen-test-promote-0001";

        // 创建 staging payload
        let staging_payload =
            model_operation_staging_payload_dir(&engine, asset_key, operation_id).unwrap();
        std::fs::create_dir_all(&staging_payload).unwrap();
        write_file(&staging_payload, "model.bin", b"model_data");

        let fp = compute_content_fingerprint(&staging_payload).unwrap();

        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine.clone(),
            model_id: "test-model".to_string(),
            revision: "v1".to_string(),
            source: ModelSource::Unverified {
                source: "test".to_string(),
                downloaded_at_ms: now_ms(),
            },
            install_id: install_id.to_string(),
            installed_at_ms: now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: fp.fingerprint.clone(),
            payload_size_bytes: fp.total_size_bytes,
            file_count: fp.file_count,
            compatibility_schema: 1,
            model_contract_identity: ModelContractIdentity {
                model_id: "test-model".to_string(),
                revision: "v1".to_string(),
                checksum_source_kind: "unverified".to_string(),
            },
        };

        promote_staging_to_generation(&engine, asset_key, install_id, operation_id, &manifest)
            .unwrap();

        // 验证 generation + payload + manifest + current.json 存在
        let gen_payload = model_payload_dir(&engine, asset_key, install_id).unwrap();
        assert!(gen_payload.exists());
        assert!(gen_payload.join("model.bin").exists());

        let pointer = read_model_current_pointer(&engine, asset_key).unwrap();
        assert!(pointer.is_some());
        assert_eq!(pointer.unwrap().install_id, install_id);

        // 清理
        let root = asset_root(&engine, asset_key).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_generation_removes_pointer_and_dir() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-delete";

        let install_id = "gen-test-delete-0001";
        let payload_dir = model_payload_dir(&engine, asset_key, install_id).unwrap();
        std::fs::create_dir_all(&payload_dir).unwrap();
        write_file(&payload_dir, "model.bin", b"data");

        let manifest = ModelManifest {
            schema_version: MODEL_MANIFEST_SCHEMA_VERSION,
            engine_id: engine.clone(),
            model_id: "test".to_string(),
            revision: "v1".to_string(),
            source: ModelSource::Unverified {
                source: "test".to_string(),
                downloaded_at_ms: now_ms(),
            },
            install_id: install_id.to_string(),
            installed_at_ms: now_ms(),
            content_fingerprint_algorithm: CONTENT_FINGERPRINT_ALGORITHM.to_string(),
            content_fingerprint: "fake".to_string(),
            payload_size_bytes: 4,
            file_count: 1,
            compatibility_schema: 1,
            model_contract_identity: ModelContractIdentity {
                model_id: "test".to_string(),
                revision: "v1".to_string(),
                checksum_source_kind: "unverified".to_string(),
            },
        };

        write_model_manifest(&engine, asset_key, install_id, &manifest).unwrap();
        let pointer = ModelCurrentPointer {
            install_id: install_id.to_string(),
            updated_at_ms: now_ms(),
            schema_version: MODEL_CURRENT_POINTER_SCHEMA_VERSION,
        };
        write_model_current_pointer(&engine, asset_key, &pointer).unwrap();

        // 删除
        delete_model_generation(&engine, asset_key).unwrap();

        // 验证
        let pointer_path = model_current_pointer_path(&engine, asset_key).unwrap();
        assert!(!pointer_path.exists());
        let gen_dir = model_generation_dir(&engine, asset_key, install_id).unwrap();
        assert!(!gen_dir.exists());

        // 清理
        let root = asset_root(&engine, asset_key).unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_generation_fails_when_no_pointer() {
        let engine = EngineId::new("funasr").unwrap();
        let asset_key = "test-delete-no-pointer";

        let result = delete_model_generation(&engine, asset_key);
        assert!(result.is_err());
    }
}
