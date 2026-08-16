//! Windows 平台热键：WH_KEYBOARD_LL + Raw Input + 状态机。
//!
//! 状态机（`state.rs` reducer）是物理键 / tap/hold /
//! Chord exclusive 的唯一决策者。
//!
//! **铁则**：
//! - Hook 回调（`ll_proc`）不查 DB、不调 Tauri、不 await、不取可能阻塞的锁。
//! - 原子操作和非阻塞 channel send 允许。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_INVALID_HOOK_HANDLE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::RemoteDesktop::{
    WTSRegisterSessionNotification, WTSUnRegisterSessionNotification,
};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::diagnostics::{self, HookDiagnosticInfo, InputDiagnosticEvent};
use super::recorder;
use super::{
    ControlMsg, HookKeyEvent, InputEffect, InputEvent, InputSource, InputState, ModifierKey,
    NormalizedRawModifier, Propagation, WindowTransitionReason, drain_control_messages,
    get_config_snapshot, send_effect, set_latest_ui_state, state,
};

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// HID usage page: Generic Desktop。
const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
/// HID usage: Keyboard。
const HID_USAGE_KEYBOARD: u16 = 0x06;

/// Raw keyboard flags（RAWKEYBOARD.Flags 位）。
const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;
const RI_KEY_E1: u16 = 4;

/// GIDC_REMOVAL：设备移除。
const GIDC_REMOVAL: u32 = 2;

/// 窗口类名。
const WND_CLASS: &str = "BlinkInputWindow";

/// 控制消息唤醒用的 WM_APP（与 mod.rs 控制队列配合）。
const WM_APP_WAKEUP: u32 = 0x8000;

/// 会话变化通知消息（WTSRegisterSessionNotification 注册后收到）。
const WM_WTSSESSION_CHANGE: u32 = 0x02B1;
/// 会话锁定。
const WTS_SESSION_LOCK: u32 = 0x7;
/// 会话解锁。
const WTS_SESSION_UNLOCK: u32 = 0x8;
/// 心跳定时器 ID（60 秒安全网，防止 hook 被系统静默移除）。
const TIMER_ID_HEARTBEAT: usize = 2;
/// 心跳间隔（毫秒）。
const HEARTBEAT_INTERVAL_MS: u32 = 60_000;
/// 重装定时器 ID（one-shot，用于 SessionRecovery 延迟和 retry 退避）。
const TIMER_ID_REINSTALL: usize = 3;
/// SessionRecovery 初始延迟（毫秒）。
const SESSION_RECOVERY_DELAY_MS: u32 = 250;
/// 门禁未满足时的短间隔重新检查延迟（毫秒）。
const REINSTALL_RECHECK_DELAY_MS: u32 = 200;

// ── Hook 线程状态 ─────────────────────────────────────────────────────────────

/// Hook 线程的 message-only window HWND（供控制消息唤醒）。
static WND_HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

// Hook 线程的输入状态机 + 重装状态（thread-local）。
thread_local! {
    static INPUT_STATE: std::cell::RefCell<Option<InputState>> = const { std::cell::RefCell::new(None) };
    static HOLD_TIMER: std::cell::Cell<Option<HoldTimerState>> = const { std::cell::Cell::new(None) };
    static HHOOK_SLOT: std::cell::RefCell<Option<HHOOK>> = std::cell::RefCell::new(None);
    static REINSTALL_STATE: std::cell::RefCell<ReinstallState> = std::cell::RefCell::new(ReinstallState::new());
    static WTS_REGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static RAW_REGISTERED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

#[derive(Clone, Copy)]
struct HoldTimerState {
    timer_id: usize,
    gesture_id: u64,
    not_before: Instant,
}

/// Hook 重装线程本地状态。
struct ReinstallState {
    pending_reason: Option<state::ReinstallReason>,
    attempt: u8,
    hook_available: bool,
    /// 当 Some(now) 时，retry 退避期内，不允许重装。
    next_retry_at: Option<Instant>,
}

impl ReinstallState {
    const fn new() -> Self {
        Self {
            pending_reason: None,
            attempt: 0,
            hook_available: true,
            next_retry_at: None,
        }
    }
}

// ── 辅助函数：VK 归一化 ─────────────────────────────────────────────────────

/// 将虚拟键码转换为配置中的键名。
fn vk_to_key(vk: u32) -> Option<String> {
    // 修饰键。通用码（VK_SHIFT / VK_CONTROL / VK_MENU，不分左右）当作左侧——
    // 兼容某些驱动/事件流只发通用码的情况。
    if vk == VK_LCONTROL.0 as u32 || vk == VK_CONTROL.0 as u32 {
        return Some("lctrl".to_string());
    }
    if vk == VK_RCONTROL.0 as u32 {
        return Some("rctrl".to_string());
    }
    if vk == VK_LSHIFT.0 as u32 || vk == VK_SHIFT.0 as u32 {
        return Some("lshift".to_string());
    }
    if vk == VK_RSHIFT.0 as u32 {
        return Some("rshift".to_string());
    }
    if vk == VK_LMENU.0 as u32 || vk == VK_MENU.0 as u32 {
        return Some("lalt".to_string());
    }
    if vk == VK_RMENU.0 as u32 {
        return Some("ralt".to_string());
    }
    if vk == VK_LWIN.0 as u32 || vk == VK_RWIN.0 as u32 {
        return Some("meta".to_string());
    }

    // 字母键 (A-Z)
    if (0x41..=0x5A).contains(&vk) {
        let c = char::from_u32(vk - 0x41 + b'a' as u32)?;
        return Some(c.to_string());
    }

    // 数字键 (0-9)
    if (0x30..=0x39).contains(&vk) {
        let c = char::from_u32(vk - 0x30 + b'0' as u32)?;
        return Some(c.to_string());
    }

    // 功能键 (F1-F12)
    if (0x70..=0x7B).contains(&vk) {
        let f_num = vk - 0x70 + 1;
        return Some(format!("F{}", f_num));
    }

    // 特殊键
    if vk == VK_SPACE.0 as u32 {
        return Some(" ".to_string());
    }
    if vk == VK_RETURN.0 as u32 {
        return Some("Enter".to_string());
    }
    if vk == VK_ESCAPE.0 as u32 {
        return Some("Escape".to_string());
    }
    if vk == VK_BACK.0 as u32 {
        return Some("Backspace".to_string());
    }
    if vk == VK_TAB.0 as u32 {
        return Some("Tab".to_string());
    }
    if vk == VK_DELETE.0 as u32 {
        return Some("Delete".to_string());
    }
    if vk == VK_UP.0 as u32 {
        return Some("ArrowUp".to_string());
    }
    if vk == VK_DOWN.0 as u32 {
        return Some("ArrowDown".to_string());
    }
    if vk == VK_LEFT.0 as u32 {
        return Some("ArrowLeft".to_string());
    }
    if vk == VK_RIGHT.0 as u32 {
        return Some("ArrowRight".to_string());
    }

    // 标点/符号键（OEM 键）
    if vk == 0xBA {
        return Some(";".to_string());
    } // VK_OEM_1
    if vk == 0xBB {
        return Some("=".to_string());
    } // VK_OEM_PLUS
    if vk == 0xBC {
        return Some(",".to_string());
    } // VK_OEM_COMMA
    if vk == 0xBD {
        return Some("-".to_string());
    } // VK_OEM_MINUS
    if vk == 0xBE {
        return Some(".".to_string());
    } // VK_OEM_PERIOD
    if vk == 0xBF {
        return Some("/".to_string());
    } // VK_OEM_2
    if vk == 0xC0 {
        return Some("`".to_string());
    } // VK_OEM_3
    if vk == 0xDB {
        return Some("[".to_string());
    } // VK_OEM_4
    if vk == 0xDC {
        return Some("\\".to_string());
    } // VK_OEM_5
    if vk == 0xDD {
        return Some("]".to_string());
    } // VK_OEM_6
    if vk == 0xDE {
        return Some("'".to_string());
    } // VK_OEM_7

    None
}

/// 检查是否为修饰键。
fn is_modifier_key(vk: u32) -> bool {
    vk == VK_LCONTROL.0 as u32
        || vk == VK_RCONTROL.0 as u32
        || vk == VK_CONTROL.0 as u32
        || vk == VK_LSHIFT.0 as u32
        || vk == VK_RSHIFT.0 as u32
        || vk == VK_SHIFT.0 as u32
        || vk == VK_LMENU.0 as u32
        || vk == VK_RMENU.0 as u32
        || vk == VK_MENU.0 as u32
        || vk == VK_LWIN.0 as u32
        || vk == VK_RWIN.0 as u32
}

// ── Hook 事件归一化 ───────────────────────────────────────────────────────────

/// 将 KBDLLHOOKSTRUCT + wparam 归一化为 `HookKeyEvent`。
fn normalize_hook_event(kb: &KBDLLHOOKSTRUCT, msg: u32) -> Option<HookKeyEvent> {
    let vk = kb.vkCode;
    let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !is_down && !is_up {
        return None;
    }

    let key = vk_to_key(vk)?;
    let is_modifier = is_modifier_key(vk);
    let injected = kb.flags.contains(LLKHF_INJECTED);
    let lower_integrity_injected = kb.flags.contains(LLKHF_LOWER_IL_INJECTED);
    let extended = kb.flags.contains(LLKHF_EXTENDED);
    let alt_down_flag = kb.flags.contains(LLKHF_ALTDOWN);

    Some(HookKeyEvent {
        source: if injected {
            InputSource::Injected
        } else {
            InputSource::Local
        },
        key,
        is_down,
        is_modifier,
        time_ms: kb.time,
        injected,
        lower_integrity_injected,
        extended,
        alt_down_flag,
    })
}

// ── Raw Input 归一化 ──────────────────────────────────────────────────────────

/// 从 RAWKEYBOARD 提取归一化修饰键。
///
/// 综合 VKey、MakeCode、RI_KEY_E0/E1 进行左右侧判定。
/// 通用 VK_MENU/VK_CONTROL/VK_SHIFT（不分左右）通过 E0 标志区分：
/// - E0=0 → 左侧
/// - E0=1 → 右侧
///
/// 返回 `NormalizedRawModifier`，其中原始字段（vkey/make_code/e0/e1）
/// 仅供诊断使用，不进入业务匹配。
fn raw_keyboard_to_modifier(kb: &RAWKEYBOARD, device_id: usize) -> Option<NormalizedRawModifier> {
    let vk = kb.VKey;
    let is_down = (kb.Flags & RI_KEY_BREAK) == 0;
    let e0 = (kb.Flags & RI_KEY_E0) != 0;
    let e1 = (kb.Flags & RI_KEY_E1) != 0;
    let make_code = kb.MakeCode;

    // 修饰键归一化：
    // 1. 明确的左右侧 VK → 直接映射
    // 2. 通用 VK_MENU/VK_CONTROL/VK_SHIFT → 用 E0 区分左右
    // 3. E1 标志（Pause/Break 键）不是修饰键 → None
    let key = if e1 {
        // E1 仅用于 Pause 键，不是修饰键
        return None;
    } else if vk == VK_LCONTROL.0 as u16 && e0 {
        ModifierKey::RCtrl
    } else if vk == VK_LCONTROL.0 as u16 {
        ModifierKey::LCtrl
    } else if vk == VK_RCONTROL.0 as u16 {
        ModifierKey::RCtrl
    } else if vk == VK_CONTROL.0 as u16 && e0 {
        ModifierKey::RCtrl
    } else if vk == VK_CONTROL.0 as u16 {
        ModifierKey::LCtrl
    } else if vk == VK_LSHIFT.0 as u16 && e0 {
        ModifierKey::RShift
    } else if vk == VK_LSHIFT.0 as u16 {
        ModifierKey::LShift
    } else if vk == VK_RSHIFT.0 as u16 {
        ModifierKey::RShift
    } else if vk == VK_SHIFT.0 as u16 && e0 {
        ModifierKey::RShift
    } else if vk == VK_SHIFT.0 as u16 {
        ModifierKey::LShift
    } else if vk == VK_LMENU.0 as u16 && e0 {
        ModifierKey::RAlt
    } else if vk == VK_LMENU.0 as u16 {
        ModifierKey::LAlt
    } else if vk == VK_RMENU.0 as u16 {
        ModifierKey::RAlt
    } else if vk == VK_MENU.0 as u16 && e0 {
        ModifierKey::RAlt
    } else if vk == VK_MENU.0 as u16 {
        ModifierKey::LAlt
    } else if vk == VK_LWIN.0 as u16 {
        ModifierKey::LMeta
    } else if vk == VK_RWIN.0 as u16 {
        ModifierKey::RMeta
    } else {
        return None; // 非修饰键，Raw Input 不进 reducer
    };

    Some(NormalizedRawModifier {
        key,
        is_down,
        vkey: vk,
        make_code,
        e0,
        e1,
        device_id,
    })
}

// ── 物理修饰键快照（GetAsyncKeyState）─────────────────────────────────────────

/// 读取 Windows 物理修饰键快照。
///
/// 使用 `GetAsyncKeyState()` 高位判断物理按下状态。
/// 该函数不阻塞，可在 Hook 回调或主线程中安全调用。
pub(crate) fn read_physical_modifier_snapshot() -> state::PhysicalModifierSnapshot {
    unsafe {
        let lalt = GetAsyncKeyState(VK_LMENU.0 as i32) < 0;
        let ralt = GetAsyncKeyState(VK_RMENU.0 as i32) < 0;
        let lctrl = GetAsyncKeyState(VK_LCONTROL.0 as i32) < 0;
        let rctrl = GetAsyncKeyState(VK_RCONTROL.0 as i32) < 0;
        let lshift = GetAsyncKeyState(VK_LSHIFT.0 as i32) < 0;
        let rshift = GetAsyncKeyState(VK_RSHIFT.0 as i32) < 0;
        let lmeta = GetAsyncKeyState(VK_LWIN.0 as i32) < 0;
        let rmeta = GetAsyncKeyState(VK_RWIN.0 as i32) < 0;
        state::PhysicalModifierSnapshot {
            lalt,
            ralt,
            lctrl,
            rctrl,
            lshift,
            rshift,
            lmeta,
            rmeta,
        }
    }
}

/// 判断是否需要主键触发前 reconciliation。
///
/// 只在以下按键的 keydown 时校验：
/// - 当前主热键的主键（如 Space）
/// - 任意非修饰键 keydown（覆盖 chord 键）
///
/// 修饰键 keydown/keyup 不需要校验——它们本身就是 modifier level 的来源。
fn needs_physical_reconciliation(event: &HookKeyEvent) -> bool {
    event.is_down && !event.is_modifier
}

// ── Hold Timer ─────────────────────────────────────────────────────────────────

/// Hold timer 回调：超过 tap 阈值时被消息循环 dispatch。
/// 喂入 `HoldDeadline` 事件到 reducer。
unsafe extern "system" fn hold_timer_callback(
    _hwnd_wnd: HWND,
    _msg: u32,
    id_event: usize,
    _time: u32,
) {
    let Some(timer) = HOLD_TIMER.get() else {
        let _ = unsafe { KillTimer(None, id_event) };
        return;
    };

    // KillTimer 不能撤回已排队的旧 callback。只有当前 timer id 匹配且已经到达
    // 新 gesture 的最早 deadline，才允许消费；否则不能清掉新 timer。
    if timer.timer_id != id_event {
        let _ = unsafe { KillTimer(None, id_event) };
        return;
    }
    if Instant::now() < timer.not_before {
        return;
    }

    let _ = unsafe { KillTimer(None, id_event) };
    HOLD_TIMER.set(None);

    INPUT_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };

        reduce_and_apply(
            state,
            InputEvent::HoldDeadline {
                gesture_id: timer.gesture_id,
            },
            Instant::now(),
        );
    });
}

// ── 录制事件喂入（平台特定）──────────────────────────────────────────────────

/// 录制期间把原始 VK 事件归一化为语义事件喂给 recorder.rs。
fn feed_recorder(vk: u32, wparam: WPARAM) {
    let msg = wparam.0 as u32;
    let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
    let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;
    if !is_down && !is_up {
        return;
    }

    if is_modifier_key(vk) {
        let Some(name) = vk_to_key(vk) else { return };
        if is_down {
            // AltGr 去模拟：右 Alt（VK_RMENU）按下会附带一个模拟的左 Ctrl，
            // 清掉它，避免状态机把右 Alt 误录成 LeftCtrl。
            if vk == VK_RMENU.0 as u32 {
                recorder::drop_modifier("lctrl");
            }
            recorder::feed(recorder::RecordInput::ModifierDown(name));
        } else {
            recorder::feed(recorder::RecordInput::ModifierUp(name));
        }
    } else if is_down {
        // 非修饰键:按下即完成录制;松开不关心。
        let Some(name) = vk_to_key(vk) else { return };
        recorder::feed(recorder::RecordInput::KeyDown(name));
    }
}

// ── Hold Timer 管理 ─────────────────────────────────────────────────────────────

/// 处理 reduce 结果后的 timer 管理（设置/清理 hold timer）。
fn manage_hold_timer(state: &InputState) {
    match state.gesture {
        state::GestureState::Armed {
            gesture_id,
            aborted,
            hold_fired,
            frozen_tap_threshold,
            ..
        } => {
            // Armed 且未 aborted 且未 hold_fired：需要 timer
            if !aborted && !hold_fired && HOLD_TIMER.get().is_none() {
                let timer_id = unsafe {
                    SetTimer(
                        None,
                        1, // ID (unused, one-shot)
                        frozen_tap_threshold.as_millis() as u32,
                        Some(hold_timer_callback),
                    )
                };
                HOLD_TIMER.set(Some(HoldTimerState {
                    timer_id,
                    gesture_id,
                    not_before: Instant::now() + frozen_tap_threshold,
                }));
            }
        }
        _ => {
            // 非 Armed 状态：清理 timer（如果有）
            if let Some(timer) = HOLD_TIMER.get() {
                let _ = unsafe { KillTimer(None, timer.timer_id) };
                HOLD_TIMER.set(None);
            }
        }
    }
}

// ── Reduce result adapter（统一处理 effects / UI / timer）────────────────────

/// 处理 reduce 结果：发送 effects、更新 UI 快照、管理 hold timer、更新诊断快照。
///
/// 无锁、无 IO——可在 Hook 回调中安全调用。
fn apply_reduce_result(state: &InputState, result: &state::ReduceResult) {
    for effect in &result.effects {
        send_effect(effect.clone());
    }
    if let Some(InputEffect::UiStateChanged(ui)) = result
        .effects
        .iter()
        .find(|e| matches!(e, InputEffect::UiStateChanged(_)))
    {
        set_latest_ui_state(ui);
    }
    manage_hold_timer(state);
    // 更新诊断快照（try_lock 不阻塞）
    diagnostics::update_state_snapshot(state);
}

/// 统一执行 reducer、发布 effect，并记录不含用户文本的诊断元数据。
fn reduce_and_apply(state: &mut InputState, event: InputEvent, now: Instant) -> Propagation {
    let chord_before = state.chord.is_active();
    let (source, key_class, transition, injected) = diagnostics::extract_event_meta(&event);
    // 普通全局打字不进入诊断环；只保留修饰键、存在 modifier/gesture/chord 上下文的
    // Hook 事件，以及 Raw/Physical/Control 等低频状态事件。
    let should_record = match &event {
        InputEvent::HookKey(hook) => {
            hook.is_modifier
                || state.modifiers.pressed_mask() != 0
                || !matches!(state.gesture, state::GestureState::Idle)
                || chord_before
        }
        _ => true,
    };
    let before_level = match key_class {
        diagnostics::DiagnosticKeyClass::Modifier(key) => Some(state.modifiers.level(key)),
        _ => None,
    };
    let result = state::reduce(state, event, now);
    let after_level = match key_class {
        diagnostics::DiagnosticKeyClass::Modifier(key) => Some(state.modifiers.level(key)),
        _ => None,
    };
    if should_record {
        diagnostics::push_diagnostic_event(InputDiagnosticEvent {
            seq: diagnostics::next_seq(),
            elapsed_ms: diagnostics::elapsed_ms(),
            source,
            key: key_class,
            transition,
            injected,
            before_level,
            after_level,
            chord_before,
            chord_after: state.chord.is_active(),
            ui_effect_emitted: result
                .effects
                .iter()
                .any(|e| matches!(e, InputEffect::UiStateChanged(_))),
        });
    }
    apply_reduce_result(state, &result);
    result.propagation
}

// ── SessionReset 适配 ─────────────────────────────────────────────────────────

/// 在 Hook 线程触发 SessionReset（锁屏/解锁时调用）。
fn apply_session_reset(reason: state::SessionResetReason) {
    INPUT_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        if let Some(state) = guard.as_mut() {
            reduce_and_apply(state, InputEvent::SessionReset { reason }, Instant::now());
        }
    });
    // 借用释放后检查重装
    try_reinstall_if_safe();
}

// ── 请求式 Hook 重装 ─────────────────────────────────────────────────────────

/// 尝试重装 Hook（如果条件安全）。
///
/// 检查 pending 请求、退避期和 idle 门禁。三者都满足时执行卸载→安装。
fn try_reinstall_if_safe() {
    // 1. 检查 pending 和退避期
    let reason = REINSTALL_STATE.with(|s| {
        let s = s.borrow();
        if s.pending_reason.is_none() {
            return None;
        }
        if let Some(t) = s.next_retry_at {
            if Instant::now() < t {
                return None; // 退避期内，等待 retry timer
            }
        }
        s.pending_reason
    });

    let Some(reason) = reason else {
        return;
    };

    // 2. 检查 idle 门禁（读取物理快照，不依赖可能故障的 modifier 缓存）
    let idle_ok = INPUT_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return false;
        };
        let physical = read_physical_modifier_snapshot();

        // 手动恢复必须先解除 stale modifier/gesture/chord 对门禁的自锁。
        // 物理键未全松开或业务录音态活跃时保持 pending，等待下一次定时检查。
        if reason == state::ReinstallReason::ManualRecovery
            && physical.all_up()
            && matches!(state.voice, state::VoicePhase::Idle)
            && matches!(state.recorder, state::RecorderMode::Idle)
        {
            reduce_and_apply(state, InputEvent::ManualRecovery, Instant::now());
        }

        state::can_reinstall(reason, state, &physical)
    });

    if !idle_ok {
        // 门禁未满足，安排短间隔重新检查
        schedule_reinstall_timer(REINSTALL_RECHECK_DELAY_MS);
        return;
    }

    // 3. 执行重装
    let success = do_reinstall(reason);

    REINSTALL_STATE.with(|s| {
        let mut s = s.borrow_mut();
        if success {
            s.pending_reason = None;
            s.attempt = 0;
            s.next_retry_at = None;
            s.hook_available = true;
        } else {
            s.attempt += 1;
            s.hook_available = false;
            let delay = state::retry_delay_ms(s.attempt);
            s.next_retry_at = Some(Instant::now() + Duration::from_millis(delay as u64));
            schedule_reinstall_timer(delay);
        }
        // 更新诊断信息
        update_hook_diagnostics(&s);
    });
}

/// 执行卸载→安装。返回 true 表示成功。
fn do_reinstall(reason: state::ReinstallReason) -> bool {
    HHOOK_SLOT.with(|slot| {
        let mut slot = slot.borrow_mut();

        // 卸载现有 Hook
        if let Some(hhook) = *slot {
            match unsafe { UnhookWindowsHookEx(hhook) } {
                Ok(()) => {
                    *slot = None;
                }
                Err(e) => {
                    if e.code() == windows::core::HRESULT::from_win32(ERROR_INVALID_HOOK_HANDLE.0) {
                        // 系统可能已经静默移除了 Hook；此时本地句柄只是陈旧快照，
                        // 清掉后继续安装才有机会真正恢复。
                        tracing::warn!(?e, ?reason, "discarding stale hook handle");
                        *slot = None;
                    } else {
                        tracing::error!(?e, ?reason, "UnhookWindowsHookEx failed");
                        // 其他错误下不能确认旧 Hook 是否仍有效，不叠装第二个 Hook。
                        return false;
                    }
                }
            }
        }

        // 安装新 Hook
        match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0) } {
            Ok(new_hook) => {
                *slot = Some(new_hook);
                tracing::info!(
                    ?reason,
                    hook_ptr = new_hook.0 as usize,
                    "WH_KEYBOARD_LL hook re-installed successfully"
                );
                true
            }
            Err(e) => {
                tracing::error!(?e, ?reason, "SetWindowsHookExW failed");
                false
            }
        }
    })
}

/// 安排 one-shot 重装定时器。
fn schedule_reinstall_timer(delay_ms: u32) {
    if let Some(&hwnd_raw) = WND_HWND.get() {
        let hwnd = HWND(hwnd_raw as *mut _);
        let _ = unsafe { SetTimer(Some(hwnd), TIMER_ID_REINSTALL, delay_ms, None) };
    }
}

/// 从线程本地状态读取 Hook 诊断信息并更新共享快照。
fn update_hook_diagnostics(reinstall: &ReinstallState) {
    let hook_installed = HHOOK_SLOT.with(|slot| slot.borrow().is_some());
    let wts_registered = WTS_REGISTERED.with(|cell| cell.get());
    let raw_registered = RAW_REGISTERED.with(|cell| cell.get());
    let info = HookDiagnosticInfo {
        hook_installed,
        hook_available: reinstall.hook_available,
        pending_reinstall: reinstall.pending_reason,
        reinstall_attempt: reinstall.attempt,
        wts_registered,
        raw_registered,
    };
    diagnostics::update_hook_info(&info);
}

// ── LL Hook 回调 ───────────────────────────────────────────────────────────────

unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const HC_ACTION: i32 = 0;
    if code != HC_ACTION {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let kb = unsafe { &*(lparam.0 as usize as *const KBDLLHOOKSTRUCT) };
    let vk = kb.vkCode;
    let msg = wparam.0 as u32;

    // 录制短路（优先于所有其他逻辑）
    if recorder::is_recording() {
        feed_recorder(vk, wparam);
        // 录制期间吞掉 Alt+Space（WebView2 无法拦截系统菜单）
        if vk == VK_SPACE.0 as u32 && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0 {
            return LRESULT(1);
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    // 归一化 Hook 事件
    let Some(event) = normalize_hook_event(kb, msg) else {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    };

    // 喂入 reducer
    let now = Instant::now();
    let propagation = INPUT_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            // 状态未初始化（启动时短暂窗口）：放行
            return Propagation::Pass;
        };

        // 主键触发前强制 reconciliation：
        // 非修饰键 keydown 前，读取物理修饰键快照，校正内部状态。
        // 这样当内部 LAlt=Down 但物理 LAlt=Up 时，先修正为 Up，Space 不会被误判为 Alt+Space。
        if needs_physical_reconciliation(&event) {
            let snapshot = read_physical_modifier_snapshot();
            reduce_and_apply(
                state,
                InputEvent::PhysicalModifiersObserved {
                    snapshot,
                    reason: state::PhysicalObservationReason::MainKeyBoundary,
                },
                now,
            );
        }

        reduce_and_apply(state, InputEvent::HookKey(event), now)
    });

    if matches!(propagation, Propagation::Swallow) {
        LRESULT(1)
    } else {
        unsafe { CallNextHookEx(None, code, wparam, lparam) }
    }
}

// ── Message-Only Window Proc ────────────────────────────────────────────────────

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_INPUT => {
            handle_wm_input(lparam);
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_INPUT_DEVICE_CHANGE => {
            // wparam: GIDC_ARRIVAL(1) 或 GIDC_REMOVAL(2)
            if wparam.0 as u32 == GIDC_REMOVAL {
                let device_id = lparam.0 as usize;
                INPUT_STATE.with(|cell| {
                    let mut guard = cell.borrow_mut();
                    if let Some(state) = guard.as_mut() {
                        reduce_and_apply(
                            state,
                            InputEvent::RawDeviceRemoved { device_id },
                            Instant::now(),
                        );
                    }
                });
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_APP_WAKEUP => {
            // 排空控制消息队列并处理
            for msg in drain_control_messages() {
                match msg {
                    ControlMsg::ManualRecovery => {
                        // 手动恢复不经过 reducer，直接设置 pending reason
                        tracing::info!("ManualRecovery requested by user");
                        REINSTALL_STATE.with(|s| {
                            let mut s = s.borrow_mut();
                            s.pending_reason = Some(match s.pending_reason {
                                Some(r) => r.merge(state::ReinstallReason::ManualRecovery),
                                None => state::ReinstallReason::ManualRecovery,
                            });
                            // 手动恢复清除退避计时器——用户显式请求应立即尝试
                            s.next_retry_at = None;
                        });
                        try_reinstall_if_safe();
                    }
                    other => {
                        INPUT_STATE.with(|cell| {
                            let mut guard = cell.borrow_mut();
                            if let Some(state) = guard.as_mut() {
                                process_control_message(state, other);
                            }
                        });
                    }
                }
            }
            LRESULT(0)
        }
        WM_TIMER => {
            let id = wparam.0 as usize;
            match id {
                TIMER_ID_HEARTBEAT => {
                    tracing::trace!("Heartbeat timer fired");
                    REINSTALL_STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        s.pending_reason = Some(match s.pending_reason {
                            Some(r) => r.merge(state::ReinstallReason::Heartbeat),
                            None => state::ReinstallReason::Heartbeat,
                        });
                    });
                    try_reinstall_if_safe();
                }
                TIMER_ID_REINSTALL => {
                    // One-shot：立即 KillTimer
                    if let Some(&hwnd_raw) = WND_HWND.get() {
                        let hwnd = HWND(hwnd_raw as *mut _);
                        let _ = unsafe { KillTimer(Some(hwnd), TIMER_ID_REINSTALL) };
                    }
                    // 清除退避标记
                    REINSTALL_STATE.with(|s| {
                        s.borrow_mut().next_retry_at = None;
                    });
                    try_reinstall_if_safe();
                }
                _ => {}
            }
            LRESULT(0)
        }
        WM_WTSSESSION_CHANGE => {
            // 会话变化通知（WTSRegisterSessionNotification 注册后收到）
            match wparam.0 as u32 {
                WTS_SESSION_LOCK => {
                    tracing::info!("Session locked — resetting input state");
                    apply_session_reset(state::SessionResetReason::Lock);
                }
                WTS_SESSION_UNLOCK => {
                    tracing::info!(
                        "Session unlocked — resetting input state and requesting hook recovery"
                    );
                    apply_session_reset(state::SessionResetReason::Unlock);
                    // 合并 SessionRecovery 请求
                    REINSTALL_STATE.with(|s| {
                        let mut s = s.borrow_mut();
                        s.pending_reason = Some(match s.pending_reason {
                            Some(r) => r.merge(state::ReinstallReason::SessionRecovery),
                            None => state::ReinstallReason::SessionRecovery,
                        });
                        s.next_retry_at = Some(
                            Instant::now()
                                + Duration::from_millis(SESSION_RECOVERY_DELAY_MS as u64),
                        );
                    });
                    schedule_reinstall_timer(SESSION_RECOVERY_DELAY_MS);
                }
                _ => {}
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 处理单个控制消息（在 hook 线程，持有 InputState 可变借用）。
///
/// 与 `ll_proc` / `hold_timer_callback` 一致，reduce 产出的 effects 通过 `send_effect`
/// 转发给主线程，`UiStateChanged` 同时更新原子快照。此前此处用 `let _ =` 丢弃了
/// `ReduceResult`，导致 `VoicePhaseChanged` 等控制消息引发的 `UiStateChanged` 事件
/// 既不 emit 到前端也不更新快照——`exclusive_chord_active` 状态对前端不可见。
fn process_control_message(state: &mut InputState, msg: ControlMsg) {
    let now = Instant::now();
    let event = match msg {
        ControlMsg::Config(snapshot) => InputEvent::ConfigChanged(snapshot),
        ControlMsg::WindowChanged { visible, revision } => InputEvent::WindowChanged {
            visible,
            revision,
            reason: WindowTransitionReason::Watchdog,
        },
        ControlMsg::ViewContext(ctx) => InputEvent::ViewContextChanged(ctx),
        ControlMsg::VoicePhase(phase) => InputEvent::VoicePhaseChanged {
            gesture_id: None,
            phase,
        },
        ControlMsg::RecorderMode(mode) => InputEvent::RecorderModeChanged(mode),
        ControlMsg::ManualRecovery => {
            // ManualRecovery 在 WM_APP_WAKEUP 中已单独处理，不应到达此处。
            // 如果到达，说明逻辑有误——记录日志但不 crash。
            tracing::warn!("ManualRecovery unexpectedly reached process_control_message");
            return;
        }
        ControlMsg::Stop => {
            unsafe { PostQuitMessage(0) };
            return;
        }
    };

    reduce_and_apply(state, event, now);
}

// ── Raw Input 处理 ───────────────────────────────────────────────────────────

fn handle_wm_input(lparam: LPARAM) {
    unsafe {
        let hrawinput = HRAWINPUT(lparam.0 as *mut _);
        // 用 MaybeUninit<RAWINPUT> 保证结构体对齐（不可用 [u8; N]，会 misaligned panic）。
        let mut data = std::mem::MaybeUninit::<RAWINPUT>::uninit();
        let mut size = std::mem::size_of::<RAWINPUT>() as u32;
        let result = GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some(data.as_mut_ptr() as *mut _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if result == 0 {
            return;
        }

        let rawinput = data.assume_init_ref();
        if rawinput.header.dwType != RIM_TYPEKEYBOARD.0 {
            return;
        }

        let kb = &rawinput.data.keyboard;
        let device_id = rawinput.header.hDevice.0 as usize;
        let time_ms = GetMessageTime() as u32;

        let Some(normalized) = raw_keyboard_to_modifier(kb, device_id) else {
            return; // 非修饰键，不进 reducer
        };

        INPUT_STATE.with(|cell| {
            let mut guard = cell.borrow_mut();
            if let Some(state) = guard.as_mut() {
                reduce_and_apply(
                    state,
                    InputEvent::RawModifier(state::RawModifierEvent {
                        device_id: normalized.device_id,
                        key: normalized.key,
                        is_down: normalized.is_down,
                        time_ms,
                    }),
                    Instant::now(),
                );
            }
        });
    }
}

// ── 启动入口 ─────────────────────────────────────────────────────────────────

/// 启动 Windows 钩子线程。
pub fn start_hook_thread() {
    std::thread::Builder::new()
        .name("blink-hotkey".into())
        .spawn(hook_thread_main)
        .expect("failed to spawn hotkey thread");
}

/// Hook 线程主函数：初始化 → 消息循环 → 清理。
fn hook_thread_main() {
    unsafe {
        // 初始化状态机
        let mut state = InputState::default();
        let initial_config = get_config_snapshot();
        let result = state::reduce(
            &mut state,
            InputEvent::ConfigChanged(initial_config),
            Instant::now(),
        );
        // 初始化阶段也需 apply_reduce_result：确保初始 UI 快照发布到原子变量，
        // 使 register_main_input_view 能读到正确 revision。
        apply_reduce_result(&state, &result);
        INPUT_STATE.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });

        // 初始化 message-only window + Raw Input
        init_window();

        // 安装 Hook。首次安装失败时保留消息泵，由同一退避机制持续恢复。
        match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0) {
            Ok(hhook) => {
                HHOOK_SLOT.with(|slot| {
                    *slot.borrow_mut() = Some(hhook);
                });
                tracing::info!(hook_ptr = hhook.0 as usize, "WH_KEYBOARD_LL hook installed");
            }
            Err(e) => {
                tracing::error!(?e, "SetWindowsHookExW failed for WH_KEYBOARD_LL");
                REINSTALL_STATE.with(|s| {
                    let mut s = s.borrow_mut();
                    s.pending_reason = Some(state::ReinstallReason::SessionRecovery);
                    s.attempt = 1;
                    s.hook_available = false;
                    let delay = state::retry_delay_ms(s.attempt);
                    s.next_retry_at = Some(Instant::now() + Duration::from_millis(delay as u64));
                    schedule_reinstall_timer(delay);
                });
            }
        }

        // 初始化 hook 诊断快照
        REINSTALL_STATE.with(|s| {
            update_hook_diagnostics(&s.borrow());
        });

        // 心跳定时器：60 秒安全网，防止 hook 被系统静默移除
        // （锁屏/超时/休眠等场景）。UnhookWindowsHookEx + SetWindowsHookExW 开销极小。
        if let Some(&hwnd_raw) = WND_HWND.get() {
            let hwnd = HWND(hwnd_raw as *mut _);
            let _ = SetTimer(Some(hwnd), TIMER_ID_HEARTBEAT, HEARTBEAT_INTERVAL_MS, None);
            tracing::debug!("Heartbeat timer started ({}ms)", HEARTBEAT_INTERVAL_MS);
        }

        // 消息循环
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
            // 每条消息处理后检查是否可以安全重装 Hook
            try_reinstall_if_safe();
        }

        // 清理
        if let Some(&hwnd_raw) = WND_HWND.get() {
            let hwnd = HWND(hwnd_raw as *mut _);
            let _ = KillTimer(Some(hwnd), TIMER_ID_HEARTBEAT);
            let _ = KillTimer(Some(hwnd), TIMER_ID_REINSTALL);
        }
        HHOOK_SLOT.with(|slot| {
            if let Some(hhook) = slot.borrow_mut().take() {
                let _ = UnhookWindowsHookEx(hhook);
            }
        });
        tracing::info!("WH_KEYBOARD_LL hook uninstalled");
        destroy_window();
    }
}

/// 初始化 message-only window + Raw Input。
fn init_window() {
    unsafe {
        // 注册窗口类
        let class_name: Vec<u16> = WND_CLASS.encode_utf16().chain(std::iter::once(0)).collect();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(wnd_proc),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            ..Default::default()
        };
        let _ = RegisterClassExW(&wc);

        // 创建不可见窗口（等同 message-only window）。
        // 不用 HWND_MESSAGE（HWND(-1)）--windows 0.62 下该构造产生
        // ERROR_INVALID_WINDOW_HANDLE(0x80070578)。改用 None parent + 无 WS_VISIBLE，
        // 与 clipboard 监听窗口同模式（已验证可行），窗口不可见即可作消息泵。
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        );

        if let Ok(hwnd) = hwnd {
            let _ = WND_HWND.set(hwnd.0 as isize);

            // 注册 Raw Input：keyboard, RIDEV_INPUTSINK | RIDEV_DEVNOTIFY
            let rid = RAWINPUTDEVICE {
                usUsagePage: HID_USAGE_PAGE_GENERIC,
                usUsage: HID_USAGE_KEYBOARD,
                dwFlags: RIDEV_INPUTSINK | RIDEV_DEVNOTIFY,
                hwndTarget: hwnd,
            };
            let result =
                RegisterRawInputDevices(&[rid], std::mem::size_of::<RAWINPUTDEVICE>() as u32);
            if result.is_ok() {
                RAW_REGISTERED.with(|cell| cell.set(true));
                tracing::info!("Raw Input registered (keyboard, INPUTSINK|DEVNOTIFY)");
            } else {
                RAW_REGISTERED.with(|cell| cell.set(false));
                tracing::warn!(?result, "RegisterRawInputDevices failed (degraded)");
            }

            // 注册会话变化通知（锁屏/解锁后重装 hook）
            // NOTIFY_FOR_THIS_SESSION = 0：只接收当前会话的通知
            if WTSRegisterSessionNotification(hwnd, 0).is_ok() {
                tracing::info!("WTS session notification registered");
                WTS_REGISTERED.with(|cell| cell.set(true));
            } else {
                tracing::warn!("WTSRegisterSessionNotification failed (degraded)");
                WTS_REGISTERED.with(|cell| cell.set(false));
            }
        } else {
            RAW_REGISTERED.with(|cell| cell.set(false));
            tracing::warn!(error = ?hwnd, "CreateWindowExW failed (degraded)");
        }
    }
}

/// 销毁 window。
fn destroy_window() {
    if let Some(&hwnd) = WND_HWND.get() {
        unsafe {
            let hwnd = HWND(hwnd as *mut _);
            // 注销 WTS 会话通知
            if WTS_REGISTERED.with(|cell| cell.get()) {
                let _ = WTSUnRegisterSessionNotification(hwnd);
                tracing::debug!("WTS session notification unregistered");
            }
            let _ = DestroyWindow(hwnd);
        }
    }
}

/// 唤醒 Hook 线程处理控制消息（由 mod.rs::send_control() 调用）。
pub fn post_control_wakeup() {
    if let Some(&hwnd) = WND_HWND.get() {
        let _ = unsafe {
            PostMessageW(
                Some(windows::Win32::Foundation::HWND(hwnd as *mut _)),
                WM_APP_WAKEUP,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mask_for_config_modifier_basic() {
        // 只测试 vk_to_key / is_modifier_key 逻辑
        assert_eq!(vk_to_key(0x41), Some("a".to_string()));
        assert_eq!(vk_to_key(0x30), Some("0".to_string()));
        assert!(is_modifier_key(VK_LCONTROL.0 as u32));
        assert!(is_modifier_key(VK_LMENU.0 as u32));
        assert!(!is_modifier_key(0x41));
    }
}
