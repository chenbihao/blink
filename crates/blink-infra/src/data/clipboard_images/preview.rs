//! 悬浮预览校验后直出原始 PNG，列表缩略图保持原有轻量路径。
use sqlx::SqlitePool;
#[cfg(test)]
use std::io::Cursor;

const MAX_PNG_BYTES: i64 = 32 * 1024 * 1024;
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
    // 先限并发再读取 BLOB，限制原始字节与元数据解析的临时内存。
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
    let (width, height) =
        crate::utils::image_metadata::preview_dimensions(&source, image::ImageFormat::Png)?;
    tracing::trace!(
        bytes = source.len(),
        width,
        height,
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
    fn preview_accepts_compressed_png_above_old_pixel_memory_gates() {
        let original = png(8192, 4097); // RGBA 超过旧 64MiB/128MiB 门槛。
        assert!(original.len() < 4 * 1024 * 1024);
        assert_eq!(prepare_preview(original.clone()).unwrap(), original);
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
