//! Awareness 域：唤起时的系统环境快照（0.8.3 收尾 · awareness 重构）。
//!
//! **重构动机**：0.8.3 落地 `Suggestion.origin` 时暴露旧 `ContextSnapshot`（平级字段
//! `selected_text` / `clipboard_text`）的架构味道 —— 消费方靠字段名认 origin,
//! 三处推断（`infer_origin` / `context_confidence.src_w` / `TextSource::extract`）
//! 需要 helper 绑一致。本重构把每条文本升为「带 source 标签的 AwarenessText」,
//! origin 从数据侧带来,intent 层零推断。
//!
//! **命名**：物理路径仍是 `infra/platform/context/`（避免 git 历史与 import 全改）;
//! 逻辑域改称 awareness 与 0.9 AI 的「意图判定」层次对齐。0.9 或 1.0 前可以做物理
//! 目录改名（`ContextSnapshot` type alias 保留一版兜底）。
//!
//! 设计（MVP §13.7 沿用）：
//! - 低频采集 + 按需快照：不持续监控，仅在唤起瞬间采集一次
//! - 敏感内容仅驻内存，不入 SQLite
//! - 内容：前台应用（元数据）、多条文本（选区 / 剪贴板 / 未来 Chord 抓的）
//!
//! 数据流：
//!   热键 → window::invoke(app) → awareness::collect() →
//!   SearchService.update_snapshot() → IntentRouter / SearchEngine / Plugin

use std::time::Instant;

use serde::Serialize;

use crate::app::config::ContextConfig;

/// 文本类环境项的来源标签（0.8.3 收尾 · 一等公民）。
///
/// **一等标签**意味着从数据侧带来,不再靠"哪个字段非空"反推。前端 `SuggestionOrigin`
/// 与本 enum 一对一映射（`impl From<AwarenessSource> for SuggestionOrigin`）。
///
/// 未来扩展位（0.9 Chord）：`ChordSelection` / `ChordClipboard`（主动抓取的显式版本）,
/// 与 passive 的 `Selection` / `Clipboard` 区分——按 [[configurable-by-default]] 精神,
/// 消费方出现前不加。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AwarenessSource {
    /// UIA 划词监听抓取的选区文本（[[黄金时机原则]]:焦点未失时抓）
    Selection,
    /// 系统剪贴板文本（`GetClipboardData` on invoke）
    Clipboard,
}

/// 单条文本类环境项。
#[derive(Debug, Clone)]
pub struct AwarenessText {
    /// 来源标签 —— 从采集侧带来
    pub source: AwarenessSource,
    /// 原文（未 trim,消费方通过 `find_text` 拿到的已经是 trim 后的 view）
    pub text: String,
    /// 单条独立时间戳（0.9.2.1 起被 `TextSource::SelectionThenClipboard` 消费:
    /// 选区与剪贴板同时非空时按时间戳择新,避免主窗口打开时更新剪贴板后仍显示老选区）
    pub captured_at: Instant,
}

/// 借出的文本视图 —— `find_text` 返回,把 `source` + trim 后 `&str` 一起带给调用方。
///
/// **关键契约**：调用方拿到 `AwarenessView` 就有 origin,不必再反推"这是选区还是剪贴板"。
///
/// 0.9.2.1：`captured_at` 加入,供 `TextSource::SelectionThenClipboard` 按时间戳择新
/// —— 避免"snapshot 已刷新剪贴板但 Selection 老、导致翻译 Ghost 一直是老选区"的死角。
#[derive(Debug, Clone, Copy)]
pub struct AwarenessView<'a> {
    pub source: AwarenessSource,
    /// trim 后的文本（`find_text` 已保证非空）
    pub text: &'a str,
    /// 采集时刻（`AwarenessText.captured_at` 直接透传，调试/日志用）
    #[allow(dead_code)]
    pub captured_at: Instant,
}

/// 唤起瞬间的系统环境快照（0.8.3 收尾 · 替代旧 `ContextSnapshot`）。
///
/// 快照是不可变的，仅用于单次搜索生命周期，不持久化。
#[derive(Debug, Clone)]
pub struct AwarenessSnapshot {
    /// 快照采集时间（元数据,调试用）
    #[allow(dead_code)]
    pub captured_at: Instant,
    /// 前台应用元数据（非文本,单独放）—— 供敏感应用过滤、插件 protocol 用
    pub foreground_app: Option<ForegroundAppInfo>,
    /// 文本类环境项(选区/剪贴板/未来 Chord)。每条带 source 标签,不再靠平级字段。
    ///
    /// **顺序无语义**：`find_text(source)` 按 source 查找,不假设顺序。
    pub texts: Vec<AwarenessText>,
}

impl Default for AwarenessSnapshot {
    fn default() -> Self {
        AwarenessSnapshot {
            captured_at: Instant::now(),
            foreground_app: None,
            texts: Vec::new(),
        }
    }
}

impl AwarenessSnapshot {
    /// 查找指定 source 的文本,返回 trim 后非空的 `AwarenessView`（0.8.3 收尾 · 内聚判定）。
    ///
    /// **单一入口**：原 `snapshot_has_meaningful_selection` / `TextSource::extract`
    /// 的取值顺序 / `infer_origin` 的判断分支全部通过本方法。**trim 后空视为 None**,
    /// 与旧行为一致。
    ///
    /// 同 source 多条时（未来剪贴板历史场景）取第一条 —— 简单先做,复杂选择策略等
    /// 消费方出现（YAGNI）。
    pub fn find_text(&self, source: AwarenessSource) -> Option<AwarenessView<'_>> {
        self.texts
            .iter()
            .filter(|t| t.source == source)
            .find_map(|t| {
                let trimmed = t.text.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(AwarenessView {
                        source,
                        text: trimmed,
                        captured_at: t.captured_at,
                    })
                }
            })
    }

    /// 替换或追加指定 source 的文本（0.8.0 §1.1 异步回填选区场景）。
    ///
    /// - `Some(text)`：找到同 source 项则原地替换,否则 append
    /// - `None`：删除同 source 项（清空选区语义）
    ///
    /// `SearchService::update_selected_text` 的实现基石。
    pub fn upsert_text(&mut self, source: AwarenessSource, text: Option<String>) {
        // 先删旧的同 source 项
        self.texts.retain(|t| t.source != source);
        // 再 append 新的（None → 相当于删除）
        if let Some(text) = text {
            self.texts.push(AwarenessText {
                source,
                text,
                captured_at: Instant::now(),
            });
        }
    }

    /// 带指定时间戳的 upsert（供 Selection 回填用，保留真实采集时间）。
    ///
    /// `captured_at = None` 时 fallback 到 `Instant::now()`（兼容旧路径）。
    ///
    /// **动机**：`upsert_text` 总是用 `Instant::now()` 覆盖时间戳，导致 Selection 的
    /// `captured_at` 丢失了"划词瞬间"，与 Clipboard 的 `Instant::now()` 打平后
    /// `TextSource::SelectionThenClipboard::extract` 的时间戳比较无意义。
    pub fn upsert_text_with_time(
        &mut self,
        source: AwarenessSource,
        text: Option<String>,
        captured_at: Option<Instant>,
    ) {
        self.texts.retain(|t| t.source != source);
        if let Some(text) = text {
            self.texts.push(AwarenessText {
                source,
                text,
                captured_at: captured_at.unwrap_or_else(Instant::now),
            });
        }
    }

    // ── 测试用 helper（单测构造 snapshot 更清晰）──────────────────

    /// 单测用：构造只带选区的快照。
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_selection(text: impl Into<String>) -> Self {
        let mut s = Self::default();
        s.upsert_text(AwarenessSource::Selection, Some(text.into()));
        s
    }

    /// 单测用：构造只带剪贴板的快照。
    #[cfg(test)]
    #[allow(dead_code)]
    pub fn with_clipboard(text: impl Into<String>) -> Self {
        let mut s = Self::default();
        s.upsert_text(AwarenessSource::Clipboard, Some(text.into()));
        s
    }
}

/// 过渡 alias（0.8.3 收尾）：`ContextSnapshot` 名字所有消费方保留,内部指向新结构。
///
/// **删除时机**：0.8.4 或 0.9 Chord 落地时,把消费方的 `ContextSnapshot` 全量替换成
/// `AwarenessSnapshot`,删除本 alias。0.8.3 阶段保留能让本次重构改动量降到最小。
pub type ContextSnapshot = AwarenessSnapshot;

/// 前台应用信息（结构不动 · 0.8.3 收尾沿用）。
#[derive(Debug, Clone)]
pub struct ForegroundAppInfo {
    /// 进程名（如 "code.exe"）
    pub process_name: String,
    /// 窗口标题（如 "main.rs - blink"）
    pub window_title: String,
    /// 完整 exe 路径（需要权限时可能为 None）
    #[allow(dead_code)]
    pub exe_path: Option<String>,
    /// 前台窗口句柄原始值（Windows HWND 的 isize 表示），0=无效。
    #[allow(dead_code)]
    pub hwnd: isize,
}

/// 运行中的进程（用于设置页「敏感应用」选择器）。
#[derive(Debug, Clone, Serialize)]
pub struct RunningProcess {
    pub process_name: String,
    pub window_title: String,
}

/// 采集环境快照 —— 按 ContextConfig 过滤（总开关 / 敏感应用 / 剪贴板）。
///
/// Windows 实现见 `windows.rs`。
pub fn collect(cfg: &ContextConfig) -> AwarenessSnapshot {
    // 总开关关闭 → 空快照（完全不采集）
    if !cfg.enabled {
        return AwarenessSnapshot::default();
    }
    // 先采集前台应用（剪贴板可关,但前台用于来源/意图,始终采）
    let foreground_app = collect_foreground_app();
    // 前台是敏感应用（如密码管理器）→ 整体放弃采集（隐私保护）
    if let Some(ref fg) = foreground_app {
        if cfg.is_sensitive(&fg.process_name) {
            tracing::debug!(app = %fg.process_name, "前台为敏感应用,放弃采集上下文");
            return AwarenessSnapshot::default();
        }
    }
    // 组装 texts：目前只有 Clipboard 走 collect(),Selection 由 window::invoke 后异步
    // 通过 SearchService::update_selected_text 回填（[[黄金时机原则]]）
    let mut texts = Vec::new();
    if cfg.clipboard_enabled {
        if let Some(clip) = collect_clipboard_text() {
            // 用剪贴板最后一次真实变化的时间戳，而非 Instant::now()（invoke 瞬间）。
            // 避免 Clipboard captured_at 总是最新的，导致与 Selection 的真实采集时间
            // 比较时 Clipboard 恒胜（即使 Selection 是更晚的用户行为）。
            let clip_changed_at =
                crate::infra::platform::clipboard::last_changed_at().unwrap_or_else(Instant::now);
            texts.push(AwarenessText {
                source: AwarenessSource::Clipboard,
                text: clip,
                captured_at: clip_changed_at,
            });
        }
    }
    // debug 日志：按 Unicode 字符数截断,避免切到中文中间 panic
    tracing::debug!(
        fg_process = ?foreground_app.as_ref().map(|f| &f.process_name),
        fg_title = ?foreground_app.as_ref().map(|f| &f.window_title),
        clipboard = %texts.iter().find(|t| t.source == AwarenessSource::Clipboard).map(|t| {
            if t.text.chars().count() > 80 {
                let end = t.text.char_indices().nth(80).map(|(i, _)| i).unwrap_or(80);
                format!("{}…", &t.text[..end])
            } else {
                t.text.clone()
            }
        }).unwrap_or_else(|| "(空)".into()),
        "上下文采集完成"
    );
    AwarenessSnapshot {
        captured_at: Instant::now(),
        foreground_app,
        texts,
    }
}

/// 列出当前有可见窗口的运行中进程（供设置页「敏感应用」选择器）。
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

/// 采集当前前台应用信息（对外暴露 · 0.9.2.1）。
///
/// 供 clipboard listener 的 change hook 判断「前台是否为敏感应用」用——不能重复
/// 走 `collect()`（那个包了 config 门控 + 剪贴板采集,hook 里已经拿到 text 了）。
///
/// 内部就是把 `collect_foreground_app()` 的 pub(super) 语义提到 crate 级。
pub fn foreground_app() -> Option<ForegroundAppInfo> {
    #[cfg(target_os = "windows")]
    {
        collect_foreground_app()
    }
    #[cfg(not(target_os = "windows"))]
    {
        None
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn find_text_returns_none_on_empty() {
        let s = AwarenessSnapshot::default();
        assert!(s.find_text(AwarenessSource::Selection).is_none());
        assert!(s.find_text(AwarenessSource::Clipboard).is_none());
    }

    #[test]
    fn find_text_returns_view_with_source_label() {
        let s = AwarenessSnapshot::with_selection("hello world");
        let view = s.find_text(AwarenessSource::Selection).unwrap();
        assert_eq!(view.source, AwarenessSource::Selection);
        assert_eq!(view.text, "hello world");
    }

    #[test]
    fn find_text_trims_and_returns_none_for_whitespace_only() {
        let s = AwarenessSnapshot::with_selection("   ");
        assert!(s.find_text(AwarenessSource::Selection).is_none());
    }

    #[test]
    fn find_text_trims_leading_trailing_whitespace() {
        let s = AwarenessSnapshot::with_selection("  hello  ");
        let view = s.find_text(AwarenessSource::Selection).unwrap();
        assert_eq!(view.text, "hello");
    }

    #[test]
    fn upsert_text_replaces_existing_source() {
        let mut s = AwarenessSnapshot::with_selection("old");
        s.upsert_text(AwarenessSource::Selection, Some("new".into()));
        assert_eq!(s.texts.len(), 1);
        assert_eq!(s.find_text(AwarenessSource::Selection).unwrap().text, "new");
    }

    #[test]
    fn upsert_text_none_removes_source() {
        let mut s = AwarenessSnapshot::with_selection("hello");
        s.upsert_text(AwarenessSource::Selection, None);
        assert_eq!(s.texts.len(), 0);
        assert!(s.find_text(AwarenessSource::Selection).is_none());
    }

    #[test]
    fn upsert_text_preserves_other_sources() {
        // Selection 更新不影响 Clipboard
        let mut s = AwarenessSnapshot::with_clipboard("CLIP");
        s.upsert_text(AwarenessSource::Selection, Some("SEL".into()));
        assert_eq!(s.find_text(AwarenessSource::Selection).unwrap().text, "SEL");
        assert_eq!(
            s.find_text(AwarenessSource::Clipboard).unwrap().text,
            "CLIP"
        );
    }

    #[test]
    fn find_text_only_matches_requested_source() {
        // 只有剪贴板时,查选区应返 None
        let s = AwarenessSnapshot::with_clipboard("hello");
        assert!(s.find_text(AwarenessSource::Selection).is_none());
        assert!(s.find_text(AwarenessSource::Clipboard).is_some());
    }
}
