//! 前台应用文本插入光标（caret）屏幕矩形查询（0.23.x G2 语音浮窗跟随光标）。
//!
//! 两级策略，任一成功即返回：
//! 1. `GetGUIThreadInfo`：经典 Win32 caret（记事本/原生 Edit 控件等），一次同步调用；
//! 2. UIA 文本模式：现代应用（Chromium/Electron/WPF 等）不设置经典 caret，
//!    优先 `IUIAutomationTextPattern2::GetCaretRange`（语义即 caret 的零长 range），
//!    再兜底 `IUIAutomationTextPattern::GetSelection`（空选区即 caret 位置）。
//!
//! 返回矩形为**虚拟屏幕物理像素**（与 `GetWindowRect` 同坐标系）。
//! UIA 是跨进程同步 COM 调用（几十 ms，目标进程挂起时更久），**必须在后台线程调用**
//! （同 [crate::infra::platform::uia] 模块注释）。

use windows::Win32::Foundation::{HWND, POINT, RECT};
use windows::Win32::Graphics::Gdi::ClientToScreen;
use windows::Win32::System::Ole::{
    SafeArrayAccessData, SafeArrayGetDim, SafeArrayGetLBound, SafeArrayGetUBound,
    SafeArrayUnaccessData,
};
use windows::Win32::UI::Accessibility::{
    IUIAutomationTextPattern, IUIAutomationTextPattern2, IUIAutomationTextRange,
    TextUnit_Character,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetGUIThreadInfo, GetWindowThreadProcessId, GUITHREADINFO,
};
use windows::core::Interface;

/// 查询指定窗口内当前文本光标的屏幕矩形（物理像素）。
///
/// `owner_hwnd` = 按快捷键那一刻的前台窗口（G2 的注入目标）。语音浮窗显示后
/// 前台可能已不再是目标应用（如被自家窗口/浮窗抢占），所以**不查"当前前台"**，
/// 而是锁定 owner 的 GUI 线程取经典 caret，并用进程 ID 校验 UIA 焦点元素归属。
/// `owner_hwnd = None` 时退化为查当前前台（无归属校验）。
///
/// 返回 `None` 表示取不到光标（焦点不在文本、目标应用不支持 UIA 文本模式等），
/// 调用方应保持原有降级定位（鼠标位置）。
pub fn get_caret_rect_for(owner_hwnd: Option<isize>) -> Option<RECT> {
    match caret_via_guithreadinfo(owner_hwnd) {
        Some(rect) => {
            tracing::trace!(?rect, "caret: GetGUIThreadInfo 命中");
            Some(rect)
        }
        None => match caret_via_uia(owner_hwnd) {
            Some(rect) => {
                tracing::trace!(?rect, "caret: UIA 命中");
                Some(rect)
            }
            None => {
                tracing::debug!(owner_hwnd, "caret: GUITHREADINFO 与 UIA 均未取到光标");
                None
            }
        },
    }
}

/// owner 窗口所属 GUI 线程 ID（owner 为 None 时取当前前台窗口的线程）。
fn owner_thread_id(owner_hwnd: Option<isize>) -> Option<u32> {
    let hwnd = match owner_hwnd {
        Some(raw) => HWND(raw as *mut _),
        None => unsafe { GetForegroundWindow() },
    };
    if hwnd.is_invalid() {
        return None;
    }
    let tid = unsafe { GetWindowThreadProcessId(hwnd, None) };
    (tid != 0).then_some(tid)
}

/// owner 窗口所属进程 ID（owner 为 None 时返回 None，表示不做归属校验）。
fn owner_process_id(owner_hwnd: Option<isize>) -> Option<u32> {
    let raw = owner_hwnd?;
    let hwnd = HWND(raw as *mut _);
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    (pid != 0).then_some(pid)
}

/// 经典 Win32 caret：`GetGUIThreadInfo` 读目标 GUI 线程的 caret 客户区矩形，
/// `ClientToScreen` 转屏幕坐标。
fn caret_via_guithreadinfo(owner_hwnd: Option<isize>) -> Option<RECT> {
    let Some(tid) = owner_thread_id(owner_hwnd) else {
        return None;
    };

    let mut gti = GUITHREADINFO {
        cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
        ..Default::default()
    };
    if (unsafe { GetGUIThreadInfo(tid, &mut gti) }).is_err() {
        tracing::trace!("caret: GetGUIThreadInfo 调用失败");
        return None;
    }
    // 无经典 caret 时 hwndCaret 为空 / rcCaret 退化——现代应用普遍如此，走 UIA。
    if gti.hwndCaret.is_invalid() || gti.rcCaret.bottom <= gti.rcCaret.top {
        tracing::trace!(caret_valid = !gti.hwndCaret.is_invalid(), "caret: 前台线程无经典 caret");
        return None;
    }

    let mut top_left = POINT {
        x: gti.rcCaret.left,
        y: gti.rcCaret.top,
    };
    let mut bottom_right = POINT {
        x: gti.rcCaret.right,
        y: gti.rcCaret.bottom,
    };
    if !(unsafe { ClientToScreen(gti.hwndCaret, &mut top_left) }).as_bool()
        || !(unsafe { ClientToScreen(gti.hwndCaret, &mut bottom_right) }).as_bool()
    {
        return None;
    }
    Some(RECT {
        left: top_left.x,
        top: top_left.y,
        right: bottom_right.x,
        bottom: bottom_right.y,
    })
}

/// UIA caret：焦点元素的文本模式。`GetCaretRange`（TextPattern2）语义即 caret 的
/// 零长 range；控件未实现 TextPattern2 时回退 `GetSelection`——空选区的退化
/// range 同样落在 caret。
///
/// 有 owner 时校验焦点元素进程归属：浮窗显示后系统焦点可能已移到 blink 自家
/// 窗口，`GetFocusedElement` 会返回自家元素——拒绝，宁可用鼠标降级也不定错位。
fn caret_via_uia(owner_hwnd: Option<isize>) -> Option<RECT> {
    let Some(elem) = crate::infra::platform::uia::get_focused_element() else {
        tracing::debug!("caret: UIA GetFocusedElement 失败");
        return None;
    };
    let Some(owner_pid) = owner_process_id(owner_hwnd) else {
        // owner 已失效（窗口关闭）或未指定：无归属校验，按原样继续
        return try_caret_patterns(&elem, "owner-invalid");
    };
    match unsafe { elem.CurrentProcessId() } {
        Ok(elem_pid) if elem_pid as u32 == owner_pid => try_caret_patterns(&elem, "owned"),
        Ok(elem_pid) => {
            tracing::debug!(elem_pid, owner_pid, "caret: 焦点元素不属于目标进程（焦点已被抢占）");
            None
        }
        Err(error) => {
            tracing::debug!(%error, "caret: 读取焦点元素进程 ID 失败");
            None
        }
    }
}

/// 在焦点元素上依次尝试 TextPattern2.GetCaretRange / TextPattern.GetSelection。
fn try_caret_patterns(elem: &windows::Win32::UI::Accessibility::IUIAutomationElement, tag: &str) -> Option<RECT> {
    if tracing::enabled!(tracing::Level::TRACE) {
        let class = unsafe { elem.CurrentClassName() }
            .ok()
            .map(|s| s.to_string())
            .unwrap_or_default();
        let control_type = unsafe { elem.CurrentControlType() }
            .map(|t| t.0)
            .unwrap_or(0);
        tracing::trace!(tag, control_type, class = %class, "caret: 焦点元素信息");
    }

    if let Ok(pattern) = elem.cast::<IUIAutomationTextPattern2>() {
        match unsafe { pattern.GetCaretRange(&mut Default::default()) } {
            Ok(range) => {
                if let Some(rect) = text_range_caret_rect(&range, "GetCaretRange") {
                    return Some(rect);
                }
            }
            Err(error) => tracing::trace!(%error, "caret: GetCaretRange 调用失败"),
        }
    } else {
        tracing::trace!(tag, "caret: 焦点元素不支持 TextPattern2");
    }

    let Ok(pattern) = elem.cast::<IUIAutomationTextPattern>() else {
        tracing::debug!(tag, "caret: 焦点元素不支持 TextPattern，无法取光标");
        return None;
    };
    let Ok(selections) = (unsafe { pattern.GetSelection() }) else {
        tracing::debug!(tag, "caret: GetSelection 调用失败");
        return None;
    };
    let count = unsafe { selections.Length() }.unwrap_or(0);
    (0..count).find_map(|j| {
        unsafe { selections.GetElement(j) }
            .ok()
            .and_then(|range| text_range_caret_rect(&range, "GetSelection"))
    })
}

/// 取文本 range 的包围矩形（多行 range 按行拆分，取最后一个有效矩形，即 caret 所在行）。
///
/// `GetBoundingRectangles` 在 windows crate 中返回裸 `SAFEARRAY`（UIA 约定
/// VT_R8 数组，每矩形 4 个元素 left/top/width/height）。
/// 零长 range（caret/空选区）在 Chromium 系 provider 上常返回空数组——此时
/// `ExpandToEnclosingUnit(Character)` 扩成单字符再取一次（IME 取 caret 几何的标准做法）。
fn text_range_caret_rect(range: &IUIAutomationTextRange, source: &str) -> Option<RECT> {
    if let Some(rect) = read_bounding_rects(range) {
        return Some(rect);
    }
    if (unsafe { range.ExpandToEnclosingUnit(TextUnit_Character) }).is_err() {
        tracing::debug!(source, "caret: range 无矩形且扩展失败");
        return None;
    }
    match read_bounding_rects(range) {
        Some(rect) => Some(rect),
        None => {
            tracing::debug!(source, "caret: 扩展到单字符后仍无矩形");
            None
        }
    }
}

fn read_bounding_rects(range: &IUIAutomationTextRange) -> Option<RECT> {
    let psa = unsafe { range.GetBoundingRectangles() }.ok()?;
    if unsafe { SafeArrayGetDim(psa) } != 1 {
        return None;
    }
    let lower = unsafe { SafeArrayGetLBound(psa, 1) }.ok()?;
    let upper = unsafe { SafeArrayGetUBound(psa, 1) }.ok()?;
    // 空数组（UBound = -1）或异常负下界：无矩形可用
    if upper < lower || lower < 0 {
        return None;
    }

    let mut data: *mut core::ffi::c_void = std::ptr::null_mut();
    (unsafe { SafeArrayAccessData(psa, &mut data) }).ok()?;
    let count = (upper - lower + 1) as usize;
    let rect = unsafe {
        let values = std::slice::from_raw_parts(data as *const f64, count);
        values.rchunks_exact(4).find_map(|group| {
            let (left, top, width, height) = (group[0], group[1], group[2], group[3]);
            if height <= 0.0 {
                return None;
            }
            Some(RECT {
                left: left as i32,
                top: top as i32,
                right: left as i32 + width.max(1.0) as i32,
                bottom: top as i32 + height as i32,
            })
        })
    };
    let _ = unsafe { SafeArrayUnaccessData(psa) };
    rect
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 手动探针（依赖当前前台应用，CI 跳过）：焦点一个文本编辑器后运行
    /// `cargo test --bin blink probe_foreground_caret -- --ignored --nocapture`，
    /// 输出各阶段结果，用于定位"未取到前台光标"发生在哪一环。
    #[test]
    #[ignore = "依赖真实前台应用的交互式探针"]
    fn probe_foreground_caret() {
        let fg = unsafe { GetForegroundWindow() };
        let tid = unsafe { GetWindowThreadProcessId(fg, None) };
        let mut gti = GUITHREADINFO {
            cbSize: std::mem::size_of::<GUITHREADINFO>() as u32,
            ..Default::default()
        };
        let gti_ok = (unsafe { GetGUIThreadInfo(tid, &mut gti) }).is_ok();
        println!("[1] GUITHREADINFO: ok={gti_ok} caret_valid={} rcCaret={:?}",
            !gti.hwndCaret.is_invalid(), gti.rcCaret);

        let Some(elem) = crate::infra::platform::uia::get_focused_element() else {
            println!("[2] UIA GetFocusedElement: 失败");
            return;
        };
        let class = unsafe { elem.CurrentClassName() }.map(|s| s.to_string()).unwrap_or_default();
        let control_type = unsafe { elem.CurrentControlType() }.map(|t| t.0).unwrap_or(0);
        println!("[2] 焦点元素: control_type={control_type} class={class}");

        match elem.cast::<IUIAutomationTextPattern2>() {
            Ok(pattern) => match unsafe { pattern.GetCaretRange(&mut Default::default()) } {
                Ok(range) => {
                    let rects = read_bounding_rects(&range);
                    println!("[3] TextPattern2.GetCaretRange: rects={rects:?}");
                    if (unsafe { range.ExpandToEnclosingUnit(TextUnit_Character) }).is_ok() {
                        let expanded = read_bounding_rects(&range);
                        println!("[3] 扩展单字符后: rects={expanded:?}");
                    }
                }
                Err(e) => println!("[3] TextPattern2.GetCaretRange: 调用失败 {e}"),
            },
            Err(_) => println!("[3] TextPattern2: 不支持"),
        }

        match elem.cast::<IUIAutomationTextPattern>() {
            Ok(pattern) => match unsafe { pattern.GetSelection() } {
                Ok(sels) => {
                    let count = unsafe { sels.Length() }.unwrap_or(0);
                    println!("[4] TextPattern.GetSelection: count={count}");
                    for j in 0..count {
                        if let Ok(range) = unsafe { sels.GetElement(j) } {
                            let rects = read_bounding_rects(&range);
                            println!("[4] range[{j}]: rects={rects:?}");
                        }
                    }
                }
                Err(e) => println!("[4] TextPattern.GetSelection: 调用失败 {e}"),
            },
            Err(_) => println!("[4] TextPattern: 不支持"),
        }

        println!(
            "[0] 当前前台: hwnd={:?} pid={}",
            fg.0 as isize,
            unsafe { GetWindowThreadProcessId(fg, None) }
        );

        // [MSAA] OBJID_CARET 探测：Java/Swing 系与部分自绘编辑器的备选光标通道
        unsafe {
            use windows::Win32::System::Variant::{VARIANT, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0, VT_I4};
            use windows::Win32::UI::Accessibility::{AccessibleObjectFromWindow, IAccessible};
            use windows::Win32::UI::WindowsAndMessaging::OBJID_CARET;
            let mut pv: *mut core::ffi::c_void = std::ptr::null_mut();
            if (AccessibleObjectFromWindow(fg, OBJID_CARET.0 as u32, &IAccessible::IID, &mut pv)).is_ok()
                && !pv.is_null()
            {
                let caret: IAccessible = windows::core::Type::from_abi(pv).unwrap();
                let (mut x, mut y, mut w, mut h) = (0i32, 0i32, 0i32, 0i32);
                // varChild = CHILDID_SELF（VT_I4 0）
                let child = VARIANT {
                    Anonymous: VARIANT_0 {
                        Anonymous: core::mem::ManuallyDrop::new(VARIANT_0_0 {
                            vt: VT_I4,
                            wReserved1: 0,
                            wReserved2: 0,
                            wReserved3: 0,
                            Anonymous: VARIANT_0_0_0 { lVal: 0 },
                        }),
                    },
                };
                match caret.accLocation(&mut x, &mut y, &mut w, &mut h, &child) {
                    Ok(()) => println!("[MSAA] OBJID_CARET: x={x} y={y} w={w} h={h}"),
                    Err(e) => println!("[MSAA] OBJID_CARET: accLocation 失败 {e}"),
                }
            } else {
                println!("[MSAA] OBJID_CARET: 无对象");
            }
        }

        let fg_raw = fg.0 as isize;
        println!(
            "[5] get_caret_rect_for(owner=前台): {:?}",
            get_caret_rect_for(Some(fg_raw))
        );
        println!("[5] get_caret_rect_for(None): {:?}", get_caret_rect_for(None));
    }
}
