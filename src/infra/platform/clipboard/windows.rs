//! Win32 剪贴板监听：隐藏消息窗口 + AddClipboardFormatListener + WM_CLIPBOARDUPDATE。
//!
//! 监听线程跑独立消息循环（GetMessageW），WM_CLIPBOARDUPDATE 时读剪贴板文本，
//! 经黑名单过滤（`data::clipboard::is_blacklisted`）后 spawn 异步存（`save_item`）+ cleanup。

use std::sync::Mutex;

use windows::Win32::Foundation::{HGLOBAL, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::DataExchange::{
    AddClipboardFormatListener, CloseClipboard, GetClipboardData, OpenClipboard,
    RemoveClipboardFormatListener,
};
use windows::Win32::System::Memory::{GlobalLock, GlobalUnlock};
use windows::Win32::System::Ole::CF_UNICODETEXT;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetForegroundWindow, GetMessageW,
    GetWindowTextW, MSG, RegisterClassW, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE,
    WM_CLIPBOARDUPDATE, WNDCLASSW,
};
use windows::core::{PCWSTR, w};

use crate::infra::data::clipboard::{self, ClipboardItem};

use super::{is_active, state};

/// 短窗口去重（0.8.7 修复：有些应用 Ctrl+C 会连发多次 WM_CLIPBOARDUPDATE）。
/// 记录最近一次入库的 (text_hash, ts_ms)；10 秒内同文本再来直接跳过。
static DEDUP_STATE: Mutex<Option<(u64, u128)>> = Mutex::new(None);
const DEDUP_WINDOW_MS: u128 = 10_000;

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

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_CLIPBOARDUPDATE {
        let active = is_active();
        tracing::trace!(active, "WM_CLIPBOARDUPDATE 收到");
        if active {
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
    // read_clipboard_text 返回 None 的常见原因：
    //   1) 写入方（外部应用）刚 SetClipboardData 后 CloseClipboard,我们 OpenClipboard 竞争失败
    //   2) 剪贴板内容不是 CF_UNICODETEXT（图片、文件列表等）
    //   3) GlobalLock 失败
    // 静默失败会让"为啥这次没记"变成黑盒——降到 trace 但至少有痕迹。
    let text = match read_clipboard_text() {
        Some(t) => t,
        None => {
            tracing::trace!("剪贴板：读取无文本内容或竞争失败,跳过");
            return;
        }
    };
    if text.trim().is_empty() {
        tracing::trace!("剪贴板：文本仅空白,跳过");
        return;
    }
    // 短窗口去重（0.8.7）：10 秒内同文本不重复记录。
    // Ctrl+C 在部分应用（浏览器/编辑器）会连发多次 WM_CLIPBOARDUPDATE,不去重会看到
    // 历史里同一条塞了 3~4 遍,后续 cleanup_excess 反而挤掉真正的旧条目。
    let text_hash = fx_hash(&text);
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    if let Ok(mut guard) = DEDUP_STATE.lock() {
        if let Some((last_hash, last_ms)) = *guard
            && last_hash == text_hash
            && now_ms.saturating_sub(last_ms) < DEDUP_WINDOW_MS
        {
            tracing::debug!(
                len = text.chars().count(),
                "剪贴板：10s 内同文本重复,跳过入库"
            );
            return;
        }
        *guard = Some((text_hash, now_ms));
    }
    // 0.9.2.1：入库前先触发 hook,让 SearchService.snapshot 局部刷新 Clipboard 项
    //（过滤 + 去重后触发,与真实入库同步;hook 侧再做 ContextConfig 门控与敏感应用
    // 过滤,避免密码管理器 Ctrl+C 悄悄进 snapshot）。
    super::notify_change(&text);
    let Some(s) = state() else { return };
    let preview = clipboard::make_preview(&text);
    let item = ClipboardItem {
        id: clipboard::generate_id(),
        preview: preview.clone(),
        text,
        created_at: now_ts(),
        source_app: None,
        hit_count: 0,
    };
    let pool = s.pool.clone();
    let max_items = s.max_items;
    tauri::async_runtime::spawn(async move {
        match clipboard::save_item(&pool, &item).await {
            Ok(_) => tracing::trace!(id = %item.id, preview = %preview, "剪贴板：已入库"),
            Err(e) => {
                tracing::warn!(?e, "剪贴板 save_item 失败");
                return;
            }
        }
        let _ = clipboard::cleanup_excess(&pool, max_items).await;
    });
}

/// 快哈希（FxHash 简化版）:去重仅用于短窗口冲突判断,不追求密码学强度。
fn fx_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h = h.wrapping_mul(0x100000001b3).wrapping_add(*b as u64);
    }
    h
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

/// 读当前剪贴板文本（0.9.7：公开给 read_clipboard Capability）。
///
/// 含短重试（与监听器内部同一逻辑），读文本成功返回 Some，非文本/空返回 None。
pub fn read_current_text() -> Option<String> {
    read_clipboard_text()
}

fn read_clipboard_text() -> Option<String> {
    // 短重试 + 微退避:写入方(浏览器/编辑器)刚 SetClipboardData + CloseClipboard,
    // 我们收到 WM_CLIPBOARDUPDATE 立即 OpenClipboard 有几率抢不过 —— 系统内部把
    // 剪贴板持有权切给写入方的窗口需要几毫秒。不重试会直接漏掉这次变化,导致
    // AwarenessSnapshot 不刷新(bug-2026-07-09)。
    //
    // 最多重试 5 次,每次退避 8ms,总上限 ~40ms —— clipboard listener 独立线程
    // 消息循环,阻塞可接受;不至于让消息循环卡到影响后续 WM 到达。
    const MAX_ATTEMPTS: u32 = 5;
    const BACKOFF_MS: u64 = 8;
    for attempt in 0..MAX_ATTEMPTS {
        unsafe {
            if OpenClipboard(None).is_ok() {
                let res = read_clipboard_inner();
                let _ = CloseClipboard();
                if res.is_some() {
                    if attempt > 0 {
                        tracing::debug!(attempt, "剪贴板读取重试成功");
                    }
                    return res;
                }
                // Open 成功但读文本失败(非 CF_UNICODETEXT 等) —— 不是竞争问题,不重试
                return None;
            }
        }
        // Open 失败(竞争) —— 退避后重试
        if attempt + 1 < MAX_ATTEMPTS {
            std::thread::sleep(std::time::Duration::from_millis(BACKOFF_MS));
        }
    }
    tracing::debug!(
        attempts = MAX_ATTEMPTS,
        "剪贴板 OpenClipboard 重试仍失败,放弃"
    );
    None
}

unsafe fn read_clipboard_inner() -> Option<String> {
    // Rust 2024：unsafe fn 内不再默认 unsafe 上下文，显式包块
    unsafe {
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
}

/// 把 **BGRA** 像素数据写入系统剪贴板（CF_DIB 格式）。
///
/// `pixels` 格式：BGRA、top-down、每行 `width * 4` 字节。CF_DIB 要求 BGRA + bottom-up，
/// 所以只做 top-down → bottom-up 翻转（`copy_from_slice` 整行拷贝，不再逐像素 shuffle）。
///
/// **失败清理**：`SetClipboardData` 成功前所有权始终在调用方；任何中途错误都必须
/// `GlobalFree` 释放 hmem（截图一次 ~14MB DIB，泄漏会累积）。`OpenClipboard` 成功
/// 后无论后续走哪条分支都必须 `CloseClipboard`，否则会锁住系统剪贴板一段时间。
pub fn write_bgra_to_clipboard(pixels: &[u8], width: u32, height: u32) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::Graphics::Gdi::BITMAPINFOHEADER;
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_DIB;

    let row_bytes = width as usize * 4;
    let expected = row_bytes * height as usize;
    if pixels.len() != expected {
        return Err(format!(
            "像素数据长度不匹配: {} vs {expected}",
            pixels.len()
        ));
    }

    // 构造 DIB：BITMAPINFOHEADER + BGRA 像素（CF_DIB 要求 BGRA + bottom-up）
    let header_size = std::mem::size_of::<BITMAPINFOHEADER>();
    let dib_size = header_size + expected;

    unsafe {
        let hmem =
            GlobalAlloc(GMEM_MOVEABLE, dib_size).map_err(|e| format!("GlobalAlloc 失败: {e}"))?;

        // 从这里往下的任意 early return 都必须 GlobalFree(hmem)——只有
        // SetClipboardData 成功后所有权才转给系统。
        let fill_result = (|| -> Result<(), String> {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }

            // 写 BITMAPINFOHEADER
            let header = BITMAPINFOHEADER {
                biSize: header_size as u32,
                biWidth: width as i32,
                biHeight: height as i32, // 正值 = bottom-up
                biPlanes: 1,
                biBitCount: 32,
                biCompression: 0, // BI_RGB
                biSizeImage: expected as u32,
                biXPelsPerMeter: 0,
                biYPelsPerMeter: 0,
                biClrUsed: 0,
                biClrImportant: 0,
            };
            std::ptr::copy_nonoverlapping(
                &header as *const _ as *const u8,
                ptr as *mut u8,
                header_size,
            );

            // 写像素：仅做 top-down → bottom-up 翻转（整行 memcpy，无逐像素 shuffle）
            let pixel_dst = (ptr as *mut u8).add(header_size);
            let stride = row_bytes;
            for y in 0..height as usize {
                let src_row = &pixels[y * stride..(y + 1) * stride];
                // bottom-up: 目标行 = (height - 1 - y)
                let dst_row = std::slice::from_raw_parts_mut(
                    pixel_dst.add((height as usize - 1 - y) * stride),
                    stride,
                );
                dst_row.copy_from_slice(src_row);
            }

            let _ = GlobalUnlock(hmem);
            Ok(())
        })();

        if let Err(e) = fill_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }

        // OpenClipboard 后必须成对 CloseClipboard——包一层保证任意分支都清
        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(format!("OpenClipboard 失败: {e}"));
        }

        let set_result: Result<(), String> = (|| {
            let _ = EmptyClipboard();
            SetClipboardData(CF_DIB.0.into(), Some(HANDLE(hmem.0 as _)))
                .map_err(|e| format!("SetClipboardData 失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();

        if let Err(e) = set_result {
            // SetClipboardData 失败时所有权未移交，仍需 free
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }
    }

    tracing::debug!(width, height, "截图已写入剪贴板");
    Ok(())
}

/// 把文本写入系统剪贴板（CF_UNICODETEXT 格式）（0.9.7：write_clipboard Capability）。
///
/// 与 `write_bgra_to_clipboard` 同模式：GlobalAlloc → GlobalLock → 写 UTF-16 →
/// EmptyClipboard → SetClipboardData。失败清理 GlobalFree，成对 CloseClipboard。
pub fn write_text_to_clipboard(text: &str) -> Result<(), String> {
    use windows::Win32::Foundation::{GlobalFree, HANDLE};
    use windows::Win32::System::DataExchange::{
        CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData,
    };
    use windows::Win32::System::Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock};
    use windows::Win32::System::Ole::CF_UNICODETEXT;

    // 编码 UTF-16 + null 终止符
    let mut wide: Vec<u16> = text.encode_utf16().collect();
    wide.push(0); // null terminator
    let byte_len = wide.len() * 2;

    unsafe {
        let hmem =
            GlobalAlloc(GMEM_MOVEABLE, byte_len).map_err(|e| format!("GlobalAlloc 失败: {e}"))?;

        let fill_result = (|| -> Result<(), String> {
            let ptr = GlobalLock(hmem);
            if ptr.is_null() {
                return Err("GlobalLock 失败".into());
            }
            // 写 UTF-16 字节（外层已 unsafe，内层不需再包）
            let byte_slice = std::slice::from_raw_parts(wide.as_ptr() as *const u8, byte_len);
            std::ptr::copy_nonoverlapping(byte_slice.as_ptr(), ptr as *mut u8, byte_len);
            let _ = GlobalUnlock(hmem);
            Ok(())
        })();

        if let Err(e) = fill_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }

        if let Err(e) = OpenClipboard(None) {
            let _ = GlobalFree(Some(hmem));
            return Err(format!("OpenClipboard 失败: {e}"));
        }

        let set_result: Result<(), String> = (|| {
            let _ = EmptyClipboard();
            SetClipboardData(CF_UNICODETEXT.0.into(), Some(HANDLE(hmem.0 as _)))
                .map_err(|e| format!("SetClipboardData 失败: {e}"))?;
            Ok(())
        })();
        let _ = CloseClipboard();

        if let Err(e) = set_result {
            let _ = GlobalFree(Some(hmem));
            return Err(e);
        }
    }

    tracing::debug!(len = text.chars().count(), "文本已写入剪贴板");
    Ok(())
}
