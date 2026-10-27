//! 插件安装状态机（0.28.1 §五.3）——暂存/校验/提交本地包，处理来源、
//! 平台/版本、归档越界、单插件互斥、崩溃恢复与 Windows 占用残留。
//!
//! 生命周期（phase §3.3）：
//! 1. 校验来源/归档/兼容性 → 暂存验证 → 安装提交 → 待配置/可用；
//!    **半成品不进入执行目录**（staging 与 installed 分离，registry 写入是提交点）。
//! 2. 更新 = 新版本先装进 `installed/<id>/<version>/`，成功后替换记录并
//!    清旧版本目录——失败时旧有效版本不动（"失败更新保持可用旧版"）。
//! 3. 卸载先改记录再删文件；删除失败（Windows 占用）标记 PendingRemove，
//!    下次启动 [`PluginInstaller::recover`] 重试——不假报物理删除。
//!
//! 崩溃恢复语义：提交点（registry 写入）之前崩溃 → installed 下可能残留
//! 孤儿版本目录，`recover` 按记录清理；之后崩溃 → 记录已生效，下次正常加载。
//!
//! 单插件互斥：同一 installer 内部串行（`Mutex`）；跨进程靠 registry 的
//! last-writer-wins + 决策点重读（见 plugin_storage 模块注释）。

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::Digest;

use blink_infra::plugin_storage::archive::{self, ExtractLimits};
use blink_infra::plugin_storage::{
    InstallRecord, InstallSource, InstallState, PluginStorage,
    PluginStorageError,
};

use super::contract::{self, PluginContract};
use super::manifest::PluginManifest;

/// 当前宿主平台的 manifest `platforms` 取值。
pub fn current_platform_tag() -> &'static str {
    #[cfg(all(windows, target_arch = "x86_64"))]
    {
        "windows-x86_64"
    }
    #[cfg(all(windows, target_arch = "aarch64"))]
    {
        "windows-aarch64"
    }
    #[cfg(not(windows))]
    {
        "unsupported"
    }
}

/// 安装/卸载错误（跨 command 边界保留结构化分类）。
#[derive(Debug, thiserror::Error)]
pub enum InstallError {
    #[error("存储失败: {0}")]
    Storage(#[from] PluginStorageError),
    #[error("归档非法: {0}")]
    Archive(String),
    #[error("包结构非法: {0}")]
    InvalidPackage(String),
    #[error("manifest 非法: {0}")]
    Manifest(String),
    #[error("宿主版本不兼容（需要 {min}，当前 {host}）")]
    HostIncompatible { min: String, host: String },
    #[error("插件 id 占用保留命名空间（非随包来源不得使用 builtin.*）")]
    NamespaceReserved,
    #[error("平台不匹配（包声明 {declared:?}，宿主 {host}）")]
    PlatformMismatch { declared: Vec<String>, host: String },
    #[error("契约校验失败: {0}")]
    Contract(#[from] contract::ContractError),
    #[error("插件处于待卸载状态，请重启后重试（文件被占用）")]
    PendingRemoveBusy,
    #[error("包摘要不匹配（期望 {expected}，实际 {actual}）——下载损坏或目录过期")]
    ChecksumMismatch { expected: String, actual: String },
    #[error("未安装的插件: {0}")]
    NotInstalled(String),
    #[error("IO 失败: {0}")]
    Io(String),
}

/// 卸载结果——如实反映文件是否删净。
#[derive(Debug, Clone, PartialEq)]
pub enum UninstallOutcome {
    /// 记录与文件全部清除。
    Removed,
    /// 记录已标记 PendingRemove，部分文件被占用（列在 pending_paths），
    /// 下次启动 recover 重试；插件已不可用（不复活）。
    RemovedPendingFiles { pending_paths: Vec<PathBuf> },
}

/// 安装提交结果：新记录 + 发布失败回滚预案（0.28.7）。
///
/// 旧版本清理从提交点推迟到发布确认后（[`PluginInstaller::finalize_publish`]），
/// 给 [`PluginInstaller::rollback_publish`] 留出完整回滚路径。
#[derive(Debug, Clone, PartialEq)]
pub struct InstallOutcome {
    pub record: InstallRecord,
    /// 回滚预案；首装（无旧记录）为 None——发布失败只能"重启后生效"。
    pub rollback: Option<RollbackPlan>,
}

/// 发布失败回滚预案。
#[derive(Debug, Clone, PartialEq)]
pub struct RollbackPlan {
    /// 提交前的旧安装记录（回滚时原样恢复到 registry）。
    pub old_record: InstallRecord,
    /// 同版本重装时被隔离的旧内容（隔离目录, 原位）——回滚时挪回原位。
    /// 跨版本更新旧内容仍在原位 → None，回滚只恢复记录。
    pub restore: Option<(PathBuf, PathBuf)>,
}

/// 启动恢复报告。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecoveryReport {
    pub staging_cleaned: usize,
    /// 本次恢复彻底移除的 PendingRemove 记录数。
    pub pending_removed: usize,
    /// 本次清掉的孤儿版本目录数。
    pub orphan_dirs_cleaned: usize,
    /// 仍占用未删净的路径数。
    pub still_pending: usize,
}

/// 安装器。`host_version` 由 app 层传入（bin 的 CARGO_PKG_VERSION）。
pub struct PluginInstaller {
    storage: PluginStorage,
    host_version: String,
    limits: ExtractLimits,
    op_lock: std::sync::Mutex<()>,
}

impl PluginInstaller {
    pub fn new(storage: PluginStorage, host_version: impl Into<String>) -> Self {
        PluginInstaller {
            storage,
            host_version: host_version.into(),
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

    /// 从 zip 归档安装（0.28.1 dev 本地包通道；官方目录下载在 0.28.5 接入）。
    ///
    /// 无论成败，本次操作的 staging 目录都会清掉（失败残留只在进程崩溃时
    /// 产生，由下次启动 `recover` 兜底）。
    pub fn install_zip(&self, zip_path: &Path) -> Result<InstallOutcome, InstallError> {
        let _guard = self.op_lock.lock().unwrap();
        let sha = sha256_file(zip_path).map_err(InstallError::Io)?;
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");

        let result = self.install_zip_inner(
            zip_path,
            &pkg_dir,
            sha,
            InstallSource::LocalDev {
                origin: zip_path.to_string_lossy().into_owned(),
            },
        );
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    /// 从官方目录下载的 zip 归档安装（0.28.5 插件中心通道）。
    ///
    /// 与 dev 通道的差别：① 归档 sha256 必须与目录条目声明一致（内容校验
    /// 是供应链主防线）；② 来源记 [`InstallSource::Official`]——官方分发与
    /// 随包同信任级（允许 `builtin.*`，实现"官方更新覆盖随包版"）。
    pub fn install_official_zip(
        &self,
        zip_path: &Path,
        expected_sha256: &str,
        catalog_url: &str,
    ) -> Result<InstallOutcome, InstallError> {
        let _guard = self.op_lock.lock().unwrap();
        if expected_sha256.len() != 64 || !expected_sha256.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(InstallError::InvalidPackage(
                "目录条目 sha256 格式非法".into(),
            ));
        }
        let actual = sha256_file(zip_path).map_err(InstallError::Io)?;
        if !actual.eq_ignore_ascii_case(expected_sha256) {
            return Err(InstallError::ChecksumMismatch {
                expected: expected_sha256.to_lowercase(),
                actual,
            });
        }
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");
        let source = InstallSource::Official {
            catalog_url: catalog_url.to_string(),
        };
        let result = self.install_zip_inner(zip_path, &pkg_dir, actual, source);
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    fn install_zip_inner(
        &self,
        zip_path: &Path,
        pkg_dir: &Path,
        sha: String,
        source: InstallSource,
    ) -> Result<InstallOutcome, InstallError> {
        let result = archive::extract_zip_safely(zip_path, pkg_dir, self.limits)
            .map_err(|e| InstallError::Archive(e.to_string()))?;
        let package_root = resolve_package_root(pkg_dir, result.single_root_dir.as_deref())?;

        self.validate_and_commit(&package_root, source, Some(sha))
    }

    /// 从已解包目录安装（dev 模式；源目录不被移动/破坏，内容有界复制进暂存）。
    pub fn install_dir(&self, dir: &Path) -> Result<InstallOutcome, InstallError> {
        let _guard = self.op_lock.lock().unwrap();
        if !dir.join("manifest.json").is_file() {
            return Err(InstallError::InvalidPackage(
                "目录根下缺少 manifest.json".into(),
            ));
        }
        let staging = self.storage.staging_dir(&next_op_id());
        let pkg_dir = staging.join("pkg");

        let result = self.install_dir_inner(dir, &pkg_dir);
        let _ = std::fs::remove_dir_all(&staging);
        result
    }

    fn install_dir_inner(
        &self,
        dir: &Path,
        pkg_dir: &Path,
    ) -> Result<InstallOutcome, InstallError> {
        archive::copy_dir_bounded(dir, pkg_dir, self.limits)
            .map_err(|e| InstallError::Archive(e.to_string()))?;

        self.validate_and_commit(
            pkg_dir,
            InstallSource::LocalDev {
                origin: dir.to_string_lossy().into_owned(),
            },
            None,
        )
    }

    /// 暂存内容已就位后的公共路径：校验 → 隔离交换提交（registry 写锁内）。
    /// 旧版本清理与发布回滚交给 [`PluginInstaller::finalize_publish`] /
    /// [`PluginInstaller::rollback_publish`]——提交点不再销毁旧内容。
    fn validate_and_commit(
        &self,
        package_root: &Path,
        source: InstallSource,
        sha256: Option<String>,
    ) -> Result<InstallOutcome, InstallError> {
        // 1. manifest 结构校验（schema/protocol 上限在 from_path 内）
        let manifest_path = package_root.join("manifest.json");
        let manifest = PluginManifest::from_path(&manifest_path)
            .map_err(InstallError::Manifest)?;

        // 2. 宿主兼容范围
        if let Some(host) = &manifest.host
            && let Some(min) = &host.min_version
            && compare_versions(&self.host_version, min) == std::cmp::Ordering::Less
        {
            return Err(InstallError::HostIncompatible {
                min: min.clone(),
                host: self.host_version.clone(),
            });
        }
        if let Some(host) = &manifest.host
            && let Some(max) = &host.max_version
            && compare_versions(&self.host_version, max) == std::cmp::Ordering::Greater
        {
            return Err(InstallError::HostIncompatible {
                min: max.clone(),
                host: self.host_version.clone(),
            });
        }

        // 3. 平台匹配（空声明 = 任意平台）
        if !manifest.platforms.is_empty()
            && !manifest
                .platforms
                .iter()
                .any(|p| p == current_platform_tag())
        {
            return Err(InstallError::PlatformMismatch {
                declared: manifest.platforms.clone(),
                host: current_platform_tag().to_string(),
            });
        }

        // 4. 命名空间 + 契约冻结（非随包/非官方来源不授 builtin.*——
        //    0.28.5：Official 与随包同信任级，支撑"官方更新覆盖随包版"）
        let trusted = matches!(source, InstallSource::Official { .. });
        if !trusted {
            contract::validate_namespace(&manifest.id, false)
                .map_err(|_| InstallError::NamespaceReserved)?;
        }
        let contract = PluginContract::from_manifest_checked(&manifest, trusted)?;

        // 5. 提交：registry 写锁内完成「互斥检查 → 隔离交换 → 记录写入」，
        //    与数据包安装器及其他写方串行（update_registry 进程级锁）。
        let target = self.storage.version_dir(&manifest.id, &manifest.version);
        let rollback_stash = self.storage.staging_dir(&format!("old-{}", next_op_id()));
        let (record, rollback) = self
            .storage
            .update_registry(|registry| -> Result<(InstallRecord, Option<RollbackPlan>), InstallError> {
                // 单插件互斥：PendingRemove 期间拒绝新安装（防文件争抢）
                if let Some(existing) = registry.records.get(&manifest.id)
                    && existing.state == InstallState::PendingRemove
                {
                    return Err(InstallError::PendingRemoveBusy);
                }
                let old_record = registry.records.get(&manifest.id).cloned();
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| InstallError::Io(e.to_string()))?;
                }
                // 6. 隔离交换：目标目录已占（同版本重装 / 提交点崩溃残留孤儿）
                //    先把旧内容原子挪进 staging 弃置区，不直接删除——任一步
                //    失败可整体恢复，不留半删状态；挪不动（插件运行中文件被
                //    锁）则干净报错，旧内容不动。
                let mut restore = None;
                if target.exists() {
                    std::fs::create_dir_all(&rollback_stash)
                        .map_err(|e| InstallError::Io(e.to_string()))?;
                    let stash = rollback_stash.join("old");
                    std::fs::rename(&target, &stash).map_err(|e| {
                        InstallError::Io(format!(
                            "旧版本目录无法隔离（{}；插件可能正在运行，请关闭插件窗口后重试）: {e}",
                            target.display()
                        ))
                    })?;
                    if old_record.as_ref().is_some_and(|r| r.version == manifest.version) {
                        restore = Some((stash, target.clone()));
                    }
                }
                if let Err(e) = std::fs::rename(package_root, &target) {
                    if let Some((stash, original)) = &restore {
                        if let Err(restore_err) = std::fs::rename(stash, original) {
                            tracing::error!(
                                plugin = %manifest.id,
                                stash = %stash.display(),
                                error = %restore_err,
                                "隔离内容恢复失败（下次启动 recover 兜底）"
                            );
                        }
                    }
                    return Err(InstallError::Io(format!(
                        "提交安装目录失败（{}）: {e}",
                        target.display()
                    )));
                }

                // 7. 写记录（提交点）。此前任何失败都不影响旧有效版本。
                let record = InstallRecord {
                    plugin_id: manifest.id.clone(),
                    version: manifest.version.clone(),
                    source,
                    installed_at: SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0),
                    state: InstallState::Active,
                    sha256,
                    manifest_schema_version: manifest.schema_version,
                    protocol_version: manifest.protocol_version,
                    pending_paths: vec![],
                };
                registry.records.insert(manifest.id.clone(), record.clone());
                let rollback = old_record.map(|old| RollbackPlan {
                    old_record: old,
                    restore,
                });
                Ok((record, rollback))
            })
            .map_err(InstallError::from)??;

        tracing::info!(
            plugin = %record.plugin_id,
            version = %record.version,
            tools = contract.tool_ids.len(),
            "插件安装已提交（旧版本清理待发布确认）"
        );
        Ok(InstallOutcome { record, rollback })
    }

    /// 发布成功后的收尾：清旧版本目录（best-effort，失败记 pending 债务）
    /// 与隔离弃置区。旧版本清理从提交点推迟到这里——发布失败还要靠它回滚；
    /// 若发布后进程崩溃未走到这里，残留由下次启动 recover 兜底。
    pub fn finalize_publish(&self, outcome: &InstallOutcome) {
        let record = &outcome.record;
        let orphans = self
            .storage
            .orphan_version_dirs(&record.plugin_id, &record.version);
        if !orphans.is_empty() {
            let failed = self.storage.delete_paths_best_effort(&orphans);
            if failed.is_empty() {
                tracing::debug!(
                    plugin = %record.plugin_id,
                    count = orphans.len(),
                    "旧版本目录已清理"
                );
            } else {
                tracing::warn!(
                    plugin = %record.plugin_id,
                    count = failed.len(),
                    "旧版本目录清理未完成，已记入待清理"
                );
                let _ = self.storage.update_registry(|reg| -> Result<(), ()> {
                    if let Some(rec) = reg.records.get_mut(&record.plugin_id) {
                        rec.pending_paths.extend(
                            failed
                                .iter()
                                .map(|p| p.to_string_lossy().into_owned()),
                        );
                    }
                    Ok(())
                });
            }
        }
        if let Some(plan) = &outcome.rollback
            && let Some((stash, _)) = &plan.restore
        {
            let _ = std::fs::remove_dir_all(stash);
        }
    }

    /// 发布失败后的回滚：恢复旧安装记录与旧版本目录（0.28.7 闭环）。
    ///
    /// 先恢复记录再动文件——两步之间崩溃的降级态是"记录指向旧版、目录是
    /// 新版内容"（同版本字符串，可再装修复），优于反向的"记录指向已不存在的
    /// 内容"。文件恢复失败记 warn，残留由下次启动 recover 兜底。首装无旧版
    /// → 返回 None（无可回滚，维持"重启后生效"语义）。
    pub fn rollback_publish(
        &self,
        outcome: &InstallOutcome,
        reason: &str,
    ) -> Result<Option<InstallRecord>, InstallError> {
        let Some(plan) = &outcome.rollback else {
            return Ok(None);
        };
        let pid = plan.old_record.plugin_id.clone();
        let new_version = outcome.record.version.clone();
        // 1. registry 恢复旧记录——仅当当前记录仍是本次新版本（不误回滚并发更新）。
        self.storage
            .update_registry(|reg| -> Result<(), InstallError> {
                let still_new = reg
                    .records
                    .get(&pid)
                    .is_some_and(|r| r.version == new_version);
                if still_new {
                    reg.records.insert(pid.clone(), plan.old_record.clone());
                }
                Ok(())
            })
            .map_err(InstallError::from)??;
        // 2. 新版本目录让位（未被发布、无进程占用，直接清）。同版本路径与
        //    旧记录冲突 → 删掉新内容后把隔离区旧内容放回原位；跨版本 →
        //    新目录本就是孤儿。
        let target = self.storage.version_dir(&pid, &new_version);
        let _ = std::fs::remove_dir_all(&target);
        if let Some((stash, original)) = &plan.restore
            && let Err(e) = std::fs::rename(stash, original)
        {
            tracing::warn!(
                plugin = %pid,
                stash = %stash.display(),
                error = %e,
                "回滚恢复旧版本目录失败（残留由下次启动 recover 清理）"
            );
        }
        tracing::warn!(
            plugin = %pid,
            from_version = %new_version,
            to_version = %plan.old_record.version,
            reason,
            "插件发布失败，已回滚到旧版本"
        );
        Ok(Some(plan.old_record.clone()))
    }

    /// 卸载：改记录 → 删文件。`keep_data = true` 保留用户数据目录。
    pub fn uninstall(&self, plugin_id: &str, keep_data: bool) -> Result<UninstallOutcome, InstallError> {
        let _guard = self.op_lock.lock().unwrap();
        self.storage
            .update_registry(|registry| -> Result<UninstallOutcome, InstallError> {
                let Some(record) = registry.records.get(plugin_id).cloned() else {
                    return Err(InstallError::NotInstalled(plugin_id.to_string()));
                };

                let mut to_delete: Vec<PathBuf> = vec![
                    self.storage.installed_root().join(plugin_id),
                    self.storage.cache_dir(plugin_id),
                ];
                if !keep_data {
                    to_delete.push(self.storage.data_dir(plugin_id));
                }
                to_delete.extend(record.pending_paths.iter().map(PathBuf::from));

                let failed = self.storage.delete_paths_best_effort(&to_delete);
                if failed.is_empty() {
                    registry.records.remove(plugin_id);
                    tracing::info!(plugin = plugin_id, "插件已卸载（文件已删净）");
                    Ok(UninstallOutcome::Removed)
                } else {
                    // 文件被占用：记录降级 PendingRemove——插件不可用，但不谎报已删除。
                    let rec = registry.records.get_mut(plugin_id).expect("上方已确认存在");
                    rec.state = InstallState::PendingRemove;
                    rec.pending_paths = failed
                        .iter()
                        .map(|p| p.to_string_lossy().into_owned())
                        .collect();
                    tracing::warn!(
                        plugin = plugin_id,
                        count = failed.len(),
                        "插件卸载部分文件被占用，标记待清理（下次启动重试）"
                    );
                    Ok(UninstallOutcome::RemovedPendingFiles { pending_paths: failed })
                }
            })
            .map_err(InstallError::from)?
    }

    /// 启动恢复：清 staging、重试 PendingRemove、清孤儿版本目录。
    /// registry 变更走 update_registry（与安装/卸载写方串行）。
    pub fn recover(&self) -> RecoveryReport {
        let staging_cleaned = self.storage.cleanup_staging();
        let mut report = RecoveryReport {
            staging_cleaned,
            ..Default::default()
        };

        let recovery = self
            .storage
            .update_registry(|registry| -> Result<RecoveryReport, ()> {
                let mut report = RecoveryReport::default();
                let ids: Vec<String> = registry.records.keys().cloned().collect();
                for id in ids {
                    let Some(record) = registry.records.get(&id).cloned() else {
                        continue;
                    };
                    match record.state {
                        InstallState::PendingRemove => {
                            // 记录已在卸载语义下：重试删除全部关联路径，删净即移除记录。
                            let mut to_delete: Vec<PathBuf> = record
                                .pending_paths
                                .iter()
                                .map(PathBuf::from)
                                .collect();
                            to_delete.push(self.storage.installed_root().join(&id));
                            to_delete.push(self.storage.cache_dir(&id));
                            // 用户数据仅在卸载时明确选择删除才进入 pending_paths。
                            // keep_data=true 的占用重试不能扩大删除范围。
                            let failed = self.storage.delete_paths_best_effort(&to_delete);
                            if failed.is_empty() {
                                registry.records.remove(&id);
                                report.pending_removed += 1;
                            } else {
                                if let Some(rec) = registry.records.get_mut(&id) {
                                    rec.pending_paths = failed
                                        .iter()
                                        .map(|p| p.to_string_lossy().into_owned())
                                        .collect();
                                }
                                report.still_pending += failed.len();
                            }
                        }
                        InstallState::Active => {
                            // 提交点崩溃残留：未被记录引用的版本目录。
                            let orphans = self.storage.orphan_version_dirs(&id, &record.version);
                            let failed = self.storage.delete_paths_best_effort(&orphans);
                            report.orphan_dirs_cleaned += orphans.len() - failed.len();
                            report.still_pending += failed.len();
                            if !failed.is_empty()
                                && let Some(rec) = registry.records.get_mut(&id)
                            {
                                rec.pending_paths
                                    .extend(failed.iter().map(|p| p.to_string_lossy().into_owned()));
                            }
                        }
                    }
                }
                Ok(report)
            });
        match recovery {
            // 读取失败（损坏/过新）在 wiring 层决策——不静默当空配置。
            Err(_) => tracing::warn!("恢复期间读取/写入安装记录失败，跳过记录级恢复"),
            Ok(Ok(inner)) => {
                report.pending_removed += inner.pending_removed;
                report.orphan_dirs_cleaned += inner.orphan_dirs_cleaned;
                report.still_pending += inner.still_pending;
            }
            Ok(Err(())) => unreachable!("恢复闭包不返回 Err"),
        }
        if report.pending_removed > 0 || report.orphan_dirs_cleaned > 0 || report.still_pending > 0
        {
            tracing::info!(?report, "插件存储恢复完成");
        }
        report
    }

    /// 读取有效安装的活动版本目录（wiring/发布快照构造用；每次现读）。
    pub fn active_version_dir(&self, plugin_id: &str) -> Option<PathBuf> {
        let registry = self.storage.load_registry().ok()?;
        let record = registry.records.get(plugin_id)?;
        match record.state {
            InstallState::Active => Some(self.storage.version_dir(plugin_id, &record.version)),
            InstallState::PendingRemove => None,
        }
    }

    /// 列出全部有效安装（含记录快照，展示用）。
    pub fn list_installed(&self) -> Result<Vec<InstallRecord>, InstallError> {
        Ok(self
            .storage
            .load_registry()?
            .records
            .into_values()
            .collect())
    }
}

/// zip 常见 `pkg-root/manifest.json` 包装层：单一顶层目录则下钻一层。
fn resolve_package_root(pkg_dir: &Path, single_root: Option<&str>) -> Result<PathBuf, InstallError> {
    let root = match single_root {
        Some(top) => pkg_dir.join(top),
        None => pkg_dir.to_path_buf(),
    };
    if !root.join("manifest.json").is_file() {
        return Err(InstallError::InvalidPackage(
            "包根下缺少 manifest.json".into(),
        ));
    }
    Ok(root)
}

fn next_op_id() -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("op-{nanos:x}")
}

fn sha256_file(path: &Path) -> Result<String, String> {
    let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mut hasher = sha2::Sha256::new();
    std::io::copy(&mut file, &mut hasher).map_err(|e| e.to_string())?;
    Ok(format!("{:x}", hasher.finalize()))
}

/// 点分数字版本的宽松比较（`0.28.1` vs `0.28.2`）。
/// 非数字段按字符串比较；段数不同短的补 0（`0.28` == `0.28.0`）。
pub fn compare_versions(a: &str, b: &str) -> std::cmp::Ordering {
    let parse = |s: &str| -> Vec<u64> {
        s.split('.')
            .map(|seg| seg.trim().parse::<u64>().unwrap_or(0))
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    let n = va.len().max(vb.len());
    for i in 0..n {
        let x = va.get(i).copied().unwrap_or(0);
        let y = vb.get(i).copied().unwrap_or(0);
        match x.cmp(&y) {
            std::cmp::Ordering::Equal => continue,
            other => return other,
        }
    }
    std::cmp::Ordering::Equal
}

#[cfg(test)]
mod tests {
    use super::*;

    fn installer(root: &Path) -> PluginInstaller {
        PluginInstaller::new(PluginStorage::new(root.to_path_buf()), "0.28.1")
    }

    fn write_manifest(dir: &Path, id: &str, version: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            format!(
                r#"{{"schema_version":1,"id":"{id}","name":"T","version":"{version}",
                    "runtime":{{"exec":"t.exe"}},
                    "capabilities":["query"],
                    "triggers":[{{"type":"keyword","keyword":"tst"}}]}}"#
            )
            .replace('\n', ""),
        )
        .unwrap();
        std::fs::write(dir.join("t.exe"), "stub").unwrap();
    }

    #[test]
    fn install_dir_commits_record_and_recovers_orphans() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        let pkg = tmp.path().join("pkg");
        write_manifest(&pkg, "extra.demo", "1.0.0");

        let record = inst.install_dir(&pkg).unwrap().record;
        assert_eq!(record.state, InstallState::Active);
        let committed = inst.storage().version_dir("extra.demo", "1.0.0");
        assert!(committed.join("manifest.json").is_file());
        // 源目录不被破坏
        assert!(pkg.join("manifest.json").is_file());
        // 记录可读回
        assert!(inst.active_version_dir("extra.demo").is_some());

        // 模拟提交点崩溃残留：无记录的孤儿版本目录
        std::fs::create_dir_all(inst.storage().version_dir("extra.demo", "0.9.0")).unwrap();
        let report = inst.recover();
        assert_eq!(report.orphan_dirs_cleaned, 1);
        assert!(!inst.storage().version_dir("extra.demo", "0.9.0").exists());
        // 活动版本不受恢复影响
        assert!(inst.storage().version_dir("extra.demo", "1.0.0").is_dir());
    }

    #[test]
    fn failed_update_keeps_old_version_active() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        let pkg = tmp.path().join("pkg");
        write_manifest(&pkg, "extra.demo", "1.0.0");
        inst.install_dir(&pkg).unwrap();

        // 新版本包带非法 id → 拒绝；旧版本目录与记录不动
        let bad = tmp.path().join("bad");
        write_manifest(&bad, "BAD ID!", "2.0.0");
        assert!(inst.install_dir(&bad).is_err());
        assert!(inst.storage().version_dir("extra.demo", "1.0.0").is_dir());
        assert_eq!(
            inst.active_version_dir("extra.demo").unwrap(),
            inst.storage().version_dir("extra.demo", "1.0.0")
        );
    }

    #[test]
    fn update_replaces_record_and_cleans_old_dir_after_finalize() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_manifest(&tmp.path().join("v1"), "extra.demo", "1.0.0");
        inst.install_dir(&tmp.path().join("v1")).unwrap();
        write_manifest(&tmp.path().join("v2"), "extra.demo", "2.0.0");
        let outcome = inst.install_dir(&tmp.path().join("v2")).unwrap();
        let record = &outcome.record;

        assert_eq!(record.version, "2.0.0");
        assert!(inst.storage().version_dir("extra.demo", "2.0.0").is_dir());
        // 旧版本目录保留到发布确认（finalize_publish）——发布失败还要靠它回滚。
        assert!(inst.storage().version_dir("extra.demo", "1.0.0").is_dir());
        inst.finalize_publish(&outcome);
        assert!(!inst.storage().version_dir("extra.demo", "1.0.0").exists());
        // 数据目录在安装/更新时创建但不被清（用户数据语义）
        assert_eq!(
            inst.active_version_dir("extra.demo").unwrap(),
            inst.storage().version_dir("extra.demo", "2.0.0")
        );
    }

    #[test]
    fn uninstall_keeps_data_when_requested() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_manifest(&tmp.path().join("pkg"), "extra.demo", "1.0.0");
        inst.install_dir(&tmp.path().join("pkg")).unwrap();
        std::fs::create_dir_all(inst.storage().data_dir("extra.demo")).unwrap();
        std::fs::write(inst.storage().data_dir("extra.demo").join("u.txt"), "keep").unwrap();

        let outcome = inst.uninstall("extra.demo", true).unwrap();
        assert_eq!(outcome, UninstallOutcome::Removed);
        assert!(inst.storage().data_dir("extra.demo").join("u.txt").is_file());
        assert!(!inst.storage().installed_root().join("extra.demo").exists());
        assert!(matches!(
            inst.uninstall("extra.demo", true),
            Err(InstallError::NotInstalled(_))
        ));
    }

    #[test]
    fn pending_uninstall_recovery_keeps_preserved_data() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_manifest(&tmp.path().join("pkg"), "extra.demo", "1.0.0");
        inst.install_dir(&tmp.path().join("pkg")).unwrap();
        let data = inst.storage().data_dir("extra.demo");
        std::fs::create_dir_all(&data).unwrap();
        std::fs::write(data.join("user.txt"), "keep").unwrap();
        // 模拟卸载因 exe 占用留下待清理记录，用户已选择保留数据。
        let mut registry = inst.storage().load_registry().unwrap();
        let record = registry.records.get_mut("extra.demo").unwrap();
        record.state = InstallState::PendingRemove;
        record.pending_paths = vec![inst.storage().installed_root().join("extra.demo").to_string_lossy().into_owned()];
        inst.storage().save_registry(&registry).unwrap();
        inst.recover();
        assert_eq!(std::fs::read_to_string(data.join("user.txt")).unwrap(), "keep");
        assert!(inst.list_installed().unwrap().is_empty());
    }

    #[test]
    fn builtin_namespace_and_host_compat_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());

        let mut dir = tmp.path().join("ns");
        write_manifest(&dir, "builtin.fake", "1.0.0");
        assert!(matches!(
            inst.install_dir(&dir),
            Err(InstallError::NamespaceReserved)
        ));

        // 宿主版本过低
        dir = tmp.path().join("ver");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("manifest.json"),
            r#"{"schema_version":1,"id":"extra.hi","name":"T","version":"1.0",
                "host":{"min_version":"99.0.0"},
                "runtime":{"exec":"t.exe"}}"#
                .replace('\n', ""),
        )
        .unwrap();
        assert!(matches!(
            inst.install_dir(&dir),
            Err(InstallError::HostIncompatible { .. })
        ));
    }

    #[test]
    fn install_zip_validates_archive_and_commits() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        // 构造包目录 → zip
        let pkg = tmp.path().join("pkg");
        write_manifest(&pkg, "extra.zipped", "1.0.0");
        let zip_path = tmp.path().join("pkg.zip");
        let file = std::fs::File::create(&zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for entry in ["manifest.json", "t.exe"] {
            let content = std::fs::read(pkg.join(entry)).unwrap();
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file(format!("pkgroot/{entry}"), opts).unwrap();
            std::io::Write::write_all(&mut zip, &content).unwrap();
        }
        zip.finish().unwrap();

        let record = inst.install_zip(&zip_path).unwrap().record;
        assert_eq!(record.plugin_id, "extra.zipped");
        assert!(record.sha256.is_some());
        assert!(inst
            .storage()
            .version_dir("extra.zipped", "1.0.0")
            .join("manifest.json")
            .is_file());
        // staging 清空
        let staging = inst.storage().root().join("staging");
        assert!(!staging.join("pkgroot").exists());
    }

    #[test]
    fn version_compare_semantics() {
        use std::cmp::Ordering::*;
        assert_eq!(compare_versions("0.28.1", "0.28.2"), Less);
        assert_eq!(compare_versions("0.28", "0.28.0"), Equal);
        assert_eq!(compare_versions("1.0.0", "0.99.99"), Greater);
    }

    /// 写一个带内容标记的包目录（marker.txt 用于区分新旧内容）。
    fn write_pkg_with_marker(dir: &Path, id: &str, version: &str, marker: &str) {
        write_manifest(dir, id, version);
        std::fs::write(dir.join("marker.txt"), marker).unwrap();
    }

    #[test]
    fn same_version_reinstall_swaps_atomically_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        // 首装：无旧记录 → 无回滚预案。
        write_pkg_with_marker(&tmp.path().join("p1"), "extra.demo", "1.0.0", "old");
        let first = inst.install_dir(&tmp.path().join("p1")).unwrap();
        assert!(first.rollback.is_none());

        // 同版本重装：旧内容隔离交换进位（不先删除），携带回滚预案。
        write_pkg_with_marker(&tmp.path().join("p2"), "extra.demo", "1.0.0", "new");
        let second = inst.install_dir(&tmp.path().join("p2")).unwrap();
        let plan = second.rollback.as_ref().expect("同版本重装必须有回滚预案");
        assert_eq!(plan.old_record.version, "1.0.0");
        assert!(plan.restore.is_some());
        let target = inst.storage().version_dir("extra.demo", "1.0.0");
        assert_eq!(
            std::fs::read_to_string(target.join("marker.txt")).unwrap(),
            "new",
            "新内容应已就位"
        );

        // 发布失败 → 回滚：旧内容回原位、记录恢复为旧记录。
        inst.rollback_publish(&second, "模拟发布失败").unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("marker.txt")).unwrap(),
            "old",
            "回滚后旧内容应恢复"
        );
        let registry = inst.storage().load_registry().unwrap();
        let restored = &registry.records["extra.demo"];
        assert_eq!(restored.installed_at, first.record.installed_at);
        assert!(restored.pending_paths.is_empty());
    }

    #[test]
    fn upgrade_keeps_old_version_until_finalize_and_rolls_back() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_pkg_with_marker(&tmp.path().join("v1"), "extra.demo", "1.0.0", "v1");
        inst.install_dir(&tmp.path().join("v1")).unwrap();
        write_pkg_with_marker(&tmp.path().join("v2"), "extra.demo", "2.0.0", "v2");
        let second = inst.install_dir(&tmp.path().join("v2")).unwrap();
        let plan = second.rollback.as_ref().expect("升级必须有回滚预案");
        assert!(plan.restore.is_none(), "跨版本旧内容仍在原位，无需挪回");
        // 提交后、finalize 前旧版本目录保留（回滚依赖）。
        assert!(inst.storage().version_dir("extra.demo", "1.0.0").is_dir());

        // 发布失败 → 回滚：记录回到 1.0.0，新版本目录清掉，旧目录完好。
        inst.rollback_publish(&second, "模拟发布失败").unwrap();
        let registry = inst.storage().load_registry().unwrap();
        assert_eq!(registry.records["extra.demo"].version, "1.0.0");
        assert!(inst.storage().version_dir("extra.demo", "1.0.0").is_dir());
        assert!(!inst.storage().version_dir("extra.demo", "2.0.0").exists());

        // 重装 2.0.0 → 发布成功 finalize：旧版本目录这次被清。
        write_pkg_with_marker(&tmp.path().join("v2b"), "extra.demo", "2.0.0", "v2b");
        let redo = inst.install_dir(&tmp.path().join("v2b")).unwrap();
        inst.finalize_publish(&redo);
        assert!(!inst.storage().version_dir("extra.demo", "1.0.0").exists());
        assert!(inst.storage().version_dir("extra.demo", "2.0.0").is_dir());
        let staging = inst.storage().root().join("staging");
        let leftovers: Vec<_> = std::fs::read_dir(&staging)
            .map(|entries| entries.flatten().collect())
            .unwrap_or_default();
        assert!(
            leftovers.is_empty(),
            "回滚/收尾后 staging 不应残留: {leftovers:?}"
        );
    }

    /// 构造包目录的 zip 归档（manifest 在根，供 official 通道测试）。
    fn make_zip(pkg_dir: &Path, zip_path: &Path) {
        let file = std::fs::File::create(zip_path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        let mut paths = vec![pkg_dir.join("manifest.json"), pkg_dir.join("t.exe")];
        let web = pkg_dir.join("web");
        if web.is_dir() {
            for entry in std::fs::read_dir(&web).unwrap().flatten() {
                paths.push(entry.path());
            }
        }
        for path in paths {
            let content = std::fs::read(&path).unwrap();
            let rel = path.strip_prefix(pkg_dir).unwrap().to_string_lossy().into_owned();
            let opts: zip::write::FileOptions<'_, ()> = zip::write::FileOptions::default();
            zip.start_file(rel, opts).unwrap();
            std::io::Write::write_all(&mut zip, &content).unwrap();
        }
        zip.finish().unwrap();
    }

    #[test]
    fn official_zip_verifies_sha_and_allows_builtin_namespace() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        let pkg = tmp.path().join("pkg");
        // builtin.* 对 LocalDev 保留；官方目录分发等同随包信任级（0.28.5）
        write_manifest(&pkg, "builtin.dictionary", "0.2.0");
        let zip_path = tmp.path().join("pkg.zip");
        make_zip(&pkg, &zip_path);

        let real_sha = sha256_file(&zip_path).unwrap();
        // 1) sha 不匹配 → 拒绝，无任何提交
        assert!(matches!(
            inst.install_official_zip(&zip_path, &"0".repeat(64), "https://example.invalid/catalog.json"),
            Err(InstallError::ChecksumMismatch { .. })
        ));
        assert!(inst.list_installed().unwrap().is_empty());
        // sha 格式非法同样拒绝
        assert!(inst
            .install_official_zip(&zip_path, "abcd", "https://example.invalid/catalog.json")
            .is_err());

        // 2) 匹配 sha → 提交成功，来源与摘要如实记录
        let record = inst
            .install_official_zip(&zip_path, &real_sha, "https://example.invalid/catalog.json")
            .unwrap()
            .record;
        assert_eq!(record.plugin_id, "builtin.dictionary");
        assert_eq!(
            record.source,
            InstallSource::Official {
                catalog_url: "https://example.invalid/catalog.json".into()
            }
        );
        assert_eq!(record.sha256.as_deref(), Some(real_sha.as_str()));
        assert_eq!(
            inst.active_version_dir("builtin.dictionary").unwrap(),
            inst.storage().version_dir("builtin.dictionary", "0.2.0")
        );

        // 对照：同一 builtin 包走 LocalDev 通道仍被命名空间拒绝
        assert!(matches!(
            inst.install_zip(&zip_path),
            Err(InstallError::NamespaceReserved)
        ));
    }

    #[test]
    fn pending_remove_blocks_reinstall_and_recovers() {
        let tmp = tempfile::tempdir().unwrap();
        let inst = installer(tmp.path());
        write_manifest(&tmp.path().join("pkg"), "extra.demo", "1.0.0");
        inst.install_dir(&tmp.path().join("pkg")).unwrap();

        // 手工制造 PendingRemove + 占用路径（模拟 Windows 锁）
        let mut registry = inst.storage().load_registry().unwrap();
        let rec = registry.records.get_mut("extra.demo").unwrap();
        rec.state = InstallState::PendingRemove;
        rec.pending_paths
            .push(inst.storage().version_dir("extra.demo", "1.0.0").to_string_lossy().into_owned());
        inst.storage().save_registry(&registry).unwrap();

        // 新安装被拒（互斥）
        assert!(matches!(
            inst.install_dir(&tmp.path().join("pkg")),
            Err(InstallError::PendingRemoveBusy)
        ));
        // 活动目录不可见
        assert!(inst.active_version_dir("extra.demo").is_none());
        // recover 删净 → 记录移除 → 可重装
        let report = inst.recover();
        assert_eq!(report.pending_removed, 1);
        let record = inst.install_dir(&tmp.path().join("pkg")).unwrap().record;
        assert_eq!(record.version, "1.0.0");
    }
}
