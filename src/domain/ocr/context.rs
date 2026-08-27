//! OCR 请求上下文（0.22.4）。
//!
//! 定义 `OcrRequestContext`，携带 deadline / cancellation token / origin。
//! `OcrBackend::recognize_with_context` 接收此上下文，
//! 普通 `ocr_image` Capability 只传调用 deadline/cancel/origin，
//! 截图 Interaction 额外传真实 session/selection generation。

use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;
/// OCR 请求来源。
#[derive(Debug, Clone)]
pub enum OcrRequestOrigin {
    /// 截图 Interaction 请求。
    /// 携带真实 session epoch / selection revision。
    Screenshot(ScreenshotOrigin),

    /// Capability 直接调用（ocr_image）。
    /// 不伪造截图 session。
    Capability,
}

/// 截图请求来源信息。
///
/// 预留诊断字段：`session_epoch` / `selection_revision` 在当前版本
/// 尚未被后端逻辑读取，但保留用于未来日志关联和请求追踪。
#[derive(Debug, Clone)]
pub struct ScreenshotOrigin {
    /// 截图 session epoch（每次新截图 session 递增）。
    #[allow(dead_code)]
    pub session_epoch: u64,
    /// 选区 revision（同一 session 内重选递增）。
    #[allow(dead_code)]
    pub selection_revision: u64,
}

/// OCR 请求的最小超时预算。
///
/// 即使调用方不设置 deadline，也会确保至少有这么多时间完成 OCR。
/// PaddleOCR 冷启动可能需要 10-30s，加上推理时间，
/// 设为 120s 作为底线保护。
pub const MIN_OCR_BUDGET: Duration = Duration::from_secs(120);

/// OCR 请求上下文（受限）。
///
/// 每次 recognize 开始时快照，中途修改配置不能改变在途请求。
///
/// **Task 5**：deadline 改为单调时钟 `Instant`，不使用 UNIX 毫秒模拟进程内 deadline。
/// 如果调用方未设置 deadline，使用 `MIN_OCR_BUDGET` 作为底线保护。
#[derive(Debug, Clone)]
pub struct OcrRequestContext {
    /// 请求唯一 ID（用于 tracker 和日志关联）。
    pub request_id: String,

    /// 请求 deadline（单调时钟的绝对时间点）。
    /// `None` = 无 deadline（确定性调用，如测试）。
    /// **Task 5**：改为 `Option<Instant>`，不使用 UNIX 毫秒。
    pub deadline: Option<Instant>,

    /// 取消 token。调用方 cancel 后，后端停止等待并丢弃结果。
    pub cancellation: CancellationToken,

    /// 请求来源。
    /// 预留：当前版本尚未在 `recognize_with_context` 实现中读取 `origin`，
    /// 但保留用于未来按来源区分超时/优先级。
    #[allow(dead_code)]
    pub origin: OcrRequestOrigin,
}

impl OcrRequestContext {
    /// 创建 Capability 调用的请求上下文。
    ///
    /// 不伪造截图 session；只传调用 deadline/cancel。
    ///
    /// **Task 5**：deadline 为 `Option<Instant>`（单调时钟）。
    /// 如果 `deadline` 为 `None`，使用 `MIN_OCR_BUDGET` 作为底线保护。
    pub fn for_capability(request_id: impl Into<String>, deadline: Option<Instant>) -> Self {
        let deadline = deadline.or_else(|| Some(Instant::now() + MIN_OCR_BUDGET));
        Self {
            request_id: request_id.into(),
            deadline,
            cancellation: CancellationToken::new(),
            origin: OcrRequestOrigin::Capability,
        }
    }

    /// 创建截图 Interaction 的请求上下文。
    ///
    /// **Task 5**：deadline 为 `Option<Instant>`（单调时钟）。
    /// 如果 `deadline` 为 `None`，使用 `MIN_OCR_BUDGET` 作为底线保护。
    pub fn for_screenshot(
        request_id: impl Into<String>,
        deadline: Option<Instant>,
        session_epoch: u64,
        selection_revision: u64,
    ) -> Self {
        let deadline = deadline.or_else(|| Some(Instant::now() + MIN_OCR_BUDGET));
        Self {
            request_id: request_id.into(),
            deadline,
            cancellation: CancellationToken::new(),
            origin: OcrRequestOrigin::Screenshot(ScreenshotOrigin {
                session_epoch,
                selection_revision,
            }),
        }
    }

    /// 返回剩余超时预算（Duration）。
    ///
    /// 如果 deadline 已过返回 `Duration::ZERO`。
    /// 如果无 deadline 返回 `None`。
    pub fn remaining_timeout(&self) -> Option<Duration> {
        self.deadline.map(|d| {
            let now = Instant::now();
            if d > now { d - now } else { Duration::ZERO }
        })
    }

    /// 检查 deadline 是否已过。
    pub fn is_deadline_expired(&self) -> bool {
        self.deadline.map(|d| Instant::now() >= d).unwrap_or(false)
    }

    /// 检查请求是否已被取消。
    pub fn is_cancelled(&self) -> bool {
        self.cancellation.is_cancelled()
    }

    /// 检查请求是否应该停止（deadline 过期或已取消）。
    pub fn should_stop(&self) -> bool {
        self.is_cancelled() || self.is_deadline_expired()
    }

    /// 返回是否来自截图 Interaction。
    /// 预留诊断方法：当前版本无 caller，保留供未来请求追踪使用。
    #[allow(dead_code)]
    pub fn is_from_screenshot(&self) -> bool {
        matches!(self.origin, OcrRequestOrigin::Screenshot(_))
    }

    /// 返回截图来源信息（如果来自截图）。
    /// 预留诊断方法：当前版本无 caller，保留供未来请求追踪使用。
    #[allow(dead_code)]
    pub fn screenshot_origin(&self) -> Option<&ScreenshotOrigin> {
        match &self.origin {
            OcrRequestOrigin::Screenshot(s) => Some(s),
            _ => None,
        }
    }
}

// ── OcrRequestTracker ──────────────────────────────────────────────────────

/// OCR 请求取消追踪器。
///
/// 全局注册表，映射 `request_id → CancellationToken`。
/// `ocr_image` Capability 在开始处理时注册，处理完成后注销。
/// `cancel_ocr_request` command 通过 `request_id` 取消在途请求。
///
/// **设计决策**：
/// - 使用 `OnceLock<Arc<RwLock<HashMap>>>` 全局单例，不依赖 Tauri State。
/// - `CancellationToken` 是 `Arc` 内部的，clone 开销极小。
/// - 请求完成后自动注销，避免 map 无限增长。
/// - `request_id` 由调用方生成（时间戳 + 随机后缀），不在此模块生成。
pub struct OcrRequestTracker {
    requests: std::sync::RwLock<std::collections::HashMap<String, CancellationToken>>,
    /// Task 6: pending-cancel tombstones——cancel 在 register 之前到达时记录。
    /// register 时发现该 request 已预取消，返回已取消的 token。
    /// 有数量上限和 TTL，避免无限增长。
    /// Task 6: 改为带时间戳的 HashMap，支持 TTL 过期清理。
    pending_cancels: std::sync::RwLock<std::collections::HashMap<String, std::time::Instant>>,
}

impl OcrRequestTracker {
    fn new() -> Self {
        Self {
            requests: std::sync::RwLock::new(std::collections::HashMap::new()),
            pending_cancels: std::sync::RwLock::new(std::collections::HashMap::new()),
        }
    }

    /// pending-cancel tombstone 的最大数量。
    const MAX_PENDING_CANCELS: usize = 64;

    /// pending-cancel tombstone 的 TTL（5 分钟）。
    /// 超过此时间的 tombstone 会被清理，避免无限增长。
    const PENDING_CANCEL_TTL: std::time::Duration = std::time::Duration::from_secs(300);

    /// 注册一个请求及其取消 token。
    ///
    /// Task 6: 如果该 request_id 已在 pending_cancels 中（cancel 先于 register 到达），
    /// 立即取消 token 并从 pending_cancels 移除。
    /// 这样 register 后的 cancel 检查能看到已取消状态。
    ///
    /// 返回 `OcrRequestGuard`，drop 时自动注销请求。
    pub fn register(&self, request_id: &str, token: CancellationToken) -> OcrRequestGuard<'_> {
        // Task 6: 检查是否有 pending cancel
        let was_pre_cancelled = if let Ok(mut pc) = self.pending_cancels.write() {
            // Task 6: 先清理过期 tombstone
            Self::evict_expired_locked(&mut pc);
            if let Some((_ts, removed)) = pc.remove_entry(request_id).map(|(k, v)| (v, k)) {
                let _ = removed;
                tracing::info!(request_id = %request_id, "OCR 请求在 register 前已被取消，立即取消 token");
                true
            } else {
                false
            }
        } else {
            false
        };
        if was_pre_cancelled {
            token.cancel();
        }
        if let Ok(mut w) = self.requests.write() {
            w.insert(request_id.to_string(), token);
            tracing::debug!(request_id = %request_id, pre_cancelled = was_pre_cancelled, "OCR 请求已注册到 tracker");
        }
        OcrRequestGuard {
            tracker: self,
            request_id: request_id.to_string(),
        }
    }

    /// 注销一个请求（请求完成后调用）。
    ///
    /// Task 6: 同时清理 pending_cancels 中的残留 tombstone。
    pub fn unregister(&self, request_id: &str) {
        if let Ok(mut w) = self.requests.write() {
            w.remove(request_id);
            tracing::debug!(request_id = %request_id, "OCR 请求已从 tracker 注销");
        }
        // 同时清理 pending cancel tombstone
        if let Ok(mut pc) = self.pending_cancels.write() {
            pc.remove(request_id);
        }
    }

    /// 取消指定 `request_id` 的请求。
    ///
    /// Task 6: 如果 request 尚未 register（cancel-before-register 竞态），
    /// 在 pending_cancels 中记录带时间戳的 tombstone，register 时会立即取消。
    /// 返回 `true` 表示找到并取消了请求或已记录 tombstone。
    pub fn cancel(&self, request_id: &str) -> bool {
        // 先尝试取消已注册的请求
        if let Ok(r) = self.requests.read() {
            if let Some(token) = r.get(request_id) {
                token.cancel();
                tracing::info!(request_id = %request_id, "OCR 请求已被取消");
                return true;
            }
        }
        // Task 6: request 尚未注册——记录带时间戳的 pending cancel tombstone
        if let Ok(mut pc) = self.pending_cancels.write() {
            // Task 6: 先清理过期 tombstone
            Self::evict_expired_locked(&mut pc);
            // 数量上限保护
            if pc.len() >= Self::MAX_PENDING_CANCELS {
                // Task 6: 按时间戳排序，删除最老的 tombstone
                let mut entries: Vec<(String, std::time::Instant)> =
                    pc.iter().map(|(k, v)| (k.clone(), *v)).collect();
                entries.sort_by_key(|(_, ts)| *ts);
                let to_remove = entries.len() - (Self::MAX_PENDING_CANCELS - 1);
                for (k, _) in entries.into_iter().take(to_remove) {
                    pc.remove(&k);
                }
                tracing::warn!(count = pc.len(), "pending_cancels 已满，清理旧 tombstone");
            }
            pc.insert(request_id.to_string(), std::time::Instant::now());
            tracing::info!(request_id = %request_id, "OCR 请求尚未注册，已记录 pending cancel tombstone");
            return true;
        }
        false
    }

    /// 返回当前在途请求数量（诊断用）。
    #[allow(dead_code)] // 预留诊断方法，未来供 /health 端点或设置页使用
    pub fn in_flight_count(&self) -> usize {
        self.requests.read().map(|r| r.len()).unwrap_or(0)
    }

    /// Task 6: 清理过期的 pending cancel tombstones（内部方法）。
    ///
    /// 清理创建时间超过 `PENDING_CANCEL_TTL` 的 tombstone。
    fn evict_expired_locked(pending: &mut std::collections::HashMap<String, std::time::Instant>) {
        let now = std::time::Instant::now();
        let ttl = Self::PENDING_CANCEL_TTL;
        let before = pending.len();
        pending.retain(|id, ts| {
            let keep = now.duration_since(*ts) < ttl;
            if !keep {
                tracing::debug!(request_id = %id, "清理过期 pending cancel tombstone");
            }
            keep
        });
        let removed = before - pending.len();
        if removed > 0 {
            tracing::debug!(
                removed,
                remaining = pending.len(),
                "清理过期 pending cancel tombstones"
            );
        }
    }
}

/// Task 6: RAII guard，drop 时自动注销 OCR 请求。
///
/// `OcrRequestTracker::register` 返回此 guard，
/// 当 guard 被 drop 时自动调用 `unregister`，
/// 确保 panic / early return 也能正确清理。
pub struct OcrRequestGuard<'a> {
    tracker: &'a OcrRequestTracker,
    request_id: String,
}

impl Drop for OcrRequestGuard<'_> {
    fn drop(&mut self) {
        self.tracker.unregister(&self.request_id);
    }
}

/// 全局 `OcrRequestTracker` 单例。
static GLOBAL_TRACKER: std::sync::OnceLock<OcrRequestTracker> = std::sync::OnceLock::new();

/// 获取全局 `OcrRequestTracker`。
pub fn ocr_request_tracker() -> &'static OcrRequestTracker {
    GLOBAL_TRACKER.get_or_init(OcrRequestTracker::new)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capability_context_does_not_fake_screenshot() {
        let ctx = OcrRequestContext::for_capability("req-1", None);
        assert!(!ctx.is_from_screenshot());
        assert!(ctx.screenshot_origin().is_none());
    }

    #[test]
    fn screenshot_context_carries_session_and_revision() {
        let ctx = OcrRequestContext::for_screenshot("req-2", None, 42, 7);
        assert!(ctx.is_from_screenshot());
        let origin = ctx.screenshot_origin().unwrap();
        assert_eq!(origin.session_epoch, 42);
        assert_eq!(origin.selection_revision, 7);
    }

    #[test]
    fn cancellation_propagates() {
        let ctx = OcrRequestContext::for_capability("req-3", None);
        assert!(!ctx.is_cancelled());
        ctx.cancellation.cancel();
        assert!(ctx.is_cancelled());
        assert!(ctx.should_stop());
    }

    #[test]
    fn deadline_expired_detected() {
        // deadline 设为过去
        let past = Instant::now() - Duration::from_secs(1);
        let ctx = OcrRequestContext::for_capability("req-4", Some(past));
        assert!(ctx.is_deadline_expired());
        assert!(ctx.should_stop());
        assert_eq!(ctx.remaining_timeout(), Some(Duration::ZERO));
    }

    #[test]
    fn no_deadline_never_expires() {
        // Task 5: for_capability(None) 现在会设置 MIN_OCR_BUDGET
        // 所以 deadline 不再是 None——验证它有合理的超时预算
        let ctx = OcrRequestContext::for_capability("req-5", None);
        assert!(!ctx.is_deadline_expired(), "刚创建的 ctx 不应已过期");
        assert!(!ctx.should_stop());
        // deadline 应该是 Some（有 min 预算保护）
        assert!(ctx.deadline.is_some());
        // remaining_timeout 应该接近 MIN_OCR_BUDGET
        let remaining = ctx.remaining_timeout().unwrap();
        assert!(
            remaining > Duration::from_secs(100),
            "刚创建的 ctx 应有接近 MIN_OCR_BUDGET 的剩余时间，实际 {:?}",
            remaining
        );
    }

    /// Task 5: 验证 None deadline 时使用 MIN_OCR_BUDGET
    #[test]
    fn none_deadline_uses_min_budget() {
        let ctx = OcrRequestContext::for_capability("req-min-budget", None);
        assert!(
            ctx.deadline.is_some(),
            "None deadline 应被替换为 MIN_OCR_BUDGET"
        );
        let remaining = ctx.remaining_timeout().unwrap();
        // 应该接近 MIN_OCR_BUDGET（允许微小的时间偏差）
        assert!(
            remaining > Duration::from_secs(MIN_OCR_BUDGET.as_secs() - 5),
            "剩余时间应接近 MIN_OCR_BUDGET，实际 {:?}",
            remaining
        );
    }

    /// Task 5: 验证显式 deadline 不被 min budget 覆盖
    #[test]
    fn explicit_short_deadline_not_overridden() {
        let short = Instant::now() + Duration::from_secs(5);
        let ctx = OcrRequestContext::for_capability("req-short", Some(short));
        assert_eq!(ctx.deadline, Some(short));
        let remaining = ctx.remaining_timeout().unwrap();
        assert!(remaining <= Duration::from_secs(5));
    }

    /// Task 5: 验证截图 ctx 也获得 min budget
    #[test]
    fn screenshot_none_deadline_uses_min_budget() {
        let ctx = OcrRequestContext::for_screenshot("req-screenshot-min", None, 1, 1);
        assert!(ctx.deadline.is_some());
        assert!(!ctx.is_deadline_expired());
        let remaining = ctx.remaining_timeout().unwrap();
        assert!(
            remaining > Duration::from_secs(MIN_OCR_BUDGET.as_secs() - 5),
            "截图 ctx 也应有 min budget，实际 {:?}",
            remaining
        );
    }

    // ── Task 6: cancel-before-register 竞态测试 ──

    #[test]
    fn cancel_before_register_cancels_on_register() {
        // cancel 在 register 之前到达——tombstone 应该在 register 时生效
        let tracker = OcrRequestTracker::new();
        let request_id = "test-cancel-before-register";

        // cancel 先到——request 尚未注册
        let cancelled = tracker.cancel(request_id);
        assert!(cancelled, "cancel 应返回 true（已记录 tombstone）");

        // register 后——token 应该立即被取消
        let token = CancellationToken::new();
        let _guard = tracker.register(request_id, token.clone());
        assert!(token.is_cancelled(), "token 应在 register 时被取消");

        // guard drop 时自动注销
    }

    #[test]
    fn cancel_after_register_cancels_immediately() {
        // 正常路径：register 先到，cancel 后到
        let tracker = OcrRequestTracker::new();
        let request_id = "test-cancel-after-register";

        let token = CancellationToken::new();
        let _guard = tracker.register(request_id, token.clone());

        let cancelled = tracker.cancel(request_id);
        assert!(cancelled);
        assert!(token.is_cancelled());
    }

    #[test]
    fn unregister_clears_pending_cancel() {
        // unregister 应该同时清理 pending_cancels
        let tracker = OcrRequestTracker::new();
        let request_id = "test-unregister-clears";

        tracker.cancel(request_id);
        assert!(
            tracker
                .pending_cancels
                .read()
                .unwrap()
                .contains_key(request_id)
        );

        tracker.unregister(request_id);
        assert!(
            !tracker
                .pending_cancels
                .read()
                .unwrap()
                .contains_key(request_id)
        );
    }

    #[test]
    fn old_cancel_does_not_affect_new_request() {
        // 旧 request 的 cancel tombstone 不应影响新 request
        let tracker = OcrRequestTracker::new();

        // 旧 request cancel（tombstone）
        tracker.cancel("old-request-id");

        // 新 request register——不应被取消
        let new_token = CancellationToken::new();
        let _guard = tracker.register("new-request-id", new_token.clone());
        assert!(!new_token.is_cancelled(), "新 request 不应被旧 cancel 影响");
    }

    #[test]
    fn pending_cancels_respects_max_limit() {
        // 超过 MAX_PENDING_CANCELS 时自动清理
        let tracker = OcrRequestTracker::new();

        // 填满 pending_cancels
        for i in 0..(OcrRequestTracker::MAX_PENDING_CANCELS + 10) {
            tracker.cancel(&format!("overflow-req-{i}"));
        }

        let count = tracker.pending_cancels.read().unwrap().len();
        assert!(
            count <= OcrRequestTracker::MAX_PENDING_CANCELS,
            "pending_cancels 不应超过上限，实际 {}",
            count
        );
    }

    /// Task 6: 验证 RAII guard 在 drop 时自动注销
    #[test]
    fn guard_drop_auto_unregisters() {
        let tracker = OcrRequestTracker::new();
        let request_id = "test-guard-drop";

        let token = CancellationToken::new();
        {
            let _guard = tracker.register(request_id, token.clone());
            assert_eq!(tracker.in_flight_count(), 1);
        }
        // guard drop 后应自动注销
        assert_eq!(tracker.in_flight_count(), 0);
    }

    /// Task 6: 验证 tombstone 带时间戳
    #[test]
    fn tombstone_has_timestamp() {
        let tracker = OcrRequestTracker::new();
        let request_id = "test-tombstone-timestamp";

        tracker.cancel(request_id);

        let pc = tracker.pending_cancels.read().unwrap();
        let ts = pc.get(request_id).expect("tombstone 应存在");
        // 时间戳应该是刚创建的
        let elapsed = std::time::Instant::now().duration_since(*ts);
        assert!(
            elapsed < std::time::Duration::from_secs(1),
            "tombstone 时间戳应是刚创建的"
        );
    }
}
