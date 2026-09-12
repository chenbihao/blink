//! `write_text_file` Capability（0.23.2）——文本文件唯一原子写入出口。
//!
//! 写 UTF-8 文本到磁盘 → `Done`。同目录临时文件 + 原子替换，可选
//! identity（size + mtime_ms）冲突保护：文件已被外部修改时返回
//! `Conflict`，绝不静默覆盖。
//!
//! - 编辑器"保存到…"/"另存为副本…"/confirmed_file 原位保存的唯一底层；
//! - 应用层散落的文本写盘（如导出对话）也收敛到此实现（§A3.5 多协议入口，
//!   单一原子执行语义）。
//!
//! `expected_size`/`expected_mtime_ms` 均提供且文件存在时才启用冲突检查；
//! 文件不存在视为首次落盘，直接写入。

use std::path::PathBuf;

use serde_json::{Value, json};

use crate::domain::capability::{
    AiDefault, Capability, CapabilityError, CapabilityPolicy, CapabilityResult, CapabilitySchema,
    ConfirmationPolicy, DangerClass, InvokeContext, McpDefault, OriginSet, RuntimeRequirement,
};

/// `write_text_file` — 原子写入 UTF-8 文本文件（可选冲突保护）。
pub struct WriteTextFile;

/// identity 冲突检查 + 原子写入的同步核心（阻塞操作，调用方负责隔离）。
///
/// 独立成自由函数便于单测： Capability `invoke` 与应用层测试共用同一实现，
/// 不存在第二套写入语义。
pub fn check_and_write_sync(
    path: &str,
    content: &str,
    expected_size: Option<u64>,
    expected_mtime_ms: Option<i64>,
) -> Result<(u64, i64), CapabilityError> {
    let path_buf = PathBuf::from(path);
    if path.trim().is_empty() {
        return Err(CapabilityError::InvalidArgs {
            detail: "path 不能为空".into(),
        });
    }
    if !path_buf.is_absolute() {
        return Err(CapabilityError::InvalidArgs {
            detail: format!("path 必须是绝对路径: {path}"),
        });
    }

    // 冲突检查（identity 完整提供且文件存在时）。
    let wants_check = expected_size.is_some() && expected_mtime_ms.is_some();
    if wants_check {
        match crate::infra::utils::fs::file_identity(&path_buf) {
            Some((actual_size, actual_mtime_ms)) => {
                if actual_size != expected_size.unwrap()
                    || actual_mtime_ms != expected_mtime_ms.unwrap()
                {
                    return Err(CapabilityError::Conflict {
                        detail: format!(
                            "文件已被外部修改（期望 size={} mtime={}，实际 size={} mtime={}）",
                            expected_size.unwrap(),
                            expected_mtime_ms.unwrap(),
                            actual_size,
                            actual_mtime_ms
                        ),
                    });
                }
            }
            // 目标文件已不存在：无内容可丢失，视为首次落盘。
            None => {
                tracing::debug!(path, "目标文件不存在，跳过冲突检查直接写入");
            }
        }
    }

    // 阻塞写入——invoke 侧已 spawn_blocking，此处直呼同步实现。
    crate::infra::utils::fs::atomic_write_bytes(&path_buf, content.as_bytes()).map_err(|e| {
        CapabilityError::Internal {
            detail: format!("写入文件失败: {e}"),
        }
    })
}

#[async_trait::async_trait]
impl Capability for WriteTextFile {
    fn id(&self) -> &str {
        "write_text_file"
    }

    fn schema(&self) -> CapabilitySchema {
        CapabilitySchema {
            name: "write_text_file".into(),
            description: "原子写入 UTF-8 文本文件（同目录临时文件 + 原子替换）。提供 expected_size 与 expected_mtime_ms 时启用冲突保护：文件被外部修改过则拒绝写入。路径必须是绝对路径。".into(),
            parameters: json!({
                "type": "object",
                "required": ["path", "content"],
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目标文件的绝对路径"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的 UTF-8 文本内容"
                    },
                    "expected_size": {
                        "type": "integer",
                        "description": "冲突保护：期望的当前文件字节数（与 expected_mtime_ms 同时提供才生效）"
                    },
                    "expected_mtime_ms": {
                        "type": "integer",
                        "description": "冲突保护：期望的当前文件修改时间（Unix 毫秒）"
                    }
                }
            }),
            ..Default::default()
        }
    }

    fn policy(&self) -> CapabilityPolicy {
        CapabilityPolicy {
            // 写盘属危险副作用：仅本地 Surface/Command 入口，AI/MCP/CLI 不开放。
            allowed_origins: OriginSet::LOCAL_SURFACE | OriginSet::LOCAL_COMMAND,
            runtime_requirement: RuntimeRequirement::MAIN_PROCESS,
            danger: DangerClass::Dangerous,
            sensitive: false,
            ai_default: AiDefault::Off,
            mcp_default: McpDefault::Forbidden,
            confirmation: ConfirmationPolicy::safe(),
        }
    }

    async fn invoke(
        &self,
        args: Value,
        _ctx: &InvokeContext<'_>,
    ) -> Result<CapabilityResult, CapabilityError> {
        let (path, content, expected_size, expected_mtime_ms) = parse_args(&args)?;
        let chars = content.chars().count();

        // 阻塞写盘隔离出 tokio 调度器（spec-backend §一）。
        let result = tokio::task::spawn_blocking(move || {
            check_and_write_sync(&path, &content, expected_size, expected_mtime_ms)
        })
        .await
        .map_err(|e| CapabilityError::Internal {
            detail: format!("写入任务失败: {e}"),
        })??;

        let (size, _) = result;
        Ok(CapabilityResult::Done {
            summary: format!("已写入文本文件（{chars} 字，{size} 字节）"),
        })
    }
}

/// 从 args 解析写入请求（纯函数，便于单测参数契约）。
fn parse_args(args: &Value) -> Result<(String, String, Option<u64>, Option<i64>), CapabilityError> {
    let path = args
        .get("path")
        .and_then(Value::as_str)
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "缺少 path".into(),
        })?
        .to_string();
    let content = args
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| CapabilityError::InvalidArgs {
            detail: "缺少 content".into(),
        })?
        .to_string();
    let expected_size = args.get("expected_size").and_then(Value::as_u64);
    let expected_mtime_ms = args.get("expected_mtime_ms").and_then(Value::as_i64);
    Ok((path, content, expected_size, expected_mtime_ms))
}

inventory::submit!(crate::domain::capability::CapabilityEntry {
    factory: || std::sync::Arc::new(WriteTextFile) as std::sync::Arc<dyn Capability>,
});

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blink-wtf-test-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn rejects_empty_and_relative_paths() {
        let err = check_and_write_sync("", "x", None, None).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));

        let err = check_and_write_sync("relative.txt", "x", None, None).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));
    }

    #[test]
    fn first_write_then_conflict_on_external_change() {
        let dir = temp_dir("conflict");
        let path = dir.join("note.md").to_string_lossy().to_string();

        // 首次写入：无 identity，直接成功
        let (size, mtime) = check_and_write_sync(&path, "v1", None, None).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"v1");

        // identity 匹配：允许覆盖
        let (_s, _m) = check_and_write_sync(&path, "v2", Some(size), Some(mtime)).unwrap();

        // 外部修改后 identity 失配：Conflict，不覆盖
        std::fs::write(&path, "external edit").unwrap();
        let err = check_and_write_sync(&path, "v3", Some(size), Some(mtime)).unwrap_err();
        assert!(matches!(err, CapabilityError::Conflict { .. }));
        assert_eq!(
            std::fs::read(&path).unwrap(),
            b"external edit",
            "冲突时不得覆盖"
        );

        // 文件被外部删除：视为首次落盘，直接写入
        std::fs::remove_file(&path).unwrap();
        check_and_write_sync(&path, "recreated", Some(size), Some(mtime)).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"recreated");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn parse_args_requires_path_and_content() {
        let err = parse_args(&json!({ "path": "C:\\a.md" })).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));

        let err = parse_args(&json!({ "content": "x" })).unwrap_err();
        assert!(matches!(err, CapabilityError::InvalidArgs { .. }));

        let (path, content, size, mtime) = parse_args(&json!({
            "path": "C:\\a.md",
            "content": "正文",
            "expected_size": 6,
            "expected_mtime_ms": 1234
        }))
        .unwrap();
        assert_eq!(path, "C:\\a.md");
        assert_eq!(content, "正文");
        assert_eq!(size, Some(6));
        assert_eq!(mtime, Some(1234));
    }
}
