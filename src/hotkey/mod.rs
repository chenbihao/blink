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

use std::sync::{Arc, OnceLock, RwLock};
use std::time::Instant;

use tokio::sync::mpsc;

use crate::config::HotkeyConfig;

/// 热键事件（由 hook 线程发往主线程）。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub enum HotkeyEvent {
    /// 快捷键触发（tap），附带触发时刻用于延迟测量。
    Tap(Instant),
}

/// 共享的热键配置状态。
static HOTKEY_CONFIG: OnceLock<Arc<RwLock<HotkeyConfig>>> = OnceLock::new();

/// 全局事件发送端。
static SENDER: OnceLock<mpsc::UnboundedSender<HotkeyEvent>> = OnceLock::new();

/// 共享的 tap 阈值配置。
static TAP_THRESHOLD: OnceLock<Arc<RwLock<u64>>> = OnceLock::new();

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
    let _ = SENDER.set(tx);

    // 初始化共享配置
    let config = Arc::new(RwLock::new(config));
    let _ = HOTKEY_CONFIG.set(config);

    // 初始化 tap 阈值
    let threshold = Arc::new(RwLock::new(tap_threshold));
    let _ = TAP_THRESHOLD.set(threshold);

    // 启动平台特定的钩子线程
    start_hook_thread();

    rx
}

/// 更新热键配置（线程安全）。
pub fn update_config(config: HotkeyConfig) {
    if let Some(shared) = HOTKEY_CONFIG.get() {
        if let Ok(mut guard) = shared.write() {
            *guard = config;
        }
    }
}

/// 更新 tap 阈值（线程安全）。
pub fn update_tap_threshold(threshold: u64) {
    if let Some(shared) = TAP_THRESHOLD.get() {
        if let Ok(mut guard) = shared.write() {
            *guard = threshold;
        }
    }
}

/// 获取当前热键配置（供平台模块调用）。
pub fn get_current_config() -> HotkeyConfig {
    HOTKEY_CONFIG
        .get()
        .and_then(|c| c.read().ok().map(|g| g.clone()))
        .unwrap_or_default()
}

/// 获取当前 tap 阈值（供平台模块调用）。
pub fn get_tap_threshold() -> u64 {
    TAP_THRESHOLD
        .get()
        .and_then(|t| t.read().ok().map(|g| *g))
        .unwrap_or(300)
}

/// 发送热键事件（供平台模块调用）。
pub fn send_event(event: HotkeyEvent) {
    if let Some(tx) = SENDER.get() {
        let _ = tx.send(event);
    }
}
