//! PaddleOCR 响应 → OcrResult/line/word 契约映射（纯函数，无副作用）。
//! 严格校验 request_id / engine / model 契约、尺寸一致性与 rect 边界。

use crate::domain::capability::builtins::ocr_engine::{OcrLine, OcrRect, OcrResult, OcrWord};
use crate::domain::ocr::error::StructuredOcrError;

// ── 响应映射 ────────────────────────────────────────────────────────────────

pub(super) fn map_paddleocr_response(
    resp: &serde_json::Value,
    expected_request_id: &str,
    expected_model_id: &str,
    expected_model_revision: &str,
    request_png_size: (u32, u32),
) -> Result<OcrResult, StructuredOcrError> {
    // ── 1. request_id 必须存在且与当前请求完全一致 ──
    let resp_rid = resp
        .get("request_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 request_id 字段或类型错误"))?;
    if resp_rid != expected_request_id {
        return Err(StructuredOcrError::protocol_error(format!(
            "响应 request_id 不匹配：expected={expected_request_id}, got={resp_rid}"
        )));
    }

    // ── 2. engine 必须存在且为 "paddleocr" ──
    let engine = resp
        .get("engine")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 engine 字段或类型错误"))?;
    if engine != "paddleocr" {
        return Err(StructuredOcrError::protocol_error(format!(
            "响应 engine 字段非预期值：expected=paddleocr, got={engine}"
        )));
    }

    // ── 3. model_id 必须存在且与当前实例契约一致 ──
    let model_id = resp
        .get("model_id")
        .and_then(|v| v.as_str())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 model_id 字段或类型错误"))?;
    if model_id != expected_model_id {
        return Err(StructuredOcrError::protocol_error(format!(
            "响应 model_id 不匹配：expected={expected_model_id}, got={model_id}"
        )));
    }

    // ── 4. model_revision 必须存在且与当前实例契约一致 ──
    let model_revision = resp
        .get("model_revision")
        .and_then(|v| v.as_str())
        .ok_or_else(|| {
            StructuredOcrError::protocol_error("响应缺少 model_revision 字段或类型错误")
        })?;
    if model_revision != expected_model_revision {
        return Err(StructuredOcrError::protocol_error(format!(
            "响应 model_revision 不匹配：expected={expected_model_revision}, got={model_revision}"
        )));
    }

    // ── 5. lines 必须存在且为数组 ──
    let lines_arr = resp
        .get("lines")
        .and_then(|v| v.as_array())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 lines 字段或非数组"))?;

    // ── 6. words 必须存在且为数组（可以为空但不能缺失）──
    let words_arr = resp
        .get("words")
        .and_then(|v| v.as_array())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 words 字段或非数组"))?;
    let words_count = words_arr.len();

    // ── 7. 获取响应中的 PNG width/height，用于 rect 边界校验 ──
    // 缺失或类型错误时必须报错，不能用 MAX 兜底，否则跳过边界校验
    // Task 7: 使用 checked conversion（u32::try_from），拒绝 0、负数、非整数、超 u32::MAX
    let image_width = resp
        .get("image_width")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 image_width 字段或类型错误"))
        .and_then(|w| {
            u32::try_from(w).map_err(|_| {
                StructuredOcrError::protocol_error(format!("image_width 超过 u32::MAX: {w}"))
            })
        })?;
    let image_height = resp
        .get("image_height")
        .and_then(|v| v.as_u64())
        .ok_or_else(|| StructuredOcrError::protocol_error("响应缺少 image_height 字段或类型错误"))
        .and_then(|h| {
            u32::try_from(h).map_err(|_| {
                StructuredOcrError::protocol_error(format!("image_height 超过 u32::MAX: {h}"))
            })
        })?;
    // 非零检查：image_width/image_height 必须大于 0
    if image_width == 0 {
        return Err(StructuredOcrError::protocol_error(
            "响应 image_width 为 0，不允许零尺寸",
        ));
    }
    if image_height == 0 {
        return Err(StructuredOcrError::protocol_error(
            "响应 image_height 为 0，不允许零尺寸",
        ));
    }
    // 与请求 PNG 尺寸一致性比对（Task 7: 生产路径必须校验，不允许 None 绕过）
    let (req_w, req_h) = request_png_size;
    if image_width != req_w || image_height != req_h {
        return Err(StructuredOcrError::protocol_error(format!(
            "响应尺寸 ({image_width}x{image_height}) 与请求 PNG 尺寸 ({req_w}x{req_h}) 不一致"
        )));
    }

    let mut lines: Vec<OcrLine> = Vec::new();
    let mut words: Vec<OcrWord> = Vec::new();

    // ── 8. 解析 lines ──
    for (line_idx, line_val) in lines_arr.iter().enumerate() {
        let text = line_val
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StructuredOcrError::protocol_error(format!(
                    "line[{line_idx}] 缺少 text 字段或类型错误"
                ))
            })?
            .to_string();
        if text.is_empty() {
            return Err(StructuredOcrError::protocol_error(format!(
                "line[{line_idx}] text 为空字符串"
            )));
        }

        let rect = parse_rect_strict(line_val, line_idx, image_width, image_height)?;

        let word_indices: Vec<usize> = line_val
            .get("word_indices")
            .and_then(|v| v.as_array())
            .ok_or_else(|| {
                StructuredOcrError::protocol_error(format!(
                    "line[{line_idx}] 缺少 word_indices 字段或非数组"
                ))
            })?
            .iter()
            .enumerate()
            .map(|(i, v)| {
                let n = v.as_u64().ok_or_else(|| {
                    StructuredOcrError::protocol_error(format!(
                        "line[{line_idx}].word_indices[{i}] 不是非负整数"
                    ))
                })?;
                Ok::<usize, StructuredOcrError>(n as usize)
            })
            .collect::<Result<Vec<_>, _>>()?;

        for (i, &idx) in word_indices.iter().enumerate() {
            if idx >= words_count {
                return Err(StructuredOcrError::protocol_error(format!(
                    "line[{line_idx}].word_indices[{i}] 越界：{idx} >= words.len()={words_count}"
                )));
            }
        }

        let mut seen = std::collections::HashSet::new();
        for (i, &idx) in word_indices.iter().enumerate() {
            if !seen.insert(idx) {
                return Err(StructuredOcrError::protocol_error(format!(
                    "line[{line_idx}].word_indices[{i}] 重复引用 word[{idx}]"
                )));
            }
        }

        lines.push(OcrLine {
            text,
            bounding_rect: rect,
            word_indices,
        });
    }

    // ── 9. 解析 words ──
    let mut word_ref_count = vec![0u32; words_count];

    for (word_idx, word_val) in words_arr.iter().enumerate() {
        let text = word_val
            .get("text")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                StructuredOcrError::protocol_error(format!(
                    "word[{word_idx}] 缺少 text 字段或类型错误"
                ))
            })?
            .to_string();
        if text.is_empty() {
            return Err(StructuredOcrError::protocol_error(format!(
                "word[{word_idx}] text 为空字符串"
            )));
        }

        let rect = parse_rect_strict(word_val, word_idx, image_width, image_height)?;

        let line_index_val = word_val.get("line_index").ok_or_else(|| {
            StructuredOcrError::protocol_error(format!("word[{word_idx}] 缺少 line_index 字段"))
        })?;
        let line_index = line_index_val.as_u64().ok_or_else(|| {
            StructuredOcrError::protocol_error(format!("word[{word_idx}].line_index 不是非负整数"))
        })? as usize;
        if line_index >= lines.len() {
            return Err(StructuredOcrError::protocol_error(format!(
                "word[{word_idx}].line_index 越界：{line_index} >= lines.len()={}",
                lines.len()
            )));
        }

        words.push(OcrWord {
            text,
            bounding_rect: rect,
            line_index,
        });
    }

    // ── 10. line.word_indices 与 word.line_index 双向一致 ──
    for (line_idx, line) in lines.iter().enumerate() {
        for &word_idx in &line.word_indices {
            if words[word_idx].line_index != line_idx {
                return Err(StructuredOcrError::protocol_error(format!(
                    "双向一致校验失败：word[{word_idx}].line_index={} 但被 line[{line_idx}] 引用",
                    words[word_idx].line_index
                )));
            }
            word_ref_count[word_idx] += 1;
        }
    }

    for (word_idx, &count) in word_ref_count.iter().enumerate() {
        if count == 0 {
            return Err(StructuredOcrError::protocol_error(format!(
                "word[{word_idx}] 未被任何 line 引用"
            )));
        }
        if count > 1 {
            return Err(StructuredOcrError::protocol_error(format!(
                "word[{word_idx}] 被多个 line 引用（count={count}）"
            )));
        }
    }

    let text = crate::domain::capability::builtins::ocr_engine::join_words_smart(&words, &lines);

    Ok(OcrResult {
        text,
        lines,
        words,
        text_angle: None,
    })
}

fn parse_rect_strict(
    val: &serde_json::Value,
    context_idx: usize,
    image_width: u32,
    image_height: u32,
) -> Result<OcrRect, StructuredOcrError> {
    let rect = val.get("rect").unwrap_or(val);
    let x = rect.get("x").and_then(|v| v.as_i64()).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.x 缺失或类型错误"))
    })?;
    let y = rect.get("y").and_then(|v| v.as_i64()).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.y 缺失或类型错误"))
    })?;
    let w = rect.get("w").and_then(|v| v.as_u64()).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.w 缺失或类型错误"))
    })?;
    let h = rect.get("h").and_then(|v| v.as_u64()).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.h 缺失或类型错误"))
    })?;

    if x < 0 || y < 0 {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect 坐标不能为负：x={x}, y={y}"
        )));
    }
    if w == 0 || h == 0 {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect 宽高必须 > 0：w={w}, h={h}"
        )));
    }

    let x_u32 = u32::try_from(x).map_err(|_| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.x 溢出 u32：{x}"))
    })?;
    let y_u32 = u32::try_from(y).map_err(|_| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.y 溢出 u32：{y}"))
    })?;
    let w_u32 = u32::try_from(w).map_err(|_| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.w 溢出 u32：{w}"))
    })?;
    let h_u32 = u32::try_from(h).map_err(|_| {
        StructuredOcrError::protocol_error(format!("item[{context_idx}] rect.h 溢出 u32：{h}"))
    })?;

    let x_plus_w = x_u32.checked_add(w_u32).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect x+w 溢出：x={x_u32}, w={w_u32}"
        ))
    })?;
    let y_plus_h = y_u32.checked_add(h_u32).ok_or_else(|| {
        StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect y+h 溢出：y={y_u32}, h={h_u32}"
        ))
    })?;

    if x_plus_w > image_width {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect x+w={x_plus_w} 超出 image_width={image_width}"
        )));
    }
    if y_plus_h > image_height {
        return Err(StructuredOcrError::protocol_error(format!(
            "item[{context_idx}] rect y+h={y_plus_h} 超出 image_height={image_height}"
        )));
    }

    Ok(OcrRect {
        x: x as i32,
        y: y as i32,
        w: w_u32,
        h: h_u32,
    })
}
