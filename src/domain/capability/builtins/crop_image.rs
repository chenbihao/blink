//! `crop_image` Capability（0.9.7 → 0.11.7-f alias）。
//!
//! **0.11.7-f 重构**：核心实现移到 `screenshot.rs`（op=crop）。本文件作为 tool 名
//! alias 保留 3 个月，避免 AI 提示词层缓存失效（详见 phases/0.11.7 §12.6）。
//!
//! **TODO(0.13)** ⏰ 别名到期删除：与 `capture_screen.rs` 一起清理，删除清单见彼处。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `crop_image` — 从当前截图会话裁剪区域（alias to `screenshot { op: crop, x/y/w/h }`）。
pub struct CropImage;

#[async_trait::async_trait]
impl Capability for CropImage {
    fn id(&self) -> &str {
        "crop_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "crop_image".into(),
            description: "从当前截图会话裁剪指定区域，返回 PNG 图片。需先 capture_screen。（alias：等价于 screenshot { op: crop }）".into(),
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
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 参数解析（与 op_crop 一致的错误消息）
        let x = args.get("x").and_then(Value::as_i64).ok_or_else(|| {
            CapabilityError::InvalidArgs { detail: "缺少 x".into() }
        })? as i32;
        let y = args.get("y").and_then(Value::as_i64).ok_or_else(|| {
            CapabilityError::InvalidArgs { detail: "缺少 y".into() }
        })? as i32;
        let w = args.get("w").and_then(Value::as_u64).ok_or_else(|| {
            CapabilityError::InvalidArgs { detail: "缺少 w".into() }
        })? as u32;
        let h = args.get("h").and_then(Value::as_u64).ok_or_else(|| {
            CapabilityError::InvalidArgs { detail: "缺少 h".into() }
        })? as u32;

        // 委托到统一 screenshot Capability 的 op=crop
        super::screenshot::op_crop(x, y, w, h).await
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
    fn schema_marks_alias() {
        let s = CropImage.schema();
        assert!(s.description.contains("alias"));
    }

    /// alias 委托：无 SESSION 时应返回 InvalidArgs（与 op_crop 一致）。
    #[tokio::test]
    async fn alias_delegates_to_op_crop() {
        crate::infra::platform::screenshot::end_session();
        let result = super::super::screenshot::op_crop(0, 0, 10, 10).await;
        assert!(matches!(result, Err(CapabilityError::InvalidArgs { .. })));
    }
}
