//! DPI 工具函数（0.11.9）。
//!
//! 抽自原散落在 `window/windows.rs`（4 处）+ `screenshot/backend_windows.rs`
//! 的 `GetDpiForMonitor` 调用与 `(dpi.max(96) as f64) / 96.0` scale 算式。
//!
//! 进程级 PerMonitorV2 awareness 已在 `main.rs` 启动时设置，本模块的所有
//! Win32 调用返回的都是 EFFECTIVE_DPI（用户在「显示设置」里看到的缩放比例
//! 对应的 DPI，96 = 100%）。

#![cfg(windows)]
#![allow(dead_code)] // 部分函数按需调用，未必全用上

use windows::Win32::Foundation::HWND;
use windows::Win32::Graphics::Gdi::{
    MONITOR_DEFAULTTONEAREST, MonitorFromWindow,
};
use windows::Win32::UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI};

/// 默认 DPI（100% 缩放）。失败兜底值。
pub const DEFAULT_DPI: u32 = 96;

/// 取 HMONITOR 的 EFFECTIVE_DPI。失败返回 96。
///
/// 这是最底层的 getter——其他 `get_dpi_for_*` 都走它。
pub fn get_dpi_for_hmonitor(hmon: windows::Win32::Graphics::Gdi::HMONITOR) -> u32 {
    let mut dpi_x: u32 = DEFAULT_DPI;
    let mut dpi_y: u32 = DEFAULT_DPI;
    // GetDpiForMonitor 返回 HRESULT，失败时保留默认 96
    let _ = unsafe { GetDpiForMonitor(hmon, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    dpi_x.max(DEFAULT_DPI)
}

/// 取 HWND 所在屏的 EFFECTIVE_DPI。失败返回 96。
pub fn get_dpi_for_hwnd(hwnd: HWND) -> u32 {
    let hmon = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    get_dpi_for_hmonitor(hmon)
}

/// DPI → scale factor（96 → 1.0，144 → 1.5，192 → 2.0）。
///
/// 内部已 `.max(96)` 兜底，输入 0 或异常小值时返回 1.0，调用方无需重复兜底。
#[inline]
pub fn scale_factor(dpi: u32) -> f64 {
    (dpi.max(DEFAULT_DPI) as f64) / (DEFAULT_DPI as f64)
}

/// 逻辑像素（CSS）→ 物理像素。四舍五入到整数。
#[inline]
pub fn logical_to_physical(logical: f64, dpi: u32) -> i32 {
    (logical * scale_factor(dpi)).round() as i32
}

/// 物理像素 → 逻辑像素（CSS）。返回浮点，由调用方决定是否 round。
#[inline]
pub fn physical_to_logical(physical: f64, dpi: u32) -> f64 {
    physical / scale_factor(dpi)
}
