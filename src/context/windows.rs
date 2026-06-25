//! Windows 平台的上下文采集实现。
//!
//! 使用 Win32 API 采集：
//! - 前台应用：GetForegroundWindow → GetWindowThreadProcessId →
//!   OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) →
//!   QueryFullProcessImageNameW

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows::Win32::Foundation::{HANDLE, MAX_PATH, HWND};
use windows::Win32::System::DataExchange::{GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetForegroundWindow, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
};

/// CF_UNICODETEXT 剪贴板格式
const CF_UNICODETEXT: u32 = 13;

use super::ForegroundAppInfo;

/// 采集前台应用信息（pub 供 mod.rs use）。
///
/// **注意**：必须在 Blink 窗口 show() 之前调用，否则拿到的是 Blink 自己。
pub(super) fn collect_foreground_app() -> Option<ForegroundAppInfo> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            tracing::trace!("GetForegroundWindow 返回 NULL");
            return None;
        }

        // 拿窗口标题（先拿长度，再拿内容）
        let title_len = GetWindowTextLengthW(hwnd);
        let mut title_buf = vec![0u16; (title_len + 1) as usize];
        let title_len = GetWindowTextW(hwnd, &mut title_buf);
        let window_title = if title_len > 0 {
            OsString::from_wide(&title_buf[..title_len as usize])
                .to_string_lossy()
                .into_owned()
        } else {
            String::new()
        };

        // 拿进程 ID
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            tracing::trace!("GetWindowThreadProcessId 返回 pid=0");
            return Some(ForegroundAppInfo {
                process_name: String::new(),
                window_title,
                exe_path: None,
            });
        }

        // 是 Blink 自己？（理论上不会，因为采集在 show 之前，但防御性检查）
        let self_pid = GetCurrentProcessId();
        if pid == self_pid {
            tracing::trace!("前台是 Blink 自己，跳过");
            return None;
        }

        // 打开进程（PROCESS_QUERY_LIMITED_INFORMATION = 无需提升权限）
        let Ok(hprocess) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            tracing::trace!(pid, "OpenProcess 失败");
            return Some(ForegroundAppInfo {
                process_name: String::new(),
                window_title,
                exe_path: None,
            });
        };

        // 拿 exe 路径
        let mut path_buf = vec![0u16; MAX_PATH as usize];
        let mut path_len = path_buf.len() as u32;
        let exe_path = if QueryFullProcessImageNameW(
            HANDLE(hprocess.0),
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path_buf.as_mut_ptr()),
            &mut path_len,
        )
        .is_ok()
        {
            let path = OsString::from_wide(&path_buf[..path_len as usize])
                .to_string_lossy()
                .into_owned();
            Some(path)
        } else {
            tracing::trace!(pid, "QueryFullProcessImageNameW 失败");
            None
        };

        // 拿进程名（从 exe 路径提取文件名）
        let process_name = exe_path
            .as_ref()
            .and_then(|p| Path::new(p).file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();

        Some(ForegroundAppInfo {
            process_name,
            window_title,
            exe_path,
        })
    }
}

/// 采集剪贴板文本（只读，不修改剪贴板内容）。
///
/// 使用 Windows 原生剪贴板 API：OpenClipboard → GetClipboardData →
/// GlobalLock → 读取 → GlobalUnlock → CloseClipboard。
/// 如果剪贴板被其他应用锁定或不含文本，返回 None。
pub(super) fn collect_clipboard_text() -> Option<String> {
    unsafe {
        // 打开剪贴板（NULL 表示与当前任务关联）
        if OpenClipboard(Some(HWND(std::ptr::null_mut()))).is_err() {
            tracing::trace!("OpenClipboard 失败（被其他应用锁定）");
            return None;
        }

        // 获取 Unicode 文本
        let Ok(hdata) = GetClipboardData(CF_UNICODETEXT) else {
            tracing::trace!("剪贴板不含 Unicode 文本");
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
            return None;
        };
        if hdata.0.is_null() {
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
            return None;
        }

        // 锁定内存获取指针
        let data_ptr = GlobalLock(windows::Win32::Foundation::HGLOBAL(hdata.0));
        if data_ptr.is_null() {
            tracing::trace!("GlobalLock 失败");
            let _ = windows::Win32::System::DataExchange::CloseClipboard();
            return None;
        }

        // 读取 UTF-16 字符串（直到遇到 NULL 终止符）
        let mut len = 0;
        while *(data_ptr as *const u16).add(len) != 0 {
            len += 1;
            if len >= 200 {
                break; // 截断 200 字符
            }
        }

        let slice = std::slice::from_raw_parts(data_ptr as *const u16, len);
        let text = String::from_utf16_lossy(slice);

        // 解锁 + 关闭
        let _ = GlobalUnlock(windows::Win32::Foundation::HGLOBAL(hdata.0));
        let _ = windows::Win32::System::DataExchange::CloseClipboard();

        if text.is_empty() {
            None
        } else {
            Some(text)
        }
    }
}
