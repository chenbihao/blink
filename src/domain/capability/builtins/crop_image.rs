//! `crop_image` Capability（0.9.7 Step 2）。
//!
//! 从 SESSION 裁剪子矩形 → `Blob{png}`。
//!
//! 链路：`crop(x,y,w,h)` → BGRA 字节 → `encode_png(bgra,w,h)` → PNG。
//! `encode_png` 内部做 BGRA→RGBA swap，所以 crop 返回的 BGRA 直接喂入，零多余转换。
//!
//! **依赖 SESSION**：若截图会话未建立（用户未 Alt+A），返回 InvalidArgs 提示。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `crop_image` — 从当前截图会话裁剪区域，返回 PNG。
///
/// 入参：`{ "x": int, "y": int, "w": int, "h": int }`（物理像素，虚拟屏幕坐标系）。
/// 出参：`Blob { mime: "image/png", bytes }`。
pub struct CropImage;

#[async_trait::async_trait]
impl Capability for CropImage {
    fn id(&self) -> &str {
        "crop_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "crop_image".into(),
            description: "从当前截图会话裁剪指定区域，返回 PNG 图片。需先 capture_screen。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "x": { "type": "integer", "description": "裁剪起点 X（物理像素）" },
                    "y": { "type": "integer", "description": "裁剪起点 Y（物理像素）" },
                    "w": { "type": "integer", "description": "裁剪宽度（物理像素）" },
                    "h": { "type": "integer", "description": "裁剪高度（物理像素）" }
                },
                "required": ["x", "y", "w", "h"]
            }),
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 解析参数
        let x =
            args.get("x")
                .and_then(Value::as_i64)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 x".into(),
                })? as i32;
        let y =
            args.get("y")
                .and_then(Value::as_i64)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 y".into(),
                })? as i32;
        let w =
            args.get("w")
                .and_then(Value::as_u64)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 w".into(),
                })? as u32;
        let h =
            args.get("h")
                .and_then(Value::as_u64)
                .ok_or_else(|| CapabilityError::InvalidArgs {
                    detail: "缺少 h".into(),
                })? as u32;

        // 裁剪（BGRA）+ 编码 PNG —— spawn_blocking 避免 Win32/编码阻塞 tokio
        let png =
            tokio::task::spawn_blocking(move || -> Result<Vec<u8>, CapabilityError> {
                let (bgra, cw, ch) = crate::infra::platform::screenshot::crop(x, y, w, h)
                    .ok_or_else(|| CapabilityError::InvalidArgs {
                        detail: "截图会话为空或裁剪区域无效".into(),
                    })?;
                crate::infra::platform::screenshot::encode_png(&bgra, cw, ch)
                    .map_err(|e| CapabilityError::Internal { detail: e })
            })
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("crop task 崩溃: {e}"),
            })??;

        Ok(CapabilityResult::Blob {
            mime: "image/png".into(),
            bytes: png,
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(CropImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_crop_image() {
        assert_eq!(CropImage.id(), "crop_image");
    }

    #[test]
    fn schema_requires_xywh() {
        let s = CropImage.schema();
        let required = s.parameters["required"].as_array().unwrap();
        assert!(required.contains(&json!("x")));
        assert!(required.contains(&json!("y")));
        assert!(required.contains(&json!("w")));
        assert!(required.contains(&json!("h")));
    }

    #[test]
    fn schema_description_mentions_session() {
        let s = CropImage.schema();
        assert!(s.description.contains("截图会话"));
    }
}
