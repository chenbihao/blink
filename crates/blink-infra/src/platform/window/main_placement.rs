//! 主窗布局会话：首帧、内容 resize 与异步 caret 共用一个几何提交入口。
//! 前端首批布局明确就绪后才校准；隐藏、重开或用户开始操作后不接受旧校准。

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITOR_DEFAULTTOPRIMARY, MONITORINFO,
    MonitorFromPoint, MonitorFromWindow,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect, IsWindowVisible,
    SET_WINDOW_POS_FLAGS, SWP_NOACTIVATE, SWP_NOSIZE, SWP_NOZORDER, SetWindowPos,
};

use crate::event_names::EventNames;
use crate::platform::caret::{self, ClipboardAnchor, ClipboardPopupSide};
use crate::platform::dpi;

const REFINE_DEADLINE: Duration = Duration::from_millis(250);

#[derive(Clone, Copy, Debug)]
pub enum MainWindowPlacement {
    Launcher,
    Clipboard {
        owner_hwnd: Option<isize>,
        cursor: Option<(i32, i32)>,
    },
}

#[derive(Clone, Copy, serde::Deserialize)]
pub struct MainWindowLayoutRequest {
    pub revision: u64,
    pub sequence: u64,
    pub clipboard: bool,
    pub ready: bool,
    pub interacted: bool,
}

#[derive(Clone, serde::Serialize)]
pub struct MainWindowLayoutState {
    pub revision: u64,
    pub visible: bool,
    pub clipboard: bool,
}

#[derive(Default)]
struct LayoutGate {
    revision: u64,
    sequence: u64,
    ready: bool,
    interacted: bool,
}

impl LayoutGate {
    fn accept(&mut self, request: &MainWindowLayoutRequest) -> bool {
        if request.revision != self.revision || request.sequence <= self.sequence {
            return false;
        }
        self.sequence = request.sequence;
        self.ready |= request.ready;
        self.interacted |= request.interacted;
        true
    }

    fn can_refine(&self, elapsed: Duration, foreground: bool) -> bool {
        self.ready && !self.interacted && elapsed <= REFINE_DEADLINE && foreground
    }
}

#[derive(Clone, Copy)]
pub(super) struct ClipboardPlacement {
    owner: Option<RECT>,
    work: RECT,
    dpi: u32,
    candidate: Option<ClipboardAnchor>,
    side: Option<ClipboardPopupSide>,
}

struct LayoutSession {
    gate: LayoutGate,
    started: Instant,
    clipboard_mode: bool,
    cache_size: bool,
    clipboard: Option<ClipboardPlacement>,
}

#[derive(Default)]
struct LayoutState {
    session: Option<LayoutSession>,
    // 只记录剪贴板真实布局尺寸，不复用普通搜索或 AI 窗口遗留的高度。
    clipboard_size: Option<(f64, f64)>,
}

static LAYOUT: Mutex<LayoutState> = Mutex::new(LayoutState {
    session: None,
    clipboard_size: None,
});

pub(super) fn begin(revision: u64, clipboard_mode: bool, clipboard: Option<ClipboardPlacement>) {
    LAYOUT.lock().unwrap_or_else(|e| e.into_inner()).session = Some(LayoutSession {
        gate: LayoutGate {
            revision,
            ..Default::default()
        },
        started: Instant::now(),
        clipboard_mode,
        cache_size: clipboard_mode,
        clipboard,
    });
}

pub(super) fn end() {
    LAYOUT.lock().unwrap_or_else(|e| e.into_inner()).session = None;
}

pub fn main_window_revision() -> u64 {
    super::windows::main_window_revision()
}

pub fn main_window_layout_state() -> MainWindowLayoutState {
    let clipboard = LAYOUT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .session
        .as_ref()
        .is_some_and(|s| s.clipboard_mode);
    MainWindowLayoutState {
        revision: main_window_revision(),
        visible: super::windows::is_visible(),
        clipboard,
    }
}

pub fn emit_main_window_shown(app: &AppHandle) {
    let state = main_window_layout_state();
    if state.visible {
        let _ = app.emit(EventNames::SHOWN, state);
    }
}

pub(super) fn prepare_clipboard(
    hwnd: HWND,
    owner_hwnd: Option<isize>,
    cursor: Option<(i32, i32)>,
) -> Result<ClipboardPlacement, String> {
    let owner = owner_hwnd
        .and_then(|raw| {
            let root = unsafe { GetAncestor(HWND(raw as *mut _), GA_ROOT) };
            (!root.is_invalid()).then_some(root.0 as isize)
        })
        .and_then(super::get_window_dwm_rect)
        .map(|(x, y, w, h)| RECT {
            left: x,
            top: y,
            right: x + w as i32,
            bottom: y + h as i32,
        });
    // 兜底与普通 launcher 共用唤起时鼠标所在屏；owner 仅用于可信 caret 校验。
    let (work, target_dpi) = launcher_monitor(cursor)?;
    let (width, height) = LAYOUT
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .clipboard_size
        .unwrap_or((
            super::windows::BASE_W_LOGICAL,
            super::windows::BASE_H_LOGICAL,
        ));
    let width = dpi::logical_to_physical(width, target_dpi);
    let height = dpi::logical_to_physical(height, target_dpi);
    let (x, y) = launcher_popup_origin(&work, width, height, target_dpi);
    apply_geometry(hwnd, x, y, width, height, target_dpi)?;
    tracing::debug!(x, y, width, height, "clipboard-mode: 首帧对齐普通主窗口");
    Ok(ClipboardPlacement {
        owner,
        work,
        dpi: target_dpi,
        candidate: None,
        side: None,
    })
}

/// command 的布局提交整体在主线程执行，检查版本后一次提交位置与尺寸。
pub async fn resize_main_window(
    app: &AppHandle,
    width: f64,
    height: f64,
    request: MainWindowLayoutRequest,
) -> Result<(), String> {
    if !width.is_finite() || !height.is_finite() || width <= 0.0 || height <= 0.0 {
        return Err("主窗口尺寸必须为有限正数".into());
    }
    let Some(win) = app.get_webview_window("main") else {
        return Ok(());
    };
    let hwnd = win.hwnd().map_err(|e| e.to_string())?.0 as isize;
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.run_on_main_thread(move || {
        let result = resize(HWND(hwnd as *mut _), width, height, request);
        let _ = tx.send(result);
    })
    .map_err(|e| e.to_string())?;
    rx.await.map_err(|e| e.to_string())?
}

fn resize(
    hwnd: HWND,
    width: f64,
    height: f64,
    request: MainWindowLayoutRequest,
) -> Result<(), String> {
    if request.revision != main_window_revision() || !unsafe { IsWindowVisible(hwnd).as_bool() } {
        return Ok(());
    }
    let mut state = LAYOUT.lock().unwrap_or_else(|e| e.into_inner());
    let Some(session) = state.session.as_mut() else {
        return Ok(());
    };
    let first_ready = request.ready && !session.gate.ready;
    if !session.gate.accept(&request) {
        return Ok(());
    }
    session.clipboard_mode = request.clipboard;
    if !request.clipboard {
        session.clipboard = None;
    }
    // 首搜未完成时保留首帧尺寸，不把空列表的一次测量当成布局完成。
    if request.clipboard && !request.ready {
        return Ok(());
    }
    let placement = placement_for_layout(session, hwnd, width, height)?;
    let source = placement_source(session);
    let cache_size = request.clipboard && session.cache_size && !session.gate.interacted;
    session.cache_size = false;
    drop(state);
    let applied = apply_geometry(
        hwnd,
        placement.0,
        placement.1,
        placement.2,
        placement.3,
        placement.4,
    )?;
    if request.clipboard && (applied.changed || first_ready) {
        tracing::debug!(
            revision = request.revision,
            sequence = request.sequence,
            source,
            x = applied.rect.left,
            y = applied.rect.top,
            width = applied.rect.right - applied.rect.left,
            height = applied.rect.bottom - applied.rect.top,
            changed = applied.changed,
            "clipboard-mode: 已提交窗口布局"
        );
    }
    if cache_size {
        LAYOUT
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clipboard_size = Some((width, height));
    }
    Ok(())
}

fn placement_for_layout(
    session: &mut LayoutSession,
    hwnd: HWND,
    width: f64,
    height: f64,
) -> Result<(i32, i32, i32, i32, u32), String> {
    let current = window_rect(hwnd)?;
    if let Some(clipboard) = session.clipboard.as_mut() {
        if clipboard.side.is_none()
            && session
                .gate
                .can_refine(session.started.elapsed(), foreground_is_main(hwnd))
            && let Some(anchor) = clipboard.candidate
        {
            let monitor = unsafe { MonitorFromPoint(anchor.point(), MONITOR_DEFAULTTONEAREST) };
            if let Ok((work, target_dpi)) = monitor_info(monitor)
                && anchor.is_usable(&work, clipboard.owner.as_ref(), target_dpi)
            {
                let h = dpi::logical_to_physical(height, target_dpi);
                clipboard.side = Some(caret::clipboard_popup_side(
                    &anchor,
                    h,
                    &work,
                    clipboard.owner.as_ref(),
                ));
                clipboard.work = work;
                clipboard.dpi = target_dpi;
                tracing::debug!(revision = session.gate.revision, source = anchor.source(),
                    ?anchor, side = ?clipboard.side, "clipboard-mode: 采用可信文本定位");
            }
        }
        let w = dpi::logical_to_physical(width, clipboard.dpi);
        let h = dpi::logical_to_physical(height, clipboard.dpi);
        let (x, y) = if session.gate.interacted {
            clamp_origin(current.left, current.top, w, h, &clipboard.work)
        } else if let (Some(anchor), Some(side)) = (clipboard.candidate, clipboard.side) {
            caret::compute_clipboard_popup_placement_on_side(
                &anchor,
                w,
                h,
                &clipboard.work,
                clipboard.owner.as_ref(),
                side,
            )
        } else {
            launcher_popup_origin(&clipboard.work, w, h, clipboard.dpi)
        };
        return Ok((x, y, w, h, clipboard.dpi));
    }
    let (work, current_dpi) =
        monitor_info(unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) })?;
    let w = dpi::logical_to_physical(width, current_dpi);
    let h = dpi::logical_to_physical(height, current_dpi);
    let (x, y) = clamp_origin(current.left, current.top, w, h, &work);
    Ok((x, y, w, h, current_dpi))
}

fn placement_source(session: &LayoutSession) -> &'static str {
    if session.gate.interacted {
        "interaction_locked"
    } else {
        session
            .clipboard
            .as_ref()
            .filter(|clipboard| clipboard.side.is_some())
            .and_then(|clipboard| clipboard.candidate)
            .map(|anchor| anchor.source())
            .unwrap_or("launcher_fallback")
    }
}

/// 慢 UIA 只生产候选；候选与布局就绪可按任意顺序到达，无需 sleep/poll。
pub fn spawn_main_window_caret_refine(
    app: &AppHandle,
    elem: Option<crate::platform::selection::SendableElement>,
) {
    let Some(elem) = elem else { return };
    let revision = main_window_revision();
    let Some(win) = app.get_webview_window("main") else {
        return;
    };
    let Ok(hwnd) = win.hwnd() else { return };
    let raw = hwnd.0 as isize;
    let app = app.clone();
    if let Err(error) = std::thread::Builder::new()
        .name("main-caret".into())
        .spawn(move || {
            let Some(anchor) = caret::clipboard_anchor_from_captured(&elem) else {
                return;
            };
            if let Err(error) = app.run_on_main_thread(move || {
                if main_window_revision() != revision {
                    return;
                }
                let hwnd = HWND(raw as *mut _);
                if !foreground_is_main(hwnd) {
                    return;
                }
                let mut state = LAYOUT.lock().unwrap_or_else(|e| e.into_inner());
                let Some(session) = state.session.as_mut() else {
                    return;
                };
                if session.gate.revision != revision
                    || session.gate.interacted
                    || session.started.elapsed() > REFINE_DEADLINE
                {
                    return;
                }
                let Some(clipboard) = session.clipboard.as_mut() else {
                    return;
                };
                clipboard.candidate = Some(anchor);
                if !session.gate.ready {
                    return;
                }
                let Ok(rect) = window_rect(hwnd) else { return };
                let current_dpi = f64::from(dpi::get_dpi_for_hwnd(hwnd).max(1));
                let result = placement_for_layout(
                    session,
                    hwnd,
                    f64::from(rect.right - rect.left) * 96.0 / current_dpi,
                    f64::from(rect.bottom - rect.top) * 96.0 / current_dpi,
                );
                let source = placement_source(session);
                drop(state);
                if let Ok((x, y, w, h, target_dpi)) = result {
                    match apply_geometry(hwnd, x, y, w, h, target_dpi) {
                        Ok(applied) => tracing::debug!(
                            revision,
                            source,
                            x = applied.rect.left,
                            y = applied.rect.top,
                            width = applied.rect.right - applied.rect.left,
                            height = applied.rect.bottom - applied.rect.top,
                            changed = applied.changed,
                            "clipboard-mode: 已提交光标校准布局"
                        ),
                        Err(error) => {
                            tracing::debug!(%error, "clipboard-mode: 光标定位失败，保持原位")
                        }
                    }
                }
            }) {
                tracing::debug!(%error, "clipboard-mode: 光标定位调度失败");
            }
        })
    {
        tracing::debug!(%error, "clipboard-mode: 光标查询线程启动失败");
    }
}

fn foreground_is_main(hwnd: HWND) -> bool {
    unsafe {
        let fg = GetForegroundWindow();
        fg == hwnd || GetAncestor(fg, GA_ROOT) == hwnd
    }
}

fn window_rect(hwnd: HWND) -> Result<RECT, String> {
    let mut rect = RECT::default();
    unsafe { GetWindowRect(hwnd, &mut rect) }.map_err(|e| e.to_string())?;
    Ok(rect)
}

fn monitor_info(monitor: windows::Win32::Graphics::Gdi::HMONITOR) -> Result<(RECT, u32), String> {
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    unsafe { GetMonitorInfoW(monitor, &mut info) }
        .ok()
        .map_err(|e| e.to_string())?;
    Ok((info.rcWork, dpi::get_dpi_for_hmonitor(monitor).max(1)))
}

/// 普通主窗与剪贴板兜底共用选屏规则：鼠标所在屏，获取失败时主屏。
pub(super) fn launcher_monitor(cursor: Option<(i32, i32)>) -> Result<(RECT, u32), String> {
    let point = cursor.map(|(x, y)| POINT { x, y });
    let monitor = unsafe {
        MonitorFromPoint(
            point.unwrap_or_default(),
            if point.is_some() {
                MONITOR_DEFAULTTONEAREST
            } else {
                MONITOR_DEFAULTTOPRIMARY
            },
        )
    };
    monitor_info(monitor)
}

/// 搜索栏以普通 launcher 的基准高度定位，内容向下展开；只有越出工作区时
/// 才按实际窗口尺寸上移，与普通主窗口 show 后 resize/clamp 的行为一致。
pub(super) fn launcher_popup_origin(
    work: &RECT,
    width: i32,
    height: i32,
    target_dpi: u32,
) -> (i32, i32) {
    let base_height = dpi::logical_to_physical(super::windows::BASE_H_LOGICAL, target_dpi);
    clamp_origin(
        work.left + (work.right - work.left) / 2 - width / 2,
        work.top + (work.bottom - work.top) / 2 - base_height / 2,
        width,
        height,
        work,
    )
}

fn clamp_origin(x: i32, y: i32, width: i32, height: i32, work: &RECT) -> (i32, i32) {
    (
        x.clamp(work.left, (work.right - width).max(work.left)),
        y.clamp(work.top, (work.bottom - height).max(work.top)),
    )
}

struct AppliedGeometry {
    rect: RECT,
    changed: bool,
}

fn apply_geometry(
    hwnd: HWND,
    x: i32,
    y: i32,
    width: i32,
    height: i32,
    target_dpi: u32,
) -> Result<AppliedGeometry, String> {
    let original = window_rect(hwnd)?;
    let flags = SET_WINDOW_POS_FLAGS(SWP_NOZORDER.0 | SWP_NOACTIVATE.0);
    // 先完成跨屏 DPI 切换，再设置目标屏物理尺寸，避免旧屏缩放二次放大尺寸。
    if dpi::get_dpi_for_hwnd(hwnd) != target_dpi {
        unsafe {
            SetWindowPos(
                hwnd,
                None,
                x,
                y,
                0,
                0,
                SET_WINDOW_POS_FLAGS(flags.0 | SWP_NOSIZE.0),
            )
        }
        .map_err(|e| e.to_string())?;
    }
    let rect = window_rect(hwnd)?;
    let changed = (
        rect.left,
        rect.top,
        rect.right - rect.left,
        rect.bottom - rect.top,
    ) != (x, y, width, height);
    if changed {
        unsafe { SetWindowPos(hwnd, None, x, y, width, height, flags) }
            .map_err(|e| e.to_string())?;
    }
    let rect = window_rect(hwnd)?;
    Ok(AppliedGeometry {
        changed: (original.left, original.top, original.right, original.bottom)
            != (rect.left, rect.top, rect.right, rect.bottom),
        rect,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(revision: u64, sequence: u64, ready: bool) -> MainWindowLayoutRequest {
        MainWindowLayoutRequest {
            revision,
            sequence,
            ready,
            clipboard: true,
            interacted: false,
        }
    }

    #[test]
    fn old_session_and_out_of_order_layouts_are_rejected() {
        let mut gate = LayoutGate {
            revision: 7,
            ..Default::default()
        };
        assert!(!gate.accept(&request(5, 100, true)));
        assert!(gate.accept(&request(7, 2, true)));
        assert!(!gate.accept(&request(7, 1, false)));
        assert!(!gate.accept(&request(7, 2, true)));
        assert!(gate.ready);
    }

    #[test]
    fn provisional_resize_does_not_enable_refine() {
        let mut gate = LayoutGate {
            revision: 1,
            ..Default::default()
        };
        assert!(gate.accept(&request(1, 1, false)));
        assert!(!gate.can_refine(Duration::ZERO, true));
        assert!(gate.accept(&request(1, 2, true)));
        assert!(gate.can_refine(Duration::ZERO, true));
        assert!(!gate.can_refine(Duration::from_millis(251), true));
        assert!(!gate.can_refine(Duration::ZERO, false));
    }

    #[test]
    fn interaction_is_sticky_for_the_whole_session() {
        let mut gate = LayoutGate {
            revision: 1,
            ..Default::default()
        };
        let mut input = request(1, 1, true);
        input.interacted = true;
        assert!(gate.accept(&input));
        assert!(gate.accept(&request(1, 2, true)));
        assert!(!gate.can_refine(Duration::ZERO, true));
    }

    #[test]
    fn fallback_keeps_search_bar_at_launcher_position_when_list_fits() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 2560,
            bottom: 1388,
        };
        let base = launcher_popup_origin(&work, 700, 65, 96);
        assert_eq!(base, (930, 662));
        assert_eq!(launcher_popup_origin(&work, 700, 100, 96), base);
        assert_eq!(launcher_popup_origin(&work, 700, 643, 96), base);
    }

    #[test]
    fn fallback_moves_up_only_as_far_as_needed_to_fit_list() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 1920,
            bottom: 1040,
        };
        let base = launcher_popup_origin(&work, 700, 65, 96);
        assert_eq!(base, (610, 488));
        assert_eq!(launcher_popup_origin(&work, 700, 643, 96), (base.0, 397));
    }

    #[test]
    fn launcher_fallback_scales_input_anchor_with_target_monitor_dpi() {
        let work = RECT {
            left: 0,
            top: 0,
            right: 3840,
            bottom: 2160,
        };
        assert_eq!(launcher_popup_origin(&work, 1400, 130, 192), (1220, 1015));
        assert_eq!(launcher_popup_origin(&work, 1400, 1000, 192), (1220, 1015));
    }

    #[test]
    fn launcher_fallback_handles_negative_and_undersized_work_areas() {
        let work = RECT {
            left: -1920,
            top: -200,
            right: 0,
            bottom: 840,
        };
        assert_eq!(launcher_popup_origin(&work, 700, 100, 96), (-1310, 288));
        assert_eq!(launcher_popup_origin(&work, 700, 600, 96), (-1310, 240));
        assert_eq!(launcher_popup_origin(&work, 2200, 1200, 96), (-1920, -200));
    }
}
