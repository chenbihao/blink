//! OCR 平台抽象——WinRT 调用与原始 DTO 提取（0.14.7 W2）。
//!
//! **职责**：PNG 解码、WinRT `Windows.Media.Ocr` 调用、SDK 数据提取为平台无关的原始 DTO。
//! **不引用** `crate::domain::*`——CJK/Latin 智能拼接和领域映射由 domain 侧完成。
//!
//! **架构**：
//! - `PlatformOcrBackend` trait — 平台无关的 OCR 后端抽象
//! - `WindowsOcrBackend` — Windows 生产实现（`backend_windows.rs`）
//! - 原始 DTO（`RawOcrResult` / `RawOcrLine` / `RawOcrWord` / `RawOcrRect`）

use async_trait::async_trait;

#[cfg(target_os = "windows")]
mod backend_windows;

/// 平台原始 OCR 矩形（SDK 返回的浮点坐标，domain 侧四舍五入为整数）。
#[derive(Debug, Clone, Copy)]
pub struct RawOcrRect {
    pub x: f64,
    pub y: f64,
    pub width: f64,
    pub height: f64,
}

/// 平台原始 OCR 单词。
#[derive(Debug, Clone)]
pub struct RawOcrWord {
    pub text: String,
    pub rect: RawOcrRect,
}

/// 平台原始 OCR 行（含该行的 word 列表）。
#[derive(Debug, Clone)]
pub struct RawOcrLine {
    pub text: String,
    pub words: Vec<RawOcrWord>,
}

/// 平台原始 OCR 结果。
#[derive(Debug, Clone)]
pub struct RawOcrResult {
    pub lines: Vec<RawOcrLine>,
    /// SDK 检测到的文本旋转角度（度）；`None` 表示未给或非旋转文本。
    pub text_angle: Option<f64>,
}

/// 平台 OCR 错误。
#[derive(Debug)]
#[allow(dead_code)] // Decode/Unsupported 仅在特定平台或未来后端构造
pub enum PlatformOcrError {
    Engine(String),
    Decode(String),
    Unsupported,
}

impl std::fmt::Display for PlatformOcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PlatformOcrError::Engine(msg) => write!(f, "OCR 引擎错误: {msg}"),
            PlatformOcrError::Decode(msg) => write!(f, "图片解码错误: {msg}"),
            PlatformOcrError::Unsupported => write!(f, "当前平台不支持 OCR"),
        }
    }
}

/// OCR 平台后端 trait——只负责平台调用和原始数据提取。
#[async_trait]
pub trait PlatformOcrBackend: Send + Sync {
    /// 识别 PNG 图片中的文字，返回原始 DTO。
    async fn recognize_raw(&self, png_data: &[u8]) -> Result<RawOcrResult, PlatformOcrError>;
}

/// 获取默认平台 OCR 后端。
pub fn default_backend() -> Box<dyn PlatformOcrBackend> {
    #[cfg(target_os = "windows")]
    {
        Box::new(backend_windows::WindowsOcrBackend)
    }
    #[cfg(not(target_os = "windows"))]
    {
        Box::new(UnsupportedOcrBackend)
    }
}

/// 非 Windows 平台回退。
#[cfg(not(target_os = "windows"))]
struct UnsupportedOcrBackend;

#[cfg(not(target_os = "windows"))]
#[async_trait]
impl PlatformOcrBackend for UnsupportedOcrBackend {
    async fn recognize_raw(&self, _png_data: &[u8]) -> Result<RawOcrResult, PlatformOcrError> {
        Err(PlatformOcrError::Unsupported)
    }
}
