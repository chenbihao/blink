//! Windows.Media.Ocr 实现的 OCR 平台后端（0.14.7 W2）。
//!
//! 只负责 PNG 解码、WinRT 调用和 SDK 数据提取为 `RawOcrResult`。
//! 不引用 `crate::domain::*`——领域映射和智能拼接由 domain 侧完成。

use std::sync::OnceLock;

use super::{PlatformOcrBackend, PlatformOcrError, RawOcrLine, RawOcrRect, RawOcrResult, RawOcrWord};
use async_trait::async_trait;

/// Windows.Media.Ocr 实现的 OCR 后端。
pub struct WindowsOcrBackend {
    /// 缓存引擎使用的语言 tag（0.17.5 诊断用，避免每次诊断都重建引擎探测）。
    engine_language: OnceLock<Option<String>>,
}

impl WindowsOcrBackend {
    fn new() -> Self {
        Self {
            engine_language: OnceLock::new(),
        }
    }
}

impl Default for WindowsOcrBackend {
    fn default() -> Self {
        Self::new()
    }
}

// ── 语言选择（0.17.5） ──────────────────────────────────────────────────────

/// 中文语言优先级匹配（0.17.5）。
///
/// 纯函数，可单测。按优先级匹配 `zh-Hans-CN` > `zh-Hans` > `zh-Hant-TW` > `zh-Hant` > 任意 `zh-*`。
/// 返回命中的完整 tag，未命中返回 `None`。
fn match_chinese_language(tags: &[String]) -> Option<String> {
    let priorities = ["zh-Hans-CN", "zh-Hans", "zh-Hant-TW", "zh-Hant"];
    for priority in &priorities {
        if let Some(found) = tags.iter().find(|tag| tag == priority) {
            return Some(found.clone());
        }
    }
    tags.iter()
        .find(|tag| tag.starts_with("zh-") || tag.starts_with("zh_"))
        .cloned()
}

/// 获取设备已安装的 OCR 语言 tag 列表。
fn get_available_language_tags() -> Vec<String> {
    use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;

    let available = match WinRtOcrEngine::AvailableRecognizerLanguages() {
        Ok(list) => list,
        Err(e) => {
            tracing::warn!(error = %e, "AvailableRecognizerLanguages 查询失败");
            return Vec::new();
        }
    };

    let count = available.Size().unwrap_or(0);
    let mut tags = Vec::new();
    for i in 0..count {
        if let Ok(lang) = available.GetAt(i) {
            if let Ok(tag) = lang.LanguageTag() {
                tags.push(tag.to_string());
            }
        }
    }
    tags
}

/// 选择中文优先的 OCR 引擎（0.17.5）。
///
/// 优先从 `AvailableRecognizerLanguages()` 匹配中文语言包：
/// `zh-Hans-CN` > `zh-Hans` > `zh-Hant-TW` > `zh-Hant` > 任意 `zh-*`。
/// 命中则 `TryCreateFromLanguage`，未命中 fallback 到 `TryCreateFromUserProfileLanguages` + `warn!`。
///
/// 返回 `(OcrEngine, Option<String>)`——引擎和命中的语言 tag（None = fallback）。
fn select_chinese_preferred_engine() -> Result<
    (windows::Media::Ocr::OcrEngine, Option<String>),
    PlatformOcrError,
> {
    use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;

    let tags = get_available_language_tags();
    let matched_tag = match_chinese_language(&tags);

    if let Some(ref tag) = matched_tag {
        let hstring = windows::core::HSTRING::from(tag.as_str());
        if let Ok(lang) = windows::Globalization::Language::CreateLanguage(&hstring) {
            if let Ok(engine) = WinRtOcrEngine::TryCreateFromLanguage(&lang) {
                tracing::info!(language = %tag, "OCR 引擎已选中文语言");
                return Ok((engine, Some(tag.clone())));
            }
        }
    }

    tracing::warn!("无中文 OCR 语言包，回退到用户配置文件语言");
    let engine = WinRtOcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|e| PlatformOcrError::Engine(format!("创建 OcrEngine 失败: {e}")))?;
    Ok((engine, None))
}

// ── trait 实现 ──────────────────────────────────────────────────────────────

#[async_trait]
impl PlatformOcrBackend for WindowsOcrBackend {
    async fn recognize_raw(&self, png_data: &[u8]) -> Result<RawOcrResult, PlatformOcrError> {
        use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
        use windows::Storage::Streams::{DataWriter, InMemoryRandomAccessStream};

        // 1. 创建 InMemoryRandomAccessStream 并写入 PNG 字节
        let stream = InMemoryRandomAccessStream::new()
            .map_err(|e| PlatformOcrError::Engine(format!("创建流失败: {e}")))?;

        let writer = DataWriter::CreateDataWriter(&stream)
            .map_err(|e| PlatformOcrError::Engine(format!("创建 DataWriter 失败: {e}")))?;

        writer
            .WriteBytes(png_data)
            .map_err(|e| PlatformOcrError::Engine(format!("写入流失败: {e}")))?;

        let _store_result = writer
            .StoreAsync()
            .map_err(|e| PlatformOcrError::Engine(format!("StoreAsync 失败: {e}")))?
            .await
            .map_err(|e| PlatformOcrError::Engine(format!("StoreAsync await 失败: {e}")))?;

        stream
            .Seek(0)
            .map_err(|e| PlatformOcrError::Engine(format!("Seek 失败: {e}")))?;

        let decoder = BitmapDecoder::CreateAsync(&stream)
            .map_err(|e| PlatformOcrError::Engine(format!("创建 BitmapDecoder 失败: {e}")))?
            .await
            .map_err(|e| PlatformOcrError::Engine(format!("BitmapDecoder await 失败: {e}")))?;

        let software_bitmap = decoder
            .GetSoftwareBitmapAsync()
            .map_err(|e| PlatformOcrError::Engine(format!("GetSoftwareBitmap 失败: {e}")))?
            .await
            .map_err(|e| PlatformOcrError::Engine(format!("GetSoftwareBitmap await 失败: {e}")))?;

        let bgra_bitmap = SoftwareBitmap::Convert(&software_bitmap, BitmapPixelFormat::Bgra8)
            .map_err(|e| PlatformOcrError::Engine(format!("转换 BGRA8 失败: {e}")))?;

        // 0.17.5：中文优先引擎选择（替代 TryCreateFromUserProfileLanguages）
        let (ocr_engine, lang_tag) = select_chinese_preferred_engine()?;
        // 缓存引擎语言供诊断查询
        let _ = self.engine_language.set(lang_tag);

        let ocr_result = ocr_engine
            .RecognizeAsync(&bgra_bitmap)
            .map_err(|e| PlatformOcrError::Engine(format!("RecognizeAsync 失败: {e}")))?
            .await
            .map_err(|e| PlatformOcrError::Engine(format!("等待识别完成失败: {e}")))?;

        // 2. 提取原始数据为 RawOcrResult（不做智能拼接，留给 domain）
        let text_angle: Option<f64> = ocr_result
            .TextAngle()
            .ok()
            .and_then(|opt| opt.Value().ok())
            .map(|d| d as f64);

        let mut lines: Vec<RawOcrLine> = Vec::new();

        if let Ok(lines_raw) = ocr_result.Lines() {
            let line_count = lines_raw.Size().unwrap_or(0);
            for i in 0..line_count {
                let Ok(line) = lines_raw.GetAt(i) else {
                    continue;
                };
                let line_text = line.Text().unwrap_or_default().to_string();
                let mut words: Vec<RawOcrWord> = Vec::new();

                if let Ok(words_raw) = line.Words() {
                    let word_count = words_raw.Size().unwrap_or(0);
                    for j in 0..word_count {
                        let Ok(w) = words_raw.GetAt(j) else {
                            continue;
                        };
                        let text = w.Text().unwrap_or_default().to_string();
                        if text.is_empty() {
                            continue;
                        }
                        let rect = w
                            .BoundingRect()
                            .map(|r| RawOcrRect {
                                x: r.X as f64,
                                y: r.Y as f64,
                                width: r.Width as f64,
                                height: r.Height as f64,
                            })
                            .unwrap_or(RawOcrRect {
                                x: 0.0,
                                y: 0.0,
                                width: 0.0,
                                height: 0.0,
                            });
                        words.push(RawOcrWord { text, rect });
                    }
                }

                // 跳过空行（SDK 偶尔给空 Line + 空 Words）
                if !line_text.is_empty() || !words.is_empty() {
                    lines.push(RawOcrLine {
                        text: line_text,
                        words,
                    });
                }
            }
        }

        Ok(RawOcrResult { lines, text_angle })
    }

    async fn available_languages(&self) -> Vec<String> {
        get_available_language_tags()
    }

    async fn engine_language(&self) -> Option<String> {
        self.engine_language
            .get_or_init(|| {
                let tags = get_available_language_tags();
                match_chinese_language(&tags)
            })
            .clone()
    }
}

// ── 测试 ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn match_chinese_prefers_zh_hans_cn() {
        let tags = vec![
            "en-US".to_string(),
            "zh-Hans-CN".to_string(),
            "zh-Hant-TW".to_string(),
        ];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hans-CN".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hans() {
        let tags = vec!["en-US".to_string(), "zh-Hans".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hans".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hant_tw() {
        let tags = vec!["en-US".to_string(), "zh-Hant-TW".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hant-TW".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hant() {
        let tags = vec!["en-US".to_string(), "zh-Hant".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hant".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_any_zh_prefix() {
        let tags = vec!["en-US".to_string(), "zh-Hans-SG".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hans-SG".to_string()));
    }

    #[test]
    fn match_chinese_returns_none_when_no_chinese() {
        let tags = vec!["en-US".to_string(), "ja-JP".to_string()];
        assert_eq!(match_chinese_language(&tags), None);
    }

    #[test]
    fn match_chinese_returns_none_for_empty_list() {
        let tags: Vec<String> = vec![];
        assert_eq!(match_chinese_language(&tags), None);
    }

    #[test]
    fn match_chinese_priority_order_zh_hans_before_zh_hant() {
        // 同时存在简体和繁体时应优先简体
        let tags = vec!["zh-Hant-TW".to_string(), "zh-Hans".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hans".to_string()));
    }
}
