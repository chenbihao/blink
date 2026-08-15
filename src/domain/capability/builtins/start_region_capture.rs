//! `start_region_capture` Capability（0.21.2）——Chord `screenshot` binding 的 GUI starter target。
//!
//! 启动区域截图选区 overlay。与 headless `screenshot` Capability 明确分离：
//! - `screenshot`：全屏截图，返回图片字节（headless，无 UI 选区）
//! - `start_region_capture`：打开全屏选区 overlay，等待用户拖选区域
//!
//! 需要 DESKTOP_SESSION 运行时（截图需要交互桌面会话）。
//! AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用 + DESKTOP_SESSION）。
//!
//! **与旧 ChordAction 的关系**：旧 `ScreenshotAction` 实现 `Action::execute()`，
//! 内部调一系列 `DomainEnv` 截图方法（`hide_for_screenshot`、`wait_frame_after_hide`、
//! `begin_session`、`show_screenshot_overlay`）。
//! 本 Capability 通过 `SurfacePort::start_region_capture` 启动截图流程，
//! 返回"已启动截图选区，等待用户"。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct StartRegionCapture;

#[async_trait::async_trait]
impl Capability for StartRegionCapture {
    fn id(&self) -> &str {
        "start_region_capture"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "start_region_capture".into(),
            description: "Start interactive region screenshot capture — shows fullscreen overlay for user to drag-select an area. No arguments.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION | RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }

    async fn invoke(
        &self,
        _args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let surface = ctx.runtime.surface.ok_or_else(|| CapabilityError::Unsupported {
            required: RuntimeRequirement::GUI_SURFACE.to_string(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;

        // 检查 DESKTOP_SESSION
        let req = RuntimeRequirement::DESKTOP_SESSION | RuntimeRequirement::GUI_SURFACE;
        if !req.is_satisfied_by(ctx.runtime.as_requirement()) {
            return Err(CapabilityError::Unsupported {
                required: req.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            });
        }

        surface.start_region_capture().await.map_err(|e| CapabilityError::Internal {
            detail: e.to_string(),
        })?;

        Ok(CapabilityResult::Done {
            summary: "已启动截图选区，等待用户拖选".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(StartRegionCapture) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_start_region_capture() {
        assert_eq!(StartRegionCapture.id(), "start_region_capture");
    }

    #[test]
    fn policy_is_safe_desktop_gui_ai_on_mcp_forbidden() {
        let p = StartRegionCapture.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(
            p.runtime_requirement,
            RuntimeRequirement::DESKTOP_SESSION | RuntimeRequirement::GUI_SURFACE
        );
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }

    #[test]
    fn schema_description_non_empty() {
        let s = StartRegionCapture.schema();
        assert!(!s.description.is_empty());
    }
}
