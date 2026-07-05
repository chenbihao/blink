//! 划词监听：全局鼠标钩子（WH_MOUSE_LL）检测选词动作 → 黄金时机 UIA 抓取 → 缓存。
//!
//! 选词动作两种：
//! - **拖拽划选**：左键 down→up 位移 > 阈值。
//! - **双击选词**：两次左键 up 间隔/距离在系统双击阈值内（单词/代码选中常用）。
//!
//! 动机（0.8.0 §1.1 实测）：原「invoke 后 spawn 抓取」对 Electron 应用失败——
//! show 后焦点转 blink，Chromium 失焦退化选区。改为「选词瞬间抓取」：
//! 此刻焦点还在原窗口、选区刚形成、未退化，是黄金时机。
//!
//! 性能约束：WH_MOUSE_LL 回调运行在系统钩子链上，**绝不能阻塞**（LowLevelHooksTimeout
//! ~300ms 超时会被系统摘钩）。回调只做极轻的选词检测 + 发起抓取，UIA 抓取丢独立线程。

use std::time::Instant;

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::*;

thread_local! {
    /// 左键按下位置（供 up 时判定是否划选）。钩子线程私有。
    static DOWN_POS: std::cell::RefCell<Option<(i32, i32)>> = std::cell::RefCell::new(None);
    /// 上次左键松开的（时刻, 位置），供双击选词判定。钩子线程私有。
    static LAST_UP: std::cell::RefCell<Option<(Instant, (i32, i32))>> = std::cell::RefCell::new(None);
}

/// 启动鼠标钩子线程。
pub(super) fn start() {
    std::thread::Builder::new()
        .name("blink-selection-hook".into())
        .spawn(hook_thread_main)
        .expect("failed to spawn selection hook thread");
}

/// 钩子线程入口：安装 WH_MOUSE_LL → 消息循环 → 卸载。
fn hook_thread_main() {
    unsafe {
        let hhook = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), None, 0)
            .expect("SetWindowsHookExW failed for WH_MOUSE_LL");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = UnhookWindowsHookEx(hhook);
    }
}

/// 低级鼠标钩子回调：选词检测。全程放行（CallNextHookEx），绝不吞鼠标事件。
unsafe extern "system" fn mouse_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    const HC_ACTION: i32 = 0;
    // 关闭态直接放行（不做选词判定，也不清 down/up 状态——反正下次开启前状态天然作废）。
    // 一次原子读，微秒级开销，不影响钩子链 300ms 超时。
    if code == HC_ACTION && super::is_active() {
        let ms = unsafe { &*(lparam.0 as usize as *const MSLLHOOKSTRUCT) };
        let msg = wparam.0 as u32;
        if msg == WM_LBUTTONDOWN {
            DOWN_POS.with(|c| *c.borrow_mut() = Some((ms.pt.x, ms.pt.y)));
        } else if msg == WM_LBUTTONUP {
            let up_pos = (ms.pt.x, ms.pt.y);
            let down = DOWN_POS.with(|c| *c.borrow());
            DOWN_POS.with(|c| *c.borrow_mut() = None); // 清除，避免跨次残留
            if let Some(down) = down
                && is_drag_selection(down, up_pos)
            {
                // 拖拽划选：清双击状态，避免与之前的单击残留组合误判双击
                LAST_UP.with(|c| *c.borrow_mut() = None);
                on_selection();
            } else {
                // 非拖拽：检查双击选词（位移小，但两次 up 间隔近）
                let now = Instant::now();
                let prev = LAST_UP.with(|c| *c.borrow());
                if let Some((pt, pp)) = prev
                    && is_double_click(pt, pp, now, up_pos)
                {
                    on_selection();
                    LAST_UP.with(|c| *c.borrow_mut() = None); // 消费，避免三连击误判
                } else {
                    LAST_UP.with(|c| *c.borrow_mut() = Some((now, up_pos)));
                }
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 判定 down→up 是否构成拖拽划选（位移超阈值，排除单击/抖动）。纯函数，可单测。
fn is_drag_selection(down: (i32, i32), up: (i32, i32)) -> bool {
    const MIN_DRAG_PX: i32 = 6;
    let dx = (up.0 - down.0).abs();
    let dy = (up.1 - down.1).abs();
    dx > MIN_DRAG_PX || dy > MIN_DRAG_PX
}

/// 判定两次左键 up 是否构成双击选词（间隔 ≤ 系统双击时间 且 位移 ≤ 系统双击距离）。
/// 时间/位置参数化便于单测；系统阈值（GetDoubleClickTime / SM_CXDOUBLECLK）内部取。
fn is_double_click(
    prev_time: Instant,
    prev_pos: (i32, i32),
    now: Instant,
    now_pos: (i32, i32),
) -> bool {
    // 系统默认双击时间 500ms（GetDoubleClickTime 不在当前 windows feature，硬编码默认值，
    // 用户极少改动；如需精确可后续补 feature）。
    const DOUBLE_CLICK_INTERVAL_MS: u32 = 500;
    // SM_CXDOUBLECLK=36：系统判定双击的鼠标位移阈值（X/Y 通常相同）。
    let distance = unsafe { GetSystemMetrics(SM_CXDOUBLECLK) }.max(1);
    now.duration_since(prev_time).as_millis() as u32 <= DOUBLE_CLICK_INTERVAL_MS
        && (now_pos.0 - prev_pos.0).abs() <= distance
        && (now_pos.1 - prev_pos.1).abs() <= distance
}

/// 选词命中：在黄金时机（焦点未失）抓取前台窗口选区。抓取丢独立线程，不阻塞钩子。
fn on_selection() {
    // 选词瞬间前台就是用户正在操作的应用（还没唤起 blink），焦点未转移。
    let fg = unsafe { GetForegroundWindow() };
    let fg_raw = fg.0 as isize;
    if fg_raw == 0 {
        return;
    }
    // 隐私门控：前台是敏感应用（如密码管理器）时直接跳过抓取，源头拦截，缓存永远不落敏感文本。
    // 与 context::collect 的敏感应用检查语义一致（共用 ContextConfig.sensitive_apps）。
    if let Some(proc_name) = super::windows::process_name_of_window(fg_raw)
        && super::is_process_sensitive(&proc_name)
    {
        tracing::debug!(app = %proc_name, "划词感知：前台为敏感应用，跳过抓取");
        return;
    }
    std::thread::spawn(move || match super::get_selected_text(fg_raw) {
        Some(text) => {
            let len = text.chars().count();
            tracing::debug!(len, "选词抓取成功（黄金时机 UIA）");
            // 选区内容属用户隐私（同剪贴板），仅 trace 级谨慎记录前 100 字符。
            tracing::trace!(
                selected_text = %text.chars().take(100).collect::<String>(),
                "选词文本预览"
            );
            super::set_last_selection(text);
        }
        None => {
            tracing::trace!("选词抓取：无选区或应用不支持 UIA TextPattern");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn drag_detected_on_movement() {
        assert!(is_drag_selection((0, 0), (20, 0)));
        assert!(is_drag_selection((0, 0), (0, 20)));
        assert!(is_drag_selection((100, 100), (130, 120)));
    }

    #[test]
    fn click_not_drag() {
        assert!(!is_drag_selection((0, 0), (0, 0)));
        assert!(!is_drag_selection((5, 5), (8, 9))); // 微小抖动 < 6px
    }

    #[test]
    fn double_click_within_threshold() {
        // 两次 up 间隔 100ms、位移 2px —— 在系统默认双击阈值（500ms / ~4px）内 → 双击
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_millis(100);
        assert!(is_double_click(t0, (10, 10), t1, (12, 12)));
    }

    #[test]
    fn not_double_click_too_far_apart_in_time() {
        // 间隔 2s —— 超过系统默认双击时间（500ms）→ 非双击
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        assert!(!is_double_click(t0, (10, 10), t1, (11, 11)));
    }
}
