//! 图片输入解析 helper（0.19.4 §3.6）。
//!
//! `pin_image`、`ocr_image` 和 `write_clipboard` 图片模式的公共解析逻辑收敛于此，
//! 避免三个消费者各自实现 `image_ref` / 原始字节二选一校验。
//!
//! **规则**（§3.9）：
//! - `image_ref` 与原始字节二选一；同时提供或都不提供均返回 `InvalidArgs`
//! - `image_ref` 从 `ImageStash` 解析，检查 MIME 是否为 `image/*`
//! - 无 stash 运行时用 `image_ref` → `InvalidArgs`（明确报错，不静默降级）

use serde_json::Value;

use crate::domain::capability::{CapabilityError, ImageStash};

/// 解析 PNG 图片输入：`image_ref` 或 `bytes_key` 二选一。
///
/// 供 `pin_image`（`bytes_key="png"`）和 `ocr_image`（`bytes_key="png"`）使用。
///
/// **返回**：解析成功返回 PNG 字节；失败返回 `InvalidArgs`。
pub fn resolve_png_input(
    args: &Value,
    stash: Option<&ImageStash>,
    bytes_key: &str,
) -> Result<Vec<u8>, CapabilityError> {
    let has_ref = args.get("image_ref").is_some();
    let has_bytes = args.get(bytes_key).is_some();

    if has_ref && has_bytes {
        return Err(CapabilityError::InvalidArgs {
            detail: format!("image_ref 和 {bytes_key} 不能同时提供，请二选一"),
        });
    }
    if !has_ref && !has_bytes {
        return Err(CapabilityError::InvalidArgs {
            detail: format!("必须提供 image_ref 或 {bytes_key} 之一"),
        });
    }

    if let Some(ref_val) = args.get("image_ref").and_then(Value::as_str) {
        // 从 stash 解析
        let stash = stash.ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "image_ref 不可用（运行时未启用 ImageStash）".into(),
        })?;
        let img = stash.get(ref_val).ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "image_ref 不存在或已过期".into(),
        })?;
        // MIME 校验
        if !img.mime.starts_with("image/") {
            return Err(CapabilityError::InvalidArgs {
                detail: format!("image_ref 指向的不是图片（mime: {}）", img.mime),
            });
        }
        Ok(img.bytes)
    } else {
        // 从原始字节解析
        let bytes = args
            .get(bytes_key)
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: format!("{bytes_key} 参数格式无效（应为整数数组）"),
            })?
            .iter()
            .filter_map(|v| v.as_u64().map(|n| n as u8))
            .collect::<Vec<u8>>();

        if bytes.is_empty() {
            return Err(CapabilityError::InvalidArgs {
                detail: format!("{bytes_key} 数据为空"),
            });
        }
        Ok(bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_png_from_bytes() {
        let args = json!({ "png": [0x89, 0x50, 0x4E, 0x47] });
        let result = resolve_png_input(&args, None, "png").unwrap();
        assert_eq!(result, vec![0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn resolve_png_from_ref() {
        let stash = ImageStash::new();
        let token = stash.put(vec![1, 2, 3, 4], "image/png".into()).unwrap();
        let args = json!({ "image_ref": token });
        let result = resolve_png_input(&args, Some(&stash), "png").unwrap();
        assert_eq!(result, vec![1, 2, 3, 4]);
    }

    #[test]
    fn resolve_both_ref_and_bytes_is_error() {
        let stash = ImageStash::new();
        let token = stash.put(vec![1], "image/png".into()).unwrap();
        let args = json!({ "image_ref": token, "png": [0x89] });
        let err = resolve_png_input(&args, Some(&stash), "png").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_neither_is_error() {
        let args = json!({});
        let err = resolve_png_input(&args, None, "png").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_ref_without_stash_is_error() {
        let args = json!({ "image_ref": "some_token" });
        let err = resolve_png_input(&args, None, "png").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_unknown_ref_is_error() {
        let stash = ImageStash::new();
        let args = json!({ "image_ref": "nonexistent" });
        let err = resolve_png_input(&args, Some(&stash), "png").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_empty_bytes_is_error() {
        let args = json!({ "png": [] });
        let err = resolve_png_input(&args, None, "png").unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }
}
