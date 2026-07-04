//! Windows 平台选区抓取：UIA TextPattern。
//!
//! API 路径：
//!   CUIAutomation → ElementFromHandle(顶层 HWND) → FindAll(子树, TextPattern 可用)
//!   → 遍历所有 Edit/Document 候选 → GetCurrentPattern(TextPattern) → GetSelection()
//!   → 取首个非空 TextRange → GetText()。
//!
//! 为何要 FindAll 而非 FindFirst：顶层窗口本身无 TextPattern，且子树里往往有多个
//! 支持 TextPattern 的元素（如 Chrome 的地址栏 Edit + 网页正文 Document）。FindFirst
//! 只取第一个，常命中无实际选区的元素（地址栏）。FindAll 遍历找「有非空选区」的那个才准。
//! 该方法不依赖焦点时机——show 后窗口仍在、子树结构与选区内容不变，适合后台 spawn_blocking。
//!
//! 局限：Scintilla(Notepad3)/Java Swing 等控件不暴露 UIA TextPattern，子树里一个候选都没有，
//! 这类 UIA 无解，只能靠 Ctrl+C 兜底（文档 §1.1 初期不做，属「明确不支持」）。
//!
//! COM 公寓用 MTA（UIA 官方建议），与图标提取那条 STA 路径互不影响。
//! 日志：失败用 debug（诊断哪些应用抓不到），成功路径细节用 trace（验证完毕后降噪）。

use windows::core::Interface;
use windows::Win32::Foundation::HWND;
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Accessibility::{
    CUIAutomation, IUIAutomation, IUIAutomationTextPattern, PropertyConditionFlags_None,
    TreeScope_Subtree, UIA_IsTextPatternAvailablePropertyId, UIA_TextPatternId,
};

// 用于 hwnd → 进程名 的 Win32 调用（隐私门控：见 listener.rs on_selection）
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use windows::Win32::Foundation::{HANDLE, MAX_PATH};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
    PROCESS_NAME_WIN32,
};
use windows::Win32::UI::WindowsAndMessaging::GetWindowThreadProcessId;

/// COM 初始化 RAII guard（MTA），与 icon.rs 的 ComGuard 同款范式。
struct ComGuard {
    should_uninit: bool,
}

impl ComGuard {
    fn init_mta() -> Self {
        // 线程已是其他公寓（如 STA）时返回 RPC_E_CHANGED_MODE，此时不该由我们 uninit。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_ok() {
            tracing::trace!("CoInit MTA OK");
            ComGuard { should_uninit: true }
        } else {
            // 不阻断：UIA 在已有公寓（如 STA）下也能工作，只是建议 MTA。
            tracing::debug!(hr = hr.0, "CoInit MTA 失败（线程已是其他公寓），继续尝试");
            ComGuard { should_uninit: false }
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

/// 抓取指定窗口当前的鼠标选区文本。
///
/// 任意环节失败均返回 None，绝不抛错。每步 debug 级留痕便于诊断。
pub(crate) fn get_selected_text(hwnd_raw: isize) -> Option<String> {
    if hwnd_raw == 0 {
        tracing::debug!("选区抓取：hwnd 为 0，跳过");
        return None;
    }
    // 生命周期：_com 最先声明、最后析构，确保 COM 对象 Release 完成后再 CoUninitialize。
    let _com = ComGuard::init_mta();

    let automation: IUIAutomation =
        match unsafe { CoCreateInstance(&CUIAutomation, None, CLSCTX_ALL) } {
            Ok(a) => a,
            Err(e) => {
                tracing::debug!(error = %e, "选区抓取：CoCreateInstance(CUIAutomation) 失败");
                return None;
            }
        };

    let root =
        match unsafe { automation.ElementFromHandle(HWND(hwnd_raw as *mut std::ffi::c_void)) } {
            Ok(e) => e,
            Err(e) => {
                tracing::debug!(error = %e, "选区抓取：ElementFromHandle 失败");
                return None;
            }
        };

    // 子树查找第一个支持 TextPattern 的元素（Edit/Document 控件）。
    let cond = match unsafe {
        automation.CreatePropertyConditionEx(
            UIA_IsTextPatternAvailablePropertyId,
            &VARIANT::from(true),
            PropertyConditionFlags_None,
        )
    } {
        Ok(c) => c,
        Err(e) => {
            tracing::debug!(error = %e, "选区抓取：CreatePropertyConditionEx 失败");
            return None;
        }
    };

    // FindAll 收集所有支持 TextPattern 的元素，遍历找有非空选区的那个。
    let candidates = match unsafe { root.FindAll(TreeScope_Subtree, &cond) } {
        Ok(a) => a,
        Err(_) => {
            tracing::debug!("选区抓取：FindAll 失败");
            return None;
        }
    };
    let total = unsafe { candidates.Length() }.unwrap_or(0);
    if total == 0 {
        tracing::debug!("选区抓取：子树中无支持 TextPattern 的元素");
        return None;
    }
    tracing::trace!(candidates = total, "选区抓取：FindAll 候选元素数");

    // 性能保护：大子树（如浏览器）候选可能很多，限定最多扫描数，避免遍历过久。
    const MAX_CANDIDATES: i32 = 64;
    for i in 0..total.min(MAX_CANDIDATES) {
        let elem = match unsafe { candidates.GetElement(i) } {
            Ok(e) => e,
            Err(e) => {
                tracing::trace!(error = %e, index = i, "选区抓取：候选 GetElement 失败，跳过");
                continue;
            }
        };

        let pattern: IUIAutomationTextPattern =
            match unsafe { elem.GetCurrentPattern(UIA_TextPatternId) } {
                Ok(p) => match p.cast::<IUIAutomationTextPattern>() {
                    Ok(tp) => tp,
                    Err(_) => continue,
                },
                Err(_) => continue,
            };

        let sels = match unsafe { pattern.GetSelection() } {
            Ok(s) => s,
            Err(_) => continue,
        };
        let c = unsafe { sels.Length() }.unwrap_or(0);
        if c == 0 {
            continue;
        }

        // 取该元素首个非空 TextRange
        for j in 0..c {
            let range = match unsafe { sels.GetElement(j) } {
                Ok(r) => r,
                Err(_) => continue,
            };
            if let Ok(text) = unsafe { range.GetText(-1) } {
                let text = text.to_string();
                if !text.trim().is_empty() {
                    let ct = unsafe { elem.CurrentControlType() }
                        .map(|t| t.0)
                        .unwrap_or(0);
                    tracing::trace!(
                        index = i,
                        control_type = ct,
                        "选区抓取：命中带选区的元素"
                    );
                    return Some(text);
                }
            }
        }
    }

    tracing::debug!(candidates = total, "选区抓取：所有候选均无非空选区");
    None
}

/// 由 HWND 查前台窗口所属进程名（如 "Bitwarden.exe"）。抓不到返回 None。
/// 用于划词感知的隐私门控：`on_selection` 调它决定是否跳过抓取。
///
/// 独立于 `infra::platform::context` 的同名 helper——避免 selection 反向依赖 context 平台层。
/// TODO(0.9 awareness 重构)：把这类 Win32 helper 统一挪进 `infra::platform::awareness::foreground`
/// 供各通道复用。
pub(crate) fn process_name_of_window(hwnd_raw: isize) -> Option<String> {
    if hwnd_raw == 0 {
        return None;
    }
    unsafe {
        let hwnd = HWND(hwnd_raw as *mut std::ffi::c_void);
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            return None;
        }
        let Ok(hprocess) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let mut path_buf = vec![0u16; MAX_PATH as usize];
        let mut path_len = path_buf.len() as u32;
        if QueryFullProcessImageNameW(
            HANDLE(hprocess.0),
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path_buf.as_mut_ptr()),
            &mut path_len,
        )
        .is_err()
        {
            return None;
        }
        let path = OsString::from_wide(&path_buf[..path_len as usize])
            .to_string_lossy()
            .into_owned();
        Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
    }
}
