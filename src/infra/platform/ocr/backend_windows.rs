//! Windows.Media.Ocr 实现的 OCR 平台后端（0.14.7 W2）。
//!
//! 只负责 PNG 解码、WinRT 调用和 SDK 数据提取为 `RawOcrResult`。
//! 不引用 `crate::domain::*`——领域映射和智能拼接由 domain 侧完成。

use std::sync::OnceLock;

use super::{
    PlatformOcrBackend, PlatformOcrError, RawOcrLine, RawOcrRect, RawOcrResult, RawOcrWord,
};
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
        if let Ok(lang) = available.GetAt(i)
            && let Ok(tag) = lang.LanguageTag()
        {
            tags.push(tag.to_string());
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
fn select_chinese_preferred_engine()
-> Result<(windows::Media::Ocr::OcrEngine, Option<String>), PlatformOcrError> {
    use windows::Media::Ocr::OcrEngine as WinRtOcrEngine;

    let tags = get_available_language_tags();
    let matched_tag = match_chinese_language(&tags);

    if let Some(ref tag) = matched_tag {
        let hstring = windows::core::HSTRING::from(tag.as_str());
        if let Ok(lang) = windows::Globalization::Language::CreateLanguage(&hstring)
            && let Ok(engine) = WinRtOcrEngine::TryCreateFromLanguage(&lang)
        {
            tracing::info!(language = %tag, "OCR 引擎已选中文语言");
            return Ok((engine, Some(tag.clone())));
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
        let text_angle: Option<f64> = ocr_result.TextAngle().ok().and_then(|opt| opt.Value().ok());

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
        assert_eq!(
            match_chinese_language(&tags),
            Some("zh-Hans-CN".to_string())
        );
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hans() {
        let tags = vec!["en-US".to_string(), "zh-Hans".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hans".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hant_tw() {
        let tags = vec!["en-US".to_string(), "zh-Hant-TW".to_string()];
        assert_eq!(
            match_chinese_language(&tags),
            Some("zh-Hant-TW".to_string())
        );
    }

    #[test]
    fn match_chinese_falls_back_to_zh_hant() {
        let tags = vec!["en-US".to_string(), "zh-Hant".to_string()];
        assert_eq!(match_chinese_language(&tags), Some("zh-Hant".to_string()));
    }

    #[test]
    fn match_chinese_falls_back_to_any_zh_prefix() {
        let tags = vec!["en-US".to_string(), "zh-Hans-SG".to_string()];
        assert_eq!(
            match_chinese_language(&tags),
            Some("zh-Hans-SG".to_string())
        );
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

    // ── WinRT OCR Baseline（0.22 spike）──────────────────────────────────────
    //
    // 对 golden corpus 跑 WinRT OCR，计算 CER 和 word rect 有效率。
    // 输出 JSON 到 xtask/spikes/ppocrv6/results/winrt_baseline.json。
    // 标记为 ignore（需要 testdata，不在常规 CI 中运行）。
    //
    // 运行: cargo test --bin blink winrt_baseline -- --ignored --nocapture

    /// 字符级编辑距离（CER = edit_distance / ref_len）。
    fn char_edit_distance(hyp: &str, ref_text: &str) -> usize {
        let h: Vec<char> = hyp.chars().collect();
        let r: Vec<char> = ref_text.chars().collect();
        let m = h.len();
        let n = r.len();
        if n == 0 {
            return m;
        }
        if m == 0 {
            return n;
        }
        let mut prev: Vec<usize> = (0..=n).collect();
        let mut curr = vec![0usize; n + 1];
        for i in 1..=m {
            curr[0] = i;
            for j in 1..=n {
                let cost = if h[i - 1] == r[j - 1] { 0 } else { 1 };
                curr[j] = (prev[j] + 1).min(curr[j - 1] + 1).min(prev[j - 1] + cost);
            }
            std::mem::swap(&mut prev, &mut curr);
        }
        prev[n]
    }

    /// Golden corpus manifest 条目。
    #[derive(serde::Deserialize)]
    struct CorpusItem {
        image: String,
        expected_text: String,
        subset: String,
    }

    #[derive(serde::Deserialize)]
    struct CorpusManifest {
        items: Vec<CorpusItem>,
    }

    #[derive(serde::Serialize)]
    struct SubsetResult {
        cer_mean: f64,
        count: usize,
        total_words: usize,
        valid_rects: usize,
        empty_rects: usize,
        out_of_bounds: usize,
        rect_valid_ratio: f64,
    }

    #[derive(serde::Serialize)]
    struct BaselineOutput {
        engine: String,
        language: Option<String>,
        weighted_cer: f64,
        total_items: usize,
        total_words: usize,
        valid_words: usize,
        word_rect_valid_ratio: f64,
        subsets: std::collections::BTreeMap<String, SubsetResult>,
    }

    /// 运行 WinRT OCR baseline。
    ///
    /// 用法: cargo test --bin blink winrt_baseline -- --ignored --nocapture
    #[tokio::test]
    #[ignore = "需要 testdata/ocr/ppocrv6 golden corpus；spike 阶段手动运行"]
    async fn winrt_baseline() {
        use std::path::Path;

        // 1. 定位 corpus
        let manifest_path =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata/ocr/ppocrv6/manifest.json");
        assert!(
            manifest_path.exists(),
            "manifest.json not found at {}",
            manifest_path.display()
        );

        let manifest_json = std::fs::read_to_string(&manifest_path).unwrap();
        let manifest: CorpusManifest =
            serde_json::from_str(&manifest_json).expect("解析 manifest.json 失败");

        let corpus_dir = manifest_path.parent().unwrap();

        // 2. 创建 OCR 后端
        let backend = WindowsOcrBackend::default();
        let langs = backend.available_languages().await;
        println!("可用 OCR 语言: {}", langs.join(", "));

        // 3. 逐图跑 OCR
        let mut all_cers: Vec<f64> = Vec::new();
        let mut total_words = 0usize;
        let mut valid_words = 0usize;

        // subset -> (cers, total_words, valid_rects, empty_rects, out_of_bounds)
        let mut subset_data: std::collections::BTreeMap<
            String,
            (Vec<f64>, usize, usize, usize, usize),
        > = std::collections::BTreeMap::new();

        for (idx, item) in manifest.items.iter().enumerate() {
            let img_path = corpus_dir.join(&item.image);
            assert!(img_path.exists(), "图片不存在: {}", img_path.display());

            let png_data = std::fs::read(&img_path).unwrap();
            let result = backend.recognize_raw(&png_data).await.expect("OCR 失败");

            // 提取 OCR 文本（lines 拼接）
            let ocr_text: String = result
                .lines
                .iter()
                .map(|l| l.text.as_str())
                .collect::<Vec<_>>()
                .join("\n");

            // 统计 word rect
            let mut word_count = 0usize;
            let mut valid_rects = 0usize;
            let mut empty_rects = 0usize;
            let mut out_of_bounds = 0usize;

            // 需要图片尺寸来判断 out_of_bounds——从 PNG header 解析
            let (img_w, img_h) = png_dimensions(&png_data).unwrap_or((0, 0));

            for line in &result.lines {
                for word in &line.words {
                    word_count += 1;
                    let r = &word.rect;
                    if r.width == 0.0 && r.height == 0.0 {
                        empty_rects += 1;
                    } else if (r.x + r.width) > (img_w as f64 + 5.0)
                        || (r.y + r.height) > (img_h as f64 + 5.0)
                    {
                        out_of_bounds += 1;
                    } else {
                        valid_rects += 1;
                    }
                }
            }

            total_words += word_count;
            valid_words += valid_rects;

            // CER
            let dist = char_edit_distance(ocr_text.trim(), item.expected_text.trim());
            let ref_len = item.expected_text.trim().chars().count();
            let cer = if ref_len == 0 {
                if ocr_text.trim().is_empty() { 0.0 } else { 1.0 }
            } else {
                dist as f64 / ref_len as f64
            };
            all_cers.push(cer);

            let entry = subset_data
                .entry(item.subset.clone())
                .or_insert((Vec::new(), 0, 0, 0, 0));
            entry.0.push(cer);
            entry.1 += word_count;
            entry.2 += valid_rects;
            entry.3 += empty_rects;
            entry.4 += out_of_bounds;

            println!(
                "  [{}/{}] [{}] {}: CER={:.3} words={} valid_rects={}",
                idx + 1,
                manifest.items.len(),
                item.subset,
                img_path.file_name().unwrap().to_string_lossy(),
                cer,
                word_count,
                valid_rects
            );
        }

        // 4. 聚合结果
        let mut subsets = std::collections::BTreeMap::new();
        for (subset, (cers, tw, vr, er, oob)) in &subset_data {
            let avg = cers.iter().sum::<f64>() / cers.len() as f64;
            let rvr = if *tw > 0 {
                *vr as f64 / *tw as f64
            } else {
                0.0
            };
            subsets.insert(
                subset.clone(),
                SubsetResult {
                    cer_mean: (avg * 10000.0).round() / 10000.0,
                    count: cers.len(),
                    total_words: *tw,
                    valid_rects: *vr,
                    empty_rects: *er,
                    out_of_bounds: *oob,
                    rect_valid_ratio: (rvr * 10000.0).round() / 10000.0,
                },
            );
        }

        let weighted_cer = if all_cers.is_empty() {
            1.0
        } else {
            let avg = all_cers.iter().sum::<f64>() / all_cers.len() as f64;
            (avg * 10000.0).round() / 10000.0
        };
        let word_rect_valid_ratio = if total_words > 0 {
            let r = valid_words as f64 / total_words as f64;
            (r * 10000.0).round() / 10000.0
        } else {
            0.0
        };

        let output = BaselineOutput {
            engine: "winrt-ocr".to_string(),
            language: backend.engine_language().await,
            weighted_cer,
            total_items: all_cers.len(),
            total_words,
            valid_words,
            word_rect_valid_ratio,
            subsets,
        };

        // 5. 输出 JSON
        let results_dir =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("xtask/spikes/ppocrv6/results");
        std::fs::create_dir_all(&results_dir).unwrap();
        let output_path = results_dir.join("winrt_baseline.json");
        let json = serde_json::to_string_pretty(&output).unwrap();
        std::fs::write(&output_path, json).unwrap();

        println!();
        println!("=== WinRT Baseline Results ===");
        println!("Engine: {}", output.engine);
        println!("Language: {:?}", output.language);
        println!("Weighted CER: {}", output.weighted_cer);
        println!("Word rect valid ratio: {}", output.word_rect_valid_ratio);
        println!("Total items: {}", output.total_items);
        println!("Total words: {}", output.total_words);
        println!("Result file: {}", output_path.display());

        // 6. 基本断言（不是 pass/fail 门，只是确保 OCR 没完全失效）
        assert!(output.total_items == 22, "应该处理 22 个样本");
        assert!(output.weighted_cer < 1.0, "CER 不应该为 1.0（完全失败）");
    }

    /// 从 PNG 文件头解析图片尺寸（不依赖外部库）。
    fn png_dimensions(data: &[u8]) -> Option<(u32, u32)> {
        // PNG 签名: 8 字节，然后 IHDR chunk: 4 字节长度 + 4 字节 "IHDR" + 4 字节宽 + 4 字节高
        if data.len() < 24 {
            return None;
        }
        // 检查 PNG 签名
        if &data[0..8] != b"\x89PNG\r\n\x1a\n" {
            return None;
        }
        // IHDR chunk 的 width 和 height 都是大端 u32
        let width = u32::from_be_bytes([data[16], data[17], data[18], data[19]]);
        let height = u32::from_be_bytes([data[20], data[21], data[22], data[23]]);
        Some((width, height))
    }
}
