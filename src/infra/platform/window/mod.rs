//! 窗口控制：平台接口 + 通用逻辑。
//!
//! 平台特定实现（如 Windows GetForegroundWindow）在对应平台模块中。
//!
//! TODO: 方案 B - 平台抽象 trait
//!
//! 当需要支持多平台时，可以将窗口控制抽象为 trait：
//!
//! ```rust
//! pub trait WindowController {
//!     fn invoke(&self, app: &AppHandle);
//!     fn hide(&self, app: &AppHandle, reason: &str);
//!     fn start_watchdog(&self, app: AppHandle);
//!     fn on_focused(&self, focused: bool);
//! }
//!
//! // 每个平台实现自己的 WindowController
//! pub struct WindowsWindowController { /* Win32 API */ }
//! pub struct MacosWindowController { /* NSWindow */ }
//! pub struct LinuxWindowController { /* X11/Wayland */ }
//! ```

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{invoke, hide, start_watchdog, on_focused, update_grace_period, is_visible, install_sysmenu_blocker, clamp_to_work_area, enable_rounded_corners, force_topmost, show_chord_ball, hide_chord_ball, open_settings};
