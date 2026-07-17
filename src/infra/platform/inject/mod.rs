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
//! `inject_text()` 始终走 SendInput Unicode（不碰剪贴板），失败时自动降级到
//! Clipboard+Ctrl+V。无需用户配置——自动降级覆盖了所有兼容性场景。
//!
//! > **0.10.5 TSF 方案已废弃**：曾引入 imekit 做 TSF Composition 注入，实测发现
//! > `ITfThreadMgr::GetFocus()` 是进程本地的——Blink 拿不到前台应用的编辑上下文，跨进程时 TSF 路径静默失败退化成 SendInput，无额外价值。imekit 依赖与 TSF 实现已移除。
//! >
//! > **inject_method 配置项已移除**：SendInput + 自动降级已覆盖所有场景，
//! > 用户无需手动选择注入方式。旧配置中的 `inject_method` 字段会被 serde 忽略。

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
/// 策略：SendInput Unicode（不碰剪贴板）→ 失败时自动降级 Clipboard+Ctrl+V。
#[cfg(target_os = "windows")]
pub fn inject_text(text: &str) -> Result<(), InjectError> {
    if text.is_empty() {
        return Ok(());
    }

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

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows_impl;
