//! 文本注入平台抽象层。
//!
//! 0.10 G2(语音输入法上屏):STT 转出的文字需要注入到前台应用的光标处。
//!
//! ## 技术路径(详见 0.10 文档 §三)
//!
//! | 方案 | 流式展示 | 最终注入 | 剪贴板 |
//! |---|---|---|---|
//! | **Clipboard+Ctrl+V** (0.10.1~0.10.2) | mini overlay 显示 partial | clipboard + SendInput(Ctrl+V) | ❌ 污染 |
//! | **SendInput Unicode** (0.10.3 默认) | mini overlay 显示 partial | KEYEVENTF_UNICODE 逐字符 | ✅ 不碰 |
//! | **TSF Composition** (0.10.5) | composition range 实时更新 | commit composition | ✅ 不碰 |
//!
//! ## 当前实现
//!
//! `inject_text()` 根据 `SttConfig.inject_method` 选择注入策略：
//! - `SendInput`（默认）：逐字符 `KEYEVENTF_UNICODE`，不碰剪贴板
//! - `Clipboard`：剪贴板 + Ctrl+V（兼容性兜底）
//! - `Tsf`：TSF Composition via imekit（0.10.5，详见 phases/0.10.5-tsf-composition.md）
//!
//! SendInput Unicode 失败时自动回退到 Clipboard+Ctrl+V（兼容性兜底）。

use std::fmt;

/// 文本注入错误。
#[derive(Debug)]
#[allow(dead_code)]
pub enum InjectError {
    /// 剪贴板操作失败
    Clipboard(String),
    /// SendInput 失败
    SendInput(String),
    /// TSF Composition 注入失败（imekit）
    Tsf(String),
    /// 其他错误
    Other(String),
}

impl fmt::Display for InjectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            InjectError::Clipboard(msg) => write!(f, "clipboard error: {msg}"),
            InjectError::SendInput(msg) => write!(f, "SendInput error: {msg}"),
            InjectError::Tsf(msg) => write!(f, "TSF inject error: {msg}"),
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
/// - `Tsf`：TSF Composition via imekit（0.10.5，详见 phases/0.10.5-tsf-composition.md）
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

    match method {
        InjectMethod::SendInput => {
            // 首选：SendInput Unicode，失败回退 Clipboard
            match windows_impl::inject_text_unicode(text) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!(%e, "SendInput Unicode 注入失败，回退 Clipboard+Ctrl+V");
                    windows_impl::inject_text_clipboard(text)
                }
            }
        }
        InjectMethod::Clipboard => windows_impl::inject_text_clipboard(text),
        InjectMethod::Tsf => {
            // 首选：imekit TSF Composition（内部 TSF → IMM32 → SendInput 三级回退）
            // 失败时回退 Clipboard+Ctrl+V（imekit 内部已有 SendInput，无需再走一次）
            match imekit_impl::ImekitInjector::commit_string(text) {
                Ok(()) => Ok(()),
                Err(e) => {
                    tracing::warn!(%e, "imekit TSF 注入失败，回退 Clipboard+Ctrl+V");
                    windows_impl::inject_text_clipboard(text)
                }
            }
        }
    }
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows_impl;

// imekit TSF Composition 实现（0.10.5）
#[cfg(target_os = "windows")]
mod imekit_impl;

/// 关闭 TSF 注入器 STA 线程（Blink 退出时调用）。
#[cfg(target_os = "windows")]
pub fn shutdown_tsf_injector() {
    imekit_impl::ImekitInjector::shutdown();
}
