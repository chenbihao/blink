//! OCR Backend 抽象（0.11.7-c 引入，0.11.7-f 能力化）。
//!
//! **命名注**：文件仍叫 `ocr_engine.rs`（避免大重命名），但**语义已改**——`OcrEngine`
//! trait → `OcrBackend` trait（对齐 `ScreenshotBackend` 命名）。旧名 `OcrEngine`
//! 作为类型别名保留避免破坏面。
//!
//! **架构**：
//! - `OcrBackend` trait — 可 mock 的 OCR 平台抽象
//! - `WindowsOcrBackend` — 生产实现（`Windows.Media.Ocr` WinRT API）
//! - `FakeOcrBackend` — 测试实现（返回预定义文本）
//! - `install_backend()` / `backend()` — 全局单例注入（对齐 ScreenshotBackend 模式）
//!
//! **Windows.Media.Ocr 要求**：Windows 10 1809+，中文语言包已安装时自动识别中文。
//! 无中文语言包时仍可识别英文。

use std::sync::{Arc, OnceLock, RwLock};

use serde::Serialize;

/// OCR 识别结果
#[derive(Debug, Clone, Serialize)]
pub struct OcrResult {
    pub text: String,
    pub lines: Vec<OcrLine>,
}

/// OCR 单行结果
#[derive(Debug, Clone, Serialize)]
pub struct OcrLine {
    pub text: String,
    #[serde(rename = "rect")]
    pub bounding_rect: OcrRect,
}

/// 矩形坐标（物理像素）
#[derive(Debug, Clone, Copy, Serialize)]
pub struct OcrRect {
    pub x: i32,
    pub y: i32,
    pub w: u32,
    pub h: u32,
}

/// OCR 引擎错误
#[derive(Debug)]
#[allow(dead_code)]
pub enum OcrError {
    Engine(String),
    Decode(String),
    Unsupported,
}

impl std::fmt::Display for OcrError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OcrError::Engine(msg) => write!(f, "OCR 引擎错误: {msg}"),
            OcrError::Decode(msg) => write!(f, "图片解码错误: {msg}"),
            OcrError::Unsupported => write!(f, "当前平台不支持 OCR"),
        }
    }
}

/// OCR 后端 trait（0.11.7-f 重命名自 `OcrEngine`）。
#[async_trait::async_trait]
pub trait OcrBackend: Send + Sync {
    /// 识别 PNG 图片中的文字
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError>;
}

/// 旧类型别名（供 0.11.7-c 遗留代码使用；新代码用 `OcrBackend`）。
#[allow(dead_code)]
pub type OcrEngine = dyn OcrBackend;

// ── 全局注入 ───────────────────────────────────────────────────────────────

static BACKEND: OnceLock<RwLock<Arc<dyn OcrBackend>>> = OnceLock::new();

/// 安装/替换 OCR backend（0.11.7-f）。
///
/// **调用时机**：`main.rs::setup` 里最早期。可重复调用替换 backend（测试用）。
#[allow(dead_code)] // 测试通过 install_backend 注入 Fake
pub fn install_backend(backend: Arc<dyn OcrBackend>) {
    match BACKEND.get() {
        Some(lock) => {
            if let Ok(mut w) = lock.write() {
                *w = backend;
            }
        }
        None => {
            let _ = BACKEND.set(RwLock::new(backend));
        }
    }
}

/// 获取当前 OCR backend（0.11.7-f）。
///
/// **首次调用兜底**：Windows 平台自动装 `WindowsOcrBackend`。
pub fn backend() -> Arc<dyn OcrBackend> {
    let lock = BACKEND.get_or_init(|| {
        #[cfg(target_os = "windows")]
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackend);
        #[cfg(not(target_os = "windows"))]
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackend); // fallback 也是 stub
        RwLock::new(default)
    });
    lock.read().expect("OCR backend RwLock 中毒").clone()
}

/// **兼容层**：旧调用者拿全局单例（0.11.7-c）。
///
/// 新代码走 `backend()` 拿 `Arc<dyn OcrBackend>`。
#[allow(dead_code)]
pub fn get_ocr_engine() -> Arc<dyn OcrBackend> {
    backend()
}

// ── WindowsOcrBackend 实现 ──────────────────────────────────

/// Windows.Media.Ocr 实现的 OCR 后端。
///
/// 内部使用 WinRT API，通过 `windows-rs` 绑定调用。
#[cfg(target_os = "windows")]
pub struct WindowsOcrBackend;

#[cfg(target_os = "windows")]
#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackend {
    #[allow(unused_qualifications)]
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError> {
        use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;
        use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

        // 1. 创建 InMemoryRandomAccessStream 并写入 PNG 字节
        let stream = InMemoryRandomAccessStream::new()
            .map_err(|e| OcrError::Engine(format!("创建流失败: {e}")))?;

        let writer = DataWriter::CreateDataWriter(&stream)
            .map_err(|e| OcrError::Engine(format!("创建 DataWriter 失败: {e}")))?;

        writer
            .WriteBytes(png_data)
            .map_err(|e| OcrError::Engine(format!("写入流失败: {e}")))?;

        let _store_result = writer
            .StoreAsync()
            .map_err(|e| OcrError::Engine(format!("StoreAsync 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("StoreAsync await 失败: {e}")))?;

        stream
            .Seek(0)
            .map_err(|e| OcrError::Engine(format!("Seek 失败: {e}")))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| OcrError::Engine(format!("创建 BitmapDecoder 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("BitmapDecoder await 失败: {e}")))?;

        let software_bitmap = decoder
            .GetSoftwareBitmapAsync()
            .map_err(|e| OcrError::Engine(format!("GetSoftwareBitmap 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("GetSoftwareBitmap await 失败: {e}")))?;

        let bgra_bitmap = SoftwareBitmap::Convert(&software_bitmap, BitmapPixelFormat::Bgra8)
            .map_err(|e| OcrError::Engine(format!("转换 BGRA8 失败: {e}")))?;

        let ocr_engine = WinRtOcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| OcrError::Engine(format!("创建 OcrEngine 失败: {e}")))?;

        let ocr_result = ocr_engine
            .RecognizeAsync(&bgra_bitmap)
            .map_err(|e| OcrError::Engine(format!("RecognizeAsync 失败: {e}")))?
            .await
            .map_err(|e| OcrError::Engine(format!("等待识别完成失败: {e}")))?;

        let text = ocr_result
            .Text()
            .map_err(|e| OcrError::Engine(format!("提取文本失败: {e}")))?
            .to_string();

        let mut lines = Vec::new();
        if let Ok(lines_raw) = ocr_result.Lines() {
            let line_count = lines_raw.Size().unwrap_or(0);
            for i in 0..line_count {
                if let Ok(line) = lines_raw.GetAt(i) {
                    let line_text = line.Text().unwrap_or_default().to_string();
                    if !line_text.is_empty() {
                        lines.push(OcrLine {
                            text: line_text,
                            bounding_rect: OcrRect { x: 0, y: 0, w: 0, h: 0 },
                        });
                    }
                }
            }
        }

        Ok(OcrResult { text, lines })
    }
}

/// 非 Windows 平台回退
#[cfg(not(target_os = "windows"))]
pub struct WindowsOcrBackend;

#[cfg(not(target_os = "windows"))]
#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        Err(OcrError::Unsupported)
    }
}

// ── FakeOcrBackend（测试用） ───────────────────────────────────────────────

/// 测试用假 OCR 后端。构造时配置固定返回值。
#[derive(Debug, Clone)]
#[allow(dead_code)]
pub struct FakeOcrBackend {
    text: String,
    lines: Vec<OcrLine>,
    err: Option<String>,
}

#[allow(dead_code)]
impl FakeOcrBackend {
    pub fn returning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            lines: Vec::new(),
            err: None,
        }
    }

    pub fn with_lines(mut self, lines: Vec<OcrLine>) -> Self {
        self.lines = lines;
        self
    }

    pub fn failing(msg: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            err: Some(msg.into()),
        }
    }
}

#[async_trait::async_trait]
impl OcrBackend for FakeOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        if let Some(msg) = &self.err {
            return Err(OcrError::Engine(msg.clone()));
        }
        Ok(OcrResult {
            text: self.text.clone(),
            lines: self.lines.clone(),
        })
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_backend_returns_configured_text() {
        let backend = FakeOcrBackend::returning("Hello 世界");
        let result = backend.recognize(&[]).await.unwrap();
        assert_eq!(result.text, "Hello 世界");
        assert!(result.lines.is_empty());
    }

    #[tokio::test]
    async fn fake_backend_returns_configured_error() {
        let backend = FakeOcrBackend::failing("模拟错误");
        let err = backend.recognize(&[]).await.unwrap_err();
        assert!(matches!(err, OcrError::Engine(msg) if msg == "模拟错误"));
    }

    #[tokio::test]
    async fn install_backend_replaces_global() {
        install_backend(Arc::new(FakeOcrBackend::returning("test-injection")));
        let b = backend();
        let result = b.recognize(&[]).await.unwrap();
        assert_eq!(result.text, "test-injection");
    }
}
