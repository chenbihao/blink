//! `open_url` Capability（0.14.2 §2.3）。
//!
//! 从 Action 提升为 Capability——AI 常用（打开搜索结果 / 用户请求的 URL）。
//! 入参：`{ "url": "..." }`，出参：`Done { summary }`。
//!
//! **0.19.0**：用户侧 `#[tauri::command] open_url` 也改经 `CapabilityRegistry`
//! 调本 Capability，消除双入口（旧 command 直调 `ShellExecuteW`，本 Capability
//! 走 `open::that`，两套独立底层）。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

/// `open_url` — 用默认浏览器打开 URL。
///
/// 入参：`{ "url": "https://..." }`
/// 出参：`Done { summary: "已打开 URL: ..." }`
pub struct OpenUrl;

#[async_trait::async_trait]
impl Capability for OpenUrl {
    fn id(&self) -> &str {
        "open_url"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_url".into(),
            description: "Open a URL in the default web browser".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "url": {
                        "type": "string",
                        "description": "URL to open"
                    }
                },
                "required": ["url"]
            }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_origins: OriginSet::ALL,
            runtime_requirement: RuntimeRequirement::DESKTOP_SESSION,
            danger: DangerClass::Safe,
            sensitive: false,
            ai_default: AiDefault::On,
            mcp_default: McpDefault::DefaultOff,
            confirmation: ConfirmationPolicy::safe(),
        }
    }
    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let url = args
            .get("url")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "open_url: 缺少 url 参数".into(),
            })?;

        tracing::debug!(%url, "open_url capability: 打开链接");

        // open::that 在 Windows 上走 ShellExecute，是非阻塞的
        if let Err(e) = open::that(&url) {
            tracing::error!(error = %e, %url, "打开链接失败");
            return Err(CapabilityError::Internal {
                detail: format!("打开链接失败: {e}"),
            });
        }

        Ok(CapabilityResult::Done {
            summary: format!("已打开 URL: {url}"),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenUrl) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_url() {
        assert_eq!(OpenUrl.id(), "open_url");
    }

    #[test]
    fn schema_has_url_parameter() {
        let s = OpenUrl.schema();
        assert_eq!(s.name, "open_url");
        assert_eq!(s.parameters["properties"]["url"]["type"], "string");
        assert_eq!(s.parameters["required"][0], "url");
    }

    #[test]
    fn schema_description_non_empty() {
        let s = OpenUrl.schema();
        assert!(!s.description.is_empty());
    }
}
