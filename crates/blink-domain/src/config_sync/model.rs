//! 同步快照与 head 协议模型（0.28.6，phase §3.5「WebDAV 连接与目录」）。
//!
//! 远端布局：
//!
//! ```text
//! blink/
//!   head.json              # 指向有效快照 + 格式/摘要
//!   snapshots/<rev>.json   # 不可变版本
//! ```
//!
//! 快照是 item map（稳定 key → JSON 值，`portable` 定义合法 key 空间），
//! `BTreeMap` 保证序列化确定性——摘要稳定，同名内容不同设备产出相同字节。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::SyncError;

/// 快照格式版本。只增不减；读到更高版本必须拒绝写入（`SchemaTooNew`），
/// 读到更低版本按无损迁移处理（当前无更低版本）。
pub const SNAPSHOT_FORMAT_VERSION: u32 = 1;

/// 快照 item map：稳定 key（`portable` 命名空间）→ JSON 值。
pub type ItemMap = BTreeMap<String, serde_json::Value>;

/// 远端 head——指向有效快照并记录格式与摘要。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SyncHead {
    pub format_version: u32,
    /// 单调递增版本号（首次为 1）。
    pub revision: u64,
    /// 快照文件名（`snapshots/` 下的相对名，如 `3.json`）。
    pub snapshot: String,
    /// 快照内容 sha256（十六进制小写）。
    pub sha256: String,
    /// 最后写入设备（仅展示）。
    pub device_id: String,
    /// 最后写入时间（Unix 秒，仅展示）。
    pub updated_at: i64,
}

/// head + 读到的强 ETag（条件更新用；None = 服务器未提供，CAS 降级）。
#[derive(Debug, Clone, PartialEq)]
pub struct HeadInfo {
    pub head: SyncHead,
    pub etag: Option<String>,
}

/// 可移植配置快照。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PortableSnapshot {
    pub format_version: u32,
    /// 生成设备（仅展示）。
    pub device_id: String,
    /// 生成时间（Unix 秒，仅展示，不参与合并决策）。
    pub created_at: i64,
    pub items: ItemMap,
}

impl PortableSnapshot {
    pub fn new(device_id: &str, items: ItemMap) -> Self {
        Self {
            format_version: SNAPSHOT_FORMAT_VERSION,
            device_id: device_id.to_string(),
            created_at: chrono_like_now(),
            items,
        }
    }

    /// 序列化为快照文件字节（pretty 关闭——确定性 + 体积）。
    pub fn to_bytes(&self) -> Result<Vec<u8>, SyncError> {
        serde_json::to_vec(self).map_err(|e| SyncError::Unknown {
            message: format!("快照序列化失败: {e}"),
        })
    }

    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SnapshotFormatError> {
        serde_json::from_slice(bytes).map_err(|e| SnapshotFormatError::Parse(e.to_string()))
    }
}

/// 快照解析错误（区分解析失败与格式版本超前）。
#[derive(Debug, Clone, PartialEq)]
pub enum SnapshotFormatError {
    Parse(String),
    /// format_version 高于本机 `SNAPSHOT_FORMAT_VERSION`——不得写入/覆盖。
    TooNew { format_version: u32 },
}

/// 计算快照字节的 sha256 摘要（十六进制小写）。
pub fn snapshot_digest(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    let out = hasher.finalize();
    let mut s = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(s, "{b:02x}");
    }
    s
}

/// revision 相同的并发候选也不能相互覆盖。
pub fn snapshot_name(revision: u64, digest: &str) -> String {
    format!("{revision}-{digest}.json")
}

fn chrono_like_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_roundtrip_and_digest_stable() {
        let mut items = ItemMap::new();
        items.insert("app.appearance#theme".into(), serde_json::json!("dark"));
        items.insert(
            "app.ai#providers/order".into(),
            serde_json::json!(["p1", "p2"]),
        );
        let snap = PortableSnapshot::new("device-a", items);
        let bytes = snap.to_bytes().unwrap();
        let parsed = PortableSnapshot::from_bytes(&bytes).unwrap();
        assert_eq!(parsed.format_version, SNAPSHOT_FORMAT_VERSION);
        assert_eq!(parsed.device_id, "device-a");
        assert_eq!(parsed.items, snap.items);
        // 确定性：同字节摘要一致；摘要是 64 位十六进制。
        let digest = snapshot_digest(&bytes);
        assert_eq!(digest.len(), 64);
        assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
        assert_eq!(digest, snapshot_digest(&bytes));
        assert_ne!(digest, snapshot_digest(b"other bytes"));
    }

    #[test]
    fn parse_rejects_garbage() {
        assert!(matches!(
            PortableSnapshot::from_bytes(b"not json"),
            Err(SnapshotFormatError::Parse(_))
        ));
    }

    #[test]
    fn same_revision_candidates_have_distinct_names() {
        let a = snapshot_digest(b"device a");
        let b = snapshot_digest(b"device b");
        assert_ne!(snapshot_name(2, &a), snapshot_name(2, &b));
    }
}
