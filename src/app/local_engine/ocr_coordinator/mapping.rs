//! ONNX executor → OcrResult 契约映射（0.22.8-D）。
//!
//! executor 从 `OnnxOcrExecutor::recognize()` 返回的 `OcrResult` 已经由
//! pipeline 层（`onnx_ocr/pipeline.rs`）完成了 `oar_ocr` → `OcrResult` 的映射。
//!
//! 本模块负责**后处理**：
//! - 校验 rect 边界（坐标非负、宽高正面积、不越出图片尺寸）
//! - 过滤零面积/空文本 word/line
//! - **0.22.8 三层契约**：当 ONNX 结果携带 `char_boxes` 时，文本已由
//!   pipeline 以 `region.text` 为真源正确构建，**不再走词级 grouping 重建**，
//!   避免逐字符框被 `join_words_intra_line_with_gaps` 误判为词级 token。
//! - 无 `char_boxes` 的旧结果仍走 `rebuild_with_line_grouping`（WinRT 兼容）。

use crate::domain::capability::builtins::ocr_engine::{
    OcrRect, OcrResult, OcrWord, rebuild_with_line_grouping_and_diag,
};
use crate::domain::ocr::error::StructuredOcrError;

/// 映射 executor 返回的 OcrResult → 最终 OcrResult（含校验 + line grouping）。
///
/// **0.22.8 三层契约**：
/// - 当 `result.char_boxes` 非空时，文本由 pipeline 以 `region.text` 为真源构建，
///   此处仅做 rect 校验和 char_boxes 偏移修正，**不走词级 grouping**。
/// - 当 `result.char_boxes` 为空时（无逐字符框），走 `rebuild_with_line_grouping`
///   做行级聚合（与 WinRT/PaddleOCR 走同一套纯函数）。
pub(super) fn map_executor_result(
    result: OcrResult,
    request_png_size: (u32, u32),
) -> Result<OcrResult, StructuredOcrError> {
    let (image_width, image_height) = request_png_size;

    // 如果 executor 没有返回任何内容，直接返回空结果
    if result.lines.is_empty() && result.words.is_empty() && result.char_boxes.is_empty() {
        return Ok(OcrResult {
            text: String::new(),
            lines: Vec::new(),
            words: Vec::new(),
            text_angle: result.text_angle,
            char_ranges: Vec::new(),
            char_boxes: Vec::new(),
        });
    }

    // ── 有 char_boxes：ONNX 三层契约路径 ──
    // pipeline 已经以 region.text 为真源构建了完整文本和行结构，
    // 此处仅校验 rect 边界，不重新拼接文本。
    if !result.char_boxes.is_empty() {
        return map_executor_result_with_char_boxes(result, image_width, image_height);
    }

    // ── 无 char_boxes：旧路径（词级 grouping 重建）──
    // 用于 WinRT 兼容和无逐字符框的场景。

    // 1. 校验并过滤 words
    let mut valid_words: Vec<OcrWord> = Vec::new();
    for (word_idx, word) in result.words.iter().enumerate() {
        if word.text.is_empty() {
            continue;
        }
        validate_rect(&word.bounding_rect, word_idx, image_width, image_height)?;
        valid_words.push(word.clone());
    }

    // 2. 校验 lines（如果有）
    for (line_idx, line) in result.lines.iter().enumerate() {
        if line.text.is_empty() {
            continue;
        }
        validate_rect(&line.bounding_rect, line_idx, image_width, image_height)?;
    }

    // 3. 走 rebuild_with_line_grouping 做 line grouping
    let (grouped_result, diag) =
        rebuild_with_line_grouping_and_diag(valid_words, result.text_angle);

    tracing::debug!(
        backend = "onnx-ocr",
        path = "word-grouping",
        image_width = image_width,
        image_height = image_height,
        source_lines = result.lines.len(),
        source_words = result.words.len(),
        grouped_lines = diag.grouped_lines,
        merged_line_count = diag.merged_line_count,
        rejected_y_center = diag.rejected_y_center,
        rejected_overlap = diag.rejected_overlap,
        rejected_height_ratio = diag.rejected_height_ratio,
        assigned_existing_line = diag.assigned_existing_line,
        created_new_line = diag.created_new_line,
        large_horizontal_gaps = diag.large_horizontal_gaps,
        inserted_extra_spaces = diag.inserted_extra_spaces,
        inserted_blank_lines = diag.inserted_blank_lines,
        output_text_chars = diag.output_text_chars,
        layout_elapsed_ms = diag.layout_elapsed_ms,
        "OCR 几何归一化完成"
    );

    Ok(grouped_result)
}

/// ONNX 三层契约路径：char_boxes 已由 pipeline 正确构建，仅做校验。
///
/// pipeline 已经：
/// 1. 以 `region.text` 为唯一真源构建 `OcrResult.text`
/// 2. 按阅读顺序排列 regions/lines
/// 3. 为每个非空白字符生成 `OcrCharBox`（含全局 char range）
/// 4. 为每个 region 生成语义级 `OcrWord`（含 char_ranges）
///
/// 此函数仅校验 rect 边界，不重新拼接文本。
fn map_executor_result_with_char_boxes(
    result: OcrResult,
    image_width: u32,
    image_height: u32,
) -> Result<OcrResult, StructuredOcrError> {
    // 校验 char_boxes rect 边界
    for (idx, cb) in result.char_boxes.iter().enumerate() {
        validate_rect(&cb.bounding_rect, idx, image_width, image_height)?;
    }

    // 校验 words rect 边界
    for (idx, word) in result.words.iter().enumerate() {
        if word.text.is_empty() {
            continue;
        }
        validate_rect(&word.bounding_rect, idx, image_width, image_height)?;
    }

    // 校验 lines rect 边界
    for (idx, line) in result.lines.iter().enumerate() {
        if line.text.is_empty() {
            continue;
        }
        validate_rect(&line.bounding_rect, idx, image_width, image_height)?;
    }

    tracing::debug!(
        backend = "onnx-ocr",
        path = "char-boxes",
        image_width = image_width,
        image_height = image_height,
        source_lines = result.lines.len(),
        source_words = result.words.len(),
        char_boxes = result.char_boxes.len(),
        output_text_chars = result.text.chars().count(),
        "OCR 三层契约映射完成（不走红级 grouping）"
    );

    Ok(result)
}

/// 校验 rect 边界——坐标非负、宽高正面积、不越出图片尺寸。
fn validate_rect(
    rect: &OcrRect,
    context_idx: usize,
    image_width: u32,
    image_height: u32,
) -> Result<(), StructuredOcrError> {
    if rect.x < 0 || rect.y < 0 {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect 坐标不能为负：x={}, y={}",
            rect.x, rect.y
        )));
    }
    if rect.w == 0 || rect.h == 0 {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect 宽高必须 > 0：w={}, h={}",
            rect.w, rect.h
        )));
    }
    let x_plus_w = rect.x.saturating_add(rect.w as i32);
    let y_plus_h = rect.y.saturating_add(rect.h as i32);
    if x_plus_w as u32 > image_width {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect x+w={x_plus_w} 超出 image_width={image_width}"
        )));
    }
    if y_plus_h as u32 > image_height {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect y+h={y_plus_h} 超出 image_height={image_height}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::capability::builtins::ocr_engine::{
        OcrCharBox, OcrLine, OcrRect, OcrResult, OcrWord,
    };

    fn make_word(text: &str, x: i32, y: i32, w: u32, h: u32, line_index: usize) -> OcrWord {
        OcrWord {
            text: text.to_string(),
            bounding_rect: OcrRect { x, y, w, h },
            line_index,
        }
    }

    fn make_rect(x: i32, y: i32, w: u32, h: u32) -> OcrRect {
        OcrRect { x, y, w, h }
    }

    #[test]
    fn map_executor_result_basic() {
        let result = OcrResult {
            text: "hello".to_string(),
            lines: vec![OcrLine {
                text: "hello".to_string(),
                bounding_rect: make_rect(0, 0, 100, 30),
                word_indices: vec![0],
            }],
            words: vec![make_word("hello", 0, 0, 100, 30, 0)],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        assert!(mapped.text.contains("hello"));
        assert!(!mapped.words.is_empty());
    }

    #[test]
    fn map_executor_result_empty() {
        let result = OcrResult {
            text: String::new(),
            lines: vec![],
            words: vec![],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        assert!(mapped.text.is_empty());
        assert!(mapped.lines.is_empty());
        assert!(mapped.words.is_empty());
    }

    #[test]
    fn map_executor_result_negative_coords_rejected() {
        let result = OcrResult {
            text: "bad".to_string(),
            lines: vec![],
            words: vec![make_word("bad", -1, 0, 100, 30, 0)],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        assert!(map_executor_result(result, (200, 100)).is_err());
    }

    #[test]
    fn map_executor_result_zero_area_rejected() {
        let result = OcrResult {
            text: "bad".to_string(),
            lines: vec![],
            words: vec![make_word("bad", 0, 0, 0, 30, 0)],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        assert!(map_executor_result(result, (200, 100)).is_err());
    }

    #[test]
    fn map_executor_result_overflow_rejected() {
        let result = OcrResult {
            text: "bad".to_string(),
            lines: vec![],
            words: vec![make_word("bad", 199, 0, 2, 30, 0)],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        assert!(map_executor_result(result, (200, 100)).is_err());
    }

    #[test]
    fn map_executor_result_empty_text_word_filtered() {
        let result = OcrResult {
            text: String::new(),
            lines: vec![],
            words: vec![
                make_word("", 0, 0, 100, 30, 0),
                make_word("ok", 0, 30, 100, 30, 0),
            ],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        assert_eq!(mapped.words.len(), 1);
        assert_eq!(mapped.words[0].text, "ok");
    }

    #[test]
    fn map_executor_result_cjk_line_grouping() {
        let result = OcrResult {
            text: "你好".to_string(),
            lines: vec![],
            words: vec![
                make_word("你", 0, 0, 25, 30, 0),
                make_word("好", 25, 0, 25, 30, 0),
            ],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        let mapped = map_executor_result(result, (100, 50)).unwrap();
        assert_eq!(mapped.text, "你好");
        assert_eq!(mapped.lines.len(), 1);
    }

    // ── 三层契约路径测试 ──────────────────────────────────────────

    #[test]
    fn map_executor_result_with_char_boxes_preserves_text() {
        // 有 char_boxes 时，文本原样保留，不走词级 grouping
        let result = OcrResult {
            text: "PP-OCRv6".to_string(),
            lines: vec![OcrLine {
                text: "PP-OCRv6".to_string(),
                bounding_rect: make_rect(0, 0, 80, 20),
                word_indices: vec![0],
            }],
            words: vec![OcrWord {
                text: "PP-OCRv6".to_string(),
                bounding_rect: make_rect(0, 0, 80, 20),
                line_index: 0,
            }],
            text_angle: None,
            char_ranges: vec![(0, 8)],
            char_boxes: vec![
                OcrCharBox {
                    text: "P".into(),
                    bounding_rect: make_rect(0, 0, 10, 20),
                    line_index: 0,
                    char_start: 0,
                    char_end: 1,
                },
                OcrCharBox {
                    text: "P".into(),
                    bounding_rect: make_rect(10, 0, 10, 20),
                    line_index: 0,
                    char_start: 1,
                    char_end: 2,
                },
                OcrCharBox {
                    text: "-".into(),
                    bounding_rect: make_rect(20, 0, 5, 20),
                    line_index: 0,
                    char_start: 2,
                    char_end: 3,
                },
                OcrCharBox {
                    text: "O".into(),
                    bounding_rect: make_rect(25, 0, 10, 20),
                    line_index: 0,
                    char_start: 3,
                    char_end: 4,
                },
            ],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        // 文本必须原样保留，不得插入空格
        assert_eq!(mapped.text, "PP-OCRv6");
        // char_boxes 必须保留
        assert_eq!(mapped.char_boxes.len(), 4);
        // char_ranges 必须保留
        assert_eq!(mapped.char_ranges.len(), 1);
    }

    #[test]
    fn map_executor_result_with_char_boxes_cjk_preserves_text() {
        let result = OcrResult {
            text: "文字识别".to_string(),
            lines: vec![OcrLine {
                text: "文字识别".to_string(),
                bounding_rect: make_rect(0, 0, 80, 20),
                word_indices: vec![0],
            }],
            words: vec![OcrWord {
                text: "文字识别".to_string(),
                bounding_rect: make_rect(0, 0, 80, 20),
                line_index: 0,
            }],
            text_angle: None,
            char_ranges: vec![(0, 4)],
            char_boxes: vec![
                OcrCharBox {
                    text: "文".into(),
                    bounding_rect: make_rect(0, 0, 20, 20),
                    line_index: 0,
                    char_start: 0,
                    char_end: 1,
                },
                OcrCharBox {
                    text: "字".into(),
                    bounding_rect: make_rect(20, 0, 20, 20),
                    line_index: 0,
                    char_start: 1,
                    char_end: 2,
                },
                OcrCharBox {
                    text: "识".into(),
                    bounding_rect: make_rect(40, 0, 20, 20),
                    line_index: 0,
                    char_start: 2,
                    char_end: 3,
                },
                OcrCharBox {
                    text: "别".into(),
                    bounding_rect: make_rect(60, 0, 20, 20),
                    line_index: 0,
                    char_start: 3,
                    char_end: 4,
                },
            ],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        // CJK 文本不得被拆开或插入空格
        assert_eq!(mapped.text, "文字识别");
        assert_eq!(mapped.char_boxes.len(), 4);
    }

    #[test]
    fn map_executor_result_with_char_boxes_rejects_bad_rect() {
        let result = OcrResult {
            text: "bad".to_string(),
            lines: vec![],
            words: vec![],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![OcrCharBox {
                text: "b".into(),
                bounding_rect: make_rect(-1, 0, 10, 20),
                line_index: 0,
                char_start: 0,
                char_end: 1,
            }],
        };
        assert!(map_executor_result(result, (200, 100)).is_err());
    }

    #[test]
    fn map_executor_result_without_char_boxes_still_groups() {
        // 无 char_boxes 时走旧路径（词级 grouping）
        let result = OcrResult {
            text: "hello world".to_string(),
            lines: vec![],
            words: vec![
                make_word("hello", 0, 0, 50, 30, 0),
                make_word("world", 60, 0, 50, 30, 0),
            ],
            text_angle: None,
            char_ranges: vec![],
            char_boxes: vec![],
        };
        let mapped = map_executor_result(result, (200, 100)).unwrap();
        // 走了 grouping → 文本由 join_words_intra_line_with_gaps 重建
        assert_eq!(mapped.text, "hello world");
        assert!(mapped.char_boxes.is_empty());
    }
}
