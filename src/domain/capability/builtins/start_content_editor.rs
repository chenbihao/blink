//! `start_content_editor` Capability（0.21.2）——Chord `edit` binding 的 GUI starter target。
//!
//! 打开通用内容编辑器窗口。可选 `body` / `title` / `origin` / `origin_ref` / `save_policy`
//! 参数作为结构化 prefill。
//! 需要 GUI_SURFACE 运行时。AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。
//!
//! **与旧 ChordAction 的关系**：旧 `EditAction` 实现 `Action::execute()`，
//! 内部调 `DomainEnv::show_content_editor(...)` + `hide_main_window`。
//! 本 Capability 通过 `SurfacePort::start_content_editor` 启动编辑器，
//! 返回"已启动内容编辑器，等待用户编辑"。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, ContentEditorRequest, DangerClass, InvokeContext, McpDefault, OriginSet,
    RuntimeRequirement,
};

pub struct StartContentEditor;

#[async_trait::async_trait]
impl Capability for StartContentEditor {
    fn id(&self) -> &str {
        "start_content_editor"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "start_content_editor".into(),
            description: "Open the content editor window with optional prefill text. Returns 'started, awaiting user'.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "body": {
                        "type": "string",
                        "description": "Initial text content for the editor"
                    },
                    "title": {
                        "type": "string",
                        "description": "Optional window title"
                    },
                    "origin": {
                        "type": "string",
                        "description": "Source identifier (e.g. 'chord', 'clipboard', 'sticky')"
                    },
                    "origin_ref": {
                        "type": "string",
                        "description": "Optional reference to original entity id (e.g. clipboard record id)"
                    },
                    "save_policy": {
                        "type": "string",
                        "description": "Save policy: 'clipboard_new' or 'sticky_update'"
                    }
                }
            }),
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
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let surface = ctx
            .runtime
            .surface
            .ok_or_else(|| CapabilityError::Unsupported {
                required: RuntimeRequirement::GUI_SURFACE.to_string(),
                actual: ctx.runtime.as_requirement().to_string(),
            })?;

        let body = args
            .get("body")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let title = args
            .get("title")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let origin = args
            .get("origin")
            .and_then(|v| v.as_str())
            .unwrap_or("chord")
            .to_string();
        let origin_ref = args
            .get("origin_ref")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let save_policy = args
            .get("save_policy")
            .and_then(|v| v.as_str())
            .unwrap_or("clipboard_new")
            .to_string();

        let request = ContentEditorRequest {
            body,
            title,
            origin,
            origin_ref,
            save_policy,
        };

        surface.hide_main_window("start_content_editor");
        surface
            .start_content_editor(request)
            .map_err(|e| CapabilityError::Internal {
                detail: e.to_string(),
            })?;

        Ok(CapabilityResult::Done {
            summary: "已启动内容编辑器，等待用户编辑".into(),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(StartContentEditor) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_start_content_editor() {
        assert_eq!(StartContentEditor.id(), "start_content_editor");
    }

    #[test]
    fn policy_is_safe_gui_ai_on_mcp_forbidden() {
        let p = StartContentEditor.policy();
        assert_eq!(p.danger, DangerClass::Safe);
        assert!(!p.sensitive);
        assert_eq!(p.runtime_requirement, RuntimeRequirement::GUI_SURFACE);
        assert_eq!(p.ai_default, AiDefault::On);
        assert_eq!(p.mcp_default, McpDefault::Forbidden);
    }

    #[test]
    fn schema_has_body_param() {
        let s = StartContentEditor.schema();
        assert!(s.parameters["properties"]["body"]["type"].is_string());
    }
}
