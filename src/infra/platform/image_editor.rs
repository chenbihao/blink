//! 用户侧图片编辑会话载荷。
//!
//! 与截图 `SESSION` 分离：这里只暂存用户显式打开编辑器的 PNG，生命周期止于
//! 当前编辑窗口。它不是 Capability ImageStash，也不跨调用或持久化。

use std::sync::RwLock;

const MAX_EDITOR_IMAGE_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageEditorMeta {
    pub width: u32,
    pub height: u32,
}

static SESSION: RwLock<Option<Vec<u8>>> = RwLock::new(None);

pub fn begin_session(png_data: Vec<u8>) -> Result<ImageEditorMeta, String> {
    if png_data.is_empty() || png_data.len() > MAX_EDITOR_IMAGE_BYTES {
        return Err(format!(
            "图片编辑载荷大小无效: bytes={}, max={MAX_EDITOR_IMAGE_BYTES}",
            png_data.len()
        ));
    }
    let (width, height) = crate::infra::platform::screenshot::parse_png_size(&png_data)
        .ok_or_else(|| "图片编辑载荷不是有效 PNG".to_string())?;
    *SESSION
        .write()
        .map_err(|e| format!("图片编辑 SESSION 写锁失败: {e}"))? = Some(png_data);
    tracing::debug!(width, height, "用户图片编辑 SESSION 已建立");
    Ok(ImageEditorMeta { width, height })
}

pub fn session_png() -> Option<Vec<u8>> {
    SESSION.read().ok()?.clone()
}

pub fn end_session() {
    if let Ok(mut session) = SESSION.write() {
        if session.is_some() {
            tracing::debug!("用户图片编辑 SESSION 已清空");
        }
        *session = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ONE_PIXEL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137,
    ];

    #[test]
    fn editor_session_is_independent_and_bounded() {
        end_session();
        let meta = begin_session(ONE_PIXEL_PNG.to_vec()).unwrap();
        assert_eq!(
            meta,
            ImageEditorMeta {
                width: 1,
                height: 1
            }
        );
        assert_eq!(session_png().as_deref(), Some(ONE_PIXEL_PNG));
        end_session();
        assert!(session_png().is_none());
        assert!(begin_session(Vec::new()).is_err());
        assert!(begin_session(vec![0; 64]).is_err());
    }
}
