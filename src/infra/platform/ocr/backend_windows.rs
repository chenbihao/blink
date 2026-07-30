//! Windows.Media.Ocr 实现的 OCR 平台后端（0.14.7 W2）。
//!
//! 只负责 PNG 解码、WinRT 调用和 SDK 数据提取为 `RawOcrResult`。
//! 不引用 `crate::domain::*`——领域映射和智能拼接由 domain 侧完成。

use super::{PlatformOcrBackend, PlatformOcrError, RawOcrLine, RawOcrRect, RawOcrResult, RawOcrWord};
use async_trait::async_trait;

/// Windows.Media.Ocr 实现的 OCR 后端。
pub struct WindowsOcrBackend;

#[async_trait]
impl PlatformOcrBackend for WindowsOcrBackend {
    async fn recognize_raw(&self, png_data: &[u8]) -> Result<RawOcrResult, PlatformOcrError> {
        use windows::Graphics::Imaging::{BitmapDecoder, BitmapPixelFormat, SoftwareBitmap};
        use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;
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

        let ocr_engine = WinRtOcrEngine::TryCreateFromUserProfileLanguages()
            .map_err(|e| PlatformOcrError::Engine(format!("创建 OcrEngine 失败: {e}")))?;

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
}
