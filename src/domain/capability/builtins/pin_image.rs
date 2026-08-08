//! `pin_image` Capability（0.19.6）。
//!
//! 将 PNG 图片钉到桌面（pin 窗口） → `Done`。
//!
//! **背景**：pin 窗口原先只在 command 层可用（`pin_clipboard_image`——前端右键钉图），
//! AI 看不到。本 cap 补上"AI pin 图到桌面"的执行入口，是"读剪贴板图片钉桌面"
//! "截图后 pin 到桌面"等场景的核心依赖。
//!
//! **DangerClass::Safe**（§3.4）：
//! - pin 是可逆的（用户能关），不标 Dangerous，与 `open_url` 同级
//! - 不标 sensitive——pin 操作不涉及读/写用户隐私数据，图片来源由上游 cap
//!   （如 `read_clipboard` / `screenshot`）的 sensitive 标签覆盖
//!
//! **位置参数**：`x`/`y` 可选（物理像素），None 时调 `parse_png_size` 取图片尺寸，
//! 再用 `get_primary_monitor_center` 居中到光标所在显示器工作区。
//! AI 可通过 `list_windows` 获取窗口位置后计算目标坐标，实现"pin 图到某窗口旁"。

use std::sync::Arc;

use serde_json::{Value, json};

use super::image_input::resolve_png_input;
use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `pin_image` — 将 PNG 图片钉到桌面。
///
/// 入参：`{ png: [u8], x?: int, y?: int }`。
/// 出参：`Done { summary: "已 pin 图到桌面" }`。
pub struct PinImage;

#[async_trait::async_trait]
impl Capability for PinImage {
    fn id(&self) -> &str {
        "pin_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "pin_image".into(),
            description: "将 PNG 图片钉到桌面（显示为始终置顶的透明窗口）。可指定位置(x/y物理像素)，不指定则居中于光标所在显示器。图片来源：image_ref（来自截图/剪贴板等能力返回的引用）或 png（原始 PNG 字节数组），二选一。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "image_ref": {
                        "type": "string",
                        "description": "图片引用（来自 read_clipboard/screenshot 等能力返回的 image_ref，与 png 二选一）"
                    },
                    "png": {
                        "type": "array",
                        "items": { "type": "integer" },
                        "description": "PNG 图片字节数组（与 image_ref 二选一）"
                    },
                    "x": {
                        "type": "integer",
                        "description": "图片左上角 x 坐标（物理像素），不指定则居中"
                    },
                    "y": {
                        "type": "integer",
                        "description": "图片左上角 y 坐标（物理像素），不指定则居中"
                    }
                }
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // 铁则 1 前置检查
        if ctx.is_expired() {
            return Err(CapabilityError::Timeout {
                detail: "pin_image 截止时刻已过".into(),
            });
        }

        // 提取 PNG 字节：image_ref 或 png 二选一（0.19.4）
        let stash = ctx.env.image_stash();
        let png_bytes = resolve_png_input(&args, stash.map(|s| s.as_ref()), "png")?;

        // 提取可选位置
        let x = args.get("x").and_then(Value::as_i64).map(|v| v as i32);
        let y = args.get("y").and_then(Value::as_i64).map(|v| v as i32);

        // 位置兜底：x/y 任一为 None 时，解析 PNG 尺寸后居中
        let (x, y) = match (x, y) {
            (Some(px), Some(py)) => (px, py),
            _ => {
                let (w, h) = crate::infra::platform::screenshot::parse_png_size(&png_bytes)
                    .map(|(pw, ph)| (pw as i32, ph as i32))
                    .unwrap_or((400, 300));
                let (cx, cy) =
                    crate::infra::platform::window::get_primary_monitor_center(w, h);
                (x.unwrap_or(cx), y.unwrap_or(cy))
            }
        };

        tracing::debug!(x, y, bytes = png_bytes.len(), "pin_image: 开始 pin 图");

        // 调 CapabilityEnv 桥接（show_translating 固定 false）
        ctx.env
            .show_pin_window(png_bytes, x, y)
            .map_err(|e| CapabilityError::Internal {
                detail: format!("pin 图失败: {e}"),
            })?;

        tracing::info!(x, y, "pin_image: 图片已钉到桌面");

        Ok(CapabilityResult::Done {
            summary: "已 pin 图到桌面".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(PinImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_pin_image() {
        assert_eq!(PinImage.id(), "pin_image");
    }

    #[test]
    fn schema_has_png_and_image_ref_params() {
        let s = PinImage.schema();
        assert_eq!(s.name, "pin_image");
        assert_eq!(s.parameters["properties"]["png"]["type"], "array");
        assert_eq!(s.parameters["properties"]["image_ref"]["type"], "string");
        // 0.19.4: png 不再 required，与 image_ref 二选一
        let required = s.parameters.get("required");
        assert!(required.is_none() || required.unwrap().as_array().unwrap().is_empty());
    }

    #[test]
    fn schema_has_optional_position_params() {
        let s = PinImage.schema();
        assert_eq!(s.parameters["properties"]["x"]["type"], "integer");
        assert_eq!(s.parameters["properties"]["y"]["type"], "integer");
        // x/y 不在 required 中（0.19.4: 无 required 字段）
        let required = s.parameters.get("required");
        assert!(
            required.is_none() || required.unwrap().as_array().unwrap().is_empty(),
            "不应有 required 字段"
        );
    }

    #[test]
    fn schema_sensitive_is_false() {
        let s = PinImage.schema();
        assert!(
            !s.sensitive,
            "pin_image 不标 sensitive——隐私由上游图片来源 cap 覆盖"
        );
    }

    #[test]
    fn danger_class_is_safe() {
        use crate::domain::execution::DangerClass;
        assert_eq!(PinImage.danger_class(), DangerClass::Safe);
    }

    #[test]
    fn schema_description_mentions_pin() {
        let s = PinImage.schema();
        assert!(
            s.description.contains("pin") || s.description.contains("钉"),
            "schema description 应提及 pin/钉"
        );
    }
}
