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
    /// 长按开始(hold)——按住超过 tap 阈值时触发,语音录音开始。
    Hold(Instant),
    /// 长按结束(hold release)——松开已触发 Hold 的按键,语音录音停止→STT→注入。
    HoldRelease(Instant),
    /// 语音取消(ESC)——录音中按 ESC,取消录音不识别不注入。
    VoiceCancel(Instant),
    /// Chord 触发（0.10.7.2）——chord 独占模式吞键后,前端收不到 keydown,
    /// 由 hook 直接发此事件,HotkeyService 消费后调 trigger_chord 逻辑。
    /// 携带已 toLowerCase 的 chord 主键（如 `"a"` / `"c"`）。
    Chord(String),
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

// ── 0.10.7：Chord 独占模式全局状态 ─────────────────────────────────────────────
//
// **设计**：主窗 focused + Alt hold + chordEligible 时，前端调 `set_chord_mode(true)`
// 命令，后端刷新 `CHORD_KEYS`（当前生效的 tap 语义 chord 键集合）并置 `CHORD_MODE=true`。
// LL hook 在 chord mode 下，Alt 按下时吞掉 CHORD_KEYS 中的 keydown，让前端
// `onChordTrigger` 独占处理（preventDefault + trigger_chord），避免其他软件的
// 全局快捷键（如 Alt+A 截图）抢键。
//
// **退出时机**：Alt 松开 / 主窗失焦 / chordEligible 不再满足 → 前端调
// `set_chord_mode(false)`，`CHORD_MODE=false`，hook 停止吞键。
//
// **与"不吞键"铁则的关系**：0.10.5.2 回滚的是"吞 Alt keyup"（破坏 GetKeyState，
// 导致 Alt+Tab 异常）。0.10.7 吞的是"chord 键的 keydown"（字母键，非修饰键），
// 且仅在 chord mode 窗口内，Alt 本身全程放行。两者本质不同，详见
// docs/production-design/phases/0.10-voice-agent.md §10.5。

/// Chord 独占模式是否激活。LL hook 读此标志决定是否吞 chord keydown。
static CHORD_MODE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前生效的 tap 语义 chord 键集合（已 toLowerCase，如 `{"a", "c"}`）。
/// chord mode 激活时由 `set_chord_mode` 刷新。hook 据此判断哪些 keydown 要吞。
/// 用 `OnceLock<RwLock<...>>` 因 `HashSet::new()` 非 const fn，无法直接用于 static。
static CHORD_KEYS: OnceLock<RwLock<std::collections::HashSet<String>>> = OnceLock::new();

/// 初始化 CHORD_KEYS（首次 set_chord_mode 调用时自动触发）。
fn ensure_chord_keys() -> &'static RwLock<std::collections::HashSet<String>> {
    CHORD_KEYS.get_or_init(|| RwLock::new(std::collections::HashSet::new()))
}

/// 查询 chord 独占模式是否激活（供 LL hook 调用）。
pub fn is_chord_mode() -> bool {
    CHORD_MODE.load(std::sync::atomic::Ordering::SeqCst)
}

/// 查询某键是否为当前 chord 键（供 LL hook 调用）。
/// `key` 调用前已 toLowerCase。未初始化时返回 false。
pub fn is_chord_key(key: &str) -> bool {
    CHORD_KEYS
        .get()
        .and_then(|g| g.read().ok().map(|g| g.contains(key)))
        .unwrap_or(false)
}

/// 设置 chord 独占模式（前端 `set_chord_mode` 命令调用）。
///
/// - `on=true`：刷新 `CHORD_KEYS` 为当前 chord 配置的 tap 键集合，置 `CHORD_MODE=true`。
/// - `on=false`：置 `CHORD_MODE=false`，`CHORD_KEYS` 清空。
///
/// `tap_keys` 由 command 层从 `ChordRegistry::list` + bindings 派生（只取 semantic=tap）。
pub fn set_chord_mode(on: bool, tap_keys: std::collections::HashSet<String>) {
    let keys = ensure_chord_keys();
    CHORD_MODE.store(on, std::sync::atomic::Ordering::SeqCst);
    if let Ok(mut g) = keys.write() {
        if on {
            *g = tap_keys;
        } else {
            g.clear();
        }
    }
    tracing::trace!(on, "chord mode 已切换");
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
pub use windows::{
    expect_synthesized_alt_keyup, is_alt_down, set_voice_recording, start_hook_thread,
};

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
