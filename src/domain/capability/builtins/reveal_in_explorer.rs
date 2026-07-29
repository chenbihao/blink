//! `reveal_in_explorer` Capability（0.14.2 §2.3）。
//!
//! 从 Action 提升为 Capability——AI 常用（在资源管理器中定位搜索结果文件）。
//! 入参：`{ "path": "..." }`，出参：`Done { summary }`。
//!
//! **与 Action 的关系**：同 `open_url`，Action 保留供主窗口搜索流使用。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `reveal_in_explorer` — 在 Windows 资源管理器中定位文件（高亮显示）。
///
/// 入参：`{ "path": "C:\\..." }`
/// 出参：`Done { summary: "已在资源管理器中定位: ..." }`
pub struct RevealInExplorer;

#[async_trait::async_trait]
impl Capability for RevealInExplorer {
    fn id(&self) -> &str {
        "reveal_in_explorer"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "reveal_in_explorer".into(),
            description:
                "Reveal a file in Windows Explorer (highlight it in its folder)".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute file path to reveal"
                    }
                },
                "required": ["path"]
            }),
            ..Default::default()
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let path = args
            .get("path")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| CapabilityError::InvalidArgs {
                detail: "reveal_in_explorer: 缺少 path 参数".into(),
            })?;

        tracing::debug!(%path, "reveal_in_explorer capability: 在资源管理器中显示");

        #[cfg(target_os = "windows")]
        {
            let status = std::process::Command::new("explorer.exe")
                .args(["/select,", &path])
                .spawn();
            if let Err(e) = status {
                tracing::error!(error = %e, %path, "调用 explorer.exe 失败");
                return Err(CapabilityError::Internal {
                    detail: format!("调用 explorer.exe 失败: {e}"),
                });
            }
        }

        Ok(CapabilityResult::Done {
            summary: format!("已在资源管理器中定位: {path}"),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(RevealInExplorer) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_reveal_in_explorer() {
        assert_eq!(RevealInExplorer.id(), "reveal_in_explorer");
    }

    #[test]
    fn schema_has_path_parameter() {
        let s = RevealInExplorer.schema();
        assert_eq!(s.name, "reveal_in_explorer");
        assert_eq!(s.parameters["properties"]["path"]["type"], "string");
        assert_eq!(s.parameters["required"][0], "path");
    }

    #[test]
    fn schema_description_non_empty() {
        let s = RevealInExplorer.schema();
        assert!(!s.description.is_empty());
    }
}
