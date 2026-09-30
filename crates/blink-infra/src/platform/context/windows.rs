//! Windows 平台的上下文采集实现。
//!
//! 使用 Win32 API 采集：
//! - 前台应用：GetForegroundWindow → GetWindowThreadProcessId →
//!   OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION) →
//!   QueryFullProcessImageNameW

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;

use windows::Win32::Foundation::{HANDLE, HWND, MAX_PATH};
use windows::Win32::System::DataExchange::{GetClipboardData, OpenClipboard};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Threading::{
    GetCurrentProcessId, OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
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
        // 留住句柄原始值（0.8.0 §1.1：供后台 UIA 选区抓取用），HWND 的裸指针转 isize。
        let hwnd_raw = hwnd.0 as isize;

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
                hwnd: hwnd_raw,
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
                hwnd: hwnd_raw,
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
            hwnd: hwnd_raw,
        })
    }
}

/// 列出当前有可见窗口的运行中进程（进程名去重、按名排序）。
/// 供设置页「敏感应用」选择器：用户从实际运行的程序里挑，避免手输错进程名。
pub(super) fn list_window_processes() -> Vec<super::RunningProcess> {
    use std::collections::BTreeSet;
    use windows::Win32::Foundation::LPARAM;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, IsWindowVisible};
    use windows::core::BOOL;

    // EnumWindows 回调：收集 (进程名, 窗口标题)。
    // edition 2024：unsafe fn 体内默认非 unsafe 上下文，需显式 unsafe {} 块。
    unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
        unsafe {
            let out = &mut *(lparam.0 as *mut Vec<(String, String)>);
            // 只收可见窗口
            if !IsWindowVisible(hwnd).as_bool() {
                return BOOL(1);
            }
            // 过滤无标题窗口（后台/托盘等）
            let title_len = GetWindowTextLengthW(hwnd);
            if title_len == 0 {
                return BOOL(1);
            }
            let mut title_buf = vec![0u16; (title_len + 1) as usize];
            let n = GetWindowTextW(hwnd, &mut title_buf);
            if n == 0 {
                return BOOL(1);
            }
            let window_title = OsString::from_wide(&title_buf[..n as usize])
                .to_string_lossy()
                .into_owned();
            // PID
            let mut pid: u32 = 0;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == 0 || pid == GetCurrentProcessId() {
                return BOOL(1);
            }
            // 进程名
            let Some(name) = process_name_of(pid) else {
                return BOOL(1);
            };
            if !name.is_empty() {
                out.push((name, window_title));
            }
            BOOL(1)
        }
    }

    unsafe {
        let mut all: Vec<(String, String)> = Vec::new();
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut all as *mut _ as isize));
        // 进程名去重（同名多窗口只留首个标题），按进程名（小写）排序
        let mut seen: BTreeSet<String> = BTreeSet::new();
        let mut result: Vec<super::RunningProcess> = Vec::new();
        for (name, title) in all {
            if seen.insert(name.clone()) {
                result.push(super::RunningProcess {
                    process_name: name,
                    window_title: title,
                });
            }
        }
        result.sort_by(|a, b| {
            a.process_name
                .to_ascii_lowercase()
                .cmp(&b.process_name.to_ascii_lowercase())
        });
        result
    }
}

/// 由 PID 取进程名（OpenProcess + QueryFullProcessImageNameW + 取文件名）。None 表示拿不到。
unsafe fn process_name_of(pid: u32) -> Option<String> {
    unsafe {
        let Ok(hprocess) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            return None;
        };
        let mut path_buf = vec![0u16; MAX_PATH as usize];
        let mut path_len = path_buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            HANDLE(hprocess.0),
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(path_buf.as_mut_ptr()),
            &mut path_len,
        );
        if ok.is_err() {
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

        if text.is_empty() { None } else { Some(text) }
    }
}
