//! Windows 文本注入实现。
//!
//! ## 方案 A: SendInput Unicode（0.10.3 默认）
//!
//! 用 `SendInput` 的 `KEYEVENTF_UNICODE` flag 逐字符发送 `WM_CHAR`，绕过剪贴板：
//!
//! 1. 将文本编码为 UTF-16
//! 2. 每个 UTF-16 码元构造一个 `INPUT { wScan: code_unit, dwFlags: KEYEVENTF_UNICODE }`
//! 3. 换行符 `\n` / `\r\n` 用 `VK_RETURN` 代替
//! 4. 批量 `SendInput`（Windows 限制单次最多 ~544 个 INPUT 结构体，分批发送）
//!
//! - ✅ 完全不碰剪贴板
//! - ⚠️ 少数 IME 激活的应用可能把 Unicode 输入当候选词
//! - **回退策略**：失败时回退到 Clipboard+Ctrl+V
//!
//! ## 方案 B: Clipboard + Ctrl+V（0.10.1~0.10.2）
//!
//! 时序:
//! 1. 备份当前剪贴板文本(若可读)
//! 2. 设置剪贴板为 STT 文本
//! 3. SendInput: Ctrl↓ → V↓ → V↑ → Ctrl↑
//! 4. 等待 100ms 让前台应用处理粘贴
//! 5. 恢复原剪贴板文本
//!
//! ## 注意事项
//!
//! - `SendInput` 要求调用线程有 UI 权限(非服务进程)。Blink 是用户态 app,OK。
//! - `KEYEVENTF_EXTENDEDKEY` 只用于扩展键(Insert/Delete/Home/End/PgUp/PgDn/方向键/
//!   Numpad/右Ctrl/右Alt),不可用于普通字母键和左Ctrl,否则系统将按键解释为
//!   Numpad 版本导致 Ctrl+V 不生效。
//! - `SendInput` 单次调用上限为 ~544 个 INPUT（内部缓冲区），超出需分批。

use std::time::Duration;

use windows::Win32::Foundation::HGLOBAL;
use windows::Win32::System::DataExchange::*;
use windows::Win32::System::Memory::*;
use windows::Win32::System::Ole::*;
use windows::Win32::UI::Input::KeyboardAndMouse::*;

use super::InjectError;

// ── 方案 A: SendInput Unicode ────────────────────────────────────────────

/// Windows `SendInput` 单次调用的最大 INPUT 结构体数量。
///
/// Windows 内部缓冲区上限约 544 个，留余量取 500。
const SENDINPUT_BATCH_LIMIT: usize = 500;

/// 通过 `SendInput` + `KEYEVENTF_UNICODE` 逐字符注入文本。
///
/// 完全不碰剪贴板。换行符用 `VK_RETURN` 代替。
/// 超出 BMP 的字符（如 emoji）通过 UTF-16 代理对发送。
pub fn inject_text_unicode(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    tracing::debug!(len = text.chars().count(), "inject_text_unicode: 开始注入");

    // 编码为 UTF-16，逐码元构造 INPUT
    let utf16: Vec<u16> = text.encode_utf16().collect();

    let mut inputs: Vec<INPUT> = Vec::with_capacity(utf16.len() * 2); // 每字符 keydown + keyup

    for &code_unit in &utf16 {
        if code_unit == b'\n' as u16 || code_unit == b'\r' as u16 {
            // 换行符用 VK_RETURN 代替（KEYEVENTF_UNICODE 发 \n 很多应用不认）
            inputs.push(make_keydown(VK_RETURN.0 as u16, KEYBD_EVENT_FLAGS(0)));
            inputs.push(make_keyup(VK_RETURN.0 as u16, KEYEVENTF_KEYUP));
        } else {
            // 普通字符：KEYEVENTF_UNICODE 逐码元发送
            inputs.push(make_keydown(code_unit, KEYEVENTF_UNICODE));
            inputs.push(make_keyup(code_unit, KEYEVENTF_UNICODE | KEYEVENTF_KEYUP));
        }
    }

    // 分批发送（Windows 限制单次 SendInput 数量）
    for chunk in inputs.chunks(SENDINPUT_BATCH_LIMIT) {
        let sent = unsafe { SendInput(chunk, std::mem::size_of::<INPUT>() as i32) };
        if sent != chunk.len() as u32 {
            let err = format!(
                "SendInput Unicode 只发送了 {}/{} 个 INPUT",
                sent,
                chunk.len()
            );
            tracing::error!(%err, "inject_text_unicode 失败");
            return Err(InjectError::SendInput(err));
        }
    }

    // 等待一小段时间让应用处理输入
    std::thread::sleep(Duration::from_millis(30));

    tracing::debug!("inject_text_unicode: 注入完成");
    Ok(())
}

/// 构造 keydown INPUT（Unicode 字符）。
fn make_keydown(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

/// 构造 keyup INPUT（Unicode 字符）。
fn make_keyup(scan: u16, flags: KEYBD_EVENT_FLAGS) -> INPUT {
    INPUT {
        r#type: INPUT_KEYBOARD,
        Anonymous: INPUT_0 {
            ki: KEYBDINPUT {
                wVk: VIRTUAL_KEY(0),
                wScan: scan,
                dwFlags: flags,
                time: 0,
                dwExtraInfo: 0,
            },
        },
    }
}

// ── 方案 B: Clipboard + Ctrl+V ──────────────────────────────────────────

/// 通过剪贴板 + Ctrl+V 注入文本到前台应用。
pub fn inject_text_clipboard(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    tracing::debug!(
        len = text.chars().count(),
        "inject_text_clipboard: 开始注入"
    );

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

    tracing::debug!("inject_text_clipboard: 注入完成");
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

        let handle =
            GlobalAlloc(GMEM_MOVEABLE, byte_size).map_err(|e| format!("GlobalAlloc: {e}"))?;
        let ptr = GlobalLock(handle);
        if ptr.is_null() {
            let _ = GlobalUnlock(handle);
            return Err("GlobalLock failed".into());
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr() as *const u8, ptr as *mut u8, byte_size);
        let _ = GlobalUnlock(handle);

        SetClipboardData(
            CF_UNICODETEXT.0 as u32,
            Some(windows::Win32::Foundation::HANDLE(handle.0)),
        )
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
