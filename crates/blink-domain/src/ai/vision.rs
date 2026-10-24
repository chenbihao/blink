//! Vision 图片输入管道（0.27.10，phase §3.4）。
//!
//! **边界**：
//! - 输入限制在校验顺序上**先于大分配**生效——编码体积上限 → 格式嗅探 →
//!   解码（带分配上限，拒绝 >24MP）→ 方向归一化 → 长边 ≤2048 缩放 →
//!   重编码（PNG/JPEG）→ 归一化后重新校验编码总量。
//! - 首版发送 PNG/JPEG；SVG 明确拒绝（光栅化不属于本层），GIF 明确拒绝并
//!   提示转静态图，BMP/WEBP 等同样拒绝——provider 约束不一，收窄到两条
//!   保守 wire 路径。
//! - 解码必须 `spawn_blocking`（调用方负责）；数值为集中命名的首版默认
//!   上限，由实际 provider 约束与性能验证校准（§3.4），不散落硬编码。

use image::DynamicImage;
use image::ImageDecoder;
use image::codecs::jpeg::JpegEncoder;
use image::codecs::png::PngEncoder;
use image::imageops::FilterType;

/// 单图编码输入上限（8 MiB）。
pub const MAX_ENCODED_BYTES_PER_IMAGE: u64 = 8 * 1024 * 1024;
/// 所有图片合计编码上限（16 MiB）。
pub const MAX_TOTAL_ENCODED_BYTES: u64 = 16 * 1024 * 1024;
/// 解码像素上限（24 MP）——超过要求用户缩小，不静默降采样绕过。
pub const MAX_PIXELS: u64 = 24_000_000;
/// 归一化长边上限（2048 px）。
pub const MAX_LONG_EDGE: u32 = 2048;
/// base64 展开系数（4/3）——编码总量按 wire 后体积校准。
const BASE64_EXPANSION: f64 = 4.0 / 3.0;

/// 图片 wire 格式（发送侧归一化后的两种保守路径）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageWireFormat {
    Png,
    Jpeg,
}

impl ImageWireFormat {
    pub fn mime(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
        }
    }
}

/// 图片输入错误——逐项反馈给用户（保留草稿，给可执行恢复操作）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VisionError(pub String);

impl std::fmt::Display for VisionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VisionError {}

fn err(reason: impl Into<String>) -> VisionError {
    VisionError(reason.into())
}

/// 魔数嗅探（不信任扩展名）。返回归一化前的源格式语义。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SniffedFormat {
    Png,
    Jpeg,
    Gif,
    Bmp,
    WebP,
    /// 以 `<?xml`/`<svg` 开头的 SVG 文本。
    Svg,
    Unknown,
}

pub fn sniff_format(bytes: &[u8]) -> SniffedFormat {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G']) {
        SniffedFormat::Png
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        SniffedFormat::Jpeg
    } else if bytes.starts_with(b"GIF8") {
        SniffedFormat::Gif
    } else if bytes.starts_with(b"BM") {
        SniffedFormat::Bmp
    } else if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        SniffedFormat::WebP
    } else {
        let head: &[u8] = &bytes[..bytes.len().min(256)];
        let trimmed = head
            .iter()
            .skip_while(|b| b.is_ascii_whitespace() || **b == b'\xEF' || **b == b'\xBB' || **b == b'\xBF')
            .copied()
            .take(5)
            .collect::<Vec<u8>>();
        if trimmed.starts_with(b"<?xml") || trimmed.starts_with(b"<svg") {
            return SniffedFormat::Svg;
        }
        SniffedFormat::Unknown
    }
}

/// 编码前校验（体积上限 + 格式白名单）——必须先于解码执行。
pub fn validate_encoded(bytes: &[u8]) -> Result<(), VisionError> {
    if bytes.is_empty() {
        return Err(err("图片数据为空"));
    }
    if bytes.len() as u64 > MAX_ENCODED_BYTES_PER_IMAGE {
        return Err(err(format!(
            "图片 {:.1} MiB 超过单图 8 MiB 上限，请压缩后再附加",
            bytes.len() as f64 / (1024.0 * 1024.0)
        )));
    }
    match sniff_format(bytes) {
        SniffedFormat::Png | SniffedFormat::Jpeg => Ok(()),
        SniffedFormat::Gif => Err(err("GIF 动图暂不支持，请先转为静态 PNG/JPEG")),
        SniffedFormat::Svg => Err(err("SVG 是矢量格式，请先导出为 PNG 再附加")),
        SniffedFormat::Bmp | SniffedFormat::WebP => Err(err(
            "仅支持 PNG/JPEG 图片，请先转换格式（BMP/WebP 不在首版支持范围）",
        )),
        SniffedFormat::Unknown => Err(err("无法识别的图片格式（仅支持 PNG/JPEG）")),
    }
}

/// 归一化产物——发给 provider 适配层的最终形态。
#[derive(Debug, Clone)]
pub struct NormalizedImage {
    pub format: ImageWireFormat,
    pub bytes: Vec<u8>,
    pub width: u32,
    pub height: u32,
    /// 保守估算的输入 tokens（预算用，§3.4 不复用单一 256 布尔惩罚）。
    pub estimated_tokens: usize,
}

impl NormalizedImage {
    /// wire MIME 类型（provider 适配层构造多模态 part 用）。
    pub fn mime(&self) -> &'static str {
        self.format.mime()
    }
}

/// 归一化主入口（**必须**在 `spawn_blocking` 中调用——解码/缩放/重编码为
/// CPU 密集操作，禁止在 async 上下文裸跑，spec-backend §一）。
///
/// 步骤：头部尺寸预检（§3.4：24MP 限制必须在大分配前生效——`into_dimensions`
/// 只读 PNG/JPEG 头不解码像素）→ 解码（limits 兜底）→ EXIF 方向归一化 →
/// 长边 >2048 缩放 → 重编码（PNG 保 PNG / JPEG 保 JPEG，JPEG 87 质量近似
/// 原视觉）→ 重校验归一化后编码体积。
pub fn normalize_image(bytes: Vec<u8>) -> Result<NormalizedImage, VisionError> {
    validate_encoded(&bytes)?;
    let source_format = match sniff_format(&bytes) {
        SniffedFormat::Png => ImageWireFormat::Png,
        SniffedFormat::Jpeg => ImageWireFormat::Jpeg,
        _ => unreachable!("validate_encoded 已拒绝其余格式"),
    };

    // 24MP 头部预检：超限在此拒绝，不把超大像素缓冲解码进内存
    let (w, h) = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| err(format!("图片格式探测失败: {e}")))?
        .into_dimensions()
        .map_err(|e| err(format!("图片尺寸读取失败: {e}")))?;
    let pixels = w as u64 * h as u64;
    if pixels > MAX_PIXELS {
        return Err(err(format!(
            "图片 {w}×{h}（{:.1} MP）超过 24 MP 上限，请缩小后再附加",
            pixels as f64 / 1_000_000.0
        )));
    }

    // 解码器 limits 兜底（头部与实际流不一致的异常流；宽高 20000 + 单次分配
    // 512MB，正常流已被上面的头部预检挡住）
    let mut limits = image::Limits::default();
    limits.max_image_width = Some(20_000);
    limits.max_image_height = Some(20_000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    let reader = image::ImageReader::new(std::io::Cursor::new(&bytes))
        .with_guessed_format()
        .map_err(|e| err(format!("图片格式探测失败: {e}")))?;
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| err(format!("图片解码器创建失败: {e}")))?;
    decoder
        .set_limits(limits)
        .map_err(|_| err("图片尺寸超出解码上限，请缩小后再附加"))?;
    let decoded = DynamicImage::from_decoder(decoder).map_err(|e| err(format!("图片解码失败: {e}")))?;

    // EXIF 方向归一化（JPEG 可能携带 Orientation；PNG/其他无方向语义）
    let oriented = apply_exif_orientation(decoded, &bytes, source_format);

    // 长边 ≤2048 缩放（超过才缩，等比）；宽高取旋转后实际值（Orientation
    // 5-8 是 90°/270°，宽高互换）
    let (image_final, width, height) = {
        let long_edge = oriented.width().max(oriented.height());
        if long_edge > MAX_LONG_EDGE {
            let scale = MAX_LONG_EDGE as f64 / long_edge as f64;
            let nw = ((oriented.width() as f64 * scale).round() as u32).max(1);
            let nh = ((oriented.height() as f64 * scale).round() as u32).max(1);
            let resized = oriented.resize_exact(nw, nh, FilterType::Lanczos3);
            (resized, nw, nh)
        } else {
            let (ow, oh) = (oriented.width(), oriented.height());
            (oriented, ow, oh)
        }
    };

    // 重编码：保持源 wire 格式（PNG 无损；JPEG 视觉无损档）
    let (format, encoded) = match source_format {
        ImageWireFormat::Png => {
            let rgba = image_final.to_rgba8();
            let mut out = Vec::new();
            let encoder = PngEncoder::new(std::io::Cursor::new(&mut out));
            rgba.write_with_encoder(encoder)
                .map_err(|e| err(format!("PNG 重编码失败: {e}")))?;
            (ImageWireFormat::Png, out)
        }
        ImageWireFormat::Jpeg => {
            let rgb = image_final.to_rgb8();
            let mut out = Vec::new();
            let encoder = JpegEncoder::new_with_quality(std::io::Cursor::new(&mut out), 87);
            rgb.write_with_encoder(encoder)
                .map_err(|e| err(format!("JPEG 重编码失败: {e}")))?;
            (ImageWireFormat::Jpeg, out)
        }
    };

    // 归一化后重新校验编码总量（§3.4：归一化后重新校验编码总量）
    if encoded.len() as u64 > MAX_ENCODED_BYTES_PER_IMAGE {
        return Err(err(format!(
            "图片归一化后 {:.1} MiB 仍超过单图上限，请降低分辨率",
            encoded.len() as f64 / (1024.0 * 1024.0)
        )));
    }

    Ok(NormalizedImage {
        format,
        estimated_tokens: estimate_vision_tokens(width, height),
        bytes: encoded,
        width,
        height,
    })
}

/// 全部图片的编码总量校验（base64 展开后按 16 MiB 计）。
pub fn check_total_encoded(images: &[&NormalizedImage]) -> Result<(), VisionError> {
    let total: u64 = images
        .iter()
        .map(|img| (img.bytes.len() as f64 * BASE64_EXPANSION) as u64)
        .sum();
    if total > MAX_TOTAL_ENCODED_BYTES {
        return Err(err(format!(
            "本轮图片合计 {:.1} MiB 超过 16 MiB 上限，请移除部分图片",
            total as f64 / (1024.0 * 1024.0)
        )));
    }
    Ok(())
}

/// 保守输入 token 估算（§3.4：按数量/尺寸保守估算，provider 校准留待实测）。
/// 取 `max(256, w*h/750)`——对齐常见 high-detail tile 估算的上包络。
pub fn estimate_vision_tokens(width: u32, height: u32) -> usize {
    let tiles = (width as f64 / 512.0).ceil() * (height as f64 / 512.0).ceil();
    let detail = (tiles * 288.0) as usize + 85;
    detail.max(256)
}

/// JPEG EXIF Orientation 归一化。失败（无 EXIF/解析错误）静默返回原图——
/// 方向错误可被用户发现并反馈，不构成拒绝附加的理由。
fn apply_exif_orientation(
    image: DynamicImage,
    source: &[u8],
    format: ImageWireFormat,
) -> DynamicImage {
    if format != ImageWireFormat::Jpeg {
        return image;
    }
    let Ok(exif_reader) = exif::Reader::new()
        .read_from_container(&mut std::io::Cursor::new(source)) else {
        return image;
    };
    let Some(orientation) = exif_reader
        .get_field(exif::Tag::Orientation, exif::In::PRIMARY)
    else {
        return image;
    };
    // Orientation 值 1-8：1/基础方向直接返回
    match orientation.value.get_uint(0).unwrap_or(1) {
        2 => image.fliph(),
        3 => image.rotate180(),
        4 => image.flipv(),
        5 => image.rotate90().fliph(),
        6 => image.rotate90(),
        7 => image.rotate270().fliph(),
        8 => image.rotate270(),
        _ => image,
    }
}

// ── 测试 ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use image::{DynamicImage, RgbImage};

    fn tiny_png(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([200, 30, 30])));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Png)
            .unwrap();
        out
    }

    fn tiny_jpeg(w: u32, h: u32) -> Vec<u8> {
        let img = DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, image::Rgb([30, 30, 200])));
        let mut out = Vec::new();
        img.write_to(&mut std::io::Cursor::new(&mut out), image::ImageFormat::Jpeg)
            .unwrap();
        out
    }

    #[test]
    fn sniff_detects_formats() {
        assert_eq!(sniff_format(&tiny_png(2, 2)), SniffedFormat::Png);
        assert_eq!(sniff_format(&tiny_jpeg(2, 2)), SniffedFormat::Jpeg);
        assert_eq!(sniff_format(b"GIF89a..."), SniffedFormat::Gif);
        assert_eq!(sniff_format(b"RIFFxxxxWEBPVP8 "), SniffedFormat::WebP);
        assert_eq!(sniff_format(b"BM\x00\x00"), SniffedFormat::Bmp);
        assert_eq!(sniff_format(b"<svg xmlns=\"...\">"), SniffedFormat::Svg);
        assert_eq!(sniff_format(b"\xEF\xBB\xBF<?xml ?><svg/>"), SniffedFormat::Svg);
        assert_eq!(sniff_format(&[1, 2, 3, 4]), SniffedFormat::Unknown);
    }

    #[test]
    fn validate_rejects_oversized_and_formats() {
        assert!(validate_encoded(&tiny_png(2, 2)).is_ok());
        assert!(validate_encoded(&[]).is_err());
        assert!(validate_encoded(b"GIF89a").is_err(), "GIF 拒绝");
        assert!(validate_encoded(b"<svg/>").is_err(), "SVG 拒绝");
        let big = vec![0x89, b'P', b'N', b'G'];
        let mut huge = big.clone();
        huge.resize(MAX_ENCODED_BYTES_PER_IMAGE as usize + 1, 0);
        assert!(validate_encoded(&huge).is_err(), "超 8MiB 拒绝");
    }

    #[test]
    fn normalize_small_png_passthrough_shape() {
        let png = tiny_png(64, 48);
        let normalized = normalize_image(png).unwrap();
        assert_eq!(normalized.format, ImageWireFormat::Png);
        assert_eq!(normalized.width, 64);
        assert_eq!(normalized.height, 48);
        assert!(normalized.bytes.starts_with(&[0x89, b'P', b'N', b'G']), "PNG 魔数保留");
        assert!(normalized.estimated_tokens >= 256);
    }

    #[test]
    fn normalize_resizes_long_edge() {
        let png = tiny_png(3000, 100);
        let normalized = normalize_image(png).unwrap();
        assert!(
            normalized.width.max(normalized.height) <= MAX_LONG_EDGE,
            "长边归一化到 ≤2048：{}×{}",
            normalized.width,
            normalized.height
        );
        // 等比近似（3000:100 → 2048:68±1）
        let ratio = normalized.width as f64 / normalized.height as f64;
        assert!((ratio - 30.0).abs() < 1.0, "保持宽高比: {ratio}");
    }

    #[test]
    fn normalize_rejects_pixel_bomb() {
        // 24MP 头部预检：6000×4001 = 24.006MP，在解码前（大分配前）被拒。
        // 纯色 PNG 编码后极小；构造期间的 72MB 像素缓冲只是测试进程临时内存。
        let bomb = tiny_png(6000, 4001);
        let result = normalize_image(bomb);
        assert!(result.is_err(), "超 24MP 的图应被拒绝");
        let msg = result.unwrap_err().to_string();
        assert!(msg.contains("24"), "错误信息应提示 24MP 上限: {msg}");
        // 正常小图不被误拦
        assert!(normalize_image(tiny_png(2, 2)).is_ok());
    }

    #[test]
    fn jpeg_keeps_jpeg_wire() {
        let jpeg = tiny_jpeg(32, 32);
        let normalized = normalize_image(jpeg).unwrap();
        assert_eq!(normalized.format, ImageWireFormat::Jpeg);
        assert_eq!(normalized.mime(), "image/jpeg");
        assert!(normalized.bytes.starts_with(&[0xFF, 0xD8]));
    }

    #[test]
    fn total_encoded_checks_sum() {
        let a = NormalizedImage {
            format: ImageWireFormat::Png,
            bytes: vec![0; 7 * 1024 * 1024],
            width: 1,
            height: 1,
            estimated_tokens: 256,
        };
        let b = NormalizedImage {
            format: ImageWireFormat::Png,
            bytes: vec![0; 7 * 1024 * 1024],
            width: 1,
            height: 1,
            estimated_tokens: 256,
        };
        // 14MiB × 4/3 ≈ 18.7MiB > 16MiB
        assert!(check_total_encoded(&[&a, &b]).is_err(), "合计按 base64 展开计算");
        assert!(check_total_encoded(&[&a]).is_ok());
    }

    #[test]
    fn token_estimate_floor_and_tiles() {
        // 单 tile：288 + 85 = 373（下限 256 不生效——公式下界已高于 256）
        assert_eq!(estimate_vision_tokens(1, 1), 373);
        let t = estimate_vision_tokens(2048, 2048);
        assert!(t >= 4 * 4 * 288, "2048² 至少 16 tile: {t}");
    }
}
