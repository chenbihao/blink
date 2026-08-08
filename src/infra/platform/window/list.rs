//! 0.15.8 选区体验增强：智能窗口吸附后端。
//!
//! `enumerate_pickable_windows()` 枚举桌面上可见、有标题、非工具窗口的顶层窗口，
//! 返回它们的 DWM 扩展边框（`DWMWA_EXTENDED_FRAME_BOUNDS`）物理矩形 + 标题 + 进程名。
//!
//! 前端截图 overlay 在选区拖拽阶段调用此接口，拿到窗口列表后做 hit-test：
//! 鼠标悬停某窗口区域 → 显示虚线框；单击 → 自动吸附选区到该窗口矩形。
//!
//! **过滤规则**：
//! - 跳过不可见窗口（`IsWindowVisible = false`）
//! - 跳过被 DWM Cloak 的窗口（UWP 最小化后的"鬼影"、Cloaked 的工具窗口）
//! - 跳过工具窗口（`WS_EX_TOOLWINDOW`）与不激活窗口（`WS_EX_NOACTIVATE`）
//! - 跳过空标题窗口（无标题的浮层/overlay，不具可辨识性）
//! - 跳过系统桌面（标题为 `Program Manager` 的 Progman 窗口）
//! - 跳过完全在虚拟屏幕之外的窗口
//!
//! **自身进程窗口**：不按进程过滤。截图 overlay（`chord-screenshot`）标题为空，
//! 已被空标题规则过滤；主窗截图时已 cloak+hide，`IsWindowVisible` 为 false。
//! 其他 Blink 窗口（便签、设置、对话等）有标题且可见时**应可被吸附**。
//!
//! **返回顺序**：`EnumWindows` 按 Z-order 从前景到背景枚举，结果数组索引 0 = 最前景窗口。
//! 前端从索引 0 开始正序遍历，第一个命中即为最前景匹配。
//!
//! **为什么用 `DWMWA_EXTENDED_FRAME_BOUNDS` 而非 `GetWindowRect`**：
//! Windows 10/11 的无边框窗口（如 Edge Chromium、Explorer）的实际可视边框
//! 比 `GetWindowRect` 返回的"Windows 7 兼容阴影框"小 7-8px。
//! `DWMWA_EXTENDED_FRAME_BOUNDS` 返回 DWM 合成后的真实可视边框，
//! 与用户看到的窗口边框一致，吸附精度更高。

use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::Graphics::Dwm::{
    DWMWA_CLOAKED, DWMWA_EXTENDED_FRAME_BOUNDS, DwmGetWindowAttribute,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GWL_EXSTYLE, GetSystemMetrics, GetWindowLongPtrW, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, SM_CXVIRTUALSCREEN,
    SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
};
use windows::core::BOOL;

/// 可吸附窗口的几何信息（物理像素，虚拟屏幕坐标系）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct PickableWindow {
    /// 窗口句柄（isize，供前端唯一标识）
    pub hwnd: isize,
    /// DWM 扩展边框左上角 X（虚拟屏幕物理像素）
    pub x: i32,
    /// Y
    pub y: i32,
    /// 宽
    pub w: i32,
    /// 高
    pub h: i32,
    /// 窗口标题
    pub title: String,
    /// 进程名（不含扩展名）
    pub process_name: String,
}

thread_local! {
    /// EnumWindows 回调收集器
    static ENUM_BUF: std::cell::RefCell<Vec<PickableWindow>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// 枚举所有可吸附的桌面窗口。
///
/// **调用时机**：截图 overlay 加载时调一次（`screenshot_window_list` command），
/// 前端缓存结果供 mousemove hit-test 用。不在 mousemove 里逐帧调用，避免 Win32
/// 枚举的 ~5-15ms 延迟。
pub fn enumerate_pickable_windows() -> Vec<PickableWindow> {
    ENUM_BUF.with(|buf| buf.borrow_mut().clear());

    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(0));
    }

    ENUM_BUF.with(|buf| buf.borrow_mut().drain(..).collect())
}

/// 获取指定窗口的 DWM 扩展边框矩形（物理像素，虚拟屏幕坐标系）。
///
/// 供 `screenshot { op: window }`（0.19.3）使用——AI 从 `list_windows` 拿到 hwnd 后，
/// 截取该窗口区域。返回 `(x, y, w, h)`：虚拟屏幕左上角坐标 + 宽高。
///
/// **为什么用 `DWMWA_EXTENDED_FRAME_BOUNDS`**：与 `enumerate_pickable_windows` 一致，
/// 返回 DWM 合成后的真实可视边框（Windows 10/11 无边框窗口比 `GetWindowRect` 小 7-8px）。
///
/// 返回 `None` 表示窗口无效、DWM 不可用或窗口尺寸为零。
pub fn get_window_dwm_rect(hwnd: isize) -> Option<(i32, i32, u32, u32)> {
    let hwnd = HWND(hwnd as *mut _);
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if !hr.is_ok() {
        return None;
    }
    let w = rect.right - rect.left;
    let h = rect.bottom - rect.top;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some((rect.left, rect.top, w as u32, h as u32))
}

unsafe extern "system" fn enum_proc(hwnd: HWND, _lparam: LPARAM) -> BOOL {
    // ── 可见性过滤 ──
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() {
        return BOOL(1);
    }

    // ── 窗口样式过滤：WS_VISIBLE 不够（被其他窗口完全遮挡的也 WS_VISIBLE），
    //    但 IsWindowVisible 已过滤了 SW_HIDE 的。这里再查 WS_EX_TOOLWINDOW。
    let ex_style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    if ex_style & (WS_EX_TOOLWINDOW.0 as isize) != 0 {
        return BOOL(1);
    }
    // 0.15.8 R1：跳过不激活窗口（WS_EX_NOACTIVATE）——悬浮球等不参与激活的窗口不应吸附
    if ex_style & (WS_EX_NOACTIVATE.0 as isize) != 0 {
        return BOOL(1);
    }

    // ── DWM Cloak 过滤 ──
    // 被 Cloak 的窗口（UWP 挂起、虚拟桌面隐藏等）WS_VISIBLE 仍为 true 但实际不可见。
    let mut cloaked: i32 = 0;
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut i32 as *mut _,
            std::mem::size_of::<i32>() as u32,
        )
    };
    if hr.is_ok() && cloaked != 0 {
        return BOOL(1);
    }

    // ── 标题过滤：无标题的窗口不具可辨识性 ──
    let title_len = unsafe { GetWindowTextLengthW(hwnd) };
    if title_len == 0 {
        return BOOL(1);
    }

    // ── DWM 扩展边框 ──
    let mut rect: RECT = unsafe { std::mem::zeroed() };
    let hr = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_EXTENDED_FRAME_BOUNDS,
            &mut rect as *mut RECT as *mut _,
            std::mem::size_of::<RECT>() as u32,
        )
    };
    if !hr.is_ok() {
        // DWM 不可用时回退到 GetWindowRect（含阴影边框，精度差但总比没有强）
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
        }
    }

    // 零尺寸窗口跳过
    if rect.right <= rect.left || rect.bottom <= rect.top {
        return BOOL(1);
    }

    // 0.15.8 R1：跳过完全在虚拟屏幕之外的窗口
    let vs_left = unsafe { GetSystemMetrics(SM_XVIRTUALSCREEN) };
    let vs_top = unsafe { GetSystemMetrics(SM_YVIRTUALSCREEN) };
    let vs_right = vs_left + unsafe { GetSystemMetrics(SM_CXVIRTUALSCREEN) };
    let vs_bottom = vs_top + unsafe { GetSystemMetrics(SM_CYVIRTUALSCREEN) };
    if rect.right <= vs_left
        || rect.left >= vs_right
        || rect.bottom <= vs_top
        || rect.top >= vs_bottom
    {
        return BOOL(1);
    }

    // ── 进程 PID（供进程名读取用，不按进程过滤）──
    // 截图 overlay 标题为空已被上面的 title_len == 0 过滤；
    // 主窗截图时已 cloak+hide（IsWindowVisible = false）。
    // 其他 Blink 窗口（便签/设置/对话等）有标题且可见时应可被吸附。
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return BOOL(1);
    }

    // ── 读取标题 ──
    let mut title_buf = [0u16; 512];
    let len = unsafe { GetWindowTextW(hwnd, &mut title_buf) };
    let title = if len > 0 {
        String::from_utf16_lossy(&title_buf[..len as usize])
    } else {
        return BOOL(1);
    };

    // 0.15.8 R1：排除系统桌面（Progman，标题固定为 "Program Manager"）
    if title == "Program Manager" {
        return BOOL(1);
    }

    // ── 进程名 ──
    let process_name = get_process_name(pid).unwrap_or_else(|| String::new());

    ENUM_BUF.with(|buf| {
        buf.borrow_mut().push(PickableWindow {
            hwnd: hwnd.0 as isize,
            x: rect.left,
            y: rect.top,
            w: rect.right - rect.left,
            h: rect.bottom - rect.top,
            title,
            process_name,
        });
    });

    BOOL(1) // 继续枚举
}

/// 通过 PID 获取进程名（不含扩展名）。
///
/// 用 `OpenProcess + QueryFullProcessImageNameW` 取路径，再提取文件名。
/// 失败时返回 None（调用方跳过该窗口的进程名显示）。
fn get_process_name(pid: u32) -> Option<String> {
    use std::ffi::OsString;
    use std::os::windows::ffi::OsStringExt;
    use windows::Win32::Foundation::{CloseHandle, MAX_PATH};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };

    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        // HANDLE 是裸 Win32 资源；把所有可能失败的步骤包进闭包，确保最后统一关闭。
        let result = (|| {
            let mut buf = vec![0u16; MAX_PATH as usize];
            let mut len = buf.len() as u32;
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            )
            .ok()?;
            let path = OsString::from_wide(&buf[..len as usize])
                .to_string_lossy()
                .into_owned();
            std::path::Path::new(&path)
                .file_stem()?
                .to_str()
                .map(str::to_string)
        })();
        let _ = CloseHandle(handle);
        result
    }
}
