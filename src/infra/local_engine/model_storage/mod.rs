//! 模型资产存储协议（0.22.6 H3-model）。
//!
//! 每个模型资产使用 `slots/{slot_id}` + `active.json` 的单 active revision
//! 事务。slot、journal 与 residue 都是内部实现，不构成历史版本产品语义。
//!
//! ## 设计铁则
//!
//! - **manifest 是唯一真源**：`Installed` 状态只能从有效 manifest + active pointer
//!   + payload + fingerprint 全部一致恢复。禁止仅凭目录非空推断 Installed。
//! - **asset_key 无碰撞编码**：可读 slug 后追加 canonical model id 的 hash。
//! - **slot 隔离**：每次安装/修复创建 candidate slot，校验通过后
//!   原子切换 `active.json`；失败时旧 active 不受影响。
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

/// 模型 active.json schema 版本。
pub const MODEL_ACTIVE_POINTER_SCHEMA_VERSION: u32 = 1;

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
/// - 追加 canonical model id SHA-256 的前 12 hex，消除 slug 碰撞
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

    let slug = if trimmed.is_empty() { "model" } else { trimmed };
    let slug = &slug[..slug.len().min(100)];
    let digest = Sha256::digest(model_id.as_bytes());
    let suffix = format!("{:x}", digest);
    format!("{slug}-{}", &suffix[..12])
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
        super::runtime::runtimes_root().join("models")
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

/// active.json 路径：`models/{engine_id}/{asset_key}/active.json`
pub fn model_active_pointer_path(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("active.json"))
}

/// slots 目录：`models/{engine_id}/{asset_key}/slots/`
pub fn model_slots_dir(engine_id: &EngineId, asset_key: &str) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("slots"))
}

/// 单个内部 slot 目录。
pub fn model_slot_dir(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<PathBuf, RuntimeError> {
    validate_install_id(slot_id)?;
    Ok(model_slots_dir(engine_id, asset_key)?.join(slot_id))
}

/// slot manifest 路径。
pub fn model_manifest_path(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(model_slot_dir(engine_id, asset_key, slot_id)?.join("manifest.json"))
}

/// slot payload 目录。
pub fn model_payload_dir(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<PathBuf, RuntimeError> {
    Ok(model_slot_dir(engine_id, asset_key, slot_id)?.join("payload"))
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

/// 模型 slot 的不可变 manifest。
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
    /// 内部 slot id。
    pub slot_id: String,
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

// ── ModelActivePointer ──────────────────────────────────────────────────────

/// `active.json` 指针文件内容。
///
/// 采用同目录临时文件 + replace/rename 原子写入。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelActivePointer {
    /// 当前 active slot id。
    pub slot_id: String,
    /// 更新时间（Unix 毫秒）。
    pub updated_at_ms: u64,
    /// schema 版本。
    pub schema_version: u32,
}

// ── active.json 读写 ───────────────────────────────────────────────────────

/// 读取 active.json。
///
/// 如果文件不存在返回 `Ok(None)`（模型未安装）。
pub fn read_model_active_pointer(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<Option<ModelActivePointer>, RuntimeError> {
    let path = model_active_pointer_path(engine_id, asset_key)?;
    if !path.exists() {
        return Ok(None);
    }
    let content = std::fs::read_to_string(&path)?;
    let pointer: ModelActivePointer =
        serde_json::from_str(&content).map_err(|e| RuntimeError::CurrentPointerParseFailed {
            message: format!("{e}"),
        })?;
    Ok(Some(pointer))
}

/// 原子写入 active.json。
pub fn write_model_active_pointer(
    engine_id: &EngineId,
    asset_key: &str,
    pointer: &ModelActivePointer,
) -> Result<(), RuntimeError> {
    let path = model_active_pointer_path(engine_id, asset_key)?;
    atomic_write_json(&path, pointer)
}

// ── manifest 读写 ───────────────────────────────────────────────────────────

/// 读取 slot manifest。
pub fn read_model_manifest(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<ModelManifest, RuntimeError> {
    let path = model_manifest_path(engine_id, asset_key, slot_id)?;
    if !path.exists() {
        return Err(RuntimeError::GenerationNotFound {
            install_id: slot_id.to_string(),
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

/// 写入 slot manifest。
pub fn write_model_manifest(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
    manifest: &ModelManifest,
) -> Result<(), RuntimeError> {
    let dir = model_slot_dir(engine_id, asset_key, slot_id)?;
    std::fs::create_dir_all(&dir)?;
    let path = dir.join("manifest.json");
    atomic_write_json(&path, manifest)
}

// ── 模型状态恢复 ────────────────────────────────────────────────────────────

/// 从磁盘恢复的模型状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RestoredModelState {
    /// 已安装：active pointer + manifest + payload 目录结构链有效。
    /// （不含内容 hash——完整校验走 [`verify_model_payload`]。）
    Installed {
        slot_id: String,
        manifest: Box<ModelManifest>,
    },
    /// 损坏：pointer/manifest/payload 目录结构链任一损坏或不一致。
    Corrupted {
        slot_id: Option<String>,
        reason: String,
    },
    /// 未安装：没有有效 active revision。
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
    recover_model_transaction(engine_id, asset_key)?;
    let pointer_path = model_active_pointer_path(engine_id, asset_key)?;
    if !pointer_path.exists() {
        return Ok(RestoredModelState::NotInstalled);
    }

    let pointer_content = match std::fs::read_to_string(&pointer_path) {
        Ok(c) => c,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                slot_id: None,
                reason: format!("读取 active.json 失败: {e}"),
            });
        }
    };

    let pointer: ModelActivePointer = match serde_json::from_str(&pointer_content) {
        Ok(p) => p,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                slot_id: None,
                reason: format!("解析 active.json 失败: {e}"),
            });
        }
    };

    if pointer.schema_version != MODEL_ACTIVE_POINTER_SCHEMA_VERSION {
        return Ok(RestoredModelState::Corrupted {
            slot_id: Some(pointer.slot_id.clone()),
            reason: format!(
                "active.json schema 版本不兼容: expected={}, actual={}",
                MODEL_ACTIVE_POINTER_SCHEMA_VERSION, pointer.schema_version
            ),
        });
    }

    let manifest = match read_model_manifest(engine_id, asset_key, &pointer.slot_id) {
        Ok(m) => m,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                slot_id: Some(pointer.slot_id.clone()),
                reason: format!("manifest 读取失败: {e}"),
            });
        }
    };

    // 验证 manifest schema
    if manifest.schema_version != MODEL_MANIFEST_SCHEMA_VERSION {
        return Ok(RestoredModelState::Corrupted {
            slot_id: Some(pointer.slot_id.clone()),
            reason: format!(
                "manifest schema 版本不兼容: expected={}, actual={}",
                MODEL_MANIFEST_SCHEMA_VERSION, manifest.schema_version
            ),
        });
    }

    // 验证 manifest identity
    if manifest.engine_id != *engine_id {
        return Ok(RestoredModelState::Corrupted {
            slot_id: Some(pointer.slot_id.clone()),
            reason: format!(
                "manifest engine_id 不匹配: expected={}, actual={}",
                engine_id, manifest.engine_id
            ),
        });
    }

    // 验证 payload 目录存在（结构校验——不读内容、不 hash）
    if manifest.slot_id != pointer.slot_id {
        return Ok(RestoredModelState::Corrupted {
            slot_id: Some(pointer.slot_id.clone()),
            reason: "active pointer 与 manifest slot_id 不一致".to_string(),
        });
    }

    let payload_dir = match model_payload_dir(engine_id, asset_key, &pointer.slot_id) {
        Ok(p) => p,
        Err(e) => {
            return Ok(RestoredModelState::Corrupted {
                slot_id: Some(pointer.slot_id.clone()),
                reason: format!("payload 路径计算失败: {e}"),
            });
        }
    };
    if !payload_dir.exists() {
        return Ok(RestoredModelState::Corrupted {
            slot_id: Some(pointer.slot_id.clone()),
            reason: "payload 目录不存在".to_string(),
        });
    }

    Ok(RestoredModelState::Installed {
        slot_id: pointer.slot_id,
        manifest: Box::new(manifest),
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
    let payload_dir = match model_payload_dir(engine_id, asset_key, &manifest.slot_id) {
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
        if name_str == "manifest.json" || name_str == "active.json" {
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

// ── 单 active slot 事务 ────────────────────────────────────────────────────
//
// 提交协议（崩溃安全）：
//
// ```text
// write journal(Preparing, candidate, previous)
//   → move payload → slots/{candidate}/payload
//   → write candidate manifest
//   → write active.json → candidate      ← 唯一原子提交点
//   → write journal(Committed)
//   → delete previous slot（失败记为有界 residue）
//   → remove journal
// ```
//
// 恢复判定不单独信任 journal phase，**必须核对 active pointer**：
// 指针写入与 journal 更新为两步，中间崩溃时 journal 仍是 `Preparing`
// 但指针已切换——此时按已提交处理（完成旧 slot 清理），绝不删除
// 指针已指向的 candidate。铁则：**恢复路径永远不删除 active pointer
// 当前指向的 slot**。

const MODEL_TRANSACTION_SCHEMA_VERSION: u32 = 1;
const MAX_CLEANUP_RESIDUES: usize = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum ModelTransactionPhase {
    Preparing,
    Committed,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ModelTransaction {
    schema_version: u32,
    operation_id: String,
    candidate_slot_id: String,
    previous_slot_id: Option<String>,
    phase: ModelTransactionPhase,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct CleanupResidue {
    slot_id: String,
    attempts: u32,
    last_error: String,
}

fn transaction_path(engine_id: &EngineId, asset_key: &str) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("transaction.json"))
}

fn residue_path(engine_id: &EngineId, asset_key: &str) -> Result<PathBuf, RuntimeError> {
    Ok(asset_root(engine_id, asset_key)?.join("cleanup-residue.json"))
}

fn write_transaction(
    engine_id: &EngineId,
    asset_key: &str,
    tx: &ModelTransaction,
) -> Result<(), RuntimeError> {
    atomic_write_json(&transaction_path(engine_id, asset_key)?, tx)
}

fn remove_transaction(engine_id: &EngineId, asset_key: &str) -> Result<(), RuntimeError> {
    let path = transaction_path(engine_id, asset_key)?;
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    Ok(())
}

fn remember_residue(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
    error: &str,
) -> Result<(), RuntimeError> {
    let path = residue_path(engine_id, asset_key)?;
    let mut residues: Vec<CleanupResidue> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_default();
    if let Some(item) = residues.iter_mut().find(|item| item.slot_id == slot_id) {
        item.attempts = item.attempts.saturating_add(1);
        item.last_error = error.to_string();
    } else {
        residues.push(CleanupResidue {
            slot_id: slot_id.to_string(),
            attempts: 1,
            last_error: error.to_string(),
        });
    }
    if residues.len() > MAX_CLEANUP_RESIDUES {
        residues.drain(..residues.len() - MAX_CLEANUP_RESIDUES);
    }
    atomic_write_json(&path, &residues)
}

/// 删除 slot 对应的 residue 记录（重试删除成功或 slot 已消失时调用）。
fn forget_residue(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<(), RuntimeError> {
    let path = residue_path(engine_id, asset_key)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok(()),
    };
    let mut residues: Vec<CleanupResidue> = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let before = residues.len();
    residues.retain(|item| item.slot_id != slot_id);
    if residues.len() == before {
        return Ok(());
    }
    if residues.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    } else {
        atomic_write_json(&path, &residues)?;
    }
    Ok(())
}

/// 清除已不再对应磁盘目录的 residue 记录（有界重试的收敛终点）。
fn prune_residues(engine_id: &EngineId, asset_key: &str) -> Result<(), RuntimeError> {
    let path = residue_path(engine_id, asset_key)?;
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return Ok(()),
    };
    let residues: Vec<CleanupResidue> = match serde_json::from_str(&raw) {
        Ok(r) => r,
        Err(_) => return Ok(()),
    };
    let remaining: Vec<CleanupResidue> = residues
        .into_iter()
        .filter(|item| {
            model_slot_dir(engine_id, asset_key, &item.slot_id)
                .map(|p| p.exists())
                .unwrap_or(false)
        })
        .collect();
    if remaining.is_empty() {
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
    } else {
        atomic_write_json(&path, &remaining)?;
    }
    Ok(())
}

fn remove_or_record_residue(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
) -> Result<(), RuntimeError> {
    let path = model_slot_dir(engine_id, asset_key, slot_id)?;
    if !path.exists() {
        forget_residue(engine_id, asset_key, slot_id)?;
        return Ok(());
    }
    match std::fs::remove_dir_all(&path) {
        Ok(()) => forget_residue(engine_id, asset_key, slot_id),
        Err(error) => remember_residue(engine_id, asset_key, slot_id, &error.to_string()),
    }
}

/// 恢复未完成的模型事务。只读取 journal、pointer、manifest 和目录结构，不 hash payload。
///
/// 判定规则见模块头「提交协议」：`Preparing` 分支必须核对 active pointer——
/// 指针已指向 candidate 说明提交点已越过，按已提交完成清理；否则回滚删除
/// candidate。任何分支都不会删除 active pointer 当前指向的 slot。
pub fn recover_model_transaction(
    engine_id: &EngineId,
    asset_key: &str,
) -> Result<(), RuntimeError> {
    let path = transaction_path(engine_id, asset_key)?;
    if !path.exists() {
        return Ok(());
    }
    let raw = std::fs::read_to_string(&path)?;
    let tx: ModelTransaction =
        serde_json::from_str(&raw).map_err(|error| RuntimeError::ManifestParseFailed {
            message: format!("model transaction parse failed: {error}"),
        })?;
    if tx.schema_version != MODEL_TRANSACTION_SCHEMA_VERSION {
        return Err(RuntimeError::ManifestSchemaIncompatible {
            expected: MODEL_TRANSACTION_SCHEMA_VERSION,
            actual: tx.schema_version,
        });
    }

    // active pointer 是唯一提交真源。读取/解析失败必须 fail-closed：保留
    // journal 和所有 slot，禁止把「无法确认」降级成可执行删除的状态。
    let active_pointer = read_model_active_pointer(engine_id, asset_key)?;
    let active_slot = active_pointer
        .as_ref()
        .map(|pointer| pointer.slot_id.as_str());
    let active_points_to_candidate = active_slot == Some(tx.candidate_slot_id.as_str());

    let committed = match tx.phase {
        ModelTransactionPhase::Preparing => {
            // 崩溃点在「指针写入之后、journal 更新之前」——指针是原子提交点，
            // 已落盘即视为已提交，完成旧 slot 清理（roll forward）。
            active_points_to_candidate
        }
        ModelTransactionPhase::Committed if active_points_to_candidate => true,
        ModelTransactionPhase::Committed => {
            return Err(RuntimeError::TransactionJournalInvalid {
                message: format!(
                    "model transaction committed but active pointer mismatch: candidate={}, active={}",
                    tx.candidate_slot_id,
                    active_slot.unwrap_or("<none>")
                ),
            });
        }
    };

    if committed {
        if let Some(previous) = tx.previous_slot_id.as_deref()
            && previous != tx.candidate_slot_id
        {
            remove_or_record_residue(engine_id, asset_key, previous)?;
        }
    } else {
        // 指针未切换（仍指向旧 active 或不存在）→ 回滚：candidate 是孤儿，删除。
        remove_or_record_residue(engine_id, asset_key, &tx.candidate_slot_id)?;
    }
    remove_transaction(engine_id, asset_key)
}

/// 将已校验 staging payload 提交为唯一 active slot。
///
/// 提交顺序见模块头「提交协议」：journal(Preparing) → payload/manifest 就位 →
/// **指针切换（原子提交点）** → journal(Committed) → 删旧 slot → 撤 journal。
/// 指针切换前的任何失败都保持旧 active 不动（candidate 由调用方或恢复路径回收）。
pub fn promote_staging_to_active_slot(
    engine_id: &EngineId,
    asset_key: &str,
    slot_id: &str,
    operation_id: &str,
    manifest: &ModelManifest,
) -> Result<(), RuntimeError> {
    let staging_payload = model_operation_staging_payload_dir(engine_id, asset_key, operation_id)?;
    let slot_dir = model_slot_dir(engine_id, asset_key, slot_id)?;
    let target_payload = slot_dir.join("payload");

    // 候选 slot id 必须全新（slot id 由每次操作独立生成；repair 也使用新 id）
    if slot_dir.exists() {
        return Err(RuntimeError::GenerationPromoteFailed {
            message: format!("candidate slot 已存在: {}", slot_dir.display()),
        });
    }

    let previous_slot_id = read_model_active_pointer(engine_id, asset_key)?.map(|p| p.slot_id);
    let transaction = ModelTransaction {
        schema_version: MODEL_TRANSACTION_SCHEMA_VERSION,
        operation_id: operation_id.to_string(),
        candidate_slot_id: slot_id.to_string(),
        previous_slot_id: previous_slot_id.clone(),
        phase: ModelTransactionPhase::Preparing,
    };
    write_transaction(engine_id, asset_key, &transaction)?;

    // 提交点前的失败：清 candidate + journal（恢复路径是同一语义的兜底）
    let abort = |slot_dir: &Path| {
        let _ = std::fs::remove_dir_all(slot_dir);
        let _ = remove_transaction(engine_id, asset_key);
    };

    std::fs::create_dir_all(&slot_dir)?;

    // 移动 payload（同卷 rename 是原子的）
    if staging_payload.exists() {
        if let Err(e) = std::fs::rename(&staging_payload, &target_payload) {
            abort(&slot_dir);
            return Err(RuntimeError::GenerationPromoteFailed {
                message: format!("payload 移动失败: {e}"),
            });
        }
    } else {
        // payload 不存在（空模型？）——创建空目录
        std::fs::create_dir_all(&target_payload)?;
    }

    // 写入 manifest
    if let Err(e) = write_model_manifest(engine_id, asset_key, slot_id, manifest) {
        abort(&slot_dir);
        return Err(e);
    }

    // ── 原子提交点：切换 active.json ──
    // 成功后事务即视为已提交；此后的失败只影响旧 slot 清理（residue 兜底），
    // 不再回滚。失败则旧 active 保持原样，candidate/journal 由 abort 清理
    // （清理失败时恢复路径按 Preparing + 指针未切换回滚）。
    let pointer = ModelActivePointer {
        slot_id: slot_id.to_string(),
        updated_at_ms: now_ms(),
        schema_version: MODEL_ACTIVE_POINTER_SCHEMA_VERSION,
    };
    if let Err(e) = write_model_active_pointer(engine_id, asset_key, &pointer) {
        abort(&slot_dir);
        return Err(e);
    }

    // journal 记录提交事实（崩溃时恢复按「指针已指向 candidate」同样 roll forward）。
    // 从这里开始不得再向调用方报告普通安装失败：active pointer 已经提交，
    // 后续只属于可恢复的事务收尾。
    let mut transaction = transaction;
    transaction.phase = ModelTransactionPhase::Committed;
    if let Err(error) = write_transaction(engine_id, asset_key, &transaction) {
        tracing::warn!(
            engine = %engine_id,
            asset_key,
            slot_id,
            %error,
            "模型已提交，但 Committed journal 写入失败；按 active pointer 尝试收尾"
        );
        if let Err(recovery_error) = recover_model_transaction(engine_id, asset_key) {
            tracing::warn!(
                engine = %engine_id,
                asset_key,
                slot_id,
                %recovery_error,
                "模型已提交，事务收尾暂未完成；保留 journal 供后续恢复"
            );
        }
        return Ok(());
    }

    // 删除旧 slot（失败记为有界 residue，可重试收敛）
    if let Some(previous) = previous_slot_id.as_deref()
        && previous != slot_id
        && let Err(error) = remove_or_record_residue(engine_id, asset_key, previous)
    {
        tracing::warn!(
            engine = %engine_id,
            asset_key,
            slot_id,
            previous_slot_id = previous,
            %error,
            "模型已提交，旧 slot 清理暂未完成；保留 Committed journal"
        );
        return Ok(());
    }
    if let Err(error) = remove_transaction(engine_id, asset_key) {
        tracing::warn!(
            engine = %engine_id,
            asset_key,
            slot_id,
            %error,
            "模型已提交，但 transaction journal 暂未清除；后续恢复将幂等收尾"
        );
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

/// 删除当前 installed model。冲突规则由 EngineManager 在调用前裁决。
pub fn delete_active_model(engine_id: &EngineId, asset_key: &str) -> Result<(), RuntimeError> {
    let pointer = read_model_active_pointer(engine_id, asset_key)?;
    let Some(pointer) = pointer else {
        return Err(RuntimeError::GenerationNotFound {
            install_id: "no active pointer".to_string(),
        });
    };

    let slot_dir = model_slot_dir(engine_id, asset_key, &pointer.slot_id)?;
    if slot_dir.exists() {
        std::fs::remove_dir_all(&slot_dir).map_err(|e| RuntimeError::CleanupFailed {
            message: format!("删除 active model slot 失败: {e}"),
        })?;
    }

    let pointer_path = model_active_pointer_path(engine_id, asset_key)?;
    if pointer_path.exists() {
        std::fs::remove_file(&pointer_path).map_err(|e| RuntimeError::CleanupFailed {
            message: format!("删除 active.json 失败: {e}"),
        })?;
    }

    // 已删除的 active slot 不再是 residue 候选
    let _ = forget_residue(engine_id, asset_key, &pointer.slot_id);

    Ok(())
}

/// 重试清理非 active slot 和有界 residue。
pub fn cleanup_inactive_slots(
    engine_id: &EngineId,
    asset_key: &str,
    active_slot_id: &str,
) -> Result<Vec<String>, RuntimeError> {
    let slots_dir = model_slots_dir(engine_id, asset_key)?;
    if !slots_dir.exists() {
        prune_residues(engine_id, asset_key)?;
        return Ok(Vec::new());
    }

    let mut cleaned = Vec::new();
    let entries = std::fs::read_dir(&slots_dir)?;

    for entry in entries {
        let entry = entry?;
        let name = entry.file_name();
        let name_str = name.to_string_lossy();

        // 跳过 current
        if name_str == active_slot_id {
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
                    asset_key = asset_key,
                    slot_id = %name_str,
                    error = %e,
                    "清理非 active slot 失败，保留为 cleanup residue"
                );
                remember_residue(engine_id, asset_key, &name_str, &e.to_string())?;
            } else {
                cleaned.push(name_str.to_string());
            }
        }
    }

    // 已成功删除（或早已消失）的 slot 对应 residue 记录一并收敛
    prune_residues(engine_id, asset_key)?;

    Ok(cleaned)
}

#[cfg(test)]
mod tests;
