//! Windows 文本注入实现:Clipboard + Ctrl+V。
//!
//! 时序:
//! 1. 备份当前剪贴板文本(若可读)
//! 2. 设置剪贴板为 STT 文本
//! 3. SendInput: Ctrl↓ → V↓ → V↑ → Ctrl↑
//! 4. 等待 50ms 让前台应用处理粘贴
//! 5. 恢复原剪贴板文本
//!
//! ## 注意事项
//!
//! - `SendInput` 要求调用线程有 UI 权限(非服务进程)。Blink 是用户态 app,OK。
//! - 粘贴后等待 50ms 是经验值:太短(<30ms)应用来不及处理,太长影响感知延迟。
//! - 剪贴板恢复可能被其他进程的剪贴板监听器干扰(竞态),但 hold-to-talk 场景下
//!   用户不太可能在 50ms 内另操作剪贴板。
//! - **KEYEVENTF_EXTENDEDKEY 只用于扩展键**(Insert/Delete/Home/End/PgUp/PgDn/方向键/
//!   Numpad/右Ctrl/右Alt),不可用于普通字母键和左Ctrl,否则系统将按键解释为
//!   Numpad 版本导致 Ctrl+V 不生效。

use std::time::Duration;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use super::InjectError;

/// 通过剪贴板 + Ctrl+V 注入文本到前台应用。
pub fn inject_text_clipboard(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    tracing::debug!(len = text.chars().count(), "inject_text: 开始注入");

    // 1. 备份当前剪贴板文本
    let backup = read_clipboard_text();

    // 2. 设置剪贴板为 STT 文本
    set_clipboard_text(text).map_err(|e| InjectError::Clipboard(e))?;

    // 3. SendInput: Ctrl↓ → V↓ → V↑ → Ctrl↑
    send_paste().map_err(|e| InjectError::SendInput(e))?;

    // 4. 等待前台应用处理粘贴（100ms：Electron/Office 等需要更多时间）
    std::thread::sleep(Duration::from_millis(100));

    // 5. 恢复原剪贴板文本
    if let Some(original) = backup {
        let _ = set_clipboard_text(&original);
    }

    tracing::debug!("inject_text: 注入完成");
    Ok(())
}

/// 读取当前剪贴板文本(若为文本格式)。
fn read_clipboard_text() -> Option<String> {
    unsafe {
        // OleInitialize 确保剪贴板可用(可能已由 Tauri 初始化,OleInitialize 可重入)
        let _ = OleInitialize(None);

        if !OpenClipboard(None).is_ok() {
            return None;
        }
        let _guard = ClipboardGuard;

        let handle = GetClipboardData(CF_UNICODETEXT.0 as u32).ok()?;
        let hg = HGLOBAL(handle.0);
        let ptr = GlobalLock(hg);
        if ptr.is_null() {
            let _ = GlobalUnlock(hg);
            return None;
        }

        let wide = windows::core::PCWSTR(ptr as *const u16);
        Some(wide.to_string().unwrap_or_default())
    }
}

/// 设置剪贴板文本。
fn set_clipboard_text(text: &str) -> Result<(), String> {
    unsafe {
        let _ = OleInitialize(None);

        if !OpenClipboard(None).is_ok() {
            return Err("OpenClipboard failed".into());
        }
        let _guard = ClipboardGuard;

        EmptyClipboard().map_err(|e| format!("EmptyClipboard: {e}"))?;

        // 分配全局内存存放 wide string
        let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0u16)).collect();
        let byte_size = wide.len() * 2;

        let handle = GlobalAlloc(GMEM_MOVEABLE, byte_size).map_err(|e| format!("GlobalAlloc: {e}"))?;
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            let _ = GlobalUnlock(handle);
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, byte_size);
        let _ = GlobalUnlock(handle);

        SetClipboardData(CF_UNICODETEXT.0 as u32, Some(windows::Win32::Foundation::HANDLE(handle.0)))
            .map_err(|e| format!("SetClipboardData: {e}"))?;
    }
    Ok(())
}

/// 发送 Ctrl+V 按键序列。
///
/// **关键**: `KEYEVENTF_EXTENDEDKEY` 只用于扩展键(右Ctrl/右Alt/Insert/Delete等),
/// 左Ctrl 和字母键 V 绝不能带此 flag,否则系统将 V 解释为 Numpad_V,
/// 导致 Ctrl+V 组合不生效。
fn send_paste() -> Result<(), String> {
    unsafe {
        let inputs = [
            // Ctrl↓ — 左Ctrl 不是扩展键,dwFlags = 0
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_LCONTROL,
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            // V↓ — 普通字母键,dwFlags = 0
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0x56), // 'V'
                        wScan: 0,
                        dwFlags: KEYBD_EVENT_FLAGS(0),
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            // V↑ — 仅 KEYEVENTF_KEYUP
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VIRTUAL_KEY(0x56), // 'V'
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
            // Ctrl↑ — 仅 KEYEVENTF_KEYUP
            INPUT {
                r#type: INPUT_KEYBOARD,
                Anonymous: INPUT_0 {
                    ki: KEYBDINPUT {
                        wVk: VK_LCONTROL,
                        wScan: 0,
                        dwFlags: KEYEVENTF_KEYUP,
                        time: 0,
                        dwExtraInfo: 0,
                    },
                },
            },
        ];

        let sent = SendInput(&inputs, std::mem::size_of::<INPUT>() as i32);
        if sent != inputs.len() as u32 {
            return Err(format!("SendInput only sent {sent}/{}", inputs.len()));
        }
    }
    Ok(())
}

/// RAII guard: 确保 CloseClipboard 被调用。
struct ClipboardGuard;
impl Drop for ClipboardGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseClipboard();
        }
    }
}
