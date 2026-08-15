//! `edit_clipboard_image` Capability（0.21.1）——从 Action 迁移为 Safe GUI Capability。
//!
//! 读取当前剪贴板图片并打开图片编辑器。返回"已启动、等待用户"。
//! 需要 GUI_SURFACE 运行时。AI 推荐 allowlist 默认开启；MCP 代码级禁止。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
    CapabilityPolicy, ConfirmationPolicy, DangerClass, AiDefault, McpDefault, OriginSet,
    RuntimeRequirement, EditorSourceRef,
};

pub struct EditClipboardImage;

#[async_trait::async_trait]
impl Capability for EditClipboardImage {
    fn id(&self) -> &str {
        "edit_clipboard_image"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "edit_clipboard_image".into(),
            description: "Open the current clipboard image in the local annotation editor".into(),
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
        let surface = ctx.runtime.surface.ok_or_else(|| CapabilityError::Unsupported {
            required: RuntimeRequirement::GUI_SURFACE.to_string(),
            actual: ctx.runtime.as_requirement().to_string(),
        })?;

        // 读取当前剪贴板内容
        let content = crate::domain::clipboard::read_current()
            .await
            .map_err(|e| CapabilityError::Internal {
                detail: format!("读取剪贴板失败: {e}"),
            })?;

        let crate::domain::clipboard::ClipboardContent::ImagePng(png_data) = content else {
            return Err(CapabilityError::InvalidState {
                detail: "当前剪贴板中没有图片".into(),
            });
        };

        surface.hide_main_window("edit_clipboard_image");
        surface
            .start_image_editor(EditorSourceRef::ClipboardImage(png_data))
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Done {
            summary: "已启动图片编辑器，等待用户编辑".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(EditClipboardImage) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_edit_clipboard_image() {
        assert_eq!(EditClipboardImage.id(), "edit_clipboard_image");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = EditClipboardImage.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }
}
