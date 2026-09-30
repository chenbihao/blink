//! **兼容薄壳**（0.11.7-f 迁移过渡）——原 GDI 截屏实现已移到 `backend_windows.rs`。
//!
//! 此模块保留 `capture_virtual_screen()` 函数导出，供仍未迁移的代码调用。
//! Step 4 收敛完成后此文件可删除。

use super::ScreenCaptureMeta;

/// **已迁移**：转发到 `backend_windows::WindowsScreenshotBackend`。
///
/// 保留供未迁移代码短期兼容；新代码应通过 `super::backend()` 获取当前 backend。
#[allow(dead_code)]
pub fn capture_virtual_screen() -> Result<(Vec<u8>, ScreenCaptureMeta), String> {
    super::backend_windows::capture_virtual_screen()
}
