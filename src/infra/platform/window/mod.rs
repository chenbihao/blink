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

// 0.15.8：窗口枚举（智能吸附）
#[cfg(target_os = "windows")]
mod list;

#[cfg(target_os = "windows")]
pub use windows::{
    apply_cloak, clamp_context_menu, clamp_to_work_area, enable_rounded_corners, force_topmost,
    get_foreground_hwnd, hide, hide_chat_window, hide_for_screenshot, hide_screenshot_overlay,
    hide_voice_overlay, install_sysmenu_blocker, invoke, is_visible, on_focused, open_settings,
    place_at_physical, preheat_secondary_windows, restore_foreground, show_chat_window,
    show_pin_window, show_screenshot_overlay, show_voice_overlay, start_watchdog,
    last_external_foreground_hwnd,
    unhide_after_screenshot, update_grace_period, wait_frame_after_hide,
};

// 0.15.8：智能窗口吸附——枚举可吸附窗口
#[cfg(target_os = "windows")]
pub use list::{enumerate_pickable_windows, PickableWindow};
