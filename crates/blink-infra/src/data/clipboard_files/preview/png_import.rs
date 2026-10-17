//! 普通 PNG 逐行校验并取样：只保留最大 1024px 的中间小图，再生成 256px 缩略图。
//! 原图不重编码、不分配完整像素缓冲；交错/动画/EXIF PNG 回落既有完整解码路径。
use super::{FilePreviewError, encode_thumbnail_png, formats};
use image::{DynamicImage, Rgba, RgbaImage};
use std::io::Cursor;

type Thumbnail = (u32, u32, Vec<u8>);

pub(super) fn thumbnail(bytes: &[u8]) -> Result<Option<Thumbnail>, FilePreviewError> {
    let failure = |e: png::DecodingError| FilePreviewError::Decode(e.to_string());
    // 64MiB 限制的是逐行解码器的辅助分配，不是整图 RGBA 大小。
    let mut decoder = png::Decoder::new_with_limits(
        Cursor::new(bytes),
        png::Limits {
            bytes: 64 * 1024 * 1024,
        },
    );
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder.read_info().map_err(failure)?;
    let info = reader.info();
    let (width, height) = (info.width, info.height);
    formats::check_pixels(width, height, 0)?;
    if info.interlaced || info.animation_control.is_some() || info.exif_metadata.is_some() {
        return Ok(None);
    }
    let (color, depth) = reader.output_color_type();
    if depth != png::BitDepth::Eight || color == png::ColorType::Indexed {
        return Err(FilePreviewError::Decode(
            "unsupported PNG row format".into(),
        ));
    }
    let channels = color.samples();
    let longest = width.max(height);
    let sample_w = width
        .min(1024)
        .min((u64::from(width) * 1024 / u64::from(longest)).max(1) as u32);
    let sample_h = height
        .min(1024)
        .min((u64::from(height) * 1024 / u64::from(longest)).max(1) as u32);
    let mut sample = RgbaImage::new(sample_w, sample_h);
    // Center sampling into a bounded intermediate image; the final reduction keeps Triangle filtering.
    let coordinate = |target: u32, source_len: u32, target_len: u32| {
        ((2 * u64::from(target) + 1) * u64::from(source_len) / (2 * u64::from(target_len))) as u32
    };
    let mut target_y = 0;
    for y in 0..height {
        let row = reader
            .next_row()
            .map_err(failure)?
            .ok_or_else(|| FilePreviewError::Decode("missing PNG row".into()))?;
        if target_y == sample_h || y != coordinate(target_y, height, sample_h) {
            continue;
        }
        for x in 0..sample_w {
            let offset = coordinate(x, width, sample_w) as usize * channels;
            let pixel = row
                .data()
                .get(offset..offset + channels)
                .ok_or_else(|| FilePreviewError::Decode("invalid PNG row".into()))?;
            let rgba = match color {
                png::ColorType::Grayscale => [pixel[0], pixel[0], pixel[0], 255],
                png::ColorType::GrayscaleAlpha => [pixel[0], pixel[0], pixel[0], pixel[1]],
                png::ColorType::Rgb => [pixel[0], pixel[1], pixel[2], 255],
                png::ColorType::Rgba => [pixel[0], pixel[1], pixel[2], pixel[3]],
                png::ColorType::Indexed => unreachable!(),
            };
            sample.put_pixel(x, target_y, Rgba(rgba));
        }
        target_y += 1;
    }
    // Drain image data (including checksum) and trailing chunks before persisting any bytes.
    if reader.next_row().map_err(failure)?.is_some() {
        return Err(FilePreviewError::Decode("unexpected PNG row".into()));
    }
    reader.finish().map_err(failure)?;
    if reader.info().exif_metadata.is_some() {
        return Ok(None); // EXIF may follow IDAT; orientation still uses the canonical decode path.
    }
    let thumbnail = encode_thumbnail_png(&DynamicImage::ImageRgba8(sample))
        .map_err(FilePreviewError::Decode)?;
    Ok(Some((width, height, thumbnail)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn row_import_normalizes_color_depth_palette_and_alpha() {
        for (color, depth, data, expected) in [
            (
                png::ColorType::Grayscale,
                png::BitDepth::Eight,
                vec![40, 90],
                [40, 40, 40, 255],
            ),
            (
                png::ColorType::GrayscaleAlpha,
                png::BitDepth::Eight,
                vec![40, 120, 90, 255],
                [40, 40, 40, 120],
            ),
            (
                png::ColorType::Rgb,
                png::BitDepth::Eight,
                vec![40, 80, 120, 90, 100, 110],
                [40, 80, 120, 255],
            ),
            (
                png::ColorType::Rgba,
                png::BitDepth::Eight,
                vec![40, 80, 120, 150, 90, 100, 110, 255],
                [40, 80, 120, 150],
            ),
            (
                png::ColorType::Rgb,
                png::BitDepth::Sixteen,
                vec![40, 0, 80, 0, 120, 0, 90, 0, 100, 0, 110, 0],
                [40, 80, 120, 255],
            ),
            (
                png::ColorType::Indexed,
                png::BitDepth::One,
                vec![0b0100_0000],
                [40, 80, 120, 150],
            ),
        ] {
            let mut bytes = Vec::new();
            {
                let mut encoder = png::Encoder::new(&mut bytes, 2, 1);
                encoder.set_color(color);
                encoder.set_depth(depth);
                if color == png::ColorType::Indexed {
                    encoder.set_palette(vec![40, 80, 120, 90, 100, 110]);
                    encoder.set_trns(vec![150, 255]);
                }
                encoder
                    .write_header()
                    .unwrap()
                    .write_image_data(&data)
                    .unwrap();
            }
            let (width, height, thumb) = thumbnail(&bytes).unwrap().unwrap();
            assert_eq!((width, height), (2, 1));
            let image = image::load_from_memory(&thumb).unwrap().into_rgba8();
            assert_eq!(image.dimensions(), (2, 1), "小图不放大");
            assert_eq!(image.get_pixel(0, 0).0, expected, "{color:?}/{depth:?}");
        }
    }

    #[test]
    fn row_import_accepts_more_than_old_64mib_and_validates_trailing_data() {
        let (width, height) = (4096, 4097);
        let mut bytes = Vec::new();
        {
            let mut encoder = png::Encoder::new(&mut bytes, width, height);
            encoder.set_color(png::ColorType::Rgba);
            encoder.set_depth(png::BitDepth::Eight);
            encoder.set_compression(png::Compression::Fast);
            let mut writer = encoder.write_header().unwrap();
            let mut stream = writer.stream_writer().unwrap();
            let row = [20, 80, 160, 100].repeat(width as usize);
            for _ in 0..height {
                stream.write_all(&row).unwrap();
            }
            stream.finish().unwrap();
        }
        let (w, h, thumb) = thumbnail(&bytes).unwrap().unwrap();
        assert_eq!((w, h), (width, height));
        let image = image::load_from_memory(&thumb).unwrap().into_rgba8();
        assert_eq!(image.dimensions(), (256, 256));
        assert_eq!(image.get_pixel(100, 100).0, [20, 80, 160, 100]);
        bytes.truncate(bytes.len() - 5);
        assert!(
            thumbnail(&bytes).is_err(),
            "不能只取够缩略图就跳过剩余数据完整性校验"
        );
    }
}
