//! 平台抽象层：热键、窗口、本地化、上下文采集、选区抓取、剪贴板监听、截图、密钥、音频采集、文本注入、Python 环境
//!
//! 0.14.6 §2.3：收纳从 domain 泄漏的 Win32 调用——icon 提取 / shell 枚举 / lock。

pub mod audio;
pub mod clipboard;
pub mod context;
#[cfg(windows)]
pub mod dpi;
pub mod hotkey;
#[cfg(windows)]
pub mod icon; // 0.14.6 §2.3：从 domain/search/icon.rs 迁入
pub mod inject;
pub mod locale;
pub mod lock; // 0.14.6 §2.3：从 domain/execution/builtin.rs 迁入
pub mod ocr; // 0.14.7 W2：从 domain/capability/builtins/ocr_engine.rs 迁入
pub mod process;
pub mod python;
pub mod screenshot;
pub mod secret;
pub mod selection;
pub mod shell; // 0.14.6 §2.3：从 domain/search/windows.rs 迁入
#[cfg(windows)]
pub mod uia;
pub mod window;

// ── 子进程窗口抑制（CREATE_NO_WINDOW）─────────────────────────────────────

/// Windows CreateProcess 标志：不创建控制台窗口。
///
/// Blink 是 GUI 应用（Tauri），从 GUI 进程 spawn 控制台子进程时
/// Windows 会为每个子进程创建一个新的控制台窗口，导致用户看到闪烁的
/// 黑色终端窗口。设置此标志可抑制该行为。
#[cfg(windows)]
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;

/// 为 `std::process::Command` 设置 CREATE_NO_WINDOW（仅 Windows 生效）。
#[cfg(windows)]
pub fn no_window(mut cmd: std::process::Command) -> std::process::Command {
    use std::os::windows::process::CommandExt;
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
pub fn no_window(cmd: std::process::Command) -> std::process::Command {
    cmd
}

/// 为 `tokio::process::Command` 设置 CREATE_NO_WINDOW（仅 Windows 生效）。
#[cfg(windows)]
pub fn no_window_tokio(mut cmd: tokio::process::Command) -> tokio::process::Command {
    cmd.creation_flags(CREATE_NO_WINDOW);
    cmd
}

#[cfg(not(windows))]
pub fn no_window_tokio(cmd: tokio::process::Command) -> tokio::process::Command {
    cmd
}
