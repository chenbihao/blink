//! 预览检查文件头/动画结构后交 WebView 解码，转存才在 Rust 展开像素。
use super::{DECODE_BUDGET, FilePreviewError};
use image::{DynamicImage, ImageDecoder, ImageFormat};
use std::io::Cursor;

const MAX_ANIMATION_FRAMES: usize = 200;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum FileImageFormat {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl FileImageFormat {
    pub(super) fn detect(bytes: &[u8]) -> Result<Self, FilePreviewError> {
        match image::guess_format(bytes) {
            Ok(ImageFormat::Png) => Ok(Self::Png),
            Ok(ImageFormat::Jpeg) => Ok(Self::Jpeg),
            Ok(ImageFormat::Gif) => Ok(Self::Gif),
            Ok(ImageFormat::WebP) => Ok(Self::Webp),
            _ => Err(FilePreviewError::NotImage),
        }
    }
    pub(super) fn mime_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }
    fn image_format(self) -> ImageFormat {
        match self {
            Self::Png => ImageFormat::Png,
            Self::Jpeg => ImageFormat::Jpeg,
            Self::Gif => ImageFormat::Gif,
            Self::Webp => ImageFormat::WebP,
        }
    }
}

pub(super) struct ValidatedImage {
    pub width: u32,
    pub height: u32,
    pub image: Option<DynamicImage>,
}

fn decode_error(error: image::ImageError) -> FilePreviewError {
    FilePreviewError::Decode(error.to_string())
}
fn limits() -> image::Limits {
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_BUDGET);
    limits
}
pub(super) fn check_pixels(
    width: u32,
    height: u32,
    decoder_bytes: u64,
) -> Result<(), FilePreviewError> {
    let rgba_bytes = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|n| n.checked_mul(4))
        .ok_or(FilePreviewError::PixelBudget)?;
    if width == 0 || height == 0 || rgba_bytes.max(decoder_bytes) > DECODE_BUDGET {
        tracing::debug!(
            width,
            height,
            rgba_bytes,
            decoder_bytes,
            max_bytes = DECODE_BUDGET,
            "图片转存解码预算超限"
        );
        return Err(FilePreviewError::PixelBudget);
    }
    Ok(())
}

pub(super) fn validate(
    bytes: &[u8],
    format: FileImageFormat,
    preview: bool,
) -> Result<ValidatedImage, FilePreviewError> {
    if format == FileImageFormat::Gif {
        if !preview {
            return Err(FilePreviewError::PreviewOnly);
        }
        return validate_gif_headers(bytes);
    }
    if preview && format == FileImageFormat::Webp {
        let decoder = image_webp::WebPDecoder::new(Cursor::new(bytes))
            .map_err(|e| FilePreviewError::Decode(e.to_string()))?;
        let (width, height) = decoder.dimensions();
        if decoder.is_animated() {
            check_animation_frames(decoder.num_frames() as usize)?;
        }
        return Ok(ValidatedImage {
            width,
            height,
            image: None,
        });
    }
    if preview {
        let (width, height) =
            crate::utils::image_metadata::preview_dimensions(bytes, format.image_format())
                .map_err(decode_error)?;
        return Ok(ValidatedImage {
            width,
            height,
            image: None,
        });
    }
    if format == FileImageFormat::Webp {
        let decoder =
            image::codecs::webp::WebPDecoder::new(Cursor::new(bytes)).map_err(decode_error)?;
        if decoder.has_animation() {
            return Err(FilePreviewError::PreviewOnly);
        }
    }
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), format.image_format());
    reader.limits(limits());
    let mut decoder = reader.into_decoder().map_err(decode_error)?;
    let (width, height) = decoder.dimensions();
    check_pixels(width, height, decoder.total_bytes())?;
    let orientation = decoder.orientation().map_err(decode_error)?;
    let mut image = DynamicImage::from_decoder(decoder).map_err(decode_error)?;
    // 原格式预览由 WebView 解释 EXIF；转 PNG 会丢 EXIF，先将方向作用到像素。
    image.apply_orientation(orientation);
    Ok(ValidatedImage {
        width: image.width(),
        height: image.height(),
        image: Some(image),
    })
}

fn check_animation_frames(count: usize) -> Result<(), FilePreviewError> {
    if count == 0 || count > MAX_ANIMATION_FRAMES {
        return Err(FilePreviewError::AnimationBudget);
    }
    Ok(())
}

fn validate_gif_headers(bytes: &[u8]) -> Result<ValidatedImage, FilePreviewError> {
    let mut options = gif::DecodeOptions::new();
    options.skip_frame_decoding(true);
    options.check_frame_consistency(true);
    let mut decoder = options
        .read_info(Cursor::new(bytes))
        .map_err(|e| FilePreviewError::Decode(e.to_string()))?;
    let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
    let mut count = 0;
    // next_frame_info 跳过 LZW 像素解码，仍扫描每帧边界/颜色表/数据块。
    while decoder
        .next_frame_info()
        .map_err(|e| FilePreviewError::Decode(e.to_string()))?
        .is_some()
    {
        count += 1;
        check_animation_frames(count)?;
    }
    if count == 0 {
        return Err(FilePreviewError::AnimationBudget);
    }
    Ok(ValidatedImage {
        width,
        height,
        image: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn gif(frames: usize) -> Vec<u8> {
        let mut bytes = Vec::new();
        {
            let mut encoder = image::codecs::gif::GifEncoder::new(&mut bytes);
            encoder
                .encode_frames((0..frames).map(|i| {
                    image::Frame::new(image::RgbaImage::from_pixel(
                        2,
                        2,
                        image::Rgba([i as u8, 20, 80, 255]),
                    ))
                }))
                .unwrap();
        }
        bytes
    }
    #[test]
    fn formats_validate_content_and_keep_gif_preview_only() {
        for (format, mime) in [
            (ImageFormat::Png, "image/png"),
            (ImageFormat::Jpeg, "image/jpeg"),
            (ImageFormat::WebP, "image/webp"),
        ] {
            let mut bytes = Cursor::new(Vec::new());
            image::RgbImage::from_pixel(12, 8, image::Rgb([90, 120, 150]))
                .write_to(&mut bytes, format)
                .unwrap();
            let format = FileImageFormat::detect(bytes.get_ref()).unwrap();
            assert_eq!(format.mime_type(), mime);
            let decoded = validate(bytes.get_ref(), format, false).unwrap();
            assert_eq!((decoded.width, decoded.height), (12, 8));
        }
        let bytes = gif(2);
        assert_eq!(
            FileImageFormat::detect(&bytes).unwrap(),
            FileImageFormat::Gif
        );
        assert!(validate(&bytes, FileImageFormat::Gif, true).is_ok());
        assert!(matches!(
            validate(&bytes, FileImageFormat::Gif, false),
            Err(FilePreviewError::PreviewOnly)
        ));
        assert!(FileImageFormat::detect(b"not an image").is_err());
        assert!(validate(b"\x89PNG\r\n\x1a\n", FileImageFormat::Png, true).is_err());
    }
    #[test]
    fn animation_frame_and_pixel_budgets_are_bounded() {
        assert!(matches!(
            validate(&gif(MAX_ANIMATION_FRAMES + 1), FileImageFormat::Gif, true),
            Err(FilePreviewError::AnimationBudget)
        ));
        assert!(check_pixels(8192, 8193, 0).is_err());
        assert!(check_pixels(12, 8, DECODE_BUDGET + 1).is_err());
        assert!(check_pixels(u32::MAX, u32::MAX, 0).is_err());
    }

    #[test]
    fn large_gif_canvas_does_not_require_rust_compositing_buffers() {
        // GIF allows small frame rectangles on a large logical canvas.
        let mut source = gif(2);
        source[6..8].copy_from_slice(&3317u16.to_le_bytes());
        source[8..10].copy_from_slice(&3205u16.to_le_bytes());
        let preview = validate(&source, FileImageFormat::Gif, true).unwrap();
        assert_eq!((preview.width, preview.height), (3317, 3205));
        assert!(preview.image.is_none());
        assert!(matches!(
            validate(&source, FileImageFormat::Gif, false),
            Err(FilePreviewError::PreviewOnly)
        ));
    }

    #[test]
    fn preview_accepts_large_canvas_while_import_keeps_pixel_budget() {
        assert!(check_pixels(2459, 12448, 0).is_ok());
        assert!(check_pixels(5437, 8467, 0).is_ok());
        // A static GIF on a large canvas must not be rejected by estimated RGBA memory.
        let mut source = gif(1);
        source[6..8].copy_from_slice(&5437u16.to_le_bytes());
        source[8..10].copy_from_slice(&8467u16.to_le_bytes());
        assert!(validate(&source, FileImageFormat::Gif, true).is_ok());
        assert!(check_animation_frames(0).is_err());
    }

    #[test]
    fn jpeg_import_applies_exif_orientation_but_preview_keeps_source_layout() {
        let mut jpeg = Cursor::new(Vec::new());
        image::RgbImage::from_pixel(12, 8, image::Rgb([90, 120, 150]))
            .write_to(&mut jpeg, ImageFormat::Jpeg)
            .unwrap();
        let jpeg = jpeg.into_inner();
        // TIFF little endian IFD0: Orientation=6 (Rotate90).
        let exif = b"Exif\0\0II\x2a\0\x08\0\0\0\x01\0\x12\x01\x03\0\x01\0\0\0\x06\0\0\0\0\0\0\0";
        let mut source = jpeg[..2].to_vec();
        source.extend_from_slice(&[0xff, 0xe1]);
        source.extend_from_slice(&((exif.len() + 2) as u16).to_be_bytes());
        source.extend_from_slice(exif);
        source.extend_from_slice(&jpeg[2..]);
        let preview = validate(&source, FileImageFormat::Jpeg, true).unwrap();
        assert_eq!((preview.width, preview.height), (12, 8));
        let imported = validate(&source, FileImageFormat::Jpeg, false).unwrap();
        assert_eq!((imported.width, imported.height), (8, 12));
    }

    #[test]
    fn animated_webp_is_validated_and_not_flattened_on_import() {
        fn chunk(out: &mut Vec<u8>, tag: &[u8; 4], data: &[u8]) {
            out.extend_from_slice(tag);
            out.extend_from_slice(&(data.len() as u32).to_le_bytes());
            out.extend_from_slice(data);
            if data.len() % 2 == 1 {
                out.push(0);
            }
        }
        let mut chunks = Vec::new();
        chunk(&mut chunks, b"VP8X", &[2, 0, 0, 0, 1, 0, 0, 1, 0, 0]);
        chunk(&mut chunks, b"ANIM", &[0, 0, 0, 0, 0, 0]);
        for red in [30, 180] {
            let mut still = Cursor::new(Vec::new());
            image::RgbImage::from_pixel(2, 2, image::Rgb([red, 80, 120]))
                .write_to(&mut still, ImageFormat::WebP)
                .unwrap();
            let mut frame = vec![0, 0, 0, 0, 0, 0, 1, 0, 0, 1, 0, 0, 20, 0, 0, 0];
            frame.extend_from_slice(&still.get_ref()[12..]);
            chunk(&mut chunks, b"ANMF", &frame);
        }
        let mut source = b"RIFF".to_vec();
        source.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
        source.extend_from_slice(b"WEBP");
        source.extend_from_slice(&chunks);
        assert!(validate(&source, FileImageFormat::Webp, true).is_ok());
        assert!(matches!(
            validate(&source, FileImageFormat::Webp, false),
            Err(FilePreviewError::PreviewOnly)
        ));
    }
}
