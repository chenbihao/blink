//! Windows 平台特定的窗口控制实现：Win32 API。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetAncestor, GetForegroundWindow, SetWindowLongPtrW, GA_ROOT, GWLP_WNDPROC,
    WNDPROC,
};

const ST_HIDDEN: u8 = 0;
const ST_VISIBLE: u8 = 1;

/// 默认 grace period。
const DEFAULT_GRACE_MS: u64 = 500;

static STATE: AtomicU8 = AtomicU8::new(ST_HIDDEN);
static START: OnceLock<Instant> = OnceLock::new();
static INVOKE_AT: AtomicU64 = AtomicU64::new(0);
static GRACE_MS: AtomicU64 = AtomicU64::new(DEFAULT_GRACE_MS);

/// 程序启动以来的毫秒数（单调时钟，用于 grace period 计算）。
fn elapsed_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 唤起：定位到前台应用所在显示器中上部 → show → set_focus → 通知前端聚焦输入框。
pub fn invoke(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        return;
    };

    if let Some(pos) = launcher_position(&win) {
        let _ = win.set_position(pos);
    }
    // 记录 invoke 时间戳（看门狗 grace period 用）
    // 立即进入 VISIBLE：set_focus() 不保证立即生效（Windows 反偷焦保护），
    // 如果卡在 SHOWING 态等 on_focused(true)，用户点其他窗口时不会触发隐藏。
    let now = elapsed_ms();
    let grace_ms = GRACE_MS.load(Ordering::SeqCst);
    INVOKE_AT.store(now, Ordering::SeqCst);
    STATE.store(ST_VISIBLE, Ordering::SeqCst);
    tracing::debug!(grace_ms, "invoke: state → VISIBLE, show + set_focus");
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit("blink://shown", ());
}

/// 隐藏：ESC / 看门狗 / 单实例重复启动。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        tracing::debug!(reason, "hide: state → HIDDEN");
        let _ = win.hide();
        let _ = app.emit("blink://hidden", ());
    }
}

/// 窗口焦点事件：仅在真正获焦时进入 Visible（启用看门狗）。
/// 失焦不在此时处理，交给看门狗轮询前台窗口。
pub fn on_focused(focused: bool) {
    let st = STATE.load(Ordering::SeqCst);
    tracing::debug!(focused, st, "on_focused");
    if focused {
        STATE.store(ST_VISIBLE, Ordering::SeqCst);
        tracing::debug!("on_focused: state → VISIBLE, watchdog armed");
    }
}

/// 常驻看门狗：窗口 Visible 时每 150ms 检查前台窗口是否仍为自身，否则隐藏。
/// invoke 后 GRACE_MS 内不触发隐藏，覆盖 show → 获焦 → 立即丢焦 的焦点抖动。
pub fn start_watchdog(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            sleep(Duration::from_millis(150)).await;
            if STATE.load(Ordering::SeqCst) != ST_VISIBLE {
                continue;
            }
            let grace_ms = GRACE_MS.load(Ordering::SeqCst);
            let since_invoke = elapsed_ms() - INVOKE_AT.load(Ordering::SeqCst);
            if since_invoke < grace_ms {
                continue;
            }
            let fg = unsafe { GetForegroundWindow() };
            if !is_self_foreground(&app, fg) {
                tracing::info!(since_invoke, "watchdog: hide! fg=0x{:x}", fg.0 as isize);
                hide(&app, "watchdog");
            }
        }
    });
}

/// 更新 grace period（线程安全）。
pub fn update_grace_period(period: u64) {
    GRACE_MS.store(period, Ordering::SeqCst);
}

/// 主窗口当前是否处于可见态（供快捷键 toggle 判断）。
pub fn is_visible() -> bool {
    STATE.load(Ordering::SeqCst) == ST_VISIBLE
}

const WM_SYSCOMMAND: u32 = 0x0112;
const SC_KEYMENU: usize = 0xF100;

/// 原始窗口过程（替换后存回，转交原逻辑用）。
static ORIGINAL_WNDPROC: OnceLock<isize> = OnceLock::new();

/// 拦截 Alt+Space 系统菜单（替换窗口过程，吞掉 SC_KEYMENU）。主窗口虽无边框仍响应
/// Alt+Space 弹出移动/最大化菜单，前端 preventDefault 与去 WS_SYSMENU 都无效，
/// 只能在窗口过程层拦截 WM_SYSCOMMAND。仅作用于主窗口。
pub fn install_sysmenu_blocker(hwnd: HWND) {
    unsafe {
        let original = SetWindowLongPtrW(hwnd, GWLP_WNDPROC, sysmenu_block_proc as *const () as usize as isize);
        let _ = ORIGINAL_WNDPROC.set(original);
    }
}

unsafe extern "system" fn sysmenu_block_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_SYSCOMMAND && (wparam.0 as usize & 0xFFF0) == SC_KEYMENU {
        return LRESULT(0);
    }
    let original = ORIGINAL_WNDPROC.get().copied().unwrap_or(0);
    // edition 2024：unsafe fn 内的 unsafe 操作需显式 unsafe block
    unsafe {
        let proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(original);
        CallWindowProcW(proc, hwnd, msg, wparam, lparam)
    }
}

/// 前台窗口是否为我们的主窗口（拿不到窗口/句柄时保守返回 true，避免误隐藏）。
fn is_self_foreground(app: &AppHandle, fg: windows::Win32::Foundation::HWND) -> bool {
    let Some(win) = app.get_webview_window("main") else {
        return true;
    };
    let Ok(hwnd) = win.hwnd() else {
        return true;
    };
    // win.hwnd() 返回 WebView2 控件 HWND（内层子窗口），GetForegroundWindow() 返回
    // 外层 Tauri 窗口 HWND。用 GetAncestor(GA_ROOT) 向上追溯到顶级窗口再比较。
    let self_hwnd = unsafe { GetAncestor(windows::Win32::Foundation::HWND(hwnd.0 as _), GA_ROOT) };
    fg.0 as isize == self_hwnd.0 as isize
}

/// 计算窗口在前台应用所在显示器的位置：中上部居中（物理像素）。
fn launcher_position(win: &WebviewWindow) -> Option<PhysicalPosition<i32>> {
    // 读窗口实际物理尺寸定位（与弹性 resize 同步，不再硬编码 700×60）
    let size = win.outer_size().ok()?;
    let w = size.width as i32;
    let h = size.height as i32;

    unsafe {
        let fg = GetForegroundWindow();
        let hmon = MonitorFromWindow(fg, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let rc = mi.rcMonitor;
            let cx = rc.left + (rc.right - rc.left) / 2;
            let cy = rc.top + (rc.bottom - rc.top) / 2; // 屏幕正中
            return Some(PhysicalPosition::new(cx - w / 2, cy - h / 2));
        }
    }
    None
}
