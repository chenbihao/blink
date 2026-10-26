//! 插件存储（0.28.1 §五.2）——安装记录与包/数据/缓存/词库分目录的唯一真源。
//!
//! 目录布局（root 默认 `%APPDATA%\blink\plugins\`，测试可注入临时根）：
//!
//! ```text
//! plugins\
//!   registry.json                          # 安装记录真源（原子写）
//!   installed\<plugin_id>\<version>\       # 版本化安装内容（提交后不可变）
//!   data\<plugin_id>\                      # 用户数据（卸载默认保留）
//!   cache\<plugin_id>\                     # 可重建缓存（卸载直接删）
//!   datasets\<plugin_id>\<dataset>\        # 数据包挂载点（版本化，0.28.4 消费）
//!   staging\<op_id>\                       # 暂存区（崩溃残留由 recover 清理）
//! ```
//!
//! **跨进程约定**（GUI 与 CLI 同读安装记录）：registry.json 只经
//! [`PluginStorage::save_registry`] 原子写（tmp + rename）；读取方在发布/
//! 执行等决策点必须重新读盘，不得缓存跨进程使用的记录快照。无跨进程
//! 文件锁——写入冲突按 last-writer-wins，由"决策点重读"限制影响面。
//!
//! 业务规则（来源校验/兼容性/状态机）在 domain/plugin/install.rs；
//! 本模块只管布局、IO 与恢复原语。

pub mod archive;
pub mod pages;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 安装记录文件名（root 下）。
const REGISTRY_FILE: &str = "registry.json";

/// 安装来源（0.28.1 仅本地开发包；官方目录在 0.28.5 扩展）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
pub enum InstallSource {
    /// 本地开发包（dev 模式调试用；生产不接任意来源安装）。
    LocalDev {
        /// 安装时的归档/目录路径（仅记录溯源，不作为运行时依赖）。
        origin: String,
    },
}

/// 安装状态。
///
/// `enabled` 不在这里——启用开关的真源是 SQLite `plugin:{id}` 配置
/// （PluginConfig.enabled），安装记录只管"装没装、装的什么"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InstallState {
    /// 已安装且记录有效。
    Active,
    /// 已标记卸载但文件未能全部删除（Windows 占用）——下次启动 recover 重试。
    PendingRemove,
}

/// 单个已安装插件的持久记录。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstallRecord {
    pub plugin_id: String,
    /// 本次安装的包版本（对应 installed/<id>/<version>/ 目录名）。
    pub version: String,
    pub source: InstallSource,
    /// 安装时间（unix epoch 秒，仅展示用）。
    pub installed_at: i64,
    pub state: InstallState,
    /// 包内容摘要（归档 sha256；目录安装时为 None）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
    /// manifest 的 schema_version（快照，诊断"升级后记录漂移"用）。
    pub manifest_schema_version: u32,
    /// manifest 的 protocol_version（快照）。
    pub protocol_version: u32,
    /// PendingRemove 时未能删除的路径（下次启动重试；全部删净后清空）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_paths: Vec<String>,
}

/// 安装记录集合（registry.json 的 wire 格式）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InstallRegistry {
    /// 格式版本（未来 schema 变更时迁移依据）。
    pub format_version: u32,
    pub records: HashMap<String, InstallRecord>,
    /// 数据包安装记录（0.28.4）——key 为 `"<plugin_id>/<dataset_id>"`。
    /// serde default：旧 registry.json 无此字段时按空表读入，向后兼容。
    #[serde(default)]
    pub datasets: HashMap<String, DatasetRecord>,
}

impl InstallRegistry {
    pub const FORMAT_VERSION: u32 = 1;

    pub fn new() -> Self {
        InstallRegistry {
            format_version: Self::FORMAT_VERSION,
            records: HashMap::new(),
            datasets: HashMap::new(),
        }
    }
}

/// 单个数据包的持久记录（0.28.4）——词库等可独立版本化的只读数据。
///
/// 运行时真源是数据集目录下的 `active.json`（插件进程按它解析当前版本）；
/// 本记录承载安装溯源/版本展示/占用待删，与插件本体记录分开。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct DatasetRecord {
    pub plugin_id: String,
    pub dataset_id: String,
    /// 本次安装的数据包版本（对应 datasets/<pid>/<did>/<version>/ 目录名）。
    pub version: String,
    /// dataset.json 的 format_version（快照，诊断用）。
    pub format_version: u32,
    pub source: InstallSource,
    pub installed_at: i64,
    pub state: InstallState,
    /// dataset.json 的 content_sha256（构建内容摘要，溯源用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content_sha256: Option<String>,
    /// PendingRemove 时未能删除的路径（下次启动重试）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending_paths: Vec<String>,
}

/// 插件存储根句柄。`new` 不做 IO（不建目录）；首次写入前由调用方
/// `ensure_layout` 或具体写操作建目录。
#[derive(Debug, Clone)]
pub struct PluginStorage {
    root: PathBuf,
}

impl PluginStorage {
    /// 生产根目录：`%APPDATA%\blink\plugins\`。
    pub fn production_root() -> Self {
        PluginStorage {
            root: crate::utils::paths::app_data_dir().join("plugins"),
        }
    }

    /// 注入根（测试/隔离模式必须走这里，禁止落入真实 AppData）。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        PluginStorage { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn registry_path(&self) -> PathBuf {
        self.root.join(REGISTRY_FILE)
    }

    pub fn installed_root(&self) -> PathBuf {
        self.root.join("installed")
    }

    /// 某插件某版本的安装内容目录（不可变，提交后不改名）。
    pub fn version_dir(&self, plugin_id: &str, version: &str) -> PathBuf {
        self.installed_root().join(plugin_id).join(version)
    }

    /// 插件用户数据目录（卸载默认保留）。
    pub fn data_dir(&self, plugin_id: &str) -> PathBuf {
        self.root.join("data").join(plugin_id)
    }

    /// 插件缓存目录（可重建，卸载直接删）。
    pub fn cache_dir(&self, plugin_id: &str) -> PathBuf {
        self.root.join("cache").join(plugin_id)
    }

    /// 插件数据包根目录（v2 握手下发给插件；包内容 0.28.4 落地）。
    pub fn datasets_dir(&self, plugin_id: &str) -> PathBuf {
        self.root.join("datasets").join(plugin_id)
    }

    /// 单个数据集的挂载目录：`datasets/<plugin_id>/<dataset_id>/`，
    /// 其下为版本子目录 + `active.json`（指向当前版本）。
    pub fn dataset_dir(&self, plugin_id: &str, dataset_id: &str) -> PathBuf {
        self.datasets_dir(plugin_id).join(dataset_id)
    }

    /// 数据集某版本的内容目录（提交后不可变）。
    pub fn dataset_version_dir(
        &self,
        plugin_id: &str,
        dataset_id: &str,
        version: &str,
    ) -> PathBuf {
        self.dataset_dir(plugin_id, dataset_id).join(version)
    }

    /// 数据集当前版本指针文件（active.json，安装提交点原子切换）。
    pub fn dataset_active_path(&self, plugin_id: &str, dataset_id: &str) -> PathBuf {
        self.dataset_dir(plugin_id, dataset_id).join("active.json")
    }

    /// 暂存区子目录（每次安装操作独立 op 目录，崩溃后整体可弃）。
    pub fn staging_dir(&self, op_id: &str) -> PathBuf {
        self.root.join("staging").join(op_id)
    }

    /// 建齐布局目录（幂等）。
    pub fn ensure_layout(&self) -> std::io::Result<()> {
        for dir in [
            self.installed_root(),
            self.root.join("data"),
            self.root.join("cache"),
            self.root.join("datasets"),
            self.root.join("staging"),
        ] {
            std::fs::create_dir_all(dir)?;
        }
        Ok(())
    }

    /// 读取安装记录。文件不存在 → 空记录（首装）；损坏 → Err（调用方
    /// 决策：不静默当空配置覆盖——恢复语义见 install.rs）。
    pub fn load_registry(&self) -> Result<InstallRegistry, PluginStorageError> {
        let path = self.registry_path();
        if !path.is_file() {
            return Ok(InstallRegistry::new());
        }
        let raw = std::fs::read_to_string(&path)
            .map_err(|e| PluginStorageError::Io { path: path.clone(), detail: e.to_string() })?;
        let reg: InstallRegistry = serde_json::from_str(&raw).map_err(|e| {
            PluginStorageError::CorruptRegistry {
                path: path.clone(),
                detail: e.to_string(),
            }
        })?;
        if reg.format_version > InstallRegistry::FORMAT_VERSION {
            return Err(PluginStorageError::RegistryTooNew {
                path,
                version: reg.format_version,
            });
        }
        Ok(reg)
    }

    /// 原子写安装记录（tmp + rename）。Windows 上 rename 不覆盖已存在
    /// 目标，先删旧文件——删除与 rename 之间崩溃最多丢一次更新，文件
    /// 不会半写（tmp 丢弃、旧文件已删则视同记录丢失，installed/ 目录
    /// 仍是真源可重建）。
    pub fn save_registry(&self, registry: &InstallRegistry) -> Result<(), PluginStorageError> {
        self.ensure_layout().map_err(|e| PluginStorageError::Io {
            path: self.root.clone(),
            detail: e.to_string(),
        })?;
        let path = self.registry_path();
        let tmp = path.with_extension("json.tmp");
        let body = serde_json::to_string_pretty(registry).map_err(|e| {
            PluginStorageError::Io {
                path: path.clone(),
                detail: e.to_string(),
            }
        })?;
        std::fs::write(&tmp, body).map_err(|e| PluginStorageError::Io {
            path: tmp.clone(),
            detail: e.to_string(),
        })?;
        if path.exists() {
            let _ = std::fs::remove_file(&path);
        }
        std::fs::rename(&tmp, &path).map_err(|e| PluginStorageError::Io {
            path: path.clone(),
            detail: e.to_string(),
        })?;
        Ok(())
    }

    /// 启动恢复：清空 staging（暂存内容不保证提交，整体可弃）。
    ///
    /// 返回被清理的 op 目录数。staging 不存在时静默 0。
    pub fn cleanup_staging(&self) -> usize {
        let staging = self.root.join("staging");
        let Ok(entries) = std::fs::read_dir(&staging) else {
            return 0;
        };
        let mut cleaned = 0;
        for entry in entries.flatten() {
            let p = entry.path();
            if std::fs::remove_dir_all(&p).is_ok() {
                cleaned += 1;
            } else {
                tracing::warn!(path = %p.display(), "staging 残留清理失败（下个启动重试）");
            }
        }
        if cleaned > 0 {
            tracing::info!(count = cleaned, "已清理插件安装暂存残留");
        }
        cleaned
    }

    /// 列出某插件目录下**未被记录**的孤儿版本目录（提交点崩溃的残留：
    /// 内容目录已 move 但 registry 未写，或旧版本目录未及删除）。
    ///
    /// 只按目录名比对（版本字符串）；`keep_version` 为记录中的活动版本。
    pub fn orphan_version_dirs(
        &self,
        plugin_id: &str,
        keep_version: &str,
    ) -> Vec<PathBuf> {
        let dir = self.installed_root().join(plugin_id);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Vec::new();
        };
        entries
            .flatten()
            .map(|e| e.path())
            .filter(|p| {
                p.is_dir() && p.file_name().map(|n| n != keep_version).unwrap_or(true)
            })
            .collect()
    }

    /// 删除路径并收集失败项（Windows 占用不假报成功）。
    /// 目录用 remove_dir_all，文件用 remove_file；返回全部删除失败的路径。
    pub fn delete_paths_best_effort(&self, paths: &[PathBuf]) -> Vec<PathBuf> {
        paths
            .iter()
            .filter(|p| {
                if !p.exists() {
                    return false;
                }
                let ok = if p.is_dir() {
                    std::fs::remove_dir_all(p).is_ok()
                } else {
                    std::fs::remove_file(p).is_ok()
                };
                if !ok {
                    tracing::warn!(path = %p.display(), "插件文件删除失败（可能被占用），记入待清理");
                }
                !ok
            })
            .cloned()
            .collect()
    }
}

/// 存储层错误（domain install.rs 映射为安装错误）。
#[derive(Debug, thiserror::Error)]
pub enum PluginStorageError {
    #[error("IO 失败 {path:?}: {detail}")]
    Io { path: PathBuf, detail: String },
    #[error("安装记录损坏 {path:?}: {detail}（不覆盖，请人工检查或删除后重装）")]
    CorruptRegistry { path: PathBuf, detail: String },
    #[error("安装记录格式版本 {version} 高于本宿主支持（请升级 Blink）")]
    RegistryTooNew { path: PathBuf, version: u32 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn registry_roundtrip_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        // 文件不存在 → 空记录
        assert_eq!(storage.load_registry().unwrap(), InstallRegistry::new());

        let mut reg = InstallRegistry::new();
        reg.records.insert(
            "extra.dict".into(),
            InstallRecord {
                plugin_id: "extra.dict".into(),
                version: "1.0.0".into(),
                source: InstallSource::LocalDev {
                    origin: "D:/pkg.zip".into(),
                },
                installed_at: 42,
                state: InstallState::Active,
                sha256: Some("ab".repeat(32)),
                manifest_schema_version: 1,
                protocol_version: 2,
                pending_paths: vec![],
            },
        );
        storage.save_registry(&reg).unwrap();
        assert_eq!(storage.load_registry().unwrap(), reg);
        // tmp 文件不残留
        assert!(!storage.registry_path().with_extension("json.tmp").exists());
    }

    #[test]
    fn corrupt_registry_is_error_not_empty() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        storage.ensure_layout().unwrap();
        std::fs::write(storage.registry_path(), "{ not json").unwrap();
        assert!(matches!(
            storage.load_registry(),
            Err(PluginStorageError::CorruptRegistry { .. })
        ));
    }

    #[test]
    fn legacy_registry_without_datasets_field_parses() {
        // 0.28.4 前的 registry.json 无 datasets 字段 → 空表读入，不报错。
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        storage.ensure_layout().unwrap();
        std::fs::write(
            storage.registry_path(),
            r#"{"format_version":1,"records":{}}"#,
        )
        .unwrap();
        let reg = storage.load_registry().unwrap();
        assert!(reg.datasets.is_empty());

        // dataset 记录 roundtrip + 路径助手
        let mut reg2 = reg.clone();
        reg2.datasets.insert(
            "builtin.dictionary/dictionary-basic".into(),
            DatasetRecord {
                plugin_id: "builtin.dictionary".into(),
                dataset_id: "dictionary-basic".into(),
                version: "1.0.0".into(),
                format_version: 1,
                source: InstallSource::LocalDev { origin: "D:/pkg".into() },
                installed_at: 42,
                state: InstallState::Active,
                content_sha256: Some("ab".repeat(32)),
                pending_paths: vec![],
            },
        );
        storage.save_registry(&reg2).unwrap();
        assert_eq!(storage.load_registry().unwrap(), reg2);
        assert_eq!(
            storage.dataset_version_dir("builtin.dictionary", "dictionary-basic", "1.0.0"),
            tmp.path()
                .join("datasets")
                .join("builtin.dictionary")
                .join("dictionary-basic")
                .join("1.0.0")
        );
        assert_eq!(
            storage.dataset_active_path("builtin.dictionary", "dictionary-basic"),
            tmp.path()
                .join("datasets")
                .join("builtin.dictionary")
                .join("dictionary-basic")
                .join("active.json")
        );
    }

    #[test]
    fn staging_cleanup_and_orphan_detection() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        storage.ensure_layout().unwrap();
        std::fs::create_dir_all(storage.staging_dir("op1").join("pkg")).unwrap();
        std::fs::create_dir_all(storage.staging_dir("op2")).unwrap();
        assert_eq!(storage.cleanup_staging(), 2);
        assert!(!storage.staging_dir("op1").exists());

        // installed 下 1.0.0 是记录版本，1.1.0 与 stray 是孤儿
        std::fs::create_dir_all(storage.version_dir("p.x", "1.0.0")).unwrap();
        std::fs::create_dir_all(storage.version_dir("p.x", "1.1.0")).unwrap();
        std::fs::create_dir_all(storage.version_dir("p.x", "stray")).unwrap();
        let orphans = storage.orphan_version_dirs("p.x", "1.0.0");
        assert_eq!(orphans.len(), 2);
        let failed = storage.delete_paths_best_effort(&orphans);
        assert!(failed.is_empty());
        assert!(storage.version_dir("p.x", "1.0.0").is_dir());
        assert!(!storage.version_dir("p.x", "1.1.0").exists());
    }

    #[test]
    fn layout_dirs_are_namespaced_per_plugin() {
        let tmp = tempfile::tempdir().unwrap();
        let storage = PluginStorage::new(tmp.path());
        let root = tmp.path();
        assert_eq!(storage.data_dir("a.b"), root.join("data").join("a.b"));
        assert_eq!(storage.cache_dir("a.b"), root.join("cache").join("a.b"));
        assert_eq!(
            storage.datasets_dir("a.b"),
            root.join("datasets").join("a.b")
        );
        assert_eq!(
            storage.version_dir("a.b", "1.0"),
            root.join("installed").join("a.b").join("1.0")
        );
    }
}
