//! 快捷键录制专项诊断（一次性、低开销、仅主动录制期间采集）。
//!
//! 目的：定位"录制期间 WH_KEYBOARD_LL 只收到 WM_KEYUP、没有对应 WM_KEYDOWN，
//! 最终 10 秒超时"这类问题，把按键丢失归因到具体层次：
//!
//! 1. 按键在 recorder armed 前已按下 → `keys_down_at_arm` 非空
//! 2. Raw Input 收到 Down、LL Hook 未收到 → Hook 链/外部软件拦截
//! 3. Raw Input 与 LL Hook 都未收到 → 远控/驱动/会话层未传入
//! 4. Raw Input 收到但 recorder feed 丢弃 → 键值映射 / session 判定问题
//! 5. 前端显示"正在录制"晚于输入 → `ui_ready_ack_ms` 偏大
//!
//! **性能铁则**（Hook 热路径）：
//! - 只做 `try_lock` + 预分配缓冲写入，无 IO、无阻塞锁、无 Tauri、无逐事件日志
//! - 缓冲满或 `try_lock` 失败时丢弃事件并计数，汇总记录 `dropped_events`
//! - 录制结束后由阻塞等待线程统一 flush（逐事件 debug）+ 汇总（info）
//!
//! **隐私铁则**：
//! - 只在 recorder 活跃（用户主动录制）的最多 10 秒内采集，日常输入零采集
//! - 只记录 VK / scan code / 标准化键名，不记录文本、剪贴板、设备路径
//! - Raw 设备用不可逆哈希 tag 标识（进程内稳定，不还原原始句柄）
//!
//! 本模块只做诊断，generation/计数均不参与录制业务判断。

use std::collections::VecDeque;
use std::sync::Mutex;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

// ── 消息/相位分类（纯函数；windows.rs 传入原始 msg / flags）──────────────────

/// LL Hook 可能出现的四种键盘消息（WM_* 常量，避免依赖平台 crate 保持纯函数）。
const WM_KEYDOWN_MSG: u32 = 0x0100;
const WM_KEYUP_MSG: u32 = 0x0101;
const WM_SYSKEYDOWN_MSG: u32 = 0x0104;
const WM_SYSKEYUP_MSG: u32 = 0x0105;

/// RAWKEYBOARD.Flags 位（与 windows.rs 的 RI_KEY_* 一致，纯位运算用）。
const RI_KEY_BREAK_FLAG: u16 = 1;
const RI_KEY_E0_FLAG: u16 = 2;
const RI_KEY_E1_FLAG: u16 = 4;

/// LL Hook 键盘消息分类。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookMsgKind {
    KeyDown,
    KeyUp,
    SysKeyDown,
    SysKeyUp,
}

impl HookMsgKind {
    pub fn from_msg(msg: u32) -> Option<Self> {
        match msg {
            WM_KEYDOWN_MSG => Some(Self::KeyDown),
            WM_KEYUP_MSG => Some(Self::KeyUp),
            WM_SYSKEYDOWN_MSG => Some(Self::SysKeyDown),
            WM_SYSKEYUP_MSG => Some(Self::SysKeyUp),
            _ => None,
        }
    }

    pub fn is_down(self) -> bool {
        matches!(self, Self::KeyDown | Self::SysKeyDown)
    }
}

/// Raw Input MAKE/BREAK + E0/E1 分类结果。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RawKeyPhase {
    /// true = BREAK（松开）；false = MAKE（按下）。
    pub is_break: bool,
    pub e0: bool,
    pub e1: bool,
}

pub fn classify_raw_flags(flags: u16) -> RawKeyPhase {
    RawKeyPhase {
        is_break: flags & RI_KEY_BREAK_FLAG != 0,
        e0: flags & RI_KEY_E0_FLAG != 0,
        e1: flags & RI_KEY_E1_FLAG != 0,
    }
}

// ── feed 判定（平台输入层返回的轻量诊断枚举）────────────────────────────────

/// 平台输入事件被 recorder 接受 / 忽略的原因。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedIgnoreReason {
    /// VK 无法映射为 Blink 键名。
    UnsupportedVk,
    /// 非修饰键 keyup：录制语义只消费 down，不消费 up。
    NonModifierKeyup,
    /// feed 时 recorder 已不在录制（recording=false）。
    RecorderInactive,
    /// feed 时会话已过期/被清理（active_session_id 不匹配）。
    StaleSession,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedOutcomeKind {
    ModifierDown,
    ModifierUp,
    KeyDown,
}

/// 单个 Raw Input 事件进入 recorder 的最终结局。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FeedOutcome {
    Mapped { kind: FeedOutcomeKind, key: String },
    Ignored(FeedIgnoreReason),
}

/// `recorder::feed` 的接受情况（不改录制行为，仅用于诊断映射）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedStatus {
    Accepted,
    RecorderInactive,
    StaleSession,
}

impl FeedStatus {
    pub(crate) fn to_outcome(self, kind: FeedOutcomeKind, key: String) -> FeedOutcome {
        match self {
            FeedStatus::Accepted => FeedOutcome::Mapped { kind, key },
            FeedStatus::RecorderInactive => {
                FeedOutcome::Ignored(FeedIgnoreReason::RecorderInactive)
            }
            FeedStatus::StaleSession => FeedOutcome::Ignored(FeedIgnoreReason::StaleSession),
        }
    }
}

// ── 会话环境快照 ─────────────────────────────────────────────────────────────

/// armed 瞬间的环境快照（平台层在 Hook 回调之外采集，见 windows.rs）。
#[derive(Clone, Debug, Default)]
pub struct SessionEnv {
    pub foreground_is_blink: bool,
    pub windows_session_id: u32,
    pub remote_session: bool,
    pub integrity_level: String,
    /// armed 瞬间处于 Down 的 VK 列表（GetAsyncKeyState 枚举）。
    pub keys_down_at_arm: Vec<u32>,
}

// ── 事件记录 ─────────────────────────────────────────────────────────────────

/// LL Hook 事件（仅录制期间采集）。
#[derive(Clone, Copy, Debug)]
pub struct HookEventRecord {
    /// 相对 armed 的毫秒数。
    pub rel_ms: u64,
    pub hook_generation: u64,
    pub vk: u32,
    pub scan_code: u32,
    pub msg: HookMsgKind,
    /// KBDLLHOOKSTRUCT.flags 原始位。
    pub flags: u32,
    pub injected: bool,
}

/// LL Hook 事件的诊断入参（Hook 线程 → 缓冲；rel_ms 由会话在入队时补齐）。
#[derive(Clone, Copy, Debug)]
pub struct HookEventInput {
    pub hook_generation: u64,
    pub vk: u32,
    pub scan_code: u32,
    pub msg: HookMsgKind,
    /// KBDLLHOOKSTRUCT.flags 原始位。
    pub flags: u32,
    pub injected: bool,
}

/// Raw Input 键盘事件（仅录制期间采集，含非修饰键）。
#[derive(Clone, Copy, Debug)]
pub struct RawEventRecord {
    pub rel_ms: u64,
    pub vkey: u16,
    pub make_code: u16,
    pub is_break: bool,
    pub e0: bool,
    pub e1: bool,
    /// 设备句柄的不可逆哈希 tag（进程内稳定，不还原原始 HANDLE/路径）。
    pub device_tag: u64,
}

/// recorder feed 结局记录。
#[derive(Clone, Debug)]
pub struct FeedEventRecord {
    pub rel_ms: u64,
    pub vk: u32,
    pub outcome: FeedOutcome,
}

/// 设备句柄 → 进程内稳定 tag（乘法散列，不记录原始句柄/路径）。
fn device_tag(device_handle: usize) -> u64 {
    (device_handle as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15)
}

// ── 会话结果与汇总 ───────────────────────────────────────────────────────────

/// 录制会话终态。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionResult {
    Completed,
    Cancelled,
    Timeout,
    /// 并发录制被拒绝（recorder 已被其他会话占用）。
    Rejected,
}

/// 每次录制一条的结构化汇总。
#[derive(Clone, Debug)]
pub struct RecorderDiagSummary {
    pub session_id: u64,
    pub result: SessionResult,
    /// armed → 结束耗时（Rejected 会话未 armed，恒为 0）。
    pub elapsed_ms: u64,
    /// armed → 前端显示"正在录制"（ready ACK）耗时。
    pub ui_ready_ack_ms: Option<u64>,
    pub keys_down_at_arm: Vec<u32>,
    pub hook_generation: u64,
    pub hook_down: u64,
    pub hook_up: u64,
    pub raw_down: u64,
    pub raw_up: u64,
    pub feed_keydown: u64,
    pub feed_ignored: u64,
    pub feed_unsupported: u64,
    /// 缓冲满 + try_lock 失败丢弃的诊断事件总数。
    pub dropped_events: u64,
    pub env: SessionEnv,
}

impl RecorderDiagSummary {
    /// 并发拒绝时的最小汇总（未 armed，无事件）。
    pub fn rejected(active_session_id: u64) -> Self {
        Self {
            session_id: active_session_id,
            result: SessionResult::Rejected,
            elapsed_ms: 0,
            ui_ready_ack_ms: None,
            keys_down_at_arm: Vec::new(),
            hook_generation: 0,
            hook_down: 0,
            hook_up: 0,
            raw_down: 0,
            raw_up: 0,
            feed_keydown: 0,
            feed_ignored: 0,
            feed_unsupported: 0,
            dropped_events: 0,
            env: SessionEnv::default(),
        }
    }

    /// 渲染为多行 key=value 块（不含首行事件名，调用方拼日志头）。
    pub fn render(&self, request_id: &str) -> String {
        let keys_down = self
            .keys_down_at_arm
            .iter()
            .map(|vk| format!("0x{vk:02X}"))
            .collect::<Vec<_>>()
            .join(",");
        format!(
            "request_id={request_id}\n\
             session_id={}\n\
             result={:?}\n\
             elapsed_ms={}\n\
             ui_ready_ms={}\n\
             keys_down_at_arm=[{keys_down}]\n\
             hook_generation={}\n\
             raw_down={}\n\
             raw_up={}\n\
             hook_down={}\n\
             hook_up={}\n\
             feed_keydown={}\n\
             ignored={}\n\
             unsupported={}\n\
             dropped_events={}\n\
             remote_session={}\n\
             foreground_is_blink={}\n\
             windows_session_id={}\n\
             integrity_level={}",
            self.session_id,
            self.result,
            self.elapsed_ms,
            self.ui_ready_ack_ms.unwrap_or(0),
            self.hook_generation,
            self.raw_down,
            self.raw_up,
            self.hook_down,
            self.hook_up,
            self.feed_keydown,
            self.feed_ignored,
            self.feed_unsupported,
            self.dropped_events,
            self.env.remote_session,
            self.env.foreground_is_blink,
            self.env.windows_session_id,
            self.env.integrity_level,
        )
    }
}

// ── 环形缓冲 ─────────────────────────────────────────────────────────────────

/// 会话结束产出：汇总 + 三类事件（调用方负责 flush 日志）。
pub type SessionFlush = (
    RecorderDiagSummary,
    Vec<HookEventRecord>,
    Vec<RawEventRecord>,
    Vec<FeedEventRecord>,
);

/// 定长诊断缓冲：满时丢弃**新**事件并计数（保留 armed 早期事件，诊断价值更高）。
pub struct DiagRing<T> {
    buf: VecDeque<T>,
    capacity: usize,
    dropped: u64,
}

impl<T> DiagRing<T> {
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: VecDeque::with_capacity(capacity),
            capacity,
            dropped: 0,
        }
    }

    pub fn push(&mut self, event: T) {
        if self.buf.len() >= self.capacity {
            self.dropped += 1;
            return;
        }
        self.buf.push_back(event);
    }

    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    pub fn take(&mut self) -> Vec<T> {
        self.buf.drain(..).collect()
    }
}

// ── 会话核心（纯逻辑，可单测）─────────────────────────────────────────────────

/// ACK 回传结果（只影响诊断，不影响录制状态）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AckOutcome {
    Accepted(u64),
    StaleSession,
    NoActiveSession,
}

/// 单次录制会话的诊断核心。全局态是 `Mutex<Option<RecorderDiag>>`（见下方包装）。
pub struct RecorderDiag {
    session_id: u64,
    armed: Instant,
    hook_generation: u64,
    env: SessionEnv,
    hook_ring: DiagRing<HookEventRecord>,
    raw_ring: DiagRing<RawEventRecord>,
    feed_ring: DiagRing<FeedEventRecord>,
    ui_ready_ack_ms: Option<u64>,
}

const CAP_HOOK_EVENTS: usize = 256;
const CAP_RAW_EVENTS: usize = 256;
const CAP_FEED_EVENTS: usize = 128;

impl RecorderDiag {
    pub fn new(session_id: u64, armed: Instant, hook_generation: u64, env: SessionEnv) -> Self {
        Self {
            session_id,
            armed,
            hook_generation,
            env,
            hook_ring: DiagRing::new(CAP_HOOK_EVENTS),
            raw_ring: DiagRing::new(CAP_RAW_EVENTS),
            feed_ring: DiagRing::new(CAP_FEED_EVENTS),
            ui_ready_ack_ms: None,
        }
    }

    pub fn session_id(&self) -> u64 {
        self.session_id
    }

    /// LL Hook 事件（Hook 线程调用，仅预分配缓冲写入）。
    pub fn push_hook(&mut self, input: HookEventInput) {
        self.hook_ring.push(HookEventRecord {
            rel_ms: self.armed.elapsed().as_millis() as u64,
            hook_generation: input.hook_generation,
            vk: input.vk,
            scan_code: input.scan_code,
            msg: input.msg,
            flags: input.flags,
            injected: input.injected,
        });
    }

    /// Raw Input 键盘事件（Hook 线程调用，含非修饰键，仅录制期间）。
    pub fn push_raw(&mut self, vkey: u16, make_code: u16, flags: u16, device_handle: usize) {
        let phase = classify_raw_flags(flags);
        self.raw_ring.push(RawEventRecord {
            rel_ms: self.armed.elapsed().as_millis() as u64,
            vkey,
            make_code,
            is_break: phase.is_break,
            e0: phase.e0,
            e1: phase.e1,
            device_tag: device_tag(device_handle),
        });
    }

    /// recorder feed 结局（输入线程调用）。
    pub fn push_feed(&mut self, vk: u32, outcome: FeedOutcome) {
        self.feed_ring.push(FeedEventRecord {
            rel_ms: self.armed.elapsed().as_millis() as u64,
            vk,
            outcome,
        });
    }

    /// 前端 ready ACK。只接受匹配当前会话的 ACK，迟到/旧会话仅返回诊断结论。
    pub fn ack_ready(&mut self, session_id: u64, now: Instant) -> AckOutcome {
        if session_id != self.session_id {
            return AckOutcome::StaleSession;
        }
        if self.ui_ready_ack_ms.is_none() {
            self.ui_ready_ack_ms = Some(now.duration_since(self.armed).as_millis() as u64);
        }
        AckOutcome::Accepted(self.ui_ready_ack_ms.unwrap_or(0))
    }

    /// 结束会话：产出汇总并返回三类事件（调用方负责 flush 日志）。
    pub fn finish(
        mut self,
        result: SessionResult,
        now: Instant,
        lock_dropped: u64,
    ) -> SessionFlush {
        let hook_events = self.hook_ring.take();
        let raw_events = self.raw_ring.take();
        let feed_events = self.feed_ring.take();

        let mut hook_down = 0u64;
        let mut hook_up = 0u64;
        for e in &hook_events {
            if e.msg.is_down() {
                hook_down += 1;
            } else {
                hook_up += 1;
            }
        }
        let mut raw_down = 0u64;
        let mut raw_up = 0u64;
        for e in &raw_events {
            if e.is_break {
                raw_up += 1;
            } else {
                raw_down += 1;
            }
        }
        let mut feed_keydown = 0u64;
        let mut feed_ignored = 0u64;
        let mut feed_unsupported = 0u64;
        for e in &feed_events {
            match &e.outcome {
                FeedOutcome::Mapped {
                    kind: FeedOutcomeKind::KeyDown,
                    ..
                } => feed_keydown += 1,
                FeedOutcome::Mapped { .. } => {}
                FeedOutcome::Ignored(FeedIgnoreReason::UnsupportedVk) => feed_unsupported += 1,
                FeedOutcome::Ignored(_) => feed_ignored += 1,
            }
        }

        let dropped_events = self.hook_ring.dropped()
            + self.raw_ring.dropped()
            + self.feed_ring.dropped()
            + lock_dropped;

        let summary = RecorderDiagSummary {
            session_id: self.session_id,
            result,
            elapsed_ms: now.duration_since(self.armed).as_millis() as u64,
            ui_ready_ack_ms: self.ui_ready_ack_ms,
            keys_down_at_arm: std::mem::take(&mut self.env.keys_down_at_arm),
            hook_generation: self.hook_generation,
            hook_down,
            hook_up,
            raw_down,
            raw_up,
            feed_keydown,
            feed_ignored,
            feed_unsupported,
            dropped_events,
            env: self.env,
        };
        (summary, hook_events, raw_events, feed_events)
    }
}

// ── 会话槽位（无全局态封装；全局 SLOT 的行为与此一致，便于单测）────────────────

/// 会话槽位：Some = 录制中。end 后为 None，此后 push/ack 一律 no-op / NoActive。
pub struct SessionSink(pub Option<RecorderDiag>);

impl SessionSink {
    pub fn push_hook(&mut self, session_id: u64, input: HookEventInput) {
        if let Some(diag) = self.0.as_mut()
            && diag.session_id() == session_id
        {
            diag.push_hook(input);
        }
    }

    pub fn push_raw(&mut self, vkey: u16, make_code: u16, flags: u16, device_handle: usize) {
        // Raw Input 无法可靠映射 session：使用采集时槽位内的 active session 快照。
        if let Some(diag) = self.0.as_mut() {
            diag.push_raw(vkey, make_code, flags, device_handle);
        }
    }

    pub fn push_feed(&mut self, session_id: u64, vk: u32, outcome: FeedOutcome) {
        if let Some(diag) = self.0.as_mut()
            && diag.session_id() == session_id
        {
            diag.push_feed(vk, outcome);
        }
    }

    pub fn ack_ready(&mut self, session_id: u64, now: Instant) -> AckOutcome {
        match self.0.as_mut() {
            Some(diag) => diag.ack_ready(session_id, now),
            None => AckOutcome::NoActiveSession,
        }
    }

    /// 结束并取出会话；session 不匹配时不取（旧会话的槽位留给所有者）。
    pub fn end(
        &mut self,
        session_id: u64,
        result: SessionResult,
        now: Instant,
        lock_dropped: u64,
    ) -> Option<SessionFlush> {
        let matched = self.0.take_if(|diag| diag.session_id() == session_id)?;
        Some(matched.finish(result, now, lock_dropped))
    }
}

// ── 全局包装 ─────────────────────────────────────────────────────────────────

static SLOT: OnceLock<Mutex<SessionSink>> = OnceLock::new();
static LAST_SUMMARY: OnceLock<Mutex<Option<RecorderDiagSummary>>> = OnceLock::new();
/// try_lock 失败（主线程正在 begin/end）时丢弃的事件计数；begin_session 时清零。
static LOCK_DROPPED: AtomicU64 = AtomicU64::new(0);

fn slot_lock() -> &'static Mutex<SessionSink> {
    SLOT.get_or_init(|| Mutex::new(SessionSink(None)))
}

fn last_summary_lock() -> &'static Mutex<Option<RecorderDiagSummary>> {
    LAST_SUMMARY.get_or_init(|| Mutex::new(None))
}

/// recorder armed 后调用（spawn_blocking 线程，**非** Hook 回调）。
pub fn begin_session(session_id: u64, hook_generation: u64, env: SessionEnv) {
    tracing::info!(
        session_id,
        hook_generation,
        remote_session = env.remote_session,
        foreground_is_blink = env.foreground_is_blink,
        windows_session_id = env.windows_session_id,
        integrity_level = %env.integrity_level,
        keys_down_at_arm = ?env.keys_down_at_arm,
        "hotkey_recorder_diag_session_begin"
    );
    if let Ok(mut slot) = slot_lock().lock() {
        LOCK_DROPPED.store(0, Ordering::Relaxed);
        *slot = SessionSink(Some(RecorderDiag::new(
            session_id,
            Instant::now(),
            hook_generation,
            env,
        )));
    }
}

/// 并发拒绝时的最小汇总（未 armed，无事件），供 command 层与 request_id 一起输出。
pub fn begin_rejected_summary(active_session_id: u64) {
    if let Ok(mut last) = last_summary_lock().lock() {
        *last = Some(RecorderDiagSummary::rejected(active_session_id));
    }
    tracing::info!(active_session_id, "hotkey_recorder_diag_rejected");
}

/// 录制结束后调用（阻塞等待线程）：构建汇总、flush 事件、存入 LAST_SUMMARY。
pub fn end_session(session_id: u64, result: SessionResult) {
    let lock_dropped = LOCK_DROPPED.swap(0, Ordering::Relaxed);
    let taken = slot_lock()
        .lock()
        .ok()
        .and_then(|mut slot| slot.end(session_id, result, Instant::now(), lock_dropped));
    let Some((summary, hook_events, raw_events, feed_events)) = taken else {
        return;
    };

    if summary.dropped_events > 0 {
        tracing::warn!(
            session_id,
            dropped_events = summary.dropped_events,
            "hotkey_recorder_diag_events_dropped"
        );
    }
    for e in hook_events {
        tracing::debug!(
            session_id,
            rel_ms = e.rel_ms,
            hook_generation = e.hook_generation,
            vk = e.vk,
            scan_code = e.scan_code,
            msg = ?e.msg,
            flags = e.flags,
            injected = e.injected,
            "hotkey_diag_hook_event"
        );
    }
    for e in raw_events {
        tracing::debug!(
            session_id,
            rel_ms = e.rel_ms,
            vkey = e.vkey,
            make_code = e.make_code,
            phase = if e.is_break { "BREAK" } else { "MAKE" },
            e0 = e.e0,
            e1 = e.e1,
            device_tag = e.device_tag,
            "hotkey_diag_raw_event"
        );
    }
    for e in feed_events {
        match &e.outcome {
            FeedOutcome::Mapped { kind, key } => tracing::debug!(
                session_id,
                rel_ms = e.rel_ms,
                vk = e.vk,
                kind = ?kind,
                key = %key,
                "hotkey_diag_feed_event"
            ),
            FeedOutcome::Ignored(reason) => tracing::debug!(
                session_id,
                rel_ms = e.rel_ms,
                vk = e.vk,
                ignored = ?reason,
                "hotkey_diag_feed_event"
            ),
        }
    }

    if let Ok(mut last) = last_summary_lock().lock() {
        *last = Some(summary);
    }
}

/// LL Hook 事件（Hook 线程调用：try_lock + 缓冲写入，绝不阻塞）。
pub fn record_hook_event(session_id: u64, input: HookEventInput) {
    match slot_lock().try_lock() {
        Ok(mut slot) => {
            slot.push_hook(session_id, input);
        }
        Err(_) => {
            LOCK_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Raw Input 键盘事件（Hook 线程调用；session 取槽位内 active 快照）。
pub fn record_raw_event(vkey: u16, make_code: u16, flags: u16, device_handle: usize) {
    match slot_lock().try_lock() {
        Ok(mut slot) => {
            slot.push_raw(vkey, make_code, flags, device_handle);
        }
        Err(_) => {
            LOCK_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// recorder feed 结局（输入线程调用）。
pub fn record_feed_event(session_id: u64, vk: u32, outcome: FeedOutcome) {
    match slot_lock().try_lock() {
        Ok(mut slot) => {
            slot.push_feed(session_id, vk, outcome);
        }
        Err(_) => {
            LOCK_DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// 前端 ready ACK（command 线程）。只记录诊断，迟到/旧会话不影响录制状态。
pub fn ack_ready(session_id: u64, request_id: &str) -> AckOutcome {
    let outcome = slot_lock()
        .lock()
        .map(|mut slot| slot.ack_ready(session_id, Instant::now()))
        .unwrap_or(AckOutcome::NoActiveSession);
    match outcome {
        AckOutcome::Accepted(elapsed_ms) => {
            tracing::info!(
                request_id,
                session_id,
                elapsed_ms,
                "hotkey_recorder_ready_ack"
            );
        }
        AckOutcome::StaleSession => {
            tracing::debug!(
                request_id,
                session_id,
                "hotkey_recorder_ready_ack_stale_session"
            );
        }
        AckOutcome::NoActiveSession => {
            tracing::debug!(
                request_id,
                session_id,
                "hotkey_recorder_ready_ack_no_active_session"
            );
        }
    }
    outcome
}

/// command 层在录制结束后取走最近一次汇总（一次录制一条）。
pub fn take_last_summary() -> Option<RecorderDiagSummary> {
    last_summary_lock()
        .lock()
        .ok()
        .and_then(|mut last| last.take())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::thread::sleep;
    use std::time::Duration;

    fn fake_env() -> SessionEnv {
        SessionEnv {
            foreground_is_blink: true,
            windows_session_id: 1,
            remote_session: false,
            integrity_level: "0x2000".to_string(),
            keys_down_at_arm: vec![0xA2, 0xA4],
        }
    }

    fn hook_input(
        hook_generation: u64,
        vk: u32,
        msg: HookMsgKind,
        flags: u32,
        injected: bool,
    ) -> HookEventInput {
        HookEventInput {
            hook_generation,
            vk,
            scan_code: 8,
            msg,
            flags,
            injected,
        }
    }

    #[test]
    fn hook_msg_classification() {
        assert_eq!(HookMsgKind::from_msg(0x0100), Some(HookMsgKind::KeyDown));
        assert_eq!(HookMsgKind::from_msg(0x0101), Some(HookMsgKind::KeyUp));
        assert_eq!(HookMsgKind::from_msg(0x0104), Some(HookMsgKind::SysKeyDown));
        assert_eq!(HookMsgKind::from_msg(0x0105), Some(HookMsgKind::SysKeyUp));
        assert_eq!(HookMsgKind::from_msg(0x0102), None);
        assert!(HookMsgKind::KeyDown.is_down());
        assert!(HookMsgKind::SysKeyDown.is_down());
        assert!(!HookMsgKind::KeyUp.is_down());
        assert!(!HookMsgKind::SysKeyUp.is_down());
    }

    #[test]
    fn raw_make_break_classification() {
        assert!(!classify_raw_flags(0).is_break, "flags=0 是 MAKE");
        assert!(
            classify_raw_flags(RI_KEY_BREAK_FLAG).is_break,
            "bit0 是 BREAK"
        );
        assert!(classify_raw_flags(RI_KEY_BREAK_FLAG | RI_KEY_E0_FLAG).e0);
        assert!(classify_raw_flags(RI_KEY_E1_FLAG).e1);
        let phase = classify_raw_flags(RI_KEY_BREAK_FLAG | RI_KEY_E0_FLAG | RI_KEY_E1_FLAG);
        assert!(phase.is_break && phase.e0 && phase.e1);
    }

    #[test]
    fn ring_capacity_full_drops_and_counts() {
        let mut ring: DiagRing<u32> = DiagRing::new(8);
        for i in 0..13 {
            ring.push(i);
        }
        let items = ring.take();
        assert_eq!(items.len(), 8, "容量满后保留最早的 8 条");
        assert_eq!(items[0], 0);
        assert_eq!(items[7], 7);
        assert_eq!(ring.dropped(), 5, "新事件被丢弃并计数");
    }

    #[test]
    fn session_generation_association() {
        let mut sink = SessionSink(Some(RecorderDiag::new(4, Instant::now(), 17, fake_env())));

        sink.push_hook(4, hook_input(17, 0x59, HookMsgKind::KeyDown, 0, false));
        sink.push_hook(4, hook_input(18, 0x59, HookMsgKind::KeyUp, 0, false));

        let (summary, hook_events, _, _) = sink
            .end(4, SessionResult::Timeout, Instant::now(), 0)
            .expect("session 必须匹配");
        assert_eq!(summary.session_id, 4);
        assert_eq!(summary.hook_generation, 17, "汇总带 armed 时的 generation");
        assert_eq!(hook_events.len(), 2);
        assert_eq!(hook_events[0].hook_generation, 17);
        assert_eq!(
            hook_events[1].hook_generation, 18,
            "逐事件保留 generation 可判断跨重装"
        );
    }

    #[test]
    fn stale_ack_does_not_affect_session() {
        let armed = Instant::now();
        let mut diag = RecorderDiag::new(9, armed, 3, fake_env());

        let outcome = diag.ack_ready(8, Instant::now());
        assert_eq!(
            outcome,
            AckOutcome::StaleSession,
            "旧 session 的 ACK 被拒绝"
        );

        let outcome = diag.ack_ready(9, Instant::now());
        assert!(matches!(outcome, AckOutcome::Accepted(_)));
        // 重复 ACK 不覆盖首个耗时
        let _ = diag.ack_ready(9, Instant::now());

        let (summary, ..) = diag.finish(SessionResult::Cancelled, Instant::now(), 0);
        assert!(summary.ui_ready_ack_ms.is_some(), "匹配 ACK 记录耗时");
    }

    #[test]
    fn ack_after_end_reports_no_active_session() {
        let mut sink = SessionSink(Some(RecorderDiag::new(2, Instant::now(), 1, fake_env())));
        let _ = sink.end(2, SessionResult::Completed, Instant::now(), 0);
        assert_eq!(
            sink.ack_ready(2, Instant::now()),
            AckOutcome::NoActiveSession,
            "录制结束后迟到 ACK 只报诊断结论"
        );
    }

    #[test]
    fn finished_session_ignores_later_keys() {
        let mut sink = SessionSink(Some(RecorderDiag::new(3, Instant::now(), 5, fake_env())));
        sink.push_hook(3, hook_input(5, 0x41, HookMsgKind::KeyDown, 0, false));
        let (summary, ..) = sink
            .end(3, SessionResult::Completed, Instant::now(), 0)
            .expect("end 应取出会话");
        assert_eq!(summary.hook_down, 1);

        // 结束后普通键不再进入专项诊断
        sink.push_hook(3, hook_input(5, 0x42, HookMsgKind::KeyDown, 0, false));
        sink.push_raw(0x42, 0x13, 0, 0xdead);
        sink.push_feed(
            3,
            0x42,
            FeedOutcome::Mapped {
                kind: FeedOutcomeKind::KeyDown,
                key: "b".to_string(),
            },
        );
        assert!(sink.0.is_none(), "end 后槽位必须为空，事件全部丢弃");
    }

    #[test]
    fn summary_counts_all_layers() {
        let mut sink = SessionSink(Some(RecorderDiag::new(7, Instant::now(), 11, fake_env())));

        // Hook: 2 down + 2 up（含 injected）
        sink.push_hook(7, hook_input(11, 0x59, HookMsgKind::KeyDown, 0, false));
        sink.push_hook(7, hook_input(11, 0x59, HookMsgKind::KeyUp, 0, false));
        sink.push_hook(7, hook_input(11, 0x54, HookMsgKind::SysKeyDown, 0x10, true));
        sink.push_hook(7, hook_input(11, 0x54, HookMsgKind::SysKeyUp, 0x10, true));

        // Raw: 3 MAKE + 1 BREAK
        sink.push_raw(0x59, 0x15, 0, 0x1);
        sink.push_raw(0x59, 0x15, RI_KEY_BREAK_FLAG, 0x1);
        sink.push_raw(0x54, 0x17, 0, 0x1);
        sink.push_raw(0x54, 0x17, 0, 0x2);

        // Feed: 1 keydown + 1 modifier + 2 ignored(non-modifier keyup) + 1 unsupported
        sink.push_feed(
            7,
            0x59,
            FeedOutcome::Mapped {
                kind: FeedOutcomeKind::KeyDown,
                key: "y".to_string(),
            },
        );
        sink.push_feed(
            7,
            0xA4,
            FeedOutcome::Mapped {
                kind: FeedOutcomeKind::ModifierDown,
                key: "lalt".to_string(),
            },
        );
        sink.push_feed(
            7,
            0x54,
            FeedOutcome::Ignored(FeedIgnoreReason::NonModifierKeyup),
        );
        sink.push_feed(
            7,
            0x54,
            FeedOutcome::Ignored(FeedIgnoreReason::NonModifierKeyup),
        );
        sink.push_feed(
            7,
            0xFF,
            FeedOutcome::Ignored(FeedIgnoreReason::UnsupportedVk),
        );

        let (summary, ..) = sink
            .end(7, SessionResult::Timeout, Instant::now(), 2)
            .expect("session 必须匹配");
        assert_eq!(summary.hook_down, 2);
        assert_eq!(summary.hook_up, 2);
        assert_eq!(summary.raw_down, 3);
        assert_eq!(summary.raw_up, 1);
        assert_eq!(summary.feed_keydown, 1);
        assert_eq!(summary.feed_ignored, 2);
        assert_eq!(summary.feed_unsupported, 1);
        assert_eq!(summary.dropped_events, 2, "try_lock 丢弃计入汇总");
        assert_eq!(summary.result, SessionResult::Timeout);
        assert_eq!(summary.env.windows_session_id, 1);
        assert!(!summary.env.remote_session && summary.env.foreground_is_blink);
    }

    #[test]
    fn rejected_summary_has_no_events() {
        let summary = RecorderDiagSummary::rejected(6);
        assert_eq!(summary.result, SessionResult::Rejected);
        assert_eq!(summary.session_id, 6);
        assert_eq!(summary.elapsed_ms, 0);
        assert!(summary.keys_down_at_arm.is_empty());
        assert!(summary.ui_ready_ack_ms.is_none());
    }

    #[test]
    fn end_session_mismatch_keeps_slot() {
        let mut sink = SessionSink(Some(RecorderDiag::new(5, Instant::now(), 1, fake_env())));
        assert!(
            sink.end(4, SessionResult::Timeout, Instant::now(), 0)
                .is_none()
        );
        assert!(sink.0.is_some(), "session 不匹配不得取走槽位");
        // 原会话仍可被正确结束
        let (summary, ..) = sink
            .end(5, SessionResult::Cancelled, Instant::now(), 0)
            .expect("匹配 session 应可结束");
        assert_eq!(summary.session_id, 5);
    }

    #[test]
    fn rel_ms_is_measured_from_armed() {
        let armed = Instant::now();
        sleep(Duration::from_millis(5));
        let mut diag = RecorderDiag::new(1, armed, 1, fake_env());
        diag.push_hook(hook_input(1, 0x41, HookMsgKind::KeyDown, 0, false));
        let (_, hook_events, ..) = diag.finish(SessionResult::Completed, Instant::now(), 0);
        assert!(hook_events[0].rel_ms >= 5, "rel_ms 必须相对 armed");
    }

    #[test]
    fn summary_render_contains_expected_fields() {
        let summary = RecorderDiagSummary::rejected(4);
        let text = summary.render("req-1");
        assert!(text.starts_with("request_id=req-1\nsession_id=4"));
        assert!(text.contains("result=Rejected"));
        assert!(text.contains("remote_session=false"));
    }

    #[test]
    fn device_tag_is_stable_and_not_raw_handle() {
        assert_eq!(device_tag(0x1234), device_tag(0x1234));
        assert_ne!(device_tag(0x1234), device_tag(0x1235));
    }
}
