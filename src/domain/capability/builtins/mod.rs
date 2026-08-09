//! 内置 Capability 集合（0.9.7 Step 2）。
//!
//! 每个能力一个文件 + 一个 `inventory::submit!`，自动进 `CapabilityRegistry`。
//! 新增能力 = 新建文件 + 写一行 submit，注册表零改动。
//!
//! **内置能力**（文档 §4.1）：
//! - [`screenshot`] — 0.11.7-f 统一入口，`op=list_displays|capture|crop|window`
//! - [`ocr_image`] — OCR 文字识别（0.11.7-c）
//! - [`read_clipboard`] — 读剪贴板 → `Text`/`Blob{png}`（0.19.1 图片分支）
//! - [`read_clipboard_history_image`] — 按 id 读剪贴板历史图片 → `Blob{png}`（0.19.1）
//! - [`list_clipboard_images`] — 列出剪贴板图片历史 → `Items`（0.19.1，sensitive=true）
//! - [`list_windows`] — 列出桌面可见窗口 → `Items`（0.19.2，sensitive=true）
//! - [`write_clipboard`] — 写剪贴板 → `Done`（图/文双模式）
//! - [`search_files`] — 搜文件 → `Items`（包装 FileEngine）
//! - [`read_text_file`] — 受控分页读取 UTF-8 文本文件（0.19.5，sensitive=true）
//! - [`search_apps`] — 搜应用 → `Items`（0.11.2 改进 5，共享 StartMenuEngine）
//! - [`search_clipboard_history`] — 搜剪贴板历史 → `Items`（0.11.5 改进 6，sensitive=true）
//! - [`open_url`] — 打开 URL → `Done`（0.14.2 从 Action 提升为 Capability）
//! - [`open_path`] — 打开文件/目录 → `Done`（0.14.2 从 Action 提升为 Capability）
//! - [`reveal_in_explorer`] — 资源管理器定位 → `Done`（0.14.2 从 Action 提升为 Capability）
//! - [`create_sticky`] — 创建便签并显示窗口 → `Done`（0.19.3，sensitive=true）
//! - [`set_sticky_geometry`] — 更新便签位置/尺寸 → `Done`（0.19.3）
//! - [`list_sticky`] — 列出活跃便签 → `Items`（0.19.3，sensitive=true）
//! - [`read_sticky`] / [`update_sticky`] / [`trash_sticky`] — 便签生命周期闭环（0.19.5）
//! - [`pin_image`] — 将 PNG 图片钉到桌面 → `Done`（0.19.3）

pub mod create_sticky;
pub mod get_settings;
pub mod image_input;
pub mod list_clipboard_images;
pub mod list_sticky;
pub mod list_windows;
pub mod ocr_engine;
pub mod ocr_image;
pub mod open_path;
pub mod open_url;
pub mod pin_image;
pub mod read_clipboard;
pub mod read_clipboard_history_image;
pub mod read_sticky;
pub mod read_text_file;
pub mod reveal_in_explorer;
pub mod screenshot;
pub mod search_apps;
pub mod search_clipboard_history;
pub mod search_files;
pub mod set_sticky_geometry;
pub mod sticky_common;
pub mod trash_sticky;
pub mod update_setting;
pub mod update_sticky;
pub mod write_clipboard;

#[cfg(test)]
mod tests {
    #[test]
    fn phase_0_19_5_capabilities_are_in_inventory() {
        let registry = crate::domain::capability::CapabilityRegistry::new();
        for id in [
            "read_sticky",
            "update_sticky",
            "trash_sticky",
            "read_text_file",
        ] {
            assert!(registry.get(id).is_some(), "{id} 应通过 inventory 注册");
        }
    }

    #[test]
    fn phase_0_19_8_capabilities_are_in_inventory() {
        let registry = crate::domain::capability::CapabilityRegistry::new();
        for id in ["get_settings", "update_setting"] {
            assert!(registry.get(id).is_some(), "{id} 应通过 inventory 注册");
        }
    }
}
