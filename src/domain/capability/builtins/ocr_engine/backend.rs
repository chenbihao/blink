//! OCR 后端 trait、Windows adapter 与全局注入。
//!
//! 0.22 收尾：从 `ocr_engine.rs` 按职责拆出。
//!
//! - `OcrBackend` trait — domain 侧 OCR 抽象（返回领域类型 `OcrResult`）
//! - `WindowsOcrBackendAdapter` — 包装 infra `PlatformOcrBackend`，做 raw → domain 映射
//! - `install_backend()` / `backend()` — 全局单例注入（对齐 ScreenshotBackend 模式）

use std::sync::{Arc, OnceLock, RwLock};

use super::layout::rebuild_with_line_grouping;
use super::types::{OcrError, OcrResult};

/// OCR 后端 trait（domain 侧抽象，返回领域类型）。
#[async_trait::async_trait]
pub trait OcrBackend: Send + Sync {
    /// 识别 PNG 图片中的文字
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError>;

    /// 返回设备已安装的 OCR 语言 BCP-47 tag 列表（0.17.5 诊断用）。
    async fn available_languages(&self) -> Vec<String> {
        Vec::new()
    }

    /// 返回当前引擎使用的语言 tag（None = fallback；0.17.5 诊断用）。
    async fn engine_language(&self) -> Option<String> {
        None
    }
}

// ── 全局注入 ───────────────────────────────────────────────────────────────

static BACKEND: OnceLock<RwLock<Arc<dyn OcrBackend>>> = OnceLock::new();

/// 安装/替换 OCR backend（0.11.7-f）。测试专用——注入 `FakeOcrBackend` 用，
/// 可重复调用替换。
///
/// 生产链路不调用：`backend()` 首次调用兜底安装 `WindowsOcrBackendAdapter`。
#[cfg(test)]
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

/// 获取当前 OCR backend。
///
/// **首次调用兜底**：自动包装 infra `PlatformOcrBackend` 为 domain `OcrBackend`。
pub fn backend() -> Arc<dyn OcrBackend> {
    let lock = BACKEND.get_or_init(|| {
        let default: Arc<dyn OcrBackend> = Arc::new(WindowsOcrBackendAdapter::new());
        RwLock::new(default)
    });
    lock.read().expect("OCR backend RwLock 中毒").clone()
}

// ── WindowsOcrBackendAdapter（0.14.7 W2）──────────────────────────────────

/// 包装 infra `PlatformOcrBackend`，将原始 DTO 映射为领域类型。
///
/// 智能拼接（`join_words_smart`）和 rect union 在此完成，不下沉到 infra。
pub struct WindowsOcrBackendAdapter {
    inner: Box<dyn crate::infra::platform::ocr::PlatformOcrBackend>,
}

impl WindowsOcrBackendAdapter {
    pub fn new() -> Self {
        Self {
            inner: crate::infra::platform::ocr::default_backend(),
        }
    }
}

impl Default for WindowsOcrBackendAdapter {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait::async_trait]
impl OcrBackend for WindowsOcrBackendAdapter {
    async fn recognize(&self, png_data: &[u8]) -> Result<OcrResult, OcrError> {
        use crate::infra::platform::ocr::PlatformOcrError;

        let raw = self
            .inner
            .recognize_raw(png_data)
            .await
            .map_err(|e| match e {
                PlatformOcrError::Engine(msg) => OcrError::Engine(msg),
                PlatformOcrError::Decode(msg) => OcrError::Decode(msg),
                PlatformOcrError::Unsupported => OcrError::Unsupported,
            })?;

        Ok(map_raw_to_domain(raw))
    }

    async fn available_languages(&self) -> Vec<String> {
        self.inner.available_languages().await
    }

    async fn engine_language(&self) -> Option<String> {
        self.inner.engine_language().await
    }
}

/// 将 infra 原始 DTO 映射为领域类型。
///
/// 负责：
/// - 浮点 rect → 整数 rect（四舍五入）
/// - word flat 数组构建（丢弃 SDK 的 line 分组，统一走 `rebuild_with_line_grouping` 重新分组）
/// - 同行聚合 + 阅读顺序恢复（`rebuild_with_line_grouping`）
/// - `join_words_smart` 智能拼接全文
///
/// WinRT SDK 虽然已按视觉行分组，但走同一套同行聚合纯函数可保证
/// WinRT 与 PaddleOCR 语义一致，且能修正 SDK 偶尔的行拆分问题。
pub(crate) fn map_raw_to_domain(raw: crate::infra::platform::ocr::RawOcrResult) -> OcrResult {
    use super::types::{OcrRect, OcrWord};

    let mut words: Vec<OcrWord> = Vec::new();

    for raw_line in &raw.lines {
        for raw_word in &raw_line.words {
            let rect = OcrRect {
                x: raw_word.rect.x.round() as i32,
                y: raw_word.rect.y.round() as i32,
                w: raw_word.rect.width.round().max(0.0) as u32,
                h: raw_word.rect.height.round().max(0.0) as u32,
            };
            words.push(OcrWord {
                text: raw_word.text.clone(),
                bounding_rect: rect,
                line_index: 0, // 会被 rebuild_with_line_grouping 覆盖
            });
        }
    }

    rebuild_with_line_grouping(words, raw.text_angle)
}
