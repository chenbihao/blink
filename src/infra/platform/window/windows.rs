//! Windows 平台特定的窗口控制实现：Win32 API。

use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::domain::event_names::EventNames;
use tauri::{AppHandle, Emitter, Manager, PhysicalPosition, WebviewWindow};
use tokio::time::sleep;
use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAK, DWMWA_WINDOW_CORNER_PREFERENCE, DwmExtendFrameIntoClientArea, DwmFlush,
    DwmSetWindowAttribute,
};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Controls::MARGINS;
use windows::Win32::UI::WindowsAndMessaging::{
    CallWindowProcW, DefWindowProcW, GWL_STYLE, GWLP_WNDPROC, GetCursorPos, GetForegroundWindow,
    GetWindowLongPtrW, GetWindowThreadProcessId, HWND_TOP, IsIconic, SET_WINDOW_POS_FLAGS,
    SW_RESTORE, SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER,
    SetWindowLongPtrW, SetWindowPos, ShowWindow, WNDPROC, WS_CAPTION, WS_THICKFRAME,
};

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
/// Blink 主窗口抢焦点前的外部前台窗口，截图/chord 后续需要恢复或驱动原应用时使用。
static LAST_EXTERNAL_HWND: AtomicIsize = AtomicIsize::new(0);

/// 0.16.11：应用退出标志。
///
/// 在 `RunEvent::Exit` 时设为 true，便签窗口的 `CloseRequested` handler 据此区分
/// 「用户关闭单条便签」与「应用整体退出」——退出时不把 visible 改成 false，
/// 只隐藏窗口，保证下次启动按原 visible 状态恢复。
static IS_APP_EXITING: AtomicBool = AtomicBool::new(false);

/// 程序启动以来的毫秒数（单调时钟，用于 grace period 计算）。
fn elapsed_ms() -> u64 {
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

/// 唤起：采集上下文快照 → 定位 → show → set_focus → 通知前端。
///
/// **采集时机很重要**：必须在 show() 之前调用，否则拿到的前台是 Blink 自己。
pub fn invoke(app: &AppHandle) {
    let t0 = std::time::Instant::now();

    // 1. 先采集上下文快照（show 之前！）
    //    读内存 ContextConfig（零 IO，热键回调不能 await），按配置过滤采集
    let context_cfg = app
        .try_state::<std::sync::Arc<std::sync::RwLock<crate::domain::config::ContextConfig>>>()
        .map(|c| c.read().unwrap().clone())
        .unwrap_or_default();
    let snapshot = crate::infra::platform::context::collect(&context_cfg);
    if let Some(hwnd) = snapshot
        .foreground_app
        .as_ref()
        .map(|foreground| foreground.hwnd)
        .filter(|hwnd| *hwnd != 0)
    {
        LAST_EXTERNAL_HWND.store(hwnd, Ordering::SeqCst);
    }
    tracing::debug!(
        foreground_app = ?snapshot.foreground_app.as_ref().map(|f| &f.process_name),
        window_title = ?snapshot.foreground_app.as_ref().map(|f| &f.window_title),
        "invoke: captured context"
    );

    // 2. 更新 SearchService 中的快照
    //
    // 选区抓取采用「快速捕获 + 慢速异步提取」模式：
    // - show() 之前：capture_focused_element() 仅做 GetFocusedElement()（O(1)，<5ms）
    // - show() 之后：spawn 线程做三段式 TextPattern 提取（可能 100-500ms）
    // 这样窗口显示不被 UIA 阻塞——慢应用上用户不再感到"卡一下才出来"。
    //
    // 提取完成后通过 update_selected_text 回填 + emit awareness-updated 触发前端 retrigger。
    let focused_element = if context_cfg.selection_enabled {
        let t_capture = std::time::Instant::now();
        let focused = snapshot
            .foreground_app
            .as_ref()
            .filter(|fg| fg.hwnd != 0)
            .and_then(|_| crate::infra::platform::selection::capture_focused_element());
        tracing::debug!(
            target: "perf",
            capture_ms = t_capture.elapsed().as_millis(),
            has_element = focused.is_some(),
            "[perf] invoke: capture_focused_element (before show)"
        );
        focused
    } else {
        None
    };

    if let Some(search_service) =
        app.try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
    {
        search_service.update_snapshot(snapshot.clone());
    }

    let Some(win) = app.get_webview_window("main") else {
        return;
    };

    let t_show = std::time::Instant::now();
    let _ = win.set_size(tauri::LogicalSize::new(BASE_W_LOGICAL, BASE_H_LOGICAL));

    if let Some(pos) = launcher_position(&win) {
        let _ = win.set_position(pos);
    }
    let now = elapsed_ms();
    let grace_ms = GRACE_MS.load(Ordering::SeqCst);
    INVOKE_AT.store(now, Ordering::SeqCst);
    STATE.store(ST_VISIBLE, Ordering::SeqCst);
    tracing::trace!(grace_ms, "invoke: state → VISIBLE, show + set_focus");
    crate::infra::platform::hotkey::expect_synthesized_alt_keyup();
    let _ = win.show();
    let _ = win.set_focus();
    let _ = app.emit(EventNames::SHOWN, ());
    tracing::debug!(
        target: "perf",
        show_ms = t_show.elapsed().as_millis(),
        total_ms = t0.elapsed().as_millis(),
        "[perf] invoke: show+focus+emit (TOTAL)"
    );

    // 3. show 之后：异步提取选区（不阻塞窗口显示）
    //
    // focused_element 在 show() 之前通过 GetFocusedElement() 捕获，
    // 此时焦点还在原应用上。show() 之后焦点已移到 Blink，但捕获的 COM 元素
    // 仍然指向原应用的焦点控件——MTA 公寓下 COM 接口跨线程安全。
    //
    // 提取完成后回填 SearchService 快照 + emit awareness-updated 触发前端 retrigger，
    // 让翻译 Ghost 等依赖选区的建议在选区就绪后自动出现。
    if let Some(focused) = focused_element {
        let search_service = app
            .try_state::<std::sync::Arc<crate::domain::search::SearchService>>()
            .map(|s| s.inner().clone());
        let app_clone = app.clone();
        std::thread::spawn(move || {
            let t_extract = std::time::Instant::now();
            let grabbed =
                crate::infra::platform::selection::extract_selection_from_element(&focused)
                    .or_else(|| {
                        // UIA 未命中，回退鼠标钩子缓存
                        let cached = crate::infra::platform::selection::get_last_selection();
                        if cached.is_some() {
                            tracing::trace!("invoke: 回退到鼠标钩子选区缓存");
                        }
                        cached.map(|(text, _)| text)
                    });
            let hit = grabbed.is_some();
            if let Some(ref text) = grabbed {
                tracing::debug!(len = text.chars().count(), "invoke: UIA 异步抓取选区成功");
            }
            if let Some(ss) = search_service {
                ss.update_selected_text(grabbed, None);
                // 通知前端重跑搜索——选区可能刚到，翻译 Ghost 等建议需要更新
                // 仅在窗口仍可见时 emit（用户可能已 ESC 关闭）
                if crate::infra::platform::window::is_visible() {
                    let _ = app_clone.emit(EventNames::AWARENESS_UPDATED, ());
                }
            }
            tracing::debug!(
                target: "perf",
                extract_ms = t_extract.elapsed().as_millis(),
                hit,
                "[perf] invoke: async UIA extraction (after show)"
            );
        });
    }
}

/// 隐藏：ESC / 看门狗 / 单实例重复启动。
/// 同时隐藏右键菜单窗口（保留窗口供下次复用）。
pub fn hide(app: &AppHandle, reason: &str) {
    if let Some(win) = app.get_webview_window("main") {
        STATE.store(ST_HIDDEN, Ordering::SeqCst);
        tracing::debug!(reason, "hide: state → HIDDEN");
        let _ = win.hide();
        let _ = app.emit(EventNames::HIDDEN, ());
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
///
/// **0.15 hotfix**：扩展为同时监视 `chord-screenshot` overlay 窗口。
/// 截图 overlay 是 `always_on_top` 全屏透明窗，前端 JS 失败/卡住时用户无法 ESC 退出、
/// 无法唤起任务管理器（被 overlay 遮挡）。watchdog 在 overlay 可见且前台非本进程时
/// 自动隐藏 overlay，提供后端兜底逃生通道。
pub fn start_watchdog(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        loop {
            sleep(Duration::from_millis(150)).await;

            // ── 主窗口失焦检测（原有逻辑）──────────────────────────
            if STATE.load(Ordering::SeqCst) == ST_VISIBLE {
                let grace_ms = GRACE_MS.load(Ordering::SeqCst);
                let since_invoke = elapsed_ms() - INVOKE_AT.load(Ordering::SeqCst);
                if since_invoke >= grace_ms {
                    let fg = unsafe { GetForegroundWindow() };
                    // fg == NULL:焦点真空(系统正在切换前台窗口的瞬态,如刚拉起子进程时)。
                    // 这不代表用户切到了别的窗口,据此隐藏会误伤——跳过本轮,等下次轮询。
                    if !fg.0.is_null() && !is_self_foreground(&app, fg) {
                        tracing::info!(since_invoke, "watchdog: hide! fg=0x{:x}", fg.0 as isize);
                        hide(&app, "watchdog");
                    }
                }
            }

            // ── 截图 overlay 失焦检测（0.15 hotfix）─────────────────
            // overlay 可见 + 前台非本进程 → 自动隐藏。
            // 覆盖场景：用户 Ctrl+Shift+Esc 唤起任务管理器、Alt+Tab 切窗口、
            // 或前端 JS 模块加载失败导致 blur handler 未注册。
            if let Some(ss_win) = app.get_webview_window("chord-screenshot") {
                if ss_win.is_visible().unwrap_or(false) {
                    let fg = unsafe { GetForegroundWindow() };
                    if !fg.0.is_null() && !is_self_foreground(&app, fg) {
                        tracing::info!(
                            "watchdog: screenshot overlay hide! fg=0x{:x}",
                            fg.0 as isize
                        );
                        // 用 hide_screenshot_overlay 而非 win.hide()，
                        // 确保同时清空 SESSION 释放位图内存
                        hide_screenshot_overlay(&app);
                    }
                }
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

/// 各窗口的原始窗口过程（0.12.4 §6.6：从 OnceLock 改为 HashMap 支持多窗口）。
/// key = HWND 指针值（isize），value = 原始 WndProc 地址。
static ORIGINAL_WNDPROCS: std::sync::Mutex<Option<std::collections::HashMap<isize, isize>>> =
    std::sync::Mutex::new(None);

/// 拦截 Alt+Space 系统菜单（替换窗口过程，吞掉 SC_KEYMENU）。
/// 主窗口和 chat 窗口虽无边框仍响应 Alt+Space 弹出移动/最大化菜单，
/// 前端 preventDefault 与去 WS_SYSMENU 都无效，
/// 只能在窗口过程层拦截 WM_SYSCOMMAND。
/// 0.12.4 §6.6：支持多窗口安装（HashMap 按 HWND 存储 original wndproc）。
pub fn install_sysmenu_blocker(hwnd: HWND) {
    unsafe {
        // 检查是否已安装——避免重复 SetWindowLongPtrW 返回 sysmenu_block_proc 自身，
        // 导致 CallWindowProcW(sysmenu_block_proc, ...) 无限递归 → stack overflow。
        // （0.12.5 修复：此前注释称"重复安装安全"是错误的）
        let already_installed = ORIGINAL_WNDPROCS
            .lock()
            .unwrap()
            .as_ref()
            .map(|m| m.contains_key(&(hwnd.0 as isize)))
            .unwrap_or(false);
        if already_installed {
            return;
        }

        let original = SetWindowLongPtrW(
            hwnd,
            GWLP_WNDPROC,
            sysmenu_block_proc as *const () as usize as isize,
        );
        let mut map = ORIGINAL_WNDPROCS.lock().unwrap();
        map.get_or_insert_with(std::collections::HashMap::new)
            .insert(hwnd.0 as isize, original);
        tracing::debug!(
            hwnd = hwnd.0 as isize,
            original_wndproc = original,
            "install_sysmenu_blocker: 已安装系统菜单拦截器"
        );
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
    let original = ORIGINAL_WNDPROCS
        .lock()
        .unwrap()
        .as_ref()
        .and_then(|m| m.get(&(hwnd.0 as isize)))
        .copied()
        .unwrap_or(0);
    if original != 0 {
        // edition 2024：unsafe fn 内的 unsafe 操作需显式 unsafe block
        unsafe {
            let proc: WNDPROC = std::mem::transmute::<isize, WNDPROC>(original);
            CallWindowProcW(proc, hwnd, msg, wparam, lparam)
        }
    } else {
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
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

            // 0.11.9：走公共 DPI helper
            let dpi_x = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
            let w = crate::infra::platform::dpi::logical_to_physical(BASE_W_LOGICAL, dpi_x);
            let h = crate::infra::platform::dpi::logical_to_physical(BASE_H_LOGICAL, dpi_x);

            let cx = rc.left + (rc.right - rc.left) / 2;
            let cy = rc.top + (rc.bottom - rc.top) / 2;
            tracing::trace!(
                cursor_x = pt.x,
                cursor_y = pt.y,
                mon_left = rc.left,
                mon_top = rc.top,
                mon_right = rc.right,
                mon_bottom = rc.bottom,
                dpi_x,
                w,
                h,
                "launcher_position: located on monitor under cursor"
            );
            return Some(PhysicalPosition::new(cx - w / 2, cy - h / 2));
        }
    }
    None
}

/// resize 后若窗口底部超出显示器工作区，向上移动使其完整可见。
pub fn clamp_to_work_area(win: &WebviewWindow) {
    let Ok(pos) = win.outer_position() else {
        return;
    };
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
                old_y = pos.y,
                new_y,
                work_bottom = work.bottom,
                height = size.height,
                "窗口超出屏幕底部,上移"
            );
        }
    }
}

/// 右键菜单多屏感知定位：直接用 Win32 `GetCursorPos` 拿光标**物理坐标**，
/// 找到目标显示器，按其 DPI 把 CSS 尺寸换算成物理尺寸 + 工作区 clamp。
///
/// **不接受前端的 x/y**：`MouseEvent.screenX/Y` 在 WebView2 里是 **CSS 像素**，
/// 高 DPI 屏（如 150%）直接当物理像素用会偏 1/3 位置；多屏跨 DPI 更乱。
/// 光标物理坐标由 Win32 直接给，绕过所有浏览器坐标系猜谜。
///
/// 返回值 `(x, y, width, height)` 均为**物理像素**，可直接传给 `PhysicalSize` / `PhysicalPosition`。
///
/// - `css_w/h`：菜单的 CSS 像素尺寸（前端估算值，会按目标屏 DPI 缩放）
pub fn clamp_context_menu(css_w: f64, css_h: f64) -> (i32, i32, u32, u32) {
    unsafe {
        // 光标物理坐标（进程需 DPI-aware，Tauri 默认 PerMonitorV2 已满足）
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        let screen_x = pt.x;
        let screen_y = pt.y;
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);

        // 获取目标显示器工作区（排除任务栏）
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            // fallback：拿不到就用主屏
            let hmon_primary = MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY);
            let mut mi2: MONITORINFO = std::mem::zeroed();
            mi2.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
            if GetMonitorInfoW(hmon_primary, &mut mi2).as_bool() {
                mi2.rcWork
            } else {
                // 极端兜底：返回原坐标原尺寸
                return (screen_x, screen_y, css_w as u32, css_h as u32);
            }
        };

        // 0.11.9：走公共 DPI helper
        let dpi_x = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
        let scale = crate::infra::platform::dpi::scale_factor(dpi_x);

        // CSS 像寸 → 物理像素
        let phys_w = (css_w * scale).round() as i32;
        let phys_h = (css_h * scale).round() as i32;

        // 智能翻转：右/下空间不够时，菜单显示在光标左/上方（老 0.5.3+ 前端行为）
        //   贴边 clamp 会让菜单紧贴屏幕右/下边缘，视觉上像是"卡住"了。
        let margin = 4;
        let prefer_x = if screen_x + phys_w + margin > work.right {
            (screen_x - phys_w).max(work.left + margin)
        } else {
            screen_x
        };
        let prefer_y = if screen_y + phys_h + margin > work.bottom {
            (screen_y - phys_h).max(work.top + margin)
        } else {
            screen_y
        };
        // 再做一次工作区 clamp（防单块屏幕比菜单还小的极端情况）
        let max_x = work.right - phys_w - margin;
        let max_y = work.bottom - phys_h - margin;
        let x = prefer_x.clamp(work.left + margin, max_x.max(work.left + margin));
        let y = prefer_y.clamp(work.top + margin, max_y.max(work.top + margin));

        tracing::trace!(
            screen_x,
            screen_y,
            css_w,
            css_h,
            dpi = dpi_x,
            scale,
            phys_w,
            phys_h,
            work_left = work.left,
            work_top = work.top,
            work_right = work.right,
            work_bottom = work.bottom,
            final_x = x,
            final_y = y,
            "clamp_context_menu: 多屏定位"
        );

        (x, y, phys_w as u32, phys_h as u32)
    }
}

/// 给窗口加 WS_EX_NOACTIVATE——点击不激活，用户能回原应用选文本。
/// 被 voice-overlay / chord-screenshot 等次级窗口复用（划词场景已移除，但
/// WS_EX_NOACTIVATE 对不抢焦点的 overlay 窗口通用）。
fn apply_no_activate(hwnd: HWND) {
    use windows::Win32::UI::WindowsAndMessaging::{GWL_EXSTYLE, WS_EX_NOACTIVATE};
    unsafe {
        let ex = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        let _ = SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex | WS_EX_NOACTIVATE.0 as isize);
    }
}

/// 获取当前前台窗口的 HWND（供 G2 注入前恢复焦点用）。
pub fn get_foreground_hwnd() -> Option<isize> {
    unsafe {
        let hwnd = windows::Win32::UI::WindowsAndMessaging::GetForegroundWindow();
        if hwnd.is_invalid() {
            None
        } else {
            Some(hwnd.0 as isize)
        }
    }
}

/// 返回 Blink 最近一次唤起、尚未抢焦点时记录的外部前台窗口。
pub fn last_external_foreground_hwnd() -> Option<isize> {
    let hwnd = LAST_EXTERNAL_HWND.load(Ordering::SeqCst);
    (hwnd != 0).then_some(hwnd)
}

/// 恢复前台窗口焦点（G2 注入文本前调用）。
///
/// Alt+Space 唤起 Blink 时，组合键到达前台应用会弹出系统菜单（Alt+Space 的系统行为），
/// 导致焦点从文本输入框漂移到系统菜单。本函数负责在注入前修复焦点：
///
/// 1. **WM_CANCELMODE**：关闭 Alt+Space 弹出的系统菜单（DefWindowProc → EndMenu），
///    无副作用（不像 ESC 会关对话框/清输入）。
/// 2. **AttachThreadInput + SetForegroundWindow**：恢复前台窗口，绕过 Windows 前台锁定。
///    不使用 Alt 欺骗——合成 Alt keydown 会被目标应用接收，在 Electron/Chromium 上
///    可能激活菜单栏，反而干扰焦点。
/// 3. **UIA SetFocus**：关闭系统菜单后 Windows 自动恢复焦点到弹出前的控件，
///    但不保证可靠。用 UIA `GetFocusedElement` + `SetFocus` 保险——如果焦点恢复后
///    的控件是文本输入框（Edit/Document），主动 SetFocus 确保焦点到位。
///
/// > **不吞键时 Alt+Space 只触发系统菜单，不触发 Alt tap 菜单栏激活**——
/// > 因为 Alt keydown→keyup 之间有 Space 到达应用，Windows 不判定为 Alt tap。
/// > 所以只需关闭系统菜单，不需要处理 Chromium 菜单栏。
pub fn restore_foreground(hwnd: isize) {
    use windows::Win32::Foundation::{LPARAM, WPARAM};
    use windows::Win32::System::Threading::{AttachThreadInput, GetCurrentThreadId};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowThreadProcessId, PostMessageW, SetForegroundWindow, WM_CANCELMODE,
    };

    let target_hwnd = HWND(hwnd as *mut _);
    if target_hwnd.is_invalid() {
        return;
    }

    unsafe {
        // 1. 关闭 Alt+Space 弹出的系统菜单（异步投递，无副作用）
        let _ = PostMessageW(Some(target_hwnd), WM_CANCELMODE, WPARAM(0), LPARAM(0));

        // 2. AttachThreadInput + SetForegroundWindow（恢复前台窗口，不使用 Alt 欺骗）
        // 通知 hotkey：若 Alt 正按住则设 flag 跳过合成 Alt keyup（RDP 场景必需）
        crate::infra::platform::hotkey::expect_synthesized_alt_keyup();
        let current_tid = GetCurrentThreadId();
        let mut target_pid: u32 = 0;
        let target_tid = GetWindowThreadProcessId(target_hwnd, Some(&mut target_pid));

        if target_tid != 0 && target_tid != current_tid {
            let attached = AttachThreadInput(current_tid, target_tid, true);
            let _ = SetForegroundWindow(target_hwnd);
            if attached.as_bool() {
                let _ = AttachThreadInput(current_tid, target_tid, false);
            }
        } else {
            let _ = SetForegroundWindow(target_hwnd);
        }
    }

    // 3. UIA 焦点恢复（保险）：关闭系统菜单后 Windows 自动恢复焦点，
    //    但不保证可靠。用 UIA GetFocusedElement + SetFocus 主动恢复。
    //    等 50ms 让菜单关闭 + 焦点自动恢复完成，再检查。
    std::thread::sleep(std::time::Duration::from_millis(50));

    if let Some(elem) = crate::infra::platform::uia::get_focused_element() {
        // 焦点已恢复到某个元素——如果它是文本输入控件，主动 SetFocus 确保到位
        let ct = unsafe { elem.CurrentControlType() }
            .map(|t| t.0)
            .unwrap_or(0);
        if crate::infra::platform::uia::is_text_input_control(ct) {
            tracing::debug!(
                control_type = ct,
                "restore_foreground: 焦点在文本输入控件，SetFocus"
            );
            let _ = crate::infra::platform::uia::set_focused_element(&elem);
        } else {
            tracing::debug!(
                control_type = ct,
                "restore_foreground: 焦点不在文本输入控件，不强制 SetFocus"
            );
        }
    } else {
        tracing::debug!(
            "restore_foreground: GetFocusedElement 返回 None（UIA 不可用或无前台窗口）"
        );
    }
}

/// 显示独立 AI 对话窗口（0.12.1）。
///
/// 与 voice-overlay 不同：对话窗口需要接收键盘输入，因此不加 `WS_EX_NOACTIVATE`；
/// 首次运行时创建，后续复用同一 WebView，避免重复窗口和状态分裂。
///
/// **生命周期**（Phase 3A）：点击关闭→隐藏不销毁；隐藏先 abort active request。
/// CloseRequested handler 只注册一次的标记。
static CHAT_CLOSE_HANDLER_REGISTERED: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub fn show_chat_window(app: &AppHandle, initial_text: Option<&str>) -> Result<(), String> {
    use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

    const LABEL: &str = "chat";
    let is_new = app.get_webview_window(LABEL).is_none();

    let win = if is_new {
        // 首次创建（预热未命中时的 fallback）
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chat.html".into()))
            .title("Blink AI")
            .inner_size(900.0, 680.0)
            .min_inner_size(560.0, 420.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(true)
            .visible(true)
            .center()
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "chat window: 创建失败");
                format!("创建 chat 窗口失败: {e}")
            })?
    } else {
        // 复用预热窗口
        app.get_webview_window(LABEL).unwrap()
    };

    // 0.12.4 §6.6：安装系统菜单拦截器 + 圆角（与主窗口一致）
    // install_sysmenu_blocker 内部按 HWND 去重，重复调用安全（0.12.5 修复递归 BUG）
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = windows::Win32::Foundation::HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // CloseRequested handler：只注册一次（预热窗口复用时不会重复注册）
    if !CHAT_CLOSE_HANDLER_REGISTERED.swap(true, std::sync::atomic::Ordering::Relaxed) {
        let app_clone = app.clone();
        win.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                api.prevent_close();
                if let Some(cs) = app_clone
                    .try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
                {
                    cs.abort_active();
                }
                if let Some(w) = app_clone.get_webview_window("chat") {
                    let _ = w.hide();
                }
                tracing::debug!("chat window: CloseRequested → prevent_close + hide");
            }
        });
    }

    // 每次显示时居中到当前屏幕（与主窗口行为一致）
    let _ = win.center();
    win.show().map_err(|e| format!("显示 chat 窗口失败: {e}"))?;
    let _ = win.unminimize();
    win.set_focus()
        .map_err(|e| format!("聚焦 chat 窗口失败: {e}"))?;

    // 0.16.2：带初始文本时 emit chat-prefill 事件，前端监听后填充输入框（仅填充不发送）。
    // 预热窗口（常见路径）JS init 已完成，listener 在线，emit 立即收到。
    // 新建窗口（冷启动 fallback）JS init 有延迟，emit 可能在 listener 注册前发出 --
    // 前端 main.js 在 init 时额外检查 window.__chatPendingPrefill 兜底（由 emit_to 写入）。
    if let Some(text) = initial_text.filter(|s| !s.is_empty()) {
        if let Err(e) = app.emit_to(LABEL, crate::domain::event_names::EventNames::CHAT_PREFILL, text) {
            tracing::warn!(error = %e, "chat-prefill emit 失败");
        }
    }

    tracing::info!("chat window: 已显示");
    Ok(())
}

/// 隐藏 chat 窗口（Phase 3A）。
///
/// 先中止 active request，再隐藏窗口。若窗口不存在则 no-op。
pub fn hide_chat_window(app: &AppHandle) {
    // 先 abort active request
    if let Some(cs) =
        app.try_state::<std::sync::Arc<crate::domain::ai::chat_service::ChatService>>()
    {
        cs.abort_active();
    }
    // 再隐藏窗口
    if let Some(win) = app.get_webview_window("chat") {
        let _ = win.hide();
        tracing::debug!("chat window: 已隐藏");
    }
}

/// 显示内容编辑器窗口（0.16.3）。
///
/// 独立 Tauri 窗口，按需创建（不预热）。窗口关闭即销毁，不 prevent_close。
/// 看门狗按 PID 判定，前台切到编辑器时主窗不会被误隐藏。
/// payload 经 PendingEditorPayload State 中转，前端 init 时调 get_content_editor_payload 拉取。
pub fn show_content_editor_window(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    const LABEL: &str = "content-editor";
    let is_new = app.get_webview_window(LABEL).is_none();

    let win = if is_new {
        // 0.16.13 fix：改回 .visible(true) + background_color 消除白屏闪烁。
        // 之前的 .visible(false) + 前端 init 调 win.show() 方案在首次点击时
        // 因 WebView2 冷启动加载 JS 模块耗时，窗口长时间不可见，用户感知为「没反应」。
        // background_color 设为 dark 主题底色 #1e1e2e，CSS 加载前不闪白。
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("content-editor.html".into()))
            .title("编辑内容")
            .inner_size(720.0, 560.0)
            .min_inner_size(400.0, 300.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(true)
            .visible(true)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "content-editor window: 创建失败");
                format!("创建编辑器窗口失败: {e}")
            })?
    } else {
        // 复用已有窗口——前端需重新拉取 payload
        let win = app.get_webview_window(LABEL).unwrap();
        let _ = win.eval("window.__contentEditorReload && window.__contentEditorReload()");
        win
    };

    // 系统菜单拦截 + 圆角（与 chat 窗口一致）
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // 复用窗口可能被 hide 了，需要重新 show；新窗口已 visible(true) 创建
    if !is_new {
        win.show().map_err(|e| format!("显示编辑器窗口失败: {e}"))?;
    }
    let _ = win.unminimize();
    win.set_focus()
        .map_err(|e| format!("聚焦编辑器窗口失败: {e}"))?;

    tracing::info!("content-editor window: 已显示");
    Ok(())
}

/// 显示便签管理窗口（0.16.10）。
///
/// 独立 Tauri 窗口，label 为 `sticky-manager`。按需创建（不预热）。
/// 窗口关闭即销毁，不 prevent_close。
/// 看门狗按 PID 判定，前台切到管理窗口时主窗不会被误隐藏。
pub fn show_sticky_manager_window(app: &AppHandle) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    const LABEL: &str = "sticky-manager";
    let is_new = app.get_webview_window(LABEL).is_none();

    let win = if is_new {
        // 0.16.13 fix：改回 .visible(true) + background_color 消除白屏闪烁。
        // 之前的 .visible(false) + 前端 init 调 win.show() 方案在首次点击时
        // 因 WebView2 冷启动加载 JS 模块耗时，窗口长时间不可见，用户感知为「没反应」。
        // background_color 设为 dark 主题底色 #1e1e2e，CSS 加载前不闪白。
        // 同时注册 prevent_close + hide，窗口复用而非销毁重建。
        let w = WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("sticky-manager.html".into()))
            .title("便签管理")
            .inner_size(560.0, 640.0)
            .min_inner_size(360.0, 400.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(true)
            .visible(true)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "sticky-manager window: 创建失败");
                format!("创建便签管理窗口失败: {e}")
            })?;

        // prevent_close + hide——与 chat/content-editor 一致的复用模式
        let app_clone = app.clone();
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if IS_APP_EXITING.load(Ordering::SeqCst) {
                    return; // 应用退出：不 prevent_close
                }
                api.prevent_close();
                if let Some(w) = app_clone.get_webview_window(LABEL) {
                    let _ = w.hide();
                }
                tracing::debug!("sticky-manager window: CloseRequested → prevent_close + hide");
            }
        });
        w
    } else {
        let win = app.get_webview_window(LABEL).unwrap();
        let _ = win.eval("window.__stickyManagerReload && window.__stickyManagerReload()");
        win
    };

    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    // 复用窗口可能被 hide 了，需要重新 show；新窗口已 visible(true) 创建
    if !is_new {
        win.show().map_err(|e| format!("显示便签管理窗口失败: {e}"))?;
    }
    let _ = win.unminimize();
    win.set_focus()
        .map_err(|e| format!("聚焦便签管理窗口失败: {e}"))?;

    tracing::info!("sticky-manager window: 已显示");
    Ok(())
}

/// 0.17.3：显示首次启动引导窗口。
///
/// 独立窗口（label "welcome"），480×440 居中，有标题栏（decorations: true），
/// 不可调整大小。关闭时自动标记 `first_run = false`（防止用户点 X 不点"开始使用"）。
/// 与主窗口独立——watchdog 只 hide "main" 窗口，不影响引导窗口。
pub fn show_welcome_window(app: &AppHandle) {
    const LABEL: &str = "welcome";

    // 已存在则直接 show + focus（安全兜底，正常不会走到——first_run=false 后不再弹）
    if let Some(win) = app.get_webview_window(LABEL) {
        let _ = win.show();
        let _ = win.set_focus();
        return;
    }

    use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};

    let win = match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("welcome.html".into()))
        .title("Blink")
        .inner_size(480.0, 440.0)
        .resizable(false)
        .decorations(true)
        .transparent(false)
        .always_on_top(false)
        .skip_taskbar(false)
        .focused(true)
        .visible(true)
        .background_color(Color(30, 30, 46, 255))
        .center()
        .build()
    {
        Ok(w) => w,
        Err(e) => {
            tracing::warn!(error = %e, "welcome window: 创建失败");
            return;
        }
    };

    // 关闭时标记 first_run = false（防用户点 X 不点"开始使用"按钮）
    let app_clone = app.clone();
    win.on_window_event(move |event| {
        if let tauri::WindowEvent::CloseRequested { .. } = event {
            let app = app_clone.clone();
            tauri::async_runtime::spawn(async move {
                let pools = app.state::<crate::infra::data::DbPools>();
                let _ = crate::app::config::update_first_run(&pools.config, false).await;
                tracing::info!("welcome window: CloseRequested -> first_run = false");
            });
        }
    });

    tracing::info!("welcome window: 已显示");
}

/// 显示便签窗口（0.16.8）。
///
/// 每条便签一个独立 Tauri 窗口，label 为 `sticky-{id}`（id 截断到 60 字符防止超长）。
/// 窗口位置、尺寸、置顶状态从 StickyNote 数据恢复。
/// 关闭按钮 = 隐藏（prevent_close），不销毁窗口——下次显示复用同一 webview。
///
/// **看门狗安全**：看门狗按 PID 判定，前台切到便签时 `fg_pid == self_pid`，主窗不会被误隐藏。
pub fn show_sticky_window(
    app: &AppHandle,
    sticky_id: &str,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    always_on_top: bool,
    focus: bool,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    // 0.16.11：安全截断——按字符而非字节切片，避免非 ASCII ID 截断 panic。
    // sticky_id 实际都是 ASCII（generate_id 产 sticky_{nanos}），但做防御性编程。
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");

    let is_new = app.get_webview_window(&label).is_none();

    let win = if is_new {
        // URL 带 sticky_id 参数，前端 init 时读取
        // P3-#22 fix: URL 编码防注入——sticky_id 来自前端任意字符串
        let encoded_id = sticky_id
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                    c.to_string()
                } else {
                    format!("%{:02X}", c as u32)
                }
            })
            .collect::<String>();
        let url = format!("sticky.html?id={encoded_id}");
        // 0.16.11：几何钳制——显示器拔插/分辨率变化后保证窗口至少部分可见
        let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);

        // 0.16.10 fix P0-#7: inner_size 接受逻辑像素，需将物理像素转换为逻辑
        let scale = unsafe {
            let pt = POINT { x: cx, y: cy };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            crate::infra::platform::dpi::scale_factor(
                crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
            )
        };
        let logical_w = cw as f64 / scale;
        let logical_h = ch as f64 / scale;

        let w = WebviewWindowBuilder::new(app, &label, WebviewUrl::App(url.into()))
            .title("便签")
            .inner_size(logical_w, logical_h)
            .min_inner_size(120.0, 80.0)
            .position(cx as f64, cy as f64)
            .decorations(false)
            .transparent(false)
            .always_on_top(always_on_top)
            .skip_taskbar(true)
            .resizable(true)
            .focused(focus)
            .visible(true)
            .build()
            .map_err(|e| {
                tracing::warn!(error = %e, "sticky window: 创建失败");
                format!("创建便签窗口失败: {e}")
            })?;

        // 注册 CloseRequested handler：仅新窗口注册一次，避免复用时重复绑定
        //
        // 0.16.11：区分「用户关闭便签」与「应用整体退出」：
        // - 用户关闭：prevent_close + set_visible(false) + hide（保持数据，只隐藏桌面窗口）
        // - 应用退出：不 prevent_close，让窗口正常关闭，**不修改 visible**——
        //   下次启动按 DB 中的 visible 状态恢复，退出不等于全部隐藏
        let label_owned = label.clone();
        let app_clone = app.clone();
        let sid = sticky_id.to_string();
        w.on_window_event(move |event| {
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                if IS_APP_EXITING.load(Ordering::SeqCst) {
                    // 应用退出：不 prevent_close，不修改 visible
                    tracing::debug!(
                        sticky_id = %sid,
                        "sticky window: CloseRequested during app exit → 不修改 visible"
                    );
                    return;
                }
                api.prevent_close();
                // P1-#12 fix: 关闭前 flush 未保存内容（前端有 500ms 防抖）
                if let Some(w) = app_clone.get_webview_window(&label_owned) {
                    let _ = w.eval("if (window.__stickyFlush) window.__stickyFlush();");
                }
                // 异步设置 visible=false 并隐藏窗口
                // P1-#9 fix: 用 try_state() 而非 state() 避免 panic；
                //   infra → domain 依赖是已知架构债，后续应改为 emit 事件由 app 层处理
                let app_c = app_clone.clone();
                let sid_owned = sid.clone();
                tauri::async_runtime::spawn(async move {
                    if let Some(svc) = app_c
                        .try_state::<std::sync::Arc<crate::domain::sticky::StickyService>>()
                    {
                        if let Err(e) = svc.set_visible(&sid_owned, false).await {
                            tracing::warn!(error = %e, "便签关闭时设置 visible=false 失败");
                        }
                    } else {
                        tracing::warn!("便签关闭时 StickyService 不可用，跳过 set_visible");
                    }
                });
                if let Some(w) = app_clone.get_webview_window(&label_owned) {
                    let _ = w.hide();
                }
                tracing::debug!(sticky_id = %sid, "sticky window: CloseRequested → prevent_close + flush + hide");
            }
        });
        w
    } else {
        // 复用已有窗口——重新定位 + 重新加载便签数据
        // 0.16.11：复用路径也做几何钳制，防止显示器变化后窗口不可见
        let (cx, cy, cw, ch) = clamp_sticky_geometry(x, y, width, height);
        // P3-#23 fix: 用 ok_or_else 替代 unwrap，窗口在判定存在后可能被并发销毁
        let win = app.get_webview_window(&label).ok_or_else(|| {
            tracing::warn!(label = %label, "复用便签窗口时发现窗口已不存在");
            "便签窗口在复用时已不存在".to_string()
        })?;
        // 0.16.10 fix P0-#7: 复用路径也用逻辑像素（与新建路径一致）
        let scale = unsafe {
            let pt = POINT { x: cx, y: cy };
            let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
            crate::infra::platform::dpi::scale_factor(
                crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon),
            )
        };
        let _ = win.set_size(tauri::LogicalSize::new(cw as f64 / scale, ch as f64 / scale));
        let _ = win.set_position(tauri::PhysicalPosition::new(cx, cy));
        let _ = win.set_always_on_top(always_on_top);
        // 通知前端重新加载便签数据
        // P3-#22 fix: JS 字符串转义防注入
        let escaped_id = sticky_id.replace('\\', "\\\\").replace('\'', "\\'").replace('\n', "\\n").replace('\r', "\\r");
        let js = format!(
            "if (window.__stickyReload) window.__stickyReload('{escaped_id}')"
        );
        let _ = win.eval(&js);
        win
    };

    // 系统菜单拦截 + 圆角
    if let Ok(hwnd) = win.hwnd() {
        let hwnd = HWND(hwnd.0 as _);
        install_sysmenu_blocker(hwnd);
        enable_rounded_corners(hwnd);
    }

    win.show().map_err(|e| format!("显示便签窗口失败: {e}"))?;
    let _ = win.unminimize();
    // 0.16.11：恢复路径（focus=false）不抢焦点，不影响主窗口 Alt+Space
    if focus {
        win.set_focus()
            .map_err(|e| format!("聚焦便签窗口失败: {e}"))?;
    }

    tracing::info!(sticky_id, focus, "sticky window: 已显示");
    Ok(())
}

/// 0.16.11：标记应用正在退出。
///
/// 在 `RunEvent::Exit` 时调用，让便签窗口的 CloseRequested handler 知道
/// 这是应用整体退出而非用户关闭单条便签。
pub fn set_app_exiting() {
    IS_APP_EXITING.store(true, Ordering::SeqCst);
    tracing::debug!("set_app_exiting: IS_APP_EXITING → true");
}

/// 0.16.11：退出前 flush 所有便签窗口的未保存内容。
///
/// 前端有 500ms 内容防抖和 300ms 几何防抖。退出时 eval flush JS，
/// 让前端立即写入后端，避免丢失最近 500ms 的编辑。
/// 返回 flush 的窗口数量。
pub fn flush_all_sticky_windows(app: &AppHandle) -> usize {
    let mut count = 0usize;
    for (label, win) in app.webview_windows() {
        if !label.starts_with("sticky-") || label == "sticky-manager" {
            continue;
        }
        // eval flush——前端 __stickyFlush 立即调用后端保存
        let _ = win.eval("if (window.__stickyFlush) window.__stickyFlush();");
        count += 1;
    }
    if count > 0 {
        tracing::debug!(count, "flush_all_sticky_windows: 已向 {} 个便签窗口发送 flush", count);
    }
    count
}

/// 计算便签在当前前台窗口所在显示器工作区的居中坐标（0.16.11）。
///
/// 新建便签时调用，让便签出现在用户当前关注的屏幕中心而非 (0,0) 角落。
/// 返回 (x, y) 物理像素。
pub fn center_of_active_monitor(width: i32, height: i32) -> (i32, i32) {
    unsafe {
        let hwnd = GetForegroundWindow();
        let hmon = if hwnd.is_invalid() {
            MonitorFromPoint(POINT { x: 0, y: 0 }, MONITOR_DEFAULTTOPRIMARY)
        } else {
            MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST)
        };
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            return (0, 0);
        }
        let work = mi.rcWork;
        let x = work.left + (work.right - work.left - width) / 2;
        let y = work.top + (work.bottom - work.top - height) / 2;
        (x, y)
    }
}

/// 0.16.11：钳制便签窗口几何到可见工作区。
///
/// 显示器拔插、分辨率/DPI 变化后，存储的 (x, y) 可能指向不存在的显示器。
/// 使用 `MonitorFromPoint` 查找位置所在显示器，找不到时 fallback 到主屏，
/// 然后钳制到工作区内，确保窗口至少部分可见。
///
/// 返回值 `(x, y, width, height)` 为钳制后的物理像素。
fn clamp_sticky_geometry(x: i32, y: i32, width: i32, height: i32) -> (i32, i32, i32, i32) {
    // 保证尺寸合理
    let w = width.max(120).min(4096);
    let h = height.max(80).min(4096);

    unsafe {
        let pt = POINT { x, y };
        // 先尝试指定位置所在显示器，找不到则取主屏
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        if !GetMonitorInfoW(hmon, &mut mi).as_bool() {
            // 极端 fallback：拿不到显示器信息，原样返回（尺寸已 clamp）
            return (x, y, w, h);
        }
        let work = mi.rcWork; // 工作区（排除任务栏）

        // 钳制到工作区：确保窗口至少 80x60 像素可见
        let min_visible_w = 80i32;
        let min_visible_h = 60i32;

        // X：如果窗口完全在 Work 区左侧，移到 work.left；
        //     完全在右侧，移到 work.right - min_visible_w；
        //     部分可见且可见部分 >= min_visible_w，保持不动；
        //     部分可见但可见部分 < min_visible_w，调整使其至少 min_visible_w 可见
        let cx = if x + w <= work.left + min_visible_w {
            // 窗口在左边界外或几乎不可见
            work.left
        } else if x >= work.right - min_visible_w {
            // 窗口在右边界外或几乎不可见
            (work.right - w).max(work.left)
        } else {
            // 至少部分可见，保持
            x
        };

        let cy = if y + h <= work.top + min_visible_h {
            work.top
        } else if y >= work.bottom - min_visible_h {
            (work.bottom - h).max(work.top)
        } else {
            y
        };

        tracing::trace!(
            orig_x = x, orig_y = y, orig_w = width, orig_h = height,
            clamped_x = cx, clamped_y = cy, clamped_w = w, clamped_h = h,
            work_left = work.left, work_top = work.top,
            work_right = work.right, work_bottom = work.bottom,
            "clamp_sticky_geometry: 钳制完成"
        );

        (cx, cy, w, h)
    }
}

/// 隐藏便签窗口（不删除数据）。
#[allow(dead_code)] // 0.16.10 管理界面将使用
pub fn hide_sticky_window(app: &AppHandle, sticky_id: &str) {
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");
    if let Some(win) = app.get_webview_window(&label) {
        let _ = win.hide();
        tracing::debug!(sticky_id, "sticky window: 已隐藏");
    }
}

/// 销毁便签窗口（删除数据后调用）。
pub fn destroy_sticky_window(app: &AppHandle, sticky_id: &str) {
    let truncated_id: String = sticky_id.chars().take(64).collect();
    let label = format!("sticky-{truncated_id}");
    if let Some(win) = app.get_webview_window(&label) {
        // 用 destroy() 而非 close()——close() 会触发 CloseRequested 被 prevent_close 拦截
        let _ = win.destroy();
        tracing::debug!(sticky_id, "sticky window: 已销毁");
    }
}

/// 显示语音录音 mini overlay（0.10 G2）。
/// 独立 webview 窗口，不抢焦点（WS_EX_NOACTIVATE），显示在光标附近。
/// 录音结束后由 voice::VoiceService::stop_recording 发 voice-recording-end → 前端隐藏。
pub fn show_voice_overlay(app: &AppHandle) {
    const LABEL: &str = "voice-overlay";
    let (mx, my) = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        (pt.x, pt.y)
    };

    if let Some(win) = app.get_webview_window(LABEL) {
        // 0.10.6: 复用时重置尺寸为默认值（上次可能被 autoResize 撑高）
        let _ = win.set_size(tauri::LogicalSize::new(260.0, 140.0));
        let _ = win.set_position(tauri::PhysicalPosition::new(mx + 16, my + 16));
        let _ = win.show();
        return;
    }

    use tauri::{WebviewUrl, WebviewWindowBuilder};
    match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("voice-overlay.html".into()))
        .title("")
        .inner_size(260.0, 140.0)
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .focused(false)
        .visible(true)
        .build()
    {
        Ok(win) => {
            let _ = win.set_position(tauri::PhysicalPosition::new(mx + 16, my + 16));
            if let Ok(hwnd) = win.hwnd() {
                apply_no_activate(HWND(hwnd.0 as _));
            }
            tracing::debug!("voice-overlay: 已显示");
        }
        Err(e) => tracing::warn!(error = %e, "voice-overlay: 创建失败"),
    }
}

/// 隐藏语音录音 mini overlay。
pub fn hide_voice_overlay(app: &AppHandle) {
    if let Some(win) = app.get_webview_window("voice-overlay") {
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

    // 0.11.9：注入每屏几何 + DPI（前端工具栏/OCR panel 按"选区所在屏"clamp，
    // 不再以整个虚拟屏幕为基准——副屏左边缘做选区时工具栏不会被推到主屏）。
    // x/y/w/h 是物理像素，前端用 devicePixelRatio 折算回 CSS 像素与 selCss 对齐。
    let displays_json = build_displays_json();
    let fg_hwnd = crate::infra::platform::screenshot::session_fg_hwnd().unwrap_or(0);
    let meta_js = format!(
        "window.__blinkScreenMeta = {{ vx: {}, vy: {}, w: {}, h: {}, fgHwnd: {}, displays: {} }};",
        meta.virtual_x, meta.virtual_y, meta.width, meta.height, fg_hwnd, displays_json
    );

    // 复用已存在的窗口：先 eval 清屏 + 重定位 → show → 触发重新加载
    if let Some(win) = app.get_webview_window(LABEL) {
        // 先清屏再 show —— 否则窗口刚出来会看到上次结束时的选区/虚线框闪一下
        // （webview `.show()` 到 __blinkReloadScreenshot 执行之间有毫秒级空档）
        let _ = win.eval("window.__blinkReloadScreenshot && window.__blinkReloadScreenshot()");
        let _ = win.eval(&meta_js);
        if let Ok(hwnd) = win.hwnd() {
            place_at_physical(
                HWND(hwnd.0 as _),
                meta.virtual_x,
                meta.virtual_y,
                meta.width,
                meta.height,
            );
        }
        let _ = win.show();
        let _ = win.set_focus();
        return Ok(());
    }

    // 首次构建：inner_size / position 会被后续 SetWindowPos 覆盖，这里只是让 Tauri 别报参数错。
    let win =
        WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("chord-screenshot.html".into()))
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
        place_at_physical(
            HWND(hwnd.0 as _),
            meta.virtual_x,
            meta.virtual_y,
            meta.width,
            meta.height,
        );
    }
    let _ = win.eval(&meta_js);
    let _ = win.set_focus();

    Ok(())
}

/// 构造 `__blinkScreenMeta.displays` 字段的 JS 数组字面量。
///
/// 每屏一项：`{ x, y, w, h, dpi, primary }`（物理像素 + DPI）。
/// 失败时返回空数组 `[]`，前端按"无 displays 信息"降级到旧的虚拟屏幕 clamp。
fn build_displays_json() -> String {
    let displays = crate::infra::platform::screenshot::list_displays();
    if displays.is_empty() {
        return "[]".to_string();
    }
    let items: Vec<String> = displays
        .iter()
        .map(|d| {
            format!(
                "{{ x: {}, y: {}, w: {}, h: {}, dpi: {}, primary: {} }}",
                d.x, d.y, d.w, d.h, d.dpi, d.primary
            )
        })
        .collect();
    format!("[{}]", items.join(", "))
}

/// 按物理像素强制定位窗口，覆盖 Tauri 逻辑像素接口的 DPI 缩放。
///
/// **为何要走 Win32 而非 Tauri 的 `set_size` + `set_position`**：
/// 当窗口跨过一块 DPI 不同的显示器时（如从主屏 150% 移到副屏 100%），
/// `set_position` 会触发 `WM_DPICHANGED`，tao 的窗口过程据此**按 DPI 比例
/// 重设窗口尺寸但不动位置**——与刚排队的 `set_size` 竞态，导致最终尺寸/位置
/// 不可预测（Tauri issue #3610 / #10263，无边框窗口尤甚）。`SetWindowPos` 一次
/// 原子地设定位置+尺寸，绕开 tao 的 WM_DPICHANGED 重设尺寸逻辑，所见即所得。
///
/// 用途：
/// - 截图 overlay 必须精确对齐虚拟屏幕物理像素（canvas.width 与窗口 CSS 尺寸比
///   值需与 DPR 匹配，否则选区坐标全歪）
/// - 右键菜单复用路径（窗口在主屏预热，需移到任意屏的物理坐标）
pub fn place_at_physical(hwnd: HWND, x: i32, y: i32, w: u32, h: u32) {
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

/// 钉图窗口的物理像素 padding（窗口比图片大一圈，给发光留空间）。
/// 20px 足够 box-shadow 的 12px 模糊半径扩散。
pub const PIN_PAD: i32 = 20;

/// 显示钉图窗口（0.11.7-d）。
///
/// 复用预热窗口（首次创建 ~300ms → 复用后 <50ms），通过 `eval` 注入 PNG base64 到 `<img>`。
///
/// **纯图片贴桌面效果**（0.11.8）：
/// - 窗口 `.transparent(true)` 让背景完全透明，只有图片本身可见
/// - 窗口尺寸 = 图片显示尺寸 + 2×PIN_PAD（预留发光空间，否则 box-shadow 被裁）
/// - 窗口左上 = `(screen_x - PAD, screen_y - PAD)`，使图片左上落在选区原位
/// - 缩放时窗口尺寸跟随变化（`screenshot_pin_transform`），图片用 width/height 不用 scale
///   —— 这样发光区不会因窗口固定被裁，放大时图片也不会被窗口边界裁
///
/// **单钉图策略**：目前只支持单张钉图，重复触发会覆盖已有内容。
pub fn show_pin_window(
    app: &AppHandle,
    png_data: Vec<u8>,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    use base64::Engine;
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    const LABEL: &str = "chord-pin";
    const FALLBACK_W: f64 = 400.0;
    const FALLBACK_H: f64 = 300.0;

    // 解析 PNG 像素尺寸用于开窗（失败兜底 400×300）
    let (png_w, png_h) = crate::infra::platform::screenshot::parse_png_size(&png_data)
        .map(|(w, h)| (w as f64, h as f64))
        .unwrap_or((FALLBACK_W, FALLBACK_H));

    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_data);
    let data_url = format!("data:image/png;base64,{b64}");

    // 窗口左上 = 图片左上 - PAD（让图片左上对齐选区原位，窗口外圈留 PAD 给发光）
    let win_x = screen_x - PIN_PAD;
    let win_y = screen_y - PIN_PAD;
    let win_w = png_w as u32 + 2 * PIN_PAD as u32;
    let win_h = png_h as u32 + 2 * PIN_PAD as u32;

    // 复用已存在的窗口（预热或上次钉图）
    if let Some(win) = app.get_webview_window(LABEL) {
        // 先按物理坐标定位（绕开 Tauri 逻辑像素的 DPI 竞态）
        if let Ok(hwnd) = win.hwnd() {
            place_at_physical(HWND(hwnd.0 as _), win_x, win_y, win_w, win_h);
        }
        // 把图片左上物理坐标也传给前端（__blinkResetPin 第 4/5 参数），
        // 前端用作缩放基准 imgScreenX/Y，避免 window.screenX 的 DPI 换算问题
        let js = format!(
            "if (window.__blinkResetPin) window.__blinkResetPin('{url}', {w}, {h}, {sx}, {sy}); else document.getElementById('pin-img').src = '{url}';",
            url = data_url,
            w = png_w,
            h = png_h,
            sx = screen_x,
            sy = screen_y
        );
        win.eval(&js)
            .map_err(|e| format!("eval 注入 PNG 失败: {e}"))?;
        let _ = win.show();
        let _ = win.set_focus();
        tracing::debug!(png_w, png_h, screen_x, screen_y, "钉图窗口已复用");
        return Ok(());
    }

    // 首次创建：transparent + 按 (图片尺寸 + 2*PAD) 开窗 + 定位使图片左上落选区原位
    match WebviewWindowBuilder::new(app, LABEL, WebviewUrl::App("pin.html".into()))
        .title("")
        .decorations(false)
        .transparent(true)
        .always_on_top(true)
        .skip_taskbar(true)
        .shadow(false)
        .inner_size(win_w as f64, win_h as f64)
        .position(win_x as f64, win_y as f64)
        .build()
    {
        Ok(win) => {
            // 再用 SetWindowPos 精确对齐物理像素（首次 build 后 Tauri 可能因 DPI 偏移）
            if let Ok(hwnd) = win.hwnd() {
                place_at_physical(HWND(hwnd.0 as _), win_x, win_y, win_w, win_h);
            }
            // 注入 PNG 数据 + 图片左上物理坐标（首次也走 __blinkResetPin 以统一状态）
            let js = format!(
                "if (window.__blinkResetPin) window.__blinkResetPin('{url}', {w}, {h}, {sx}, {sy}); else document.getElementById('pin-img').src = '{url}';",
                url = data_url,
                w = png_w,
                h = png_h,
                sx = screen_x,
                sy = screen_y
            );
            win.eval(&js)
                .map_err(|e| format!("eval 注入 PNG 失败: {e}"))?;
            let _ = win.show();
            tracing::debug!(png_w, png_h, screen_x, screen_y, "钉图窗口已创建");
            Ok(())
        }
        Err(e) => {
            tracing::warn!(error = %e, "钉图窗口创建失败");
            Err(format!("钉图窗口创建失败: {e}"))
        }
    }
}

/// Apply or remove DWM Cloak on a window.
///
/// Cloak = true: DWM 层瞬间"雾化"窗口（无 fade 动画），WS_VISIBLE 仍为 on。
/// Cloak = false: 恢复正常可见性。
///
/// 调用方负责确保 cloak 状态对称——cloak 后必须在下次 show 前 uncloak，
/// 否则窗口 show 出来仍不可见。
pub fn apply_cloak(hwnd: HWND, on: bool) {
    unsafe {
        let cloak: i32 = if on { 1 } else { 0 };
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_CLOAK,
            &cloak as *const _ as *const _,
            std::mem::size_of::<i32>() as u32,
        );
    }
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
            apply_cloak(HWND(hwnd.0 as _), true);
        }
        let _ = win.hide();
        let _ = app.emit(EventNames::HIDDEN, ());
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
            apply_cloak(HWND(hwnd.0 as _), false);
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

/// 后台预热次级窗口：延迟创建 chord-screenshot / context-menu / voice-overlay /
/// chord-pin / chat / settings / content-editor / sticky-manager 并立即隐藏。
///
/// WebView2 首次建实例 300~400ms，预热后 show 只是切可见性 (<50ms)。
/// 代价：常驻内存 +10~20MB × N（8 窗口 + 动态便签，实测 < 300MB 预算内）；
/// 收益：所有次级窗口首次触发无感。
///
/// 0.17.2：追加 settings / content-editor / sticky-manager 三个窗口预热。
/// sticky-manager 预热时注册 prevent_close + hide（show 函数复用路径不注册）。
///
/// chord-ball 悬浮球预热已随划词翻译 chord 移除而删除。
pub fn preheat_secondary_windows(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        // 等主窗稳定 + 前端加载完毕，不与启动路径抢资源
        tokio::time::sleep(Duration::from_secs(3)).await;
        tracing::debug!("preheat: 开始预热次级窗口");

        // --- chord-screenshot（截图 overlay，透明全屏层） ---
        if app.get_webview_window("chord-screenshot").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(
                &app,
                "chord-screenshot",
                // 0.11.7-f：URL 加 ?preheat=1，前端识别参数跳过 loadScreenshot，
                // 避免 SESSION 空时 img.onerror 遗留 error-hint 到用户实际唤起的 overlay
                WebviewUrl::App("chord-screenshot.html?preheat=1".into()),
            )
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
            match WebviewWindowBuilder::new(
                &app,
                "context-menu",
                WebviewUrl::App("contextmenu-popup.html".into()),
            )
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

        // --- voice-overlay（语音录音 mini overlay，0.10 G2） ---
        if app.get_webview_window("voice-overlay").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(
                &app,
                "voice-overlay",
                WebviewUrl::App("voice-overlay.html".into()),
            )
            .title("")
            .inner_size(260.0, 140.0)
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
                    tracing::debug!("preheat: voice-overlay ✓");
                }
                Err(e) => tracing::warn!(error = %e, "preheat: voice-overlay 失败"),
            }
        }

        // --- chord-pin（钉图窗口，0.11.7-d；0.11.8 透明贴合） ---
        if app.get_webview_window("chord-pin").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "chord-pin", WebviewUrl::App("pin.html".into()))
                .title("")
                .inner_size(400.0, 300.0)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .always_on_top(true)
                .skip_taskbar(true)
                .focused(false)
                .visible(false)
                .build()
            {
                Ok(_) => tracing::debug!("preheat: chord-pin ✓"),
                Err(e) => tracing::warn!(error = %e, "preheat: chord-pin 失败"),
            }
        }

        // --- chat（对话窗口，0.12.2 加入预热） ---
        if app.get_webview_window("chat").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "chat", WebviewUrl::App("chat.html".into()))
                .title("Blink AI")
                .inner_size(900.0, 680.0)
                .min_inner_size(560.0, 420.0)
                .decorations(false)
                .transparent(false)
                .always_on_top(false)
                .skip_taskbar(false)
                .resizable(true)
                .focused(false)
                .visible(false)
                .build()
            {
                Ok(_) => tracing::debug!("preheat: chat ✓"),
                Err(e) => tracing::warn!(error = %e, "preheat: chat 失败"),
            }
        }

        // --- settings（设置窗口，0.17.2 加入预热） ---
        // 静态 URL 无参数，open_settings 已有复用路径（get_webview_window → 重新定位 + show）。
        // 预热时补 strip_window_border + enable_rounded_corners，
        // 因为 open_settings 的复用路径不调这两个（只在首次创建路径调）。
        if app.get_webview_window("settings").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder};
            match WebviewWindowBuilder::new(&app, "settings", WebviewUrl::App("settings.html".into()))
                .title("Blink Settings")
                .inner_size(960.0, 680.0)
                .min_inner_size(760.0, 520.0)
                .position(0.0, 0.0)
                .visible(false)
                .decorations(false)
                .transparent(true)
                .shadow(false)
                .background_color(tauri::window::Color(0, 0, 0, 0))
                .build()
            {
                Ok(win) => {
                    if let Ok(hwnd) = win.hwnd() {
                        let hwnd = HWND(hwnd.0 as _);
                        strip_window_border(hwnd);
                        enable_rounded_corners(hwnd);
                    }
                    tracing::debug!("preheat: settings ✓");
                }
                Err(e) => tracing::warn!(error = %e, "preheat: settings 失败"),
            }
        }

        // --- content-editor（内容编辑器，0.17.2 加入预热） ---
        // 静态 URL，payload 走 Tauri State（PendingEditorPayload）。
        // show_content_editor_window 的复用路径会 eval __contentEditorReload + show，
        // install_sysmenu_blocker / enable_rounded_corners 在 show 函数中对新旧窗口都调。
        if app.get_webview_window("content-editor").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};
            match WebviewWindowBuilder::new(
                &app,
                "content-editor",
                WebviewUrl::App("content-editor.html".into()),
            )
            .title("编辑内容")
            .inner_size(720.0, 560.0)
            .min_inner_size(400.0, 300.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(false)
            .visible(false)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
            {
                Ok(_) => tracing::debug!("preheat: content-editor ✓"),
                Err(e) => tracing::warn!(error = %e, "preheat: content-editor 失败"),
            }
        }

        // --- sticky-manager（便签管理，0.17.2 加入预热） ---
        // 静态 URL，自取列表（listStickyNotes）。
        // 预热时注册 prevent_close + hide（与 show_sticky_manager_window 创建路径一致），
        // 因为 show 函数的复用路径（is_new=false）不注册 on_window_event。
        if app.get_webview_window("sticky-manager").is_none() {
            use tauri::{WebviewUrl, WebviewWindowBuilder, window::Color};
            match WebviewWindowBuilder::new(
                &app,
                "sticky-manager",
                WebviewUrl::App("sticky-manager.html".into()),
            )
            .title("便签管理")
            .inner_size(560.0, 640.0)
            .min_inner_size(360.0, 400.0)
            .decorations(false)
            .transparent(false)
            .always_on_top(false)
            .skip_taskbar(false)
            .resizable(true)
            .focused(false)
            .visible(false)
            .background_color(Color(30, 30, 46, 255))
            .center()
            .build()
            {
                Ok(w) => {
                    // 注册 prevent_close + hide（复用模式）
                    let app_clone = app.clone();
                    w.on_window_event(move |event| {
                        if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                            if IS_APP_EXITING.load(Ordering::SeqCst) {
                                return; // 应用退出：不 prevent_close
                            }
                            api.prevent_close();
                            if let Some(w) = app_clone.get_webview_window("sticky-manager") {
                                let _ = w.hide();
                            }
                            tracing::debug!(
                                "preheat sticky-manager: CloseRequested → prevent_close + hide"
                            );
                        }
                    });
                    tracing::debug!("preheat: sticky-manager ✓");
                }
                Err(e) => tracing::warn!(error = %e, "preheat: sticky-manager 失败"),
            }
        }

        tracing::debug!("preheat: 预热完成");
    });
}

/// 打开设置窗口：**每次都定位到光标所在屏的工作区中心**。
///
/// - 已存在：从 iconic 恢复 → 读当前 outer_size 保留用户 resize 过的尺寸 →
///   `place_at_physical` 一次原子挪到光标屏中心（避开 WM_DPICHANGED 抢跑）。
/// - 首次创建：build 完立刻按目标屏 DPI 把 960×680 CSS → 物理尺寸，挪过去。
///
/// 语义：用户在哪块屏发起动作（右键 → 打开设置 / 托盘 → 设置），设置就出现在
/// 那块屏。跟 Universal Action Layer 的直觉一致，也省了跨屏找窗口的动作。
pub fn open_settings(app: &AppHandle) {
    // 光标所在屏工作区 + DPI（一次读，两条路径复用）
    let (work, target_dpi) = unsafe {
        let mut pt = POINT { x: 0, y: 0 };
        let _ = GetCursorPos(&mut pt);
        let hmon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
        let mut mi: MONITORINFO = std::mem::zeroed();
        mi.cbSize = std::mem::size_of::<MONITORINFO>() as u32;
        let work = if GetMonitorInfoW(hmon, &mut mi).as_bool() {
            mi.rcWork
        } else {
            windows::Win32::Foundation::RECT {
                left: 0,
                top: 0,
                right: 1920,
                bottom: 1080,
            }
        };
        // 0.11.9：走公共 DPI helper（get_dpi_for_hmonitor 内部已 .max(96) 兜底）
        let target_dpi = crate::infra::platform::dpi::get_dpi_for_hmonitor(hmon);
        (work, target_dpi)
    };
    let work_w = work.right - work.left;
    let work_h = work.bottom - work.top;

    if let Some(w) = app.get_webview_window("settings") {
        // 从最小化恢复
        let hwnd_raw = w.hwnd().ok();
        if let Some(h) = hwnd_raw {
            let hwnd = HWND(h.0 as _);
            unsafe {
                if IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
        }
        // 保留 **CSS 尺寸**（不是物理）——跨 DPI 屏保留物理尺寸会越挪越离谱：
        //   主屏 150% 首次 1440 phys(=960 CSS)
        //   → 挪副屏 100%,tao 处理 WM_DPICHANGED 按 100/150 缩到 960 phys
        //   → 回主屏读 outer_size=960 phys,若直接用作物理 → 主屏 150% 视觉 640 CSS,变小 1/3
        // 用当前 scale_factor 折算 CSS,再按目标屏 DPI 换回物理。scale_factor 和 outer_size
        // 都反映"窗口当前所在屏",配对读一致快照,比值稳定 = CSS 尺寸恒定。
        let cur_scale = w.scale_factor().unwrap_or(1.0).max(1.0);
        let cur_phys = w.outer_size().unwrap_or_else(|_| {
            tauri::PhysicalSize::new(
                (960.0 * cur_scale).round() as u32,
                (680.0 * cur_scale).round() as u32,
            )
        });
        let css_w = (cur_phys.width as f64) / cur_scale;
        let css_h = (cur_phys.height as f64) / cur_scale;
        let target_scale = crate::infra::platform::dpi::scale_factor(target_dpi);
        let phys_w = (css_w * target_scale).round() as i32;
        let phys_h = (css_h * target_scale).round() as i32;
        // clamp 到目标屏工作区
        let win_w = phys_w.min(work_w).max(1);
        let win_h = phys_h.min(work_h).max(1);
        let fx = work.left + (work_w - win_w) / 2;
        let fy = work.top + (work_h - win_h) / 2;
        if let Some(h) = hwnd_raw {
            let hwnd = HWND(h.0 as _);
            place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
            let _ = w.show();
            // 跨 DPI 屏时 WM_DPICHANGED 会抢跑改尺寸,补一次覆盖回来
            place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        } else {
            let _ = w.set_position(PhysicalPosition::new(fx, fy));
            let _ = w.show();
        }
        let _ = w.set_focus();
        return;
    }
    use tauri::{WebviewUrl, WebviewWindowBuilder};
    // 首次创建：先 hidden build（避免主屏闪一下），然后按目标屏 DPI 把默认
    // CSS 尺寸(960×680) 折算成物理尺寸，place_at_physical 挪到光标屏中心。
    // 位置给 (0,0) 占位，builder 的 .center() 只会居中主屏——用不上。
    let win = WebviewWindowBuilder::new(app, "settings", WebviewUrl::App("settings.html".into()))
        .title("Blink Settings")
        .inner_size(960.0, 680.0)
        .min_inner_size(760.0, 520.0)
        .position(0.0, 0.0)
        .visible(false)
        .decorations(false)
        .transparent(true)
        .shadow(false)
        .background_color(tauri::window::Color(0, 0, 0, 0))
        .build()
        .expect("创建设置窗口失败");
    let scale = crate::infra::platform::dpi::scale_factor(target_dpi);
    let phys_w = (960.0 * scale).round() as i32;
    let phys_h = (680.0 * scale).round() as i32;
    let win_w = phys_w.min(work_w);
    let win_h = phys_h.min(work_h);
    let fx = work.left + (work_w - win_w) / 2;
    let fy = work.top + (work_h - win_h) / 2;
    if let Ok(h) = win.hwnd() {
        let hwnd = HWND(h.0 as _);
        strip_window_border(hwnd);
        enable_rounded_corners(hwnd);
        place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        let _ = win.show();
        // 补一次：show 触发 WM_DPICHANGED 时 tao 会改尺寸，覆盖回来
        place_at_physical(hwnd, fx, fy, win_w as u32, win_h as u32);
        let _ = win.set_focus();
    } else {
        let _ = win.show();
    }
}
