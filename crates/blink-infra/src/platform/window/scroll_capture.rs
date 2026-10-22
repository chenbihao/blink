//! 长截图输入租约与平台操作。状态与窗口操作在同一锁内排序，旧恢复任务不能影响新会话。

use tauri::Manager;

static INPUT_STATE: std::sync::Mutex<CaptureInputState> =
    std::sync::Mutex::new(CaptureInputState::new());

struct CaptureInputState {
    generation: u64,
    revision: u64,
    active: bool,
}

impl CaptureInputState {
    const fn new() -> Self {
        Self {
            generation: 0,
            revision: 0,
            active: false,
        }
    }
    fn reset(&mut self, active: bool) -> u64 {
        self.generation += 1;
        self.revision += 1;
        self.active = active;
        self.generation
    }
    fn accepts(&self, token: Option<u64>) -> bool {
        self.active && token == Some(self.generation)
    }
    fn lease(&mut self) -> (u64, u64) {
        self.revision += 1;
        (self.generation, self.revision)
    }
    fn owns(&self, ticket: (u64, u64)) -> bool {
        self.active && ticket == (self.generation, self.revision)
    }
}

// 只在主线程执行 HWND/命中状态操作；后台线程持锁等主线程会形成死锁。
async fn on_main<T: Send + 'static>(
    app: &tauri::AppHandle,
    operation: impl FnOnce(&tauri::AppHandle) -> Result<T, String> + Send + 'static,
) -> Result<T, String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    let handle = app.clone();
    app.run_on_main_thread(move || {
        let _ = tx.send(operation(&handle));
    })
    .map_err(|e| format!("调度长截图窗口操作失败: {e}"))?;
    rx.await.map_err(|_| "长截图窗口操作已取消".to_string())?
}

pub async fn set_capture_exclusion(
    app: &tauri::AppHandle,
    exclude: bool,
    token: Option<u64>,
) -> Result<Option<u64>, String> {
    on_main(app, move |app| {
        set_capture_exclusion_on_main(app, exclude, token)
    })
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn forward_wheel(
    app: &tauri::AppHandle,
    token: Option<u64>,
    hwnd: isize,
    delta: i32,
    screen_x: i32,
    screen_y: i32,
    passthrough_ms: u64,
    position_cursor: bool,
    force_message: bool,
) -> Result<(), String> {
    on_main(app, move |app| {
        forward_wheel_on_main(
            app,
            token,
            hwnd,
            delta,
            screen_x,
            screen_y,
            passthrough_ms,
            position_cursor,
            force_message,
        )
    })
    .await
}

pub(super) fn reset_capture_input(app: &tauri::AppHandle) {
    let handle = app.clone();
    if let Err(error) = app.run_on_main_thread(move || reset_capture_input_on_main(&handle)) {
        tracing::warn!(%error, "调度截图输入重置失败");
    }
}

fn set_affinity(win: &tauri::WebviewWindow, exclude: bool) -> Result<(), String> {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
    };
    let hwnd = win.hwnd().map_err(|e| format!("获取截图 HWND 失败: {e}"))?;
    unsafe {
        SetWindowDisplayAffinity(
            HWND(hwnd.0 as _),
            if exclude {
                WDA_EXCLUDEFROMCAPTURE
            } else {
                WDA_NONE
            },
        )
    }
    .map_err(|e| format!("设置截图捕获排除失败: {e}"))
}

fn set_capture_exclusion_on_main(
    app: &tauri::AppHandle,
    exclude: bool,
    token: Option<u64>,
) -> Result<Option<u64>, String> {
    let mut state = INPUT_STATE.lock().map_err(|_| "长截图输入锁中毒")?;
    if !exclude && !state.accepts(token) {
        return Ok(None);
    }
    let win = app
        .get_webview_window("chord-screenshot")
        .ok_or("截图 overlay 窗口未找到")?;
    // 先失效租约，即使恢复窗口失败，旧任务也不能再修改后继会话。
    state.reset(false);
    win.set_ignore_cursor_events(false)
        .map_err(|e| format!("恢复截图鼠标交互失败: {e}"))?;
    set_affinity(&win, exclude)?;
    state.active = exclude;
    tracing::debug!(
        exclude,
        generation = state.generation,
        "长截图捕获会话已切换"
    );
    Ok(exclude.then_some(state.generation))
}

/// 所有原生 show/hide 路径调用；不依赖前端迟到的 cleanup IPC。
fn reset_capture_input_on_main(app: &tauri::AppHandle) {
    let Ok(mut state) = INPUT_STATE.lock() else {
        return;
    };
    state.reset(false);
    if let Some(win) = app.get_webview_window("chord-screenshot") {
        if let Err(error) = win.set_ignore_cursor_events(false) {
            tracing::warn!(%error, "重置截图鼠标交互失败");
        }
        if let Err(error) = set_affinity(&win, false) {
            tracing::warn!(%error, "重置截图捕获排除失败");
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn forward_wheel_on_main(
    app: &tauri::AppHandle,
    token: Option<u64>,
    hwnd: isize,
    delta: i32,
    screen_x: i32,
    screen_y: i32,
    passthrough_ms: u64,
    position_cursor: bool,
    force_message: bool,
) -> Result<(), String> {
    let mut state = INPUT_STATE.lock().map_err(|_| "长截图输入锁中毒")?;
    if !state.accepts(token) {
        return Err("长截图输入会话已失效".into());
    }
    if position_cursor {
        unsafe { windows::Win32::UI::WindowsAndMessaging::SetCursorPos(screen_x, screen_y) }
            .map_err(|e| format!("定位自动滚动光标失败: {e}"))?;
    }
    if force_message {
        return post_wheel_to_target(hwnd, delta, screen_x, screen_y);
    }
    if let Err(error) = inject_wheel_through_overlay(app, delta, passthrough_ms, &mut state) {
        tracing::warn!(%error, "滚轮注入失败，回退到窗口消息");
        return post_wheel_to_target(hwnd, delta, screen_x, screen_y);
    }
    Ok(())
}

/// overlay 会截获首个滚轮事件。开启一个短时连续穿透窗口并用
/// SendInput 补发首个事件，后续真实滚轮可直达底层应用。这避免每一格
/// 都切换 overlay hit-test 并阻塞线程，同时兼容忽略 WM_MOUSEWHEEL 的自绘窗口。
fn inject_wheel_through_overlay(
    app: &tauri::AppHandle,
    delta: i32,
    passthrough_ms: u64,
    state: &mut CaptureInputState,
) -> Result<(), String> {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_MOUSE, MOUSEEVENTF_WHEEL, MOUSEINPUT, SendInput,
    };

    let overlay = app
        .get_webview_window("chord-screenshot")
        .ok_or_else(|| "截图 overlay 窗口未找到".to_string())?;
    overlay
        .set_ignore_cursor_events(true)
        .map_err(|e| format!("开启 overlay 鼠标穿透失败: {e}"))?;

    let wheel_delta = delta.clamp(i16::MIN as i32, i16::MAX as i32);
    let input = INPUT {
        r#type: INPUT_MOUSE,
        Anonymous: INPUT_0 {
            mi: MOUSEINPUT {
                dx: 0,
                dy: 0,
                mouseData: wheel_delta as u32,
                dwFlags: MOUSEEVENTF_WHEEL,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    };
    let sent = unsafe { SendInput(&[input], std::mem::size_of::<INPUT>() as i32) };

    if sent != 1 {
        let _ = overlay.set_ignore_cursor_events(false);
        return Err("SendInput 未能注入滚轮事件".to_string());
    }
    let ticket = state.lease();
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(passthrough_ms)).await;
        if let Err(error) = handle.run_on_main_thread(move || {
            let Ok(state) = INPUT_STATE.lock() else {
                return;
            };
            if !state.owns(ticket) {
                return;
            }
            if let Err(error) = overlay.set_ignore_cursor_events(false) {
                tracing::warn!(%error, "恢复截图 overlay 鼠标交互失败");
            }
        }) {
            tracing::warn!(%error, "调度截图鼠标交互恢复失败");
        }
    });
    Ok(())
}

fn post_wheel_to_target(
    hwnd_val: isize,
    delta: i32,
    screen_x: i32,
    screen_y: i32,
) -> Result<(), String> {
    use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::WindowsAndMessaging::{
        CWP_SKIPDISABLED, CWP_SKIPINVISIBLE, CWP_SKIPTRANSPARENT, ChildWindowFromPointEx, IsWindow,
        PostMessageW, WM_MOUSEWHEEL,
    };

    let root = HWND(hwnd_val as _);
    if !unsafe { IsWindow(Some(root)) }.as_bool() {
        return Err(format!("滚动目标窗口已失效: hwnd={hwnd_val}"));
    }

    // WM_MOUSEWHEEL 正常会发给光标所在控件。overlay 截获输入后需显式沿选区中心
    // 找到最深子 HWND；只投顶层窗口对 Chromium/Electron/自绘控件通常无效。
    let mut target = root;
    for _ in 0..8 {
        let mut client_point = POINT {
            x: screen_x,
            y: screen_y,
        };
        if !unsafe { ScreenToClient(target, &mut client_point) }.as_bool() {
            break;
        }
        let child = unsafe {
            ChildWindowFromPointEx(
                target,
                client_point,
                CWP_SKIPINVISIBLE | CWP_SKIPDISABLED | CWP_SKIPTRANSPARENT,
            )
        };
        if child.is_invalid() || child == target {
            break;
        }
        target = child;
    }

    let wheel_delta = delta.clamp(i16::MIN as i32, i16::MAX as i32) as i16;
    let wparam = WPARAM(((wheel_delta as u16 as usize) & 0xFFFF) << 16);
    let packed_point = ((screen_x as u16 as u32) | ((screen_y as u16 as u32) << 16)) as isize;
    unsafe { PostMessageW(Some(target), WM_MOUSEWHEEL, wparam, LPARAM(packed_point)) }
        .map_err(|e| format!("投递 WM_MOUSEWHEEL 失败: {e}"))?;
    tracing::debug!(
        hwnd = hwnd_val,
        child_hwnd = target.0 as isize,
        delta,
        screen_x,
        screen_y,
        "已转发滚轮事件"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_restore_cannot_end_new_lease() {
        let mut state = CaptureInputState::new();
        let token = state.reset(true);
        let old = state.lease();
        let new = state.lease();
        assert!(!state.owns(old));
        assert!(state.owns(new));
        assert!(state.accepts(Some(token)));
    }

    #[test]
    fn close_reopen_rejects_old_wheel_cleanup_and_restore() {
        let mut state = CaptureInputState::new();
        let old_token = state.reset(true);
        let old_restore = state.lease();
        state.reset(false);
        assert!(!state.owns(old_restore));
        let new_token = state.reset(true);
        assert!(!state.accepts(Some(old_token)));
        assert!(!state.accepts(None));
        assert!(!state.owns(old_restore));
        assert!(state.accepts(Some(new_token)));
    }
}
