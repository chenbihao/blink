//! UI Automation (UIA) 公共原语。
//!
//! 从 `selection/windows.rs` 抽取，供 selection（划词抓取）和 inject（G2 焦点恢复）共用。
//!
//! 核心能力：
//! - `get_focused_element()`：跨进程获取前台焦点 UIA 元素
//! - `set_focused_element()`：跨进程恢复焦点到指定元素
//! - `focused_control_type()`：获取焦点控件类型（判断是否文本输入框）
//!
//! COM 公寓用 MTA（UIA 官方建议）。所有函数在后台线程调用（UIA 是跨进程 COM 调用，单次几十 ms）。

use windows::Win32::System::Com::{
    CLSCTX_ALL, COINIT_MULTITHREADED, CoCreateInstance, CoInitializeEx, CoUninitialize,
};
use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation, IUIAutomationElement};

// ── COM 初始化 RAII ──────────────────────────────────────────────────────

/// COM MTA 初始化 RAII guard。
///
/// 构造时 `CoInitializeEx(MTA)`，析构时 `CoUninitialize`。
/// 线程已是其他公寓（如 STA）时不 uninit（避免破坏调用方的公寓状态）。
pub(crate) struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    pub fn init_mta() -> Self {
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            ComGuard {
                should_uninit: true,
            }
        } else {
            tracing::debug!(hr = hr.0, "CoInit MTA 失败（线程已是其他公寓），继续尝试");
            ComGuard {
                should_uninit: false,
            }
        }
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        if self.should_uninit {
            unsafe { CoUninitialize() };
        }
    }
}

/// 创建 UIA 实例。调用方需已初始化 COM（MTA）。
fn create_automation() -> Option<IUIAutomation> {
    match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) } {
        Ok(a) => Some(a),
        Err(e) => {
            tracing::debug!(error = %e, "CoCreateInstance(CUIAutomation) 失败");
            None
        }
    }
}

// ── 公共 API ──────────────────────────────────────────────────────────────

/// 获取前台应用的焦点 UIA 元素（跨进程）。
///
/// 在后台线程调用。返回 None 表示获取失败或无前台窗口。
/// COM MTA 自动初始化/释放（内部用 ComGuard）。
pub fn get_focused_element() -> Option<IUIAutomationElement> {
    let _com = ComGuard::init_mta();
    let automation = create_automation()?;
    unsafe { automation.GetFocusedElement() }.ok()
}

/// 恢复焦点到指定 UIA 元素（跨进程）。
///
/// 返回 true 表示成功。调用方需已初始化 COM（MTA）。
pub fn set_focused_element(elem: &IUIAutomationElement) -> bool {
    unsafe { elem.SetFocus() }.is_ok()
}

/// 获取前台焦点元素的控件类型 ID。
///
/// 用于判断焦点是否在文本输入控件上（如 `UIA_EditControlTypeId`、
/// `UIA_DocumentControlTypeId`）。返回 None 表示获取失败。
#[allow(dead_code)]
pub fn focused_control_type() -> Option<i32> {
    let elem = get_focused_element()?;
    unsafe { elem.CurrentControlType() }.map(|t| t.0).ok()
}

/// 获取前台焦点元素的类名（诊断用）。
#[allow(dead_code)]
pub fn focused_class_name() -> Option<String> {
    let elem = get_focused_element()?;
    unsafe { elem.CurrentClassName() }
        .ok()
        .map(|s| s.to_string())
}

// ── 文本输入控件类型判断 ────────────────────────────────────────────────

/// UIA 控件类型 ID 常量（来自 windows crate 的 CONTROLTYPE_ID）。
///
/// 见 https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-controltype-ids
const UIA_EDIT_CONTROL_TYPE_ID: i32 = 50004;
const UIA_DOCUMENT_CONTROL_TYPE_ID: i32 = 50036;
const UIA_EDIT2_CONTROL_TYPE_ID: i32 = 50089; // Chromium 内部 "Edit" 变体

/// 判断指定控件类型 ID 是否属于文本输入控件。
///
/// 覆盖原生 Win32 Edit、UWP/WinUI TextBox（Document）、Chromium 输入框（Edit2）。
pub fn is_text_input_control(control_type_id: i32) -> bool {
    control_type_id == UIA_EDIT_CONTROL_TYPE_ID
        || control_type_id == UIA_DOCUMENT_CONTROL_TYPE_ID
        || control_type_id == UIA_EDIT2_CONTROL_TYPE_ID
}

/// 判断当前前台焦点是否在文本输入控件上。
///
/// 在后台线程调用。返回 false 表示获取失败或焦点不在文本输入框。
#[allow(dead_code)]
pub fn is_focused_on_text_input() -> bool {
    focused_control_type()
        .map(|ct| is_text_input_control(ct))
        .unwrap_or(false)
}
