//! 0.26.9：把文件条目的路径列表写回系统剪贴板（CF_HDROP）。
//!
//! 入口绑定条目引用（id），经领域服务查库取路径——不接受 UI 任意路径列表。
//! 写回按**复制语义**：不继承剪切标记、不预检路径存在性、不发送粘贴键；
//! 历史路径已失效时仍可复制，目标应用决定如何处理。

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    id: String,
}

pub struct CopyClipboardFiles;
#[async_trait::async_trait]
impl Capability for CopyClipboardFiles {
    fn id(&self) -> &str {
        "copy_clipboard_files"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Copy a clipboard file-list entry back to the system clipboard (CF_HDROP)"
                .into(),
            parameters: json!({"type":"object","properties":{
                "id":{"type":"string","description":"clipboard_files 条目 id"}},
                "required":["id"],"additionalProperties":false}),
            ..Default::default()
        }
    }
    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::GUI_SURFACE,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let request: Request =
            serde_json::from_value(args).map_err(|e| CapabilityError::InvalidArgs {
                detail: e.to_string(),
            })?;
        let pools = ctx.env.db_pools();
        let item = blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, &request.id)
            .await
            .ok_or_else(|| CapabilityError::InvalidState {
                detail: "文件条目不存在或已删除".into(),
            })?;
        let count = item.paths.len();
        crate::clipboard::write_file_list(item.paths, crate::clipboard::ClipboardWriteSource::HistoryRepost)
            .await
            .map_err(|e| CapabilityError::Permission {
                detail: e.to_string(),
            })?;
        tracing::debug!(count, "文件条目已写回剪贴板 (CF_HDROP)");
        Ok(CapabilityResult::Done {
            summary: format!("已复制 {count} 个文件路径到剪贴板，粘贴由目标应用决定"),
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(CopyClipboardFiles) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_is_local_explicit_and_registered() {
        let registry = crate::capability::CapabilityRegistry::new();
        assert!(registry.get("copy_clipboard_files").is_some());
        let policy = CopyClipboardFiles.policy();
        assert!(
            policy
                .allowed_origins
                .contains(crate::capability::InvocationOrigin::LocalSurface)
        );
        assert!(!policy
            .allowed_origins
            .contains(crate::capability::InvocationOrigin::Mcp));
    }
    #[test]
    fn request_rejects_unknown_fields_and_missing_id() {
        assert!(serde_json::from_value::<Request>(json!({"id":"x"})).is_ok());
        assert!(serde_json::from_value::<Request>(json!({})).is_err());
        assert!(
            serde_json::from_value::<Request>(json!({"id":"x","paths":["C:\\a"]})).is_err(),
            "不接受 UI 任意路径列表"
        );
    }
}
