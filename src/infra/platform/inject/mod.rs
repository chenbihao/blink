//! 文本注入平台抽象层。
//!
//! 0.10 G2(语音输入法上屏):STT 转出的文字需要注入到前台应用的光标处。
//!
//! ## 技术路径(详见 0.10 文档 §五)
//!
//! | 阶段 | 方案 | 流式展示 | 最终注入 |
//! |---|---|---|---|
//! | **0.10.1(当前)** | Clipboard + Ctrl+V | mini overlay 显示 partial | clipboard + SendInput(Ctrl+V) |
//! | **0.10.2+(未来)** | TSF Composition | composition range 实时更新 | commit composition |
//!
//! ## 当前实现:Clipboard + Ctrl+V
//!
//! 1. 备份当前剪贴板内容(文本)
//! 2. 设置剪贴板为 STT 文本
//! 3. SendInput(Ctrl down → V down → V up → Ctrl up)
//! 4. 等待 50ms 让应用处理粘贴
//! 5. 恢复原剪贴板内容

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
/// 当前实现:Clipboard + Ctrl+V。
/// 0.10.2+ 可加 TSF composition 路径。
#[cfg(target_os = "windows")]
pub fn inject_text(text: &str) -> Result<(), InjectError> {
    windows_impl::inject_text_clipboard(text)
}

// 平台特定实现
#[cfg(target_os = "windows")]
mod windows_impl;
