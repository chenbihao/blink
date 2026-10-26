//! 本机同步状态文件（0.28.6）。
//!
//! `%APPDATA%\blink\sync\state.json`（原子写：tmp + rename），内容：
//! - `device_id`：首次生成，仅展示用途。
//! - `baseline`：共同基线（远端 revision + 摘要 + item 快照）——三方合并的 B。
//! - `overrides`：本机覆盖记录——同步应用失败（如热键被占用/校验拒绝）时，
//!   本机保留旧值、记录共享值与原因；**不上传成本机新意图**（phase §3.5
//!   「不能反向把自动回退上传成新的全局偏好」）。用户此后显式修改该项时
//!   （本地值 ≠ 记录时的本机值）覆盖自然失效。
//! - `pending_plan`：预览后待提交计划（含冻结的本机原始值指纹），提交前复核。
//!
//! 备份：每次提交前把本机可移植分片原始 JSON 写入 `sync/backups/backup-<ts>.json`，
//! 保留最近 [`MAX_BACKUPS`] 份。

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use super::model::{HeadInfo, ItemMap};
use super::SyncError;

/// 备份保留份数。
pub const MAX_BACKUPS: usize = 10;

/// 共同基线。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Baseline {
    pub revision: u64,
    pub sha256: String,
    pub items: ItemMap,
}

/// 本机覆盖记录（item 粒度）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LocalOverride {
    pub item: String,
    /// 共享值（合并结果中该 item 的值；None = 共享侧已删除）。
    pub shared_value: Option<serde_json::Value>,
    /// 记录时的本机值（None = 本机当时缺项）。再次同步时本地值若仍等于它，
    /// 视为覆盖仍生效（L 视角替换为基线值）；不相等说明用户已显式修改 → 覆盖失效。
    pub local_value: Option<serde_json::Value>,
    pub reason: String,
    pub at: i64,
}

/// 待提交计划（冻结快照）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PendingPlan {
    /// 计划指纹（merged items + 冻结原始值 + head revision 的 sha256）。
    pub plan_id: String,
    /// 目标 head revision（远端当前 + 1）。
    pub target_revision: u64,
    /// 冻结的本机原始分片 JSON（提交前复核；变化即 StalePlan）。
    pub frozen_raw: BTreeMap<String, serde_json::Value>,
    /// 冻结的提示词库行投影。
    #[serde(default)]
    pub frozen_prompts: BTreeMap<String, serde_json::Value>,
    /// 合并结果 items（提交时重建应用操作）。
    pub merged: ItemMap,
    pub created_at: i64,
}

/// 同步状态文件。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncState {
    pub schema_version: u32,
    pub device_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub baseline: Option<Baseline>,
    #[serde(default)]
    pub overrides: Vec<LocalOverride>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pending_plan: Option<PendingPlan>,
}

pub const STATE_SCHEMA_VERSION: u32 = 1;

impl SyncState {
    /// 全新状态（生成 device_id）。
    pub fn new() -> Self {
        Self {
            schema_version: STATE_SCHEMA_VERSION,
            device_id: generate_device_id(),
            baseline: None,
            overrides: Vec::new(),
            pending_plan: None,
        }
    }

    pub fn load(path: &Path) -> Result<Self, SyncError> {
        let bytes = std::fs::read(path).map_err(|e| SyncError::Io {
            message: format!("读取同步状态失败: {e}"),
        })?;
        let state: SyncState = serde_json::from_slice(&bytes).map_err(|e| SyncError::Io {
            message: format!(
                "同步状态文件损坏（可删除 {} 后重新初始化）: {e}",
                path.display()
            ),
        })?;
        if state.schema_version > STATE_SCHEMA_VERSION {
            return Err(SyncError::SchemaTooNew {
                format_version: state.schema_version,
            });
        }
        Ok(state)
    }

    /// 原子写（tmp + rename；Windows 语义与 plugin registry 相同）。
    pub fn save(&self, path: &Path) -> Result<(), SyncError> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| SyncError::Io {
                message: format!("创建同步目录失败: {e}"),
            })?;
        }
        let bytes = serde_json::to_vec_pretty(self).map_err(|e| SyncError::Io {
            message: format!("同步状态序列化失败: {e}"),
        })?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, &bytes).map_err(|e| SyncError::Io {
            message: format!("写入同步状态失败: {e}"),
        })?;
        // Windows rename 目标存在时失败——先移除再 rename（崩溃窗口由下次
        // save/load 兜底；状态文件仅是缓存性真源，损坏可重建基线）。
        if path.exists() && std::fs::remove_file(path).is_err() {
            // 删除失败（占用）→ 尝试直接 rename 覆盖。
        }
        std::fs::rename(&tmp, path).map_err(|e| SyncError::Io {
            message: format!("同步状态落盘失败: {e}"),
        })?;
        Ok(())
    }

    /// 基线推进到新快照。
    pub fn advance_baseline(&mut self, head_info: &HeadInfo, items: ItemMap) {
        self.baseline = Some(Baseline {
            revision: head_info.head.revision,
            sha256: head_info.head.sha256.clone(),
            items,
        });
    }

    /// 记录应用失败的覆盖（item 粒度；已有同 item 记录则更新共享值）。
    pub fn record_overrides(
        &mut self,
        entries: impl IntoIterator<Item = LocalOverride>,
    ) {
        for entry in entries {
            if let Some(existing) = self.overrides.iter_mut().find(|o| o.item == entry.item) {
                *existing = entry;
            } else {
                self.overrides.push(entry);
            }
        }
    }

    /// L 视角替换：覆盖仍生效的 item 用基线值替换本机值（避免假冲突 + 不上传覆盖）。
    /// 返回替换后的 item map。无基线时不替换。
    pub fn substitute_overrides(&self, local: &ItemMap) -> ItemMap {
        let Some(baseline) = &self.baseline else {
            return local.clone();
        };
        let mut out = local.clone();
        for entry in &self.overrides {
            let current = out.get(&entry.item);
            let still = match (current, &entry.local_value) {
                (Some(c), Some(lv)) => c == lv,
                (None, None) => true,
                _ => false,
            };
            if still {
                match baseline.items.get(&entry.item) {
                    Some(v) => {
                        out.insert(entry.item.clone(), v.clone());
                    }
                    None => {
                        out.remove(&entry.item);
                    }
                }
            }
            // 本地值已变 → 用户显式修改，覆盖自然失效（不删除记录；
            // 下次记录覆盖时被更新，或用户手动重置）。
        }
        out
    }
}

fn generate_device_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::fill(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// 写一份本地备份并裁剪到 [`MAX_BACKUPS`]。
pub fn write_backup(backup_dir: &Path, raw: &BTreeMap<String, serde_json::Value>) -> Result<PathBuf, SyncError> {
    std::fs::create_dir_all(backup_dir).map_err(io_err)?;
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let path = backup_dir.join(format!("backup-{ts}.json"));
    let bytes = serde_json::to_vec_pretty(raw).map_err(|e| SyncError::Io {
        message: format!("备份序列化失败: {e}"),
    })?;
    std::fs::write(&path, bytes).map_err(io_err)?;
    prune_backups(backup_dir);
    Ok(path)
}

fn prune_backups(backup_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(backup_dir) else {
        return;
    };
    let mut backups: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with("backup-") && n.ends_with(".json"))
        })
        .collect();
    if backups.len() <= MAX_BACKUPS {
        return;
    }
    backups.sort(); // 文件名含时间戳，字典序即时间序
    let excess = backups.len() - MAX_BACKUPS;
    for path in &backups[..excess] {
        let _ = std::fs::remove_file(path);
    }
}

fn io_err(e: std::io::Error) -> SyncError {
    SyncError::Io {
        message: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn temp_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "blink-sync-state-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn item_map(pairs: Vec<(&str, serde_json::Value)>) -> ItemMap {
        pairs
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect()
    }

    #[test]
    fn state_roundtrip_and_baseline_advance() {
        let dir = temp_dir();
        let path = dir.join("state.json");
        let mut state = SyncState::new();
        assert!(!state.device_id.is_empty());
        state.save(&path).unwrap();
        let loaded = SyncState::load(&path).unwrap();
        assert_eq!(loaded.device_id, state.device_id);
        assert!(loaded.baseline.is_none());

        let head = HeadInfo {
            head: super::super::model::SyncHead {
                format_version: 1,
                revision: 3,
                snapshot: "3.json".into(),
                sha256: "abc".into(),
                device_id: "other".into(),
                updated_at: 1,
            },
            etag: None,
        };
        state.advance_baseline(&head, item_map(vec![("a", json!(1))]));
        state.save(&path).unwrap();
        let loaded = SyncState::load(&path).unwrap();
        assert_eq!(loaded.baseline.as_ref().unwrap().revision, 3);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn override_substitution_and_natural_expiry() {
        let mut state = SyncState::new();
        let head = HeadInfo {
            head: super::super::model::SyncHead {
                format_version: 1,
                revision: 1,
                snapshot: "1.json".into(),
                sha256: "abc".into(),
                device_id: "d".into(),
                updated_at: 1,
            },
            etag: None,
        };
        let baseline_items = item_map(vec![("app.hotkey#key", json!("k9"))]);
        state.advance_baseline(&head, baseline_items);
        state.record_overrides([LocalOverride {
            item: "app.hotkey#key".into(),
            shared_value: Some(json!("k9")),
            local_value: Some(json!("k1")),
            reason: "热键校验失败".into(),
            at: 1,
        }]);

        // 本机值未变 → 替换为基线值（不上传覆盖）。
        let local = item_map(vec![("app.hotkey#key", json!("k1"))]);
        let view = state.substitute_overrides(&local);
        assert_eq!(view.get("app.hotkey#key"), Some(&json!("k9")));

        // 用户显式改了热键 → 覆盖失效，L 视角反映新意图。
        let local2 = item_map(vec![("app.hotkey#key", json!("F2"))]);
        let view2 = state.substitute_overrides(&local2);
        assert_eq!(view2.get("app.hotkey#key"), Some(&json!("F2")));
    }

    #[test]
    fn backups_pruned_to_limit() {
        let dir = temp_dir();
        for _i in 0..(MAX_BACKUPS + 3) {
            let raw: BTreeMap<String, serde_json::Value> = BTreeMap::new();
            let path = write_backup(&dir, &raw).unwrap();
            // 人为控制时间戳排序：重命名到固定序号名再改回不可行，直接断言裁剪数量。
            let _ = path;
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let count = std::fs::read_dir(&dir)
            .unwrap()
            .flatten()
            .count();
        assert_eq!(count, MAX_BACKUPS);
        std::fs::remove_dir_all(&dir).ok();
    }
}
