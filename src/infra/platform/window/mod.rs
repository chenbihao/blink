//! 窗口控制：平台接口 + 通用逻辑。
//!
//! 平台特定实现（如 Windows GetForegroundWindow）在对应平台模块中。

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

// 0.15.8：窗口枚举（智能吸附）
#[cfg(target_os = "windows")]
mod list;

#[cfg(target_os = "windows")]
pub use windows::{
    ack_chat_prefill, apply_cloak, center_of_active_monitor, clamp_context_menu, get_pin_image,
    clamp_to_work_area, compute_cursor_titlebar_position, destroy_sticky_window,
    enable_rounded_corners, flush_all_sticky_windows, force_topmost, get_foreground_hwnd,
    get_or_create_context_menu_window, get_primary_monitor_center, hide, hide_chat_window,
    hide_for_screenshot, hide_image_editor_window, hide_screenshot_overlay, hide_sticky_window,
    hide_voice_overlay, install_sysmenu_blocker, invoke, is_main_ai_active, is_visible,
    last_external_foreground_hwnd, mark_pin_spare_ready, mark_spare_ready, on_focused, open_settings, place_at_physical,
    preheat_secondary_windows, refresh_pin_image, restore_foreground, set_app_exiting,
    set_context_menu_payload, set_main_ai_active, show_chat_window, show_content_editor_window,
show_image_editor_window, show_pin_window, show_screenshot_overlay, show_sticky_manager_window,
show_sticky_window, show_voice_overlay, show_welcome_window, start_watchdog, take_chat_prefill,
take_context_menu_payload, unhide_after_screenshot, update_grace_period, update_sticky_taskbar,
wait_frame_after_hide, PinImage,
};

// 0.15.8：智能窗口吸附——枚举可吸附窗口
#[cfg(target_os = "windows")]
pub use list::{PickableWindow, enumerate_pickable_windows, get_window_dwm_rect};

// 0.18.2：控件级智能吸附——UIA 控件提示
#[cfg(target_os = "windows")]
pub use crate::infra::platform::uia::ControlHint;
