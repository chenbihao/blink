//! 图片输入解析 helper（0.19.4 §3.6；0.23.12 迁移到统一 ResourceStore）。
//!
//! `pin_image`、`ocr_image` 和 `write_clipboard` 图片模式的公共解析逻辑收敛于此，
//! 避免三个消费者各自实现 `image_ref` / 原始字节二选一校验。
//!
//! **规则**（§3.9 + 0.23.12 use 授权）：
//! - `image_ref` 与原始字节二选一；同时提供或都不提供均返回 `InvalidArgs`
//! - `image_ref` 从统一 ResourceStore 按**消费方声明的 use** open
//!   （ocr_image→OcrImage / pin_image→PinImage / write_clipboard·palette→DecodeImage），
//!   并校验 MIME 是否为 `image/*`
//! - 无 store 运行时用 `image_ref` → `InvalidArgs`（明确报错，不静默降级）

use bytes::Bytes;
use serde_json::Value;

use crate::domain::capability::CapabilityError;
use crate::domain::resource::{DefaultResourceStore, ResourceError, ResourceRef, ResourceUse};

/// 解析 PNG 图片输入：`image_ref` 或 `bytes_key` 二选一。
///
/// 供 `pin_image`（`bytes_key="png"`）和 `ocr_image`（`bytes_key="png"`）使用。
///
/// 返回 `Bytes`（Arc-backed），`clone()` 零字节复制——从 store 读取时
/// 只增加 Arc 引用计数。失败返回 `InvalidArgs`。
pub fn resolve_png_input(
    args: &Value,
    store: Option<&DefaultResourceStore>,
    bytes_key: &str,
    use_: ResourceUse,
) -> Result<Bytes, CapabilityError> {
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

    if args.get("image_ref").is_some() {
        resolve_image_ref(args, store, use_)
    } else {
        let bytes = parse_byte_array(args, bytes_key)?;
        if bytes.is_empty() {
            return Err(CapabilityError::InvalidArgs {
                detail: format!("{bytes_key} 数据为空"),
            });
        }
        Ok(Bytes::from(bytes))
    }
}

/// 解析并校验 `image_ref` 指向的图片字节。
///
/// 返回 `Bytes`——零拷贝。`use_` 由消费方点名（0.23.12 ResourceUse 封闭枚举），
/// grant 未授予该 use 时返回结构化错误（AI 只能消费签发方授予的用途）。
pub fn resolve_image_ref(
    args: &Value,
    store: Option<&DefaultResourceStore>,
    use_: ResourceUse,
) -> Result<Bytes, CapabilityError> {
    let ref_val = args
        .get("image_ref")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "image_ref 参数格式无效（应为非空字符串）".into(),
        })?;
    let store = store.ok_or_else(|| CapabilityError::InvalidArgs {
        detail: "image_ref 不可用（运行时未启用 ResourceStore）".into(),
    })?;
    let mut opened = store
        .open(&ResourceRef::from_token(ref_val), use_)
        .map_err(|error| CapabilityError::InvalidArgs {
            detail: map_resource_error_message(&error),
        })?;
    if let Some(mime) = opened.mime()
        && !mime.starts_with("image/")
    {
        return Err(CapabilityError::InvalidArgs {
            detail: format!("image_ref 指向的不是图片（mime: {mime}）"),
        });
    }
    // 零拷贝——Memory 腿 Bytes clone 只增 Arc 引用计数
    opened
        .read_all_bounded(IMAGE_REF_READ_BOUND)
        .map_err(|error| CapabilityError::InvalidArgs {
            detail: map_resource_error_message(&error),
        })
}

/// image_ref 整读上限（与内存腿单项上限 32 MiB 对齐）。
const IMAGE_REF_READ_BOUND: u64 = 32 * 1024 * 1024;

/// ResourceError → 用户可读消息（不含路径）。
fn map_resource_error_message(error: &ResourceError) -> String {
    match error.kind {
        crate::domain::resource::ResourceErrorKind::UseDenied => {
            "image_ref 不允许该用途（use denied）".into()
        }
        crate::domain::resource::ResourceErrorKind::StaleResourceRef => "image_ref 已过期".into(),
        _ => "image_ref 不存在或已过期".into(),
    }
}

/// 严格解析 JSON 字节数组；拒绝非整数和超出 u8 范围的元素，不静默丢弃/截断。
pub fn parse_byte_array(args: &Value, key: &str) -> Result<Vec<u8>, CapabilityError> {
    let values =
        args.get(key)
            .and_then(Value::as_array)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: format!("{key} 参数格式无效（应为整数数组）"),
            })?;
    values
        .iter()
        .enumerate()
        .map(|(index, value)| {
            value
                .as_u64()
                .and_then(|number| u8::try_from(number).ok())
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: format!("{key}[{index}] 必须是 0..=255 的整数"),
                })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::resource::{ResourceGrantSpec, ReusePolicy};
    use serde_json::json;

    fn image_store() -> DefaultResourceStore {
        DefaultResourceStore::default()
    }

    #[test]
    fn resolve_png_from_bytes() {
        let args = json!({ "png": [0x89, 0x50, 0x4E, 0x47] });
        let result = resolve_png_input(&args, None, "png", ResourceUse::OcrImage).unwrap();
        assert_eq!(result.as_ref(), &[0x89, 0x50, 0x4E, 0x47]);
    }

    #[test]
    fn resolve_png_from_ref() {
        let store = image_store();
        let token = store
            .issue_memory(
                Bytes::from(vec![1, 2, 3, 4]),
                "image/png",
                ResourceGrantSpec::new(ResourceUse::OcrImage, ReusePolicy::Reusable, "t"),
            )
            .unwrap();
        let args = json!({ "image_ref": token.as_str() });
        let result = resolve_png_input(&args, Some(&store), "png", ResourceUse::OcrImage).unwrap();
        assert_eq!(result.as_ref(), &[1, 2, 3, 4]);
    }

    #[test]
    fn resolve_both_ref_and_bytes_is_error() {
        let store = image_store();
        let token = store
            .issue_memory(
                Bytes::from(vec![1]),
                "image/png",
                ResourceGrantSpec::new(ResourceUse::OcrImage, ReusePolicy::Reusable, "t"),
            )
            .unwrap();
        let args = json!({ "image_ref": token.as_str(), "png": [0x89] });
        let err = resolve_png_input(&args, Some(&store), "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_neither_is_error() {
        let args = json!({});
        let err = resolve_png_input(&args, None, "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_ref_without_store_is_error() {
        let args = json!({ "image_ref": "some_token" });
        let err = resolve_png_input(&args, None, "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_unknown_ref_is_error() {
        let store = image_store();
        let args = json!({ "image_ref": "nonexistent" });
        let err = resolve_png_input(&args, Some(&store), "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    /// 0.23.12：use 未授予 → 结构化拒绝（且不静默降级为字节路径）。
    #[test]
    fn resolve_ref_with_wrong_use_is_error() {
        let store = image_store();
        let token = store
            .issue_memory(
                Bytes::from(vec![1]),
                "image/png",
                ResourceGrantSpec::new(ResourceUse::OcrImage, ReusePolicy::Reusable, "t"),
            )
            .unwrap();
        let args = json!({ "image_ref": token.as_str() });
        let err = resolve_png_input(&args, Some(&store), "png", ResourceUse::PinImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    /// 非 image mime 的 ref 拒绝（防跨媒体误用）。
    #[test]
    fn resolve_non_image_mime_is_error() {
        let store = image_store();
        let token = store
            .issue_memory(
                Bytes::from(vec![1]),
                "application/octet-stream",
                ResourceGrantSpec::new(ResourceUse::OcrImage, ReusePolicy::Reusable, "t"),
            )
            .unwrap();
        let args = json!({ "image_ref": token.as_str() });
        let err = resolve_png_input(&args, Some(&store), "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn resolve_empty_bytes_is_error() {
        let args = json!({ "png": [] });
        let err = resolve_png_input(&args, None, "png", ResourceUse::OcrImage).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn byte_array_rejects_invalid_or_out_of_range_values() {
        for args in [json!({ "png": [1, "2"] }), json!({ "png": [256] })] {
            let err = resolve_png_input(&args, None, "png", ResourceUse::OcrImage).unwrap_err();
            assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
        }
    }
}
