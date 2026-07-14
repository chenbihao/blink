//! 平台抽象层：热键、窗口、本地化、上下文采集、选区抓取、剪贴板监听、截图、密钥、音频采集、文本注入、Python 环境

pub mod audio;
pub mod clipboard;
pub mod context;
pub mod hotkey;
pub mod inject;
pub mod locale;
pub mod python;
pub mod screenshot;
pub mod secret;
pub mod selection;
pub mod window;
