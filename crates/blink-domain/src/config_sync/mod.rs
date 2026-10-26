//! 固定范围跨设备配置同步（0.28.6，phase §3.5）。
//!
//! **分层**：
//! - `model`：快照/head 协议 DTO、格式版本、摘要与错误分类（纯数据）。
//! - `portable`：固定可移植字段清单（单一真源）——本地配置 KV/提示词库 ↔ item map
//!   的双向投影，以及合并结果 → 既有 `set_config` 键路由的应用操作合成。
//! - `diff`：共同基线 B + 本机 L + 远端 R 的三方合并、冲突分类与用户裁决。
//! - `state`：本机同步状态文件（device_id / 基线 / 本机覆盖 / 待提交计划 / 备份）。
//! - `engine`：同步状态机（读 head → 三方合并 → 冻结计划 → 上传候选 → 条件更新
//!   head → 本地应用 → 推进基线），依赖通过 `SyncStore`（WebDAV）与
//!   `SyncLocalEnv`（bin 侧应用服务）两个 port 注入，域内可完整 mock。
//!
//! **铁则**（phase §3.5 定案）：
//! - 固定范围不是整个 DB——按本模块 `portable` 清单逐项枚举，不遍历原始 KV 上传。
//! - 秘密（API key/secret/CM 引用/WebDAV 密码）、设备字段、AI/MCP 授权与确认记忆
//!   不漫游；已有本机密钥不被远端缺字段清空。
//! - 时间戳仅展示，不决定覆盖顺序；删除记录走共同基线比对，不靠缺字段猜删除。
//! - 首版仅手动同步；有未解决冲突不提交覆盖。

pub mod diff;
pub mod engine;
pub mod model;
pub mod portable;
pub mod state;
pub mod webdav;

pub use diff::{ConflictKind, MergeConflict, Resolution, ResolutionChoice};
pub use engine::{
    InitDirection, SyncEngine, SyncLocalEnv, SyncPlan, SyncReport, SyncRunOutcome, SyncStore,
    SyncSummaryEntry,
};
pub use model::{
    snapshot_digest, HeadInfo, ItemMap, PortableSnapshot, SnapshotFormatError, SyncHead,
    SNAPSHOT_FORMAT_VERSION,
};
pub use portable::{extract_local_items, read_raw_local, AppliedItemOutcome, ApplyOp, RawLocal};
pub use webdav::{normalize_root_url, WebDavClient, WebDavConfig};
pub use state::{LocalOverride, PendingPlan, SyncState};

/// 同步错误分类（serde 序列化给前端按类展示；不含任何秘密）。
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SyncError {
    /// 未配置（缺地址/用户名/密码）
    NotConfigured,
    /// 网络不可达 / 超时
    Network { message: String },
    /// 认证失败（401/403）
    Auth { message: String },
    /// 无写权限
    Permission { message: String },
    /// 配额不足（507）
    Quota { message: String },
    /// 远端目录被同名文件占用 / 已有不识别格式
    RemoteConflict { message: String },
    /// 远端数据损坏（head/snapshot 解析失败、摘要不符）
    CorruptRemote { message: String },
    /// 远端 schema 版本高于本机支持——暂停本机写入，提示升级
    SchemaTooNew { format_version: u32 },
    /// 本机配置损坏——不允许当作空配置上传/覆盖
    CorruptLocal { shard: String },
    /// 预览后本地又被修改，计划过期需重新预览
    StalePlan,
    /// 没有共同基线且未显式选择初始化方向
    NeedsInit,
    /// 本地 IO 错误（状态文件/备份）
    Io { message: String },
    /// 其他
    Unknown { message: String },
}

impl SyncError {
    /// 前端 CommandError code 映射。
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "sync_not_configured",
            Self::Network { .. } => "sync_network",
            Self::Auth { .. } => "sync_auth",
            Self::Permission { .. } => "sync_permission",
            Self::Quota { .. } => "sync_quota",
            Self::RemoteConflict { .. } => "sync_remote_conflict",
            Self::CorruptRemote { .. } => "sync_corrupt_remote",
            Self::SchemaTooNew { .. } => "sync_schema_too_new",
            Self::CorruptLocal { .. } => "sync_corrupt_local",
            Self::StalePlan => "sync_stale_plan",
            Self::NeedsInit => "sync_needs_init",
            Self::Io { .. } => "sync_io",
            Self::Unknown { .. } => "sync_unknown",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::NotConfigured => "请先填写 WebDAV 地址、用户名与密码".to_string(),
            Self::Network { message } => format!("网络错误：{message}"),
            Self::Auth { message } => format!("认证失败：{message}"),
            Self::Permission { message } => format!("无写权限：{message}"),
            Self::Quota { message } => format!("远端空间不足：{message}"),
            Self::RemoteConflict { message } => format!("远端目录冲突：{message}"),
            Self::CorruptRemote { message } => format!("远端数据损坏：{message}"),
            Self::SchemaTooNew { format_version } => {
                format!("远端配置格式版本 v{format_version} 高于本机支持范围，请先升级应用再同步")
            }
            Self::CorruptLocal { shard } => {
                format!("本机配置分片 {shard} 已损坏，请先修复后再同步（不会当作空配置上传）")
            }
            Self::StalePlan => "预览后本机配置已变化，请重新预览".to_string(),
            Self::NeedsInit => "首次同步需要选择初始化方向".to_string(),
            Self::Io { message } => format!("本地读写失败：{message}"),
            Self::Unknown { message } => message.clone(),
        }
    }

    /// 是否值得重试（网络类瞬态）。
    pub fn retryable(&self) -> bool {
        matches!(self, Self::Network { .. })
    }
}

impl std::fmt::Display for SyncError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message())
    }
}

impl std::error::Error for SyncError {}

impl From<serde_json::Error> for SyncError {
    fn from(e: serde_json::Error) -> Self {
        Self::CorruptRemote {
            message: e.to_string(),
        }
    }
}

impl From<std::io::Error> for SyncError {
    fn from(e: std::io::Error) -> Self {
        Self::Io {
            message: e.to_string(),
        }
    }
}
