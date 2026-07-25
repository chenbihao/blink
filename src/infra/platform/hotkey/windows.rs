//! Windows 平台特定的热键实现：WH_KEYBOARD_LL 低级键盘钩子。
//!
//! 触发判定（tap/hold 状态机）设计：
//! - **不维护按键累积镜像**（组合键 arm/keyup 边界场景）。Windows 已维护权威的键物理态
//!   (GetAsyncKeyState),应用层再累积一份(靠 down/up 事件 push/remove)会被系统注入的
//!   合成事件(AltGr 假 Ctrl、Alt+Space 额外 Alt、IDEA 瞬时 Alt down/up、WebView2 吞 Alt up)
//!   打乱且无法自愈。
//! - 改为:只在**主键 down/up 边界**现查修饰键物理态。状态机仅 3 个字段,不依赖任何
//!   需要 down/up 配对的累积量。
//! - 主键 down 且修饰键满足 → armed;armed 后任何异键 down → aborted(判 hold);
//!   主键 up 时若未 aborted、时长达标 → 触发 Tap(修饰键只在 arm 时现查,keyup 不复查,
//!   避免快速松手时修饰键略早释放导致漏触发)。
//!
//! **例外：跨秒级 hold 状态需要累积**（`ALT_LOGICALLY_HELD`）。
//! - `SetForegroundWindow` 合成的 Alt keyup 会**持续**污染 `GetAsyncKeyState`——用户手指
//!   还按着，内核里已经记为松开，直到用户真松开+再按才恢复。这类污染时长跨越秒级，
//!   arm 边界瞬时判定的"漏一次"容忍不了。
//! - LL hook 是唯一能看到 `LLKHF_INJECTED` flag 的层，flag=1 的合成事件直接不参与逻辑态。
//!   `is_alt_down()` / chord 独占吞键 / 语音录音吞 Alt+Space 都读 `ALT_LOGICALLY_HELD`，
//!   免疫合成事件。
//! - 铁则仍在——只是明确划分场景：**边界现查、跨时长累积**。

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::WindowsAndMessaging::*;

use super::{HotkeyEvent, get_current_config, get_tap_threshold, send_event};
use super::{is_chord_key, is_chord_mode};

/// 全局标志：语音录音中。hotkey hook 读它判断 ESC 是否应触发取消。
/// VoiceService 在 start/cancel/stop 时写它。
static VOICE_RECORDING: AtomicBool = AtomicBool::new(false);

/// 设置语音录音标志（供 VoiceService 调用）。
pub fn set_voice_recording(active: bool) {
    VOICE_RECORDING.store(active, Ordering::SeqCst);
}

/// Alt 键的**逻辑** hold 态：由所有 LMENU/RMENU keydown/keyup 驱动（含远程控制软件
/// 注入的事件），仅用 [`EXPECT_SYNTH_KEYUP_AT`] one-shot flag 跳过我们自己
/// `SetForegroundWindow` 合成的 keyup。
///
/// 为什么不是直接读 `GetAsyncKeyState(VK_MENU)`：Windows 的 `SetForegroundWindow`（及某些
/// 焦点转移场景）会**合成一个 Alt keyup 事件**用于释放系统菜单栏激活态，此事件会持续
/// 污染 `GetAsyncKeyState` 的物理态读数——用户手指还按着，内核里已经记为松开，直到
/// 用户真的松开+再按才能恢复。前端 alt-poll 靠 `GetAsyncKeyState` 时，冷启动首唤按住
/// Alt 呼窗后 chord 会短暂进入又立刻退出（0.11.10 定位）。
///
/// **设计演进**：
/// - 0.11.10：用 `LLKHF_INJECTED` flag 过滤合成事件——只跟踪非注入的 Alt 事件。
/// - 0.11.11：移除 `LLKHF_INJECTED` 过滤。该 flag 无法区分"远程控制软件（RustDesk/
///   TeamViewer）通过 SendInput 注入的真实按键"与"`SetForegroundWindow` 合成的 Alt
///   keyup"——`SM_REMOTESESSION` 只检测 Windows 原生 RDP，对第三方工具返回 false，
///   导致远程控制下 Alt 事件被全量过滤。改为：接受所有 Alt 事件，仅用 one-shot
///   flag 过滤我们自己 `SetForegroundWindow` 合成的 keyup（这是原过滤的真正目标）。
///
/// **例外声明（与顶部"不做累积镜像"铁则的关系）**：
/// - 铁则针对**组合键 arm/keyup 边界一次性判定**——那种场景瞬时读物理态最稳，累积镜像会被
///   合成事件搅乱且无法自愈。`modifiers_satisfied` / `current_modifier_mask` 仍走物理态。
/// - 本字段针对**跨秒级 hold 状态**（chord 独占期 / 语音录音期 / 前端 alt-poll）——这种
///   场景物理态被合成事件毒化的时长远长于铁则关心的边界瞬间，反而只有 one-shot flag
///   过滤合成 keyup + 累积才能自愈。
static ALT_LOGICALLY_HELD: AtomicBool = AtomicBool::new(false);

/// 一次性 flag：我们自己调 `SetForegroundWindow` 前设此时间戳，LL hook 收到 Alt keyup
/// 时若在 200ms 窗口内则判定为合成 keyup 并跳过（不更新 `ALT_LOGICALLY_HELD`）。
///
/// **设计**：
/// - 仅在 `ALT_LOGICALLY_HELD == true` 时才设——`SetForegroundWindow` 只在 Alt
///   按下（系统菜单栏激活态）时才合成 keyup，Alt 未按下时设 flag 是无意义的，
///   反而会误跳下一个真实 keyup（如 `restore_foreground` 在语音结束后调用时
///   Alt 已松开）。
/// - 200ms 时间窗口是兜底：若 `SetForegroundWindow` 未合成 keyup（如窗口已是
///   前台），flag 会在 200ms 后自然过期，不会误跳后续真实 keyup。
/// - **本地 vs RDP**：本地场景下合成 keyup 带 `LLKHF_INJECTED`，被 `should_track`
///   过滤先行拦截，flag 不会被消费——但 keyup 时无条件 `swap(false)` 清除，
///   防止残留。RDP 场景下合成 keyup 也带 `LLKHF_INJECTED` 但 `should_track=true`，
///   flag 被消费并跳过 keyup。
static EXPECT_SYNTH_KEYUP_AT: std::sync::atomic::AtomicI64 = std::sync::atomic::AtomicI64::new(0);

// ── 修饰键物理态 bitmask ────────────────────────────────────────────────────────
// 8 个具体修饰键各占一位。用于「现查物理态 → 与配置精确匹配」,替代旧的累积镜像。
const MOD_LCTRL: u16 = 1 << 0;
const MOD_RCTRL: u16 = 1 << 1;
const MOD_LSHIFT: u16 = 1 << 2;
const MOD_RSHIFT: u16 = 1 << 3;
const MOD_LALT: u16 = 1 << 4;
const MOD_RALT: u16 = 1 << 5;
const MOD_LMETA: u16 = 1 << 6;
const MOD_RMETA: u16 = 1 << 7;

/// hook 线程私有状态(触发判定)。生命周期都限于一次主键 down→up。
struct State {
    /// 主键首次 down 时刻(tap/hold 时长判定)。
    down_since: Option<Instant>,
    /// 当前 armed 的目标主键。Some = 主键按下待判定。**非按键镜像**——只记「当前在等
    /// 哪个主键松开」,一次 down→up 即清。用 String 而非 bool 以稳健处理 autorepeat、
    /// keyup 配对与运行时配置切换。
    armed_key: Option<String>,
    /// armed 后是否出现过其他键 down(出现 → 判 hold,不触发)。
    aborted: bool,
    /// Hold timer 是否已 fire(超过 tap 阈值)。
    /// false = 还在 tap 窗口内;true = Hold 事件已发出,keyup 时发 HoldRelease。
    hold_fired: bool,
    /// SetTimer 返回的 timer ID(用于 KillTimer)。None = 无活动 timer。
    hold_timer_id: Option<usize>,
}

thread_local! {
    static STATE: std::cell::RefCell<State> = std::cell::RefCell::new(State {
        down_since: None,
        armed_key: None,
        aborted: false,
        hold_fired: false,
        hold_timer_id: None,
    });
}

/// Hold timer 回调:超过 tap 阈值时被 hook 线程消息循环 dispatch。
/// 设 hold_fired=true 并发送 Hold 事件(语音录音开始)。
unsafe extern "system" fn hold_timer_callback(
    _hwnd: windows::Win32::Foundation::HWND,
    _msg: u32,
    id_event: usize,
    _time: u32,
) {
    // One-shot: 立即 KillTimer 防止重复 fire
    let _ = unsafe { KillTimer(None, id_event) };
    STATE.with(|cell| {
        let mut s = cell.borrow_mut();
        s.hold_timer_id = None;
        // 只有仍在 armed 且未 fire 过才发 Hold
        if s.armed_key.is_some() && !s.hold_fired && !s.aborted {
            s.hold_fired = true;
            send_event(HotkeyEvent::Hold(Instant::now()));
        }
    });
}

/// 启动 Windows 钩子线程。
pub fn start_hook_thread() {
    std::thread::Builder::new()
        .name("blink-hotkey".into())
        .spawn(hook_thread_main)
        .expect("failed to spawn hotkey thread");
}

/// 热键线程入口：安装钩子 → 消息循环 → 卸载。
fn hook_thread_main() {
    unsafe {
        let hhook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0)
            .expect("SetWindowsHookExW failed for WH_KEYBOARD_LL");

        // hook 挂上之前发生的 Alt keydown 收不到，此刻用 GetAsyncKeyState 兜底初始化。
        // 此时进程刚起，SetForegroundWindow 还没被调用过，物理态尚未被合成 keyup 污染。
        let alt_now = key_down(VK_LMENU) || key_down(VK_RMENU);
        ALT_LOGICALLY_HELD.store(alt_now, Ordering::SeqCst);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hhook);
    }
}

/// 配置修饰键名 → 可接受的物理位集合。通用名(`alt`)= 左右任一;具体名(`lalt`)= 单侧。
/// 未知名返回 None(→ 不匹配,避免损坏配置宽松放过)。纯函数,可单测。
fn mask_for_config_modifier(name: &str) -> Option<u16> {
    match name {
        "ctrl" => Some(MOD_LCTRL | MOD_RCTRL),
        "lctrl" => Some(MOD_LCTRL),
        "rctrl" => Some(MOD_RCTRL),
        "shift" => Some(MOD_LSHIFT | MOD_RSHIFT),
        "lshift" => Some(MOD_LSHIFT),
        "rshift" => Some(MOD_RSHIFT),
        "alt" => Some(MOD_LALT | MOD_RALT),
        "lalt" => Some(MOD_LALT),
        "ralt" => Some(MOD_RALT),
        "meta" => Some(MOD_LMETA | MOD_RMETA),
        _ => None,
    }
}

/// 取 mask 中最低位(优先消耗的物理位)。
fn first_set_bit(mask: u16) -> u16 {
    mask & mask.wrapping_neg()
}

/// 当前物理修饰键集合是否**精确**满足配置要求。纯函数,可单测。
///
/// 「消耗」模型:每个配置修饰键吃掉一个当前按下的物理位(通用名吃任一侧),最后要求
/// 无剩余位 —— 即「配置要求的都按下,且没有多余修饰键」。这保证 `Ctrl+Alt+空格`
/// 不会误触发 `Alt+空格`(remaining 非空 → false)。
fn modifiers_mask_satisfies_config(config_modifiers: &[String], pressed_mask: u16) -> bool {
    let mut remaining = pressed_mask;
    for config_mod in config_modifiers {
        let Some(allowed) = mask_for_config_modifier(config_mod) else {
            return false;
        };
        let matched = remaining & allowed;
        if matched == 0 {
            return false;
        }
        remaining &= !first_set_bit(matched); // 精确消耗一个物理位
    }
    remaining == 0
}

/// AltGr 修正:很多键盘布局下右 Alt(AltGr)按下会伴随系统合成的左 Ctrl,
/// `GetAsyncKeyState(VK_LCONTROL)` 也显示按下。若不修正,用户用 AltGr 输入字符时
/// 会误触发含 Ctrl 的组合键。故 RAlt+LCtrl 同时按下时,把 LCtrl 视为合成、从 mask 去掉。
/// 代价:真实 `LCtrl+RAlt+key` 无法触发 `Ctrl+RAlt+key`(极少见,与旧 recorder 取舍一致)。
/// 纯函数,可单测。
fn apply_altgr_correction(mask: u16) -> u16 {
    if mask & MOD_RALT != 0 && mask & MOD_LCTRL != 0 {
        mask & !MOD_LCTRL
    } else {
        mask
    }
}

/// 是否「单独修饰键」配置(modifiers 空 + key 是单修饰键,如右 Alt 单击)。纯函数,可单测。
fn is_standalone_config(config: &crate::app::config::HotkeyConfig) -> bool {
    config.modifiers.is_empty() && super::recorder::is_standalone_modifier_key(&config.key)
}

/// 查某虚拟键当前物理是否按下(GetAsyncKeyState 高位)。封装以便将来切换实现。
fn key_down(vk: VIRTUAL_KEY) -> bool {
    unsafe { GetAsyncKeyState(vk.0 as i32) as u16 & 0x8000 != 0 }
}

/// 当前 Alt（任一侧）是否**逻辑**按下——只跟随真实键盘事件，免疫合成 keyup。
///
/// 供前端轮询——WebView2 不转发 Alt 键自身的 keydown 到 JS（系统键被 Windows 用于菜单
/// 激活），前端 keydown 监听不可靠，故 0.8.5 增强菜单的 alt-active 状态改由前端轮询此
/// 接口驱动（§6.1）。0.11.10 改为读 `ALT_LOGICALLY_HELD`——见其文档字符串对 SetForegroundWindow
/// 合成 keyup 污染 `GetAsyncKeyState` 的说明。
pub fn is_alt_down() -> bool {
    ALT_LOGICALLY_HELD.load(Ordering::SeqCst)
}

/// 在调 `SetForegroundWindow` 前调用，通知 LL hook 即将产生合成 Alt keyup。
///
/// 仅在 Alt 当前逻辑按下时才设 flag——`SetForegroundWindow` 只在 Alt 按下时
/// 合成 keyup（释放系统菜单栏激活态）。Alt 未按下时调此函数是 no-op。
///
/// 见 [`EXPECT_SYNTH_KEYUP_AT`] 文档字符串了解完整设计。
pub fn expect_synthesized_alt_keyup() {
    let held = ALT_LOGICALLY_HELD.load(Ordering::SeqCst);
    if held {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        EXPECT_SYNTH_KEYUP_AT.store(now, Ordering::SeqCst);
        tracing::debug!(held, "expect_synthesized_alt_keyup: flag set");
    } else {
        tracing::debug!(held, "expect_synthesized_alt_keyup: skip (Alt not held)");
    }
}

/// 采样当前 8 个修饰键的物理态为 bitmask。
fn current_modifier_mask() -> u16 {
    let mut mask = 0u16;
    if key_down(VK_LCONTROL) {
        mask |= MOD_LCTRL;
    }
    if key_down(VK_RCONTROL) {
        mask |= MOD_RCTRL;
    }
    if key_down(VK_LSHIFT) {
        mask |= MOD_LSHIFT;
    }
    if key_down(VK_RSHIFT) {
        mask |= MOD_RSHIFT;
    }
    if key_down(VK_LMENU) {
        mask |= MOD_LALT;
    }
    if key_down(VK_RMENU) {
        mask |= MOD_RALT;
    }
    if key_down(VK_LWIN) {
        mask |= MOD_LMETA;
    }
    if key_down(VK_RWIN) {
        mask |= MOD_RMETA;
    }
    mask
}

/// 当前修饰键物理态是否满足配置(在主键 down/up 边界调用)。
/// standalone 配置无需额外修饰键(主键本身即修饰键),直接 true。
fn modifiers_satisfied(config: &crate::app::config::HotkeyConfig) -> bool {
    if is_standalone_config(config) {
        return true;
    }
    let mask = apply_altgr_correction(current_modifier_mask());
    modifiers_mask_satisfies_config(&config.modifiers, mask)
}

/// 将虚拟键码转换为配置中的键名。
fn vk_to_key(vk: u32) -> Option<String> {
    // 修饰键。通用码（VK_SHIFT / VK_CONTROL / VK_MENU，不分左右）当作左侧——
    // 兼容某些驱动/事件流只发通用码的情况；否则这些按键会被 vk_to_key 忽略，
    // 导致录制单独修饰键（如左 Alt）时永远等不到松开事件、无法结束。
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

    // 标点/符号键（OEM 键，按美式键盘布局命名）。非修饰键，作为主键录制。
    if vk == 0xBA {
        return Some(";".to_string());
    } // VK_OEM_1      ';'
    if vk == 0xBB {
        return Some("=".to_string());
    } // VK_OEM_PLUS   '='
    if vk == 0xBC {
        return Some(",".to_string());
    } // VK_OEM_COMMA  ','
    if vk == 0xBD {
        return Some("-".to_string());
    } // VK_OEM_MINUS  '-'
    if vk == 0xBE {
        return Some(".".to_string());
    } // VK_OEM_PERIOD '.'
    if vk == 0xBF {
        return Some("/".to_string());
    } // VK_OEM_2      '/'
    if vk == 0xC0 {
        return Some("`".to_string());
    } // VK_OEM_3      '`'
    if vk == 0xDB {
        return Some("[".to_string());
    } // VK_OEM_4      '['
    if vk == 0xDC {
        return Some("\\".to_string());
    } // VK_OEM_5      '\'
    if vk == 0xDD {
        return Some("]".to_string());
    } // VK_OEM_6      ']'
    if vk == 0xDE {
        return Some("'".to_string());
    } // VK_OEM_7      '''

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

/// 低级键盘钩子回调：tap/hold 状态机。全程放行，绝不吞键。
unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const HC_ACTION: i32 = 0;
    if code == HC_ACTION {
        let kb = unsafe { &*(lparam.0 as usize as *const KBDLLHOOKSTRUCT) };
        let vk = kb.vkCode;
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        // ── Alt 逻辑 hold 态维护（早于任何短路，跨所有分支生效） ──────────────────
        //
        // **设计**（0.11.11）：不再使用 `LLKHF_INJECTED` 过滤 Alt 事件。
        // 该 flag 无法区分“远程控制软件注入的真实按键”与“SetForegroundWindow
        // 合成的 Alt keyup”——RustDesk/TeamViewer 等第三方远程控制工具通过
        // SendInput 注入键盘事件，事件携带 LLKHF_INJECTED，但
        // `GetSystemMetrics(SM_REMOTESESSION)` 对它们返回 false（只检测 Windows
        // 原生 RDP），导致 Alt 事件被全量过滤，ALT_LOGICALLY_HELD 永不为 true。
        //
        // 改为：接受所有 Alt keydown/keyup 事件更新 ALT_LOGICALLY_HELD，仅用
        // `EXPECT_SYNTH_KEYUP_AT` one-shot flag 过滤我们自己 SetForegroundWindow
        // 合成的 keyup（这是原 LLKHF_INJECTED 过滤的真正目标）。
        let injected = (kb.flags & LLKHF_INJECTED) == LLKHF_INJECTED;

        if vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 {
            // one-shot flag：Alt keyup 时无条件 swap 清除
            let was_expected = if is_up {
                let expected_at = EXPECT_SYNTH_KEYUP_AT.load(Ordering::SeqCst);
                if expected_at > 0 {
                    EXPECT_SYNTH_KEYUP_AT.store(0, Ordering::SeqCst);
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    now - expected_at < 200
                } else {
                    false
                }
            } else {
                false
            };

            let prev_held = ALT_LOGICALLY_HELD.load(Ordering::SeqCst);
            if is_down {
                ALT_LOGICALLY_HELD.store(true, Ordering::SeqCst);
            } else if is_up && !was_expected {
                // 真实 keyup——一侧松开时若另一侧仍按着，逻辑态保持 true。
                let other_vk = if vk == VK_LMENU.0 as u32 {
                    VK_RMENU
                } else {
                    VK_LMENU
                };
                if !key_down(other_vk) {
                    ALT_LOGICALLY_HELD.store(false, Ordering::SeqCst);
                }
            }
            // was_expected=true → 跳过（我们自己 SetForegroundWindow 合成的 keyup）

            // tracing::trace!(
            //     side = if vk == VK_LMENU.0 as u32 { "L" } else { "R" },
            //     is_down,
            //     is_up,
            //     injected,
            //     was_expected,
            //     prev_held,
            //     now_held = ALT_LOGICALLY_HELD.load(Ordering::SeqCst),
            //     "alt-event"
            // );
        }

        // 录制短路：录制期间把事件喂给 recorder，且不碰触发的 thread_local STATE。
        if super::recorder::is_recording() {
            feed_recorder(vk, wparam);
            // 录制期间吞掉 Alt+Space：WebView2 在底层把 WM_SYSKEYDOWN(VK_SPACE)+Alt
            // 转发给宿主，前端 preventDefault 拦不住，会呼出左上角系统菜单并冻结
            // webview 消息泵。仅在录制期间、仅此组合吞键，不破坏日常「不吞键」原则。
            if vk == VK_SPACE.0 as u32 && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0 {
                return LRESULT(1);
            }
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        // hold 录音中吞掉 Alt+Space 的 keydown：防止 Windows 反复弹出系统菜单（"噔噔噔"声）。
        // 仅在 hold_fired=true（录音已启动）时吞 keydown，keyup 不吞（否则 HoldRelease 收不到）。
        //
        // Alt 判定走 `ALT_LOGICALLY_HELD` 而非 GetAsyncKeyState——录音期间焦点可能被
        // SetForegroundWindow 移动过，物理态被合成 keyup 污染，若读现查会漏吞 Space。
        if is_down
            && VOICE_RECORDING.load(Ordering::SeqCst)
            && (vk == VK_SPACE.0 as u32
                || vk == VK_MENU.0 as u32
                || vk == VK_LMENU.0 as u32
                || vk == VK_RMENU.0 as u32)
            && ALT_LOGICALLY_HELD.load(Ordering::SeqCst)
        {
            return LRESULT(1);
        }

        // 0.10.7 Chord 独占模式：主窗 focused + Alt hold 时，吞掉 chord 键的 keydown，
        // **并发 Chord 事件给 HotkeyService 触发**（0.10.7.2 修复：原实现只吞不发，
        // 导致前端收不到 keydown，chord 触发链路断裂）。
        //
        // **吞键范围**：仅 keydown、仅非修饰键、仅 Alt 逻辑 hold、仅键在 CHORD_KEYS 中。
        // **不吞**：修饰键本身（Alt 全程放行）、keyup（让其他软件收到完整 up）。
        //
        // **为什么需要吞键 + 发事件**：前端 `onChordTrigger` 靠 webview keydown 事件触发，
        // 但 LL hook 吞键后 webview 收不到 keydown。故 hook 吞键后直接发 `Chord(key)` 事件，
        // HotkeyService 在主线程调 trigger 逻辑，绕过前端 keydown 链路。同时吞键防止
        // 其他软件的全局快捷键（如 Alt+A 截图）抢键。
        //
        // Alt 判定走 `ALT_LOGICALLY_HELD` 而非 GetAsyncKeyState——用户按住 Alt 呼窗时
        // SetForegroundWindow 合成 keyup 污染物理态，若读现查 chord 独占永不激活。
        if is_down
            && is_chord_mode()
            && !is_modifier_key(vk)
            && ALT_LOGICALLY_HELD.load(Ordering::SeqCst)
        {
            if let Some(key) = vk_to_key(vk) {
                if is_chord_key(&key) {
                    // 吞键 + 发 Chord 事件
                    send_event(HotkeyEvent::Chord(key));
                    return LRESULT(1);
                }
            }
        }

        let config = get_current_config();
        let tap_threshold = get_tap_threshold();

        STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            let key = vk_to_key(vk);

            if is_down {
                // ESC 录音取消：录音中按 ESC → 发 VoiceCancel
                if vk == VK_ESCAPE.0 as u32 && VOICE_RECORDING.load(Ordering::SeqCst) {
                    send_event(HotkeyEvent::VoiceCancel(Instant::now()));
                    return; // 不 set aborted，让 ESC 自然放行
                }

                let Some(key) = key else {
                    // 未映射键 down:armed 期间出现 → 判 hold(用户按了别的键)。
                    if s.armed_key.is_some() {
                        s.aborted = true;
                    }
                    return;
                };

                if let Some(armed) = s.armed_key.as_deref() {
                    if key == armed {
                        // 同一主键重复 down = autorepeat,忽略(不重置 down_since)。
                        return;
                    }
                    // 修饰键 auto-repeat 不算 abort（Alt/Ctrl/Shift 按住会自动重复）
                    if is_modifier_key(vk) {
                        return;
                    }
                    // armed 后按了别的非修饰键 → hold。
                    s.aborted = true;
                    return;
                }

                // 未 armed:仅当「是配置主键 且 修饰键此刻满足」才 arm。
                if key == config.key && modifiers_satisfied(&config) {
                    s.armed_key = Some(key);
                    s.down_since = Some(Instant::now());
                    s.aborted = false;
                    s.hold_fired = false;
                    // 启动 hold timer:超阈值后发 Hold 事件(语音录音开始)
                    let timer_id = unsafe {
                        SetTimer(None, 1, tap_threshold as u32, Some(hold_timer_callback))
                    };
                    s.hold_timer_id = Some(timer_id);
                }
            } else if is_up {
                let Some(key) = key else { return };
                // 只有 armed 的那个主键松开才判定。
                if s.armed_key.as_deref() != Some(key.as_str()) {
                    return;
                }
                // 清理 hold timer(tap 路径下 timer 可能还没 fire)
                if let Some(tid) = s.hold_timer_id.take() {
                    let _ = unsafe { KillTimer(None, tid) };
                }
                let since = s.down_since.take();
                let aborted = s.aborted;
                let hold_fired = s.hold_fired;
                s.armed_key = None;
                s.aborted = false;
                s.hold_fired = false;

                if aborted {
                    return;
                }
                let Some(since) = since else { return };

                // 判断是 Hold 释放还是 Tap:
                // - hold_fired=true → Hold 事件已发(超过阈值)→ 发 HoldRelease
                // - hold_fired=false → 在 tap 窗口内松开 → 发 Tap(也兼容 elapsed 判断,
                //   双保险:timer 未 fire 但已超阈值,仍判 hold)
                if hold_fired || since.elapsed() > Duration::from_millis(tap_threshold) {
                    // Hold 释放(语音录音停止→STT→注入)
                    send_event(HotkeyEvent::HoldRelease(Instant::now()));
                } else {
                    // Tap 触发。无需在此复查修饰键:arm 时已现查物理态精确匹配,
                    // 按下期间任何异键 down 都会 aborted。
                    send_event(HotkeyEvent::Tap(Instant::now()));
                }
            }
        });
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 录制期间把原始 VK 事件归一化为语义事件喂给 [`super::recorder`]。
///
/// 平台特定逻辑集中在此：VK→键名映射复用 [`vk_to_key`]，AltGr 去模拟
/// （右 Alt 附带的左 Ctrl）通过 [`super::recorder::drop_modifier`] 清除。
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
                super::recorder::drop_modifier("lctrl");
            }
            super::recorder::feed(super::recorder::RecordInput::ModifierDown(name));
        } else {
            super::recorder::feed(super::recorder::RecordInput::ModifierUp(name));
        }
    } else if is_down {
        // 非修饰键:按下即完成录制;松开不关心。
        let Some(name) = vk_to_key(vk) else { return };
        super::recorder::feed(super::recorder::RecordInput::KeyDown(name));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::config::HotkeyConfig;

    fn cfg(modifiers: &[&str], key: &str) -> HotkeyConfig {
        HotkeyConfig {
            modifiers: modifiers.iter().map(|s| s.to_string()).collect(),
            key: key.to_string(),
            display: String::new(),
            ..Default::default()
        }
    }

    #[test]
    fn mask_for_config_modifier_names() {
        assert_eq!(
            mask_for_config_modifier("ctrl"),
            Some(MOD_LCTRL | MOD_RCTRL)
        );
        assert_eq!(mask_for_config_modifier("lctrl"), Some(MOD_LCTRL));
        assert_eq!(mask_for_config_modifier("rctrl"), Some(MOD_RCTRL));
        assert_eq!(mask_for_config_modifier("alt"), Some(MOD_LALT | MOD_RALT));
        assert_eq!(mask_for_config_modifier("lalt"), Some(MOD_LALT));
        assert_eq!(mask_for_config_modifier("ralt"), Some(MOD_RALT));
        assert_eq!(
            mask_for_config_modifier("meta"),
            Some(MOD_LMETA | MOD_RMETA)
        );
        assert_eq!(mask_for_config_modifier("cmd"), None); // 未知名
    }

    #[test]
    fn exact_specific_modifier() {
        assert!(modifiers_mask_satisfies_config(&["lalt".into()], MOD_LALT));
        assert!(!modifiers_mask_satisfies_config(&["lalt".into()], MOD_RALT)); // 右非左
        assert!(!modifiers_mask_satisfies_config(
            &["lalt".into()],
            MOD_LALT | MOD_LCTRL
        )); // 多余 ctrl
    }

    #[test]
    fn generic_modifier_either_side() {
        assert!(modifiers_mask_satisfies_config(&["alt".into()], MOD_LALT));
        assert!(modifiers_mask_satisfies_config(&["alt".into()], MOD_RALT));
        // 通用名也不允许两侧同时(多余)
        assert!(!modifiers_mask_satisfies_config(
            &["alt".into()],
            MOD_LALT | MOD_RALT
        ));
    }

    #[test]
    fn multi_modifier_combo() {
        assert!(modifiers_mask_satisfies_config(
            &["alt".into(), "ctrl".into()],
            MOD_LALT | MOD_LCTRL
        ));
        assert!(!modifiers_mask_satisfies_config(
            &["alt".into(), "ctrl".into()],
            MOD_LALT
        )); // 缺 ctrl
    }

    #[test]
    fn ctrl_alt_space_must_not_match_alt_space() {
        // 核心安全断言:配置 Alt+空格,物理按下 Ctrl+Alt → 不匹配(remaining 含 ctrl)
        let alt_only = ["alt".to_string()];
        assert!(!modifiers_mask_satisfies_config(
            &alt_only,
            MOD_LALT | MOD_LCTRL
        ));
    }

    #[test]
    fn empty_modifiers_requires_no_modifier() {
        assert!(modifiers_mask_satisfies_config(&[], 0));
        assert!(!modifiers_mask_satisfies_config(&[], MOD_LALT)); // 按了多余的
    }

    #[test]
    fn altgr_correction_strips_synthetic_lctrl() {
        assert_eq!(apply_altgr_correction(MOD_RALT | MOD_LCTRL), MOD_RALT);
        assert_eq!(
            apply_altgr_correction(MOD_RALT | MOD_LCTRL | MOD_LSHIFT),
            MOD_RALT | MOD_LSHIFT
        );
        // 非 AltGr 场景不动
        assert_eq!(
            apply_altgr_correction(MOD_LALT | MOD_LCTRL),
            MOD_LALT | MOD_LCTRL
        );
        assert_eq!(apply_altgr_correction(MOD_RALT), MOD_RALT);
        assert_eq!(apply_altgr_correction(MOD_LCTRL), MOD_LCTRL);
    }

    #[test]
    fn standalone_config_detection() {
        assert!(is_standalone_config(&cfg(&[], "ralt")));
        assert!(is_standalone_config(&cfg(&[], "meta")));
        assert!(!is_standalone_config(&cfg(&[], " "))); // 空格不是修饰键
        assert!(!is_standalone_config(&cfg(&["alt"], " "))); // 组合键
    }
}
