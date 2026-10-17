//! 悬浮预览校验后直出原始 PNG，列表缩略图保持原有轻量路径。
use sqlx::SqlitePool;
use std::io::Cursor;

const MAX_PNG_BYTES: i64 = 32 * 1024 * 1024;
const DECODE_BUDGET: u64 = 64 * 1024 * 1024;
static PREVIEW_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// 0.26.9：尝试进入共享图片解码额度（并发 2，忙即 None，不排队）。
///
/// 供文件预览/入库路径复用同一解码额度——文件内容读出后 try_acquire，
/// 繁忙则立即释放缓冲返回 Busy，不排队持有完整文件。
pub(crate) fn try_acquire_image_decode_slot() -> Option<tokio::sync::SemaphorePermit<'static>> {
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
    match tokio::task::spawn_blocking(move || prepare_preview(source)).await {
        Ok(Ok(png)) => Some(png),
        Ok(Err(error)) => {
            tracing::debug!(%error, "剪贴板悬浮图片无法通过校验");
            None
        }
        Err(error) => {
            tracing::warn!(%error, "剪贴板悬浮图片预览任务失败");
            None
        }
    }
}

fn prepare_preview(source: Vec<u8>) -> image::ImageResult<Vec<u8>> {
    let started = std::time::Instant::now();
    let mut reader = image::ImageReader::with_format(Cursor::new(&source), image::ImageFormat::Png);
    let mut limits = image::Limits::default();
    limits.max_alloc = Some(DECODE_BUDGET);
    reader.limits(limits);
    let image = reader.decode()?;
    // 解码只用于校验损坏数据与像素内存预算；显示复用原始字节，避免二次采样/编码。
    tracing::trace!(
        bytes = source.len(),
        width = image.width(),
        height = image.height(),
        elapsed_ms = started.elapsed().as_millis() as u64,
        "剪贴板图片预览校验完成"
    );
    Ok(source)
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
    fn preview_preserves_original_png_dimensions_and_alpha() {
        for (width, height) in [(2048, 512), (512, 2048), (80, 40)] {
            let original = png(width, height);
            let bytes = prepare_preview(original.clone()).unwrap();
            assert_eq!(bytes, original, "悬浮预览必须逐字节保留原 PNG");
            assert_eq!(&bytes[..8], b"\x89PNG\r\n\x1a\n");
            let image = image::load_from_memory(&bytes).unwrap().into_rgba8();
            assert_eq!(image.dimensions(), (width, height));
            assert_eq!(image.get_pixel(0, 0).0[3], 100);
        }
        assert!(prepare_preview(b"invalid PNG".to_vec()).is_err());
    }

    #[test]
    fn preview_rejects_images_over_decode_budget() {
        // 压缩后很小的纯色 PNG 也必须遵守解码后的像素内存上限。
        assert!(matches!(
            prepare_preview(png(8192, 2049)),
            Err(image::ImageError::Limits(_))
        ));
    }
    #[tokio::test]
    async fn preview_reads_original_and_missing_id_is_empty() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::init_db(&pool).await.unwrap();
        let source = png(1600, 400);
        sqlx::query("INSERT INTO clipboard_images(id, png_blob, thumb_blob, width, height, sha256, created_at) VALUES ('image', ?1, ?2, 1600, 400, 'hash', 1)")
            .bind(&source).bind(png(256, 64)).execute(&pool).await.unwrap();
        let bytes = get_preview_by_id(&pool, "image").await.unwrap();
        assert_eq!(bytes, source, "从数据库读取原图而非列表缩略图");
        assert_eq!(image::load_from_memory(&bytes).unwrap().width(), 1600);
        assert!(get_preview_by_id(&pool, "missing").await.is_none());
    }
}
