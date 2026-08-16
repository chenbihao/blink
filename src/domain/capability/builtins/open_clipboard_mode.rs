//! `open_clipboard_mode` Capability（0.21.2）——Chord `clipboard_history` binding 的 GUI starter target。
//!
//! 打开剪贴板历史浏览模式（主窗 + fill-query）。打开模式即完成。
//! 需要 GUI_SURFACE 运行时。AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。
//!
//! **与旧 ChordAction 的关系**：旧 `ClipboardHistoryAction` 实现 `Action::execute()`，
//! 内部调 `DomainEnv::invoke_main_window()` + emit `CHORD_ENTER_MODE` 事件。
//! 本 Capability 通过 `SurfacePort::open_clipboard_mode` 启动剪贴板模式，
//! SurfacePort 实现内部完成主窗 show + emit 副作用。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

pub struct OpenClipboardMode;

#[async_trait::async_trait]
impl Capability for OpenClipboardMode {
    fn id(&self) -> &str {
        "open_clipboard_mode"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_clipboard_mode".into(),
            description:
                "Open the clipboard history browser mode in the main window. No arguments.".into(),
            parameters: json!({ "type": "object", "properties": {} }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL_LOCAL,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
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
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: RuntimeRequirement::GUI_SURFACE.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;

        surface
            .open_clipboard_mode()
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Done {
            summary: "已打开剪贴板历史".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenClipboardMode) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_clipboard_mode() {
        assert_eq!(OpenClipboardMode.id(), "open_clipboard_mode");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = OpenClipboardMode.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }
}
