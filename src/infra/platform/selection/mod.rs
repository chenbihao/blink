//! 选中文本感知（0.8.0 §1.1）：抓取鼠标选中文本。
//!
//! 两条路径：
//! 1. **划词监听**（主）：全局鼠标钩子在「划词瞬间」抓取（焦点未失、选区未退化），
//!    缓存最近选区，绕开 Electron 应用失焦退化问题。见 `listener.rs`。
//! 2. **get_selected_text**：纯 UIA 抓取原语（按 HWND），供划词监听调用。见 `windows.rs`。
//!
//! 缓存独立于 SearchService（避免 infra→domain 反向依赖），由 `window::invoke` 合并进 snapshot。

use std::sync::{OnceLock, RwLock};
use std::time::Instant;

// ── UIA 抓取原语 ──────────────────────────────────────────────

/// 抓取指定窗口当前的鼠标选区文本。
///
/// `hwnd_raw` 为窗口句柄原始值，0 视为无效。
/// 返回 None 表示：无选区、应用不支持 UIA TextPattern、或抓取任意环节失败。
/// 调用方应在后台线程中调用——UIA 是跨进程调用，单次几十 ms。
#[cfg(target_os = "windows")]
pub fn get_selected_text(hwnd_raw: isize) -> Option<String> {
    self::windows::get_selected_text(hwnd_raw)
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn get_selected_text(_hwnd_raw: isize) -> Option<String> {
    None
}

// ── 划词缓存 ──────────────────────────────────────────────────
//
// 划词监听在「黄金时机」抓到的选区缓存于此；invoke 唤起时读取。

struct SelectionCache {
    text: Option<String>,
    at: Option<Instant>,
}

static CACHE: OnceLock<RwLock<SelectionCache>> = OnceLock::new();

fn cache() -> &'static RwLock<SelectionCache> {
    CACHE.get_or_init(|| RwLock::new(SelectionCache { text: None, at: None }))
}

/// 缓存最近选区（划词抓取线程调用）。
pub(crate) fn set_last_selection(text: String) {
    let mut g = cache().write().unwrap();
    g.text = Some(text);
    g.at = Some(Instant::now());
}

/// 选区缓存 TTL：超过此时长视为失效（用户可能已编辑/清选）。
const SELECTION_TTL_SECS: u64 = 10;

/// 取最近选区（invoke 调用），带 TTL 过期判断。
pub fn get_last_selection() -> Option<String> {
    let g = cache().read().unwrap();
    let at = g.at?;
    if at.elapsed().as_secs() > SELECTION_TTL_SECS {
        return None;
    }
    g.text.clone()
}

// ── 划词监听（鼠标钩子） ──────────────────────────────────────

/// 启动划词监听（main 启动时调用一次）。
#[cfg(target_os = "windows")]
pub fn start_listener() {
    self::listener::start();
}

#[cfg(not(target_os = "windows"))]
#[allow(dead_code)]
pub fn start_listener() {}

#[cfg(target_os = "windows")]
mod listener;

#[cfg(target_os = "windows")]
mod windows;
