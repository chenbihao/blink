//! 统一资源 ref 层——ResourceStore 协议（0.23.12）。
//!
//! **定位**：ResourceStore 是 Capability 之间的**数据平面**——跨调用传递大资源
//! 的短期、最小授权句柄协议；不是新的执行平面，不改变 0.21 Capability 唯一
//! 原子执行语义。
//!
//! ```text
//! 内存字节 / 本地文件 /（未来）远程对象
//!        → ResourceStore（身份、授权、生命周期、配额）
//!        → opaque ResourceRef
//!        → OCR / pin / STT /（未来）文本读取等 Capability 消费
//! ```
//!
//! **合并来源**（两套平行 Registry 的收敛）：
//! - ImageStash（0.19.4）：内存字节、15 分钟 TTL、16 项/64 MiB/单项 32 MiB、
//!   非消费读取 → Memory backing 腿
//! - AudioResourceRegistry（0.22.16 H03）：磁盘路径句柄 + FileIdentity 校验、
//!   5 分钟 TTL、32 项/256 MB/单项 128 MB、一次性 resolve → LocalFile backing 腿
//!
//! **关键决策**（0.23 §8.2 已定案）：
//! 1. backing 双模：`Memory(Bytes)` 与 `LocalFile(FrozenLocalFile)`；`Remote` 只留
//!    类型边界不实现。配额双口径分离（`resident_memory_bytes` 与
//!    `referenced_local_bytes`），禁止混入同一 max_total_bytes；TTL 按 backing
//!    分口径（Memory 15 分钟 / LocalFile 5 分钟），读取不续期。
//! 2. `ResourceUse` 封闭枚举：audio_ref 的 scope 上升为所有 ref 的通用属性，
//!    use 集合由签发方（UI/Interaction/内部 Capability）决定，AI 只能消费不能
//!    自选。每新增消费型 Capability 需扩枚举并评审签发方白名单——这是有意的
//!    收敛摩擦，不是债。
//! 3. token 来自 OS CSPRNG（getrandom，≥128 位）——ref 会被 AI/MCP 持有，
//!    本质是 bearer capability。
//! 4. resolve 顺序：校验全过才消费——查找 → TTL/revoke_group/use/identity 校验
//!    → 成功取得 lease → 同一临界区内提交使用次数。错误 use 不消耗 one-shot。
//! 5. 撤销语义收敛：`revoke_group` 取代裸 generation；`revoke_by_owner` 供
//!    会话级批量撤销（chat 附件等）。
//!
//! **隐私铁则**：Debug/Display/日志输出不含绝对路径、文件名正文或资源字节。
//! token 本身是纯 hex，不含路径片段。

mod error;
mod store;
mod types;

#[cfg(test)]
mod tests;

pub use error::{ResourceError, ResourceErrorKind};
// 协议完整表面——bin crate 对暂无生产消费方的 pub 项告警，此处显式豁免
//（LegQuota/Config/Stats 等由测试与后续消费方使用）。
#[allow(unused_imports)]
pub use store::{DefaultResourceStore, LegQuota, ResourceStoreConfig, ResourceStoreStats};
#[allow(unused_imports)]
pub use types::{
    BACKING_KIND_LOCAL_FILE, BACKING_KIND_MEMORY, BACKING_KIND_REMOTE, FrozenLocalFile,
    OpenedResource, RemoteObjectKey, ResourceBacking, ResourceGrant, ResourceGrantSpec,
    ResourceMetadata, ResourceRef, ResourceUse, ResourceUseSet, ReusePolicy,
};
