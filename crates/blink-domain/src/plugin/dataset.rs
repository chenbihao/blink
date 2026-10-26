//! 数据包安装状态机（0.28.4 §五.4）——词库等只读数据集的暂存/校验/提交/
//! 版本切换与恢复。与插件本体安装（install.rs）同一套纪律，差异在于：
//!
//! - 挂载点是 `datasets/<plugin_id>/<dataset_id>/<version>/`，目录内容提交后
//!   不可变；**当前版本由 `active.json` 指针表达**（原子切换是提交点），
//!   插件进程每次查询按指针解析——升级/回滚只动指针，不覆盖使用中的 DB。
//! - 数据集与插件独立版本化：不校验宿主兼容范围（那是插件包的事），只校验
//!   自身的 `format_version` 上限与文件完整性。
//! - `plugin_id` 来自 dataset.json 声明（构建期写入），安装时校验合法字符集；
//!   调用方不必传 plugin_id，防止把词典数据挂到无关插件名下要靠包自身声明。
//!
//! 崩溃恢复语义：move 版本目录之后、active.json 切换之前崩溃 → 新版本目录
//! 是孤儿（recover 按 active 指针清理）；active.json 已切、registry 未写 →
//! 文件系统已是运行时真源，recover 从 dataset.json 重建记录（自愈）。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::Digest;

use blink_infra::plugin_storage::archive::{self, ExtractLimits};
use blink_infra::plugin_storage::{
    DatasetRecord, InstallSource, InstallState, PluginStorage, PluginStorageError,
};

use super::contract::validate_plugin_id;

/// 数据包 dataset.json 的格式版本上限（0.28.4 首发 = 1）。
pub const SUPPORTED_DATASET_FORMAT_VERSION: u32 = 1;

/// 数据包安装/卸载错误。
#[derive(Debug, thiserror::Error)]
pub enum DatasetError {
    #[error("存储失败: {0}")]
    Storage(#[from] PluginStorageError),
    #[error("归档非法: {0}")]
    Archive(String),
    #[error("dataset.json 非法: {0}")]
    InvalidManifest(String),
    #[error("数据包损坏: {0}")]
    Corrupt(String),
    #[error("格式版本 {found} 高于本宿主支持（最高 {supported}）")]
    FormatTooNew { found: u32, supported: u32 },
    #[error("归档摘要不匹配（期望 {expected}，实际 {actual}）——下载损坏或目录过期")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("IO 失败: {0}")]
    Io(String),
}

/// dataset.json 的 files 表项。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DatasetFileEntry {
    /// 文件内容 sha256（十六进制小写）。安装时逐文件核验——损坏包拒绝。
    pub sha256: String,
    /// 文件字节数（先比大小再算哈希，快速拒绝）。
    pub bytes: u64,
}

/// 数据包清单（dataset.json）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DatasetManifest {
    pub format_version: u32,
    /// 数据集 id（如 "dictionary-basic"）。字符集 [a-z0-9-]。
    pub dataset_id: String,
    /// 声明归属的插件 id（安装挂载命名空间，安装时校验合法字符集）。
    pub plugin_id: String,
    /// 数据包版本（版本目录名，字符集 [A-Za-z0-9.\-_]）。
    pub version: String,
    /// 构建剖面（basic/full 等，仅展示）。
    #[serde(default)]
    pub profile: Option<String>,
    /// 展示名。
    #[serde(default)]
    pub name: Option<String>,
    /// 数据来源与许可（可追溯性要求，phase §五.4.1）。
    pub source: DatasetSourceInfo,
    /// 包内文件清单（相对包根，含哈希与大小）。
    pub files: HashMap<String, DatasetFileEntry>,
    /// 构建内容摘要（行流哈希，重建可复验）。
    #[serde(default)]
    pub content_sha256: Option<String>,
    /// 条目计数（entries/phrases/forms/zh_terms 等，仅展示与核验）。
    #[serde(default)]
    pub counts: HashMap<String, u64>,
    #[serde(default)]
    pub built_at: Option<String>,
}

/// 数据来源信息（许可可追溯）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DatasetSourceInfo {
    pub name: String,
    #[serde(default)]
    pub url: Option<String>,
    /// 许可证名（如 "MIT"）。来源许可必须可追溯，构建期写入。
    pub license: String,
    /// 上游修订（commit/date 等）。
    #[serde(default)]
    pub revision: Option<String>,
}

/// active.json 的 wire 格式。
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
struct ActivePointer {
    version: String,
}

/// 数据包当前版本解析结果（readiness 与插件运行时共用语义）。
#[derive(Debug, Clone, PartialEq)]
pub enum ActiveVersion {
    /// 未挂载（无目录或无 active.json）——尚未安装。
    NotMounted,
    /// 指针存在但无效/版本目录缺失——损坏，重装修复。
    Corrupt,
    /// 已挂载到该版本。
    Mounted(String),
}

/// 解析数据集当前版本：`<base>/<dataset_id>/active.json` → 版本目录存在性复核。
/// base 为插件 dirs.datasets（v2 握手下发），dataset_id 为 manifest 声明。
pub fn resolve_active_version(base: &Path, dataset_id: &str) -> ActiveVersion {
    resolve_active_at(&base.join(dataset_id))
}

/// 同 [`resolve_active_version`]，直接以数据集挂载目录为入参。
pub fn resolve_active_at(dataset_dir: &Path) -> ActiveVersion {
    if !dataset_dir.is_dir() {
        return ActiveVersion::NotMounted;
    }
    let Ok(raw) = std::fs::read_to_string(dataset_dir.join("active.json")) else {
        return ActiveVersion::NotMounted;
    };
    let Ok(ptr) = serde_json::from_str::<ActivePointer>(&raw) else {
        return ActiveVersion::Corrupt;
    };
    if ptr.version.is_empty() || !valid_version_charset(&ptr.version) {
        return ActiveVersion::Corrupt;
    }
    if !dataset_dir
        .join(&ptr.version)
        .join("dataset.json")
        .is_file()
    {
        return ActiveVersion::Corrupt;
    }
    ActiveVersion::Mounted(ptr.version)
}

/// 数据包安装器。与 PluginInstaller 同存储根、同 op 纪律；锁独立（插件安装
/// 与数据包安装互不阻塞，二者写 registry 不同分片，last-writer-wins 由
/// "决策点重读"约束——见 plugin_storage 模块注释）。
pub struct DatasetInstaller {
    storage: PluginStorage,
    limits: ExtractLimits,
    op_lock: std::sync::Mutex<()>,
}

impl DatasetInstaller {
    pub fn new(storage: PluginStorage) -> Self {
        DatasetInstaller {
            storage,
            limits: ExtractLimits::default(),
            op_lock: std::sync::Mutex::new(()),
        }
    }

    /// 注入限额（测试用小限额验证拒绝路径）。
    pub fn with_limits(mut self, limits: ExtractLimits) -> Self {
        self.limits = limits;
        self
    }

    pub fn storage(&self) -> &PluginStorage {
        &self.storage
    }

    /// 从 zip 归档安装（官方目录下载在 0.28.5；当前为 dev 本地包通道）。
    pub fn install_zip(&self, zip_path: &Path) -> Result<DatasetRecord, DatasetError> {
        let _guard = self.op_lock.lock().unwrap();
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");
        let result = self.install_inner(
            |pkg| {
                archive::extract_zip_safely(zip_path, pkg, self.limits)
                    .map(|_| ())
                    .map_err(|e| DatasetError::Archive(e.to_string()))
            },
            &pkg_dir,
            InstallSource::LocalDev {
                origin: zip_path.to_string_lossy().into_owned(),
            },
        );
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// 从官方目录下载的 zip 归档安装（0.28.5 插件中心通道）：归档 sha256
    /// 先与目录条目核验（包内 dataset.json 的逐文件哈希校验仍独立执行）。
    pub fn install_official_zip(
        &self,
        zip_path: &Path,
        expected_sha256: &str,
        catalog_url: &str,
    ) -> Result<DatasetRecord, DatasetError> {
        let _guard = self.op_lock.lock().unwrap();
        if expected_sha256.len() != 64 || !expected_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(DatasetError::InvalidManifest(
                "目录条目 sha256 格式非法".into(),
            ));
        }
        let mut hasher = sha2::Sha256::new();
        let mut file = std::fs::File::open(zip_path).map_err(|e| DatasetError::Io(e.to_string()))?;
        std::io::copy(&mut file, &mut hasher)
            .map_err(|e| DatasetError::Io(e.to_string()))?;
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(DatasetError::ChecksumMismatch {
                expected: expected_sha256.to_lowercase(),
                actual,
            });
        }
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");
        let result = self.install_inner(
            |pkg| {
                archive::extract_zip_safely(zip_path, pkg, self.limits)
                    .map(|_| ())
                    .map_err(|e| DatasetError::Archive(e.to_string()))
            },
            &pkg_dir,
            InstallSource::Official {
                catalog_url: catalog_url.to_string(),
            },
        );
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// 从已解包目录安装（dev 模式；源目录不被移动/破坏）。
    pub fn install_dir(&self, dir: &Path) -> Result<DatasetRecord, DatasetError> {
        let _guard = self.op_lock.lock().unwrap();
        if !dir.join("dataset.json").is_file() {
            return Err(DatasetError::InvalidManifest(
                "目录根下缺少 dataset.json".into(),
            ));
        }
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");
        let result = self.install_inner(
            |pkg| {
                archive::copy_dir_bounded(dir, pkg, self.limits)
                    .map(|_| ())
                    .map_err(|e| DatasetError::Archive(e.to_string()))
            },
            &pkg_dir,
            InstallSource::LocalDev {
                origin: dir.to_string_lossy().into_owned(),
            },
        );
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// 暂存就位后的公共路径：校验 dataset.json → 核验文件 → move 版本目录 →
    /// 切 active.json（提交点）→ 记录 → 清旧版本。
    fn install_inner(
        &self,
        stage: impl FnOnce(&Path) -> Result<(), DatasetError>,
        pkg_dir: &Path,
        source: InstallSource,
    ) -> Result<DatasetRecord, DatasetError> {
        stage(pkg_dir)?;

        // 1. dataset.json 结构校验
        let manifest = read_dataset_manifest(&pkg_dir.join("dataset.json"))?;
        validate_manifest_shape(&manifest)?;

        // 2. 文件完整性：逐文件大小 + sha256（损坏包拒绝，不进挂载目录）
        verify_files(pkg_dir, &manifest)?;

        // 3. 提交版本目录（同版本重装先弃旧内容；rename 原子）
        let target = self.storage.dataset_version_dir(
            &manifest.plugin_id,
            &manifest.dataset_id,
            &manifest.version,
        );
        if target.exists() {
            let _ = std::fs::remove_dir_all(&target);
        }
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).map_err(|e| DatasetError::Io(e.to_string()))?;
        }
        std::fs::rename(pkg_dir, &target).map_err(|e| {
            DatasetError::Io(format!("提交数据包目录失败（{}）: {e}", target.display()))
        })?;

        // 4. active.json 指针切换 = 提交点（此前任何失败不影响旧有效版本）
        let active_path = self
            .storage
            .dataset_active_path(&manifest.plugin_id, &manifest.dataset_id);
        write_active_pointer(&active_path, &manifest.version)?;

        // 5. registry 记录（覆盖同 key 旧版本记录）
        let mut registry = self.storage.load_registry()?;
        let record = DatasetRecord {
            plugin_id: manifest.plugin_id.clone(),
            dataset_id: manifest.dataset_id.clone(),
            version: manifest.version.clone(),
            format_version: manifest.format_version,
            source,
            installed_at: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
            state: InstallState::Active,
            content_sha256: manifest.content_sha256.clone(),
            pending_paths: vec![],
        };
        registry.datasets.insert(record_key(&manifest.plugin_id, &manifest.dataset_id), record.clone());
        self.storage.save_registry(&registry)?;

        // 6. 清旧版本目录（best-effort：插件可能仍持有旧 DB 句柄，删不掉
        //    留作孤儿，由 recover 按 active 指针重试）
        clean_stale_versions(
            &self.storage.dataset_dir(&manifest.plugin_id, &manifest.dataset_id),
            &manifest.version,
        );

        tracing::info!(
            plugin = %manifest.plugin_id,
            dataset = %manifest.dataset_id,
            version = %manifest.version,
            files = manifest.files.len(),
            "数据包安装已提交"
        );
        Ok(record)
    }

    /// 卸载数据集：改记录 → 删目录。删除失败（Windows 占用）标 PendingRemove。
    pub fn uninstall(
        &self,
        plugin_id: &str,
        dataset_id: &str,
    ) -> Result<UninstallDatasetOutcome, DatasetError> {
        let _guard = self.op_lock.lock().unwrap();
        let mut registry = self.storage.load_registry()?;
        let key = record_key(plugin_id, dataset_id);
        if !registry.datasets.contains_key(&key) {
            // 文件系统可能仍有挂载（自愈前的孤儿）——即使无记录也尝试删目录，
            // 但结果如实报告（不谎报"未安装就没文件"）。
            let dir = self.storage.dataset_dir(plugin_id, dataset_id);
            let failed = self.storage.delete_paths_best_effort(&[dir]);
            if failed.is_empty() {
                return Ok(UninstallDatasetOutcome::Removed);
            }
            return Ok(UninstallDatasetOutcome::RemovedPendingFiles { pending_paths: failed });
        }

        let mut to_delete: Vec<PathBuf> = vec![self.storage.dataset_dir(plugin_id, dataset_id)];
        let rec = registry.datasets.get_mut(&key).expect("上方已确认存在");
        to_delete.extend(rec.pending_paths.iter().map(PathBuf::from));
        let failed = self.storage.delete_paths_best_effort(&to_delete);
        if failed.is_empty() {
            registry.datasets.remove(&key);
            self.storage.save_registry(&registry)?;
            tracing::info!(plugin = plugin_id, dataset = dataset_id, "数据包已卸载");
            Ok(UninstallDatasetOutcome::Removed)
        } else {
            rec.state = InstallState::PendingRemove;
            rec.pending_paths = failed
                .iter()
                .map(|p| p.to_string_lossy().into_owned())
                .collect();
            self.storage.save_registry(&registry)?;
            tracing::warn!(
                plugin = plugin_id,
                dataset = dataset_id,
                count = failed.len(),
                "数据包卸载部分文件被占用，标记待清理"
            );
            Ok(UninstallDatasetOutcome::RemovedPendingFiles { pending_paths: failed })
        }
    }

    /// 列出全部数据包记录（展示用）。
    pub fn list(&self) -> Result<Vec<DatasetRecord>, DatasetError> {
        Ok(self
            .storage
            .load_registry()?
            .datasets
            .into_values()
            .collect())
    }

    /// 启动恢复：重试 PendingRemove、清孤儿版本目录、自愈无记录的已挂载数据集。
    pub fn recover(&self) -> DatasetRecoveryReport {
        let mut report = DatasetRecoveryReport::default();
        let Ok(mut registry) = self.storage.load_registry() else {
            return report;
        };

        // 1. PendingRemove 重试删除
        let keys: Vec<String> = registry.datasets.keys().cloned().collect();
        for key in keys {
            let Some(rec) = registry.datasets.get(&key).cloned() else {
                continue;
            };
            if rec.state != InstallState::PendingRemove {
                continue;
            }
            let mut to_delete: Vec<PathBuf> = rec
                .pending_paths
                .iter()
                .map(PathBuf::from)
                .collect();
            to_delete.push(self.storage.dataset_dir(&rec.plugin_id, &rec.dataset_id));
            let failed = self.storage.delete_paths_best_effort(&to_delete);
            if failed.is_empty() {
                registry.datasets.remove(&key);
                report.pending_removed += 1;
            } else if let Some(r) = registry.datasets.get_mut(&key) {
                r.pending_paths = failed
                    .iter()
                    .map(|p| p.to_string_lossy().into_owned())
                    .collect();
                report.still_pending += failed.len();
            }
        }

        // 2+3. 遍历挂载目录：清非 active 版本的孤儿目录；无记录但 active
        //     有效的自愈重建（提交点在 active.json 之后、registry 之前崩溃）。
        let Ok(pids) = std::fs::read_dir(self.storage.root().join("datasets")) else {
            let _ = self.storage.save_registry(&registry);
            return report;
        };
        for pid_entry in pids.flatten() {
            let pid = pid_entry.file_name().to_string_lossy().into_owned();
            let Ok(dids) = std::fs::read_dir(pid_entry.path()) else {
                continue;
            };
            for did_entry in dids.flatten() {
                let did = did_entry.file_name().to_string_lossy().into_owned();
                let dir = did_entry.path();
                match resolve_active_at(&dir) {
                    ActiveVersion::Mounted(active_v) => {
                        report.orphan_dirs_cleaned += clean_stale_versions(&dir, &active_v);
                        let key = record_key(&pid, &did);
                        if !registry.datasets.contains_key(&key)
                            && let Ok(manifest) =
                                read_dataset_manifest(&dir.join(&active_v).join("dataset.json"))
                        {
                            registry.datasets.insert(
                                key,
                                DatasetRecord {
                                    plugin_id: pid.clone(),
                                    dataset_id: did.clone(),
                                    version: active_v,
                                    format_version: manifest.format_version,
                                    source: InstallSource::LocalDev {
                                        origin: "recovered（安装记录提交点崩溃后自愈）".into(),
                                    },
                                    installed_at: 0,
                                    state: InstallState::Active,
                                    content_sha256: manifest.content_sha256,
                                    pending_paths: vec![],
                                },
                            );
                            report.adopted += 1;
                        }
                    }
                    ActiveVersion::Corrupt => {
                        // 指针损坏：不动文件（插件运行时按 corrupt 如实报错，
                        // 重装走 install 覆盖路径），恢复不越权猜测。
                    }
                    ActiveVersion::NotMounted => {
                        // 有目录但无指针 = active.json 写入前崩溃 → 整目录可弃。
                        let failed = self.storage.delete_paths_best_effort(&[dir.clone()]);
                        if failed.is_empty() {
                            report.orphan_dirs_cleaned += 1;
                        }
                    }
                }
            }
        }
        let _ = self.storage.save_registry(&registry);
        if report.pending_removed > 0
            || report.orphan_dirs_cleaned > 0
            || report.adopted > 0
            || report.still_pending > 0
        {
            tracing::info!(?report, "数据包存储恢复完成");
        }
        report
    }
}

/// 卸载数据集结果。
#[derive(Debug, Clone, PartialEq)]
pub enum UninstallDatasetOutcome {
    Removed,
    RemovedPendingFiles { pending_paths: Vec<PathBuf> },
}

/// 启动恢复报告。
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DatasetRecoveryReport {
    pub pending_removed: usize,
    pub orphan_dirs_cleaned: usize,
    /// 自愈重建记录数（提交点崩溃残留）。
    pub adopted: usize,
    pub still_pending: usize,
}

fn record_key(plugin_id: &str, dataset_id: &str) -> String {
    format!("{plugin_id}/{dataset_id}")
}

fn next_op_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("ds-{nanos:x}")
}

/// dataset.json 读取 + 结构校验。
fn read_dataset_manifest(path: &Path) -> Result<DatasetManifest, DatasetError> {
    let raw = std::fs::read_to_string(path)
        .map_err(|e| DatasetError::Io(format!("读取 dataset.json 失败: {e}")))?;
    let manifest: DatasetManifest = serde_json::from_str(&raw).map_err(|e| {
        DatasetError::InvalidManifest(format!("dataset.json 解析失败: {e}"))
    })?;
    Ok(manifest)
}

/// 形状校验：格式版本上限、id/版本字符集、files 非空且无路径穿越。
fn validate_manifest_shape(m: &DatasetManifest) -> Result<(), DatasetError> {
    if m.format_version > SUPPORTED_DATASET_FORMAT_VERSION {
        return Err(DatasetError::FormatTooNew {
            found: m.format_version,
            supported: SUPPORTED_DATASET_FORMAT_VERSION,
        });
    }
    let valid_dataset_id = !m.dataset_id.is_empty()
        && m.dataset_id.len() <= 64
        && m
            .dataset_id
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_dataset_id {
        return Err(DatasetError::InvalidManifest(format!(
            "dataset_id 非法: {:?}（允许 [a-z0-9-]，长度 1..=64）",
            m.dataset_id
        )));
    }
    if validate_plugin_id(&m.plugin_id).is_err() {
        return Err(DatasetError::InvalidManifest(format!(
            "plugin_id 非法: {:?}",
            m.plugin_id
        )));
    }
    if !valid_version_charset(&m.version) {
        return Err(DatasetError::InvalidManifest(format!(
            "version 非法: {:?}（允许 [A-Za-z0-9._-]，长度 1..=32）",
            m.version
        )));
    }
    if m.files.is_empty() {
        return Err(DatasetError::InvalidManifest(
            "files 为空——数据包必须至少声明一个载荷文件".into(),
        ));
    }
    for name in m.files.keys() {
        let path = Path::new(name);
        if name.is_empty()
            || path.is_absolute()
            || path
                .components()
                .any(|c| !matches!(c, std::path::Component::Normal(_)))
        {
            return Err(DatasetError::InvalidManifest(format!(
                "files 声明非法路径: {name:?}"
            )));
        }
    }
    Ok(())
}

/// 版本目录名字符集（同时用于 active.json 指针校验）。
fn valid_version_charset(v: &str) -> bool {
    !v.is_empty()
        && v.len() <= 32
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-')
}

/// 逐文件核验大小 + sha256。
fn verify_files(pkg_root: &Path, m: &DatasetManifest) -> Result<(), DatasetError> {
    use sha2::Digest;
    for (name, entry) in &m.files {
        let path = pkg_root.join(name);
        let meta = std::fs::metadata(&path).map_err(|e| {
            DatasetError::Corrupt(format!("包内文件缺失 {name}: {e}"))
        })?;
        if !meta.is_file() {
            return Err(DatasetError::Corrupt(format!(
                "包内声明不是常规文件: {name}"
            )));
        }
        if meta.len() != entry.bytes {
            return Err(DatasetError::Corrupt(format!(
                "文件大小不符 {name}：声明 {}，实际 {}",
                entry.bytes,
                meta.len()
            )));
        }
        let mut file =
            std::fs::File::open(&path).map_err(|e| DatasetError::Io(e.to_string()))?;
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut file, &mut hasher)
            .map_err(|e| DatasetError::Io(format!("读取 {name} 失败: {e}")))?;
        let actual = format!("{:x}", hasher.finalize());
        if !actual.eq_ignore_ascii_case(&entry.sha256) {
            return Err(DatasetError::Corrupt(format!(
                "文件校验失败 {name}：声明 sha256 {}，实际 {actual}",
                entry.sha256
            )));
        }
    }
    Ok(())
}

/// active.json 原子写（tmp + rename；Windows rename 不覆盖，先删旧）。
fn write_active_pointer(path: &Path, version: &str) -> Result<(), DatasetError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| DatasetError::Io(e.to_string()))?;
    }
    let tmp = path.with_extension("json.tmp");
    let body = serde_json::to_string(&ActivePointer {
        version: version.to_string(),
    })
    .map_err(|e| DatasetError::Io(e.to_string()))?;
    std::fs::write(&tmp, body).map_err(|e| DatasetError::Io(e.to_string()))?;
    if path.exists() {
        let _ = std::fs::remove_file(path);
    }
    std::fs::rename(&tmp, path).map_err(|e| {
        DatasetError::Io(format!("切换 active.json 失败（{}）: {e}", path.display()))
    })?;
    Ok(())
}

/// 清理非 keep_version 的兄弟版本目录，返回删净的数量（失败留待下次）。
fn clean_stale_versions(dataset_dir: &Path, keep_version: &str) -> usize {
    let Ok(entries) = std::fs::read_dir(dataset_dir) else {
        return 0;
    };
    let mut cleaned = 0;
    for entry in entries.flatten() {
        let p = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "active.json" || name == keep_version || !p.is_dir() {
            continue;
        }
        if std::fs::remove_dir_all(&p).is_ok() {
            cleaned += 1;
        } else {
            tracing::warn!(path = %p.display(), "数据包旧版本清理失败（可能被占用），留待下次");
        }
    }
    cleaned
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installer(root: &Path) -> DatasetInstaller {
        DatasetInstaller::new(PluginStorage::new(root.to_path_buf()))
    }

    /// 构造一个最小合法数据包目录。
    fn write_pkg(dir: &Path, plugin_id: &str, dataset_id: &str, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("dict.db"), b"sqlite-bytes-v1").unwrap();
        let sha = {
            use sha2::Digest;
            let mut h = sha2::Sha256::new();
            h.update(b"sqlite-bytes-v1");
            format!("{:x}", h.finalize())
        };
        std::fs::write(
            dir.join("dataset.json"),
            serde_json::json!({
                "format_version": 1,
                "dataset_id": dataset_id,
                "plugin_id": plugin_id,
                "version": version,
                "source": {"name": "ECDICT", "license": "MIT"},
                "content_sha256": "deadbeef",
                "counts": {"entries": 1},
                "files": {"dict.db": {"sha256": sha, "bytes": 15}},
            })
            .to_string(),
        )
        .unwrap();
    }

    #[test]
    fn install_commits_version_and_switches_active() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "builtin.dictionary", "dictionary-basic", "1.0.0");

        let record = inst.install_dir(&pkg).unwrap();
        assert_eq!(record.state, InstallState::Active);
        let base = inst.storage().datasets_dir("builtin.dictionary");
        assert_eq!(
            resolve_active_version(&base, "dictionary-basic"),
            ActiveVersion::Mounted("1.0.0".into())
        );
        assert!(inst
            .storage()
            .dataset_version_dir("builtin.dictionary", "dictionary-basic", "1.0.0")
            .join("dict.db")
            .is_file());
        assert_eq!(inst.list().unwrap().len(), 1);
    }

    /// 把数据包目录打成 zip（dataset.json 在根，供 official 通道测试）。
    fn make_zip(pkg_dir: &Path, zip_path: &Path) {
        let file = std::fs::File::create(zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for entry in ["dataset.json", "dict.db"] {
            let content = std::fs::read(pkg_dir.join(entry)).unwrap();
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file(entry, opts).unwrap();
            std::io::Write::write_all(&mut zip, &content).unwrap();
        }
        zip.finish().unwrap();
    }

    fn sha256_of(path: &Path) -> String {
        let mut hasher = sha2::Sha256::new();
        std::io::copy(&mut std::fs::File::open(path).unwrap(), &mut hasher).unwrap();
        format!("{:x}", hasher.finalize())
    }

    #[test]
    fn official_zip_verifies_sha_and_records_source() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        let pkg = tmp.path().join("pkg");
        write_pkg(&pkg, "builtin.dictionary", "dictionary-basic", "1.0.0");
        let zip_path = tmp.path().join("ds.zip");
        make_zip(&pkg, &zip_path);

        let real_sha = sha256_of(&zip_path);
        assert!(matches!(
            inst.install_official_zip(&zip_path, &"f".repeat(64), "https://example.invalid/c.json"),
            Err(DatasetError::ChecksumMismatch { .. })
        ));
        assert!(inst.list().unwrap().is_empty());

        let record = inst
            .install_official_zip(&zip_path, &real_sha, "https://example.invalid/c.json")
            .unwrap();
        assert_eq!(
            record.source,
            InstallSource::Official {
                catalog_url: "https://example.invalid/c.json".into()
            }
        );
        let base = inst.storage().datasets_dir("builtin.dictionary");
        assert_eq!(
            resolve_active_version(&base, "dictionary-basic"),
            ActiveVersion::Mounted("1.0.0".into())
        );
    }

    #[test]
    fn upgrade_keeps_old_active_until_commit_and_cleans_after() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_pkg(&tmp.path().join("v1"), "builtin.dictionary", "d", "1.0.0");
        inst.install_dir(&tmp.path().join("v1")).unwrap();

        // 升级到 2.0.0：提交后 active 指向新版本，旧版本目录被清
        write_pkg(&tmp.path().join("v2"), "builtin.dictionary", "d", "2.0.0");
        let record = inst.install_dir(&tmp.path().join("v2")).unwrap();
        assert_eq!(record.version, "2.0.0");
        let base = inst.storage().datasets_dir("builtin.dictionary");
        assert_eq!(resolve_active_version(&base, "d"), ActiveVersion::Mounted("2.0.0".into()));
        assert!(!inst
            .storage()
            .dataset_version_dir("builtin.dictionary", "d", "1.0.0")
            .exists());
        // 记录覆盖为唯一活动版本
        assert_eq!(inst.list().unwrap().len(), 1);
    }

    #[test]
    fn corrupted_file_hash_rejected_and_old_version_stays() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_pkg(&tmp.path().join("v1"), "p.x", "d", "1.0.0");
        inst.install_dir(&tmp.path().join("v1")).unwrap();

        // 声明的 sha256 与实际内容不符 → 拒绝，不影响已装版本
        let mut manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(tmp.path().join("v1").join("dataset.json")).unwrap(),
        )
        .unwrap();
        manifest["files"]["dict.db"]["sha256"] = "00".repeat(32).into();
        let bad = tmp.path().join("bad");
        std::fs::create_dir_all(&bad).unwrap();
        std::fs::write(bad.join("dict.db"), b"sqlite-bytes-v1").unwrap();
        std::fs::write(bad.join("dataset.json"), manifest.to_string()).unwrap();

        assert!(matches!(
            inst.install_dir(&bad),
            Err(DatasetError::Corrupt(_))
        ));
        let base = inst.storage().datasets_dir("p.x");
        assert_eq!(resolve_active_version(&base, "d"), ActiveVersion::Mounted("1.0.0".into()));
    }

    #[test]
    fn format_too_new_and_bad_ids_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_pkg(&tmp.path().join("new"), "p.x", "d", "1.0.0");
        let path = tmp.path().join("new").join("dataset.json");
        let mut m: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        m["format_version"] = 99u32.into();
        std::fs::write(&path, m.to_string()).unwrap();
        assert!(matches!(
            inst.install_dir(&tmp.path().join("new")),
            Err(DatasetError::FormatTooNew { .. })
        ));

        write_pkg(&tmp.path().join("badid"), "p.x", "Bad_ID!", "1.0.0");
        assert!(matches!(
            inst.install_dir(&tmp.path().join("badid")),
            Err(DatasetError::InvalidManifest(_))
        ));
    }

    #[test]
    fn uninstall_and_recover_semantics() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_pkg(&tmp.path().join("v1"), "p.x", "d", "1.0.0");
        inst.install_dir(&tmp.path().join("v1")).unwrap();

        // 卸载：目录删净、记录移除
        assert_eq!(
            inst.uninstall("p.x", "d").unwrap(),
            UninstallDatasetOutcome::Removed
        );
        assert!(inst.list().unwrap().is_empty());
        assert!(!inst.storage().dataset_dir("p.x", "d").exists());

        // 无 active 指针的孤儿目录（active 写入前崩溃）→ recover 清掉
        write_pkg(&tmp.path().join("v2"), "p.x", "e", "1.0.0");
        let base = inst.storage().datasets_dir("p.x");
        std::fs::create_dir_all(&base).unwrap();
        std::fs::rename(tmp.path().join("v2"), inst.storage().dataset_dir("p.x", "e")).unwrap();
        let report = inst.recover();
        assert_eq!(report.orphan_dirs_cleaned, 1);
        assert!(!inst.storage().dataset_dir("p.x", "e").exists());

        // 有 active 但无记录（registry 写入前崩溃）→ recover 自愈重建
        write_pkg(&tmp.path().join("v3"), "p.x", "f", "1.0.0");
        let dir = inst.storage().dataset_dir("p.x", "f");
        let version_dir = dir.join("1.0.0");
        std::fs::create_dir_all(&version_dir).unwrap();
        std::fs::rename(
            tmp.path().join("v3").join("dataset.json"),
            version_dir.join("dataset.json"),
        )
        .unwrap();
        std::fs::rename(
            tmp.path().join("v3").join("dict.db"),
            version_dir.join("dict.db"),
        )
        .unwrap();
        write_active_pointer(&dir.join("active.json"), "1.0.0").unwrap();
        let report = inst.recover();
        assert_eq!(report.adopted, 1);
        assert_eq!(inst.list().unwrap().len(), 1);
    }

    #[test]
    fn resolve_active_distinguishes_missing_and_corrupt() {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path();
        // 无目录 → NotMounted
        assert_eq!(resolve_active_version(base, "nope"), ActiveVersion::NotMounted);
        // 目录存在无指针 → NotMounted（未完成安装）
        std::fs::create_dir_all(base.join("half")).unwrap();
        assert_eq!(resolve_active_version(base, "half"), ActiveVersion::NotMounted);
        // 指针指向不存在的版本 → Corrupt
        write_active_pointer(&base.join("half").join("active.json"), "9.9.9").unwrap();
        assert_eq!(resolve_active_version(base, "half"), ActiveVersion::Corrupt);
        // 指针非法 JSON → Corrupt
        std::fs::write(base.join("half").join("active.json"), "{ nope").unwrap();
        assert_eq!(resolve_active_version(base, "half"), ActiveVersion::Corrupt);
    }
}
