//! 悬浮窗口显隐与焦点控制。
//!
//! 失焦检测采用「常驻看门狗轮询前台窗口」而非纯事件驱动：
//! - 不依赖 WM_ACTIVATE(deactivate)，能覆盖 IDEA 终端子进程等不发失焦通知的窗口；
//! - invoke 后 500ms grace period 覆盖焦点抖动（show → 获焦 → 立即丢焦 的常见时序）。
//!
//! 状态机：Hidden(隐藏) → Showing(已 show，等获焦) → Visible(已获焦，看门狗生效)。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use chrono::Local;
use tokio::time::sleep;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::UI::WindowsAndMessaging::{GetAncestor, GetForegroundWindow, GA_ROOT};

const ST_HIDDEN: u8 = 0;
const ST_VISIBLE: u8 = 1;

/// invoke 后看门狗不触发隐藏的 grace period（覆盖焦点抖动）。
const GRACE_MS: u64 = 500;

static STATE: AtomicU8 = AtomicU8::new(ST_HIDDEN);
static START: OnceLock<Instant> = OnceLock::new();
static INVOKE_AT: AtomicU64 = AtomicU64::new(0);

/// 程序启动以来的毫秒数（单调时钟，用于 grace period 计算）。
fn elapsed_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 实时时分秒毫秒（用于日志）。
fn now_str() -> String {
    Local::now().format("%H:%M:%S%.3f").to_string()
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
    INVOKE_AT.store(now, Ordering::SeqCst);
    STATE.store(ST_VISIBLE, Ordering::SeqCst);
    eprintln!("[ctl {}] invoke: state → VISIBLE, show + set_focus (watchdog grace {GRACE_MS}ms)", now_str());
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit("blink://shown", ());
}

/// 隐藏：ESC / 看门狗 / 单实例重复启动。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        eprintln!("[ctl {}] hide: state → HIDDEN ({reason})", now_str());
        let _ = win.hide();
        let _ = app.emit("blink://hidden", ());
    }
}

/// 窗口焦点事件：仅在真正获焦时进入 Visible（启用看门狗）。
/// 失焦不在此时处理，交给看门狗轮询前台窗口。
pub fn on_focused(focused: bool) {
    let st = STATE.load(Ordering::SeqCst);
    eprintln!("[ctl {}] on_focused({focused}): state={st}", now_str());
    if focused {
        STATE.store(ST_VISIBLE, Ordering::SeqCst);
        eprintln!("[ctl {}] on_focused: state → VISIBLE, watchdog armed", now_str());
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
            let since_invoke = elapsed_ms() - INVOKE_AT.load(Ordering::SeqCst);
            if since_invoke < GRACE_MS {
                continue;
            }
            let fg = unsafe { GetForegroundWindow() };
            if !is_self_foreground(&app, fg) {
                eprintln!("[ctl {}] watchdog: hide! fg=0x{:x}, since_invoke={since_invoke}ms", now_str(), fg.0 as isize);
                hide(&app, "watchdog");
            }
        }
    });
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
    // 逻辑尺寸须与 tauri.conf.json 一致；按 scale 转物理像素定位
    let (lw, lh): (f64, f64) = (700.0, 60.0);
    let scale = win.scale_factor().unwrap_or(1.0);
    let w = (lw * scale) as i32;
    let h = (lh * scale) as i32;

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
