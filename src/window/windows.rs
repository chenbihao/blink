//! Windows 平台特定的窗口控制实现：Win32 API。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromWindow, MONITORINFO, MONITOR_DEFAULTTONEAREST,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetForegroundWindow, GetWindowLongPtrW, GetWindowThreadProcessId,
    SetWindowLongPtrW, SetWindowPos, GWLP_WNDPROC, GWL_STYLE, HWND_TOP, SET_WINDOW_POS_FLAGS,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOZORDER, WNDPROC, WS_CAPTION, WS_THICKFRAME,
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

/// 唤起：采集上下文快照 → 定位 → show → set_focus → 通知前端。
///
/// **采集时机很重要**：必须在 show() 之前调用，否则拿到的前台是 Blink 自己。
pub fn invoke(app: &AppHandle) {
    // 1. 先采集上下文快照（show 之前！）
    //    读内存 ContextConfig（零 IO，热键回调不能 await），按配置过滤采集
    let context_cfg = app
        .try_state::<std::sync::Arc<std::sync::RwLock<crate::config::ContextConfig>>>()
        .map(|c| c.read().unwrap().clone())
        .unwrap_or_default();
    let snapshot = crate::context::collect(&context_cfg);
    tracing::debug!(
        foreground_app = ?snapshot.foreground_app.as_ref().map(|f| &f.process_name),
        window_title = ?snapshot.foreground_app.as_ref().map(|f| &f.window_title),
        "invoke: captured context"
    );

    // 2. 更新 SearchService 中的快照
    if let Some(search_service) = app.try_state::<std::sync::Arc<crate::search::SearchService>>() {
        search_service.update_snapshot(snapshot);
    }

    let Some(win) = app.get_webview_window("main") else {
        return;
    };

    if let Some(pos) = launcher_position(&win) {
        let _ = win.set_position(pos);
    }
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
/// 同时关闭右键菜单独立窗口（如果存在）。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        tracing::debug!(reason, "hide: state → HIDDEN");
        let _ = win.hide();
        let _ = app.emit("blink://hidden", ());
    }
    // 主窗口隐藏时联动关闭右键菜单（突破边界的独立窗口）
    if let Some(menu_win) = app.get_webview_window("context-menu") {
        let _ = menu_win.close();
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

/// 启用系统级圆角（Windows 11+）。Win10 不支持此 API，静默忽略。
///
/// DWMWCP_ROUND = 2，让系统 DWM 绘制圆角，与 CSS border-radius 同步，
/// 避免窗口四角露出不透明背景。
pub fn enable_rounded_corners(hwnd: HWND) {
    // DWMWA_WINDOW_CORNER_PREFERENCE = 33, DWMWCP_ROUND = 2
    let pref: u32 = 2; // DWMWCP_ROUND
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &pref as *const u32 as *const _,
            std::mem::size_of::<u32>() as u32,
        );
    }
}

/// 彻底移除窗口边框和标题栏（DWM 在 transparent + decorations:false 时仍会画）。
///
/// 双重手段：① 去掉 WS_CAPTION + WS_THICKFRAME 窗口样式；
/// ② DwmExtendFrameIntoClientArea 设负 margin 把 DWM 帧完全推出可视区域。
pub fn strip_window_border(hwnd: HWND) {
    unsafe {
        // 1. 去掉窗口样式中的标题栏和可拖拽边框
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let new_style = style & !(WS_CAPTION.0 as isize) & !(WS_THICKFRAME.0 as isize);
        SetWindowLongPtrW(hwnd, GWL_STYLE, new_style);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_FRAMECHANGED | SWP_NOZORDER | SWP_NOACTIVATE | SET_WINDOW_POS_FLAGS(0x0003),
        );

        // 2. 负 margin 把 DWM 帧完全推出可视区域
        let margins = MARGINS {
            cxLeftWidth: -1,
            cxRightWidth: -1,
            cyTopHeight: -1,
            cyBottomHeight: -1,
        };
        let _ = DwmExtendFrameIntoClientArea(hwnd, &margins);
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
            // fg == NULL:焦点真空(系统正在切换前台窗口的瞬态,如刚拉起子进程时)。
            // 这不代表用户切到了别的窗口,据此隐藏会误伤——跳过本轮,等下次轮询。
            if fg.0.is_null() {
                continue;
            }
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
/// 前台窗口是否属于本应用(按进程 ID 判定)。
///
/// 不再死比单个主窗口 HWND——那样会把「同属本进程的其它窗口」(debug 下 cargo run 的
/// 控制台、子进程交互产生的瞬时窗口等)误判为「别人」而隐藏。只要前台窗口的进程 ==
/// 本进程,就算焦点仍在自己,不隐藏。
fn is_self_foreground(_app: &AppHandle, fg: windows::Win32::Foundation::HWND) -> bool {
    let mut fg_pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(fg, Some(&mut fg_pid)) };
    if fg_pid == 0 {
        return true; // 拿不到 PID:保守不隐藏
    }
    let self_pid = unsafe { GetCurrentProcessId() };
    fg_pid == self_pid
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

/// resize 后若窗口底部超出显示器工作区，向上移动使其完整可见。
pub fn clamp_to_work_area(win: &WebviewWindow) {
    let Ok(pos) = win.outer_position() else { return };
    let Ok(size) = win.outer_size() else { return };
    let Ok(hwnd_raw) = win.hwnd() else { return };
    let hwnd = HWND(hwnd_raw.0 as _);

    unsafe {
        let hmon = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return;
        }
        let work = mi.rcWork; // 工作区(排除任务栏)
        let bottom = pos.y + size.height as i32;
        if bottom > work.bottom {
            let new_y = (work.bottom - size.height as i32).max(work.top);
            let _ = win.set_position(PhysicalPosition::new(pos.x, new_y));
            tracing::debug!(
                old_y = pos.y, new_y, work_bottom = work.bottom, height = size.height,
                "窗口超出屏幕底部,上移"
            );
        }
    }
}

/// 打开设置窗口：已存在则聚焦，否则创建（无边框 + 透明 + 圆角）。
///
/// 统一入口：主窗口搜索结果和托盘菜单都走这里，避免重复代码漏配置。
pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    let win = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Blink Settings")
        .inner_size(960.0, 680.0)
        .min_inner_size(760.0, 520.0)
        .center()
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .background_color(tauri::window::Color(0, 0, 0, 0))
        .build()
        .expect("创建设置窗口失败");
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        strip_window_border(hwnd);
        enable_rounded_corners(hwnd);
    }
}
