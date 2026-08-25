//! ImageStash — 进程级图片引用暂存（0.19.4 §3.6）。
//!
//! **目的**：修复 `CapabilityResult::Blob` 在 agent 投影层只产生尺寸摘要、
//! 原始字节无法传给下一个 tool call 的断点。图片生产者（screenshot /
//! read_clipboard）继续返回完整 Blob；投影层把 `image/*` Blob 字节移入
//! stash 并返回结构化 `image_ref`，后续 tool 只传 ref。
//!
//! **约束**（§3.8）：
//! - 固定 15 分钟 TTL，读取不续期
//! - 最多 16 项、总计 64 MiB、单项 32 MiB
//! - 超限时按最早创建顺序淘汰
//! - 不启后台线程；put/get 时内联清理过期项
//! - 引用使用不可猜测的进程内 token，不持久化、不写入日志，重启失效
//!
//! **读取语义**（§3.8）：`get` 为非消费读取，同一图片可以先 OCR 再 pin。

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use bytes::Bytes;

/// 固定 TTL：15 分钟（§3.8）。
const TTL: Duration = Duration::from_secs(15 * 60);

/// 最多 16 项（§3.8）。
const MAX_ITEMS: usize = 16;

/// 总计 64 MiB（§3.8）。
const MAX_TOTAL_BYTES: usize = 64 * 1024 * 1024;

/// 单项 32 MiB（§3.8）。
const MAX_SINGLE_BYTES: usize = 32 * 1024 * 1024;

/// 暂存的图片条目。
///
/// Task 10: `bytes` 使用 `bytes::Bytes`（Arc-backed），
/// `clone()` 零字节复制——只增加 Arc 引用计数。
#[derive(Debug, Clone)]
pub struct StashedImage {
    /// 图片字节（PNG 等）。
    /// `Bytes` 是不可变、Arc-backed 的——`clone()` 不复制底层 buffer。
    pub bytes: Bytes,
    /// MIME 类型（如 `image/png`）。
    pub mime: String,
    /// 创建时刻——用于淘汰排序。
    pub created_at: Instant,
    /// 过期时刻——创建时固定，不续期。
    pub expires_at: Instant,
}

impl StashedImage {
    /// 剩余秒数（向下取整，最小 0）。
    #[cfg(test)]
    pub fn expires_in_seconds(&self) -> u64 {
        let now = Instant::now();
        if now >= self.expires_at {
            0
        } else {
            self.expires_at.duration_since(now).as_secs()
        }
    }
}

/// 进程级图片暂存——线程安全，无后台线程。
///
/// 由 `TauriDomainEnv` 持有，经 `CapabilityEnv::image_stash()` 只读访问。
/// CLI / MCP 最小运行时不构造，返回 `None`，投影层降级为摘要。
pub struct ImageStash {
    entries: RwLock<HashMap<String, StashedImage>>,
    /// token 生成计数器——混入时间戳后 xorshift，不可猜测。
    counter: AtomicU64,
}

impl Default for ImageStash {
    fn default() -> Self {
        Self::new()
    }
}

impl ImageStash {
    /// 构造空 stash。
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
            counter: AtomicU64::new(0),
        }
    }

    /// 生成不可猜测的进程内 token。
    ///
    /// 混合原子计数器 + 系统纳秒时间戳，经 xorshift 打散后 hex 编码。
    /// 不依赖外部 rand crate，足够防猜测（进程内短期 bearer）。
    fn generate_token(&self) -> String {
        let seq = self.counter.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0);
        // xorshift64 打散——简单但足够消除序列相关性
        let mut x = seq.wrapping_add(nanos).wrapping_mul(0x9E3779B97F4A7C15);
        x ^= x >> 30;
        x = x.wrapping_mul(0xBF58476D1CE4E5B9);
        x ^= x >> 27;
        x = x.wrapping_mul(0x94D049BB133111EB);
        x ^= x >> 31;
        format!("{x:016x}")
    }

    /// 内联清理已过期条目。调用方已持有写锁。
    fn evict_expired(entries: &mut HashMap<String, StashedImage>) {
        let now = Instant::now();
        entries.retain(|_, img| img.expires_at > now);
    }

    /// 计算当前总字节数。调用方已持有读锁或写锁。
    fn total_bytes(entries: &HashMap<String, StashedImage>) -> usize {
        entries.values().map(|img| img.bytes.len()).sum()
    }

    /// 删除指定 image_ref。
    /// Task 10: 请求结束后显式删除临时 stash ref，避免占用上限。
    pub fn remove(&self, image_ref: &str) -> bool {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        entries.remove(image_ref).is_some()
    }

    /// 按最早创建顺序淘汰，直到满足 `max_items` 和 `max_total` 约束。
    ///
    /// 调用方已持有写锁，且已清理过期项。
    fn evict_oldest(
        entries: &mut HashMap<String, StashedImage>,
        max_items: usize,
        max_total: usize,
    ) {
        // 先按项数淘汰
        while entries.len() > max_items {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, img)| img.created_at)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                entries.remove(&key);
            } else {
                break;
            }
        }
        // 再按总字节淘汰
        while Self::total_bytes(entries) > max_total {
            let oldest_key = entries
                .iter()
                .min_by_key(|(_, img)| img.created_at)
                .map(|(k, _)| k.clone());
            if let Some(key) = oldest_key {
                entries.remove(&key);
            } else {
                break;
            }
        }
    }

    /// 暂存图片字节，返回不可猜测的 `image_ref`。
    ///
    /// Task 10: 接收 `Bytes`（Arc-backed），`put` 不额外复制字节。
    /// `get` 只 clone `Bytes` handle（引用计数 +1），不复制底层 buffer。
    ///
    /// **返回 `None`** 当且仅当单项超过 `MAX_SINGLE_BYTES`（32 MiB）。
    /// 超过项数或总量上限时，按最早创建顺序淘汰已有条目后存入。
    ///
    /// 空字节也返回 `None`（无意义）。
    pub fn put(&self, bytes: Bytes, mime: String) -> Option<String> {
        if bytes.is_empty() || bytes.len() > MAX_SINGLE_BYTES {
            return None;
        }

        let now = Instant::now();
        let token = self.generate_token();
        let stashed = StashedImage {
            bytes,
            mime,
            created_at: now,
            expires_at: now + TTL,
        };

        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        Self::evict_expired(&mut entries);
        entries.insert(token.clone(), stashed);
        Self::evict_oldest(&mut entries, MAX_ITEMS, MAX_TOTAL_BYTES);

        // evict_oldest 可能淘汰了刚插入的（极端情况：总量上限 < 单项），
        // 检查 token 是否仍在
        if entries.contains_key(&token) {
            Some(token)
        } else {
            None
        }
    }

    /// 非消费读取——同一 ref 可多次读（先 OCR 再 pin）。
    ///
    /// Task 10: 返回的 `StashedImage.bytes` 是 `Bytes` 的 clone——
    /// 只增加 Arc 引用计数，不复制底层 buffer（零拷贝）。
    ///
    /// 返回 `None` 当 ref 不存在或已过期。过期项在此次调用中被清理。
    /// 返回的 `StashedImage` 是 clone，调用方持有期间 stash 可能被淘汰，
    /// 但已拿到的数据不受影响。
    pub fn get(&self, image_ref: &str) -> Option<StashedImage> {
        let mut entries = self.entries.write().unwrap_or_else(|e| e.into_inner());
        Self::evict_expired(&mut entries);
        entries.get(image_ref).cloned()
    }

    /// 当前条目数（测试 / 诊断用）。
    #[cfg(test)]
    pub fn len(&self) -> usize {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        entries.len()
    }

    /// 当前总字节数（测试 / 诊断用）。
    #[cfg(test)]
    pub fn total_size(&self) -> usize {
        let entries = self.entries.read().unwrap_or_else(|e| e.into_inner());
        Self::total_bytes(&entries)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── 基本功能 ──────────────────────────────────────────────────────────

    #[test]
    fn put_and_get_roundtrip() {
        let stash = ImageStash::new();
        let data = bytes::Bytes::from(vec![0x89, 0x50, 0x4E, 0x47]);
        let token = stash.put(data.clone(), "image/png".into()).unwrap();

        let img = stash.get(&token).expect("刚放入应能取到");
        assert_eq!(img.bytes, data);
        assert_eq!(img.mime, "image/png");
    }

    #[test]
    fn get_non_consuming_can_read_twice() {
        let stash = ImageStash::new();
        let token = stash
            .put(bytes::Bytes::from(vec![1, 2, 3]), "image/png".into())
            .unwrap();

        let first = stash.get(&token).expect("第一次读取");
        let second = stash.get(&token).expect("第二次读取");
        assert_eq!(first.bytes, second.bytes);
    }

    #[test]
    fn get_unknown_ref_returns_none() {
        let stash = ImageStash::new();
        assert!(stash.get("nonexistent").is_none());
    }

    #[test]
    fn put_empty_bytes_returns_none() {
        let stash = ImageStash::new();
        assert!(stash.put(bytes::Bytes::new(), "image/png".into()).is_none());
    }

    #[test]
    fn token_is_not_sequential() {
        let stash = ImageStash::new();
        let t1 = stash
            .put(bytes::Bytes::from(vec![1]), "image/png".into())
            .unwrap();
        let t2 = stash
            .put(bytes::Bytes::from(vec![2]), "image/png".into())
            .unwrap();
        // token 不可猜测——不应是简单递增的数字串
        assert_ne!(t1, t2);
        assert_eq!(t1.len(), 16); // hex 编码 64 位 = 16 字符
        assert_eq!(t2.len(), 16);
    }

    // ── 单项大小上限 ──────────────────────────────────────────────────────

    #[test]
    fn put_exceeding_single_limit_returns_none() {
        let stash = ImageStash::new();
        let too_large = bytes::Bytes::from(vec![0u8; MAX_SINGLE_BYTES + 1]);
        assert!(stash.put(too_large, "image/png".into()).is_none());
    }

    #[test]
    fn put_at_single_limit_succeeds() {
        let stash = ImageStash::new();
        let at_limit = bytes::Bytes::from(vec![0u8; MAX_SINGLE_BYTES]);
        assert!(stash.put(at_limit, "image/png".into()).is_some());
    }

    // ── TTL 过期 ──────────────────────────────────────────────────────────

    #[test]
    fn expired_entry_returns_none() {
        let stash = ImageStash::new();
        let token = stash
            .put(bytes::Bytes::from(vec![1, 2, 3]), "image/png".into())
            .unwrap();

        // 手动把 expires_at 设为过去——模拟过期
        {
            let mut entries = stash.entries.write().unwrap();
            let img = entries.get_mut(&token).unwrap();
            img.expires_at = Instant::now() - Duration::from_secs(1);
        }

        assert!(stash.get(&token).is_none(), "过期条目应返回 None");
    }

    #[test]
    fn expires_in_seconds_decreasing() {
        let stash = ImageStash::new();
        let token = stash
            .put(bytes::Bytes::from(vec![1]), "image/png".into())
            .unwrap();
        let img1 = stash.get(&token).unwrap();
        let secs1 = img1.expires_in_seconds();
        // 应在 0..=900 秒范围（15 分钟 = 900 秒）
        assert!(secs1 <= 900, "剩余秒数应 <= 900，实际 {secs1}");
        assert!(secs1 > 850, "刚放入剩余应接近 900，实际 {secs1}");
    }

    // ── 项数上限淘汰 ──────────────────────────────────────────────────────

    #[test]
    fn evict_when_exceeding_max_items() {
        let stash = ImageStash::new();
        let mut tokens = Vec::new();
        for i in 0..MAX_ITEMS {
            tokens.push(
                stash
                    .put(bytes::Bytes::from(vec![i as u8]), "image/png".into())
                    .unwrap(),
            );
        }
        assert_eq!(stash.len(), MAX_ITEMS);

        // 放入第 MAX_ITEMS+1 项——应淘汰最早的
        let new_token = stash
            .put(bytes::Bytes::from(vec![0xFF]), "image/png".into())
            .unwrap();
        assert_eq!(stash.len(), MAX_ITEMS, "项数应保持 MAX_ITEMS");
        // 最早放入的应已被淘汰
        assert!(stash.get(&tokens[0]).is_none(), "最早放入的应被淘汰");
        // 新放入的应在
        assert!(stash.get(&new_token).is_some(), "新放入的应在");
    }

    // ── 总量上限淘汰 ──────────────────────────────────────────────────────

    #[test]
    fn evict_when_exceeding_max_total_bytes() {
        let stash = ImageStash::new();
        // 每项 10 MiB，放 6 项 = 60 MiB（< 64 MiB），放 7 项 = 70 MiB（> 64 MiB）
        let item_size = 10 * 1024 * 1024;
        let mut tokens = Vec::new();
        for i in 0..6 {
            tokens.push(
                stash
                    .put(
                        bytes::Bytes::from(vec![i as u8; item_size]),
                        "image/png".into(),
                    )
                    .unwrap(),
            );
        }
        assert_eq!(stash.len(), 6);

        // 第 7 项 → 总量 70 MiB > 64 MiB → 淘汰最早的
        let _t7 = stash
            .put(bytes::Bytes::from(vec![6u8; item_size]), "image/png".into())
            .unwrap();
        assert!(stash.total_size() <= MAX_TOTAL_BYTES, "总量应 <= 64 MiB");
        assert!(stash.get(&tokens[0]).is_none(), "最早的应被淘汰");
    }

    // ── put 清理过期项 ────────────────────────────────────────────────────

    #[test]
    fn put_cleans_expired_entries() {
        let stash = ImageStash::new();
        let token1 = stash
            .put(bytes::Bytes::from(vec![1]), "image/png".into())
            .unwrap();
        // 手动让 token1 过期
        {
            let mut entries = stash.entries.write().unwrap();
            entries.get_mut(&token1).unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        }
        // put 新条目应触发清理
        let _token2 = stash
            .put(bytes::Bytes::from(vec![2]), "image/png".into())
            .unwrap();
        assert!(stash.get(&token1).is_none(), "过期条目应已被清理");
    }

    // ── get 清理过期项 ────────────────────────────────────────────────────

    #[test]
    fn get_cleans_expired_entries() {
        let stash = ImageStash::new();
        let token1 = stash
            .put(bytes::Bytes::from(vec![1]), "image/png".into())
            .unwrap();
        let token2 = stash
            .put(bytes::Bytes::from(vec![2]), "image/png".into())
            .unwrap();
        // 手动让 token1 过期
        {
            let mut entries = stash.entries.write().unwrap();
            entries.get_mut(&token1).unwrap().expires_at = Instant::now() - Duration::from_secs(1);
        }
        // get token2 应触发清理 token1
        let _ = stash.get(&token2);
        assert!(stash.get(&token1).is_none(), "过期条目应已被清理");
    }

    // ── StashedImage::expires_in_seconds ─────────────────────────────────

    #[test]
    fn expires_in_seconds_zero_after_expiry() {
        let img = StashedImage {
            bytes: bytes::Bytes::from(vec![1]),
            mime: "image/png".into(),
            created_at: Instant::now() - Duration::from_secs(1000),
            expires_at: Instant::now() - Duration::from_secs(1),
        };
        assert_eq!(img.expires_in_seconds(), 0);
    }
}
