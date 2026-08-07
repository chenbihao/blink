//! Windows 平台热键：WH_KEYBOARD_LL + Raw Input + 状态机。
//!
//! 状态机（`state.rs` reducer）是物理键 / tap/hold /
//! Chord exclusive 的唯一决策者。
//!
//! **铁则**：
//! - Hook 回调（`ll_proc`）不查 DB、不调 Tauri、不 await、不取可能阻塞的锁。
//! - 原子操作和非阻塞 channel send 允许。

use std::time::Instant;

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Input::*;
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::PCWSTR;

use super::recorder;
use super::{
    ControlMsg, HookKeyEvent, InputEffect, InputEvent, InputSource, InputState, Propagation,
    WindowTransitionReason, drain_control_messages, get_config_snapshot, send_effect,
    set_latest_ui_state, state,
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

// ── Hook 线程状态 ─────────────────────────────────────────────────────────────

/// Hook 线程的 message-only window HWND（供控制消息唤醒）。
static WND_HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

// Hook 线程的输入状态机（thread-local）。
thread_local! {
    static INPUT_STATE: std::cell::RefCell<Option<InputState>> = const { std::cell::RefCell::new(None) };
    static HOLD_TIMER: std::cell::Cell<Option<usize>> = std::cell::Cell::new(None);
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

/// 从 RAWKEYBOARD 提取归一化修饰键名和 down/up。
fn raw_keyboard_to_modifier(kb: &RAWKEYBOARD) -> Option<(String, bool)> {
    let vk = kb.VKey;
    let is_down = (kb.Flags & RI_KEY_BREAK) == 0;
    let e0 = (kb.Flags & RI_KEY_E0) != 0;
    let _e1 = (kb.Flags & RI_KEY_E1) != 0;

    // 只处理修饰键（Raw Input 只校正修饰键 level）
    let key = if vk == VK_LCONTROL.0 as u16 && e0 {
        "rctrl".to_string()
    } else if vk == VK_LCONTROL.0 as u16 {
        "lctrl".to_string()
    } else if vk == VK_RCONTROL.0 as u16 {
        "rctrl".to_string()
    } else if vk == VK_LSHIFT.0 as u16 && e0 {
        "rshift".to_string()
    } else if vk == VK_LSHIFT.0 as u16 {
        "lshift".to_string()
    } else if vk == VK_RSHIFT.0 as u16 {
        "rshift".to_string()
    } else if vk == VK_LMENU.0 as u16 && e0 {
        "ralt".to_string()
    } else if vk == VK_LMENU.0 as u16 {
        "lalt".to_string()
    } else if vk == VK_RMENU.0 as u16 {
        "ralt".to_string()
    } else if vk == VK_LWIN.0 as u16 {
        "meta".to_string()
    } else if vk == VK_RWIN.0 as u16 {
        "meta".to_string()
    } else {
        return None; // 非修饰键，Raw Input 不进 reducer
    };

    Some((key, is_down))
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
    // One-shot: 立即 KillTimer 防止重复 fire
    let _ = unsafe { KillTimer(None, id_event) };
    HOLD_TIMER.set(None);

    INPUT_STATE.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };

        // 从当前 gesture 获取 gesture_id（timer 只在 Armed 时设置）
        let gesture_id = state.gesture.gesture_id();
        if let Some(gid) = gesture_id {
            let now = Instant::now();
            let result = state::reduce(state, InputEvent::HoldDeadline { gesture_id: gid }, now);

            // 处理 effects
            for effect in &result.effects {
                send_effect(effect.clone());
            }

            // 更新 UI 状态
            if let Some(ui_effect) = result
                .effects
                .iter()
                .find(|e| matches!(e, InputEffect::UiStateChanged(_)))
            {
                if let InputEffect::UiStateChanged(ui) = ui_effect {
                    set_latest_ui_state(ui);
                }
            }
        }
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
                HOLD_TIMER.set(Some(timer_id));
            }
        }
        _ => {
            // 非 Armed 状态：清理 timer（如果有）
            if let Some(tid) = HOLD_TIMER.get() {
                let _ = unsafe { KillTimer(None, tid) };
                HOLD_TIMER.set(None);
            }
        }
    }
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

        let result = state::reduce(state, InputEvent::HookKey(event), now);

        // 发送 effects
        for effect in &result.effects {
            send_effect(effect.clone());
        }

        // 更新最新 UI 状态快照
        if let Some(ui_effect) = result
            .effects
            .iter()
            .find(|e| matches!(e, InputEffect::UiStateChanged(_)))
        {
            if let InputEffect::UiStateChanged(ui) = ui_effect {
                set_latest_ui_state(ui);
            }
        }

        // 管理 hold timer
        manage_hold_timer(state);

        result.propagation
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
                        let _ = state::reduce(
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
                INPUT_STATE.with(|cell| {
                    let mut guard = cell.borrow_mut();
                    if let Some(state) = guard.as_mut() {
                        process_control_message(state, msg);
                    }
                });
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 处理单个控制消息（在 hook 线程，持有 InputState 可变借用）。
fn process_control_message(state: &mut InputState, msg: ControlMsg) {
    let now = Instant::now();
    match msg {
        ControlMsg::Config(snapshot) => {
            let _ = state::reduce(state, InputEvent::ConfigChanged(snapshot), now);
        }
        ControlMsg::WindowChanged { visible, revision } => {
            let _ = state::reduce(
                state,
                InputEvent::WindowChanged {
                    visible,
                    revision,
                    reason: WindowTransitionReason::Watchdog,
                },
                now,
            );
        }
        ControlMsg::ViewContext(ctx) => {
            let _ = state::reduce(state, InputEvent::ViewContextChanged(ctx), now);
        }
        ControlMsg::VoicePhase(phase) => {
            let _ = state::reduce(
                state,
                InputEvent::VoicePhaseChanged {
                    gesture_id: None,
                    phase,
                },
                now,
            );
        }
        ControlMsg::RecorderMode(mode) => {
            let _ = state::reduce(state, InputEvent::RecorderModeChanged(mode), now);
        }
        ControlMsg::Stop => {
            unsafe { PostQuitMessage(0) };
        }
    }
}

// ── Raw Input 处理 ───────────────────────────────────────────────────────────

fn handle_wm_input(lparam: LPARAM) {
    unsafe {
        let hrawinput = HRAWINPUT(lparam.0 as *mut _);
        let mut data = [0u8; 64];
        let mut size = 64u32;
        let result = GetRawInputData(
            hrawinput,
            RID_INPUT,
            Some(data.as_mut_ptr() as *mut _),
            &mut size,
            std::mem::size_of::<RAWINPUTHEADER>() as u32,
        );
        if result == 0 || result > 64 {
            return;
        }

        let rawinput = &*(data.as_ptr() as *const RAWINPUT);
        if rawinput.header.dwType != RIM_TYPEKEYBOARD.0 {
            return;
        }

        let kb = &rawinput.data.keyboard;
        let device_id = rawinput.header.hDevice.0 as usize;
        let time_ms = GetMessageTime() as u32;

        let Some((key, is_down)) = raw_keyboard_to_modifier(kb) else {
            return; // 非修饰键，不进 reducer
        };

        INPUT_STATE.with(|cell| {
            let mut guard = cell.borrow_mut();
            if let Some(state) = guard.as_mut() {
                let _ = state::reduce(
                    state,
                    InputEvent::RawModifier(state::RawModifierEvent {
                        device_id,
                        key,
                        is_down,
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
        let _ = state::reduce(
            &mut state,
            InputEvent::ConfigChanged(initial_config),
            Instant::now(),
        );
        INPUT_STATE.with(|cell| {
            *cell.borrow_mut() = Some(state);
        });

        // 初始化 message-only window + Raw Input
        init_window();

        // 安装 Hook
        let hhook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(?e, "SetWindowsHookExW failed for WH_KEYBOARD_LL");
                return;
            }
        };
        tracing::info!(hook_ptr = hhook.0 as usize, "WH_KEYBOARD_LL hook installed");

        // 消息循环
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        // 清理
        let _ = UnhookWindowsHookEx(hhook);
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

        // 创建 message-only window（HWND_MESSAGE = HWND(-1)）
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0x08000000), // WS_EX_NOACTIVATE
            PCWSTR(class_name.as_ptr()),
            PCWSTR::null(),
            WINDOW_STYLE(0),
            0,
            0,
            0,
            0,
            Some(windows::Win32::Foundation::HWND(-1isize as *mut _)), // HWND_MESSAGE
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
                tracing::info!("Raw Input registered (keyboard, INPUTSINK|DEVNOTIFY)");
            } else {
                tracing::warn!(?result, "RegisterRawInputDevices failed (degraded)");
            }
        } else {
            tracing::warn!("CreateWindowExW failed (degraded)");
        }
    }
}

/// 销毁 window。
fn destroy_window() {
    if let Some(&hwnd) = WND_HWND.get() {
        unsafe {
            let _ = DestroyWindow(windows::Win32::Foundation::HWND(hwnd as *mut _));
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
