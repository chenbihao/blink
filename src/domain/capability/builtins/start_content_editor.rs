//! `start_content_editor` Capability（0.21.2，0.23.1 迁移结构化请求）——
//! Chord `edit` binding 的 GUI starter target。
//!
//! 打开通用内容编辑器窗口。可选 `body` / `title` 参数作为结构化 prefill，
//! 来源固定为 `CapabilityResult`（保存目标按来源默认映射推导为剪贴板结果）。
//! 需要 GUI_SURFACE 运行时。AI 推荐 allowlist 默认开启；MCP 代码级禁止（GUI 副作用）。
//!
//! **会话语义（0.23.1）**：编辑器同一时刻只允许一个活动会话；已有异源任务时
//! 返回“已有编辑任务进行中”，不覆盖正文。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use crate::domain::editor::{OpenEditorRequest, SourceDescriptor};

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

        let request = OpenEditorRequest {
            body,
            title,
            source: SourceDescriptor::CapabilityResult {
                capability_id: self.id().to_string(),
            },
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
