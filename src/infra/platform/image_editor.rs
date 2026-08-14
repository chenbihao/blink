//! 用户侧图片编辑会话载荷。
//!
//! 与截图 `SESSION` 分离：这里只暂存用户显式打开编辑器的 PNG，生命周期止于
//! 当前编辑窗口。它不是 Capability ImageStash，也不跨调用或持久化。
//!
//! 0.20.4：多来源闭环——当前剪贴板、历史图片、pin 图统一进入同一编辑会话。
//! 资源限制按 phase 文档 §5.5 定义：
//! - compressed/input bytes ≤ 32 MiB
//! - width, height ≤ 16_384
//! - decoded RGBA bytes ≤ 256 MiB
//! - single active editor session（同一时刻只有一个编辑会话）

use std::sync::{Arc, RwLock};

/// 输入 PNG 压缩字节上限（§5.5）。
const MAX_INPUT_BYTES: usize = 32 * 1024 * 1024;

/// 单边像素上限（§5.5）。
const MAX_DIMENSION: u32 = 16_384;

/// 解码后 RGBA 字节上限（§5.5）。
const MAX_DECODED_BYTES: usize = 256 * 1024 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageEditorMeta {
    pub width: u32,
    pub height: u32,
}

static SESSION: RwLock<Option<Arc<Vec<u8>>>> = RwLock::new(None);

/// 查询编辑会话是否活跃（0.20.4 单会话约束）。
pub fn is_active() -> bool {
    SESSION.read().map(|g| g.is_some()).unwrap_or(false)
}

/// 尝试建立编辑会话。若已有活跃会话则返回 `AlreadyActive` 错误，不替换。
///
/// 资源限制校验：
/// - 输入字节 ≤ 32 MiB
/// - PNG 尺寸 width/height ≤ 16_384
/// - 解码后 RGBA bytes ≤ 256 MiB
pub fn begin_session(png_data: Vec<u8>) -> Result<ImageEditorMeta, String> {
    // 0.20.4：单会话约束——已有活跃会话时拒绝
    if is_active() {
        return Err("AlreadyActive".to_string());
    }

    let data_len = png_data.len();
    if png_data.is_empty() || data_len > MAX_INPUT_BYTES {
        return Err(format!(
            "图片编辑载荷大小无效: bytes={}, max={MAX_INPUT_BYTES}",
            data_len
        ));
    }

    let (width, height) = crate::infra::platform::screenshot::parse_png_size(&png_data)
        .ok_or_else(|| "图片编辑载荷不是有效 PNG".to_string())?;

    // 尺寸上限
    if width > MAX_DIMENSION || height > MAX_DIMENSION {
        return Err(format!(
            "图片尺寸超限: {width}x{height}, max={MAX_DIMENSION}x{MAX_DIMENSION}"
        ));
    }

    // 解码后 RGBA 字节预算
    let decoded_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|p| p.checked_mul(4));
    match decoded_bytes {
        Some(db) if db <= MAX_DECODED_BYTES => {}
        Some(db) => {
            return Err(format!(
                "图片解码预算超限: decoded_rgba_bytes={db}, max={MAX_DECODED_BYTES}"
            ));
        }
        None => {
            return Err(format!(
                "图片解码预算溢出: {width}x{height}"
            ));
        }
    }

    *SESSION
        .write()
        .map_err(|e| format!("图片编辑 SESSION 写锁失败: {e}"))? = Some(Arc::new(png_data));
    tracing::debug!(width, height, data_len, "用户图片编辑 SESSION 已建立");
    Ok(ImageEditorMeta { width, height })
}

pub fn session_png() -> Option<Arc<Vec<u8>>> {
    SESSION.read().ok()?.as_ref().map(Arc::clone)
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
    use std::sync::Mutex;

    /// 测试互斥锁——所有测试操作全局 SESSION，必须串行执行。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    const ONE_PIXEL_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137,
    ];

    #[test]
    fn editor_session_is_independent_and_bounded() {
        let _guard = TEST_LOCK.lock().unwrap();
        end_session();
        let meta = begin_session(ONE_PIXEL_PNG.to_vec()).unwrap();
        assert_eq!(
            meta,
            ImageEditorMeta {
                width: 1,
                height: 1
            }
        );
        assert_eq!(session_png().as_deref().map(|v| v.as_slice()), Some(ONE_PIXEL_PNG));
        end_session();
        assert!(session_png().is_none());
        assert!(begin_session(Vec::new()).is_err());
        assert!(begin_session(vec![0; 64]).is_err());
    }

    #[test]
    fn already_active_rejects_second_session() {
        let _guard = TEST_LOCK.lock().unwrap();
        end_session();
        assert!(begin_session(ONE_PIXEL_PNG.to_vec()).is_ok());
        assert!(is_active());
        // 第二次 begin 应返回 AlreadyActive
        let err = begin_session(ONE_PIXEL_PNG.to_vec()).unwrap_err();
        assert_eq!(err, "AlreadyActive");
        end_session();
        assert!(!is_active());
    }

    #[test]
    fn size_limit_rejects_oversized_payload() {
        let _guard = TEST_LOCK.lock().unwrap();
        end_session();
        // 33 MiB 超过 32 MiB 输入上限
        let oversized = vec![0u8; MAX_INPUT_BYTES + 1];
        let err = begin_session(oversized).unwrap_err();
        assert!(err.contains("大小无效"));
        end_session();
    }

    #[test]
    fn end_session_is_idempotent() {
        let _guard = TEST_LOCK.lock().unwrap();
        end_session();
        end_session();
        assert!(!is_active());
    }
}
