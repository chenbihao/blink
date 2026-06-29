//! 全局热键：平台接口 + 通用逻辑。
//!
//! 平台特定实现（如 Windows WH_KEYBOARD_LL）在对应平台模块中。
//!
//! TODO: 方案 B - 平台抽象 trait
//!
//! 当需要支持多平台时，可以将热键抽象为 trait：
//!
//! ```rust
//! pub trait HotkeyManager {
//!     fn start(&self, config: &HotkeyConfig, tap_threshold: u64) -> mpsc::UnboundedReceiver<HotkeyEvent>;
//!     fn update_config(&self, config: HotkeyConfig);
//!     fn update_tap_threshold(&self, threshold: u64);
//!     fn stop(&self);
//! }
//!
//! // 每个平台实现自己的 HotkeyManager
//! pub struct WindowsHotkeyManager { /* WH_KEYBOARD_LL */ }
//! pub struct MacosHotkeyManager { /* CGEventTap */ }
//! pub struct LinuxHotkeyManager { /* X11/Wayland */ }
//! ```

use std::sync::{OnceLock, RwLock};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::app::config::HotkeyConfig;

/// 热键事件（由 hook 线程发往主线程）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HotkeyEvent {
    /// 快捷键触发（tap），附带触发时刻用于延迟测量。
    Tap(Instant),
}

/// 热键运行时状态(配置 + tap 阈值 + 事件发送端)。
///
/// Win32 hook 回调(`windows.rs::ll_proc`)运行在 OS 直接调用的 C 函数里,无法接收
/// `&self`,只能访问进程级全局。故这里用单一 `OnceLock<HotkeyRuntime>` 收敛——
/// 把原先四散的 config / sender / tap_threshold 三个全局合并(见 0.2 设计 §1.6)。
/// 对外暴露的访问函数(get_current_config / get_tap_threshold / send_event 等)签名不变,
/// 回调与 command 层无需改动。
struct HotkeyRuntime {
    config: RwLock<HotkeyConfig>,
    tap_threshold: RwLock<u64>,
    sender: mpsc::UnboundedSender<HotkeyEvent>,
}

static RUNTIME: OnceLock<HotkeyRuntime> = OnceLock::new();

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::start_hook_thread;

// 快捷键录制
mod recorder;
pub use recorder::record_hotkey_blocking;

/// 启动热键线程，返回事件接收端。
pub fn start(config: HotkeyConfig, tap_threshold: u64) -> mpsc::UnboundedReceiver<HotkeyEvent> {
    let (tx, rx) = mpsc::unbounded_channel::<HotkeyEvent>();

    // 初始化运行时状态(config / tap_threshold / sender 合并为单一全局)
    let _ = RUNTIME.set(HotkeyRuntime {
        config: RwLock::new(config),
        tap_threshold: RwLock::new(tap_threshold),
        sender: tx,
    });

    // 启动平台特定的钩子线程
    start_hook_thread();

    rx
}

/// 更新热键配置（线程安全）。
pub fn update_config(config: HotkeyConfig) {
    if let Some(rt) = RUNTIME.get() {
        if let Ok(mut guard) = rt.config.write() {
            *guard = config;
        }
    }
}

/// 更新 tap 阈值（线程安全）。
pub fn update_tap_threshold(threshold: u64) {
    if let Some(rt) = RUNTIME.get() {
        if let Ok(mut guard) = rt.tap_threshold.write() {
            *guard = threshold;
        }
    }
}

/// 获取当前热键配置（供平台模块调用）。
pub fn get_current_config() -> HotkeyConfig {
    RUNTIME
        .get()
        .and_then(|rt| rt.config.read().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

/// 获取当前 tap 阈值（供平台模块调用）。
pub fn get_tap_threshold() -> u64 {
    RUNTIME
        .get()
        .and_then(|rt| rt.tap_threshold.read().ok().map(|g| *g))
        .unwrap_or(300)
}

/// 发送热键事件（供平台模块调用）。
pub fn send_event(event: HotkeyEvent) {
    if let Some(rt) = RUNTIME.get() {
        let _ = rt.sender.send(event);
    }
}
