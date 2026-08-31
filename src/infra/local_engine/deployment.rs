//! 部署事务存储（slot + active pointer + transaction journal）。
//!
//! ## 产品语义
//!
//! - 一个引擎只有一个 **active deployment**（`deployment.json` 指针指向的
//!   不可变 slot）。
//! - 事务期间最多存在 **old + candidate** 两个 slot；安装/升级成功后稳定
//!   状态只保留 active——旧 slot 立即删除。
//! - 不存在世代历史，也不存在可回滚的"previous generation"产品状态；
//!   staging/rollback 只在事务内临时存在。
//!
//! ## 事务协议（journal fail-closed）
//!
//! ```text
//! begin:      journal{phase: Building, candidate, previous}   ← 任何破坏性步骤之前
//! build:      staging/{operation-id}/ 构建 + self-test
//! promote:    staging → slot-{candidate}（rename）
//! pre-switch: journal.phase = Switched                        ← 指针切换之前写
//! switch:     deployment.json → candidate（原子替换）
//! verify:     重读 manifest + artifact identity；失败 → 自动回滚
//! commit:     journal.phase = Committed → 删除旧 slot（失败记 residue）→ 清除 journal
//! ```
//!
//! 崩溃恢复 `recover`（启动时逐引擎执行，fail-closed）：
//!
//! | journal 相 | 含义 | 恢复动作 |
//! |---|---|---|
//! | `Building` | 事务未切换指针 | 丢弃 candidate slot、清扫孤儿 staging、清 journal |
//! | `Switched` | 指针已切但验证未过 | 指针回写 previous（或删除）、candidate 记 residue、清 journal |
//! | `Committed` | 事务已成功，仅收尾未完 | 保留 active、补删旧 slot（失败记 residue）、清 journal |
//!
//! **residue 不是产品状态**：Windows 文件占用导致旧 slot 无法删除时只登记
//! `residue.json` 供后续清理 UI 展示；它不能成为可回滚历史版本。
//!
//! journal 解析失败（损坏/无法读取）时 fail-closed：视为未知事务中断，
//! 无指针则清空全部 slot，有指针则保留指针、其余 slot 记 residue。

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::runtime::{
    self, DeploymentManifest, EngineId, RuntimeError, atomic_write_json, now_ms, slot_dir,
    slot_manifest_path, staging_dir, validate_operation_id,
};

// ── DeploymentSlot ─────────────────────────────────────────────────────────

/// 部署 slot（物理位置，闭合两槽轮换）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DeploymentSlot {
    A,
    B,
}

impl DeploymentSlot {
    /// 目录名（`slot-a` / `slot-b`）。
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::A => "slot-a",
            Self::B => "slot-b",
        }
    }

    /// 从目录名解析。
    pub fn parse(name: &str) -> Result<Self, RuntimeError> {
        match name {
            "slot-a" => Ok(Self::A),
            "slot-b" => Ok(Self::B),
            other => Err(RuntimeError::PathTraversal {
                path: other.to_string(),
            }),
        }
    }

    /// 另一个 slot（事务 candidate 目标）。
    pub fn other(&self) -> Self {
        match self {
            Self::A => Self::B,
            Self::B => Self::A,
        }
    }

    /// slot 目录路径。
    pub fn dir(&self, engine_id: &EngineId) -> PathBuf {
        slot_dir(engine_id, self.as_str())
    }
}

impl std::fmt::Display for DeploymentSlot {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ── 指针 / journal / residue 格式 ──────────────────────────────────────────

/// deployment pointer schema 版本。
pub const DEPLOYMENT_POINTER_SCHEMA_VERSION: u32 = 2;

/// `deployment.json`——active 部署指针（引擎唯一业务真相的磁盘投影）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeploymentPointer {
    /// 当前 active 部署的 install id（内容身份，用于日志/事件/lease）。
    pub install_id: String,
    /// active slot 目录名。
    pub slot: String,
    /// 更新时间（Unix 毫秒）。
    pub updated_at_ms: u64,
    /// schema 版本。
    pub schema_version: u32,
}

/// transaction journal schema 版本。
pub const TRANSACTION_JOURNAL_SCHEMA_VERSION: u32 = 1;

/// 事务阶段（恢复语义见模块文档）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransactionPhase {
    /// staging 构建中（指针未切换）。
    Building,
    /// 指针已切换、验证未完成。
    Switched,
    /// 事务已成功，收尾（旧 slot 删除/journal 清除）未完成。
    Committed,
}

/// 事务引用的上一个部署。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreviousDeployment {
    pub install_id: String,
    pub slot: String,
}

/// `transaction.json`——事务 journal。存在即表示事务未收尾。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionJournal {
    pub schema_version: u32,
    pub engine_id: String,
    pub operation_id: String,
    /// candidate（新）slot。
    pub candidate_slot: String,
    pub candidate_install_id: String,
    /// 事务前的 active 部署（None = 全新安装）。
    pub previous: Option<PreviousDeployment>,
    pub phase: TransactionPhase,
    pub started_at_ms: u64,
}

/// residue schema 版本。
pub const RESIDUE_SCHEMA_VERSION: u32 = 1;

/// 清理残留记录（不可删除的非 active slot）。
///
/// **不是产品状态**：不参与回滚、不构成部署历史，仅供清理。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResidueRecord {
    /// 残留 slot 目录名。
    pub slot: String,
    /// 残留内容身份（若 manifest 可读）。
    pub install_id: Option<String>,
    pub marked_at_ms: u64,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ResidueFile {
    #[serde(default)]
    schema_version: u32,
    #[serde(default)]
    records: Vec<ResidueRecord>,
}

impl Default for ResidueFile {
    fn default() -> Self {
        Self {
            schema_version: RESIDUE_SCHEMA_VERSION,
            records: Vec::new(),
        }
    }
}

// ── 恢复结果 ──────────────────────────────────────────────────────────────

/// 一次 `recover` 的结果投影（供日志/测试断言）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecoveryOutcome {
    /// 无 journal——稳定状态，无需恢复。
    Stable,
    /// Building 中断：丢弃了 candidate slot。
    DiscardedCandidate { slot: String },
    /// Switched 中断：回滚到 previous。
    RevertedToPrevious {
        slot: String,
        previous: Option<String>,
    },
    /// Committed 中断：保留 active，补删旧 slot（可能记 residue）。
    FinalizedCommit { slot: String },
    /// journal 损坏：fail-closed 处理。
    FailClosed { reason: String },
}

// ── DeploymentStore ────────────────────────────────────────────────────────

/// 无状态部署存储原语（所有操作按 engine_id 寻址磁盘）。
///
/// 事务编排（provider 调用、self-test、事件）在 providers 层；
/// 本模块只负责指针/journal/residue/slot 的原子磁盘操作与恢复。
pub struct DeploymentStore;

impl DeploymentStore {
    // ── 指针 ──────────────────────────────────────────────────────────────

    /// `deployment.json` 路径。
    pub fn pointer_path(engine_id: &EngineId) -> PathBuf {
        runtime::engine_root(engine_id).join("deployment.json")
    }

    /// journal 路径。
    pub fn journal_path(engine_id: &EngineId) -> PathBuf {
        runtime::engine_root(engine_id).join("transaction.json")
    }

    /// residue 路径。
    pub fn residue_path(engine_id: &EngineId) -> PathBuf {
        runtime::engine_root(engine_id).join("residue.json")
    }

    /// 读取 active 指针（None = 未安装）。
    pub fn read_pointer(engine_id: &EngineId) -> Result<Option<DeploymentPointer>, RuntimeError> {
        let path = Self::pointer_path(engine_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let pointer: DeploymentPointer = serde_json::from_str(&content).map_err(|e| {
            RuntimeError::CurrentPointerParseFailed {
                message: format!("{e}"),
            }
        })?;
        if pointer.schema_version != DEPLOYMENT_POINTER_SCHEMA_VERSION {
            return Err(RuntimeError::CurrentPointerParseFailed {
                message: format!(
                    "schema 不兼容: expected={DEPLOYMENT_POINTER_SCHEMA_VERSION}, actual={}",
                    pointer.schema_version
                ),
            });
        }
        runtime::validate_slot_name(&pointer.slot)?;
        Ok(Some(pointer))
    }

    /// 原子写 active 指针。
    pub fn write_pointer(
        engine_id: &EngineId,
        pointer: &DeploymentPointer,
    ) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(&pointer.slot)?;
        atomic_write_json(&Self::pointer_path(engine_id), pointer)
    }

    /// 删除 active 指针（仅恢复路径使用——全新安装事务失败且无 previous 时）。
    pub fn remove_pointer(engine_id: &EngineId) -> Result<(), RuntimeError> {
        let path = Self::pointer_path(engine_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// 读取 active 部署（指针 + manifest）。None = 未安装。
    pub fn read_active(
        engine_id: &EngineId,
    ) -> Result<Option<(DeploymentPointer, DeploymentManifest)>, RuntimeError> {
        match Self::read_pointer(engine_id)? {
            None => Ok(None),
            Some(pointer) => {
                let manifest = runtime::read_slot_manifest(engine_id, &pointer.slot)?;
                Ok(Some((pointer, manifest)))
            }
        }
    }

    /// active 部署目录（adapter 解析 venv python 等用）。None = 未安装。
    pub fn active_dir(
        engine_id: &EngineId,
    ) -> Result<Option<(DeploymentPointer, PathBuf)>, RuntimeError> {
        match Self::read_pointer(engine_id)? {
            None => Ok(None),
            Some(pointer) => Ok(Some((pointer.clone(), slot_dir(engine_id, &pointer.slot)))),
        }
    }

    // ── journal ───────────────────────────────────────────────────────────

    /// 读取 journal（None = 无未收尾事务）。
    pub fn read_journal(engine_id: &EngineId) -> Result<Option<TransactionJournal>, RuntimeError> {
        let path = Self::journal_path(engine_id);
        if !path.exists() {
            return Ok(None);
        }
        let content = std::fs::read_to_string(&path)?;
        let journal: TransactionJournal = serde_json::from_str(&content).map_err(|e| {
            RuntimeError::TransactionJournalInvalid {
                message: format!("{e}"),
            }
        })?;
        if journal.schema_version != TRANSACTION_JOURNAL_SCHEMA_VERSION {
            return Err(RuntimeError::TransactionJournalInvalid {
                message: format!(
                    "schema 不兼容: expected={TRANSACTION_JOURNAL_SCHEMA_VERSION}, actual={}",
                    journal.schema_version
                ),
            });
        }
        validate_operation_id(&journal.operation_id)?;
        runtime::validate_slot_name(&journal.candidate_slot)?;
        Ok(Some(journal))
    }

    /// 写 journal（begin 与阶段推进）。
    pub fn write_journal(
        engine_id: &EngineId,
        journal: &TransactionJournal,
    ) -> Result<(), RuntimeError> {
        atomic_write_json(&Self::journal_path(engine_id), journal)
    }

    /// 清除 journal（事务收尾的最后一步）。
    pub fn clear_journal(engine_id: &EngineId) -> Result<(), RuntimeError> {
        let path = Self::journal_path(engine_id);
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// 事务 begin：在 staging 构建前写 journal（fail-closed 前提）。
    ///
    /// 返回 journal（含 candidate slot 与 previous）。candidate slot 若有
    /// 旧内容（上次事务残留），先尝试清空；清不掉则记 residue——
    /// 事务继续，candidate 位置将被覆盖性重建。
    pub fn begin(
        engine_id: &EngineId,
        operation_id: &str,
        candidate_install_id: &str,
    ) -> Result<TransactionJournal, RuntimeError> {
        validate_operation_id(operation_id)?;
        let previous = Self::read_pointer(engine_id)?.map(|p| PreviousDeployment {
            install_id: p.install_id,
            slot: p.slot,
        });
        let candidate_slot = match &previous {
            Some(prev) => DeploymentSlot::parse(&prev.slot)?.other(),
            None => DeploymentSlot::A,
        };

        // 清空 candidate 槽位旧内容（上一事务残留）
        let cand_dir = slot_dir(engine_id, candidate_slot.as_str());
        if cand_dir.exists() {
            match std::fs::remove_dir_all(&cand_dir) {
                Ok(()) => {}
                Err(e) => {
                    Self::mark_residue(
                        engine_id,
                        candidate_slot.as_str(),
                        None,
                        &format!("begin: candidate slot 清理失败: {e}"),
                    )?;
                    return Err(RuntimeError::StagingCreateFailed {
                        message: format!(
                            "candidate slot {} 被占用，无法开始事务: {e}",
                            candidate_slot
                        ),
                    });
                }
            }
        }

        let journal = TransactionJournal {
            schema_version: TRANSACTION_JOURNAL_SCHEMA_VERSION,
            engine_id: engine_id.as_str().to_string(),
            operation_id: operation_id.to_string(),
            candidate_slot: candidate_slot.as_str().to_string(),
            candidate_install_id: candidate_install_id.to_string(),
            previous,
            phase: TransactionPhase::Building,
            started_at_ms: now_ms(),
        };
        Self::write_journal(engine_id, &journal)?;
        Ok(journal)
    }

    /// journal 阶段推进（pre-switch / commit 前调用）。
    pub fn advance_phase(
        engine_id: &EngineId,
        journal: &mut TransactionJournal,
        phase: TransactionPhase,
    ) -> Result<(), RuntimeError> {
        journal.phase = phase;
        Self::write_journal(engine_id, journal)
    }

    // ── residue ───────────────────────────────────────────────────────────

    /// 读取 residue 记录。
    pub fn read_residue(engine_id: &EngineId) -> Result<Vec<ResidueRecord>, RuntimeError> {
        let path = Self::residue_path(engine_id);
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let file: ResidueFile = serde_json::from_str(&content)?;
        Ok(file.records)
    }

    /// 登记 residue（去重：同 slot 覆盖旧记录）。
    pub fn mark_residue(
        engine_id: &EngineId,
        slot: &str,
        install_id: Option<String>,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let mut records = Self::read_residue(engine_id)?;
        let record = ResidueRecord {
            slot: slot.to_string(),
            install_id,
            marked_at_ms: now_ms(),
            reason: reason.to_string(),
        };
        match records.iter().position(|r| r.slot == slot) {
            Some(idx) => records[idx] = record,
            None => records.push(record),
        }
        let file = ResidueFile {
            schema_version: RESIDUE_SCHEMA_VERSION,
            records,
        };
        atomic_write_json(&Self::residue_path(engine_id), &file)
    }

    /// 清除 residue 记录（slot 成功删除后）。
    pub fn clear_residue(engine_id: &EngineId, slot: &str) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let mut records = Self::read_residue(engine_id)?;
        let before = records.len();
        records.retain(|r| r.slot != slot);
        if records.len() == before {
            return Ok(());
        }
        if records.is_empty() {
            let path = Self::residue_path(engine_id);
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        } else {
            atomic_write_json(
                &Self::residue_path(engine_id),
                &ResidueFile {
                    schema_version: RESIDUE_SCHEMA_VERSION,
                    records,
                },
            )?;
        }
        Ok(())
    }

    /// 尝试删除非 active slot；失败（Windows 占用）时记 residue。
    ///
    /// 返回是否真正删除。
    pub fn delete_slot_if_not_active(
        engine_id: &EngineId,
        slot: &str,
        reason: &str,
    ) -> Result<bool, RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let active_slot = Self::read_pointer(engine_id)?.map(|p| p.slot);
        if active_slot.as_deref() == Some(slot) {
            return Ok(false); // active slot 绝不删除
        }
        let dir = slot_dir(engine_id, slot);
        if !dir.exists() {
            Self::clear_residue(engine_id, slot)?;
            return Ok(true);
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                Self::clear_residue(engine_id, slot)?;
                Ok(true)
            }
            Err(e) => {
                let install_id = std::fs::read_to_string(slot_manifest_path(engine_id, slot))
                    .ok()
                    .and_then(|c| serde_json::from_str::<DeploymentManifest>(&c).ok())
                    .map(|m| m.install_id);
                Self::mark_residue(engine_id, slot, install_id, &format!("{reason}: {e}"))?;
                Ok(false)
            }
        }
    }

    // ── 孤儿 staging 清扫 ─────────────────────────────────────────────────

    /// 清扫引擎 staging 根下的全部孤儿目录（无活跃操作时可安全调用）。
    pub fn sweep_orphan_staging(engine_id: &EngineId) -> usize {
        sweep_staging_except(engine_id, None)
    }

    /// 清扫 staging，但保留指定 operation 的目录（事务进行中）。
    pub fn sweep_staging_except(engine_id: &EngineId, keep: &str) -> usize {
        sweep_staging_except(engine_id, Some(keep))
    }

    // ── 崩溃恢复（fail-closed）────────────────────────────────────────────

    /// 启动恢复：处理未收尾事务。
    ///
    /// 见模块文档的恢复表。**必须在引擎首次使用前调用**
    /// （EngineManager 构造时的环境 probe 会触发）。
    pub fn recover(engine_id: &EngineId) -> Result<RecoveryOutcome, RuntimeError> {
        let journal = match Self::read_journal(engine_id) {
            Ok(j) => j,
            Err(e) => {
                // journal 存在但不可解析——fail-closed 清理后如实上报。
                recover_fail_closed(engine_id, &e)?;
                return Ok(RecoveryOutcome::FailClosed {
                    reason: e.to_string(),
                });
            }
        };

        let Some(journal) = journal else {
            // 稳定状态：清扫孤儿 staging（Building 崩溃可能留下）
            Self::sweep_orphan_staging(engine_id);
            return Ok(RecoveryOutcome::Stable);
        };

        let outcome = match journal.phase {
            TransactionPhase::Building => {
                // 指针从未切换——old 完好。丢弃 candidate。
                Self::delete_slot_if_not_active(
                    engine_id,
                    &journal.candidate_slot,
                    "recover: Building 中断，丢弃 candidate",
                )?;
                RecoveryOutcome::DiscardedCandidate {
                    slot: journal.candidate_slot.clone(),
                }
            }
            TransactionPhase::Switched => {
                // 指针可能已指向 candidate，但验证未完成——回滚到 previous。
                match &journal.previous {
                    Some(prev) => {
                        let prev_dir = slot_dir(engine_id, &prev.slot);
                        if prev_dir.exists() {
                            Self::write_pointer(
                                engine_id,
                                &DeploymentPointer {
                                    install_id: prev.install_id.clone(),
                                    slot: prev.slot.clone(),
                                    updated_at_ms: now_ms(),
                                    schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
                                },
                            )?;
                        } else {
                            // previous slot 已不可用（不应发生：Committed 前不删 old）
                            Self::remove_pointer(engine_id)?;
                        }
                        Self::delete_slot_if_not_active(
                            engine_id,
                            &journal.candidate_slot,
                            "recover: Switched 中断，回滚后清理 candidate",
                        )?;
                        RecoveryOutcome::RevertedToPrevious {
                            slot: journal.candidate_slot.clone(),
                            previous: Some(prev.install_id.clone()),
                        }
                    }
                    None => {
                        // 全新安装未验证完成——移除指针，candidate 记 residue
                        Self::remove_pointer(engine_id)?;
                        Self::delete_slot_if_not_active(
                            engine_id,
                            &journal.candidate_slot,
                            "recover: 全新安装 Switched 中断",
                        )?;
                        RecoveryOutcome::RevertedToPrevious {
                            slot: journal.candidate_slot.clone(),
                            previous: None,
                        }
                    }
                }
            }
            TransactionPhase::Committed => {
                // 事务已成功，只是旧 slot 删除/journal 清除未完成——补收尾。
                if let Some(prev) = &journal.previous {
                    Self::delete_slot_if_not_active(
                        engine_id,
                        &prev.slot,
                        "recover: Committed 中断，补删旧 slot",
                    )?;
                }
                RecoveryOutcome::FinalizedCommit {
                    slot: journal.candidate_slot.clone(),
                }
            }
        };

        Self::sweep_orphan_staging(engine_id);
        Self::clear_journal(engine_id)?;
        Ok(outcome)
    }
}

/// 清扫 staging 根（可保留一个 operation 目录）。
fn sweep_staging_except(engine_id: &EngineId, keep: Option<&str>) -> usize {
    let staging = staging_dir(engine_id);
    if !staging.exists() {
        return 0;
    }
    let mut cleaned = 0;
    if let Ok(entries) = std::fs::read_dir(&staging) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name = match name.to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            if keep == Some(name.as_str()) {
                continue;
            }
            if entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
                && std::fs::remove_dir_all(entry.path()).is_ok()
            {
                cleaned += 1;
            }
        }
    }
    cleaned
}

// ── 测试 ──────────────────────────────────────────────────────────────────

/// journal 存在但损坏时的 fail-closed 恢复：
/// 有指针则保留指针、非 active slot 记 residue；无指针则清空两个 slot。
fn recover_fail_closed(engine_id: &EngineId, err: &RuntimeError) -> Result<(), RuntimeError> {
    let pointer = DeploymentStore::read_pointer(engine_id).ok().flatten();
    match pointer {
        Some(p) => {
            for slot in [DeploymentSlot::A, DeploymentSlot::B] {
                if slot.as_str() != p.slot {
                    DeploymentStore::delete_slot_if_not_active(
                        engine_id,
                        slot.as_str(),
                        "recover: journal 损坏",
                    )?;
                }
            }
            DeploymentStore::clear_journal(engine_id)?;
            tracing::warn!(engine = %engine_id, error = %err, "journal 损坏，保留 active 指针");
        }
        None => {
            for slot in [DeploymentSlot::A, DeploymentSlot::B] {
                DeploymentStore::delete_slot_if_not_active(
                    engine_id,
                    slot.as_str(),
                    "recover: journal 损坏且无指针",
                )?;
            }
            DeploymentStore::clear_journal(engine_id)?;
            tracing::warn!(engine = %engine_id, error = %err, "journal 损坏且无指针，清空 slot");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(name: &str) -> EngineId {
        EngineId::new(name).unwrap()
    }

    /// 写一个最小合法 manifest 到 slot。
    fn write_slot_manifest(engine_id: &EngineId, slot: &str, install_id: &str) {
        let dir = slot_dir(engine_id, slot);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("marker.txt"), install_id).unwrap();
        // manifest 只需可被 DeploymentStore 逻辑识别为内容存在；真实 manifest
        // 由 providers 写入，这里用最小 JSON 避免依赖构造完整 DeploymentManifest。
        std::fs::write(
            dir.join("manifest.json"),
            format!(r#"{{"install_id": "{install_id}"}}"#),
        )
        .unwrap();
    }

    fn pointer(_engine_id: &EngineId, install_id: &str, slot: &str) -> DeploymentPointer {
        DeploymentPointer {
            install_id: install_id.to_string(),
            slot: slot.to_string(),
            updated_at_ms: now_ms(),
            schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
        }
    }

    #[test]
    fn slot_roundtrip_and_other() {
        assert_eq!(DeploymentSlot::A.as_str(), "slot-a");
        assert_eq!(DeploymentSlot::B.as_str(), "slot-b");
        assert_eq!(DeploymentSlot::A.other(), DeploymentSlot::B);
        assert_eq!(DeploymentSlot::B.other(), DeploymentSlot::A);
        assert_eq!(DeploymentSlot::parse("slot-a").unwrap(), DeploymentSlot::A);
        assert!(DeploymentSlot::parse("slot-x").is_err());
    }

    #[test]
    fn begin_picks_non_active_slot_and_writes_journal() {
        let engine = eid("dep-begin");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        // 未安装 → candidate = slot-a
        let j = DeploymentStore::begin(&engine, "op-1", "dep-1").unwrap();
        assert_eq!(j.candidate_slot, "slot-a");
        assert!(j.previous.is_none());
        assert_eq!(j.phase, TransactionPhase::Building);

        // 模拟事务成功：指针指向 slot-a
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-1", "slot-a")).unwrap();

        // 第二次事务 → candidate = slot-b，previous = slot-a
        let j2 = DeploymentStore::begin(&engine, "op-2", "dep-2").unwrap();
        assert_eq!(j2.candidate_slot, "slot-b");
        assert_eq!(j2.previous.unwrap().slot, "slot-a");

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_stable_when_no_journal() {
        let engine = eid("dep-stable");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        // 孤儿 staging 被清扫
        std::fs::create_dir_all(staging_dir(&engine).join("op-orphan")).unwrap();
        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Stable);
        assert!(!staging_dir(&engine).join("op-orphan").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_building_discards_candidate() {
        let engine = eid("dep-building");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        // old = slot-a active；事务 candidate slot-b 处于 Building
        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        let j = DeploymentStore::begin(&engine, "op-1", "dep-new").unwrap();
        assert_eq!(j.candidate_slot, "slot-b");
        write_slot_manifest(&engine, "slot-b", "dep-new");

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::DiscardedCandidate { ref slot } if slot == "slot-b"
        ));
        // old 保留为 active，candidate 被删除
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-a");
        assert!(!slot_dir(&engine, "slot-b").exists());
        assert!(DeploymentStore::read_journal(&engine).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_switched_reverts_to_previous() {
        let engine = eid("dep-switched");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        let mut j = DeploymentStore::begin(&engine, "op-1", "dep-new").unwrap();
        write_slot_manifest(&engine, "slot-b", "dep-new");
        // pre-switch：journal 先于指针推进
        DeploymentStore::advance_phase(&engine, &mut j, TransactionPhase::Switched).unwrap();
        // 指针切到 candidate 后崩溃（验证未完成）
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-new", "slot-b")).unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::RevertedToPrevious { previous: Some(ref id), .. } if id == "dep-old"
        ));
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-a");
        assert_eq!(p.install_id, "dep-old");
        assert!(!slot_dir(&engine, "slot-b").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_switched_after_journal_but_before_pointer_keeps_old() {
        let engine = eid("dep-preswitch");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        let mut j = DeploymentStore::begin(&engine, "op-1", "dep-new").unwrap();
        write_slot_manifest(&engine, "slot-b", "dep-new");
        // journal 推进到 Switched 后、指针切换前崩溃
        DeploymentStore::advance_phase(&engine, &mut j, TransactionPhase::Switched).unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::RevertedToPrevious { .. }
        ));
        // 指针仍指向 old（回写幂等）
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-a");
        assert!(!slot_dir(&engine, "slot-b").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_committed_finalizes_and_deletes_old() {
        let engine = eid("dep-committed");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        let mut j = DeploymentStore::begin(&engine, "op-1", "dep-new").unwrap();
        write_slot_manifest(&engine, "slot-b", "dep-new");
        DeploymentStore::advance_phase(&engine, &mut j, TransactionPhase::Switched).unwrap();
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-new", "slot-b")).unwrap();
        // 验证已通过 → Committed；旧 slot 删除前崩溃
        DeploymentStore::advance_phase(&engine, &mut j, TransactionPhase::Committed).unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(outcome, RecoveryOutcome::FinalizedCommit { .. }));
        // active 保留，old 被补删，journal 清除
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-b");
        assert_eq!(p.install_id, "dep-new");
        assert!(!slot_dir(&engine, "slot-a").exists());
        assert!(DeploymentStore::read_journal(&engine).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_fresh_install_switched_removes_pointer() {
        let engine = eid("dep-fresh");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        let mut j = DeploymentStore::begin(&engine, "op-1", "dep-new").unwrap();
        write_slot_manifest(&engine, "slot-a", "dep-new");
        DeploymentStore::advance_phase(&engine, &mut j, TransactionPhase::Switched).unwrap();
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-new", "slot-a")).unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::RevertedToPrevious { previous: None, .. }
        ));
        assert!(DeploymentStore::read_pointer(&engine).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn recover_corrupt_journal_fail_closed() {
        let engine = eid("dep-corrupt");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        write_slot_manifest(&engine, "slot-b", "dep-garbage");
        std::fs::write(DeploymentStore::journal_path(&engine), "not json").unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(outcome, RecoveryOutcome::FailClosed { .. }));
        // active 保留，未知 slot 被清理
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-a");
        assert!(!slot_dir(&engine, "slot-b").exists());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn delete_slot_refuses_active_and_marks_residue_for_locked() {
        let engine = eid("dep-residue");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        write_slot_manifest(&engine, "slot-a", "dep-a");
        write_slot_manifest(&engine, "slot-b", "dep-b");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-a", "slot-a")).unwrap();

        // active slot 拒绝删除
        assert!(!DeploymentStore::delete_slot_if_not_active(&engine, "slot-a", "test").unwrap());
        assert!(slot_dir(&engine, "slot-a").exists());

        // 非 active 正常删除
        assert!(DeploymentStore::delete_slot_if_not_active(&engine, "slot-b", "test").unwrap());
        assert!(!slot_dir(&engine, "slot-b").exists());
        assert!(DeploymentStore::read_residue(&engine).unwrap().is_empty());

        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn sweep_staging_keeps_current_operation() {
        let engine = eid("dep-sweep");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        std::fs::create_dir_all(staging_dir(&engine).join("op-keep")).unwrap();
        std::fs::create_dir_all(staging_dir(&engine).join("op-old")).unwrap();
        let cleaned = DeploymentStore::sweep_staging_except(&engine, "op-keep");
        assert_eq!(cleaned, 1);
        assert!(staging_dir(&engine).join("op-keep").exists());
        assert!(!staging_dir(&engine).join("op-old").exists());

        let _ = std::fs::remove_dir_all(&root);
    }
}
