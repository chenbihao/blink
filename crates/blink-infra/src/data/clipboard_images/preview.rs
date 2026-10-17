//! 悬浮预览从原图生成，列表缩略图保持原有轻量路径。
use sqlx::SqlitePool;
use std::io::Cursor;

const PREVIEW_EDGE: u32 = 1024;
const MAX_PNG_BYTES: i64 = 32 * 1024 * 1024;
const DECODE_BUDGET: u64 = 64 * 1024 * 1024;
static PREVIEW_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// 0.26.9：尝试进入共享图片解码额度（并发 2，忙即 None，不排队）。
///
/// 供文件预览/入库路径复用同一解码额度——文件内容读出后 try_acquire，
/// 繁忙则立即释放缓冲返回 Busy，不排队持有完整文件。
pub(crate) fn try_acquire_image_decode_slot(
) -> Option<tokio::sync::SemaphorePermit<'static>> {
    PREVIEW_WORKERS.try_acquire().ok()
}

pub async fn get_preview_by_id_global(id: &str) -> Option<Vec<u8>> {
    get_preview_by_id(super::POOL.get()?, id).await
}

async fn get_preview_by_id(pool: &SqlitePool, id: &str) -> Option<Vec<u8>> {
    // 先限并发再读取 BLOB，限制并发原图解码与临时内存。
    let _permit = PREVIEW_WORKERS.acquire().await.ok()?;
    let source = sqlx::query_scalar::<_, Vec<u8>>(
        "SELECT png_blob FROM clipboard_images WHERE id = ?1 AND LENGTH(png_blob) <= ?2",
    )
    .bind(id)
    .bind(MAX_PNG_BYTES)
    .fetch_optional(pool)
    .await
    .ok()??;
    match tokio::task::spawn_blocking(move || make_preview(&source)).await {
        Ok(Ok(png)) => Some(png),
        Ok(Err(error)) => {
            tracing::debug!(%error, "剪贴板悬浮图片无法生成预览");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "剪贴板悬浮图片预览任务失败");
            None
        }
    }
}

fn make_preview(source: &[u8]) -> image::ImageResult<Vec<u8>> {
    let mut reader = image::ImageReader::with_format(Cursor::new(source), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_BUDGET);
    reader.limits(limits);
    let image = reader.decode()?;
    // Triangle 滤波：截图里的文字在 thumbnail 的快速整数采样下会发糊，
    // 双线性缩放在同等预算下明显更清晰，代价只在生成路径（不在列表热路径）。
    let image = if image.width().max(image.height()) > PREVIEW_EDGE {
        image.resize(PREVIEW_EDGE, PREVIEW_EDGE, image::imageops::FilterType::Triangle)
    } else {
        image
    };
    let mut bytes = Cursor::new(Vec::new());
    image.write_to(&mut bytes, image::ImageFormat::Png)?;
    Ok(bytes.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn png(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(width, height, image::Rgba([20, 80, 160, 100]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        bytes.into_inner()
    }
    #[test]
    fn preview_preserves_aspect_alpha_and_small_images() {
        for (width, height, expected) in [
            (2048, 512, (1024, 256)),
            (512, 2048, (256, 1024)),
            (80, 40, (80, 40)),
        ] {
            let bytes = make_preview(&png(width, height)).unwrap();
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
            let image = image::load_from_memory(&bytes).unwrap().into_rgba8();
            assert_eq!(image.dimensions(), expected);
            assert_eq!(image.get_pixel(0, 0).0[3], 100);
        }
        assert!(make_preview(b"invalid PNG").is_err());
    }
    #[tokio::test]
    async fn preview_reads_original_and_missing_id_is_empty() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::init_db(&pool).await.unwrap();
        let source = png(1600, 400);
        sqlx::query("INSERT INTO clipboard_images(id, png_blob, thumb_blob, width, height, sha256, created_at) VALUES ('image', ?1, ?2, 1600, 400, 'hash', 1)")
            .bind(source).bind(png(256, 64)).execute(&pool).await.unwrap();
        let bytes = get_preview_by_id(&pool, "image").await.unwrap();
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 1024);
        assert!(get_preview_by_id(&pool, "missing").await.is_none());
    }
}
