//! `capture_screen` Capability（0.9.7 Step 2）。
//!
//! 截取整个虚拟屏幕 → `Blob{png}`。
//!
//! **SESSION cache 模式**（文档 §5.1 甲方案）：
//! 有最近一帧 SESSION（Alt+A 截图会话遗留）就复用其 PNG；无则新截。
//! AI 调用时 SESSION 多半已过期 → 走新截，行为正确。
//! Alt+A 交互入口复用同一份能力，性能零回退。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `capture_screen` — 截取虚拟屏幕，返回 PNG。
///
/// 入参：`{}` 或 `{ "display": 0 }`（display 暂未实现多屏，留参数位）。
/// 出参：`Blob { mime: "image/png", bytes }`。
pub struct CaptureScreen;

#[async_trait::async_trait]
impl Capability for CaptureScreen {
    fn id(&self) -> &str {
        "capture_screen"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "capture_screen".into(),
            description: "截取屏幕（整个虚拟屏幕），返回 PNG 图片。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "display": {
                        "type": "integer",
                        "description": "显示器编号（0=主屏，预留多屏，当前忽略）",
                        "default": 0
                    }
                }
            }),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        // SESSION cache 优先——Alt+A 截图会话遗留的最近一帧直接复用
        if let Some(png) = crate::infra::platform::screenshot::session_png() {
            tracing::debug!(bytes = png.len(), "capture_screen: 复用 SESSION cache");
            return Ok(CapabilityResult::Blob {
                mime: "image/png".into(),
                bytes: png,
            });
        }

        // 无 SESSION → 新截一帧（begin_session 内部截屏 + 编码 PNG）
        let _meta = tokio::task::spawn_blocking(crate::infra::platform::screenshot::begin_session)
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("截屏 task 崩溃: {e}"),
            })?
            .map_err(|e| CapabilityError::Internal { detail: e })?;

        // begin_session 成功后 SESSION 已填充，读 PNG
        match crate::infra::platform::screenshot::session_png() {
            Some(png) => {
                tracing::debug!(bytes = png.len(), "capture_screen: 新截 + 编码 PNG");
                Ok(CapabilityResult::Blob {
                    mime: "image/png".into(),
                    bytes: png,
                })
            }
            None => Err(CapabilityError::Internal {
                detail: "begin_session 成功但 session_png 返回空".into(),
            }),
        }
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(CaptureScreen) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_capture_screen() {
        assert_eq!(CaptureScreen.id(), "capture_screen");
    }

    #[test]
    fn schema_has_png_description() {
        let s = CaptureScreen.schema();
        assert_eq!(s.name, "capture_screen");
        assert!(s.description.contains("PNG"));
        // display 参数存在但非 required（可选）
        assert!(s.parameters["properties"]["display"]["type"] == "integer");
    }
}
