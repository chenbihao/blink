//! 内置 Capability 集合（0.9.7 Step 2）。
//!
//! 每个能力一个文件 + 一个 `inventory::submit!`，自动进 `CapabilityRegistry`。
//! 新增能力 = 新建文件 + 写一行 submit，注册表零改动。
//!
//! **内置能力**（文档 §4.1）：
//! - [`screenshot`] — 0.11.7-f 统一入口，`op=list_displays|capture|crop`
//! - [`capture_screen`] — alias to `screenshot { op: capture }`（保留 3 个月）
//! - [`crop_image`] — alias to `screenshot { op: crop }`（保留 3 个月）
//! - [`ocr_image`] — OCR 文字识别（0.11.7-c）
//! - [`read_clipboard`] — 读剪贴板 → `Text`/`Blob`
//! - [`write_clipboard`] — 写剪贴板 → `Done`（图/文双模式）
//! - [`search_files`] — 搜文件 → `Items`（包装 FileEngine）
//! - [`search_apps`] — 搜应用 → `Items`（0.11.2 改进 5，共享 StartMenuEngine）
//! - [`search_clipboard_history`] — 搜剪贴板历史 → `Items`（0.11.5 改进 6，sensitive=true）
//! - [`open_url`] — 打开 URL → `Done`（0.14.2 从 Action 提升为 Capability）
//! - [`open_path`] — 打开文件/目录 → `Done`（0.14.2 从 Action 提升为 Capability）
//! - [`reveal_in_explorer`] — 资源管理器定位 → `Done`（0.14.2 从 Action 提升为 Capability）

pub mod capture_screen;
pub mod crop_image;
pub mod ocr_engine;
pub mod ocr_image;
pub mod open_path;
pub mod open_url;
pub mod read_clipboard;
pub mod reveal_in_explorer;
pub mod screenshot;
pub mod search_apps;
pub mod search_clipboard_history;
pub mod search_files;
pub mod write_clipboard;
