//! 只合成标注画布；窗口捕获排除负责移除工具栏、边框及输入控件。
use std::{
    io::Cursor,
    sync::{Arc, Mutex},
};

pub const MAX_BYTES: usize = 64 << 20;
pub type SharedAnnotation = Arc<Mutex<AnnotationState>>;

#[derive(Default)]
pub struct AnnotationState {
    pub revision: u64,
    pub frame: Option<Arc<AnnotationFrame>>,
}

pub struct AnnotationFrame {
    width: u32,
    height: u32,
    // 空白行不保存；各行只遍历首尾非透明像素之间的区段。
    rows: Vec<(usize, Box<[u8]>)>,
}

impl AnnotationFrame {
    pub fn decode(png: &[u8], width: u32, height: u32) -> Result<Self, String> {
        let size = u64::from(width)
            .saturating_mul(u64::from(height))
            .saturating_mul(4);
        if width == 0 || height == 0 || size > MAX_BYTES as u64 || png.len() > MAX_BYTES {
            return Err("标注画布超过解码预算".into());
        }
        let mut reader = image::ImageReader::with_format(Cursor::new(png), image::ImageFormat::Png);
        let mut limits = image::Limits::default();
        limits.max_image_width = Some(width);
        limits.max_image_height = Some(height);
        limits.max_alloc = Some((MAX_BYTES as u64) * 2);
        reader.limits(limits);
        let image = reader
            .decode()
            .map_err(|e| format!("标注解码失败: {e}"))?
            .into_rgba8();
        if image.dimensions() != (width, height) {
            return Err("标注画布与录制区域尺寸不匹配".into());
        }
        Ok(Self::from_rgba(image.as_raw(), width, height))
    }

    fn from_rgba(rgba: &[u8], width: u32, height: u32) -> Self {
        let stride = width as usize * 4;
        let mut rows = Vec::new();
        for (y, row) in rgba.chunks_exact(stride).enumerate() {
            let first = row.chunks_exact(4).position(|p| p[3] != 0);
            let last = row.chunks_exact(4).rposition(|p| p[3] != 0);
            if let (Some(first), Some(last)) = (first, last) {
                rows.push((
                    y * stride + first * 4,
                    row[first * 4..(last + 1) * 4].into(),
                ));
            }
        }
        Self {
            width,
            height,
            rows,
        }
    }

    pub fn blend_bgra(&self, frame: &mut [u8], width: u32, height: u32) {
        if (width, height) != (self.width, self.height)
            || frame.len() != width as usize * height as usize * 4
        {
            return;
        }
        for (offset, rgba) in &self.rows {
            for (dest, src) in frame[*offset..*offset + rgba.len()]
                .chunks_exact_mut(4)
                .zip(rgba.chunks_exact(4))
            {
                let alpha = u32::from(src[3]);
                if alpha == 0 {
                    continue;
                }
                for channel in 0..3 {
                    dest[channel] = ((u32::from(src[2 - channel]) * alpha
                        + u32::from(dest[channel]) * (255 - alpha)
                        + 127)
                        / 255) as u8;
                }
                dest[3] = 255;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn transparent_pixels_stay_unchanged_and_translucent_ink_blends_once() {
        let ink = AnnotationFrame::from_rgba(&[0, 0, 0, 0, 255, 0, 0, 128, 0, 255, 0, 255], 3, 1);
        let mut frame = vec![100, 100, 100, 255, 100, 100, 100, 255, 100, 100, 100, 255];
        ink.blend_bgra(&mut frame, 3, 1);
        assert_eq!(
            frame,
            [100, 100, 100, 255, 50, 50, 178, 255, 0, 255, 0, 255]
        );
        assert_eq!(ink.rows.len(), 1);
        assert_eq!(ink.rows[0].0, 4);
        let unchanged = frame.clone();
        ink.blend_bgra(&mut frame, 1, 3);
        assert_eq!(frame, unchanged);
    }
    #[test]
    fn png_dimensions_and_budget_are_validated_before_publication() {
        let image = image::RgbaImage::from_pixel(2, 2, image::Rgba([255, 0, 0, 128]));
        let mut png = Cursor::new(Vec::new());
        image.write_to(&mut png, image::ImageFormat::Png).unwrap();
        assert!(AnnotationFrame::decode(png.get_ref(), 2, 2).is_ok());
        assert!(AnnotationFrame::decode(png.get_ref(), 1, 1).is_err());
        assert!(AnnotationFrame::decode(png.get_ref(), 3, 2).is_err());
        assert!(AnnotationFrame::decode(png.get_ref(), u32::MAX, u32::MAX).is_err());
    }
}
