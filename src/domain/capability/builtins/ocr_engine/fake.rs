//! 测试用假 OCR 后端。
//!
//! 0.22 收尾：从 `ocr_engine.rs` 按职责拆出。

use super::types::{OcrError, OcrLine, OcrResult, OcrWord};

/// 测试用假 OCR 后端。构造时配置固定返回值。
///
/// 0.11.9-b：支持配置 `words` 让 Capability 层测试 word 级链路。
/// 0.17.5：支持配置 `available_langs` / `engine_lang` 测试诊断面板。
#[derive(Debug, Clone)]
#[allow(dead_code)] // builder 方法仅在测试中消费
pub struct FakeOcrBackend {
    text: String,
    lines: Vec<OcrLine>,
    words: Vec<OcrWord>,
    err: Option<String>,
    available_langs: Vec<String>,
    engine_lang: Option<String>,
}

#[allow(dead_code)] // builder 方法仅在测试中消费
impl FakeOcrBackend {
    pub fn returning(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            lines: Vec::new(),
            words: Vec::new(),
            err: None,
            available_langs: Vec::new(),
            engine_lang: None,
        }
    }

    pub fn with_lines(mut self, lines: Vec<OcrLine>) -> Self {
        self.lines = lines;
        self
    }

    pub fn with_words(mut self, words: Vec<OcrWord>) -> Self {
        self.words = words;
        self
    }

    pub fn failing(msg: impl Into<String>) -> Self {
        Self {
            text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
            err: Some(msg.into()),
            available_langs: Vec::new(),
            engine_lang: None,
        }
    }

    /// 配置诊断返回值（0.17.5）。
    pub fn with_available_langs(mut self, langs: Vec<String>) -> Self {
        self.available_langs = langs;
        self
    }

    pub fn with_engine_lang(mut self, lang: Option<String>) -> Self {
        self.engine_lang = lang;
        self
    }
}

#[async_trait::async_trait]
impl super::backend::OcrBackend for FakeOcrBackend {
    async fn recognize(&self, _png_data: &[u8]) -> Result<OcrResult, OcrError> {
        if let Some(msg) = &self.err {
            return Err(OcrError::Engine(msg.clone()));
        }
        Ok(OcrResult {
            backend_used: None,
            backend_fallback_reason: None,
            backend_degrade_hint: None,
            text: self.text.clone(),
            lines: self.lines.clone(),
            words: self.words.clone(),
            text_angle: None,
            char_ranges: Vec::new(),
            char_boxes: Vec::new(),
        })
    }

    async fn available_languages(&self) -> Vec<String> {
        self.available_langs.clone()
    }

    async fn engine_language(&self) -> Option<String> {
        self.engine_lang.clone()
    }
}
