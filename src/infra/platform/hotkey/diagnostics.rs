//! 输入系统诊断：快照 + 环形缓冲区。
//!
//! 供 `blink_print_debug_info` 和 `blink_debug_inithook` 使用。
//!
//! **设计原则**：
//! - 写入端（hook 线程）使用 `try_lock`，绝不阻塞热路径
//! - 读取端（主线程）使用 `lock`，频率低（仅 debug 请求时）
//! - 不记录用户输入内容（隐私）
//! - 不写入磁盘

#![allow(dead_code)]

use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use super::state::{
    InputState, ModifierKey, ModifierLevel, PhysicalModifierSnapshot, ReinstallReason,
};

// ── 快照 ──────────────────────────────────────────────────────────────────────

/// InputState 的诊断子集（可 Copy，避免克隆整个 InputState）。
///
/// 由 hook 线程在每次 `apply_reduce_result` 后更新，
/// 由主线程在 `blink_print_debug_info` 请求时读取。
#[derive(Clone, Debug)]
pub struct InputStateSnapshot {
    // Modifiers
    pub modifier_levels: [ModifierLevel; 8],
    pub pressed_mask: u16,

    // Gesture
    pub gesture_idle: bool,
    pub gesture_armed: bool,

    // Chord
    pub chord_active: bool,
    pub chord_session_id: Option<u64>,

    // Voice
    pub voice_idle: bool,

    // Recorder
    pub recorder_idle: bool,

    // Window
    pub window_visible: bool,
    pub window_revision: u64,

    // View
    pub view_ready: bool,
    pub view_epoch: u64,
    pub view_revision: u64,
    pub view_query_empty: bool,
    pub view_ai_mode: bool,

    // Config
    pub config_revision: u64,

    // Desired UI (from InputState.last_ui_state)
    pub desired_alt_down: bool,
    pub desired_chord_active: bool,
    pub desired_revision: u64,
}

impl InputStateSnapshot {
    /// 从 InputState 提取诊断快照。
    pub fn from_state(state: &InputState) -> Self {
        let modifier_levels = ModifierKey::ALL.map(|k| state.modifiers.level(k));
        let pressed_mask = state.modifiers.pressed_mask();

        Self {
            modifier_levels,
            pressed_mask,
            gesture_idle: matches!(state.gesture, super::state::GestureState::Idle),
            gesture_armed: matches!(state.gesture, super::state::GestureState::Armed { .. }),
            chord_active: state.chord.is_active(),
            chord_session_id: match state.chord {
                super::state::ChordSession::Active { session_id, .. } => Some(session_id),
                _ => None,
            },
            voice_idle: matches!(state.voice, super::state::VoicePhase::Idle),
            recorder_idle: matches!(state.recorder, super::state::RecorderMode::Idle),
            window_visible: state.window.visible,
            window_revision: state.window.revision,
            view_ready: state.view.ready,
            view_epoch: state.view.view_epoch,
            view_revision: state.view.revision,
            view_query_empty: state.view.query_empty,
            view_ai_mode: state.view.ai_mode,
            config_revision: state.config_revision,
            desired_alt_down: state.ui_state().alt_down,
            desired_chord_active: state.ui_state().exclusive_chord_active,
            desired_revision: state.ui_state().revision,
        }
    }
}

/// Hook 状态诊断信息（由 Windows adapter 通过共享原子更新）。
#[derive(Clone, Debug, Default)]
pub struct HookDiagnosticInfo {
    pub hook_installed: bool,
    pub hook_available: bool,
    pub pending_reinstall: Option<ReinstallReason>,
    pub reinstall_attempt: u8,
    pub wts_registered: bool,
    pub raw_registered: bool,
    /// WH_KEYBOARD_LL generation（每次成功安装递增，仅诊断用途）。
    pub hook_generation: u64,
}

/// 完整的输入系统诊断快照。
///
/// 由 `blink_print_debug_info` 动作读取，格式化为可复制文本。
#[derive(Clone, Debug)]
pub struct InputDiagnosticSnapshot {
    pub state: InputStateSnapshot,
    pub hook: HookDiagnosticInfo,
    pub physical: PhysicalModifierSnapshot,
    /// 已发布的 UI 状态（从原子变量读取）。
    pub published_alt_down: bool,
    pub published_chord_active: bool,
    pub published_revision: u64,
    /// 快照生成时的运行时间（毫秒）。
    pub uptime_ms: u64,
}

// ── 共享存储 ──────────────────────────────────────────────────────────────────

/// 全局状态快照（hook 线程写 try_lock，主线程读 lock）。
static STATE_SNAPSHOT: OnceLock<Mutex<Option<InputStateSnapshot>>> = OnceLock::new();

/// 全局 Hook 诊断信息（hook 线程写，主线程读）。
static HOOK_INFO: OnceLock<Mutex<HookDiagnosticInfo>> = OnceLock::new();

/// 进程启动时间（用于计算 uptime）。
static START_TIME: OnceLock<Instant> = OnceLock::new();

fn state_snapshot_lock() -> &'static Mutex<Option<InputStateSnapshot>> {
    STATE_SNAPSHOT.get_or_init(|| Mutex::new(None))
}

fn hook_info_lock() -> &'static Mutex<HookDiagnosticInfo> {
    HOOK_INFO.get_or_init(|| Mutex::new(HookDiagnosticInfo::default()))
}

fn start_time() -> Instant {
    *START_TIME.get_or_init(Instant::now)
}

// ── 写入 API（hook 线程调用）──────────────────────────────────────────────────

/// 更新状态快照（hook 线程，try_lock 不阻塞）。
///
/// 在 `apply_reduce_result` 中调用。如果主线程正在读取（罕见），
/// 此次更新被跳过——可接受，因为下次事件即修正。
pub fn update_state_snapshot(state: &InputState) {
    let snapshot = InputStateSnapshot::from_state(state);
    if let Ok(mut guard) = state_snapshot_lock().try_lock() {
        *guard = Some(snapshot);
    }
}

/// 更新 Hook 诊断信息（hook 线程调用）。
pub fn update_hook_info(info: &HookDiagnosticInfo) {
    if let Ok(mut guard) = hook_info_lock().try_lock() {
        *guard = info.clone();
    }
}

// ── 读取 API（主线程调用）──────────────────────────────────────────────────────

/// 读取完整的输入系统诊断快照（主线程调用）。
///
/// `physical` 由调用方提供（Windows adapter 通过 `GetAsyncKeyState` 读取）。
pub fn take_diagnostic_snapshot(physical: PhysicalModifierSnapshot) -> InputDiagnosticSnapshot {
    let state = state_snapshot_lock()
        .lock()
        .map(|g| g.clone())
        .ok()
        .flatten();
    let hook = hook_info_lock()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default();

    let published = super::get_latest_ui_state();

    InputDiagnosticSnapshot {
        state: state.unwrap_or_default(),
        hook,
        physical,
        published_alt_down: published.alt_down,
        published_chord_active: published.exclusive_chord_active,
        published_revision: published.revision,
        uptime_ms: start_time().elapsed().as_millis() as u64,
    }
}

impl Default for InputStateSnapshot {
    fn default() -> Self {
        Self {
            modifier_levels: [ModifierLevel::Unknown; 8],
            pressed_mask: 0,
            gesture_idle: true,
            gesture_armed: false,
            chord_active: false,
            chord_session_id: None,
            voice_idle: true,
            recorder_idle: true,
            window_visible: false,
            window_revision: 0,
            view_ready: false,
            view_epoch: 0,
            view_revision: 0,
            view_query_empty: true,
            view_ai_mode: false,
            config_revision: 0,
            desired_alt_down: false,
            desired_chord_active: false,
            desired_revision: 0,
        }
    }
}

// ── 环形缓冲区 ────────────────────────────────────────────────────────────────

/// 诊断事件来源。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticSource {
    Hook,
    Raw,
    Physical,
    Control,
    SessionReset,
    HoldTimer,
}

/// 诊断事件中的键分类（不记录具体字符，保护隐私）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticKeyClass {
    Modifier(ModifierKey),
    /// 主热键主键（如 Space）。
    MainKey,
    /// Chord 键或其他非修饰键。
    OtherKey,
    /// 无具体键（ConfigChanged / WindowChanged 等）。
    None,
}

/// 诊断事件转换类型。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiagnosticTransition {
    Down,
    Up,
    Reconcile,
    ConfigChanged,
    WindowChanged,
    VoicePhaseChanged,
    RecorderModeChanged,
    SessionReset,
    ManualRecovery,
    HoldDeadline,
    RawDeviceRemoved,
}

/// 单条诊断事件（仅元数据，不含用户输入内容）。
#[derive(Clone, Debug)]
pub struct InputDiagnosticEvent {
    pub seq: u64,
    pub elapsed_ms: u64,
    pub source: DiagnosticSource,
    pub key: DiagnosticKeyClass,
    pub transition: DiagnosticTransition,
    pub injected: Option<bool>,
    pub before_level: Option<ModifierLevel>,
    pub after_level: Option<ModifierLevel>,
    pub chord_before: bool,
    pub chord_after: bool,
    pub ui_effect_emitted: bool,
}

/// 环形缓冲区容量。
const RING_CAPACITY: usize = 64;

/// 全局环形缓冲区。
static RING_BUFFER: OnceLock<Mutex<Vec<InputDiagnosticEvent>>> = OnceLock::new();
static RING_SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

fn ring_buffer_lock() -> &'static Mutex<Vec<InputDiagnosticEvent>> {
    RING_BUFFER.get_or_init(|| Mutex::new(Vec::with_capacity(RING_CAPACITY)))
}

/// 推入一条诊断事件（hook 线程，try_lock 不阻塞）。
pub fn push_diagnostic_event(event: InputDiagnosticEvent) {
    if let Ok(mut guard) = ring_buffer_lock().try_lock() {
        if guard.len() >= RING_CAPACITY {
            guard.remove(0);
        }
        guard.push(event);
    }
}

/// 读取所有诊断事件（主线程调用）。
pub fn take_diagnostic_events() -> Vec<InputDiagnosticEvent> {
    ring_buffer_lock()
        .lock()
        .map(|g| g.clone())
        .unwrap_or_default()
}

/// 分配下一个序列号。
pub fn next_seq() -> u64 {
    RING_SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
}

/// 返回相对进程内诊断时钟的毫秒数，不包含墙钟或用户信息。
pub fn elapsed_ms() -> u64 {
    start_time().elapsed().as_millis() as u64
}

/// 从 `InputEvent` 提取诊断事件信息。
///
/// 返回 `(source, key_class, transition, injected)` 元组，
/// 调用方负责填充 before/after level 等动态字段。
pub fn extract_event_meta(
    event: &super::state::InputEvent,
) -> (
    DiagnosticSource,
    DiagnosticKeyClass,
    DiagnosticTransition,
    Option<bool>,
) {
    use super::state::InputEvent as E;

    match event {
        E::HookKey(e) => {
            let key_class = if e.is_modifier {
                DiagnosticKeyClass::Modifier(
                    ModifierKey::from_key_name(&e.key).unwrap_or(ModifierKey::LAlt),
                )
            } else if e.key == " " {
                DiagnosticKeyClass::MainKey
            } else {
                DiagnosticKeyClass::OtherKey
            };
            let transition = if e.is_down {
                DiagnosticTransition::Down
            } else {
                DiagnosticTransition::Up
            };
            (
                DiagnosticSource::Hook,
                key_class,
                transition,
                Some(e.injected),
            )
        }
        E::RawModifier(e) => {
            let transition = if e.is_down {
                DiagnosticTransition::Down
            } else {
                DiagnosticTransition::Up
            };
            (
                DiagnosticSource::Raw,
                DiagnosticKeyClass::Modifier(e.key),
                transition,
                None,
            )
        }
        E::RawDeviceRemoved { .. } => (
            DiagnosticSource::Raw,
            DiagnosticKeyClass::None,
            DiagnosticTransition::RawDeviceRemoved,
            None,
        ),
        E::PhysicalModifiersObserved { .. } => (
            DiagnosticSource::Physical,
            DiagnosticKeyClass::None,
            DiagnosticTransition::Reconcile,
            None,
        ),
        E::HoldDeadline { .. } => (
            DiagnosticSource::HoldTimer,
            DiagnosticKeyClass::None,
            DiagnosticTransition::HoldDeadline,
            None,
        ),
        E::WindowChanged { .. } => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::WindowChanged,
            None,
        ),
        E::WindowFocusObserved(_) => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::WindowChanged,
            None,
        ),
        E::ViewContextChanged(_) => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::ConfigChanged,
            None,
        ),
        E::VoicePhaseChanged { .. } => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::VoicePhaseChanged,
            None,
        ),
        E::RecorderModeChanged(_) => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::RecorderModeChanged,
            None,
        ),
        E::ConfigChanged(_) => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::ConfigChanged,
            None,
        ),
        E::SessionReset { .. } => (
            DiagnosticSource::SessionReset,
            DiagnosticKeyClass::None,
            DiagnosticTransition::SessionReset,
            None,
        ),
        E::ManualRecovery => (
            DiagnosticSource::Control,
            DiagnosticKeyClass::None,
            DiagnosticTransition::ManualRecovery,
            None,
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_default_is_idle() {
        let s = InputStateSnapshot::default();
        assert!(s.gesture_idle);
        assert!(!s.gesture_armed);
        assert!(!s.chord_active);
        assert!(s.voice_idle);
        assert!(s.recorder_idle);
    }

    #[test]
    fn ring_buffer_push_and_take() {
        // 使用局部 ring buffer 测试
        let mut buf = Vec::with_capacity(RING_CAPACITY);
        for i in 0..RING_CAPACITY + 10 {
            let event = InputDiagnosticEvent {
                seq: i as u64,
                elapsed_ms: i as u64,
                source: DiagnosticSource::Hook,
                key: DiagnosticKeyClass::None,
                transition: DiagnosticTransition::Down,
                injected: None,
                before_level: None,
                after_level: None,
                chord_before: false,
                chord_after: false,
                ui_effect_emitted: false,
            };
            if buf.len() >= RING_CAPACITY {
                buf.remove(0);
            }
            buf.push(event);
        }
        // 只保留最后 RING_CAPACITY 条
        assert_eq!(buf.len(), RING_CAPACITY);
        assert_eq!(buf[0].seq, 10);
        assert_eq!(buf.last().unwrap().seq, (RING_CAPACITY + 9) as u64);
    }
}
