//! Windows 平台特定的窗口控制实现：Win32 API。

use std::sync::atomic::{AtomicU64, AtomicU8, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use tokio::time::sleep;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{DwmExtendFrameIntoClientArea, DwmFlush, DwmSetWindowAttribute, DWMWA_WINDOW_CORNER_PREFERENCE};
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MonitorFromPoint, MonitorFromWindow, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, GetCursorPos, GetForegroundWindow, GetWindowLongPtrW,
    GetWindowThreadProcessId, IsIconic, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    GWLP_WNDPROC, GWL_STYLE, HWND_TOP, SET_WINDOW_POS_FLAGS, SWP_FRAMECHANGED, SWP_NOMOVE,
    SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SW_RESTORE, WNDPROC, WS_CAPTION, WS_THICKFRAME,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

const ST_HIDDEN: u8 = 0;
const ST_VISIBLE: u8 = 1;

/// 默认 grace period。
const DEFAULT_GRACE_MS: u64 = 500;

/// 唤起时的基准逻辑尺寸——用来在跨 DPI 屏定位时算出目标屏上的物理尺寸。
/// 与前端 `syncWindowSize()` 首帧一致（宽 700 / 高 65 含 CSS padding），
/// 避免"定位算 60、前端 resize 到 65"导致的 5px 视觉抖动。
const BASE_W_LOGICAL: f64 = 700.0;
const BASE_H_LOGICAL: f64 = 65.0;

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
        .try_state::<std::sync::Arc<std::sync::RwLock<crate::app::config::ContextConfig>>>()
        .map(|c| c.read().unwrap().clone())
        .unwrap_or_default();
    let snapshot = crate::infra::platform::context::collect(&context_cfg);
    tracing::debug!(
        foreground_app = ?snapshot.foreground_app.as_ref().map(|f| &f.process_name),
        window_title = ?snapshot.foreground_app.as_ref().map(|f| &f.window_title),
        "invoke: captured context"
    );

    // 2. 更新 SearchService 中的快照
    if let Some(search_service) = app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>() {
        search_service.update_snapshot(snapshot);
        // 选区来自划词监听缓存（鼠标划词黄金时机抓取），单独回填，不覆盖整份快照。
        // 不再在 show 后抓取——那会让 Electron 应用失焦退化选区（0.8.0 §1.1 实测）。
        if let Some(sel) = crate::infra::platform::selection::get_last_selection() {
            search_service.update_selected_text(Some(sel));
        }
    }

    let Some(win) = app.get_webview_window("main") else {
        return;
    };

    // 复位到基准尺寸（逻辑像素）——`launcher_position` 内部按目标屏 DPI 算
    // 物理尺寸做居中；跨 DPI 屏时 winit 在 set_position 后会响应 WM_DPICHANGED
    // 自动 rescale 尺寸，与我们算出的位置对齐。前端 syncWindowSize 首帧
    // 会立即再 resize 到真实内容高度，此步骤零可见成本。
    let _ = win.set_size(tauri::LogicalSize::new(BASE_W_LOGICAL, BASE_H_LOGICAL));

    if let Some(pos) = launcher_position(&win) {
        let _ = win.set_position(pos);
    }
    let now = elapsed_ms();
    let grace_ms = GRACE_MS.load(Ordering::SeqCst);
    INVOKE_AT.store(now, Ordering::SeqCst);
    STATE.store(ST_VISIBLE, Ordering::SeqCst);
    tracing::trace!(grace_ms, "invoke: state → VISIBLE, show + set_focus");
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit("blink://shown", ());
}

/// 隐藏：ESC / 看门狗 / 单实例重复启动。
/// 同时隐藏右键菜单窗口（保留窗口供下次复用）。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        tracing::debug!(reason, "hide: state → HIDDEN");
        let _ = win.hide();
        let _ = app.emit("blink://hidden", ());
    }
    // 主窗口隐藏时联动隐藏右键菜单（保留窗口供下次复用）
    if let Some(menu_win) = app.get_webview_window("context-menu") {
        let _ = menu_win.hide();
    }
}

/// 窗口焦点事件：仅在真正获焦时进入 Visible（启用看门狗）。
/// 失焦不在此时处理，交给看门狗轮询前台窗口。
pub fn on_focused(focused: bool) {
    let st = STATE.load(Ordering::SeqCst);
    tracing::trace!(focused, st, "on_focused");
    if focused {
        STATE.store(ST_VISIBLE, Ordering::SeqCst);
        tracing::trace!("on_focused: state → VISIBLE, watchdog armed");
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

/// 强制窗口置顶（HWND_TOPMOST）。Tauri 的 `show()` / `set_always_on_top()` 在
/// WebView2 窗口上不一定可靠恢复 z-order，直接走 Win32 更稳妥。
pub fn force_topmost(hwnd: HWND) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND(-1isize as *mut _)), // HWND_TOPMOST
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
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

/// 计算窗口在鼠标所在显示器上的位置：工作区中心居中（物理像素）。
///
/// 跟随鼠标所在屏（业界主流：Alfred / PowerToys Run 都这么做）——
/// 用户按热键前手在哪、窗口就在哪，无需感知"前台窗口在哪块屏"。
/// 天然规避 `GetForegroundWindow` 返回 NULL（切桌面 / 前台切换瞬态）时
/// `MonitorFromWindow(NULL, …)` 会误落到主屏的问题。
///
/// 用 `rcWork`（工作区，排除任务栏）而非 `rcMonitor`，与
/// `clamp_to_work_area` 行为一致：任务栏放屏顶部/侧边时也不会视觉偏移。
///
/// **跨 DPI 屏关键**：物理尺寸 **不能读 `outer_size()`**——它反映的是
/// 「窗口当前所在屏」的 DPI 换算结果，而我们要去的可能是另一块 DPI 不同的屏。
/// 一旦 `set_position` 把窗口移过去，Windows 发 `WM_DPICHANGED` 让 winit
/// 按目标屏 DPI **rescale 尺寸但不动位置**，就会视觉偏移。
/// 正确做法：`GetDpiForMonitor(目标屏) × 基准逻辑尺寸` 直接算目标屏物理尺寸，
/// 位置随之对齐——首次跨屏也一步到位。
fn launcher_position(_win: &WebviewWindow) -> Option<PhysicalPosition<i32>> {
    unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let hmon = if GetCursorPos(&mut pt).is_ok() {
            MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST)
        } else {
            // 极端 fallback：拿不到光标就落主屏
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        };

        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            let rc = mi.rcWork; // 工作区（排除任务栏），与 clamp_to_work_area 一致

            // 目标屏 DPI（EFFECTIVE）→ scale。取不到时按 96 DPI（100%）兜底。
            let mut dpi_x: u32 = 96;
            let mut dpi_y: u32 = 96;
            let _ = GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y);
            let scale = (dpi_x.max(96) as f64) / 96.0;
            let w = (BASE_W_LOGICAL * scale).round() as i32;
            let h = (BASE_H_LOGICAL * scale).round() as i32;

            let cx = rc.left + (rc.right - rc.left) / 2;
            let cy = rc.top + (rc.bottom - rc.top) / 2;
            tracing::trace!(
                cursor_x = pt.x, cursor_y = pt.y,
                mon_left = rc.left, mon_top = rc.top,
                mon_right = rc.right, mon_bottom = rc.bottom,
                dpi_x, w, h,
                "launcher_position: located on monitor under cursor"
            );
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
/// 显示 chord-ball 悬浮窗（0.8.5 §6.5 划词指示）。
/// 独立 webview 窗口，不抢焦点（WS_EX_NOACTIVATE），看门狗按 PID 判定天然豁免
/// （看门狗只 hide 主窗 "main"，不碰 "chord-ball"）。
pub fn show_chord_ball(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    const LABEL: &str = "chord-ball";
    // 球出现在鼠标附近（划词选区在鼠标处）
    let (mx, my) = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        (pt.x, pt.y)
    };
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.set_position(PhysicalPosition::new(mx + 16, my + 16));
        let _ = win.show();
        return Ok(());
    }
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chord-ball.html".into()))
        .title("")
        .inner_size(48.0, 48.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .build()
        .map_err(|e| e.to_string())?;
    let _ = win.set_position(PhysicalPosition::new(mx + 16, my + 16));
    if let Ok(hwnd) = win.hwnd() {
        apply_no_activate(HWND(hwnd.0 as _));
    }
    Ok(())
}

/// 给窗口加 WS_EX_NOACTIVATE——点击不激活，用户能回原应用选文本（划词必需）。
fn apply_no_activate(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, WS_EX_NOACTIVATE};
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
    }
}

pub fn hide_chord_ball(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("chord-ball") {
        let _ = win.hide();
    }
}

/// 显示截图覆盖窗（0.8.7 §九）。
///
/// **前置条件**：调用方已通过 `screenshot::begin_session()` 完成截屏，SESSION 中
/// 已有位图；`meta` 是该 session 的元数据（物理像素坐标 + 尺寸）。
///
/// 流程：构建 overlay → SetWindowPos 按物理像素强制定位（绕开 Tauri 逻辑像素接口）
/// → 前端通过 `blink-screenshot://capture` 协议只读 SESSION 拿 PNG。
/// 前端拿到图后先铺暗色蒙版，用户拖选才显示亮区；ESC / 失焦 / 确认走 command 层。
pub fn show_screenshot_overlay(
    app: &AppHandle,
    meta: crate::infra::platform::screenshot::ScreenCaptureMeta,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    const LABEL: &str = "chord-screenshot";

    // 复用已存在的窗口：先 eval 清屏 + 重定位 → show → 触发重新加载
    if let Some(win) = app.get_webview_window(LABEL) {
        // 先清屏再 show —— 否则窗口刚出来会看到上次结束时的选区/虚线框闪一下
        // （webview `.show()` 到 __blinkReloadScreenshot 执行之间有毫秒级空档）
        let _ = win.eval("window.__blinkReloadScreenshot && window.__blinkReloadScreenshot()");
        if let Ok(hwnd) = win.hwnd() {
            place_at_physical(HWND(hwnd.0 as _), meta.virtual_x, meta.virtual_y, meta.width, meta.height);
        }
        let _ = win.show();
        return Ok(());
    }

    // 首次构建：inner_size / position 会被后续 SetWindowPos 覆盖，这里只是让 Tauri 别报参数错。
    let win = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chord-screenshot.html".into()))
        .title("")
        .inner_size(meta.width as f64, meta.height as f64)
        .position(meta.virtual_x as f64, meta.virtual_y as f64)
        .decorations(false)
        .transparent(true) // 透明背景，让 canvas 上的桌面截图独占视觉
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(true)
        .build()
        .map_err(|e| e.to_string())?;

    if let Ok(hwnd) = win.hwnd() {
        place_at_physical(HWND(hwnd.0 as _), meta.virtual_x, meta.virtual_y, meta.width, meta.height);
    }

    Ok(())
}

/// 按物理像素强制定位窗口，覆盖 Tauri 逻辑像素接口的 DPI 缩放。
///
/// 截图 overlay 必须精确对齐虚拟屏幕物理像素——否则前端 canvas.width（物理像素）
/// 与窗口 CSS 尺寸的比值会与 DPR 失配，选区坐标全歪。
fn place_at_physical(hwnd: HWND, x: i32, y: i32, w: u32, h: u32) {
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            x,
            y,
            w as i32,
            h as i32,
            SET_WINDOW_POS_FLAGS(SWP_NOACTIVATE.0),
        );
    }
}

/// 隐藏截图覆盖窗 + 清空 SESSION（释放位图内存）。
pub fn hide_screenshot_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("chord-screenshot") {
        let _ = win.hide();
    }
    crate::infra::platform::screenshot::end_session();
}

/// 截图专用：**瞬间**隐藏主窗（DWM Cloak + hide），零 fade 动画。
///
/// **和 `hide()` 的区别**：
/// - `hide()` 走 `ShowWindow(SW_HIDE)`，触发 Windows 11 系统级 fade-out（~200ms 视觉延迟）
/// - `hide_for_screenshot()` 先 `DwmSetWindowAttribute(DWMWA_CLOAK, TRUE)` 让 DWM
///   **立即**从合成里剔除窗口（无动画），再调 `ShowWindow(SW_HIDE)` 落 Win32 状态
///
/// Cloak 是任务视图/Alt-Tab 预览用的机制，DWM 层瞬间"雾化"窗口——远快于走 fade。
///
/// 调用侧应在截图完成后（成功或取消）调 `unhide_after_screenshot` 撤销 cloak，
/// 否则下次 `show()` 出来的窗口是不可见的。
pub fn hide_for_screenshot(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        if let Ok(hwnd) = win.hwnd() {
            let hwnd = HWND(hwnd.0 as _);
            unsafe {
                let cloak: i32 = 1;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    windows::Win32::Graphics::Dwm::DWMWA_CLOAK,
                    &cloak as *const _ as *const _,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
        let _ = win.hide();
        let _ = app.emit("blink://hidden", ());
    }
    // 联动隐藏右键菜单（保留窗口供下次复用）
    if let Some(menu_win) = app.get_webview_window("context-menu") {
        let _ = menu_win.hide();
    }
}

/// 撤销 `hide_for_screenshot` 的 cloak 标志。
///
/// 只清 cloak，不 `show`——主窗此时仍应保持 hidden 状态（截图完成后主窗不该出来）。
/// 下次 `invoke()` 时 `show()` 会正常工作。
pub fn unhide_after_screenshot(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("main") {
        if let Ok(hwnd) = win.hwnd() {
            let hwnd = HWND(hwnd.0 as _);
            unsafe {
                let cloak: i32 = 0;
                let _ = DwmSetWindowAttribute(
                    hwnd,
                    windows::Win32::Graphics::Dwm::DWMWA_CLOAK,
                    &cloak as *const _ as *const _,
                    std::mem::size_of::<i32>() as u32,
                );
            }
        }
    }
}

/// 等主窗真正从桌面上消失（截图前调用，防"BitBlt 拍到主窗"）。
///
/// 配 `hide_for_screenshot()` 使用时无需等 fade 动画——cloak 是瞬时的，只需要一次
/// DwmFlush 保证 DWM 完成一帧新合成（不含主窗）即可。
///
/// 调用侧应保证跑在 blocking 线程（tokio `spawn_blocking`），DwmFlush 是同步阻塞。
pub fn wait_frame_after_hide(app: &AppHandle) {
    use std::time::Instant;
    use windows::Win32::UI::WindowsAndMessaging::IsWindowVisible;

    let t0 = Instant::now();

    // DwmFlush x 1：cloak 后瞬时生效，一次 flush 保证 DWM 完成不含主窗的新合成。
    // 0.8.8 优化：从 2 次减到 1 次，实测截图无残影，省 ~10ms。
    unsafe {
        let _ = DwmFlush();
    }
    let t_flush = t0.elapsed();

    // 轮询 IsWindowVisible —— cloak + hide 后立刻就是 false，这里主要作日志用
    let hwnd = app
        .get_webview_window("main")
        .and_then(|w| w.hwnd().ok())
        .map(|h| HWND(h.0 as _));
    let mut polled_ms = 0u64;
    let mut visible_final = None;
    if let Some(hwnd) = hwnd {
        loop {
            let visible = unsafe { IsWindowVisible(hwnd).as_bool() };
            visible_final = Some(visible);
            if !visible || polled_ms >= 100 {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(8));
            polled_ms += 8;
        }
    }

    tracing::debug!(
        flush_ms = t_flush.as_millis() as u64,
        poll_ms = polled_ms,
        total_ms = t0.elapsed().as_millis() as u64,
        visible_final = ?visible_final,
        "wait_frame_after_hide 完成"
    );
}

/// 后台预热次级窗口：延迟创建 chord-ball / chord-screenshot / context-menu 并立即隐藏。
///
/// WebView2 首次建实例 300~400ms，预热后 show 只是切可见性 (<50ms)。
/// 代价：常驻内存 +10~20MB × 3；收益：Alt+A / 右键菜单 / 悬浮球首次触发无感。
pub fn preheat_secondary_windows(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 等主窗稳定 + 前端加载完毕，不与启动路径抢资源
        tokio::time::sleep(Duration::from_secs(3)).await;
        tracing::debug!("preheat: 开始预热次级窗口");

        // --- chord-ball（悬浮球，48×48 透明无焦点） ---
        if app.get_webview_window("chord-ball").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "chord-ball", WebviewUrl::App("chord-ball.html".into()))
                .title("")
                .inner_size(48.0, 48.0)
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .focused(false)
                .visible(false)
                .build()
            {
                Ok(win) => {
                    if let Ok(hwnd) = win.hwnd() {
                        apply_no_activate(HWND(hwnd.0 as _));
                    }
                    tracing::debug!("preheat: chord-ball ✓");
                }
                Err(e) => tracing::warn!(error = %e, "preheat: chord-ball 失败"),
            }
        }

        // --- chord-screenshot（截图 overlay，透明全屏层） ---
        if app.get_webview_window("chord-screenshot").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "chord-screenshot", WebviewUrl::App("chord-screenshot.html".into()))
                .title("")
                .inner_size(1920.0, 1080.0) // 默认尺寸，实际使用时 place_at_physical 会覆盖
                .decorations(false)
                .transparent(true)
                .always_on_top(true)
                .skip_taskbar(true)
                .shadow(false)
                .focused(false)
                .visible(false)
                .build()
            {
                Ok(_) => tracing::debug!("preheat: chord-screenshot ✓"),
                Err(e) => tracing::warn!(error = %e, "preheat: chord-screenshot 失败"),
            }
        }

        // --- context-menu（右键菜单，非透明小窗） ---
        if app.get_webview_window("context-menu").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "context-menu", WebviewUrl::App("contextmenu-popup.html".into()))
                .title("")
                .inner_size(200.0, 200.0) // 默认尺寸，实际使用时会 resize
                .decorations(false)
                .transparent(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .resizable(false)
                .visible(false)
                .build()
            {
                Ok(win) => {
                    if let Ok(hwnd) = win.hwnd() {
                        force_topmost(HWND(hwnd.0 as _));
                    }
                    tracing::debug!("preheat: context-menu ✓");
                }
                Err(e) => tracing::warn!(error = %e, "preheat: context-menu 失败"),
            }
        }

        tracing::debug!("preheat: 预热完成");
    });
}

pub fn open_settings(app: &AppHandle) {
    if let Some(w) = app.get_webview_window("settings") {
        // 如果窗口已最小化，先恢复
        if let Ok(hwnd) = w.hwnd() {
            let hwnd = HWND(hwnd.0 as _);
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
        }
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
