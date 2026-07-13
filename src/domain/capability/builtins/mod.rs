//! 内置 Capability 集合（0.9.7 Step 2）。
//!
//! 每个能力一个文件 + 一个 `inventory::submit!`，自动进 `CapabilityRegistry`。
//! 新增能力 = 新建文件 + 写一行 submit，注册表零改动。
//!
//! **五个样板能力**（文档 §4.1）：
//! - [`capture_screen`] — 截屏 → `Blob{png}`（SESSION cache 模式）
//! - [`crop_image`] — 裁剪 → `Blob{png}`（BGRA→PNG）
//! - [`read_clipboard`] — 读剪贴板 → `Text`/`Blob`
//! - [`write_clipboard`] — 写剪贴板 → `Done`（图/文双模式）
//! - [`search_files`] — 搜文件 → `Items`（包装 FileEngine）

pub mod capture_screen;
pub mod crop_image;
pub mod read_clipboard;
pub mod search_files;
pub mod write_clipboard;
