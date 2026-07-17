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
//! `inject_text()` 根据 `SttConfig.inject_method` 选择注入策略：
//! - `SendInput`（默认）：逐字符 `KEYEVENTF_UNICODE`，不碰剪贴板
//! - `Clipboard`：剪贴板 + Ctrl+V（兼容性兜底）
//!
//! SendInput Unicode 失败时自动回退到 Clipboard+Ctrl+V（兼容性兜底）。
//!
//! > **0.10.5 TSF 方案已废弃**：曾引入 imekit 做 TSF Composition 注入，实测发现
//! > `ITfThreadMgr::GetFocus()` 是进程本地的——Blink 在自己进程创建的 TSF 管理器
//! > 拿不到前台应用的编辑上下文，跨进程时 TSF 路径静默失败，退化成 SendInput，
//! > 无额外价值。imekit 依赖与 TSF 实现已移除。

use std::fmt;

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
/// 根据 `SttConfig.inject_method` 选择注入方式：
/// - `SendInput`（默认）：SendInput Unicode 逐字符，不碰剪贴板
/// - `Clipboard`：剪贴板 + Ctrl+V
///
/// SendInput 失败时自动回退到 Clipboard+Ctrl+V。
#[cfg(target_os = "windows")]
pub fn inject_text(text: &str) -> Result<(), InjectError> {
    let config = crate::app::stt_config::get_stt_config();
    inject_text_with_method(text, config.inject_method)
}

/// 按指定方式注入文本。
#[cfg(target_os = "windows")]
pub fn inject_text_with_method(
    text: &str,
    method: crate::app::stt_config::InjectMethod,
) -> Result<(), InjectError> {
    use crate::app::stt_config::InjectMethod;

    if text.is_empty() {
        return Ok(());
    }

    let chars = text.chars().count();

    match method {
        InjectMethod::SendInput => {
            tracing::info!(chars, "G2 注入: SendInput Unicode");
            match windows_impl::inject_text_unicode(text) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!(%e, chars, "SendInput Unicode 失败, 降级 Clipboard+Ctrl+V");
                    windows_impl::inject_text_clipboard(text)
                }
            }
        }
        InjectMethod::Clipboard => {
            tracing::info!(chars, "G2 注入: Clipboard+Ctrl+V");
            windows_impl::inject_text_clipboard(text)
        }
    }
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows_impl;
