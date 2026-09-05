//! 部署事务存储（deployment key + slot + active pointer + transaction journal）。
//!
//! ## 部署寻址空间（0.22.9）
//!
//! deployment 的 key 是 **engine id + implementation id**（受限、编译期闭合）。
//! 一个 key 对应一个 `DeploymentSpace`——独立的 pointer、slot、journal、
//! residue 和 staging，互不覆盖：
//!
//! ```text
//! %APPDATA%\blink\runtimes\engines\{engine-id}\
//! ├─ slot-a\  slot-b\                     # engine 级 slot（0.22.7/0.22.8 兼容真源）
//! ├─ deployment.json                      # engine 级 active 指针
//! ├─ transaction.json                     # engine 级事务 journal
//! ├─ residue.json                         # engine 级清理残留
//! ├─ staging\{operation-id}\              # engine 级事务 staging
//! └─ impl-{implementation}\               # implementation 级空间（0.22.9 新增）
//!    ├─ slot-a\  slot-b\
//!    ├─ deployment.json
//!    ├─ transaction.json
//!    ├─ residue.json
//!    └─ staging\{operation-id}\
//! ```
//!
//! **兼容真源规则**（0.22.9 不强制搬迁用户资产）：
//!
//! - 0.22.7 FunASR GGUF 与 0.22.8 PaddleOCR ONNX 的 deployment 位于 engine 级
//!   空间，路径与历史版本完全一致；`DeploymentSpace::resolve` 把这两个
//!   implementation 显式映射到 engine 级空间——读取层"旧 pointer 即 GGUF
//!   implementation 的 deployment"，不存在复制、改写或搬迁。
//! - 0.22.9 起的新 implementation（如 ParaformerOnline ONNX worker）使用
//!   implementation 级空间，与 engine 级互不可见。
//! - 磁盘上出现无法映射到闭合枚举的 `impl-*` 目录（如高版本降级残留）时
//!   fail-closed：不当作 engine 级资产、不进入任何清理/删除路径，只记录
//!   警告；其 deployment.json 仍参与共享 artifact 引用扫描（保守方向）。
//!
//! ## 产品语义
//!
//! - 每个空间只有一个 **active deployment**（该空间 `deployment.json` 指向的
//!   不可变 slot）。
//! - 事务期间最多存在 **old + candidate** 两个 slot；安装/升级成功后稳定
//!   状态只保留 active——旧 slot 立即删除。
//! - 不存在世代历史，也不存在可回滚的"previous generation"产品状态；
//!   staging/rollback 只在事务内临时存在。
//! - 模型 payload 不在 deployment 空间内——继续以 engine + model 管理
//!   （`model_storage`），与 runtime deployment 正交。
//!
//! ## 事务协议（journal fail-closed，按空间独立执行）
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
//! 崩溃恢复 `recover`（启动时逐空间执行，fail-closed；一个空间的事务
//! 恢复绝不触碰另一个空间的指针与 slot）：
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

use crate::domain::local_engine::ImplementationId;

use super::runtime::{
    self, DeploymentManifest, EngineId, RuntimeError, atomic_write_json, now_ms,
    validate_operation_id, validate_slot_name,
};

// ── DeploymentSpace（deployment key：engine + implementation）───────────────

/// implementation 级空间目录前缀：`impl-{wire}`。
pub const IMPL_DIR_PREFIX: &str = "impl-";

/// `DeploymentSpace` 的 serde 投影（扁平形状：engine + 可选 implementation）。
///
/// 反序列化经 `resolve`/`engine` 闭合映射重建，保证不变量：
/// engine 级空间 ↔ `implementation: None`，implementation 级空间 ↔
/// `implementation: Some`——不会从磁盘数据构造出逃逸映射规则的空间。
#[derive(Debug, Serialize, Deserialize)]
struct DeploymentSpaceSerde {
    engine_id: EngineId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    implementation: Option<ImplementationId>,
}

impl serde::Serialize for DeploymentSpace {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        DeploymentSpaceSerde {
            engine_id: self.engine_id.clone(),
            implementation: self.implementation(),
        }
        .serialize(serializer)
    }
}

impl<'de> serde::Deserialize<'de> for DeploymentSpace {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = DeploymentSpaceSerde::deserialize(deserializer)?;
        Ok(match raw.implementation {
            Some(implementation) => Self::resolve(&raw.engine_id, implementation),
            None => Self::engine(&raw.engine_id),
        })
    }
}

/// 部署寻址空间——deployment key（engine id + implementation）的磁盘投影。
///
/// 同一产品 engine 下的每个 implementation 拥有独立的 pointer/slot/journal/
/// residue/staging；一个 implementation 的安装、回滚、清理与恢复只作用于
/// 本空间，绝不触碰其他空间。
///
/// 空间由 `resolve`（闭合映射）或 `engine`（显式 engine 级）构造，
/// 不接受任意路径或字符串作用域。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct DeploymentSpace {
    engine_id: EngineId,
    scope: SpaceScope,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum SpaceScope {
    /// engine 级空间——0.22.7 GGUF / 0.22.8 OCR ONNX 的兼容真源，
    /// 路径与历史版本字节一致。
    Engine,
    /// implementation 级空间——`engines/{engine}/impl-{implementation}/`。
    // handoff-11 后闭合映射内无使用者；机制保留，测试经直接构造覆盖
    #[cfg_attr(not(test), allow(dead_code))]
    Implementation(ImplementationId),
}

impl DeploymentSpace {
    /// engine 级空间（0.22.7/0.22.8 兼容真源；亦为无 implementation 声明的
    /// 测试 fake 引擎的默认空间）。
    pub fn engine(engine_id: &EngineId) -> Self {
        Self {
            engine_id: engine_id.clone(),
            scope: SpaceScope::Engine,
        }
    }

    /// implementation → 空间的**闭合映射**（唯一真源）。
    ///
    /// 新增 `ImplementationId` 变体时必须在此显式决策空间归属：
    /// - 0.22.7 GGUF / 0.22.8 OCR ONNX：engine 级空间（兼容真源，不搬迁）；
    /// - 0.22.9 起新实现：implementation 级空间。
    ///
    /// implementation 的 engine 归属由调用方保证（Manager 从编译期绑定表
    /// 解析后才调用本函数；跨 engine 归属在注册表构造期已拒绝）。
    pub fn resolve(engine_id: &EngineId, implementation: ImplementationId) -> Self {
        match implementation {
            // 0.22.7：FunASR GGUF engine-level deployment 保持兼容真源
            ImplementationId::FunasrGgufWorker => Self::engine(engine_id),
            // 0.22.8：PaddleOCR ONNX in-process deployment 保持 engine 级
            ImplementationId::PaddleOcrOnnxInProcess => Self::engine(engine_id),
        }
    }

    /// 所属 engine id。
    pub fn engine_id(&self) -> &EngineId {
        &self.engine_id
    }

    /// 空间对应的 implementation（engine 级空间为 `None`）。
    pub fn implementation(&self) -> Option<ImplementationId> {
        match &self.scope {
            SpaceScope::Engine => None,
            SpaceScope::Implementation(id) => Some(*id),
        }
    }

    /// 空间根目录：engine 级 = `engines/{engine}`；
    /// implementation 级 = `engines/{engine}/impl-{wire}`。
    pub fn root(&self) -> PathBuf {
        match &self.scope {
            SpaceScope::Engine => runtime::engine_root(&self.engine_id),
            SpaceScope::Implementation(id) => {
                runtime::engine_root(&self.engine_id).join(Self::impl_dir_name(*id))
            }
        }
    }

    /// 测试专用：直接构造 implementation 级空间。
    ///
    /// handoff-11 后闭合映射内已无 implementation 级实现——crate 内测试
    /// （providers/manager 双空间隔离等）经此构造覆盖通用机制。
    #[cfg(test)]
    pub(crate) fn impl_space_for_test(engine_id: &EngineId) -> Self {
        Self {
            engine_id: engine_id.clone(),
            scope: SpaceScope::Implementation(ImplementationId::FunasrGgufWorker),
        }
    }

    /// implementation 级空间目录名（`impl-{wire}`；wire 值只含 `[a-z0-9_]`，
    /// 由编译期闭合枚举派生，不存在路径逃逸输入）。
    pub fn impl_dir_name(implementation: ImplementationId) -> String {
        format!("{IMPL_DIR_PREFIX}{}", implementation.as_str())
    }

    /// 从磁盘目录名反解 implementation（未知名字 fail-closed 返回 `None`）。
    pub fn parse_impl_dir_name(dir_name: &str) -> Option<ImplementationId> {
        let wire = dir_name.strip_prefix(IMPL_DIR_PREFIX)?;
        ImplementationId::parse_wire(wire)
    }

    /// `deployment.json` 路径。
    pub fn pointer_path(&self) -> PathBuf {
        self.root().join("deployment.json")
    }

    /// `transaction.json` 路径。
    pub fn journal_path(&self) -> PathBuf {
        self.root().join("transaction.json")
    }

    /// `residue.json` 路径。
    pub fn residue_path(&self) -> PathBuf {
        self.root().join("residue.json")
    }

    /// slot 目录路径。
    pub fn slot_dir(&self, slot: &str) -> PathBuf {
        self.root().join(slot)
    }

    /// slot 内 manifest 路径。
    pub fn slot_manifest_path(&self, slot: &str) -> PathBuf {
        self.slot_dir(slot).join("manifest.json")
    }

    /// 空间 staging 根目录。
    pub fn staging_dir(&self) -> PathBuf {
        self.root().join("staging")
    }

    /// 单个 operation 的 staging 目录。
    pub fn operation_staging_dir(&self, operation_id: &str) -> PathBuf {
        self.staging_dir().join(operation_id)
    }
}

// ── DeploymentSlot ─────────────────────────────────────────────────────────

/// 部署 slot（物理位置，闭合两槽轮换；每个空间独立持有 a/b 两槽）。
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

    /// 空间内 slot 目录路径。
    pub fn dir_in(&self, space: &DeploymentSpace) -> PathBuf {
        space.slot_dir(self.as_str())
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

/// `deployment.json`——active 部署指针（所属空间内唯一业务真相的磁盘投影）。
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

/// `transaction.json`——事务 journal。存在即表示该空间有事务未收尾。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransactionJournal {
    pub schema_version: u32,
    pub engine_id: String,
    /// 事务所属 implementation 的 wire 值（engine 级空间为 `null`/缺失，
    /// 兼容 0.22.9 之前写入的 journal）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<String>,
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

/// 无状态部署存储原语（所有操作按 `DeploymentSpace` 寻址磁盘）。
///
/// 事务编排（provider 调用、self-test、事件）在 providers 层；
/// 本模块只负责指针/journal/residue/slot 的原子磁盘操作与恢复。
/// 空间是唯一寻址单位——不存在绕过空间直接按 engine id 寻址 deployment
/// 的入口（模型 payload 走 `model_storage`，与本模块正交）。
pub struct DeploymentStore;

impl DeploymentStore {
    // ── 指针 ──────────────────────────────────────────────────────────────

    /// 读取 active 指针（None = 该空间未安装）。
    pub fn read_pointer(
        space: &DeploymentSpace,
    ) -> Result<Option<DeploymentPointer>, RuntimeError> {
        let path = space.pointer_path();
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
        space: &DeploymentSpace,
        pointer: &DeploymentPointer,
    ) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(&pointer.slot)?;
        atomic_write_json(&space.pointer_path(), pointer)
    }

    /// 删除 active 指针（仅恢复路径使用——全新安装事务失败且无 previous 时）。
    pub fn remove_pointer(space: &DeploymentSpace) -> Result<(), RuntimeError> {
        let path = space.pointer_path();
        if path.exists() {
            std::fs::remove_file(&path)?;
        }
        Ok(())
    }

    /// 读取 active 部署（指针 + manifest）。None = 该空间未安装。
    pub fn read_active(
        space: &DeploymentSpace,
    ) -> Result<Option<(DeploymentPointer, DeploymentManifest)>, RuntimeError> {
        match Self::read_pointer(space)? {
            None => Ok(None),
            Some(pointer) => {
                let manifest = Self::read_slot_manifest(space, &pointer.slot)?;
                Ok(Some((pointer, manifest)))
            }
        }
    }

    /// active 部署目录（adapter 解析 worker exe 等用）。None = 未安装。
    pub fn active_dir(
        space: &DeploymentSpace,
    ) -> Result<Option<(DeploymentPointer, PathBuf)>, RuntimeError> {
        match Self::read_pointer(space)? {
            None => Ok(None),
            Some(pointer) => Ok(Some((pointer.clone(), space.slot_dir(&pointer.slot)))),
        }
    }

    /// 删除 implementation 级部署空间整体（模型资产卸载，0.22.9 Handoff 08）。
    ///
    /// **仅限 implementation 级空间**——engine 级空间是 0.22.7 GGUF /
    /// 0.22.8 OCR 的兼容真源，整体删除被拒绝。空间不存在时幂等成功。
    /// 空间内有未收尾事务时先按 fail-closed 规则收尾（避免删掉 half-state）；
    /// 调用方负责操作串行（manager 层 engine 级操作互斥已保证）。
    // handoff-11：ONNX 模型删除路径退役后生产暂无调用方；通用机制保留
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn remove_impl_space(space: &DeploymentSpace) -> Result<(), RuntimeError> {
        if space.implementation().is_none() {
            return Err(RuntimeError::CleanupFailed {
                message: "engine 级部署空间不可整体删除（多模型共享 runtime 兼容真源）".to_string(),
            });
        }
        Self::recover(space)?;
        let root = space.root();
        if root.exists() {
            std::fs::remove_dir_all(&root)?;
        }
        Ok(())
    }

    // ── slot manifest ─────────────────────────────────────────────────────

    /// 读取空间内部署 slot 的 manifest（schema 校验）。
    pub fn read_slot_manifest(
        space: &DeploymentSpace,
        slot: &str,
    ) -> Result<DeploymentManifest, RuntimeError> {
        validate_slot_name(slot)?;
        let path = space.slot_manifest_path(slot);
        if !path.exists() {
            return Err(RuntimeError::GenerationNotFound {
                install_id: slot.to_string(),
            });
        }
        let content = std::fs::read_to_string(&path)?;
        let manifest: DeploymentManifest =
            serde_json::from_str(&content).map_err(|e| RuntimeError::ManifestParseFailed {
                message: format!("{e}"),
            })?;
        if manifest.schema_version != runtime::MANIFEST_SCHEMA_VERSION {
            return Err(RuntimeError::ManifestSchemaIncompatible {
                expected: runtime::MANIFEST_SCHEMA_VERSION,
                actual: manifest.schema_version,
            });
        }
        Ok(manifest)
    }

    // ── journal ───────────────────────────────────────────────────────────

    /// 读取 journal（None = 无未收尾事务）。
    pub fn read_journal(
        space: &DeploymentSpace,
    ) -> Result<Option<TransactionJournal>, RuntimeError> {
        let path = space.journal_path();
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
        space: &DeploymentSpace,
        journal: &TransactionJournal,
    ) -> Result<(), RuntimeError> {
        atomic_write_json(&space.journal_path(), journal)
    }

    /// 清除 journal（事务收尾的最后一步）。
    pub fn clear_journal(space: &DeploymentSpace) -> Result<(), RuntimeError> {
        let path = space.journal_path();
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
        space: &DeploymentSpace,
        operation_id: &str,
        candidate_install_id: &str,
    ) -> Result<TransactionJournal, RuntimeError> {
        validate_operation_id(operation_id)?;
        let previous = Self::read_pointer(space)?.map(|p| PreviousDeployment {
            install_id: p.install_id,
            slot: p.slot,
        });
        let candidate_slot = match &previous {
            Some(prev) => DeploymentSlot::parse(&prev.slot)?.other(),
            None => DeploymentSlot::A,
        };

        // 清空 candidate 槽位旧内容（上一事务残留）
        let cand_dir = space.slot_dir(candidate_slot.as_str());
        if cand_dir.exists() {
            match std::fs::remove_dir_all(&cand_dir) {
                Ok(()) => {}
                Err(e) => {
                    Self::mark_residue(
                        space,
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
            engine_id: space.engine_id().as_str().to_string(),
            implementation: space.implementation().map(|i| i.as_str().to_string()),
            operation_id: operation_id.to_string(),
            candidate_slot: candidate_slot.as_str().to_string(),
            candidate_install_id: candidate_install_id.to_string(),
            previous,
            phase: TransactionPhase::Building,
            started_at_ms: now_ms(),
        };
        Self::write_journal(space, &journal)?;
        Ok(journal)
    }

    /// journal 阶段推进（pre-switch / commit 前调用）。
    pub fn advance_phase(
        space: &DeploymentSpace,
        journal: &mut TransactionJournal,
        phase: TransactionPhase,
    ) -> Result<(), RuntimeError> {
        journal.phase = phase;
        Self::write_journal(space, journal)
    }

    // ── residue ───────────────────────────────────────────────────────────

    /// 读取 residue 记录。
    pub fn read_residue(space: &DeploymentSpace) -> Result<Vec<ResidueRecord>, RuntimeError> {
        let path = space.residue_path();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let content = std::fs::read_to_string(&path)?;
        let file: ResidueFile = serde_json::from_str(&content)?;
        Ok(file.records)
    }

    /// 登记 residue（去重：同 slot 覆盖旧记录）。
    pub fn mark_residue(
        space: &DeploymentSpace,
        slot: &str,
        install_id: Option<String>,
        reason: &str,
    ) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let mut records = Self::read_residue(space)?;
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
        atomic_write_json(&space.residue_path(), &file)
    }

    /// 清除 residue 记录（slot 成功删除后）。
    pub fn clear_residue(space: &DeploymentSpace, slot: &str) -> Result<(), RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let mut records = Self::read_residue(space)?;
        let before = records.len();
        records.retain(|r| r.slot != slot);
        if records.len() == before {
            return Ok(());
        }
        if records.is_empty() {
            let path = space.residue_path();
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
        } else {
            atomic_write_json(
                &space.residue_path(),
                &ResidueFile {
                    schema_version: RESIDUE_SCHEMA_VERSION,
                    records,
                },
            )?;
        }
        Ok(())
    }

    /// 尝试删除空间内非 active slot；失败（Windows 占用）时记 residue。
    ///
    /// active 判定只看**本空间**指针——一个 implementation 的 active slot
    /// 不会被另一个空间的清理误删（删除保护按 implementation 生效）。
    ///
    /// 返回是否真正删除。
    pub fn delete_slot_if_not_active(
        space: &DeploymentSpace,
        slot: &str,
        reason: &str,
    ) -> Result<bool, RuntimeError> {
        runtime::validate_slot_name(slot)?;
        let active_slot = Self::read_pointer(space)?.map(|p| p.slot);
        if active_slot.as_deref() == Some(slot) {
            return Ok(false); // 本空间 active slot 绝不删除
        }
        let dir = space.slot_dir(slot);
        if !dir.exists() {
            Self::clear_residue(space, slot)?;
            return Ok(true);
        }
        match std::fs::remove_dir_all(&dir) {
            Ok(()) => {
                Self::clear_residue(space, slot)?;
                Ok(true)
            }
            Err(e) => {
                let install_id = std::fs::read_to_string(space.slot_manifest_path(slot))
                    .ok()
                    .and_then(|c| serde_json::from_str::<DeploymentManifest>(&c).ok())
                    .map(|m| m.install_id);
                Self::mark_residue(space, slot, install_id, &format!("{reason}: {e}"))?;
                Ok(false)
            }
        }
    }

    // ── 孤儿 staging 清扫 ─────────────────────────────────────────────────

    /// 清扫空间 staging 根下的全部孤儿目录（无活跃操作时可安全调用）。
    pub fn sweep_orphan_staging(space: &DeploymentSpace) -> usize {
        sweep_staging_in(space, None)
    }

    /// 清扫空间 staging，但保留指定 operation 的目录（事务进行中）。
    pub fn sweep_staging_except(space: &DeploymentSpace, keep: &str) -> usize {
        sweep_staging_in(space, Some(keep))
    }

    // ── 空间枚举 ──────────────────────────────────────────────────────────

    /// 枚举引擎当前在磁盘上拥有的全部部署空间。
    ///
    /// 总是包含 engine 级空间；engine 根下的 `impl-{wire}` 子目录若能映射
    /// 到闭合枚举则加入。无法映射的 `impl-*` 目录（高版本降级残留等）
    /// fail-closed：不返回、不清理、只记警告——任何删除路径都不可达。
    pub fn spaces_for_engine(engine_id: &EngineId) -> Result<Vec<DeploymentSpace>, RuntimeError> {
        let mut spaces = vec![DeploymentSpace::engine(engine_id)];
        let engine_root = runtime::engine_root(engine_id);
        if !engine_root.exists() {
            return Ok(spaces);
        }
        for entry in std::fs::read_dir(&engine_root)? {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = match entry.file_name().to_str() {
                Some(n) => n.to_string(),
                None => continue,
            };
            if !name.starts_with(IMPL_DIR_PREFIX) {
                continue;
            }
            match DeploymentSpace::parse_impl_dir_name(&name) {
                Some(implementation) => {
                    let space = DeploymentSpace::resolve(engine_id, implementation);
                    // 闭合映射落在 engine 级的 implementation（GGUF/OCR）已在
                    // 列表首位——不重复 push，避免同名空间双计
                    if space.implementation().is_some() {
                        spaces.push(space);
                    }
                }
                None => {
                    tracing::warn!(
                        engine = %engine_id,
                        dir = %name,
                        "发现未知 implementation 部署空间（可能来自更高版本）——\
                         不参与恢复/清理，保守保留"
                    );
                }
            }
        }
        Ok(spaces)
    }

    // ── 崩溃恢复（fail-closed，按空间独立执行）────────────────────────────

    /// 空间恢复：处理该空间未收尾的事务。
    ///
    /// 见模块文档的恢复表。**必须在空间首次使用前调用**
    /// （EngineManager 构造时的环境 probe 会对每个空间触发）。
    /// 恢复只作用于本空间的指针与 slot——其他空间（包括 engine 级）不受影响。
    pub fn recover(space: &DeploymentSpace) -> Result<RecoveryOutcome, RuntimeError> {
        let journal = match Self::read_journal(space) {
            Ok(j) => j,
            Err(e) => {
                // journal 存在但不可解析——fail-closed 清理后如实上报。
                recover_fail_closed(space, &e)?;
                return Ok(RecoveryOutcome::FailClosed {
                    reason: e.to_string(),
                });
            }
        };

        let Some(journal) = journal else {
            // 稳定状态：清扫孤儿 staging（Building 崩溃可能留下）
            Self::sweep_orphan_staging(space);
            return Ok(RecoveryOutcome::Stable);
        };

        let outcome = match journal.phase {
            TransactionPhase::Building => {
                // 指针从未切换——old 完好。丢弃 candidate。
                Self::delete_slot_if_not_active(
                    space,
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
                        let prev_dir = space.slot_dir(&prev.slot);
                        if prev_dir.exists() {
                            Self::write_pointer(
                                space,
                                &DeploymentPointer {
                                    install_id: prev.install_id.clone(),
                                    slot: prev.slot.clone(),
                                    updated_at_ms: now_ms(),
                                    schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
                                },
                            )?;
                        } else {
                            // previous slot 已不可用（不应发生：Committed 前不删 old）
                            Self::remove_pointer(space)?;
                        }
                        Self::delete_slot_if_not_active(
                            space,
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
                        Self::remove_pointer(space)?;
                        Self::delete_slot_if_not_active(
                            space,
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
                        space,
                        &prev.slot,
                        "recover: Committed 中断，补删旧 slot",
                    )?;
                }
                RecoveryOutcome::FinalizedCommit {
                    slot: journal.candidate_slot.clone(),
                }
            }
        };

        Self::sweep_orphan_staging(space);
        Self::clear_journal(space)?;
        Ok(outcome)
    }
}

/// 清扫空间 staging 根（可保留一个 operation 目录）。
fn sweep_staging_in(space: &DeploymentSpace, keep: Option<&str>) -> usize {
    let staging = space.staging_dir();
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
fn recover_fail_closed(space: &DeploymentSpace, err: &RuntimeError) -> Result<(), RuntimeError> {
    let pointer = DeploymentStore::read_pointer(space).ok().flatten();
    match pointer {
        Some(p) => {
            for slot in [DeploymentSlot::A, DeploymentSlot::B] {
                if slot.as_str() != p.slot {
                    DeploymentStore::delete_slot_if_not_active(
                        space,
                        slot.as_str(),
                        "recover: journal 损坏",
                    )?;
                }
            }
            DeploymentStore::clear_journal(space)?;
            tracing::warn!(
                engine = %space.engine_id(),
                implementation = ?space.implementation(),
                error = %err,
                "journal 损坏，保留 active 指针"
            );
        }
        None => {
            for slot in [DeploymentSlot::A, DeploymentSlot::B] {
                DeploymentStore::delete_slot_if_not_active(
                    space,
                    slot.as_str(),
                    "recover: journal 损坏且无指针",
                )?;
            }
            DeploymentStore::clear_journal(space)?;
            tracing::warn!(
                engine = %space.engine_id(),
                implementation = ?space.implementation(),
                error = %err,
                "journal 损坏且无指针，清空 slot"
            );
        }
    }
    Ok(())
}

/// 校验 implementation 级目录名（只允许 `[a-z0-9_]` 前缀映射产物，
/// 防御性约束——名字源自编译期闭合枚举，不接受外部构造）。
#[allow(dead_code)]
fn validate_impl_dir_name(name: &str) -> Result<(), RuntimeError> {
    let wire = name
        .strip_prefix(IMPL_DIR_PREFIX)
        .ok_or_else(|| RuntimeError::PathTraversal {
            path: name.to_string(),
        })?;
    if wire.is_empty()
        || !wire
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(RuntimeError::PathTraversal {
            path: name.to_string(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn eid(name: &str) -> EngineId {
        EngineId::new(name).unwrap()
    }

    /// engine 级空间便捷构造。
    fn eng(name: &str) -> DeploymentSpace {
        DeploymentSpace::engine(&eid(name))
    }

    /// implementation 级空间测试构造。
    ///
    /// handoff-11 后闭合映射内已无 implementation 级实现——直接构造
    /// `SpaceScope::Implementation` 以保持通用机制（双空间隔离/journal/
    /// residue）的测试覆盖，不经过 `resolve`。
    fn onnx_space(engine: &str) -> DeploymentSpace {
        DeploymentSpace {
            engine_id: eid(engine),
            scope: SpaceScope::Implementation(ImplementationId::FunasrGgufWorker),
        }
    }

    /// 写一个最小合法 manifest 到空间 slot。
    fn write_slot_manifest(space: &DeploymentSpace, slot: &str, install_id: &str) {
        let dir = space.slot_dir(slot);
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

    fn pointer(_space: &DeploymentSpace, install_id: &str, slot: &str) -> DeploymentPointer {
        DeploymentPointer {
            install_id: install_id.to_string(),
            slot: slot.to_string(),
            updated_at_ms: now_ms(),
            schema_version: DEPLOYMENT_POINTER_SCHEMA_VERSION,
        }
    }

    fn cleanup_space(space: &DeploymentSpace) {
        let _ = std::fs::remove_dir_all(space.root());
    }

    // ── 空间解析规则 ─────────────────────────────────────────────────────

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
    fn space_resolution_keeps_legacy_implementations_on_engine_root() {
        // 0.22.7 GGUF：engine 级空间是兼容真源——读取层把旧 pointer 明确
        // 映射到 GGUF implementation，路径与 0.22.8 字节一致。
        let fun = eid("funasr");
        let gguf = DeploymentSpace::resolve(&fun, ImplementationId::FunasrGgufWorker);
        assert_eq!(gguf.root(), runtime::engine_root(&fun));
        // implementation() 反映空间作用域：映射到 engine 级空间的 implementation
        // （GGUF / OCR in-process）其空间作用域是 Engine——存储投影/清理的
        // engine 级 target_id 语义因此保持不变。
        assert_eq!(gguf.implementation(), None);
        assert_eq!(
            gguf.pointer_path(),
            runtime::engine_root(&fun).join("deployment.json")
        );

        // 0.22.8 OCR ONNX in-process：同样保持 engine 级。
        let poc = eid("paddleocr");
        let ocr = DeploymentSpace::resolve(&poc, ImplementationId::PaddleOcrOnnxInProcess);
        assert_eq!(ocr.root(), runtime::engine_root(&poc));
        assert_eq!(ocr.implementation(), None);
    }

    #[test]
    fn implementation_scope_paths_live_in_impl_subdir() {
        let fun = eid("funasr");
        let onnx = onnx_space("funasr");
        assert_eq!(
            onnx.root(),
            runtime::engine_root(&fun).join("impl-funasr_gguf_worker")
        );
        // 空间内 pointer/journal/residue/slot 全部落在 impl 子目录
        assert_eq!(onnx.pointer_path(), onnx.root().join("deployment.json"));
        assert_eq!(onnx.journal_path(), onnx.root().join("transaction.json"));
        assert_eq!(onnx.residue_path(), onnx.root().join("residue.json"));
        assert_eq!(onnx.slot_dir("slot-a"), onnx.root().join("slot-a"));
        assert_eq!(
            onnx.staging_dir().join("op-1"),
            onnx.root().join("staging").join("op-1")
        );
        // engine 级空间与 impl 空间的 pointer 路径互不相同
        assert_ne!(
            onnx.pointer_path(),
            DeploymentSpace::engine(&fun).pointer_path()
        );
    }

    #[test]
    fn impl_dir_name_roundtrip_is_fail_closed() {
        assert_eq!(
            DeploymentSpace::parse_impl_dir_name("impl-funasr_gguf_worker"),
            Some(ImplementationId::FunasrGgufWorker)
        );
        // 未知 implementation 目录名不映射任何枚举（fail-closed）——
        // 包括 handoff-11 退役的 paraformer_onnx_worker
        assert_eq!(
            DeploymentSpace::parse_impl_dir_name("impl-paraformer_onnx_worker"),
            None
        );
        assert_eq!(
            DeploymentSpace::parse_impl_dir_name("impl-custom-worker"),
            None
        );
        assert_eq!(DeploymentSpace::parse_impl_dir_name("impl-"), None);
        assert_eq!(DeploymentSpace::parse_impl_dir_name("slot-a"), None);
        assert!(validate_impl_dir_name("impl-../escape").is_err());
        assert!(validate_impl_dir_name("impl-").is_err());
    }

    #[test]
    fn spaces_for_engine_enumerates_known_impl_dirs_only() {
        let engine = eid("space-enum");
        let root = runtime::engine_root(&engine);
        let _ = std::fs::remove_dir_all(&root);

        // 未安装任何空间 → 只有 engine 级
        assert_eq!(
            DeploymentStore::spaces_for_engine(&engine).unwrap().len(),
            1
        );

        // 已知 implementation 目录（闭合映射落 engine 级）不重复枚举
        std::fs::create_dir_all(root.join("impl-funasr_gguf_worker")).unwrap();
        // 未知/退役 implementation 目录被跳过（fail-closed，不映射默认）
        std::fs::create_dir_all(root.join("impl-paraformer_onnx_worker")).unwrap();
        std::fs::create_dir_all(root.join("impl-future_impl")).unwrap();
        let spaces = DeploymentStore::spaces_for_engine(&engine).unwrap();
        assert_eq!(spaces.len(), 1);
        assert!(spaces.contains(&DeploymentSpace::engine(&engine)));

        let _ = std::fs::remove_dir_all(&root);
    }

    // ── 指针 / begin ─────────────────────────────────────────────────────

    #[test]
    fn begin_picks_non_active_slot_and_writes_journal() {
        let engine = eng("dep-begin");
        cleanup_space(&engine);

        // 未安装 → candidate = slot-a
        let j = DeploymentStore::begin(&engine, "op-1", "dep-1").unwrap();
        assert_eq!(j.candidate_slot, "slot-a");
        assert!(j.previous.is_none());
        assert_eq!(j.phase, TransactionPhase::Building);
        // engine 级 journal 不携带 implementation 字段（兼容旧格式）
        assert!(j.implementation.is_none());

        // 模拟事务成功：指针指向 slot-a
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-1", "slot-a")).unwrap();

        // 第二次事务 → candidate = slot-b，previous = slot-a
        let j2 = DeploymentStore::begin(&engine, "op-2", "dep-2").unwrap();
        assert_eq!(j2.candidate_slot, "slot-b");
        assert_eq!(j2.previous.unwrap().slot, "slot-a");

        cleanup_space(&engine);
    }

    #[test]
    fn begin_in_impl_space_writes_scoped_journal() {
        let space = onnx_space("dep-impl-begin");
        cleanup_space(&space);

        let j = DeploymentStore::begin(&space, "op-1", "dep-1").unwrap();
        assert_eq!(j.implementation.as_deref(), Some("funasr_gguf_worker"));
        assert_eq!(j.engine_id, "dep-impl-begin");
        // journal 物理位置在 impl 子目录
        assert!(space.journal_path().exists());
        // engine 级空间无 journal
        assert!(
            !DeploymentSpace::engine(&eid("dep-impl-begin"))
                .journal_path()
                .exists()
        );

        cleanup_space(&space);
    }

    // ── 双空间隔离（pointer / slot / journal 互不覆盖）────────────────────

    #[test]
    fn dual_spaces_keep_independent_pointers_and_slots() {
        let engine = eng("dep-dual");
        let impl_space = onnx_space("dep-dual");
        cleanup_space(&engine);
        cleanup_space(&impl_space);

        // engine 级事务：slot-a
        let j1 = DeploymentStore::begin(&engine, "op-e1", "dep-gguf").unwrap();
        assert_eq!(j1.candidate_slot, "slot-a");
        write_slot_manifest(&engine, "slot-a", "dep-gguf");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-gguf", "slot-a")).unwrap();

        // implementation 空间事务：独立从 slot-a 开始，不受 engine 指针影响
        let j2 = DeploymentStore::begin(&impl_space, "op-i1", "dep-onnx").unwrap();
        assert_eq!(j2.candidate_slot, "slot-a", "impl 空间独立轮换自己的槽位");
        write_slot_manifest(&impl_space, "slot-a", "dep-onnx");
        DeploymentStore::write_pointer(&impl_space, &pointer(&impl_space, "dep-onnx", "slot-a"))
            .unwrap();

        // 两条 pointer、slot、journal 同时存在且互不覆盖
        let p_engine = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        let p_impl = DeploymentStore::read_pointer(&impl_space).unwrap().unwrap();
        assert_eq!(p_engine.install_id, "dep-gguf");
        assert_eq!(p_impl.install_id, "dep-onnx");
        assert!(engine.slot_dir("slot-a").join("marker.txt").exists());
        assert!(impl_space.slot_dir("slot-a").join("marker.txt").exists());
        assert_ne!(engine.slot_dir("slot-a"), impl_space.slot_dir("slot-a"));

        // implementation 空间第二轮事务用 slot-b，engine 级 slot-b 仍空闲
        let j3 = DeploymentStore::begin(&impl_space, "op-i2", "dep-onnx-2").unwrap();
        assert_eq!(j3.candidate_slot, "slot-b");
        assert!(!engine.slot_dir("slot-b").exists());

        cleanup_space(&engine);
        cleanup_space(&impl_space);
    }

    #[test]
    fn delete_slot_protection_is_per_space() {
        let engine = eng("dep-protect");
        let impl_space = onnx_space("dep-protect");
        cleanup_space(&engine);
        cleanup_space(&impl_space);

        // 两个空间各有 active slot-a 与残留 slot-b
        write_slot_manifest(&engine, "slot-a", "dep-gguf");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-gguf", "slot-a")).unwrap();
        write_slot_manifest(&impl_space, "slot-a", "dep-onnx");
        DeploymentStore::write_pointer(&impl_space, &pointer(&impl_space, "dep-onnx", "slot-a"))
            .unwrap();
        write_slot_manifest(&impl_space, "slot-b", "dep-onnx-old");

        // engine 级 active 拒绝删除（engine 空间判定）
        assert!(!DeploymentStore::delete_slot_if_not_active(&engine, "slot-a", "test").unwrap());
        assert!(engine.slot_dir("slot-a").exists());

        // impl 空间的 slot-a 不是 engine 空间的 active——按 impl 空间判定
        // 其是否 active：是 → 同样拒绝（但物理目录是 impl 下的 slot-a）
        assert!(
            !DeploymentStore::delete_slot_if_not_active(&impl_space, "slot-a", "test").unwrap()
        );
        assert!(impl_space.slot_dir("slot-a").exists());

        // impl 空间非 active slot-b 正常删除
        assert!(DeploymentStore::delete_slot_if_not_active(&impl_space, "slot-b", "test").unwrap());
        assert!(!impl_space.slot_dir("slot-b").exists());
        // engine 级 slot-b 不存在也未受影响
        assert!(!engine.slot_dir("slot-b").exists());

        cleanup_space(&engine);
        cleanup_space(&impl_space);
    }

    #[test]
    fn residue_is_isolated_per_space() {
        let engine = eng("dep-residue-dual");
        let impl_space = onnx_space("dep-residue-dual");
        cleanup_space(&engine);
        cleanup_space(&impl_space);

        DeploymentStore::mark_residue(&engine, "slot-b", None, "engine 级占用").unwrap();
        DeploymentStore::mark_residue(&impl_space, "slot-b", None, "impl 级占用").unwrap();

        assert_eq!(DeploymentStore::read_residue(&engine).unwrap().len(), 1);
        assert_eq!(DeploymentStore::read_residue(&impl_space).unwrap().len(), 1);
        // 清除 impl 空间 residue 不影响 engine 级记录
        DeploymentStore::clear_residue(&impl_space, "slot-b").unwrap();
        assert!(
            DeploymentStore::read_residue(&impl_space)
                .unwrap()
                .is_empty()
        );
        assert_eq!(DeploymentStore::read_residue(&engine).unwrap().len(), 1);

        cleanup_space(&engine);
        cleanup_space(&impl_space);
    }

    // ── 恢复（按空间独立）────────────────────────────────────────────────

    #[test]
    fn recover_stable_when_no_journal() {
        let engine = eng("dep-stable");
        cleanup_space(&engine);

        // 孤儿 staging 被清扫
        std::fs::create_dir_all(engine.staging_dir().join("op-orphan")).unwrap();
        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Stable);
        assert!(!engine.staging_dir().join("op-orphan").exists());

        cleanup_space(&engine);
    }

    #[test]
    fn recover_building_discards_candidate() {
        let engine = eng("dep-building");
        cleanup_space(&engine);

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
        assert!(!engine.slot_dir("slot-b").exists());
        assert!(DeploymentStore::read_journal(&engine).unwrap().is_none());

        cleanup_space(&engine);
    }

    #[test]
    fn recover_switched_reverts_to_previous() {
        let engine = eng("dep-switched");
        cleanup_space(&engine);

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
        assert!(!engine.slot_dir("slot-b").exists());

        cleanup_space(&engine);
    }

    #[test]
    fn recover_switched_after_journal_but_before_pointer_keeps_old() {
        let engine = eng("dep-preswitch");
        cleanup_space(&engine);

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
        assert!(!engine.slot_dir("slot-b").exists());

        cleanup_space(&engine);
    }

    #[test]
    fn recover_committed_finalizes_and_deletes_old() {
        let engine = eng("dep-committed");
        cleanup_space(&engine);

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
        assert!(!engine.slot_dir("slot-a").exists());
        assert!(DeploymentStore::read_journal(&engine).unwrap().is_none());

        cleanup_space(&engine);
    }

    #[test]
    fn recover_fresh_install_switched_removes_pointer() {
        let engine = eng("dep-fresh");
        cleanup_space(&engine);

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

        cleanup_space(&engine);
    }

    #[test]
    fn recover_corrupt_journal_fail_closed() {
        let engine = eng("dep-corrupt");
        cleanup_space(&engine);

        write_slot_manifest(&engine, "slot-a", "dep-old");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-old", "slot-a")).unwrap();
        write_slot_manifest(&engine, "slot-b", "dep-garbage");
        std::fs::write(engine.journal_path(), "not json").unwrap();

        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert!(matches!(outcome, RecoveryOutcome::FailClosed { .. }));
        // active 保留，未知 slot 被清理
        let p = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p.slot, "slot-a");
        assert!(!engine.slot_dir("slot-b").exists());

        cleanup_space(&engine);
    }

    /// implementation 空间的崩溃恢复不触碰 engine 级资产（反之亦然）。
    #[test]
    fn recover_in_impl_space_never_touches_engine_assets() {
        let engine = eng("dep-rec-iso");
        let impl_space = onnx_space("dep-rec-iso");
        cleanup_space(&engine);
        cleanup_space(&impl_space);

        // engine 级：GGUF 部署 active（模拟 0.22.7 真源）
        write_slot_manifest(&engine, "slot-a", "dep-gguf");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-gguf", "slot-a")).unwrap();

        // impl 空间：Switched 中断（candidate = slot-b，指针已切）
        write_slot_manifest(&impl_space, "slot-a", "dep-onnx-old");
        DeploymentStore::write_pointer(
            &impl_space,
            &pointer(&impl_space, "dep-onnx-old", "slot-a"),
        )
        .unwrap();
        let mut j = DeploymentStore::begin(&impl_space, "op-i1", "dep-onnx-new").unwrap();
        write_slot_manifest(&impl_space, "slot-b", "dep-onnx-new");
        DeploymentStore::advance_phase(&impl_space, &mut j, TransactionPhase::Switched).unwrap();
        DeploymentStore::write_pointer(
            &impl_space,
            &pointer(&impl_space, "dep-onnx-new", "slot-b"),
        )
        .unwrap();

        // 只恢复 impl 空间
        let outcome = DeploymentStore::recover(&impl_space).unwrap();
        assert!(matches!(
            outcome,
            RecoveryOutcome::RevertedToPrevious { previous: Some(ref id), .. } if id == "dep-onnx-old"
        ));

        // impl 空间回滚到 dep-onnx-old；engine 级 GGUF 指针与 slot 原样
        let p_impl = DeploymentStore::read_pointer(&impl_space).unwrap().unwrap();
        assert_eq!(p_impl.install_id, "dep-onnx-old");
        let p_engine = DeploymentStore::read_pointer(&engine).unwrap().unwrap();
        assert_eq!(p_engine.install_id, "dep-gguf");
        assert_eq!(p_engine.slot, "slot-a");
        assert!(engine.slot_dir("slot-a").join("marker.txt").exists());
        assert!(
            !engine.slot_dir("slot-b").exists(),
            "engine 级 slot-b 不被 impl 恢复触碰"
        );

        // 反向：engine 级恢复不动 impl 空间
        let outcome = DeploymentStore::recover(&engine).unwrap();
        assert_eq!(outcome, RecoveryOutcome::Stable);
        assert!(impl_space.slot_dir("slot-a").exists());

        cleanup_space(&engine);
        cleanup_space(&impl_space);
    }

    #[test]
    fn sweep_staging_keeps_current_operation_and_is_scoped() {
        let engine = eng("dep-sweep");
        let impl_space = onnx_space("dep-sweep");
        cleanup_space(&engine);
        cleanup_space(&impl_space);

        std::fs::create_dir_all(engine.staging_dir().join("op-keep")).unwrap();
        std::fs::create_dir_all(engine.staging_dir().join("op-old")).unwrap();
        std::fs::create_dir_all(impl_space.staging_dir().join("op-impl-old")).unwrap();
        let cleaned = DeploymentStore::sweep_staging_except(&engine, "op-keep");
        assert_eq!(cleaned, 1);
        assert!(engine.staging_dir().join("op-keep").exists());
        assert!(!engine.staging_dir().join("op-old").exists());
        // impl 空间 staging 不被 engine 空间清扫影响
        assert!(impl_space.staging_dir().join("op-impl-old").exists());
        let cleaned_impl = DeploymentStore::sweep_orphan_staging(&impl_space);
        assert_eq!(cleaned_impl, 1);

        cleanup_space(&engine);
        cleanup_space(&impl_space);
    }

    #[test]
    fn delete_slot_refuses_active_and_marks_residue_for_locked() {
        let engine = eng("dep-residue");
        cleanup_space(&engine);

        write_slot_manifest(&engine, "slot-a", "dep-a");
        write_slot_manifest(&engine, "slot-b", "dep-b");
        DeploymentStore::write_pointer(&engine, &pointer(&engine, "dep-a", "slot-a")).unwrap();

        // active slot 拒绝删除
        assert!(!DeploymentStore::delete_slot_if_not_active(&engine, "slot-a", "test").unwrap());
        assert!(engine.slot_dir("slot-a").exists());

        // 非 active 正常删除
        assert!(DeploymentStore::delete_slot_if_not_active(&engine, "slot-b", "test").unwrap());
        assert!(!engine.slot_dir("slot-b").exists());
        assert!(DeploymentStore::read_residue(&engine).unwrap().is_empty());

        cleanup_space(&engine);
    }
}
