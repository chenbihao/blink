//! 编辑器预览二进制：8 字节 LE 宽高 + RGBA，避免 PNG 编码/Base64/再次解码。
pub const MAX_BYTES: usize = 16 << 20;

pub fn frame(bgra: &[u8], width: u32, height: u32, max_width: u32) -> Result<Vec<u8>, String> {
    let source_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4));
    if width == 0 || height == 0 || source_bytes != Some(bgra.len() as u64) {
        return Err("预览源帧尺寸无效".into());
    }
    let scale = (max_width.clamp(80, 4096) as f64 / width as f64).min(1.0);
    let w = (width as f64 * scale).round().max(1.0) as u32;
    let h = (height as f64 * scale).round().max(1.0) as u32;
    if u64::from(w) * u64::from(h) * 4 > MAX_BYTES as u64 {
        return Err("单帧预览超过 16MiB，请降低预览尺寸".into());
    }
    let rgba = super::encode::resize_bgra(bgra, width, height, w, h);
    let mut bytes = Vec::with_capacity(8 + rgba.len());
    bytes.extend(w.to_le_bytes());
    bytes.extend(h.to_le_bytes());
    bytes.extend(rgba);
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn binary_preview_has_exact_dimensions_rgba_order_and_bounded_size() {
        let bytes = frame(&[10, 20, 30, 255, 40, 50, 60, 255], 2, 1, 80).unwrap();
        assert_eq!(&bytes[..8], &[2, 0, 0, 0, 1, 0, 0, 0]);
        assert_eq!(&bytes[8..], &[30, 20, 10, 255, 60, 50, 40, 255]);
        assert!(frame(&[0; 4], 0, 1, 80).is_err());
        assert!(frame(&[0; 4], 2, 1, 80).is_err());
        assert!(frame(&[0; 4], u32::MAX, u32::MAX, 80).is_err());
        let pixels = vec![0; 64 * 64 * 4];
        let small = frame(&pixels, 64, 64, 80).unwrap();
        assert_eq!(small.len(), 8 + pixels.len());
    }
}
