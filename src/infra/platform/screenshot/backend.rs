//! ScreenshotBackend trait（0.11.7-f）：可 mock 的截屏平台抽象。
//!
//! **动机**：`screenshot/mod.rs` 原直接嵌入 `windows::capture_virtual_screen()`，测试
//! 必须跑真实 Win32。抽出 trait 后：
//! - 生产走 `WindowsScreenshotBackend`（BitBlt + EnumDisplayMonitors）
//! - 测试走 `FakeScreenshotBackend`（内存 BGRA + 预设 DisplayGeometry）
//! - AI 通过统一 `Screenshot` Capability 拿到显示器列表 + 截屏能力
//!
//! **依赖注入**：backend 挂到 Tauri managed state（对齐 `AIProviderRegistry` 模式），
//! Capability 从 `InvokeContext.app_handle.state::<Arc<dyn ScreenshotBackend>>()` 拿。

use serde::Serialize;

use super::ScreenCaptureMeta;

/// 单个显示器的几何信息（0.11.7-f）。
///
/// 坐标系：虚拟屏幕 = 所有显示器拼接后的联合矩形，主屏原点 (0, 0)，副屏坐标可能为负。
/// 单位：物理像素（未做 DPI 缩放）。
#[derive(Debug, Clone, Serialize)]
#[allow(dead_code)] // 字段供 AI Capability 序列化用，Step 2 消费
pub struct DisplayGeometry {
    /// 稳定 id（EnumDisplayMonitors 顺序，同一次进程内固定；跨启动可能变）。
    pub id: u32,
    /// 显示器名（EDID 或 fallback `Display #{id}`）。
    pub name: String,
    /// 虚拟屏幕坐标系下的左上角 X。
    pub x: i32,
    /// 虚拟屏幕坐标系下的左上角 Y。
    pub y: i32,
    /// 物理像素宽度。
    pub w: u32,
    /// 物理像素高度。
    pub h: u32,
    /// DPI（96 = 100% 缩放）。
    pub dpi: u32,
    /// 是否主屏。
    pub primary: bool,
}

/// 截屏平台后端 trait。
///
/// **同步接口**：BitBlt 本身是同步的，Rust async 无法帮它更快；调用方需要在
/// `spawn_blocking` 中执行以避免阻塞 tokio worker。
///
/// **像素格式约定**：BGRA、top-down、每行 `width * 4` 字节。这是 BitBlt 原生输出
/// 且剪贴板 CF_DIB / SoftwareBitmap.Bgra8 都直接接受，避免多余的 R↔B swap。
pub trait ScreenshotBackend: Send + Sync {
    /// 枚举所有显示器。
    ///
    /// 返回顺序：主屏永远在第一个（`primary=true`），其余按 EnumDisplayMonitors 顺序。
    /// 空 vec 表示无显示器（headless 或异常）。
    fn list_displays(&self) -> Vec<DisplayGeometry>;

    /// 截取整个虚拟屏幕（拼接所有显示器）。
    ///
    /// 返回 `(BGRA 像素, meta)`；meta 描述虚拟屏幕的几何。
    fn capture_virtual_screen(&self) -> Result<(Vec<u8>, ScreenCaptureMeta), String>;

    /// 截取指定显示器。
    ///
    /// `display_id` 必须是 `list_displays()` 返回过的 id；否则返回 Err。
    /// 返回 `(BGRA 像素, geometry)`。
    fn capture_display(&self, display_id: u32) -> Result<(Vec<u8>, DisplayGeometry), String>;
}
