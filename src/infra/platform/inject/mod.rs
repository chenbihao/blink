//! 文本注入平台抽象层。
//!
//! 0.10 G2(语音输入法上屏):STT 转出的文字需要注入到前台应用的光标处。
//!
//! ## 技术路径(详见 0.10 文档 §五)
//!
//! | 方案 | 流式展示 | 最终注入 | 剪贴板 |
//! |---|---|---|---|
//! | **Clipboard+Ctrl+V** (0.10.1~0.10.2) | mini overlay 显示 partial | clipboard + SendInput(Ctrl+V) | ❌ 污染 |
//! | **SendInput Unicode** (0.10.3 默认) | mini overlay 显示 partial | KEYEVENTF_UNICODE 逐字符 | ✅ 不碰 |
//!
//! ## 当前实现
//!
//! `inject_text()` 按长度分流（0.23.20）：
//!
//! - **短文本**（UTF-16 码元 ≤ [`CLIPBOARD_INJECT_THRESHOLD`]）走 SendInput Unicode
//!   （不碰剪贴板、无选区替换），失败时自动降级 Clipboard+Ctrl+V。
//! - **长文本**（超过阈值）直接走 Clipboard+Ctrl+V 一次上屏：逐字符 SendInput 的
//!   发送端不慢，瓶颈在接收端——目标应用要逐个处理上千条 WM_CHAR（各自走输入
//!   管线/IME），UI 线程被淹没，且与真实硬件输入共享系统输入队列产生背压。
//!
//! 无需用户配置——自动分流覆盖了所有场景。已预写剪贴板的调用方（`paste_to_input`
//! 的复制兜底）可用 [`paste_clipboard_text()`] 免去备份→写→恢复三连。
//!
//! > **0.10.5 TSF 方案已废弃**：曾引入 imekit 做 TSF Composition 注入，实测发现
//! > `ITfThreadMgr::GetFocus()` 是进程本地的——Blink 拿不到前台应用的编辑上下文，跨进程时 TSF 路径静默失败退化成 SendInput，无额外价值。imekit 依赖与 TSF 实现已移除。
//! >
//! > **inject_method 配置项已移除**：SendInput + 自动降级已覆盖所有场景，
//! > 用户无需手动选择注入方式。旧配置中的 `inject_method` 字段会被 serde 忽略。

use std::fmt;

/// 长文本阈值（UTF-16 码元数，0.23.20）：超过即改走剪贴板+Ctrl+V 一次上屏。
///
/// 取 200：对应 400 个 INPUT，仍在 `SENDINPUT_BATCH_LIMIT`(500) 单批内，短路径
/// 行为与历史版本完全一致；中文一句常见 20~50 字，百字级长段（复制的历史文本、
/// 语音大段终态）才切换粘贴路径。
pub const CLIPBOARD_INJECT_THRESHOLD: usize = 200;

/// 注入策略（0.23.20）：按文本规模选择上屏方式。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InjectStrategy {
    /// SendInput Unicode 逐字符（不碰剪贴板、纯插入不替换选区）。
    Unicode,
    /// 剪贴板 + Ctrl+V 一次上屏（目标应用单次粘贴，UI 线程零压力）。
    ClipboardPaste,
}

/// 按文本规模决定注入策略（纯函数，单测锁定边界）。
pub fn choose_inject_strategy(utf16_units: usize) -> InjectStrategy {
    if utf16_units > CLIPBOARD_INJECT_THRESHOLD {
        InjectStrategy::ClipboardPaste
    } else {
        InjectStrategy::Unicode
    }
}

impl InjectStrategy {
    /// 结构化日志用的策略标签（`method` 字段值）。
    pub fn as_label(self) -> &'static str {
        match self {
            Self::Unicode => "sendinput_unicode",
            Self::ClipboardPaste => "clipboard_ctrl_v",
        }
    }
}

/// 文本注入错误。
#[derive(Debug)]
#[allow(dead_code)]
pub enum InjectError {
    /// 剪贴板操作失败
    Clipboard(String),
    /// SendInput 失败
    SendInput(String),
    /// 其他错误
    Other(String),
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InjectError::Clipboard(msg) => write!(f, "clipboard error: {msg}"),
            InjectError::SendInput(msg) => write!(f, "SendInput error: {msg}"),
            InjectError::Other(msg) => write!(f, "inject error: {msg}"),
        }
    }
}

impl std::error::Error for InjectError {}

/// 注入文本到前台应用光标处。
///
/// 策略（0.23.20 长度分流）：
/// - 短文本（≤ [`CLIPBOARD_INJECT_THRESHOLD`] 码元）：SendInput Unicode →
///   失败时自动降级 Clipboard+Ctrl+V；
/// - 长文本：直接 Clipboard+Ctrl+V 一次上屏（备份→写→恢复，见
///   [`windows_impl::inject_text_clipboard`]）。
#[cfg(target_os = "windows")]
pub fn inject_text(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

    let units = text.encode_utf16().count();
    match choose_inject_strategy(units) {
        InjectStrategy::ClipboardPaste => {
            tracing::info!(units, threshold = CLIPBOARD_INJECT_THRESHOLD, "长文本注入: Clipboard+Ctrl+V");
            windows_impl::inject_text_clipboard(text)
        }
        InjectStrategy::Unicode => {
            let chars = text.chars().count();
            tracing::info!(chars, "文本注入: SendInput Unicode");
            match windows_impl::inject_text_unicode(text) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!(%e, chars, "SendInput Unicode 失败, 降级 Clipboard+Ctrl+V");
                    windows_impl::inject_text_clipboard(text)
                }
            }
        }
    }
}

/// 对系统剪贴板中的**现有内容**直接发送 Ctrl+V（0.23.20）。
///
/// 供已把目标文本写入系统剪贴板的调用方（`paste_to_input` 的复制兜底）：剪贴板
/// 内容恰好就是要上屏的文本，免去 `inject_text_clipboard` 的备份→写→恢复三连，
/// 也天然规避「目标应用延迟粘贴导致恢复抢先覆盖」的竞态——粘贴后剪贴板保持
/// 目标文本正是该调用方的预期（复制语义）。调用方自行保证剪贴板内容已就位。
#[cfg(target_os = "windows")]
pub fn paste_clipboard_text() -> Result<(), InjectError> {
    windows_impl::paste_clipboard_text()
}

/// 仅 SendInput Unicode 注入，失败不降级剪贴板（0.23.13 G2 渐进上屏）。
///
/// 录音进行中的渐进冲刷必须走此路径：剪贴板降级会注入真实 Ctrl+V 按键，
/// 其 keydown 会触发输入状态机 armed→aborted（该分支不区分 injected 键），
/// 破坏 hold-to-talk 会话。失败由调用方挂起重试，终态再走完整路径。
#[cfg(target_os = "windows")]
pub fn inject_text_unicode_strict(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }
    windows_impl::inject_text_unicode(text)
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows_impl;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strategy_switches_at_threshold() {
        assert_eq!(choose_inject_strategy(0), InjectStrategy::Unicode);
        assert_eq!(
            choose_inject_strategy(CLIPBOARD_INJECT_THRESHOLD),
            InjectStrategy::Unicode
        );
        assert_eq!(
            choose_inject_strategy(CLIPBOARD_INJECT_THRESHOLD + 1),
            InjectStrategy::ClipboardPaste
        );
        assert_eq!(
            choose_inject_strategy(CLIPBOARD_INJECT_THRESHOLD * 100),
            InjectStrategy::ClipboardPaste
        );
    }
}
