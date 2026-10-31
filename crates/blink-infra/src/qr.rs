//! Bounded, local QR decoding. No network, model, logs or persistent results.
use std::collections::HashSet;
use std::io::Cursor;
use std::time::{Duration, Instant};

pub const MAX_PIXELS: u64 = 8_000_000;
pub const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;
pub const DECODE_BUDGET: Duration = Duration::from_secs(5);
const MAX_RESULTS: usize = 32;
const MAX_TEXT_BYTES: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum QrError {
    #[error("QR input or result exceeds resource budget")]
    ResourceLimit,
    #[error("Invalid QR image")]
    InvalidImage,
    #[error("QR decoding timed out")]
    Timeout,
    #[error("QR decoder failed")]
    DecoderFailed,
}

pub fn validate_dimensions(w: u32, h: u32) -> Result<(), QrError> {
    if w == 0 || h == 0 || u64::from(w) * u64::from(h) > MAX_PIXELS {
        return Err(QrError::ResourceLimit);
    }
    Ok(())
}

/// The pixel ceiling is checked before PNG allocation, including editor images.
pub fn decode_png(png: &[u8]) -> Result<Vec<String>, QrError> {
    if png.len() > MAX_INPUT_BYTES {
        return Err(QrError::ResourceLimit);
    }
    let started = Instant::now();
    let decoder =
        image::codecs::png::PngDecoder::new(Cursor::new(png)).map_err(|_| QrError::InvalidImage)?;
    use image::ImageDecoder;
    let (w, h) = decoder.dimensions();
    validate_dimensions(w, h)?;
    let gray = image::DynamicImage::from_decoder(decoder)
        .map_err(|_| QrError::InvalidImage)?
        .into_luma8();
    std::panic::catch_unwind(move || decode_gray(gray, started))
        .map_err(|_| QrError::DecoderFailed)?
}

fn decode_gray(gray: image::GrayImage, started: Instant) -> Result<Vec<String>, QrError> {
    let mut prepared = rqrr::PreparedImage::prepare(gray);
    let grids = prepared.detect_grids();
    if started.elapsed() > DECODE_BUDGET {
        return Err(QrError::Timeout);
    }
    // Limit candidate decode work too, including damaged/non-text codes.
    if grids.len() > MAX_RESULTS * 2 {
        return Err(QrError::ResourceLimit);
    }
    let mut results = Vec::new();
    for grid in grids {
        if started.elapsed() > DECODE_BUDGET {
            return Err(QrError::Timeout);
        }
        if let Ok((_, content)) = grid.decode() {
            let y = grid.bounds.iter().map(|p| p.y).min().unwrap_or(0);
            let x = grid.bounds.iter().map(|p| p.x).min().unwrap_or(0);
            results.push((y, x, content));
        }
    }
    project_results(results)
}

fn project_results(mut results: Vec<(i32, i32, String)>) -> Result<Vec<String>, QrError> {
    results.sort_by_key(|(y, x, _)| (*y, *x));
    let mut seen = HashSet::new();
    let mut bytes = 0;
    let mut output = Vec::new();
    for (_, _, content) in results {
        if !seen.insert(content.clone()) {
            continue;
        }
        bytes += content.len();
        if output.len() >= MAX_RESULTS || bytes > MAX_TEXT_BYTES {
            return Err(QrError::ResourceLimit);
        }
        output.push(content);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_corrupt_and_oversized_input_before_decode() {
        assert!(matches!(decode_png(b"broken"), Err(QrError::InvalidImage)));
        assert!(validate_dimensions(4000, 3000).is_err());
        assert!(validate_dimensions(0, 100).is_err());
        assert!(validate_dimensions(3840, 2000).is_ok());
    }
    #[test]
    fn empty_image_is_a_normal_empty_result() {
        let gray = image::GrayImage::from_pixel(200, 200, image::Luma([255]));
        assert!(decode_gray(gray, Instant::now()).unwrap().is_empty());
    }

    fn qr_image(content: &str, scale: u32) -> image::GrayImage {
        let code = qrcode::QrCode::new(content.as_bytes()).unwrap();
        let width = code.width() as u32;
        image::GrayImage::from_fn((width + 8) * scale, (width + 8) * scale, |x, y| {
            let x = x / scale;
            let y = y / scale;
            image::Luma([
                if x >= 4
                    && y >= 4
                    && x < width + 4
                    && y < width + 4
                    && code[((x - 4) as usize, (y - 4) as usize)] == qrcode::Color::Dark
                {
                    0
                } else {
                    255
                },
            ])
        })
    }
    fn png(image: image::GrayImage) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::DynamicImage::ImageLuma8(image)
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let bytes = bytes.into_inner();
        assert!(bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
        bytes
    }
    #[test]
    fn actual_png_roundtrip_handles_chinese_long_text_rotation_and_scale() {
        for text in [
            "https://example.com/qr",
            "中文文本，空格与百分号 100%",
            &"long text ".repeat(80),
        ] {
            let image = qr_image(text, 4);
            assert_eq!(
                decode_png(&png(image::imageops::rotate90(&image))).unwrap(),
                [text]
            );
        }
    }
    #[test]
    fn multi_code_output_is_sorted_and_deduplicated() {
        let first = qr_image("top left", 4);
        let second = qr_image("bottom right", 5);
        let mut image = image::GrayImage::from_pixel(600, 600, image::Luma([255]));
        image::imageops::overlay(&mut image, &second, 350, 350);
        image::imageops::overlay(&mut image, &first, 10, 10);
        image::imageops::overlay(&mut image, &first, 350, 10);
        assert_eq!(
            decode_png(&png(image)).unwrap(),
            ["top left", "bottom right"]
        );
    }
    #[test]
    fn resource_and_time_budgets_reject_instead_of_truncating() {
        let results = (0..33).map(|i| (i, 0, format!("item-{i}"))).collect();
        assert!(matches!(
            project_results(results),
            Err(QrError::ResourceLimit)
        ));
        assert!(matches!(
            project_results(vec![(0, 0, "x".repeat(MAX_TEXT_BYTES + 1))]),
            Err(QrError::ResourceLimit)
        ));
        assert!(matches!(
            decode_gray(qr_image("late", 4), Instant::now() - DECODE_BUDGET),
            Err(QrError::Timeout)
        ));
    }
}
