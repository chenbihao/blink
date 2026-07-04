//! Win32 剪贴板监听：隐藏消息窗口 + AddClipboardFormatListener + WM_CLIPBOARDUPDATE。
//!
//! 监听线程跑独立消息循环（GetMessageW），WM_CLIPBOARDUPDATE 时读剪贴板文本，
//! 经黑名单过滤（`data::clipboard::is_blacklisted`）后 spawn 异步存（`save_item`）+ cleanup。

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, OpenClipboard,
    RemoveClipboardFormatListener,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowTextW, RegisterClassW, TranslateMessage, MSG, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};

use crate::infra::data::clipboard::{self, ClipboardItem};

use super::{is_active, state};

pub(super) fn start_watcher_thread() {
    if let Err(e) = std::thread::Builder::new()
        .name("blink-clipboard-watcher".into())
        .spawn(watcher_main)
    {
        tracing::warn!(?e, "剪贴板监听线程启动失败");
    }
}

fn watcher_main() {
    unsafe {
        let class_name = w!("BlinkClipboardListener");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(std::ptr::null_mut()),
            lpszClassName: class_name,
            ..Default::default()
        };
        let _ = RegisterClassW(&wc);
        let hwnd = match CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!(""),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            None,
            None,
            None,
            None,
        ) {
            Ok(h) => h,
            Err(e) => {
                tracing::warn!(?e, "CreateWindowExW 剪贴板监听窗口失败");
                return;
            }
        };
        if let Err(e) = AddClipboardFormatListener(hwnd) {
            tracing::warn!(?e, "AddClipboardFormatListener 失败");
            return;
        }
        tracing::debug!("剪贴板监听窗口已注册");
        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        let _ = RemoveClipboardFormatListener(hwnd);
    }
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        if is_active() {
            on_clipboard_change();
        }
        return LRESULT(0);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

fn on_clipboard_change() {
    // 黑名单：前台窗口标题命中则不记（密码管理器等）
    if let Some(title) = foreground_title()
        && let Some(s) = state()
    {
        let bl = s.blacklist.read().unwrap();
        if clipboard::is_blacklisted(&title, &bl) {
            tracing::debug!(title = %title, "剪贴板：前台黑名单，跳过");
            return;
        }
    }
    let Some(text) = read_clipboard_text() else { return };
    if text.trim().is_empty() {
        return;
    }
    let Some(s) = state() else { return };
    let item = ClipboardItem {
        id: clipboard::generate_id(),
        preview: clipboard::make_preview(&text),
        text,
        created_at: now_ts(),
        source_app: None,
        hit_count: 0,
    };
    let pool = s.pool.clone();
    let max_items = s.max_items;
    tauri::async_runtime::spawn(async move {
        if let Err(e) = clipboard::save_item(&pool, &item).await {
            tracing::warn!(?e, "剪贴板 save_item 失败");
            return;
        }
        let _ = clipboard::cleanup_excess(&pool, max_items).await;
    });
}

fn now_ts() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

fn foreground_title() -> Option<String> {
    unsafe {
        let hwnd = GetForegroundWindow();
        if hwnd.0.is_null() {
            return None;
        }
        let mut buf = [0u16; 512];
        let len = GetWindowTextW(hwnd, &mut buf);
        if len <= 0 {
            return None;
        }
        Some(String::from_utf16_lossy(&buf[..len as usize]))
    }
}

fn read_clipboard_text() -> Option<String> {
    unsafe {
        if OpenClipboard(None).is_err() {
            return None;
        }
        let res = read_clipboard_inner();
        let _ = CloseClipboard();
        res
    }
}

unsafe fn read_clipboard_inner() -> Option<String> {
    let handle = GetClipboardData(CF_UNICODETEXT.0.into()).ok()?;
    let hg = HGLOBAL(handle.0);
    let ptr = GlobalLock(hg);
    if ptr.is_null() {
        let _ = GlobalUnlock(hg);
        return None;
    }
    let result = PCWSTR(ptr as *const u16).to_string().ok();
    let _ = GlobalUnlock(hg);
    result
}
