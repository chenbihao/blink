//! 平台抽象层：热键、窗口、本地化、上下文采集、选区抓取、剪贴板监听、截图、密钥、音频采集、文本注入、Python 环境

pub mod audio;
pub mod clipboard;
pub mod context;
pub mod hotkey;
pub mod inject;
pub mod locale;
pub mod python;
pub mod screenshot;
pub mod secret;
pub mod selection;
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
