//! 全局右 Alt 热键：低级键盘钩子（WH_KEYBOARD_LL）+ tap/hold 状态机。
//!
//! 单击右 Alt → 唤起；按住右 Alt 配合其他键 → 仍作系统 Alt。
//! 关键：回调全程 `CallNextHookEx` 放行，绝不吞右 Alt，故系统组合不受影响。
//! 对应 Todo R1 / T1.x，主文档 §13.1。

use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::Input::KeyboardAndMouse::VK_RMENU;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, DispatchMessageW, GetMessageW, KBDLLHOOKSTRUCT, SetWindowsHookExW,
    TranslateMessage, UnhookWindowsHookEx, MSG, WH_KEYBOARD_LL, WM_KEYDOWN, WM_KEYUP,
    WM_SYSKEYDOWN, WM_SYSKEYUP,
};

/// 单击右 Alt 的最大按压时长：超过则视为长按，不唤起（防误触）。
const TAP_MAX_MS: u64 = 300;

/// 热键事件（由 hook 线程发往主线程）。
#[derive(Debug, Clone)]
#[allow(dead_code)] // Tap 携带的时间戳为延迟测量预留
pub enum HotkeyEvent {
    /// 右 Alt 单击（tap），附带触发时刻用于延迟测量。
    Tap(Instant),
}

/// hook 线程私有状态：仅 hook 线程访问，无需同步。
struct State {
    /// 右 Alt 处于按下状态时的按下时刻；None 表示未按下。
    down_since: Option<Instant>,
    /// 按下期间是否出现过其他键（出现 → 判 hold，不唤起）。
    aborted: bool,
}

thread_local! {
    static STATE: std::cell::RefCell<State> = std::cell::RefCell::new(State {
        down_since: None,
        aborted: false,
    });
}

/// 全局事件发送端：hook 回调通过它把 tap 信号发给主线程（unbounded，非阻塞）。
static SENDER: std::sync::OnceLock<mpsc::UnboundedSender<HotkeyEvent>> = std::sync::OnceLock::new();

/// 启动专用热键线程，返回事件接收端。
/// 线程内安装 LL hook 并跑消息循环；回调仅更新状态机 + 发信号，绝不做阻塞操作。
pub fn start() -> mpsc::UnboundedReceiver<HotkeyEvent> {
    let (tx, rx) = mpsc::unbounded_channel::<HotkeyEvent>();
    let _ = SENDER.set(tx);

    std::thread::Builder::new()
        .name("blink-hotkey".into())
        .spawn(hook_thread_main)
        .expect("failed to spawn hotkey thread");

    rx
}

/// 热键线程入口：安装钩子 → 消息循环 → 卸载。
fn hook_thread_main() {
    unsafe {
        // 安装全局低级键盘钩子（dwThreadId=0 表示全局）；hmod 可为空。
        let hhook = SetWindowsHookExW(WH_KEYBOARD_LL, Some(ll_proc), None, 0)
            .expect("SetWindowsHookExW failed for WH_KEYBOARD_LL");

        // 标准消息循环：LL hook 要求所在线程具备消息循环，否则不生效。
        // hook 线程无窗口，GetMessageW 不会返回 -1；as_bool 即可。
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        let _ = UnhookWindowsHookEx(hhook);
    }
}

/// 低级键盘钩子回调：tap/hold 状态机。全程放行，绝不吞键。
unsafe extern "system" fn ll_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const HC_ACTION: i32 = 0;
    if code == HC_ACTION {
        // edition 2024：unsafe fn 内的裸指针解引用仍需显式 unsafe block
        let kb = unsafe { &*(lparam.0 as usize as *const KBDLLHOOKSTRUCT) };
        // VK_RMENU 是 u16（虚拟键码），vkCode 是 u32，需统一类型
        let is_ralt = kb.vkCode == VK_RMENU.0 as u32;
        let msg = wparam.0 as u32;
        let is_down = msg == WM_KEYDOWN || msg == WM_SYSKEYDOWN;
        let is_up = msg == WM_KEYUP || msg == WM_SYSKEYUP;

        STATE.with(|cell| {
            let mut s = cell.borrow_mut();
            if is_ralt {
                if is_down {
                    s.down_since = Some(Instant::now());
                    s.aborted = false;
                } else if is_up {
                    if let Some(since) = s.down_since.take() {
                        let held = since.elapsed();
                        // tap 判定：期间无其他键 且 按压时长在阈值内
                        if !s.aborted && held <= Duration::from_millis(TAP_MAX_MS) {
                            if let Some(tx) = SENDER.get() {
                                // unbounded send 非阻塞，绝不卡 hook 线程
                                let _ = tx.send(HotkeyEvent::Tap(Instant::now()));
                            }
                        }
                    }
                    s.aborted = false;
                }
            } else if is_down {
                // 右 Alt 按下期间出现其他键 → 判 hold（组合键），整段放行
                if s.down_since.is_some() {
                    s.aborted = true;
                }
            }
        });
    }

    // 全程放行。做得差的产品在此 `return LRESULT(1)` 吞掉右 Alt（独占），
    // 致使右 Alt 无法再作系统修饰键；本方案不吞，系统组合不受影响。
    // hhk 参数对 LL hook 被忽略，传 None（NULL）即可。
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}
