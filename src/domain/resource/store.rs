//! DefaultResourceStore——统一资源 ref 层的默认实现（0.23.12）。
//!
//! 合并 ImageStash（Memory 腿）与 AudioResourceRegistry（LocalFile 腿），
//! 配额/TTL 按 backing 双口径分离，token 来自 OS CSPRNG，
//! open 在同一临界区内完成「校验全过才消费」。

use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use bytes::Bytes;

use crate::infra::platform::file_identity::{self};

use super::error::{ResourceError, ResourceErrorKind};
use super::types::{
    FrozenLocalFile, OpenedResource, ResourceBacking, ResourceGrant, ResourceGrantSpec,
    ResourceRef, ResourceUse, ResourceUseSet, ReusePolicy,
};

/// 内存腿默认 TTL：15 分钟（沿用 ImageStash；投影层 `expires_in_seconds: 900` 与此绑定）。
pub(crate) const MEMORY_TTL: Duration = Duration::from_secs(15 * 60);
/// 内存腿默认容量：16 项 / 64 MiB / 单项 32 MiB。
const MEMORY_MAX_ITEMS: usize = 16;
const MEMORY_MAX_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
const MEMORY_MAX_SINGLE_BYTES: u64 = 32 * 1024 * 1024;

/// 本地文件腿默认 TTL：5 分钟（沿用 AudioResourceRegistry）。
pub(crate) const LOCAL_TTL: Duration = Duration::from_secs(5 * 60);
/// 本地文件腿默认容量：32 项 / 256 MB / 单项 128 MB。
const LOCAL_MAX_ITEMS: usize = 32;
const LOCAL_MAX_TOTAL_BYTES: u64 = 256 * 1024 * 1024;
const LOCAL_MAX_SINGLE_BYTES: u64 = 128 * 1024 * 1024;

/// 单腿配额——参数化以便测试；生产值见 `ResourceStoreConfig::production`。
#[derive(Debug, Clone)]
pub struct LegQuota {
    pub ttl: Duration,
    pub max_items: usize,
    pub max_total_bytes: u64,
    pub max_single_bytes: u64,
}

/// store 配置——内存腿与本地文件腿独立配额（0.23 §8.2 决策 1：
/// `resident_memory_bytes` 与 `referenced_local_bytes` 分口径计数，
/// 禁止混入同一 max_total_bytes）。
#[derive(Debug, Clone)]
pub struct ResourceStoreConfig {
    pub memory: LegQuota,
    pub local: LegQuota,
}

impl Default for ResourceStoreConfig {
    fn default() -> Self {
        Self::production()
    }
}

impl ResourceStoreConfig {
    /// 生产默认值（内存 15min/16/64MiB/32MiB；本地 5min/32/256MB/128MB）。
    pub fn production() -> Self {
        Self {
            memory: LegQuota {
                ttl: MEMORY_TTL,
                max_items: MEMORY_MAX_ITEMS,
                max_total_bytes: MEMORY_MAX_TOTAL_BYTES,
                max_single_bytes: MEMORY_MAX_SINGLE_BYTES,
            },
            local: LegQuota {
                ttl: LOCAL_TTL,
                max_items: LOCAL_MAX_ITEMS,
                max_total_bytes: LOCAL_MAX_TOTAL_BYTES,
                max_single_bytes: LOCAL_MAX_SINGLE_BYTES,
            },
        }
    }

    fn leg(&self, leg: Leg) -> &LegQuota {
        match leg {
            Leg::Memory => &self.memory,
            Leg::LocalFile => &self.local,
        }
    }
}

/// 诊断统计（测试/诊断用，不含敏感信息）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[allow(dead_code)]
pub struct ResourceStoreStats {
    pub memory_entries: usize,
    pub memory_bytes: u64,
    pub local_entries: usize,
    pub local_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Leg {
    Memory,
    LocalFile,
}

/// store 条目。
struct StoreEntry {
    backing: ResourceBacking,
    mime: Option<String>,
    grant: ResourceGrant,
    created_at: Instant,
    /// 过期时刻——签发时固定，读取不续期（由 insert_entry 按 spec 落位）。
    expires_at: Instant,
    /// MaxReads 剩余次数；Reusable/OneShot 为 None（OneShot 在成功 open 时整条移除）。
    remaining_reads: Option<u32>,
}

impl StoreEntry {
    fn leg(&self) -> Leg {
        match &self.backing {
            ResourceBacking::Memory(_) => Leg::Memory,
            ResourceBacking::LocalFile(_) => Leg::LocalFile,
            // Remote 未实现——签发路径已拦截，此处仅兜底归类
            ResourceBacking::Remote(_) => Leg::Memory,
        }
    }
}

/// 统一资源 store——线程安全，无后台线程。
///
/// 由 `TauriDomainEnv` / CLI 持有；MCP minimal 等无 store 运行时不构造
/// （`CapabilityEnv::resource_store()` 返回 `None`，消费方降级为结构化错误）。
pub struct DefaultResourceStore {
    entries: Mutex<HashMap<String, StoreEntry>>,
    /// 撤销组计数器（`revoke_group` 句柄；取代旧 generation 机制）。
    group_counter: AtomicU64,
    config: ResourceStoreConfig,
}

impl Default for DefaultResourceStore {
    fn default() -> Self {
        Self::new(ResourceStoreConfig::production())
    }
}

impl DefaultResourceStore {
    /// 构造空 store。
    pub fn new(config: ResourceStoreConfig) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            group_counter: AtomicU64::new(1),
            config,
        }
    }

    /// 分配一个新撤销组（签发方自行持有组号，会话结束时 `revoke_group`）。
    pub fn fresh_group(&self) -> u64 {
        self.group_counter.fetch_add(1, Ordering::SeqCst)
    }

    /// 生成不可预测 token：OS CSPRNG 128 位 → `rref_` + 32 hex。
    fn generate_token() -> Result<String, ResourceError> {
        let mut buf = [0u8; 16];
        getrandom::fill(&mut buf).map_err(|e| {
            ResourceError::with_detail(
                ResourceErrorKind::IoError,
                format!("csprng unavailable: {e}"),
            )
        })?;
        Ok(format!("rref_{}", hex_encode(&buf)))
    }

    // ── 签发 ─────────────────────────────────────────────────────────────

    /// 签发内存资源（图片字节等）。
    ///
    /// `bytes` 空或超过内存腿单项上限 → `ResourceBudgetExceeded`。
    pub fn issue_memory(
        &self,
        bytes: Bytes,
        mime: impl Into<String>,
        spec: ResourceGrantSpec,
    ) -> Result<ResourceRef, ResourceError> {
        if bytes.is_empty() {
            return Err(ResourceError::with_detail(
                ResourceErrorKind::ResourceBudgetExceeded,
                "memory resource bytes are empty",
            ));
        }
        if bytes.len() as u64 > self.config.memory.max_single_bytes {
            return Err(ResourceError::with_detail(
                ResourceErrorKind::ResourceBudgetExceeded,
                format!(
                    "single memory resource {} exceeds max {}",
                    bytes.len(),
                    self.config.memory.max_single_bytes
                ),
            ));
        }
        let token = Self::generate_token()?;
        let entry = StoreEntry {
            backing: ResourceBacking::Memory(bytes),
            mime: Some(mime.into()),
            grant: ResourceGrant {
                uses: ResourceUseSet::default(),
                reuse: spec.reuse,
                group: 0,
                owner: String::new(),
            },
            created_at: Instant::now(),
            expires_at: Instant::now(),
            remaining_reads: None,
        };
        self.insert_entry(token, entry, spec)
    }

    /// 签发本地文件资源（音频文件等）。
    ///
    /// 前置条件：`abs_path` 是用户通过可信入口显式选择的绝对本地路径。
    /// 签发时记录 canonical path、size、mtime 与平台文件 identity；
    /// open 时重新打开并复核 identity（TOCTOU 防护）。
    pub fn issue_local_file(
        &self,
        abs_path: &Path,
        spec: ResourceGrantSpec,
    ) -> Result<ResourceRef, ResourceError> {
        if !abs_path.is_absolute() {
            return Err(ResourceError::with_detail(
                ResourceErrorKind::NotRegularFile,
                "path is not absolute",
            ));
        }
        if !file_identity::is_regular_file(abs_path) {
            return Err(ResourceError::new(ResourceErrorKind::NotRegularFile));
        }
        // 打开并取 identity（先打开再校验，避免校验路径后重开的 TOCTOU）
        let (_file, identity) =
            file_identity::open_file_for_identity(abs_path).map_err(ResourceError::from_io)?;
        if identity.size > self.config.local.max_single_bytes {
            return Err(ResourceError::with_detail(
                ResourceErrorKind::ResourceBudgetExceeded,
                format!(
                    "single file size {} exceeds max {}",
                    identity.size, self.config.local.max_single_bytes
                ),
            ));
        }
        let canonical_path = abs_path
            .canonicalize()
            .unwrap_or_else(|_| abs_path.to_path_buf());
        let token = Self::generate_token()?;
        let entry = StoreEntry {
            backing: ResourceBacking::LocalFile(FrozenLocalFile {
                canonical_path,
                size: identity.size,
                identity,
            }),
            mime: None,
            grant: ResourceGrant {
                uses: ResourceUseSet::default(),
                reuse: spec.reuse,
                group: 0,
                owner: String::new(),
            },
            created_at: Instant::now(),
            expires_at: Instant::now(),
            remaining_reads: None,
        };
        self.insert_entry(token, entry, spec)
    }

    /// 写入条目——物化 grant、落位 TTL（backing 默认或 ttl_override）与
    /// MaxReads 计数，带惰性清理与按腿淘汰。
    fn insert_entry(
        &self,
        token: String,
        mut entry: StoreEntry,
        spec: ResourceGrantSpec,
    ) -> Result<ResourceRef, ResourceError> {
        // 空 use 集的 ref 无人能 open——签发层直接拒绝
        if spec.uses.is_empty() {
            return Err(ResourceError::with_detail(
                ResourceErrorKind::ResourceBudgetExceeded,
                "grant must carry at least one use",
            ));
        }
        let leg = entry.leg();
        let ttl = spec
            .ttl_override
            .unwrap_or_else(|| self.config.leg(leg).ttl);
        entry.grant = ResourceGrant {
            uses: spec.uses,
            reuse: spec.reuse,
            group: spec
                .group
                .unwrap_or_else(|| self.group_counter.fetch_add(1, Ordering::SeqCst)),
            owner: spec.owner,
        };
        entry.expires_at = entry.created_at + ttl;
        entry.remaining_reads = match spec.reuse {
            ReusePolicy::MaxReads(n) => Some(n),
            _ => None,
        };

        let mut entries = self.lock_entries();
        Self::evict_expired(&mut entries);
        entries.insert(token.clone(), entry);
        Self::evict_oldest(&mut entries, &self.config);

        if entries.contains_key(&token) {
            tracing::debug!(
                token_len = token.len(),
                leg = matches!(leg, Leg::Memory)
                    .then_some("memory")
                    .unwrap_or("local"),
                "resource ref issued"
            );
            Ok(ResourceRef(token))
        } else {
            Err(ResourceError::with_detail(
                ResourceErrorKind::ResourceBudgetExceeded,
                "evicted immediately after insert",
            ))
        }
    }

    // ── 消费 ─────────────────────────────────────────────────────────────

    /// open——按声明的 use 取得 lease。
    ///
    /// **校验链（全过才消费，同一临界区内原子完成；0.23 §8.2 决策 5）**：
    /// 1. ref 存在
    /// 2. TTL 未超时
    /// 3. use 在 grant 授予集合内（错误 use **不**消耗 one-shot——修复
    ///    0.22.16「先 remove 再校验」的烧 ref 缺陷）
    /// 4. LocalFile：重新打开并复核 identity/regular file（TOCTOU 防护）
    /// 5. 提交使用次数（OneShot 移除 / MaxReads 递减 / Reusable 不变）
    pub fn open(
        &self,
        resource_ref: &ResourceRef,
        use_: ResourceUse,
    ) -> Result<OpenedResource, ResourceError> {
        let token = resource_ref.as_str();
        if token.is_empty() || !token.starts_with("rref_") {
            return Err(ResourceError::new(ResourceErrorKind::InvalidResourceRef));
        }

        let mut entries = self.lock_entries();

        // ── 校验阶段（不修改条目）──
        // TTL 先查不改——过期返回 StaleResourceRef 而非 Invalid，随后清理
        // 其他过期条目（借用已随 expires_at 拷贝结束，可安全可变借用）。
        let expires_at = match entries.get(token) {
            Some(entry) => entry.expires_at,
            None => {
                return Err(ResourceError::new(ResourceErrorKind::InvalidResourceRef));
            }
        };
        if Instant::now() > expires_at {
            Self::evict_expired(&mut entries);
            return Err(ResourceError::new(ResourceErrorKind::StaleResourceRef));
        }

        let (opened, reuse, remaining) = {
            let entry = entries
                .get(token)
                .ok_or_else(|| ResourceError::new(ResourceErrorKind::InvalidResourceRef))?;

            if !entry.grant.uses.contains(use_) {
                return Err(ResourceError::with_detail(
                    ResourceErrorKind::UseDenied,
                    format!("use '{}' is not granted for this ref", use_.as_str()),
                ));
            }

            let mime = entry.mime.clone();
            let opened = match &entry.backing {
                ResourceBacking::Memory(bytes) => {
                    OpenedResource::memory(bytes.clone(), mime.clone())
                }
                ResourceBacking::LocalFile(frozen) => {
                    // 锁内打开——与 0.22.16 相同的临界区策略
                    let (file, current_identity) =
                        file_identity::open_file_for_identity(&frozen.canonical_path)
                            .map_err(ResourceError::from_io)?;
                    if !frozen.identity.matches(&current_identity) {
                        return Err(ResourceError::with_detail(
                            ResourceErrorKind::FileIdentityChanged,
                            "file identity mismatch (size/mtime/volume/index changed)",
                        ));
                    }
                    if !file_identity::is_regular_file(&frozen.canonical_path) {
                        return Err(ResourceError::new(ResourceErrorKind::NotRegularFile));
                    }
                    OpenedResource::local_file(file, frozen.size)
                }
                ResourceBacking::Remote(_) => {
                    return Err(ResourceError::with_detail(
                        ResourceErrorKind::UnsupportedBacking,
                        "remote backing is not implemented in this phase",
                    ));
                }
            };

            (opened, entry.grant.reuse, entry.remaining_reads)
        };

        // ── 消费阶段（校验全过，同一临界区提交）──
        match reuse {
            ReusePolicy::Reusable => {}
            ReusePolicy::OneShot => {
                entries.remove(token);
            }
            ReusePolicy::MaxReads(n) => {
                let remaining = remaining.unwrap_or(n);
                if remaining <= 1 {
                    entries.remove(token);
                } else if let Some(entry) = entries.get_mut(token) {
                    entry.remaining_reads = Some(remaining - 1);
                }
            }
        }

        // 顺手清理其他过期条目（不影响本次结果）
        Self::evict_expired(&mut entries);

        tracing::debug!(
            token_len = token.len(),
            use_ = use_.as_str(),
            "resource ref opened"
        );
        Ok(opened)
    }

    // ── 检查 / 撤销 ──────────────────────────────────────────────────────

    /// 非消费检查——返回元数据（无路径、无字节）。
    pub fn inspect(&self, resource_ref: &ResourceRef) -> Option<super::types::ResourceMetadata> {
        let entries = self.lock_entries();
        let entry = entries.get(resource_ref.as_str())?;
        let now = Instant::now();
        let expires_in_seconds = if now >= entry.expires_at {
            0
        } else {
            entry.expires_at.duration_since(now).as_secs()
        };
        Some(super::types::ResourceMetadata {
            mime: entry.mime.clone(),
            size_bytes: entry.backing.size_bytes(),
            expires_in_seconds,
            backing_kind: entry.backing.kind_str(),
            uses: entry.grant.uses.iter().map(|u| u.as_str()).collect(),
        })
    }

    /// 撤销单条 ref（媒体请求结束的 RAII 清理等）。
    pub fn revoke(&self, resource_ref: &ResourceRef) -> bool {
        let mut entries = self.lock_entries();
        entries.remove(resource_ref.as_str()).is_some()
    }

    /// 按撤销组撤销——取代旧 generation 机制（组内所有 ref 立即失效）。
    /// 返回撤销条数。
    pub fn revoke_group(&self, group: u64) -> usize {
        let mut entries = self.lock_entries();
        let before = entries.len();
        entries.retain(|_, e| e.grant.group != group);
        before - entries.len()
    }

    /// 按所有者撤销——会话级批量清理（chat 附件会话结束等）。
    /// 返回撤销条数。
    pub fn revoke_by_owner(&self, owner: &str) -> usize {
        let mut entries = self.lock_entries();
        let before = entries.len();
        entries.retain(|_, e| e.grant.owner != owner);
        before - entries.len()
    }

    // ── 派生 ─────────────────────────────────────────────────────────────

    /// 从仍有效的 ref 派生新 grant——**权限衰减**语义（VAD 调试 clone 语义平移，
    /// 0.23 §8.2 决策 3：attenuation-only）。
    ///
    /// 派生只能缩小授权，不能仅凭 bearer ref 增权：
    /// - **use 子集**：`spec.uses` 必须全部来自源 grant 的 use 集；
    /// - **读取授权不扩大**：派生后的总读取授权不得超过源 ref 剩余授权——
    ///   OneShot 不得派生成 Reusable / MaxReads(n>1)，MaxReads 按源剩余次数收紧；
    /// - **TTL 不延长**：派生有效期 = min(backing 默认或 spec.ttl_override，
    ///   源 ref 剩余有效期)。
    ///
    /// 任一越界返回 `PermissionEscalation`；需要新 use（如同时保留分析与回放）
    /// 时应由可信签发入口在签发阶段授予，不得靠通用派生补权。
    ///
    /// **不消费原 ref**；新条目使用新 token/组/owner。LocalFile 在派生时
    /// 重新打开并复核 identity（防 TOCTOU）。
    pub fn issue_from_ref(
        &self,
        source: &ResourceRef,
        spec: ResourceGrantSpec,
    ) -> Result<ResourceRef, ResourceError> {
        let (derived_backing, mime, source_expires_at) = {
            let entries = self.lock_entries();
            let entry = entries
                .get(source.as_str())
                .ok_or_else(|| ResourceError::new(ResourceErrorKind::InvalidResourceRef))?;
            if Instant::now() > entry.expires_at {
                return Err(ResourceError::new(ResourceErrorKind::StaleResourceRef));
            }

            // 衰减铁则 1：新 use 必须来自源 grant——bearer ref 本身不构成增权依据
            if !entry.grant.uses.covers(spec.uses) {
                return Err(ResourceError::with_detail(
                    ResourceErrorKind::PermissionEscalation,
                    "derived uses must be a subset of the source grant",
                ));
            }

            // 衰减铁则 2：总读取授权不得扩大（以源 ref 剩余授权为上限）
            let source_reads: Option<u32> = match entry.grant.reuse {
                ReusePolicy::Reusable => None,
                ReusePolicy::OneShot => Some(1),
                ReusePolicy::MaxReads(_) => entry.remaining_reads,
            };
            let derived_reads: Option<u32> = match spec.reuse {
                ReusePolicy::Reusable => None,
                ReusePolicy::OneShot => Some(1),
                ReusePolicy::MaxReads(n) => Some(n),
            };
            match (source_reads, derived_reads) {
                (Some(_), None) => {
                    return Err(ResourceError::with_detail(
                        ResourceErrorKind::PermissionEscalation,
                        "cannot derive an unbounded reuse grant from a bounded source",
                    ));
                }
                (Some(source_left), Some(derived)) if derived > source_left => {
                    return Err(ResourceError::with_detail(
                        ResourceErrorKind::PermissionEscalation,
                        "derived read authorization exceeds the source's remaining reads",
                    ));
                }
                _ => {}
            }

            let expires_at = entry.expires_at;
            match &entry.backing {
                ResourceBacking::Memory(bytes) => {
                    (ResourceBacking::Memory(bytes.clone()), entry.mime.clone(), expires_at)
                }
                ResourceBacking::LocalFile(frozen) => {
                    // 锁内复核 identity（与 open 同策略）
                    let (_file, current_identity) =
                        file_identity::open_file_for_identity(&frozen.canonical_path)
                            .map_err(ResourceError::from_io)?;
                    if !frozen.identity.matches(&current_identity) {
                        return Err(ResourceError::with_detail(
                            ResourceErrorKind::FileIdentityChanged,
                            "file identity mismatch during derive",
                        ));
                    }
                    (
                        ResourceBacking::LocalFile(frozen.clone()),
                        entry.mime.clone(),
                        expires_at,
                    )
                }
                ResourceBacking::Remote(_) => {
                    return Err(ResourceError::with_detail(
                        ResourceErrorKind::UnsupportedBacking,
                        "remote backing is not implemented in this phase",
                    ));
                }
            }
        };

        // 衰减铁则 3：派生不得延长源 ref 的有效期——请求 TTL（显式 override 或
        // backing 默认）超过源剩余有效期时钳制到源剩余
        let now = Instant::now();
        let backing_ttl = match derived_backing {
            ResourceBacking::Memory(_) => self.config.memory.ttl,
            // Remote 不可达——上方锁内已拦截
            ResourceBacking::Remote(_) | ResourceBacking::LocalFile(_) => self.config.local.ttl,
        };
        let requested_ttl = spec.ttl_override.unwrap_or(backing_ttl);
        let mut effective_spec = spec;
        if let Some(source_left) = now.checked_duration_since(source_expires_at) {
            // 源 ref 已在临界（剩余 0）——按 0 处理，派生立即过期（不可消费）
            effective_spec.ttl_override = Some(source_left);
        } else {
            let source_left = source_expires_at.duration_since(now);
            if requested_ttl > source_left {
                effective_spec.ttl_override = Some(source_left);
            }
        }

        let token = Self::generate_token()?;
        let entry = StoreEntry {
            backing: derived_backing,
            mime,
            grant: ResourceGrant {
                uses: ResourceUseSet::default(),
                reuse: effective_spec.reuse,
                group: 0,
                owner: String::new(),
            },
            created_at: now,
            expires_at: now,
            remaining_reads: None,
        };
        self.insert_entry(token, entry, effective_spec)
    }

    // ── 诊断 ─────────────────────────────────────────────────────────────

    /// 当前诊断统计（不含敏感信息）。
    #[allow(dead_code)]
    pub fn stats(&self) -> ResourceStoreStats {
        let entries = self.lock_entries();
        let mut stats = ResourceStoreStats::default();
        for entry in entries.values() {
            match entry.leg() {
                Leg::Memory => {
                    stats.memory_entries += 1;
                    stats.memory_bytes += entry.backing.size_bytes();
                }
                Leg::LocalFile => {
                    stats.local_entries += 1;
                    stats.local_bytes += entry.backing.size_bytes();
                }
            }
        }
        stats
    }

    // ── 内部 ─────────────────────────────────────────────────────────────

    fn lock_entries(&self) -> std::sync::MutexGuard<'_, HashMap<String, StoreEntry>> {
        self.entries.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// 内联清理已过期条目。调用方已持有锁。
    fn evict_expired(entries: &mut HashMap<String, StoreEntry>) {
        let now = Instant::now();
        entries.retain(|_, e| e.expires_at > now);
    }

    /// 按最早创建顺序淘汰，直到各腿分别满足项数与总量约束（双口径互不挤占）。
    fn evict_oldest(entries: &mut HashMap<String, StoreEntry>, config: &ResourceStoreConfig) {
        fn leg_len(entries: &HashMap<String, StoreEntry>, leg: Leg) -> usize {
            entries.values().filter(|e| e.leg() == leg).count()
        }
        fn leg_bytes(entries: &HashMap<String, StoreEntry>, leg: Leg) -> u64 {
            entries
                .values()
                .filter(|e| e.leg() == leg)
                .map(|e| e.backing.size_bytes())
                .sum()
        }
        for leg in [Leg::Memory, Leg::LocalFile] {
            let quota = config.leg(leg);
            while leg_len(entries, leg) > quota.max_items {
                if !remove_oldest_of_leg(entries, leg) {
                    break;
                }
            }
            while leg_bytes(entries, leg) > quota.max_total_bytes {
                if !remove_oldest_of_leg(entries, leg) {
                    break;
                }
            }
        }
    }
}

/// 淘汰指定腿最早创建的条目；返回是否淘汰了条目。
fn remove_oldest_of_leg(entries: &mut HashMap<String, StoreEntry>, leg: Leg) -> bool {
    let oldest_key = entries
        .iter()
        .filter(|(_, e)| e.leg() == leg)
        .min_by_key(|(_, e)| e.created_at)
        .map(|(k, _)| k.clone());
    match oldest_key {
        Some(key) => entries.remove(&key).is_some(),
        None => false,
    }
}

/// hex 编码（16 字节仅此一处，不引入 hex crate）。
fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(HEX[(byte >> 4) as usize] as char);
        out.push(HEX[(byte & 0x0F) as usize] as char);
    }
    out
}
