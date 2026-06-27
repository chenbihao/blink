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

use serde::Serialize;

use crate::config::ContextConfig;

/// 唤起瞬间的系统上下文快照。
///
/// 快照是不可变的，仅用于单次搜索生命周期，不持久化。
/// 所有字段均为 Option，采集失败时为 None（静默降级）。
#[derive(Debug, Clone)]
pub struct ContextSnapshot {
    /// 采集时间（用于调试日志）
    #[allow(dead_code)] // 调试用，未来可能用于性能统计
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
    #[allow(dead_code)] // 0.3+ 意图引擎可能用到
    pub exe_path: Option<String>,
}

/// 运行中的进程（用于设置页「敏感应用」选择器：从实际运行的程序里挑）。
#[derive(Debug, Clone, Serialize)]
pub struct RunningProcess {
    /// 进程名（如 "Bitwarden.exe"）
    pub process_name: String,
    /// 窗口标题（展示用，帮助用户识别）
    pub window_title: String,
}

/// 平台接口：采集上下文快照。按 ContextConfig 过滤（总开关 / 敏感应用 / 剪贴板）。
///
/// Windows 实现见 `windows.rs`。
pub fn collect(cfg: &ContextConfig) -> ContextSnapshot {
    // 总开关关闭 → 空快照（完全不采集）
    if !cfg.enabled {
        return ContextSnapshot::default();
    }
    // 先采集前台应用（剪贴板可关，但前台用于来源/意图，始终采）
    let foreground_app = collect_foreground_app();
    // 前台是敏感应用（如密码管理器）→ 整体放弃采集（隐私保护：不采剪贴板等）
    if let Some(ref fg) = foreground_app {
        if cfg.is_sensitive(&fg.process_name) {
            tracing::debug!(app = %fg.process_name, "前台为敏感应用,放弃采集上下文");
            return ContextSnapshot::default();
        }
    }
    // 剪贴板开关：关闭则不采集
    let clipboard_text = if cfg.clipboard_enabled {
        collect_clipboard_text()
    } else {
        None
    };
    // 打印采集内容（debug 级，需在设置页调日志级别到 debug 可见）
    // 注意：按 Unicode 字符数截断，不是字节数，避免切到中文字符中间导致 panic
    tracing::debug!(
        fg_process = ?foreground_app.as_ref().map(|f| &f.process_name),
        fg_title = ?foreground_app.as_ref().map(|f| &f.window_title),
        clipboard = %clipboard_text.as_ref().map(|t| {
            // 取前 80 个 Unicode 字符，不是 80 个字节
            if t.chars().count() > 80 {
                let end = t.char_indices().nth(80).map(|(i, _)| i).unwrap_or(80);
                format!("{}…", &t[..end])
            } else {
                t.clone()
            }
        }).unwrap_or_else(|| "(空)".into()),
        "上下文采集完成"
    );
    ContextSnapshot {
        captured_at: Instant::now(),
        foreground_app,
        clipboard_text,
    }
}

/// 列出当前有可见窗口的运行中进程（去重、排序）。供设置页「敏感应用」选择器使用。
pub fn list_running_processes() -> Vec<RunningProcess> {
    #[cfg(target_os = "windows")]
    {
        self::windows::list_window_processes()
    }
    #[cfg(not(target_os = "windows"))]
    {
        Vec::new()
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
