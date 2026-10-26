//! 同步状态机（0.28.6，phase §3.5「提交与失败恢复」）。
//!
//! 流程：预读/校验双方 → 三方合并/解决冲突 → 本地预检/备份并冻结计划 →
//! 上传不可变候选 → 按 ETag 条件更新 head → 复核并应用本地计划 → 推进基线。
//!
//! - 候选快照不可变（`snapshots/<rev>.json`），head 条件更新（首次
//!   `If-None-Match: *`，后续 `If-Match`）；412 → 报可重试冲突由用户重跑。
//! - 网络超时结果未知 → 先重读 head 判断候选是否已提交，不盲目重传。
//! - 本地应用失败不打断其余分片——失败项转本机覆盖记录（不上传），
//!   远端已提交的快照不回滚。
//! - 首次连接无共同基线：返回两端状态由用户显式选择初始化方向。
//!
//! 依赖注入：`SyncStore`（WebDAV 等远端存储）与 `SyncLocalEnv`（bin 侧
//! 配置应用服务），域内 mock 可完整回归状态机行为。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use super::diff::{merge_items, Resolution};
use super::model::{
    snapshot_digest, snapshot_name, HeadInfo, ItemMap, PortableSnapshot, SnapshotFormatError,
    SyncHead, SNAPSHOT_FORMAT_VERSION,
};
use super::portable::{build_apply_ops, items_from_raw, normalize_item, ApplyOp, AppliedItemOutcome, RawLocal};
use super::state::{write_backup, LocalOverride, PendingPlan, SyncState};
use super::SyncError;

/// 远端存储 port（WebDAV 客户端实现；测试用内存 mock）。
#[async_trait::async_trait]
pub trait SyncStore: Send + Sync {
    /// 确保布局：根目录可达、`blink/` 与 `blink/snapshots/` 存在（自动创建；
    /// 同名文件冲突报 `RemoteConflict`）。
    async fn ensure_layout(&self) -> Result<(), SyncError>;
    /// 读 head；None = 不存在（首次）。etag 为强 ETag（无则 None，CAS 降级）。
    async fn read_head(&self) -> Result<Option<HeadInfo>, SyncError>;
    /// 读指定快照字节。
    async fn read_snapshot(&self, name: &str) -> Result<Vec<u8>, SyncError>;
    /// 上传不可变候选快照。
    async fn put_snapshot(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError>;
    /// 条件更新 head。`etag = None` 表示首次创建（If-None-Match: *）。
    /// 前置条件失败（412）返回 message 为 [`PRECONDITION_FAILED`] 的
    /// `SyncError::RemoteConflict`。
    async fn put_head_conditional(&self, etag: Option<&str>, head: &SyncHead)
        -> Result<(), SyncError>;
}

/// CAS 失败标记（RemoteConflict 的专用 message）。
pub const PRECONDITION_FAILED: &str = "__precondition_failed__";

pub fn is_precondition_failed(e: &SyncError) -> bool {
    matches!(e, SyncError::RemoteConflict { message } if message == PRECONDITION_FAILED)
}

/// 本机环境 port（bin 侧实现：既有 set_config 链 + CM 探测 + 状态目录）。
#[async_trait::async_trait]
pub trait SyncLocalEnv: Send + Sync {
    /// 状态文件目录（`%APPDATA%\blink\sync`）。
    fn state_dir(&self) -> PathBuf;
    /// 读取本机原始投影（portable::read_raw_local 的池版本）。
    async fn read_raw(&self) -> Result<RawLocal, SyncError>;
    /// 执行应用操作（逐条执行，失败不打断，返回逐条结果）。
    async fn apply_ops(&self, ops: Vec<ApplyOp>) -> Vec<AppliedItemOutcome>;
    /// 探测 AI provider 是否已配置本机密钥（只读 CM）。
    fn provider_has_secret(&self, provider_id: &str) -> bool;
    /// 插件是否已安装（本机安装状态，不漫游）。
    fn plugin_installed(&self, plugin_id: &str) -> bool;
}

/// 同步结果条目（前端逐条展示）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncSummaryEntry {
    /// 已生效（remote = 远端变更已在本机应用；local = 本机变更已上传）
    Applied { item: String, from: AppliedFrom },
    /// 本机覆盖保留（应用失败；本机值保留，共享值未生效，不上传）
    Overridden { item: String, reason: String },
    /// 待安装（远端有该插件配置，本机未安装；保留待应用）
    PendingPlugin { plugin_id: String },
    /// 待填密钥（合并后的 provider 在本机无密钥；已有密钥不会被清空）
    PendingSecret { provider_id: String },
    /// 不认识的远端扩展 item（整分片命名空间未知；保留转发，不在本机应用）
    PreservedUnknown { item: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AppliedFrom {
    Remote,
    Local,
}

/// 预览计划（含待裁决冲突；无冲突时可直接提交）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncPlan {
    pub plan_id: String,
    pub target_revision: u64,
    pub conflicts: Vec<super::diff::MergeConflict>,
    pub preview: Vec<SyncSummaryEntry>,
}

/// 同步完成报告。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncReport {
    pub revision: u64,
    pub summary: Vec<SyncSummaryEntry>,
    /// 远端是否缺可靠条件写（降级提示：并发保护弱化为最后写入胜）。
    pub degraded_cas: bool,
    /// 本地是否实际发生了变更（false = 双方一致空跑）。
    pub changed: bool,
    /// 提交前备份文件名。
    pub backup: Option<String>,
}

/// run_sync 结果。
#[derive(Debug, Clone, PartialEq)]
pub enum SyncRunOutcome {
    /// 双方一致，无需变更。
    UpToDate { revision: u64 },
    /// 无共同基线——返回两端概要，UI 显式选择初始化方向。
    NeedsInit {
        remote_head: Option<HeadInfo>,
        local_items: usize,
    },
    /// 有冲突待裁决（已保存 pending plan）。
    AwaitingResolution(SyncPlan),
    /// 完成。
    Done(SyncReport),
}

/// 单次同步的上下文。
pub struct SyncRunContext<'a> {
    pub store: &'a dyn SyncStore,
    pub env: &'a dyn SyncLocalEnv,
    /// 用户裁决（对当前冲突）。提供时走提交路径。
    pub resolutions: Vec<Resolution>,
    /// 提交时校验的 plan_id（与预览一致才提交）。
    pub plan_id: Option<String>,
}

pub struct SyncEngine;

impl SyncEngine {
    /// 手动同步主入口。
    pub async fn run(ctx: SyncRunContext<'_>) -> Result<SyncRunOutcome, SyncError> {
        let state_path = ctx.env.state_dir().join("state.json");
        let mut state = load_or_new(&state_path)?;
        ctx.store.ensure_layout().await?;
        let Some(head) = ctx.store.read_head().await? else {
            // 远端尚无任何内容——首次初始化（本机上传或稍后在其他设备创建）。
            return Ok(SyncRunOutcome::NeedsInit {
                remote_head: None,
                local_items: ctx.env.read_raw().await?.count_items(),
            });
        };
        let (remote_items, head_info) = read_remote_snapshot(ctx.store, &head).await?;

        if state.baseline.is_none() {
            // 远端已有内容但本机无基线——显式选择初始化方向。
            return Ok(SyncRunOutcome::NeedsInit {
                remote_head: Some(head_info),
                local_items: ctx.env.read_raw().await?.count_items(),
            });
        }

        if !ctx.resolutions.is_empty() || ctx.plan_id.is_some() {
            Self::commit(ctx, &mut state, &state_path, head_info, remote_items).await
        } else {
            Self::preview_or_commit(ctx, &mut state, &state_path, head_info, remote_items).await
        }
    }

    /// 无裁决入口：合并 → 无冲突直接提交；有冲突存计划待裁决。
    async fn preview_or_commit(
        ctx: SyncRunContext<'_>,
        state: &mut SyncState,
        state_path: &Path,
        head_info: HeadInfo,
        remote_items: ItemMap,
    ) -> Result<SyncRunOutcome, SyncError> {
        let raw = ctx.env.read_raw().await?;
        let local_actual = items_from_raw(&raw)?;
        let local_view = effective_local_view(&local_actual, state, ctx.env);
        let base = state.baseline_items();

        let mut merged = merge_items(&base, &local_view, &remote_items);
        merged.apply_resolutions(&[]);
        merged.validate_references();

        if !merged.conflicts.is_empty() {
            let plan_id = plan_fingerprint(&merged.merged, &raw, head_info.head.revision);
            tracing::info!(
                conflicts = merged.conflicts.len(),
                remote_revision = head_info.head.revision,
                "配置同步存在冲突，等待用户裁决"
            );
            let preview = summarize(&merged.merged, &local_view, &remote_items, ctx.env);
            state.pending_plan = Some(PendingPlan {
                plan_id: plan_id.clone(),
                target_revision: head_info.head.revision + 1,
                frozen_raw: raw.shards.clone(),
                frozen_prompts: raw.prompts.clone(),
                merged: merged.merged.clone(),
                created_at: now(),
            });
            state.save(state_path)?;
            return Ok(SyncRunOutcome::AwaitingResolution(SyncPlan {
                plan_id,
                target_revision: head_info.head.revision + 1,
                conflicts: merged.conflicts,
                preview,
            }));
        }
        Self::do_commit(ctx, state, state_path, head_info, raw, merged.merged, local_actual, local_view, remote_items)
            .await
    }

    /// 裁决提交路径：校验 pending plan（存在 + plan_id 匹配 + 冻结值未变）后提交。
    async fn commit(
        ctx: SyncRunContext<'_>,
        state: &mut SyncState,
        state_path: &Path,
        head_info: HeadInfo,
        remote_items: ItemMap,
    ) -> Result<SyncRunOutcome, SyncError> {
        let pending = state.pending_plan.clone().ok_or(SyncError::StalePlan)?;
        if let Some(expect) = &ctx.plan_id
            && expect != &pending.plan_id
        {
            return Err(SyncError::StalePlan);
        }
        // 冻结复核：预览后本地又被修改 → 重做计划。
        let raw = ctx.env.read_raw().await?;
        if raw.shards != pending.frozen_raw || raw.prompts != pending.frozen_prompts {
            state.pending_plan = None;
            state.save(state_path)?;
            return Err(SyncError::StalePlan);
        }
        let local_actual = items_from_raw(&raw)?;
        let local_view = effective_local_view(&local_actual, state, ctx.env);
        let base = state.baseline_items();
        let mut merged = merge_items(&base, &local_view, &remote_items);
        merged.apply_resolutions(&ctx.resolutions);
        merged.validate_references();
        if !merged.conflicts.is_empty() {
            let plan_id = plan_fingerprint(&merged.merged, &raw, head_info.head.revision);
            state.pending_plan = Some(PendingPlan {
                plan_id: plan_id.clone(),
                target_revision: head_info.head.revision + 1,
                frozen_raw: raw.shards.clone(),
                frozen_prompts: raw.prompts.clone(),
                merged: merged.merged.clone(),
                created_at: now(),
            });
            state.save(state_path)?;
            return Ok(SyncRunOutcome::AwaitingResolution(SyncPlan {
                plan_id,
                target_revision: head_info.head.revision + 1,
                conflicts: merged.conflicts,
                preview: summarize(&merged.merged, &local_view, &remote_items, ctx.env),
            }));
        }
        Self::do_commit(ctx, state, state_path, head_info, raw, merged.merged, local_actual, local_view, remote_items)
            .await
    }

    /// 实际提交：备份 → 上传候选 → 条件 head → 本地应用 → 推进基线。
    #[allow(clippy::too_many_arguments)]
    async fn do_commit(
        ctx: SyncRunContext<'_>,
        state: &mut SyncState,
        state_path: &Path,
        head_info: HeadInfo,
        raw: RawLocal,
        merged: ItemMap,
        local_actual: ItemMap,
        local_view: ItemMap,
        remote_items: ItemMap,
    ) -> Result<SyncRunOutcome, SyncError> {
        let ops = build_apply_ops(&merged, &raw, &local_view)?;
        // 待安装插件分片：本机未装则不应用（保留在快照链中）。
        let (applicable, deferred): (Vec<ApplyOp>, Vec<ApplyOp>) = ops
            .into_iter()
            .partition(|op| match op {
                ApplyOp::PluginConfig { plugin_id, .. } => ctx.env.plugin_installed(plugin_id),
                _ => true,
            });
        // 空跑判定：无本地可应用变更、无可上传差异、也无待装插件分片——
        // 覆盖生效后的重复同步走这里。
        let upload_changed = !items_equal_normalized(&merged, &remote_items);
        if applicable.is_empty() && deferred.is_empty() && !upload_changed {
            return Ok(SyncRunOutcome::UpToDate {
                revision: head_info.head.revision,
            });
        }

        // 本地备份（提交前原始可移植分片）。
        let backup_dir = ctx.env.state_dir().join("backups");
        let backup = write_backup(&backup_dir, &raw.shards).ok();

        // 远端提交：仅在合并结果与远端不同时上传候选 + 条件更新 head；
        // 纯追赶（远端已是目标内容、本机只需应用）不重写远端、不推进版本。
        let new_head = if upload_changed {
            let snapshot = PortableSnapshot::new(&state.device_id, merged.clone());
            let bytes = snapshot.to_bytes()?;
            let digest = snapshot_digest(&bytes);
            let target_rev = head_info.head.revision + 1;
            let candidate = SyncHead {
                format_version: SNAPSHOT_FORMAT_VERSION,
                revision: target_rev,
                snapshot: snapshot_name(target_rev),
                sha256: digest.clone(),
                device_id: state.device_id.clone(),
                updated_at: now(),
            };
            ctx.store.put_snapshot(&candidate.snapshot, &bytes).await?;
            let mut etag_used = head_info.etag.clone();
            let mut committed = false;
            for _ in 0..3 {
                match ctx
                    .store
                    .put_head_conditional(etag_used.as_deref(), &candidate)
                    .await
                {
                    Ok(()) => {
                        committed = true;
                        break;
                    }
                    Err(e) if is_precondition_failed(&e) => {
                        // 并发更新——不重放旧确认；用户重跑 run 即重新三方合并。
                        state.pending_plan = None;
                        state.save(state_path)?;
                        return Err(SyncError::RemoteConflict {
                            message: "远端已被其他设备更新，请重新同步".into(),
                        });
                    }
                    Err(e) if e.retryable() => {
                        // 网络结果未知 → 重读 head 判断候选是否已提交，不盲目重传。
                        let current = ctx.store.read_head().await?;
                        if let Some(info) = &current
                            && info.head.revision == target_rev
                            && info.head.sha256 == digest
                        {
                            committed = true;
                            break;
                        }
                        etag_used = current.and_then(|i| i.etag);
                        continue;
                    }
                    Err(e) => return Err(e),
                }
            }
            if !committed {
                return Err(SyncError::Network {
                    message: "远端 head 更新多次失败，请稍后重试（候选快照已保留）".into(),
                });
            }
            candidate
        } else {
            head_info.head.clone()
        };

        // 本地应用（失败项转覆盖，不回滚远端）。覆盖记录的 local_value 用
        // 未替换的实际本机值——与 substitute_overrides 的失效判定同源。
        let has_applicable = !applicable.is_empty();
        let outcomes = ctx.env.apply_ops(applicable).await;
        let mut summary = summarize(&merged, &local_view, &remote_items, ctx.env);
        let mut overrides = Vec::new();
        for outcome in &outcomes {
            let Some(err) = &outcome.error else { continue };
            for item in &outcome.items {
                summary.retain(|s| {
                    !matches!(s, SyncSummaryEntry::Applied { item: i, .. } if i == item)
                });
                summary.push(SyncSummaryEntry::Overridden {
                    item: item.clone(),
                    reason: err.clone(),
                });
                overrides.push(LocalOverride {
                    item: item.clone(),
                    shared_value: merged.get(item).cloned(),
                    local_value: local_actual.get(item).cloned(),
                    reason: err.clone(),
                    at: now(),
                });
            }
        }
        state.record_overrides(overrides);
        for op in &deferred {
            if let ApplyOp::PluginConfig { plugin_id, .. } = op {
                summary.push(SyncSummaryEntry::PendingPlugin {
                    plugin_id: plugin_id.clone(),
                });
            }
        }

        // 推进基线 + 清计划。
        let final_revision = new_head.revision;
        state.advance_baseline(
            &HeadInfo {
                head: new_head,
                etag: None,
            },
            merged,
        );
        state.pending_plan = None;
        state.save(state_path)?;
        tracing::info!(
            revision = final_revision,
            uploaded = upload_changed,
            applied = has_applicable,
            "配置同步完成"
        );

        Ok(SyncRunOutcome::Done(SyncReport {
            revision: final_revision,
            summary,
            degraded_cas: head_info.etag.is_none(),
            changed: upload_changed || has_applicable,
            backup: backup
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string),
        }))
    }

    /// 首次初始化——显式方向（用户确认后调用）。
    pub async fn initialize(
        store: &dyn SyncStore,
        env: &dyn SyncLocalEnv,
        direction: InitDirection,
    ) -> Result<SyncReport, SyncError> {
        store.ensure_layout().await?;
        let state_path = env.state_dir().join("state.json");
        let mut state = load_or_new(&state_path)?;
        let head = store.read_head().await?;

        let (merged, base_rev) = match direction {
            InitDirection::Upload => {
                let raw = env.read_raw().await?;
                let items = items_from_raw(&raw)?;
                let rev = head.as_ref().map(|h| h.head.revision).unwrap_or(0) + 1;
                (items, rev)
            }
            InitDirection::Download => {
                let head = head.ok_or(SyncError::NeedsInit)?;
                let (items, info) = read_remote_snapshot(store, &head).await?;
                let raw = env.read_raw().await?;
                let local_actual = items_from_raw(&raw)?;
                let ops = build_apply_ops(&items, &raw, &local_actual)?;
                let (applicable, deferred): (Vec<ApplyOp>, Vec<ApplyOp>) = ops
                    .into_iter()
                    .partition(|op| match op {
                        ApplyOp::PluginConfig { plugin_id, .. } => env.plugin_installed(plugin_id),
                        _ => true,
                    });
                let outcomes = env.apply_ops(applicable).await;
                let mut summary = summarize(&items, &local_actual, &items, env);
                let mut overrides = Vec::new();
                for outcome in &outcomes {
                    let Some(err) = &outcome.error else { continue };
                    for item in &outcome.items {
                        summary.retain(|s| {
                            !matches!(s, SyncSummaryEntry::Applied { item: i, .. } if i == item)
                        });
                        summary.push(SyncSummaryEntry::Overridden {
                            item: item.clone(),
                            reason: err.clone(),
                        });
                        overrides.push(LocalOverride {
                            item: item.clone(),
                            shared_value: items.get(item).cloned(),
                            local_value: local_actual.get(item).cloned(),
                            reason: err.clone(),
                            at: now(),
                        });
                    }
                }
                state.record_overrides(overrides);
                for op in &deferred {
                    if let ApplyOp::PluginConfig { plugin_id, .. } = op {
                        summary.push(SyncSummaryEntry::PendingPlugin {
                            plugin_id: plugin_id.clone(),
                        });
                    }
                }
                state.advance_baseline(&info, items);
                state.pending_plan = None;
                state.save(&state_path)?;
                return Ok(SyncReport {
                    revision: info.head.revision,
                    summary,
                    degraded_cas: false,
                    changed: true,
                    backup: None,
                });
            }
        };

        // Upload 方向：本机配置作为初始共享状态（远端已有 head 时为用户确认的覆盖）。
        let backup_dir = env.state_dir().join("backups");
        let raw = env.read_raw().await?;
        let backup = write_backup(&backup_dir, &raw.shards).ok();
        let snapshot = PortableSnapshot::new(&state.device_id, merged.clone());
        let bytes = snapshot.to_bytes()?;
        let digest = snapshot_digest(&bytes);
        let new_head = SyncHead {
            format_version: SNAPSHOT_FORMAT_VERSION,
            revision: base_rev,
            snapshot: snapshot_name(base_rev),
            sha256: digest,
            device_id: state.device_id.clone(),
            updated_at: now(),
        };
        store.put_snapshot(&new_head.snapshot, &bytes).await?;
        store
            .put_head_conditional(head.as_ref().and_then(|h| h.etag.as_deref()), &new_head)
            .await
            .map_err(|e| {
                if is_precondition_failed(&e) {
                    SyncError::RemoteConflict {
                        message: "远端已被其他设备更新，请改用「立即同步」合并".into(),
                    }
                } else {
                    e
                }
            })?;

        let summary = summarize(&merged, &merged, &merged, env);
        state.advance_baseline(
            &HeadInfo {
                head: new_head,
                etag: None,
            },
            merged,
        );
        state.pending_plan = None;
        state.save(&state_path)?;
        Ok(SyncReport {
            revision: base_rev,
            summary,
            degraded_cas: head.as_ref().and_then(|h| h.etag.as_ref()).is_none(),
            changed: true,
            backup: backup
                .as_ref()
                .and_then(|p| p.file_name())
                .and_then(|n| n.to_str())
                .map(str::to_string),
        })
    }

    /// 清空本机覆盖记录（用户修复阻塞项后重试应用共享值）。
    pub async fn reset_overrides(env: &dyn SyncLocalEnv) -> Result<usize, SyncError> {
        let state_path = env.state_dir().join("state.json");
        let mut state = load_or_new(&state_path)?;
        let count = state.overrides.len();
        state.overrides.clear();
        state.save(&state_path)?;
        Ok(count)
    }
}

/// 初始化方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InitDirection {
    /// 以本机配置初始化远端（远端已有内容时为覆盖，需用户确认）。
    Upload,
    /// 以远端快照初始化本机（覆盖本机共享范围；秘密/本机字段保留）。
    Download,
}

fn load_or_new(path: &Path) -> Result<SyncState, SyncError> {
    if path.exists() {
        SyncState::load(path)
    } else {
        Ok(SyncState::new())
    }
}

async fn read_remote_snapshot(
    store: &dyn SyncStore,
    head: &HeadInfo,
) -> Result<(ItemMap, HeadInfo), SyncError> {
    let bytes = store.read_snapshot(&head.head.snapshot).await?;
    if snapshot_digest(&bytes) != head.head.sha256 {
        return Err(SyncError::CorruptRemote {
            message: "远端快照摘要不符（可能传输损坏），请重试".into(),
        });
    }
    let snapshot: PortableSnapshot =
        PortableSnapshot::from_bytes(&bytes).map_err(|e| match e {
            SnapshotFormatError::Parse(m) => SyncError::CorruptRemote { message: m },
            SnapshotFormatError::TooNew { format_version } => {
                SyncError::SchemaTooNew { format_version }
            }
        })?;
    if snapshot.format_version > SNAPSHOT_FORMAT_VERSION {
        return Err(SyncError::SchemaTooNew {
            format_version: snapshot.format_version,
        });
    }
    Ok((snapshot.items, head.clone()))
}

fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 计划指纹：merged + 冻结原始值 + 目标版本。
fn plan_fingerprint(merged: &ItemMap, raw: &RawLocal, head_rev: u64) -> String {
    let payload = json!({
        "merged": merged,
        "frozen": raw.shards,
        "prompts": raw.prompts,
        "rev": head_rev,
    });
    let bytes = serde_json::to_vec(&payload).unwrap_or_default();
    snapshot_digest(&bytes)
}

/// L 视角 = 实际本机 items + 两类替换：
/// 1. 本机覆盖记录（应用失败项）→ 基线值；
/// 2. 未安装插件的休眠分片 → 基线值（休眠配置不代表本机意图，不参与上传，
///    避免远端对休眠插件的更新被本机旧值翻转）。
fn effective_local_view(
    local_actual: &ItemMap,
    state: &SyncState,
    env: &dyn SyncLocalEnv,
) -> ItemMap {
    let mut view = state.substitute_overrides(local_actual);
    let Some(baseline) = &state.baseline else {
        return view;
    };
    for key in view.keys().cloned().collect::<Vec<_>>() {
        if let Some(pid) = key
            .strip_prefix("plugin:")
            .and_then(|rest| rest.split('#').next())
            && !env.plugin_installed(pid)
        {
            match baseline.items.get(&key) {
                Some(v) => {
                    view.insert(key, v.clone());
                }
                None => {
                    view.remove(&key);
                }
            }
        }
    }
    view
}

fn items_equal_normalized(a: &ItemMap, b: &ItemMap) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.iter().all(|(k, v)| {
        b.get(k)
            .is_some_and(|bv| normalize_item(k, v) == normalize_item(k, bv))
    })
}

/// 差异摘要：远端采纳（merged ≠ 本机视图）与本机上传（merged ≠ 远端）双向列出，
/// 附待装插件、待填密钥与整分片级未知扩展。
fn summarize(
    merged: &ItemMap,
    local_view: &ItemMap,
    remote: &ItemMap,
    env: &dyn SyncLocalEnv,
) -> Vec<SyncSummaryEntry> {
    let mut summary = Vec::new();
    let mut keys: Vec<&String> = merged.keys().collect();
    keys.extend(local_view.keys());
    keys.extend(remote.keys());
    keys.sort();
    keys.dedup();
    for key in keys {
        let Some(mv) = merged.get(key) else { continue };
        let m_norm = normalize_item(key, mv);
        let local_differs = local_view
            .get(key)
            .map(|lv| normalize_item(key, lv) != m_norm)
            .unwrap_or(true);
        if local_differs {
            summary.push(SyncSummaryEntry::Applied {
                item: key.clone(),
                from: AppliedFrom::Remote,
            });
        } else if remote.get(key).map(|rv| normalize_item(key, rv) != m_norm) == Some(true) {
            summary.push(SyncSummaryEntry::Applied {
                item: key.clone(),
                from: AppliedFrom::Local,
            });
        }
    }
    for key in merged.keys() {
        if let Some(pid) = key
            .strip_prefix("plugin:")
            .and_then(|rest| rest.split('#').next())
            && !env.plugin_installed(pid)
            && !summary.iter().any(|s| matches!(s,
                SyncSummaryEntry::PendingPlugin { plugin_id } if plugin_id == pid))
        {
            summary.push(SyncSummaryEntry::PendingPlugin {
                plugin_id: pid.to_string(),
            });
        }
    }
    for (key, value) in merged {
        let Some(pid) = key.strip_prefix("app.ai#providers/") else {
            continue;
        };
        if key.ends_with("$order") {
            continue;
        }
        // ollama_http 本地服务不需要密钥；其余 kind 需要。
        let needs = value
            .get("kind")
            .and_then(|k| k.as_str())
            .is_some_and(|k| k != "ollama_http");
        if needs
            && !env.provider_has_secret(pid)
            && !summary.iter().any(|s| matches!(s,
                SyncSummaryEntry::PendingSecret { provider_id } if provider_id == pid))
        {
            summary.push(SyncSummaryEntry::PendingSecret {
                provider_id: pid.to_string(),
            });
        }
    }
    // 整分片级未知命名空间（未来新分片）：保留转发，不在本机应用。
    for key in merged.keys() {
        if is_unknown_shard_item(key) {
            summary.push(SyncSummaryEntry::PreservedUnknown { item: key.clone() });
        }
    }
    summary
}

/// 未知 = 分片前缀不在固定清单、不是 plugin:*、不是提示词库。
fn is_unknown_shard_item(key: &str) -> bool {
    let Some((shard, _)) = key.split_once('#') else {
        return true;
    };
    let known_shards = [
        "app.appearance",
        "app.hotkey",
        "app.search",
        "app.suggestion",
        "app.chord",
        "app.disable",
        "app.weblinks",
        "app.bookmarks",
        "app.ai",
    ];
    !known_shards.contains(&shard)
        && !shard.starts_with("plugin:")
        && shard != "prompt_library"
}

impl RawLocal {
    fn count_items(&self) -> usize {
        items_from_raw(self).map(|m| m.len()).unwrap_or(0)
    }
}

impl SyncState {
    fn baseline_items(&self) -> ItemMap {
        self.baseline
            .as_ref()
            .map(|b| b.items.clone())
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::super::diff::ResolutionChoice;
    use super::*;
    use std::collections::BTreeMap as Map;
    use std::sync::Mutex;

    // ── 内存 mock store ─────────────────────────────────────────────────────

    #[derive(Default)]
    struct MockStore {
        head: Mutex<Option<(SyncHead, Option<String>)>>,
        snapshots: Mutex<Map<String, Vec<u8>>>,
        etag_counter: Mutex<u64>,
        fail_head_once: Mutex<bool>,
    }

    impl MockStore {
        fn with_head(items: ItemMap, revision: u64) -> Self {
            let snap = PortableSnapshot::new("remote-device", items);
            let bytes = snap.to_bytes().unwrap();
            let digest = snapshot_digest(&bytes);
            let name = snapshot_name(revision);
            let head = SyncHead {
                format_version: SNAPSHOT_FORMAT_VERSION,
                revision,
                snapshot: name.clone(),
                sha256: digest,
                device_id: "remote-device".into(),
                updated_at: 1,
            };
            let mut snapshots = Map::new();
            snapshots.insert(name, bytes);
            Self {
                head: Mutex::new(Some((head, Some("etag-1".into())))),
                snapshots: Mutex::new(snapshots),
                etag_counter: Mutex::new(1),
                fail_head_once: Mutex::new(false),
            }
        }

        /// 模拟另一台设备推进远端到 rev+1。
        async fn push_remote(&self, items: ItemMap) {
            let rev = self.read_head().await.unwrap().unwrap().head.revision + 1;
            let snap = PortableSnapshot::new("remote", items);
            let bytes = snap.to_bytes().unwrap();
            let digest = snapshot_digest(&bytes);
            let name = snapshot_name(rev);
            self.snapshots.lock().unwrap().insert(name.clone(), bytes);
            let etag = self.head.lock().unwrap().as_ref().and_then(|(_, e)| e.clone());
            self.put_head_conditional(
                etag.as_deref(),
                &SyncHead {
                    format_version: 1,
                    revision: rev,
                    snapshot: name,
                    sha256: digest,
                    device_id: "remote".into(),
                    updated_at: rev as i64,
                },
            )
            .await
            .unwrap();
        }
    }

    #[async_trait::async_trait]
    impl SyncStore for MockStore {
        async fn ensure_layout(&self) -> Result<(), SyncError> {
            Ok(())
        }
        async fn read_head(&self) -> Result<Option<HeadInfo>, SyncError> {
            Ok(self
                .head
                .lock()
                .unwrap()
                .clone()
                .map(|(head, etag)| HeadInfo { head, etag }))
        }
        async fn read_snapshot(&self, name: &str) -> Result<Vec<u8>, SyncError> {
            self.snapshots
                .lock()
                .unwrap()
                .get(name)
                .cloned()
                .ok_or(SyncError::CorruptRemote {
                    message: format!("快照 {name} 不存在"),
                })
        }
        async fn put_snapshot(&self, name: &str, bytes: &[u8]) -> Result<(), SyncError> {
            self.snapshots
                .lock().unwrap()
                .insert(name.to_string(), bytes.to_vec());
            Ok(())
        }
        async fn put_head_conditional(
            &self,
            etag: Option<&str>,
            head: &SyncHead,
        ) -> Result<(), SyncError> {
            if *self.fail_head_once.lock().unwrap() {
                *self.fail_head_once.lock().unwrap() = false;
                return Err(SyncError::RemoteConflict {
                    message: PRECONDITION_FAILED.into(),
                });
            }
            let current = self.head.lock().unwrap().clone();
            match (&current, etag) {
                (None, None) => {}
                (Some((_, cur)), Some(got)) if cur.as_deref() == Some(got) => {}
                _ => {
                    return Err(SyncError::RemoteConflict {
                        message: PRECONDITION_FAILED.into(),
                    })
                }
            }
            *self.etag_counter.lock().unwrap() += 1;
            let etag = format!("etag-{}", *self.etag_counter.lock().unwrap());
            *self.head.lock().unwrap() = Some((head.clone(), Some(etag)));
            Ok(())
        }
    }

    // ── 内存 mock env ───────────────────────────────────────────────────────

    struct MockEnv {
        dir: PathBuf,
        raw: Mutex<RawLocal>,
        apply_results: Mutex<Vec<Option<String>>>,
        installed: Mutex<Vec<String>>,
        secrets: Vec<String>,
    }

    impl MockEnv {
        fn new(dir: &Path, raw: RawLocal) -> Self {
            Self {
                dir: dir.to_path_buf(),
                raw: Mutex::new(raw),
                apply_results: Mutex::new(vec![]),
                installed: Mutex::new(vec![]),
                secrets: vec![],
            }
        }
        fn raw(shards: Vec<(&str, serde_json::Value)>) -> RawLocal {
            RawLocal {
                shards: shards
                    .into_iter()
                    .map(|(k, v)| (k.to_string(), v))
                    .collect(),
                prompts: Map::new(),
            }
        }
        fn set_theme(&self, theme: &str) {
            self.raw.lock().unwrap().shards.insert(
                "app.appearance".into(),
                json!({"theme": theme, "language": "zh", "window_opacity": 1.0,
                       "auto_start": false, "log_level": "error"}),
            );
        }
    }

    #[async_trait::async_trait]
    impl SyncLocalEnv for MockEnv {
        fn state_dir(&self) -> PathBuf {
            self.dir.clone()
        }
        async fn read_raw(&self) -> Result<RawLocal, SyncError> {
            Ok(self.raw.lock().unwrap().clone())
        }
        async fn apply_ops(&self, ops: Vec<ApplyOp>) -> Vec<AppliedItemOutcome> {
            let results = self.apply_results.lock().unwrap().clone();
            ops.into_iter()
                .enumerate()
                .map(|(i, op)| AppliedItemOutcome {
                    shard: op_shard(&op),
                    items: op_items(&op),
                    error: results.get(i).cloned().flatten(),
                })
                .collect()
        }
        fn provider_has_secret(&self, provider_id: &str) -> bool {
            self.secrets.iter().any(|s| s == provider_id)
        }
        fn plugin_installed(&self, plugin_id: &str) -> bool {
            self.installed.lock().unwrap().iter().any(|p| p == plugin_id)
        }
    }

    fn op_shard(op: &ApplyOp) -> String {
        match op {
            ApplyOp::SetConfig { shard, .. }
            | ApplyOp::PluginConfig { shard, .. }
            | ApplyOp::PromptUpsert { shard, .. }
            | ApplyOp::PromptDelete { shard, .. } => shard.clone(),
        }
    }

    fn op_items(op: &ApplyOp) -> Vec<String> {
        match op {
            ApplyOp::SetConfig { items, .. } | ApplyOp::PluginConfig { items, .. } => {
                items.clone()
            }
            ApplyOp::PromptUpsert { item, .. } | ApplyOp::PromptDelete { item, .. } => {
                vec![item.clone()]
            }
        }
    }

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blink-sync-engine-{tag}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn appearance_shard(theme: &str) -> (&'static str, serde_json::Value) {
        (
            "app.appearance",
            json!({"theme": theme, "language": "zh", "window_opacity": 1.0,
                   "auto_start": false, "log_level": "error"}),
        )
    }

    /// 完整 appearance 三字段（与本机提取对齐——远端快照同样完整）。
    fn full_appearance(theme: &str) -> Vec<(&'static str, serde_json::Value)> {
        vec![
            ("app.appearance#theme", json!(theme)),
            ("app.appearance#language", json!("zh")),
            ("app.appearance#window_opacity", json!(1.0)),
        ]
    }

    fn items(pairs: Vec<(&str, serde_json::Value)>) -> ItemMap {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    fn run<'a>(
        store: &'a MockStore,
        env: &'a MockEnv,
        resolutions: Vec<super::super::diff::Resolution>,
        plan_id: Option<String>,
    ) -> SyncRunContext<'a> {
        SyncRunContext {
            store,
            env,
            resolutions,
            plan_id,
        }
    }

    // ── 场景回归 ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn first_sync_without_head_reports_needs_init() {
        let dir = temp_dir("init");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("dark")]));
        let store = MockStore::default();
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        match out {
            SyncRunOutcome::NeedsInit { remote_head, local_items } => {
                assert!(remote_head.is_none());
                assert!(local_items > 0);
            }
            other => panic!("expect NeedsInit, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remote_head_without_local_baseline_requires_explicit_choice() {
        let dir = temp_dir("remote-only");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("dark")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("light"))]), 1);
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        assert!(matches!(out, SyncRunOutcome::NeedsInit { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn upload_local_change_commits() {
        let dir = temp_dir("upload");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        env.set_theme("dark");
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        match out {
            SyncRunOutcome::Done(report) => {
                assert_eq!(report.revision, 2);
                assert!(report
                    .summary
                    .iter()
                    .any(|s| matches!(s, SyncSummaryEntry::Applied { item, from: AppliedFrom::Local }
                        if item == "app.appearance#theme")));
            }
            other => panic!("expect Done, got {other:?}"),
        }
        // 再同步一次：双方一致 → UpToDate，不推进版本。
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        assert!(matches!(out, SyncRunOutcome::UpToDate { revision: 2 }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn remote_change_applies_locally() {
        let dir = temp_dir("download");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        let mut next = items(full_appearance("dark"));
        next.insert("app.suggestion#autosuggest_enabled".into(), json!(true));
        store.push_remote(next).await;

        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        match out {
            SyncRunOutcome::Done(report) => {
                assert_eq!(report.revision, 2);
                assert!(report.summary.iter().any(|s| matches!(s,
                    SyncSummaryEntry::Applied { item, from: AppliedFrom::Remote }
                        if item == "app.appearance#theme")));
            }
            other => panic!("expect Done, got {other:?}"),
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn conflict_requires_resolution_then_commits_choice() {
        let dir = temp_dir("conflict");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        env.set_theme("dark");
        store
            .push_remote(items(vec![("app.appearance#theme", json!("light"))]))
            .await;

        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        let SyncRunOutcome::AwaitingResolution(plan) = out else {
            panic!("expect AwaitingResolution")
        };
        assert_eq!(plan.conflicts.len(), 1);
        assert_eq!(plan.conflicts[0].key, "app.appearance#theme");

        // 选本机 → 提交，共享值 = dark（远端被本机意图覆盖）。
        let out = SyncEngine::run(run(
            &store,
            &env,
            vec![super::super::diff::Resolution {
                key: "app.appearance#theme".into(),
                choice: ResolutionChoice::Local,
            }],
            Some(plan.plan_id.clone()),
        ))
        .await
        .unwrap();
        match out {
            SyncRunOutcome::Done(report) => assert_eq!(report.revision, 3),
            other => panic!("expect Done, got {other:?}"),
        }
        let head = store.read_head().await.unwrap().unwrap();
        let bytes = store.read_snapshot(&head.head.snapshot).await.unwrap();
        let snap = PortableSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(
            snap.items.get("app.appearance#theme"),
            Some(&json!("dark"))
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn stale_plan_when_local_changes_after_preview() {
        let dir = temp_dir("stale");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        env.set_theme("dark");
        store
            .push_remote(items(vec![("app.appearance#theme", json!("light"))]))
            .await;
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        let SyncRunOutcome::AwaitingResolution(plan) = out else {
            panic!("expect AwaitingResolution")
        };
        // 预览后本地又改。
        env.set_theme("solarized");
        let err = SyncEngine::run(run(
            &store,
            &env,
            vec![super::super::diff::Resolution {
                key: "app.appearance#theme".into(),
                choice: ResolutionChoice::Local,
            }],
            Some(plan.plan_id.clone()),
        ))
        .await
        .unwrap_err();
        assert_eq!(err, SyncError::StalePlan);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn apply_failure_becomes_override_and_not_uploaded() {
        let dir = temp_dir("override");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        store.push_remote(items(full_appearance("dark"))).await;
        // 本机应用失败（模拟热键/校验拒绝）。
        *env.apply_results.lock().unwrap() = vec![Some("热键校验失败：与 Chord 冲突".into())];

        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        let SyncRunOutcome::Done(report) = out else {
            panic!("expect Done")
        };
        assert!(report.summary.iter().any(|s| matches!(s,
            SyncSummaryEntry::Overridden { item, .. } if item == "app.appearance#theme")));
        let state = SyncState::load(&dir.join("state.json")).unwrap();
        assert_eq!(state.overrides.len(), 1);
        assert_eq!(state.overrides[0].shared_value, Some(json!("dark")));
        assert_eq!(state.overrides[0].local_value, Some(json!("auto")));

        // 远端快照已含共享值；覆盖生效后再次同步 = 空跑（本地值保留不上传）。
        let head = store.read_head().await.unwrap().unwrap();
        let bytes = store.read_snapshot(&head.head.snapshot).await.unwrap();
        let snap = PortableSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(snap.items.get("app.appearance#theme"), Some(&json!("dark")));

        *env.apply_results.lock().unwrap() = vec![];
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        assert!(matches!(out, SyncRunOutcome::UpToDate { revision: 2 }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn schema_too_new_blocks_write() {
        let dir = temp_dir("schema");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::default();
        let snap = json!({
            "format_version": 99, "device_id": "r", "created_at": 1, "items": {}
        });
        let bytes = serde_json::to_vec(&snap).unwrap();
        let digest = snapshot_digest(&bytes);
        store.snapshots.lock().unwrap().insert("1.json".into(), bytes);
        *store.head.lock().unwrap() = Some((
            SyncHead {
                format_version: 99,
                revision: 1,
                snapshot: "1.json".into(),
                sha256: digest,
                device_id: "r".into(),
                updated_at: 1,
            },
            Some("etag-1".into()),
        ));
        // 手动造本机基线（绕过 NeedsInit）。
        let mut state = SyncState::new();
        state.advance_baseline(
            &HeadInfo {
                head: SyncHead {
                    format_version: 1,
                    revision: 1,
                    snapshot: "0.json".into(),
                    sha256: "0".into(),
                    device_id: "x".into(),
                    updated_at: 0,
                },
                etag: None,
            },
            ItemMap::new(),
        );
        state.save(&dir.join("state.json")).unwrap();

        let err = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::SchemaTooNew { format_version: 99 }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn uninstalled_plugin_config_deferred_and_baseline_advances_without_churn() {
        let dir = temp_dir("plugin-defer");
        let mut raw = MockEnv::raw(vec![appearance_shard("auto")]);
        raw.shards.insert(
            "plugin:builtin.translate".into(),
            json!({"revision": 3, "enabled": true, "settings": {}, "custom_triggers": [], "disabled_default_triggers": []}),
        );
        let env = MockEnv::new(&dir, raw);
        env.installed.lock().unwrap().push("builtin.translate".into());
        let store = MockStore::with_head(ItemMap::new(), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Upload)
            .await
            .unwrap();
        // 另一台设备改插件 enabled=false。
        let mut rev2 = items(full_appearance("auto"));
        rev2.insert("plugin:builtin.translate#enabled".into(), json!(false));
        store.push_remote(rev2).await;
        // 本机卸载该插件（installed 清空，shard 仍在）。
        env.installed.lock().unwrap().clear();
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        let SyncRunOutcome::Done(report) = out else { panic!("expect Done") };
        assert!(report.summary.iter().any(|s| matches!(s,
            SyncSummaryEntry::PendingPlugin { plugin_id } if plugin_id == "builtin.translate")));
        // 再同步：插件分片与远端一致（基线吸收），仅差未装插件 → 空跑不推进版本。
        let out = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap();
        assert!(matches!(out, SyncRunOutcome::UpToDate { revision: 3 }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn head_precondition_failure_surfaces_conflict() {
        let dir = temp_dir("cas");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("auto"))]), 1);
        SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        env.set_theme("dark");
        // 上传时另一设备并发推进（head CAS 失败一次）。
        *store.fail_head_once.lock().unwrap() = true;
        let err = SyncEngine::run(run(&store, &env, vec![], None))
            .await
            .unwrap_err();
        assert!(matches!(err, SyncError::RemoteConflict { .. }));
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn init_download_overwrites_shared_scope_with_overrides_recorded() {
        let dir = temp_dir("init-dl");
        let env = MockEnv::new(&dir, MockEnv::raw(vec![appearance_shard("auto")]));
        let store = MockStore::with_head(items(vec![("app.appearance#theme", json!("light"))]), 4);
        // 应用第一条失败（theme），其余成功。
        *env.apply_results.lock().unwrap() = vec![Some("窗口未就绪".into())];
        let report = SyncEngine::initialize(&store, &env, InitDirection::Download)
            .await
            .unwrap();
        assert_eq!(report.revision, 4);
        let state = SyncState::load(&dir.join("state.json")).unwrap();
        assert_eq!(state.baseline.as_ref().unwrap().revision, 4);
        assert_eq!(state.overrides.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }
}
