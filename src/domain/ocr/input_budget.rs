//! OCR 输入资源预算（0.22.6.1）。
//!
//! 与 Python 侧 `resources/ocr/paddleocr/blink_ocr_server.py` 的信任边界
//! **两侧一致**（后者在 PIL → numpy 解码前重复执行同一检查）：
//!
//! - compressed/input bytes ≤ 32 MiB（与 ImageStash 单项上限一致）
//! - 单边尺寸 ≤ 16384 px
//! - decoded RGB/RGBA 预算 ≤ 256 MiB（`width * height * 4`，checked 乘法）
//!
//! 检查全部基于 PNG header（24 字节），**不在完成预算检查前解码像素**。
//! 超预算返回 `InputTooLarge`（带实际值与允许上限），格式损坏返回
//! `DecodeError`；不记录图片内容。资源上限不删除——资源有界是必要安全机制。
//!
//! 契约锁定：本模块测试会校验 Python 服务源码中的字面量常量，
//! 防止 Rust/Python 预算静默漂移。

use bytes::Bytes;

use super::error::StructuredOcrError;

/// compressed/input bytes 上限：32 MiB（与 ImageStash 单项上限一致）。
pub const MAX_COMPRESSED_BYTES: usize = 32 * 1024 * 1024;

/// 单边尺寸上限：16384 px。
pub const MAX_DIMENSION: u32 = 16_384;

/// decoded 像素预算上限：256 MiB（按 RGBA 4 通道计）。
pub const MAX_DECODED_BYTES: u64 = 256 * 1024 * 1024;

/// 校验 OCR 输入 PNG 并返回其尺寸。
///
/// 通过后返回 `(width, height)`——调用方（response mapper）可复用它做
/// 响应尺寸一致性校验，无需二次解析。
pub fn validate_ocr_input(png: &Bytes) -> Result<(u32, u32), StructuredOcrError> {
    // 1. 非空
    if png.is_empty() {
        return Err(StructuredOcrError::decode_error("OCR 输入为空"));
    }

    // 2. compressed bytes 预算
    if png.len() > MAX_COMPRESSED_BYTES {
        return Err(StructuredOcrError::input_too_large(
            format!(
                "OCR 输入压缩字节 {} 超出上限 {}",
                png.len(),
                MAX_COMPRESSED_BYTES
            ),
            serde_json::json!({
                "field": "compressed_bytes",
                "actual": png.len(),
                "max": MAX_COMPRESSED_BYTES,
            }),
        ));
    }

    // 3. PNG header / 尺寸合法（header 级检查，不解码像素）
    let (width, height) =
        crate::infra::platform::screenshot::parse_png_size(png).ok_or_else(|| {
            StructuredOcrError::decode_error("无法解析 PNG header（签名/IHDR/尺寸非法）")
        })?;

    // 4. 单边尺寸预算
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(StructuredOcrError::input_too_large(
            format!("OCR 输入尺寸 {width}x{height} 单边超出上限 {MAX_DIMENSION}"),
            serde_json::json!({
                "field": "dimensions",
                "actual_width": width,
                "actual_height": height,
                "max_side": MAX_DIMENSION,
            }),
        ));
    }

    // 5. decoded 像素预算（checked 乘法，width * height * 4 通道）
    let decoded_bytes = (u64::from(width))
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .ok_or_else(|| {
            StructuredOcrError::input_too_large(
                format!("OCR 输入 {width}x{height} decoded 像素预算计算溢出"),
                serde_json::json!({
                    "field": "decoded_bytes",
                    "actual_width": width,
                    "actual_height": height,
                    "max": MAX_DECODED_BYTES,
                }),
            )
        })?;
    if decoded_bytes > MAX_DECODED_BYTES {
        return Err(StructuredOcrError::input_too_large(
            format!(
                "OCR 输入 {width}x{height} decoded 像素 {decoded_bytes} 字节超出上限 {MAX_DECODED_BYTES}"
            ),
            serde_json::json!({
                "field": "decoded_bytes",
                "actual_width": width,
                "actual_height": height,
                "actual_decoded_bytes": decoded_bytes,
                "max": MAX_DECODED_BYTES,
            }),
        ));
    }

    Ok((width, height))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::ocr::error::OcrErrorCategory;

    /// 构造最小合法 PNG header（signature + IHDR，无像素数据）。
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut buf = vec![
            0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A, // signature
            0x00, 0x00, 0x00, 0x0D, // IHDR length = 13
        ];
        buf.extend_from_slice(b"IHDR");
        buf.extend_from_slice(&width.to_be_bytes());
        buf.extend_from_slice(&height.to_be_bytes());
        buf.extend_from_slice(&[0x08, 0x06, 0x00, 0x00, 0x00]); // bit depth/color/etc
        buf
    }

    #[test]
    fn accepts_normal_screenshot_header() {
        let png = Bytes::from(png_header(1188, 800));
        let (w, h) = validate_ocr_input(&png).unwrap();
        assert_eq!((w, h), (1188, 800));
    }

    #[test]
    fn rejects_empty_input() {
        let err = validate_ocr_input(&Bytes::new()).unwrap_err();
        assert_eq!(err.category, OcrErrorCategory::DecodeError);
    }

    #[test]
    fn rejects_invalid_png_header() {
        let err = validate_ocr_input(&Bytes::from(vec![0u8; 64])).unwrap_err();
        assert_eq!(err.category, OcrErrorCategory::DecodeError);
    }

    #[test]
    fn rejects_oversized_compressed_bytes() {
        let mut png = png_header(100, 100);
        png.resize(MAX_COMPRESSED_BYTES + 1, 0);
        let err = validate_ocr_input(&Bytes::from(png)).unwrap_err();
        assert_eq!(err.category, OcrErrorCategory::InputTooLarge);
        assert_eq!(err.detail.as_ref().unwrap()["field"], "compressed_bytes");
        assert_eq!(
            err.detail.as_ref().unwrap()["max"],
            MAX_COMPRESSED_BYTES as u64
        );
    }

    #[test]
    fn rejects_oversized_dimension() {
        // 16384x16384 单边合法，但 decoded = 16384*16384*4 = 1 GiB 超预算；
        // 先用 16384x100 验证单边维度边界通过，再用超边值验证拒绝。
        let ok = Bytes::from(png_header(MAX_DIMENSION, 100));
        assert!(validate_ocr_input(&ok).is_ok());

        let bad = Bytes::from(png_header(MAX_DIMENSION + 1, 10));
        let err = validate_ocr_input(&bad).unwrap_err();
        assert_eq!(err.category, OcrErrorCategory::InputTooLarge);
        assert_eq!(err.detail.as_ref().unwrap()["field"], "dimensions");
        assert_eq!(
            err.detail.as_ref().unwrap()["max_side"],
            MAX_DIMENSION as u64
        );
    }

    #[test]
    fn rejects_oversized_decoded_budget() {
        // 16384x16384 单边合法但 decoded = 1 GiB > 256 MiB
        let png = Bytes::from(png_header(16_384, 16_384));
        let err = validate_ocr_input(&png).unwrap_err();
        assert_eq!(err.category, OcrErrorCategory::InputTooLarge);
        assert_eq!(err.detail.as_ref().unwrap()["field"], "decoded_bytes");
        assert_eq!(
            err.detail.as_ref().unwrap()["actual_decoded_bytes"],
            1_073_741_824u64
        );
    }

    #[test]
    fn decoded_budget_boundary_accepted() {
        // 8192x8192 = 64M px * 4 = 256 MiB —— 恰好等于预算，应通过
        let png = Bytes::from(png_header(8192, 8192));
        assert!(validate_ocr_input(&png).is_ok());
    }

    // ── Rust/Python 契约锁定（防两侧常量静默漂移） ─────────────────────────

    const PYTHON_SERVER_SRC: &str =
        include_str!("../../../resources/ocr/paddleocr/blink_ocr_server.py");

    #[test]
    fn python_budget_constants_match_rust_contract() {
        for expected in [
            "MAX_BODY_BYTES = 32 * 1024 * 1024",
            "MAX_DIMENSION = 16384",
            "MAX_DECODED_BYTES = 256 * 1024 * 1024",
        ] {
            assert!(
                PYTHON_SERVER_SRC.contains(expected),
                "Python OCR 服务预算常量漂移：缺少 `{expected}`（Rust 侧 32MiB/16384/256MiB）"
            );
        }
        // 413 必须用于超预算响应（投影为 input_too_large，而非 ProtocolError/Internal）
        assert!(
            PYTHON_SERVER_SRC.contains("status_code=413"),
            "Python OCR 服务超预算必须返回 HTTP 413"
        );
    }

    #[test]
    fn rust_constants_match_contract_values() {
        assert_eq!(MAX_COMPRESSED_BYTES, 32 * 1024 * 1024);
        assert_eq!(MAX_DIMENSION, 16_384);
        assert_eq!(MAX_DECODED_BYTES, 256 * 1024 * 1024);
    }
}
