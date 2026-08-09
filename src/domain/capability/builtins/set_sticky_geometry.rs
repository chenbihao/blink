//! `set_sticky_geometry` Capability（0.19.3）。
//!
//! 更新便签窗口的位置和尺寸 → `Done`。
//!
//! **背景**：AI 创建便签后可能需要调整位置（如"把便签移到右上角"），或
//! 通过 `list_windows` 获取窗口布局后将便签钉在特定位置旁。本 cap 提供独立的
//! 几何更新入口，与 `create_sticky`（创建时指定位置）互补。
//!
//! **DangerClass::Safe**（§3.4）：可逆窗口移动不标 Dangerous。
//! 不标 sensitive——几何参数不含隐私数据。
//!
//! **仅更新 DB**：本 cap 调 `sticky_service().update_geometry()` 更新持久化几何，
//! 桌面窗口的实际位置在下次窗口显示时生效。若需立即移动已显示的便签窗口，
//! 可通过 `create_sticky` 重新创建或后续版本补充窗口重定位 cap。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `set_sticky_geometry` — 更新便签的位置和尺寸。
///
/// 入参：`{ id: String, x: int, y: int, w: int, h: int }`。
/// 出参：`Done { summary: "已更新便签几何" }`。
pub struct SetStickyGeometry;

#[async_trait::async_trait]
impl Capability for SetStickyGeometry {
    fn id(&self) -> &str {
        "set_sticky_geometry"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "set_sticky_geometry".into(),
            description: "更新指定便签的位置和尺寸（物理像素坐标）。仅更新持久化的几何数据，已显示的便签窗口不会立即移动。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "id": {
                        "type": "string",
                        "description": "便签 id（由 create_sticky 返回或 list_sticky 查到）"
                    },
                    "x": {
                        "type": "integer",
                        "description": "窗口左上角 x 坐标（物理像素）"
                    },
                    "y": {
                        "type": "integer",
                        "description": "窗口左上角 y 坐标（物理像素）"
                    },
                    "w": {
                        "type": "integer",
                        "description": "窗口宽度（物理像素）"
                    },
                    "h": {
                        "type": "integer",
                        "description": "窗口高度（物理像素）"
                    }
                },
                "required": ["id", "x", "y", "w", "h"]
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
                detail: "set_sticky_geometry 截止时刻已过".into(),
            });
        }

        let id = args
            .get("id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_geometry: 缺少 id 参数".into(),
            })?;

        let x = args
            .get("x")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_geometry: 缺少 x 参数".into(),
            })? as i32;
        let y = args
            .get("y")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_geometry: 缺少 y 参数".into(),
            })? as i32;
        let w = args
            .get("w")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_geometry: 缺少 w 参数".into(),
            })? as i32;
        let h = args
            .get("h")
            .and_then(Value::as_i64)
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "set_sticky_geometry: 缺少 h 参数".into(),
            })? as i32;

        let svc = ctx.env.sticky_service().ok_or_else(|| CapabilityError::Internal {
            detail: "StickyService 不可用".into(),
        })?;

        svc.update_geometry(id, x, y, w, h)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("更新便签几何失败: {e}"),
            })?;

        tracing::info!(sticky_id = %id, x, y, w, h, "set_sticky_geometry: 便签几何已更新");

        Ok(CapabilityResult::Done {
            summary: "已更新便签几何".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(SetStickyGeometry) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_set_sticky_geometry() {
        assert_eq!(SetStickyGeometry.id(), "set_sticky_geometry");
    }

    #[test]
    fn schema_has_all_required_params() {
        let s = SetStickyGeometry.schema();
        assert_eq!(s.name, "set_sticky_geometry");
        let required = s.parameters["required"].as_array().unwrap();
        assert!(required.iter().any(|v| v == "id"));
        assert!(required.iter().any(|v| v == "x"));
        assert!(required.iter().any(|v| v == "y"));
        assert!(required.iter().any(|v| v == "w"));
        assert!(required.iter().any(|v| v == "h"));
    }

    #[test]
    fn schema_sensitive_is_false() {
        let s = SetStickyGeometry.schema();
        assert!(!s.sensitive, "set_sticky_geometry 不标 sensitive（几何参数不含隐私）");
    }

    #[test]
    fn danger_class_is_safe() {
        use crate::domain::execution::DangerClass;
        assert_eq!(SetStickyGeometry.danger_class(), DangerClass::Safe);
    }
}
