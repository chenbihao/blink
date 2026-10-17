//! 预览只读取图片头部；像素解码由 WebView 执行。
use image::{ImageDecoder, ImageFormat};
use std::io::Cursor;

/// 限制解析器的元数据/辅助缓冲分配，不按完整像素量拒绝正常图片。
pub(crate) fn preview_dimensions(
    bytes: &[u8],
    format: ImageFormat,
) -> image::ImageResult<(u32, u32)> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(64 * 1024 * 1024);
    reader.limits(limits);
    let dimensions = reader.into_decoder()?.dimensions();
    if dimensions.0 == 0 || dimensions.1 == 0 {
        return Err(image::ImageError::Limits(
            image::error::LimitError::from_kind(image::error::LimitErrorKind::DimensionError),
        ));
    }
    Ok(dimensions)
}
