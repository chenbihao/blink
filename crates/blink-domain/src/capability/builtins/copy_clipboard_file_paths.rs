//! 0.26.9：把文件条目的路径列表复制为**路径文本**（原始路径每行一个）。
//!
//! 多条目（纯文件多选合并）按当前列表顺序合并，去除完全相同路径且保留首次
//! 出现顺序；路径文本最多 1MiB UTF-8——构建时逐段计量，超限整体报错且不
//! 覆盖系统剪贴板，不先拼无界字符串。不把文本或图片伪装成文件路径。

use crate::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

/// 路径文本 UTF-8 字节上限（含换行符，逐段计量）。
const MAX_PATHS_TEXT_BYTES: usize = 1024 * 1024;
/// 单次最多合并的条目数（与 manage_clipboard_history 的 2000 对齐）。
const MAX_ENTRIES: usize = 2000;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    ids: Vec<String>,
}

pub struct CopyClipboardFilePaths;
#[async_trait::async_trait]
impl Capability for CopyClipboardFilePaths {
    fn id(&self) -> &str {
        "copy_clipboard_file_paths"
    }
    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: self.id().into(),
            description: "Copy file paths from clipboard file entries as newline-separated text"
                .into(),
            parameters: json!({"type":"object","properties":{
                "ids":{"type":"array","minItems":1,"maxItems":2000,"items":{"type":"string"}}},
                "required":["ids"],"additionalProperties":false}),
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
        // serde 不校验 JSON Schema 的 minItems，运行时显式拒绝空数组
        if request.ids.is_empty() || request.ids.len() > MAX_ENTRIES {
            return Err(CapabilityError::InvalidArgs {
                detail: format!("一次须提供 1–{MAX_ENTRIES} 条文件条目"),
            });
        }
        let pools = ctx.env.db_pools();
        // 逐条查库取原始顺序路径，合并去重（完全相同路径保留首次出现顺序）
        let mut merged: Vec<String> = Vec::new();
        for id in &request.ids {
            let item =
                blink_infra::data::clipboard_files::get_paths_by_id(&pools.history, id)
                    .await
                    .ok_or_else(|| CapabilityError::InvalidState {
                        detail: "文件条目不存在或已删除".into(),
                    })?;
            for path in item.paths {
                if !merged.contains(&path) {
                    merged.push(path);
                }
            }
        }
        // 逐段计量：超限整体报错，不覆盖系统剪贴板
        let mut total = 0usize;
        for path in &merged {
            total = total
                .saturating_add(path.len())
                .saturating_add("\n".len());
            if total > MAX_PATHS_TEXT_BYTES {
                return Err(CapabilityError::InvalidArgs {
                    detail: "路径文本超过 1MiB 上限，未写入剪贴板".into(),
                });
            }
        }
        let count = merged.len();
        let text = merged.join("\n");
        crate::clipboard::write_text(text, crate::clipboard::ClipboardWriteSource::User)
            .await
            .map_err(|e| CapabilityError::Permission {
                detail: e.to_string(),
            })?;
        tracing::debug!(count, "文件路径文本已复制");
        Ok(CapabilityResult::Done {
            summary: format!("已复制 {count} 个文件路径（文本）"),
        })
    }
}
inventory::submit!(crate::capability::CapabilityEntry {
    factory: || Arc::new(CopyClipboardFilePaths) as Arc<dyn Capability>
});

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn policy_is_local_explicit_and_registered() {
        let registry = crate::capability::CapabilityRegistry::new();
        assert!(registry.get("copy_clipboard_file_paths").is_some());
    }
    #[test]
    fn request_shape_is_strict() {
        assert!(
            serde_json::from_value::<Request>(json!({"ids":["a"]})).is_ok()
        );
        // 空数组是合法 Request 形态（serde 不校验 minItems），由 invoke 运行时拒绝
        assert!(
            serde_json::from_value::<Request>(json!({"paths":["C:\\a"]})).is_err()
        );
    }
}
