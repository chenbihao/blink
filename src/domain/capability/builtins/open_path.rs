//! `open_path` Capability（0.14.2 §2.3）。
//!
//! 从 Action 提升为 Capability——AI 常用（打开搜索到的文件 / 目录）。
//! 入参：`{ "path": "..." }`，出参：`Done { summary }`。
//!
//! **与 Action 的关系**：同 `open_url`，Action 保留供主窗口搜索流使用。

use std::sync::Arc;

use serde_json::{Value, json};

use crate::domain::capability::{
    Capability, CapabilityError, CapabilityResult, CapabilitySchema, InvokeContext,
};

/// `open_path` — 用系统默认程序打开文件或目录。
///
/// 入参：`{ "path": "C:\\..." }`
/// 出参：`Done { summary: "已打开: ..." }`
pub struct OpenPath;

#[async_trait::async_trait]
impl Capability for OpenPath {
    fn id(&self) -> &str {
        "open_path"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "open_path".into(),
            description: "Open a file or directory with the system default program".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "Absolute file or directory path"
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
                detail: "open_path: 缺少 path 参数".into(),
            })?;

        tracing::debug!(%path, "open_path capability: 打开路径");

        if let Err(e) = open::that(&path) {
            tracing::error!(error = %e, %path, "打开路径失败");
            return Err(CapabilityError::Internal {
                detail: format!("打开路径失败: {e}"),
            });
        }

        Ok(CapabilityResult::Done {
            summary: format!("已打开: {path}"),
        })
    }
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || Arc::new(OpenPath) as Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn id_is_open_path() {
        assert_eq!(OpenPath.id(), "open_path");
    }

    #[test]
    fn schema_has_path_parameter() {
        let s = OpenPath.schema();
        assert_eq!(s.name, "open_path");
        assert_eq!(s.parameters["properties"]["path"]["type"], "string");
        assert_eq!(s.parameters["required"][0], "path");
    }

    #[test]
    fn schema_description_non_empty() {
        let s = OpenPath.schema();
        assert!(!s.description.is_empty());
    }
}
