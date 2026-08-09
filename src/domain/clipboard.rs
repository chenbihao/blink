//! 剪贴板共享业务语义（0.19.6）。
//!
//! command、Capability 与截图输出只负责各自的协议适配；阻塞隔离、来源标记、
//! BGRA 校验和历史图片加载统一收敛在这里。平台层仍只提供同步 Win32 原语。

use sqlx::SqlitePool;

/// 剪贴板读取结果。图片优先，未读到图片时再读取文本。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipboardContent {
    ImagePng(Vec<u8>),
    Text(String),
}

/// 写入来源决定监听器记录的来源标签及是否跳过历史持久化。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClipboardWriteSource {
    /// 用户在 Blink 界面显式复制。
    User,
    /// AI Capability 写入。
    Capability,
    /// 截图输出；新截图应进入剪贴板历史。
    Screenshot,
    /// 历史图片回贴；避免同一图片删旧留新后覆盖原始来源。
    HistoryRepost,
}

impl ClipboardWriteSource {
    fn marker(self) -> (&'static str, bool) {
        use crate::infra::platform::clipboard::{
            SELF_LABEL_APP, SELF_LABEL_BLINK, SELF_LABEL_REPOST, SELF_LABEL_SCREENSHOT,
        };
        match self {
            Self::User => (SELF_LABEL_APP, false),
            Self::Capability => (SELF_LABEL_BLINK, false),
            Self::Screenshot => (SELF_LABEL_SCREENSHOT, false),
            Self::HistoryRepost => (SELF_LABEL_REPOST, true),
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ClipboardError {
    #[error("剪贴板任务失败: {detail}")]
    Task { detail: String },
    #[error("剪贴板操作失败: {detail}")]
    Platform { detail: String },
    #[error("图片不存在: {id}")]
    ImageNotFound { id: String },
    #[error("像素尺寸溢出: {width}x{height}x4")]
    PixelSizeOverflow { width: u32, height: u32 },
    #[error("像素长度不匹配: {actual} vs {expected} ({width}x{height}x4)")]
    PixelLengthMismatch {
        actual: usize,
        expected: usize,
        width: u32,
        height: u32,
    },
}

fn task_error(error: tokio::task::JoinError) -> ClipboardError {
    ClipboardError::Task {
        detail: error.to_string(),
    }
}

fn platform_error(detail: String) -> ClipboardError {
    ClipboardError::Platform { detail }
}

/// 读取当前剪贴板；图片优先，空剪贴板投影为空文本。
pub async fn read_current() -> Result<ClipboardContent, ClipboardError> {
    tokio::task::spawn_blocking(|| {
        if let Some(png) = crate::infra::platform::clipboard::read_current_image() {
            ClipboardContent::ImagePng(png)
        } else {
            ClipboardContent::Text(
                crate::infra::platform::clipboard::read_current_text().unwrap_or_default(),
            )
        }
    })
    .await
    .map_err(task_error)
}

/// 写文本到系统剪贴板。
pub async fn write_text(
    text: String,
    source: ClipboardWriteSource,
) -> Result<(), ClipboardError> {
    let (label, skip_persist) = source.marker();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_text_to_clipboard(
            &text,
            label,
            skip_persist,
        )
    })
    .await
    .map_err(task_error)?
    .map_err(platform_error)
}

/// 写 PNG 到系统剪贴板。
pub async fn write_png(
    png: Vec<u8>,
    source: ClipboardWriteSource,
) -> Result<(), ClipboardError> {
    let (label, skip_persist) = source.marker();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_png_to_clipboard(&png, label, skip_persist)
    })
    .await
    .map_err(task_error)?
    .map_err(platform_error)
}

/// 写 BGRA 像素到系统剪贴板；长度校验在进入阻塞平台调用前统一完成。
pub async fn write_bgra(
    pixels: Vec<u8>,
    width: u32,
    height: u32,
    source: ClipboardWriteSource,
) -> Result<(), ClipboardError> {
    validate_bgra_len(pixels.len(), width, height)?;
    let (label, skip_persist) = source.marker();
    tokio::task::spawn_blocking(move || {
        crate::infra::platform::clipboard::write_bgra_to_clipboard(
            &pixels,
            width,
            height,
            label,
            skip_persist,
        )
    })
    .await
    .map_err(task_error)?
    .map_err(platform_error)
}

/// 从剪贴板图片历史加载完整 PNG。command 与 Capability 共用同一缺失语义。
pub async fn load_history_png(pool: &SqlitePool, id: &str) -> Result<Vec<u8>, ClipboardError> {
    crate::infra::data::clipboard_images::get_png_by_id(pool, id)
        .await
        .ok_or_else(|| ClipboardError::ImageNotFound { id: id.to_string() })
}

fn validate_bgra_len(actual: usize, width: u32, height: u32) -> Result<(), ClipboardError> {
    let expected = (width as usize)
        .checked_mul(height as usize)
        .and_then(|size| size.checked_mul(4))
        .ok_or(ClipboardError::PixelSizeOverflow { width, height })?;
    if actual != expected {
        return Err(ClipboardError::PixelLengthMismatch {
            actual,
            expected,
            width,
            height,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_source_preserves_marker_and_persistence_policy() {
        assert_eq!(
            ClipboardWriteSource::User.marker(),
            (crate::infra::platform::clipboard::SELF_LABEL_APP, false)
        );
        assert_eq!(
            ClipboardWriteSource::Capability.marker(),
            (crate::infra::platform::clipboard::SELF_LABEL_BLINK, false)
        );
        assert_eq!(
            ClipboardWriteSource::Screenshot.marker(),
            (
                crate::infra::platform::clipboard::SELF_LABEL_SCREENSHOT,
                false
            )
        );
        assert_eq!(
            ClipboardWriteSource::HistoryRepost.marker(),
            (crate::infra::platform::clipboard::SELF_LABEL_REPOST, true)
        );
    }

    #[test]
    fn bgra_length_validation_is_checked_and_exact() {
        assert!(validate_bgra_len(16, 2, 2).is_ok());
        assert!(matches!(
            validate_bgra_len(15, 2, 2),
            Err(ClipboardError::PixelLengthMismatch {
                actual: 15,
                expected: 16,
                ..
            })
        ));
        assert!(matches!(
            validate_bgra_len(0, u32::MAX, u32::MAX),
            Err(ClipboardError::PixelSizeOverflow { .. })
        ));
    }
}
