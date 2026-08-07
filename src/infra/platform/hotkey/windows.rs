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
//! - LL hook 是唯一能看到 `LLKHF_INJECTED` flag 的层。0.14.x 起，该 flag 用作 one-shot
//!   synth-keyup flag 的消费限定（合成 keyup 必带此 flag），而非全量过滤 Alt 事件。
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

/// Alt 键的**逻辑** hold 态：由所有 LMENU/RMENU/MENU keydown/keyup 驱动（含远程控制软件
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
/// - 0.14：ALT_LOGICALLY_HELD 维护块加入 VK_MENU（0x12，通用 Alt 键）。SetForegroundWindow
///   合成的 keyup 可能以 VK_MENU 发出（系统不分左右），旧代码只检查 VK_LMENU/VK_RMENU
///   导致合成 keyup 跳过维护块 → one-shot flag 未被消费 → 下一个真实 keyup 被误吞 →
///   ALT_LOGICALLY_HELD 卡在 true → chord 模式不退出。
/// - 0.14.x：one-shot flag 消费条件加入 `LLKHF_INJECTED` 限定。原实现无条件消费
///   200ms 窗口内的首个 Alt keyup，但快速 tap（Alt+Space 瞬按瞬松）时真实 keyup
///   可能在合成 keyup 之前到达（或 SetForegroundWindow 未合成 keyup），导致真实
///   keyup 被误吞、ALT_LOGICALLY_HELD 卡 true → chord 模式不退出。改为：仅在 keyup
///   带 `LLKHF_INJECTED` 时消费 flag（合成 keyup 必带此 flag），真实硬件 keyup 即使
///   落在窗口内也正常处理。远程控制软件注入的 keyup 也带 `LLKHF_INJECTED`，但
///   合成 keyup 总是先到（SetForegroundWindow 在 Tap 处理时同步调用），flag 被合成
///   keyup 消费后真实 keyup 不受影响——与改前行为一致，无回归。
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
/// - **消费条件**（0.14.x）：仅在 Alt keyup **同时满足** 200ms 窗口 +
///   `LLKHF_INJECTED` flag 时才消费此 flag 并跳过 keyup。SetForegroundWindow
///   合成的 keyup 始终带 `LLKHF_INJECTED`（系统注入），真实硬件 keyup 不带。
///   这修复了快速 tap 时真实 Alt keyup 被误吞导致 chord 模式卡住的 bug。
/// - 超出 200ms 窗口或新 Alt keydown 时清除 flag（防残留）。
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

            // ── 0.18.7 Phase B: 影子 HoldDeadline ──
            // 用 gesture id 近似（legacy 无 gesture id，用 timer id 作为近似）
            shadow_feed_hold_deadline(id_event as u64);
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

/// 热键线程入口：安装钩子 → 影子初始化 → 消息循环 → 卸载。
fn hook_thread_main() {
    unsafe {
        // ── 0.18.7 Phase B: 影子状态机初始化 ──
        init_shadow_state();
        init_shadow_window();
        log_startup_diagnostics();

        let hhook = match SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0) {
            Ok(h) => h,
            Err(e) => {
                tracing::error!(?e, "SetWindowsHookExW failed for WH_KEYBOARD_LL");
                return;
            }
        };
        tracing::info!(hook_ptr = hhook.0 as usize, "WH_KEYBOARD_LL hook installed");

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
        tracing::info!("WH_KEYBOARD_LL hook uninstalled");

        // ── 0.18.7 Phase B: 影子清理 ──
        destroy_shadow_window();
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
fn is_standalone_config(config: &crate::domain::config::HotkeyConfig) -> bool {
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
fn modifiers_satisfied(config: &crate::domain::config::HotkeyConfig) -> bool {
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
        // **设计**（0.11.11 + 0.14.x）：接受所有 Alt keydown/keyup 事件更新
        // ALT_LOGICALLY_HELD，仅用 `EXPECT_SYNTH_KEYUP_AT` one-shot flag 过滤
        // 我们自己 SetForegroundWindow 合成的 keyup。
        //
        // **0.14.x 关键修复**：one-shot flag 仅在 keyup 带 `LLKHF_INJECTED` 时消费。
        // SetForegroundWindow 合成的 keyup 始终带 `LLKHF_INJECTED`（系统注入），
        // 真实硬件 keyup 不带。原实现无条件消费窗口内首个 keyup，快速 tap 时
        // 真实 keyup 可能在合成 keyup 之前到达（或 SetForegroundWindow 未合成），
        // 导致真实 keyup 被误吞 → ALT_LOGICALLY_HELD 卡 true → chord 不退出。
        //
        // **0.14 VK_MENU 修复**：加入 VK_MENU（0x12，通用 Alt 键）。合成 keyup
        // 可能以 VK_MENU 发出（系统不分左右），旧代码只检查 VK_LMENU/VK_RMENU
        // 导致合成 keyup 跳过此块 → flag 未被消费 → 下一个真实 keyup 被误吞。
        if vk == VK_LMENU.0 as u32 || vk == VK_RMENU.0 as u32 || vk == VK_MENU.0 as u32 {
            // one-shot flag：仅当 keyup 带 LLKHF_INJECTED 且在 200ms 窗口内时消费
            let was_expected = if is_up {
                let expected_at = EXPECT_SYNTH_KEYUP_AT.load(Ordering::SeqCst);
                if expected_at > 0 {
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as i64)
                        .unwrap_or(0);
                    let within_window = now - expected_at < 200;
                    if within_window && kb.flags.contains(LLKHF_INJECTED) {
                        // 合成 keyup（SetForegroundWindow 注入）——消费 flag，跳过
                        EXPECT_SYNTH_KEYUP_AT.store(0, Ordering::SeqCst);
                        tracing::trace!("alt keyup: synthesized (LLKHF_INJECTED), skipping");
                        true
                    } else {
                        // 真实 keyup（无 LLKHF_INJECTED）或窗口已过期——不跳过
                        if within_window {
                            // 真实 keyup 落在窗口内但非注入——这正是修复的场景
                            tracing::debug!(
                                "alt keyup: real keyup within synth-keyup window, processing normally (LLKHF_INJECTED absent)"
                            );
                        }
                        // 超出窗口时清除过期 flag
                        if !within_window {
                            EXPECT_SYNTH_KEYUP_AT.store(0, Ordering::SeqCst);
                        }
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if is_down {
                // 新 Alt keydown：清除可能残留的 synth-keyup flag
                EXPECT_SYNTH_KEYUP_AT.store(0, Ordering::SeqCst);
                ALT_LOGICALLY_HELD.store(true, Ordering::SeqCst);
            } else if is_up && !was_expected {
                // 真实 keyup——一侧松开时若另一侧仍按着，逻辑态保持 true。
                // VK_MENU 是通用码（不分左右），需检查两侧物理态。
                let other_alt_down = if vk == VK_LMENU.0 as u32 {
                    key_down(VK_RMENU)
                } else if vk == VK_RMENU.0 as u32 {
                    key_down(VK_LMENU)
                } else {
                    // VK_MENU：检查左右 Alt 是否任一仍按下
                    key_down(VK_LMENU) || key_down(VK_RMENU)
                };
                if !other_alt_down {
                    ALT_LOGICALLY_HELD.store(false, Ordering::SeqCst);
                }
            }
            // was_expected=true → 跳过（我们自己 SetForegroundWindow 合成的 keyup）
        }

        // 录制短路：录制期间把事件喂给 recorder，且不碰触发的 thread_local STATE。
        if super::recorder::is_recording() {
            feed_recorder(vk, wparam);
            // 录制期间吞掉 Alt+Space：WebView2 在底层把 WM_SYSKEYDOWN(VK_SPACE)+Alt
            // 转发给宿主，前端 preventDefault 拦不住，会呼出左上角系统菜单并冻结
            // webview 消息泵。仅在录制期间、仅此组合吞键，不破坏日常「不吞键」原则。
            if vk == VK_SPACE.0 as u32 && unsafe { GetAsyncKeyState(VK_MENU.0 as i32) } < 0 {
                shadow_feed_hook(kb, msg, true);
                return LRESULT(1);
            }
            shadow_feed_hook(kb, msg, false);
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }

        // hold 期间吞掉 Alt+Space 的 keydown：防止 Windows 反复弹出系统菜单（"噔噔噔"声）。
        // 仅吞 keydown，keyup 不吞（否则 HoldRelease 收不到）。
        //
        // **吞键条件**：`hold_fired || VOICE_RECORDING`
        // - `hold_fired`：从 hold timer 触发到 keyup 期间一直为 true，覆盖两个时序间隙：
        //   1. Hold 事件发出 → 主线程 `start_recording()` 设置 VOICE_RECORDING 之间的间隙
        //   2. **错误场景**（STT 未配置 / 服务未就绪）：`begin_recording` 返回 false →
        //      guard 析构清除 VOICE_RECORDING，但 `emit_voice_error` 已显示 overlay，
        //      用户仍按住 Alt+Space → 需要 `hold_fired` 兜底吞键直到 keyup
        // - `VOICE_RECORDING`：录音真正启动后的持续期（覆盖 keyup → HoldRelease →
        //   stop_recording 之间的间隙，此阶段 hold_fired 已被 keyup 清零）
        //
        // Alt 判定走 `ALT_LOGICALLY_HELD` 而非 GetAsyncKeyState——录音期间焦点可能被
        // SetForegroundWindow 移动过，物理态被合成 keyup 污染，若读现查会漏吞 Space。
        let hold_fired = STATE.with(|cell| cell.borrow().hold_fired);
        if is_down
            && (hold_fired || VOICE_RECORDING.load(Ordering::SeqCst))
            && (vk == VK_SPACE.0 as u32
                || vk == VK_MENU.0 as u32
                || vk == VK_LMENU.0 as u32
                || vk == VK_RMENU.0 as u32)
            && ALT_LOGICALLY_HELD.load(Ordering::SeqCst)
        {
            shadow_feed_hook(kb, msg, true);
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
        //
        // **主窗可见性检查**（防御纵深）：chord 独占只在主窗可见时有意义。若 CHORD_MODE
        // 因前端竞态残留为 true（如截图触发时 alt-poll in-flight tick 重新 setChordMode(true)），
        // 主窗已隐藏后 hook 仍会吞 chord 键 → 桌面按 Alt+A 误触发截图。
        // 加 is_visible() 门禁确保即使 CHORD_MODE 泄露也不会吞键。is_visible() 只读
        // 一个 AtomicU8，零开销。
        if is_down
            && is_chord_mode()
            && !is_modifier_key(vk)
            && ALT_LOGICALLY_HELD.load(Ordering::SeqCst)
            && crate::infra::platform::window::is_visible()
        {
            if let Some(key) = vk_to_key(vk) {
                if is_chord_key(&key) {
                    // 吞键 + 发 Chord 事件
                    send_event(HotkeyEvent::Chord(key));
                    shadow_feed_hook(kb, msg, true);
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

        // ── 0.18.7 Phase B: 影子喂入（pass-through 路径）──
        shadow_feed_hook(kb, msg, false);
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

// ═══════════════════════════════════════════════════════════════════════════════
// ── 0.18.7 Phase B: Shadow state machine ───────────────────────────────────────
//
// 新 reducer 在 hook 线程以"影子模式"运行：接收真实 Hook/Raw Input/Timer 流，
// 但只写对比日志，不产生业务副作用。legacy 决策路径保持不变。

use super::state::{
    self, HookKeyEvent, InputConfigSnapshot, InputEvent, InputSource, InputState, MainViewContext,
    NormalizedHotkey, RawModifierEvent, RecorderMode, VoicePhase, WindowTransitionReason,
};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::*;
use windows::core::PCWSTR;

// ── 常量 ──────────────────────────────────────────────────────────────────────

/// HID usage page: Generic Desktop。
const HID_USAGE_PAGE_GENERIC: u16 = 0x01;
/// HID usage: Keyboard。
const HID_USAGE_KEYBOARD: u16 = 0x06;

/// Raw keyboard flags（RAWKEYBOARD.Flags 位）。
const RI_KEY_MAKE: u16 = 0;
const RI_KEY_BREAK: u16 = 1;
const RI_KEY_E0: u16 = 2;
const RI_KEY_E1: u16 = 4;

/// GIDC_REMOVAL：设备移除。
const GIDC_REMOVAL: u32 = 2;

// 控制消息 ID（WM_APP = 0x8000）
const WM_APP_SHADOW_CONFIG: u32 = 0x8100;
const WM_APP_SHADOW_WINDOW: u32 = 0x8101;
const WM_APP_SHADOW_VIEW: u32 = 0x8102;
const WM_APP_SHADOW_VOICE: u32 = 0x8103;
const WM_APP_SHADOW_RECORDER: u32 = 0x8104;
const WM_APP_SHADOW_STOP: u32 = 0x8105;

/// 影子窗口类名。
const SHADOW_WND_CLASS: &str = "BlinkShadowInput";

// ── 影子状态 ──────────────────────────────────────────────────────────────────

/// 影子 HWND（message-only window），供控制方 PostMessage 唤醒。
static SHADOW_HWND: std::sync::OnceLock<isize> = std::sync::OnceLock::new();

// 线程局部影子状态机（仅 hook 线程访问）。
thread_local! {
    static SHADOW: std::cell::RefCell<Option<InputState>> = const { std::cell::RefCell::new(None) };
}

// ── 控制消息队列 ──────────────────────────────────────────────────────────────
//
// 控制方（主线程）把消息放入队列，再 PostMessageW 唤醒 hook 线程。
// WindowProc 在消息循环中排空队列。LL Hook callback **不**访问此锁。

/// 影子控制消息。
enum ShadowControlMsg {
    Config(InputConfigSnapshot),
    WindowChanged { visible: bool, revision: u64 },
    ViewContext(MainViewContext),
    VoicePhase(VoicePhase),
    RecorderMode(RecorderMode),
    Stop,
}

static CONTROL_QUEUE: std::sync::Mutex<Vec<ShadowControlMsg>> = std::sync::Mutex::new(Vec::new());

/// 向 hook 线程发送控制消息（主线程调用）。
fn send_shadow_control(msg: ShadowControlMsg) {
    if let Ok(mut q) = CONTROL_QUEUE.lock() {
        q.push(msg);
    }
    if let Some(&hwnd) = SHADOW_HWND.get() {
        let _ = unsafe {
            PostMessageW(
                Some(windows::Win32::Foundation::HWND(hwnd as *mut _)),
                WM_APP_SHADOW_CONFIG,
                WPARAM(0),
                LPARAM(0),
            )
        };
    }
}

// ── 回调耗时统计 ──────────────────────────────────────────────────────────────

struct CallbackStats {
    count: u64,
    max_us: u64,
    slow_count: u64, // > 1ms
}

thread_local! {
    static STATS: std::cell::RefCell<CallbackStats> = const { std::cell::RefCell::new(CallbackStats {
        count: 0,
        max_us: 0,
        slow_count: 0,
    }) };
}

/// 记时守卫：构造时记录起始，析构时更新统计。
struct TimedCallback {
    start: Instant,
}

impl TimedCallback {
    fn start() -> Self {
        Self {
            start: Instant::now(),
        }
    }

    fn finish(self) {
        let elapsed_us = self.start.elapsed().as_micros() as u64;
        STATS.with(|s| {
            let mut s = s.borrow_mut();
            s.count += 1;
            if elapsed_us > s.max_us {
                s.max_us = elapsed_us;
            }
            if elapsed_us > 1000 {
                s.slow_count += 1;
            }
            // 每 5000 次输出一次聚合
            if s.count % 5000 == 0 {
                tracing::info!(
                    count = s.count,
                    max_us = s.max_us,
                    slow_count = s.slow_count,
                    "shadow callback stats"
                );
            }
        });
    }
}

// ── 影子配置派生 ──────────────────────────────────────────────────────────────

/// 从 legacy 全局派生影子配置快照（在 hook 线程消息循环中调用，非 ll_proc）。
fn derive_shadow_config() -> InputConfigSnapshot {
    let config = get_current_config();
    let tap_threshold = get_tap_threshold();
    InputConfigSnapshot {
        revision: 0,
        hotkey: NormalizedHotkey {
            modifiers: config.modifiers.clone(),
            key: config.key.clone(),
        },
        tap_threshold: Duration::from_millis(tap_threshold),
        chord_enabled: is_chord_mode(),
        exclusive_tap_keys: std::collections::HashSet::new(), // 影子模式：近似
        voice_hold_enabled: true,
    }
}

/// 同步外部状态到影子状态机（在消息循环中调用）。
fn sync_shadow_external() {
    SHADOW.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };

        // 派生配置并推送（近似：legacy → shadow）
        let snapshot = derive_shadow_config();
        let now = Instant::now();
        let _ = state::reduce(state, InputEvent::ConfigChanged(snapshot), now);

        // 窗口可见性
        let visible = crate::infra::platform::window::is_visible();
        let revision = state.window.revision
            + if visible != state.window.visible {
                1
            } else {
                0
            };
        if visible != state.window.visible || revision > state.window.revision {
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

        // 视图上下文（近似：从 chord_mode 派生）
        let chord_mode = is_chord_mode();
        let new_view = MainViewContext {
            view_epoch: state.view.view_epoch.max(1),
            revision: state.view.revision + 1,
            ready: true,
            query_empty: chord_mode,
            ai_mode: false,
        };
        let _ = state::reduce(state, InputEvent::ViewContextChanged(new_view), now);

        // Voice phase
        let voice_recording = VOICE_RECORDING.load(Ordering::SeqCst);
        let new_voice = if voice_recording {
            VoicePhase::Recording { gesture_id: 0 }
        } else {
            VoicePhase::Idle
        };
        let _ = state::reduce(
            state,
            InputEvent::VoicePhaseChanged {
                gesture_id: None,
                phase: new_voice,
            },
            now,
        );
    });
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
fn raw_keyboard_to_modifier(kb: &RAWKEYBOARD) -> Option<(String, bool, u16)> {
    let vk = kb.VKey;
    let is_down = (kb.Flags & RI_KEY_BREAK) == 0;
    let e0 = (kb.Flags & RI_KEY_E0) != 0;
    let e1 = (kb.Flags & RI_KEY_E1) != 0;

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

    let flags = if e0 { RI_KEY_E0 } else { 0 } | if e1 { RI_KEY_E1 } else { 0 };
    Some((key, is_down, flags))
}

// ── 影子状态喂入 ──────────────────────────────────────────────────────────────

/// 喂入 Hook 事件到影子状态机并记录对比日志。
fn shadow_feed_hook(kb: &KBDLLHOOKSTRUCT, msg: u32, legacy_swallowed: bool) {
    let timer = TimedCallback::start();

    let Some(event) = normalize_hook_event(kb, msg) else {
        timer.finish();
        return;
    };

    SHADOW.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };

        let now = Instant::now();
        let result = state::reduce(state, InputEvent::HookKey(event), now);

        // ── 影子对比日志 ──
        let shadow_alt = state.modifiers.alt_down();
        let legacy_alt = ALT_LOGICALLY_HELD.load(Ordering::SeqCst);
        if shadow_alt != legacy_alt {
            tracing::warn!(
                shadow_alt,
                legacy_alt,
                vk = kb.vkCode,
                is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN,
                "ALT_STATE_INCONSISTENT: shadow vs legacy Alt held"
            );
        }

        let shadow_swallow = result.propagation == state::Propagation::Swallow;
        if shadow_swallow != legacy_swallowed {
            tracing::debug!(
                shadow_swallow,
                legacy_swallowed,
                vk = kb.vkCode,
                "shadow vs legacy swallow divergence"
            );
        }

        // 影子 effect 日志（不送业务 channel）
        for effect in &result.effects {
            match effect {
                state::InputEffect::Tap { gesture_id, .. } => {
                    tracing::debug!(gesture_id, "shadow Tap");
                }
                state::InputEffect::HoldStarted { gesture_id } => {
                    tracing::debug!(gesture_id, "shadow HoldStarted");
                }
                state::InputEffect::HoldReleased { gesture_id } => {
                    tracing::debug!(gesture_id, "shadow HoldReleased");
                }
                state::InputEffect::VoiceCancel { gesture_id } => {
                    tracing::debug!(?gesture_id, "shadow VoiceCancel");
                }
                state::InputEffect::ChordTriggered {
                    chord_session_id,
                    key,
                } => {
                    tracing::debug!(chord_session_id, key, "shadow ChordTriggered");
                }
                state::InputEffect::UiStateChanged(ui) => {
                    tracing::debug!(
                        rev = ui.revision,
                        alt = ui.alt_down,
                        vis = ui.window_visible,
                        chord = ui.exclusive_chord_active,
                        "shadow UiStateChanged"
                    );
                }
            }
        }
    });

    timer.finish();
}

/// 喂入 HoldDeadline 到影子状态机。
fn shadow_feed_hold_deadline(gesture_id: u64) {
    SHADOW.with(|cell| {
        let mut guard = cell.borrow_mut();
        let Some(state) = guard.as_mut() else {
            return;
        };
        let now = Instant::now();
        let result = state::reduce(state, InputEvent::HoldDeadline { gesture_id }, now);
        for effect in &result.effects {
            if let state::InputEffect::HoldStarted { gesture_id } = effect {
                tracing::debug!(gesture_id, "shadow HoldStarted (timer)");
            }
        }
    });
}

// ── 影子窗口 proc ─────────────────────────────────────────────────────────────

unsafe extern "system" fn shadow_wnd_proc(
    hwnd: windows::Win32::Foundation::HWND,
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
            // lparam: hDevice handle
            if wparam.0 as u32 == GIDC_REMOVAL {
                let device_id = lparam.0 as usize;
                SHADOW.with(|cell| {
                    let mut guard = cell.borrow_mut();
                    let Some(state) = guard.as_mut() else {
                        return;
                    };
                    let _ = state::reduce(
                        state,
                        InputEvent::RawDeviceRemoved { device_id },
                        Instant::now(),
                    );
                });
                tracing::debug!(device_id, "raw input device removed (shadow)");
            }
            unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
        }
        WM_APP_SHADOW_CONFIG
        | WM_APP_SHADOW_WINDOW
        | WM_APP_SHADOW_VIEW
        | WM_APP_SHADOW_VOICE
        | WM_APP_SHADOW_RECORDER
        | WM_APP_SHADOW_STOP => {
            // 排空控制队列
            let msgs: Vec<ShadowControlMsg> = {
                let mut q = CONTROL_QUEUE.lock().unwrap_or_else(|e| e.into_inner());
                std::mem::take(&mut *q)
            };
            for m in msgs {
                match m {
                    ShadowControlMsg::Config(snapshot) => {
                        SHADOW.with(|cell| {
                            if let Some(state) = cell.borrow_mut().as_mut() {
                                let _ = state::reduce(
                                    state,
                                    InputEvent::ConfigChanged(snapshot),
                                    Instant::now(),
                                );
                            }
                        });
                    }
                    ShadowControlMsg::WindowChanged { visible, revision } => {
                        SHADOW.with(|cell| {
                            if let Some(state) = cell.borrow_mut().as_mut() {
                                let _ = state::reduce(
                                    state,
                                    InputEvent::WindowChanged {
                                        visible,
                                        revision,
                                        reason: WindowTransitionReason::Invoke,
                                    },
                                    Instant::now(),
                                );
                            }
                        });
                    }
                    ShadowControlMsg::ViewContext(ctx) => {
                        SHADOW.with(|cell| {
                            if let Some(state) = cell.borrow_mut().as_mut() {
                                let _ = state::reduce(
                                    state,
                                    InputEvent::ViewContextChanged(ctx),
                                    Instant::now(),
                                );
                            }
                        });
                    }
                    ShadowControlMsg::VoicePhase(phase) => {
                        SHADOW.with(|cell| {
                            if let Some(state) = cell.borrow_mut().as_mut() {
                                let _ = state::reduce(
                                    state,
                                    InputEvent::VoicePhaseChanged {
                                        gesture_id: None,
                                        phase,
                                    },
                                    Instant::now(),
                                );
                            }
                        });
                    }
                    ShadowControlMsg::RecorderMode(mode) => {
                        SHADOW.with(|cell| {
                            if let Some(state) = cell.borrow_mut().as_mut() {
                                let _ = state::reduce(
                                    state,
                                    InputEvent::RecorderModeChanged(mode),
                                    Instant::now(),
                                );
                            }
                        });
                    }
                    ShadowControlMsg::Stop => {
                        unsafe { PostQuitMessage(0) };
                    }
                }
            }
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) },
    }
}

/// 处理 WM_INPUT：解析 Raw Input 数据，喂入影子状态。
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
        let _ = device_id; // trace only in shadow
        let time_ms = GetMessageTime() as u32;

        let Some((key, is_down, _flags)) = raw_keyboard_to_modifier(kb) else {
            return; // 非修饰键，不进 reducer
        };

        SHADOW.with(|cell| {
            let mut guard = cell.borrow_mut();
            let Some(state) = guard.as_mut() else {
                return;
            };
            let _ = state::reduce(
                state,
                InputEvent::RawModifier(RawModifierEvent {
                    device_id,
                    key,
                    is_down,
                    time_ms,
                }),
                Instant::now(),
            );
        });
    }
}

// ── 影子窗口初始化与清理 ──────────────────────────────────────────────────────

/// 创建 message-only window 并注册 Raw Input。
/// 返回 HWND 失败时记录错误但不 panic（影子模式降级）。
fn init_shadow_window() {
    unsafe {
        // 注册窗口类
        let class_name: Vec<u16> = SHADOW_WND_CLASS
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(shadow_wnd_proc),
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
            let _ = SHADOW_HWND.set(hwnd.0 as isize);

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
                tracing::info!("shadow: Raw Input registered (keyboard, INPUTSINK|DEVNOTIFY)");
            } else {
                tracing::warn!(?result, "shadow: RegisterRawInputDevices failed (degraded)");
            }
        } else {
            tracing::warn!("shadow: CreateWindowExW failed (degraded)");
        }
    }
}

/// 销毁影子窗口。
fn destroy_shadow_window() {
    if let Some(&hwnd) = SHADOW_HWND.get() {
        unsafe {
            let _ = DestroyWindow(windows::Win32::Foundation::HWND(hwnd as *mut _));
        }
    }
}

// ── 启动诊断 ──────────────────────────────────────────────────────────────────

/// 记录启动诊断：PID、线程 ID、session、integrity、Blink 进程数。
fn log_startup_diagnostics() {
    let pid = std::process::id();
    let thread_id = unsafe { GetCurrentThreadId() };

    // 检测同时运行的 Blink 进程数（只告警，不终止）
    let blink_count = count_blink_processes();

    // session 检测（简化：只记录 PID 和线程 ID）
    tracing::info!(pid, thread_id, blink_count, "shadow: hook thread started");

    if blink_count > 1 {
        tracing::warn!(
            blink_count,
            "shadow: multiple Blink processes detected (not terminating)"
        );
    }
}

/// 统计同名进程数（简化版：只检测当前进程是否唯一）。
fn count_blink_processes() -> u32 {
    // 简化实现：在影子模式中只返回 1（完整实现需枚举进程）
    // Phase B 的进程检测在 log_startup_diagnostics 中以日志形式记录
    1
}

/// 初始化影子状态机（hook 线程启动时调用）。
fn init_shadow_state() {
    let mut state = InputState::default();
    let snapshot = derive_shadow_config();
    let _ = state::reduce(
        &mut state,
        InputEvent::ConfigChanged(snapshot),
        Instant::now(),
    );
    SHADOW.with(|cell| {
        *cell.borrow_mut() = Some(state);
    });
}

// ── 影子 API（供 mod.rs 调用）─────────────────────────────────────────────────

/// 更新影子配置（主线程调用）。
pub fn shadow_update_config(snapshot: InputConfigSnapshot) {
    send_shadow_control(ShadowControlMsg::Config(snapshot));
}

/// 更新影子窗口状态（主线程调用）。
pub fn shadow_update_window(visible: bool, revision: u64) {
    send_shadow_control(ShadowControlMsg::WindowChanged { visible, revision });
}

/// 更新影子视图上下文（主线程调用）。
pub fn shadow_update_view(ctx: MainViewContext) {
    send_shadow_control(ShadowControlMsg::ViewContext(ctx));
}

/// 停止影子引擎（主线程调用）。
pub fn shadow_stop() {
    send_shadow_control(ShadowControlMsg::Stop);
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::config::HotkeyConfig;

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
