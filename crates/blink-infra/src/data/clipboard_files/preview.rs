//! 文件条目内容预览/入库服务（0.26.9 §5.10 悬浮预览与图片手动入库）。
//!
//! **额度模型**：
//! - 文件 I/O（打开、属性探测、magic 嗅探、读取）走独立有界通道
//!   （`FILE_IO_WORKERS`，并发 2，**无等待队列**：`try_acquire` 忙即返回 Busy），
//!   与 DB 图片预览的读取额度（`clipboard_images::preview` 的 PREVIEW_WORKERS）
//!   隔离，慢磁盘不占满原图预览入口。
//! - 内容读出后**尝试进入共享图片解码额度**（并发 2，try_acquire，繁忙立即
//!   释放缓冲返回 Busy，不排队持有完整文件）。
//! - 等待结果上限 1 秒：超时只表示本次停止等待，工作额度随实际任务结束才
//!   释放（permit 移入 blocking 闭包），不把超时当底层 I/O 已取消。
//!
//! **解码边界**：原文件最多 32MiB（读取用上限+1 再核验，防属性检查后变大）、
//! 64MiB 解码预算、最长边 1024px、Triangle 滤波、小图不放大；magic 检测结果
//! 必须在当前构建可解码格式白名单内（image crate 仅启用 png feature → PNG）。
//! 每次读取反映当前文件内容，不按条目 id 永久缓存。

use sqlx::SqlitePool;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::time::Duration;

use crate::platform::fs_guard::{DenyReason, open_content_file};

/// 预览最长边（px），与 DB 图片预览一致。
const PREVIEW_EDGE: u32 = 1024;
/// 原文件大小上限（32MiB）。
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
/// 解码分配预算（64MiB）。
const DECODE_BUDGET: u64 = 64 * 1024 * 1024;
/// 等待文件 I/O 结果的上限；超时停止等待（额度随任务结束释放）。
const IO_WAIT_TIMEOUT: Duration = Duration::from_secs(1);
/// 文件 I/O 独立通道：最多 2 个实际在途任务，无等待队列。
static FILE_IO_WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(2);

/// PNG magic（当前构建唯一可解码格式——image crate 仅启用 png feature）。
const PNG_MAGIC: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

/// 文件内容读取结果分类。
#[derive(Debug)]
pub enum FilePreviewError {
    /// 条目不存在（或非单路径条目——多文件不读内容）。
    EntryMissing,
    /// 文件 I/O 通道忙（立即返回，不排队）。
    Busy,
    /// 等待超时（本次停止等待，不代表底层 I/O 取消）。
    Timeout,
    /// 受限路径：仅保存路径，未读取内容。
    Denied(DenyReason),
    /// 非 PNG（magic 白名单外）。
    NotImage,
    /// 文件超过 32MiB 或读取中变大。
    TooLarge,
    /// 读取/解码失败（损坏、超大尺寸等）。
    Decode(String),
}

impl FilePreviewError {
    /// 稳定原因码（前端 i18n 映射；不含路径原文）。
    pub fn reason(&self) -> &'static str {
        match self {
            FilePreviewError::EntryMissing => "missing",
            FilePreviewError::Busy => "busy",
            FilePreviewError::Timeout => "timeout",
            FilePreviewError::Denied(reason) => reason.reason(),
            FilePreviewError::NotImage => "not_image",
            FilePreviewError::TooLarge => "too_large",
            FilePreviewError::Decode(_) => "decode_failed",
        }
    }
}

/// 解码后的文件图片（预览 PNG 与入库共用同一读取/解码服务）。
pub struct DecodedFileImage {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub thumbnail_png: Option<Vec<u8>>,
}

/// 按条目 id 读取并解码单路径文件为预览 PNG（`/file-preview/<id>` 协议消费）。
///
/// 查库确认恰好单路径；多文件条目返回 `EntryMissing`（前端展示路径列表，
/// 不逐个读取文件）。请求不携带自由路径。
pub async fn get_file_preview_png_by_id(
    pool: &SqlitePool,
    id: &str,
) -> Result<Vec<u8>, FilePreviewError> {
    let decoded = read_file_image(pool, id, true).await?;
    Ok(decoded.png)
}

/// 按条目 id 读取文件为解码图片（预览与「存为图片条目」共用）。
///
/// 全流程：查库（恰好单路径）→ 受限路径防御 → 有界文件 I/O 通道读取
/// （上限+1 核验防属性检查后变大）→ magic 白名单 → 共享解码额度内原图/缩略图编码。
pub async fn read_file_image_by_id(
    pool: &SqlitePool,
    id: &str,
) -> Result<DecodedFileImage, FilePreviewError> {
    read_file_image(pool, id, false).await
}

async fn read_file_image(
    pool: &SqlitePool,
    id: &str,
    preview: bool,
) -> Result<DecodedFileImage, FilePreviewError> {
    let item = super::get_paths_by_id(pool, id)
        .await
        .ok_or(FilePreviewError::EntryMissing)?;
    let [single] = item.paths.as_slice() else {
        // 多文件条目不读取内容（只展示 DB 路径列表）
        return Err(FilePreviewError::EntryMissing);
    };
    let path = single.to_string();

    // 属性、链接目标、打开句柄与内容读取全部在同一个限流/超时边界内。
    let bytes = file_io(&FILE_IO_WORKERS, IO_WAIT_TIMEOUT, move || {
        read_bounded(&path)
    })
    .await?;
    if bytes.is_empty() || bytes.get(..8) != Some(PNG_MAGIC.as_slice()) {
        return Err(FilePreviewError::NotImage);
    }

    // 共享图片解码额度（与 DB 图片预览同池）：try_acquire，繁忙立即放弃缓冲
    let decode_permit = crate::data::clipboard_images::try_acquire_image_decode_slot()
        .ok_or(FilePreviewError::Busy)?;
    let decode_task = tokio::task::spawn_blocking(move || {
        let _hold = decode_permit;
        decode_file_png(&bytes, preview)
    });
    match decode_task.await {
        Ok(Ok(decoded)) => Ok(decoded),
        Ok(Err(error)) => {
            tracing::debug!(%error, "文件图片解码失败");
            Err(FilePreviewError::Decode(error))
        }
        Err(error) => {
            tracing::warn!(%error, "文件图片解码任务失败");
            Err(FilePreviewError::Decode("task failed".into()))
        }
    }
}

async fn file_io<T: Send + 'static>(
    workers: &'static tokio::sync::Semaphore,
    timeout: Duration,
    operation: impl FnOnce() -> Result<T, FilePreviewError> + Send + 'static,
) -> Result<T, FilePreviewError> {
    let permit = workers.try_acquire().map_err(|_| FilePreviewError::Busy)?;
    let task = tokio::task::spawn_blocking(move || {
        let _hold = permit;
        operation()
    });
    match tokio::time::timeout(timeout, task).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            tracing::warn!(%error, "文件 I/O 任务失败");
            Err(FilePreviewError::Decode("task failed".into()))
        }
        Err(_) => {
            tracing::debug!("文件 I/O 等待超时,本次停止等待");
            Err(FilePreviewError::Timeout)
        }
    }
}

/// 有界读取：上限+1 核验（属性检查后文件变大则拒绝），失败分类不带路径原文。
fn read_bounded(path: &str) -> Result<Vec<u8>, FilePreviewError> {
    let file = open_content_file(path).map_err(FilePreviewError::Denied)?;
    let metadata = file
        .metadata()
        .map_err(|_| FilePreviewError::Denied(DenyReason::Missing))?;
    let len = metadata.len();
    if len > MAX_SOURCE_BYTES {
        return Err(FilePreviewError::TooLarge);
    }
    read_limited(file, MAX_SOURCE_BYTES)
}

fn read_limited(reader: impl Read, limit: u64) -> Result<Vec<u8>, FilePreviewError> {
    let mut bytes = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut bytes)
        .map_err(|_| FilePreviewError::Decode("read failed".into()))?;
    if bytes.len() as u64 > limit {
        return Err(FilePreviewError::TooLarge);
    }
    Ok(bytes)
}

/// 原图解码/编码共用边界，仅预览缩放至 1024px；入库保持原尺寸。
///
/// 入口已做 magic 白名单校验（仅 PNG），此处固定按 PNG 解码。
fn decode_file_png(bytes: &[u8], preview: bool) -> Result<DecodedFileImage, String> {
    let mut reader = image::ImageReader::with_format(Cursor::new(bytes), image::ImageFormat::Png);
    reader.limits({
        let mut limits = image::Limits::default();
        limits.max_alloc = Some(DECODE_BUDGET);
        limits
    });
    let decoded = reader.decode().map_err(|e| format!("decode failed: {e}"))?;
    let (width, height) = (decoded.width(), decoded.height());
    let image = if preview && width.max(height) > PREVIEW_EDGE {
        decoded.resize(
            PREVIEW_EDGE,
            PREVIEW_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        decoded
    };
    let (out_w, out_h) = (image.width(), image.height());
    let thumbnail_png = if preview {
        None
    } else {
        Some(encode_thumbnail_png(&image)?)
    };
    let png = encode_full_png(image)?;
    Ok(DecodedFileImage {
        png,
        width: out_w,
        height: out_h,
        thumbnail_png,
    })
}

/// 探测条目的内容读取可用性（前端 img 加载失败后调 status 查询消费）。
///
/// 只做查库 + 属性探测（不读内容、不解码），探测同样走文件 I/O 通道。
pub async fn probe_file_entry_status(pool: &SqlitePool, id: &str) -> &'static str {
    let item = match super::get_paths_by_id(pool, id).await {
        Some(item) => item,
        None => return FilePreviewError::EntryMissing.reason(),
    };
    let [single] = item.paths.as_slice() else {
        return "multi";
    };
    let path = single.clone();
    match file_io(&FILE_IO_WORKERS, IO_WAIT_TIMEOUT, move || {
        open_content_file(&path)
            .map(|_| ())
            .map_err(FilePreviewError::Denied)
    })
    .await
    {
        Ok(()) => "ok",
        Err(error) => error.reason(),
    }
}

/// 入库用的完整 PNG 编码（不缩放；输出上限 32MiB，不以解码预算冒充输出上限）。
///
/// 入库保留解码原图尺寸；只有预览路径应用 1024px 缩放。
pub fn encode_full_png(image: image::DynamicImage) -> Result<Vec<u8>, String> {
    let mut out = BoundedPngWriter {
        inner: Cursor::new(Vec::new()),
        limit: MAX_SOURCE_BYTES,
    };
    image
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("encode failed: {e}"))?;
    Ok(out.inner.into_inner())
}

struct BoundedPngWriter {
    inner: Cursor<Vec<u8>>,
    limit: u64,
}

impl Write for BoundedPngWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if self.inner.position().saturating_add(bytes.len() as u64) > self.limit {
            return Err(std::io::Error::other("output png too large"));
        }
        self.inner.write(bytes)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

impl Seek for BoundedPngWriter {
    fn seek(&mut self, position: SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(position)
    }
}

/// 入库缩略图（最长边 256px，Triangle；小图不放大）。
pub fn encode_thumbnail_png(image: &image::DynamicImage) -> Result<Vec<u8>, String> {
    const THUMB_EDGE: u32 = 256;
    let thumb = if image.width().max(image.height()) > THUMB_EDGE {
        image.resize(
            THUMB_EDGE,
            THUMB_EDGE,
            image::imageops::FilterType::Triangle,
        )
    } else {
        image.clone()
    };
    let mut out = Cursor::new(Vec::new());
    thumb
        .write_to(&mut out, image::ImageFormat::Png)
        .map_err(|e| format!("encode failed: {e}"))?;
    Ok(out.into_inner())
}

/// 从 PNG 字节生成入库缩略图（domain 层不依赖 image crate，经此入口）。
pub fn make_thumbnail_from_png_bytes(png: &[u8]) -> Result<Vec<u8>, String> {
    let image = image::load_from_memory(png).map_err(|e| format!("reload failed: {e}"))?;
    encode_thumbnail_png(&image)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_preview_png_resizes_and_keeps_small() {
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2048, 512, image::Rgba([10, 90, 160, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let bytes = bytes.into_inner();
        let decoded = decode_file_png(&bytes, true).unwrap();
        assert_eq!((decoded.width, decoded.height), (1024, 256));
        assert_eq!(&decoded.png[..8], PNG_MAGIC.as_slice());
        let imported = decode_file_png(&bytes, false).unwrap();
        assert_eq!((imported.width, imported.height), (2048, 512));
        let png = image::load_from_memory(&imported.png).unwrap();
        assert_eq!(png.to_rgba8().get_pixel(1500, 400).0, [10, 90, 160, 255]);
        let thumb = image::load_from_memory(imported.thumbnail_png.as_ref().unwrap()).unwrap();
        assert_eq!((thumb.width(), thumb.height()), (256, 64));
    }

    #[test]
    fn decode_preview_png_rejects_non_png_magic_early() {
        // magic 白名单在 read_file_image_by_id 内校验；此处验证解码失败路径
        assert!(decode_file_png(b"not a png at all", true).is_err());
    }

    #[test]
    fn error_reasons_are_stable_codes() {
        assert_eq!(FilePreviewError::Busy.reason(), "busy");
        assert_eq!(FilePreviewError::Timeout.reason(), "timeout");
        assert_eq!(FilePreviewError::NotImage.reason(), "not_image");
        assert_eq!(FilePreviewError::TooLarge.reason(), "too_large");
        assert_eq!(FilePreviewError::Denied(DenyReason::Unc).reason(), "unc");
    }

    #[tokio::test]
    async fn missing_entry_and_multi_file_do_not_touch_fs() {
        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::init_db(&pool).await.unwrap();
        assert!(matches!(
            read_file_image_by_id(&pool, "missing").await,
            Err(FilePreviewError::EntryMissing)
        ));
        sqlx::query(
            "INSERT INTO clipboard_files (id, paths_json, dedup_hash, file_count, first_name, created_at, source_app, payload_bytes)
             VALUES ('multi', ?1, 'h', 2, 'a', 1, NULL, 10)",
        )
        .bind(r#"["C:\\a.png","C:\\b.png"]"#)
        .execute(&pool)
        .await
        .unwrap();
        assert!(matches!(
            read_file_image_by_id(&pool, "multi").await,
            Err(FilePreviewError::EntryMissing)
        ));
        assert_eq!(probe_file_entry_status(&pool, "multi").await, "multi");
        assert_eq!(probe_file_entry_status(&pool, "missing").await, "missing");
    }

    /// 真实临时 PNG 文件的预览闭环（受限路径防御允许本地盘符）。
    #[tokio::test]
    async fn local_png_file_previews_end_to_end() {
        let dir = std::env::temp_dir().join(format!(
            "blink-preview-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sample.png");
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(2048, 512, image::Rgba([1, 2, 3, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        std::fs::write(&path, bytes.into_inner()).unwrap();
        let path_str = path.to_string_lossy().into_owned();

        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::init_db(&pool).await.unwrap();
        super::super::save_file(
            &pool,
            &super::super::ClipboardFileItem {
                id: "f1".into(),
                paths: vec![path_str.clone()],
                created_at: 1,
                source_app: None,
            },
        )
        .await
        .unwrap();

        let decoded = read_file_image_by_id(&pool, "f1").await.unwrap();
        assert_eq!((decoded.width, decoded.height), (2048, 512));
        let preview = get_file_preview_png_by_id(&pool, "f1").await.unwrap();
        let preview = image::load_from_memory(&preview).unwrap();
        assert_eq!((preview.width(), preview.height()), (1024, 256));
        assert_eq!(probe_file_entry_status(&pool, "f1").await, "ok");

        // 替换为非 PNG 内容 → NotImage（每次读取反映当前文件内容，不缓存）
        std::fs::write(&path, b"plain text bytes").unwrap();
        assert!(matches!(
            read_file_image_by_id(&pool, "f1").await,
            Err(FilePreviewError::NotImage)
        ));
        // 删除文件 → Missing
        let _ = std::fs::remove_file(&path);
        assert!(matches!(
            read_file_image_by_id(&pool, "f1").await,
            Err(FilePreviewError::Denied(DenyReason::Missing))
        ));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 文件 I/O 通道并发上限 2、忙即返回（无排队）：用人为慢读取验证。
    #[tokio::test]
    async fn file_io_channel_rejects_third_concurrent_read_as_busy() {
        let dir = std::env::temp_dir().join(format!(
            "blink-io-test-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let mut bytes = Cursor::new(Vec::new());
        image::RgbaImage::from_pixel(64, 64, image::Rgba([9, 9, 9, 255]))
            .write_to(&mut bytes, image::ImageFormat::Png)
            .unwrap();
        let payload = bytes.into_inner();

        let pool = SqlitePool::connect("sqlite::memory:").await.unwrap();
        super::super::init_db(&pool).await.unwrap();
        let mut ids = Vec::new();
        for i in 0..3 {
            let path = dir.join(format!("f{i}.png"));
            std::fs::write(&path, &payload).unwrap();
            let id = format!("id{i}");
            super::super::save_file(
                &pool,
                &super::super::ClipboardFileItem {
                    id: id.clone(),
                    paths: vec![path.to_string_lossy().into_owned()],
                    created_at: i as i64,
                    source_app: None,
                },
            )
            .await
            .unwrap();
            ids.push(id);
        }

        // 先占满文件 I/O 通道（2 个 permit），第三个读取应立即 Busy 而非排队
        let _p1 = FILE_IO_WORKERS.try_acquire().unwrap();
        let _p2 = FILE_IO_WORKERS.try_acquire().unwrap();
        assert!(matches!(
            read_file_image_by_id(&pool, &ids[0]).await,
            Err(FilePreviewError::Busy)
        ));
        drop(_p1);
        drop(_p2);
        match read_file_image_by_id(&pool, &ids[0]).await {
            Ok(_) => {}
            // 共享解码额度是全局 static，可能被并行用例短暂占用——IO 通道
            // 已释放、读取确实进行了即可（非 IO Busy 路径已由上面断言覆盖）
            Err(FilePreviewError::Busy) => {}
            Err(other) => panic!("IO 通道释放后读取不应失败: {other:?}"),
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn actual_read_stops_at_limit_plus_one_even_if_source_grows() {
        let mut source = Cursor::new(vec![0; 128]);
        assert!(matches!(
            read_limited(&mut source, 16),
            Err(FilePreviewError::TooLarge)
        ));
        assert_eq!(source.position(), 17, "不读取超过探测字节");
        assert_eq!(
            read_limited(Cursor::new(vec![0; 16]), 16).unwrap().len(),
            16
        );
    }

    #[test]
    fn png_output_limit_is_checked_before_buffer_growth() {
        let mut writer = BoundedPngWriter {
            inner: Cursor::new(Vec::new()),
            limit: 16,
        };
        writer.write_all(&[0; 16]).unwrap();
        assert!(writer.write_all(&[1]).is_err());
        assert_eq!(writer.inner.get_ref().len(), 16);
        writer.seek(SeekFrom::Start(1_000)).unwrap();
        assert!(writer.write_all(&[1]).is_err());
        assert_eq!(writer.inner.get_ref().len(), 16);
    }

    #[tokio::test]
    async fn timeout_keeps_io_slot_until_blocking_probe_finishes() {
        static WORKERS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);
        let entered = std::sync::Arc::new(tokio::sync::Notify::new());
        let worker_entered = entered.clone();
        let (release, wait) = std::sync::mpsc::channel();
        let task = tokio::spawn(file_io(&WORKERS, Duration::from_millis(20), move || {
            worker_entered.notify_one();
            wait.recv_timeout(Duration::from_secs(2)).unwrap();
            Ok(())
        }));
        tokio::time::timeout(Duration::from_secs(1), entered.notified())
            .await
            .unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(1), task)
                .await
                .unwrap()
                .unwrap(),
            Err(FilePreviewError::Timeout)
        ));
        assert_eq!(
            WORKERS.available_permits(),
            0,
            "停止等待没有释放实际 I/O 额度"
        );
        assert!(matches!(
            file_io(&WORKERS, Duration::from_secs(1), || Ok(())).await,
            Err(FilePreviewError::Busy)
        ));
        release.send(()).unwrap();
        let permit = tokio::time::timeout(Duration::from_secs(1), WORKERS.acquire())
            .await
            .unwrap()
            .unwrap();
        drop(permit);
    }
}
