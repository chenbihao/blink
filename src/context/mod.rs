//! Context 层：唤起时的系统环境快照。
//!
//! 设计（MVP §13.7）：
//! - 低频采集 + 按需快照：不持续监控，仅在唤起瞬间采集一次
//! - 敏感内容仅驻内存，不入 SQLite
//! - 内容：前台应用、剪贴板、选中文本（P2）
//!
//! 数据流：
//!   热键 → window::invoke(app) → context::collect() →
//!   SearchService.update_snapshot() → IntentRouter / SearchEngine / Plugin

use std::time::Instant;

/// 唤起瞬间的系统上下文快照。
///
/// 快照是不可变的，仅用于单次搜索生命周期，不持久化。
/// 所有字段均为 Option，采集失败时为 None（静默降级）。
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    /// 采集时间（用于调试日志）
    pub captured_at: Instant,
    /// 前台应用信息（唤起时的前台，不是 Blink 自身）
    pub foreground_app: Option<ForegroundAppInfo>,
    /// 剪贴板文本（截断 200 字符，Phase 2）
    pub clipboard_text: Option<String>,
}

impl Default for ContextSnapshot {
    fn default() -> Self {
        ContextSnapshot {
            captured_at: Instant::now(),
            foreground_app: None,
            clipboard_text: None,
        }
    }
}

/// 前台应用信息。
#[derive(Debug, Clone)]
pub struct ForegroundAppInfo {
    /// 进程名（如 "code.exe"）
    pub process_name: String,
    /// 窗口标题（如 "main.rs - blink"）
    pub window_title: String,
    /// 完整 exe 路径（需要权限时可能为 None）
    pub exe_path: Option<String>,
}

/// 平台接口：采集上下文快照。
///
/// Windows 实现见 `windows.rs`。
pub fn collect() -> ContextSnapshot {
    ContextSnapshot {
        captured_at: Instant::now(),
        foreground_app: collect_foreground_app(),
        clipboard_text: collect_clipboard_text(),
    }
}

#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "windows")]
use self::windows::{collect_clipboard_text, collect_foreground_app};

#[cfg(not(target_os = "windows"))]
fn collect_foreground_app() -> Option<ForegroundAppInfo> {
    None
}

#[cfg(not(target_os = "windows"))]
fn collect_clipboard_text() -> Option<String> {
    None
}
